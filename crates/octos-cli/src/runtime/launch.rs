//! Launch-time decision logic for per-project (`appui.sessions_in_cwd`)
//! sessions: given the folder a client launched in and the profile it
//! requested, decide whether to **resume** the folder's existing conversation,
//! prompt to **activate** a brand-new one, or surface a **cross-profile**
//! choice.
//!
//! This is the server half of the launch UX. It is deliberately split into a
//! **pure** decision core ([`resolve_launch_decision`]) and a thin filesystem
//! scan ([`scan_folder_sessions`]) so the branchy decision table is unit-tested
//! without touching disk, and the scan reuses the SAME on-disk layout the write
//! path uses ([`super::session::project_sessions_root`]) — the two can never
//! disagree about where a project's sessions live.

use std::path::Path;
use std::time::SystemTime;

use super::session::project_sessions_root;

/// A profile that has an activated session store under a project's `.octos/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderProfile {
    /// The profile id (human-readable, as it appears in the registry).
    pub profile_id: String,
    /// Best-effort recency of the store, used to derive the sticky profile
    /// when no explicit `active-profile` marker is present. `None` if the
    /// mtime could not be read.
    pub last_used: Option<SystemTime>,
}

/// What a launch found under `<cwd>/.octos/`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FolderSessions {
    /// Known profiles with an activated store in this folder.
    pub profiles: Vec<FolderProfile>,
    /// The explicit sticky profile recorded at `<cwd>/.octos/active-profile`,
    /// if any (trimmed, non-empty).
    pub active_profile: Option<String>,
}

/// The launch decision the client renders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchDecision {
    /// The resolved profile already has a conversation here — resume it.
    Resume { profile_id: String },
    /// The folder has no conversation for any known profile — offer to
    /// activate one for `profile_id`.
    NeedsActivation { profile_id: String },
    /// The folder holds conversation(s) for other profile(s), but not the
    /// resolved one. The client offers: switch to one of `existing_profiles`
    /// and resume, or start fresh as `launching_profile`.
    CrossProfile {
        launching_profile: String,
        existing_profiles: Vec<String>,
    },
    /// No profile exists on the machine at all — the client must send the
    /// user to `octos-tui onboard`.
    NoProfile,
}

/// The sticky profile for a folder: the explicit `active-profile` marker if
/// present, else the most-recently-used known profile that has a store here.
/// `None` when the folder has neither.
pub fn derive_sticky_profile(folder: &FolderSessions) -> Option<String> {
    if let Some(marker) = folder
        .active_profile
        .as_deref()
        .map(str::trim)
        .filter(|marker| !marker.is_empty())
    {
        return Some(marker.to_string());
    }
    folder
        .profiles
        .iter()
        .max_by_key(|entry| entry.last_used)
        .map(|entry| entry.profile_id.clone())
}

/// Decide what a launch in a folder should do.
///
/// Resolution order for the *launching* profile:
/// 1. `requested_profile` — an explicit `--profile` always wins.
/// 2. the folder's sticky profile ([`derive_sticky_profile`]) — "the brain you
///    last used *here*", so a bare launch returns to the right conversation.
/// 3. `default_profile` — the machine's global default.
///
/// With `None` at every step (no profiles configured) the result is
/// [`LaunchDecision::NoProfile`].
pub fn resolve_launch_decision(
    requested_profile: Option<&str>,
    default_profile: Option<&str>,
    folder: &FolderSessions,
) -> LaunchDecision {
    let resolved = requested_profile
        .map(str::trim)
        .filter(|profile| !profile.is_empty())
        .map(str::to_string)
        .or_else(|| derive_sticky_profile(folder))
        .or_else(|| {
            default_profile
                .map(str::trim)
                .filter(|profile| !profile.is_empty())
                .map(str::to_string)
        });

    let Some(profile_id) = resolved else {
        return LaunchDecision::NoProfile;
    };

    if folder
        .profiles
        .iter()
        .any(|entry| entry.profile_id == profile_id)
    {
        return LaunchDecision::Resume { profile_id };
    }

    // The resolved profile has no store here. A folder with no known profiles
    // at all is brand-new → offer to activate; a folder holding other
    // profiles' conversations → surface the cross-profile choice.
    if folder.profiles.is_empty() {
        return LaunchDecision::NeedsActivation { profile_id };
    }

    let mut existing_profiles: Vec<String> = folder
        .profiles
        .iter()
        .map(|entry| entry.profile_id.clone())
        .collect();
    existing_profiles.sort();
    existing_profiles.dedup();
    LaunchDecision::CrossProfile {
        launching_profile: profile_id,
        existing_profiles,
    }
}

