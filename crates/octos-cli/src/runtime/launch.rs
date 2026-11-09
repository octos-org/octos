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

use super::session::{ActiveProfileOwner, project_sessions_root, read_active_profile_marker};

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
    /// Authenticated opener recorded by a v1 marker. Legacy/unknown markers
    /// have no owner and are never treated as admin-owned.
    pub(crate) active_profile_owner: Option<ActiveProfileOwner>,
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
    /// user to `octoscode onboard`.
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
    resolve_launch_decision_with_sticky(
        requested_profile,
        derive_sticky_profile(folder),
        default_profile,
        folder,
    )
}

/// Admin implicit launches may follow only a marker the admin connection
/// wrote itself. Explicit profile requests still win, preserving the admin's
/// cross-profile entry point. Legacy markers and store recency have no owner
/// and are deliberately ignored rather than migrated.
pub fn resolve_admin_launch_decision(
    requested_profile: Option<&str>,
    default_profile: Option<&str>,
    folder: &FolderSessions,
) -> LaunchDecision {
    let sticky = (folder.active_profile_owner == Some(ActiveProfileOwner::Admin))
        .then(|| folder.active_profile.clone())
        .flatten();
    resolve_launch_decision_with_sticky(requested_profile, sticky, default_profile, folder)
}

fn resolve_launch_decision_with_sticky(
    requested_profile: Option<&str>,
    sticky_profile: Option<String>,
    default_profile: Option<&str>,
    folder: &FolderSessions,
) -> LaunchDecision {
    let resolved = requested_profile
        .map(str::trim)
        .filter(|profile| !profile.is_empty())
        .map(str::to_string)
        .or(sticky_profile)
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
    let marker = read_active_profile_marker(&octos_dir);
    let active_profile = marker.as_ref().map(|marker| marker.profile_id.clone());
    let active_profile_owner = marker.and_then(|marker| marker.owner);

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
        active_profile_owner,
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