/// Scan `<cwd>/.octos/` for the launch decision inputs: which of
/// `known_profiles` have an activated store here, plus the explicit
/// `active-profile` sticky marker.
///
/// Bounded by `known_profiles` (rather than decoding arbitrary directory
/// names) so a store dir left behind by a since-deleted profile is ignored —
/// the launch UX can only ever resume/switch to a profile that still exists.
/// The store location is derived with the SAME
/// [`super::session::project_sessions_root`] the write path uses.
pub fn scan_folder_sessions(cwd: &Path, known_profiles: &[String]) -> FolderSessions {
    let octos_dir = cwd.join(".octos");
    let active_profile = std::fs::read_to_string(octos_dir.join("active-profile"))
        .ok()
        .map(|marker| marker.trim().to_string())
        .filter(|marker| !marker.is_empty());

    let mut profiles = Vec::new();
    for profile_id in known_profiles {
        let root = project_sessions_root(cwd, profile_id);
        if store_is_activated(&root) {
            let last_used = std::fs::metadata(&root)
                .and_then(|meta| meta.modified())
                .ok();
            profiles.push(FolderProfile {
                profile_id: profile_id.clone(),
                last_used,
            });
        }
    }

    FolderSessions {
        profiles,
        active_profile,
    }
}

/// Whether a per-project store root has been activated (bootstrapped) — i.e.
/// the transcript store or its `users/` subtree exists. `SessionManager::open`
/// creates `sessions/`, so either marker means the profile has been launched in
/// this folder at least once. A bare (never-opened) `.octos/<id>` directory
/// therefore does not count as activated.
fn store_is_activated(root: &Path) -> bool {
    root.join("sessions").is_dir() || root.join("users").is_dir()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn profile(id: &str, secs: u64) -> FolderProfile {
        FolderProfile {
            profile_id: id.to_string(),
            last_used: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs)),
        }
    }

    // ---- resolve_launch_decision (pure) ----

    #[test]
    fn should_return_no_profile_when_none_configured_and_folder_empty() {
        let folder = FolderSessions::default();
        assert_eq!(
            resolve_launch_decision(None, None, &folder),
            LaunchDecision::NoProfile
        );
    }

    #[test]
    fn should_activate_when_folder_empty_and_default_exists() {
        let folder = FolderSessions::default();
        assert_eq!(
            resolve_launch_decision(None, Some("glm"), &folder),
            LaunchDecision::NeedsActivation {
                profile_id: "glm".to_string()
            }
        );
    }

    #[test]
    fn should_resume_when_requested_profile_has_store_here() {
        let folder = FolderSessions {
            profiles: vec![profile("glm", 1)],
            active_profile: None,
        };
        assert_eq!(
            resolve_launch_decision(Some("glm"), Some("glm"), &folder),
            LaunchDecision::Resume {
                profile_id: "glm".to_string()
            }
        );
    }

    #[test]
    fn should_resume_folder_sticky_over_global_default() {
        // Bare launch (no --profile); folder has glm; global default is
        // deepseek. Sticky (the folder's brain) must win → resume glm, not
        // activate deepseek.
        let folder = FolderSessions {
            profiles: vec![profile("glm", 5)],
            active_profile: None,
        };
        assert_eq!(
            resolve_launch_decision(None, Some("deepseek"), &folder),
            LaunchDecision::Resume {
                profile_id: "glm".to_string()
            }
        );
    }

    #[test]
    fn should_honor_explicit_active_profile_marker_over_recency() {
        // glm is newer, but the active-profile marker pins deepseek.
        let folder = FolderSessions {
            profiles: vec![profile("glm", 9), profile("deepseek", 1)],
            active_profile: Some("deepseek".to_string()),
        };
        assert_eq!(
            resolve_launch_decision(None, None, &folder),
            LaunchDecision::Resume {
                profile_id: "deepseek".to_string()
            }
        );
    }

    #[test]
    fn should_pick_most_recent_profile_as_sticky_without_marker() {
        let folder = FolderSessions {
            profiles: vec![profile("glm", 1), profile("deepseek", 2)],
            active_profile: None,
        };
        assert_eq!(
            resolve_launch_decision(None, None, &folder),
            LaunchDecision::Resume {
                profile_id: "deepseek".to_string()
            }
        );
    }

    #[test]
    fn should_cross_profile_when_explicit_profile_differs_from_folder() {
        let folder = FolderSessions {
            profiles: vec![profile("glm", 1)],
            active_profile: None,
        };
        assert_eq!(
            resolve_launch_decision(Some("deepseek"), Some("deepseek"), &folder),
            LaunchDecision::CrossProfile {
                launching_profile: "deepseek".to_string(),
                existing_profiles: vec!["glm".to_string()],
            }
        );
    }

    #[test]
    fn should_activate_when_resolved_profile_absent_and_no_other_profiles() {
        let folder = FolderSessions::default();
        assert_eq!(
            resolve_launch_decision(Some("glm"), None, &folder),
            LaunchDecision::NeedsActivation {
                profile_id: "glm".to_string()
            }
        );
    }

    #[test]
    fn should_list_existing_profiles_sorted_in_cross_profile() {
        let folder = FolderSessions {
            profiles: vec![profile("zeta", 1), profile("alpha", 2)],
            active_profile: None,
        };
        // Explicit request for a third profile → cross-profile listing both,
        // sorted for a stable client render.
        assert_eq!(
            resolve_launch_decision(Some("mid"), None, &folder),
            LaunchDecision::CrossProfile {
                launching_profile: "mid".to_string(),
                existing_profiles: vec!["alpha".to_string(), "zeta".to_string()],
            }
        );
    }

    // ---- scan_folder_sessions (filesystem) ----

    #[test]
    fn should_scan_empty_folder_as_no_profiles() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = scan_folder_sessions(tmp.path(), &["glm".to_string()]);
        assert_eq!(folder, FolderSessions::default());
    }

    #[test]
    fn should_detect_activated_profile_store() {
        let tmp = tempfile::tempdir().unwrap();
        let known = vec!["glm".to_string(), "deepseek".to_string()];
        let glm_root = project_sessions_root(tmp.path(), "glm");
        std::fs::create_dir_all(glm_root.join("sessions")).unwrap();

        let folder = scan_folder_sessions(tmp.path(), &known);
        assert_eq!(folder.profiles.len(), 1);
        assert_eq!(folder.profiles[0].profile_id, "glm");
        assert_eq!(folder.active_profile, None);
    }

    #[test]
    fn should_read_active_profile_marker() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".octos")).unwrap();
        std::fs::write(tmp.path().join(".octos").join("active-profile"), "  glm\n").unwrap();

        let folder = scan_folder_sessions(tmp.path(), &["glm".to_string()]);
        assert_eq!(folder.active_profile, Some("glm".to_string()));
    }

    #[test]
    fn should_ignore_store_dirs_of_unknown_profiles() {
        let tmp = tempfile::tempdir().unwrap();
        // A store exists for "ghost", but it is not in the known set.
        let ghost_root = project_sessions_root(tmp.path(), "ghost");
        std::fs::create_dir_all(ghost_root.join("sessions")).unwrap();

        let folder = scan_folder_sessions(tmp.path(), &["glm".to_string()]);
        assert!(folder.profiles.is_empty());
    }
}
