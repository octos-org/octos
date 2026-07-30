use std::path::{Path, PathBuf};

use eyre::Result;
use octos_agent::{SkillFilter, SkillsLoader};

use crate::profiles::{ProfileStore, SkillSelectionMode, UserProfile};

/// Outcome of resolving a profile's inherited skill selection against a set of
/// discovered candidate skills (skill layering v1).
///
/// Callers pass the output of
/// [`ProfileStore::resolve_runtime_profile`](crate::profiles::ProfileStore::resolve_runtime_profile)
/// (an already parent- and defaults-merged profile) plus the discovered
/// candidate skill ids; the resolver partitions the candidates into the ones
/// that load and the ones an inherited rule excludes, and produces the
/// crate-agnostic [`SkillFilter`] handed to both the plugin loader and the
/// [`SkillsLoader`].
#[derive(Debug, Clone)]
pub struct ResolvedSkillCatalog {
    /// The effective selection mode that was applied.
    pub mode: SkillSelectionMode,
    /// Candidate skill ids that will load (deduped, precedence preserved).
    pub enabled: Vec<String>,
    /// Candidate skill ids excluded by the active selection (debug logging).
    pub disabled: Vec<String>,
    /// Filter to hand to the loaders. `None` ⇒ the profile has no skills layer
    /// ⇒ load everything (byte-identical to pre-skill-layering behavior).
    pub filter: Option<SkillFilter>,
}

impl ResolvedSkillCatalog {
    /// Whether any candidate was excluded (for a one-line debug log).
    pub fn has_disabled(&self) -> bool {
        !self.disabled.is_empty()
    }
}

/// Resolve a profile's inherited skill selection over `discovered` candidate
/// skill ids.
///
/// `effective_profile` MUST be the resolved runtime profile (parent + global
/// `profile-defaults.json` already merged via
/// [`ProfileStore::resolve_runtime_profile`](crate::profiles::ProfileStore::resolve_runtime_profile))
/// so its `config.skills` carries the fully-merged selection layer.
///
/// Candidates are deduped by id keeping first-occurrence precedence (mirrors
/// the loader's "first dir wins" discovery). Rule matching is last-wins per id:
/// in `AllDiscovered` a skill loads unless a rule disables it; in `AllowList`
/// only skills with an enabling rule load.
///
/// When the profile has no skills layer at all the returned `filter` is `None`
/// and every candidate is reported enabled — callers pass `None` to the loaders
/// and nothing is filtered.
pub fn resolve_profile_skills(
    effective_profile: &UserProfile,
    discovered: &[String],
) -> ResolvedSkillCatalog {
    // Dedupe candidates, preserving first-occurrence precedence.
    let mut seen = std::collections::HashSet::new();
    let unique: Vec<String> = discovered
        .iter()
        .filter(|id| seen.insert((*id).clone()))
        .cloned()
        .collect();

    match effective_profile.config.skills.as_ref() {
        None => ResolvedSkillCatalog {
            mode: SkillSelectionMode::AllDiscovered,
            enabled: unique,
            disabled: Vec::new(),
            filter: None,
        },
        Some(skills) => {
            let (mut enabled, mut disabled) = (Vec::new(), Vec::new());
            for id in unique {
                if skills.allows(&id) {
                    enabled.push(id);
                } else {
                    disabled.push(id);
                }
            }
            ResolvedSkillCatalog {
                mode: skills.effective_mode(),
                enabled,
                disabled,
                filter: Some(skills.to_agent_filter()),
            }
        }
    }
}

/// Resolve the installed skills directory for exactly the requested account.
///
/// This is intentionally strict: sub-accounts do not inherit their parent
/// profile's installed customer skills.
pub fn resolve_account_skills_dir(store: &ProfileStore, profile_id: &str) -> Result<PathBuf> {
    let profile = store
        .get(profile_id)?
        .ok_or_else(|| eyre::eyre!("profile '{profile_id}' not found"))?;
    let data_dir = store.resolve_data_dir(&profile);
    Ok(data_dir.join("skills"))
}

/// Build a skills loader scoped to the current account only.
pub fn build_account_skills_loader(data_dir: &Path) -> SkillsLoader {
    SkillsLoader::new(data_dir)
}

/// Return plugin/skill package directories for the current account only.
pub fn build_account_plugin_dirs(data_dir: &Path) -> Vec<PathBuf> {
    let skills_dir = data_dir.join("skills");
    if skills_dir.exists() {
        vec![skills_dir]
    } else {
        Vec::new()
    }
}

/// Resolve the ominix-api URL the runtime should hand to skills as
/// `OMINIX_API_URL`. Prefers the explicit env override, falls back to
/// the `~/.ominix/api_url` discovery file dropped by the installer.
///
/// Used by both `gateway` and `serve` plugin loaders so dashboard-
/// installed skills (`mofa-fm`, etc.) can reach the local inference
/// server.
pub(crate) fn discover_ominix_url() -> Option<String> {
    std::env::var("OMINIX_API_URL")
        .ok()
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            let home = std::env::var_os("HOME")?;
            for dir in [".ominix", ".OminiX"] {
                let discovery = std::path::Path::new(&home).join(dir).join("api_url");
                if let Some(url) = std::fs::read_to_string(discovery)
                    .ok()
                    .map(|s| s.trim().trim_end_matches('/').to_string())
                    .filter(|s| !s.is_empty())
                {
                    return Some(url);
                }
            }
            None
        })
}

/// Append the standard per-profile runtime env vars onto a plugin-env
/// vector. Mirrors the gateway path's call site at
/// `gateway_runtime.rs:435` so the `serve` plugin loader can spawn
/// dashboard-installed skills with the same environment they expect.
///
/// The set is intentionally narrow: every entry is something a
/// dashboard-installed skill (e.g. `mofa-fm`) needs to locate
/// per-profile state (voice profiles, data dir) or to reach the
/// local inference server (`ominix-api`).
pub(crate) fn push_runtime_plugin_env(
    plugin_env: &mut Vec<(String, String)>,
    data_dir: &Path,
    octos_home: &Path,
    profile_id: Option<&str>,
    ominix_url: Option<&str>,
) {
    plugin_env.push((
        "OCTOS_DATA_DIR".to_string(),
        data_dir.to_string_lossy().to_string(),
    ));
    plugin_env.push((
        "OCTOS_HOME".to_string(),
        octos_home.to_string_lossy().to_string(),
    ));
    if let Some(profile_id) = profile_id {
        plugin_env.push(("OCTOS_PROFILE_ID".to_string(), profile_id.to_string()));
    }
    plugin_env.push((
        "OCTOS_VOICE_DIR".to_string(),
        data_dir
            .join("voice_profiles")
            .to_string_lossy()
            .to_string(),
    ));
    if let Some(ominix_url) = ominix_url {
        plugin_env.push(("OMINIX_API_URL".to_string(), ominix_url.to_string()));
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::profiles::{
        GatewaySettings, ProfileConfig, ProfileSkillsConfig, SkillRule, UserProfile,
    };

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn profile_with_skills(skills: Option<ProfileSkillsConfig>) -> UserProfile {
        UserProfile {
            id: "p".into(),
            name: "p".into(),
            enabled: true,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: ProfileConfig {
                skills,
                ..Default::default()
            },
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn rule(id: &str, enabled: bool) -> SkillRule {
        SkillRule {
            id: id.to_string(),
            enabled,
        }
    }

    #[test]
    fn resolve_none_layer_loads_everything_and_filters_nothing() {
        let profile = profile_with_skills(None);
        let discovered = ids(&["news", "weather", "time"]);
        let catalog = resolve_profile_skills(&profile, &discovered);
        assert_eq!(catalog.mode, SkillSelectionMode::AllDiscovered);
        assert_eq!(catalog.enabled, discovered);
        assert!(catalog.disabled.is_empty());
        // No filter ⇒ loaders do zero filtering (backwards-compatible).
        assert!(catalog.filter.is_none());
    }

    #[test]
    fn resolve_all_discovered_excludes_only_disabled_rule() {
        let profile = profile_with_skills(Some(ProfileSkillsConfig {
            mode: Some(SkillSelectionMode::AllDiscovered),
            rules: vec![rule("weather", false)],
        }));
        let catalog = resolve_profile_skills(&profile, &ids(&["news", "weather", "time"]));
        assert_eq!(catalog.enabled, ids(&["news", "time"]));
        assert_eq!(catalog.disabled, ids(&["weather"]));
        let filter = catalog.filter.expect("filter present");
        assert!(filter.allows("news"));
        assert!(!filter.allows("weather"));
        // An id with no rule still loads under AllDiscovered.
        assert!(filter.allows("brand-new-skill"));
    }

    #[test]
    fn resolve_allow_list_loads_only_enabled_rules() {
        let profile = profile_with_skills(Some(ProfileSkillsConfig {
            mode: Some(SkillSelectionMode::AllowList),
            rules: vec![rule("news", true), rule("time", false)],
        }));
        let catalog = resolve_profile_skills(&profile, &ids(&["news", "weather", "time"]));
        assert_eq!(catalog.enabled, ids(&["news"]));
        assert_eq!(catalog.disabled, ids(&["weather", "time"]));
        let filter = catalog.filter.expect("filter present");
        assert!(filter.allows("news"));
        // Not allow-listed ⇒ excluded.
        assert!(!filter.allows("weather"));
        // Explicitly disabled ⇒ excluded even though a rule exists.
        assert!(!filter.allows("time"));
    }

    #[test]
    fn resolve_profile_may_reenable_inherited_disabled_rule() {
        // Simulates the post-merge config where an inherited `enabled: false`
        // was replaced by the profile's `enabled: true` (last-wins per id).
        let profile = profile_with_skills(Some(ProfileSkillsConfig {
            mode: Some(SkillSelectionMode::AllDiscovered),
            rules: vec![rule("news", false), rule("news", true)],
        }));
        let catalog = resolve_profile_skills(&profile, &ids(&["news"]));
        assert_eq!(catalog.enabled, ids(&["news"]));
        assert!(catalog.disabled.is_empty());
    }

    #[test]
    fn resolve_dedupes_candidates_preserving_precedence() {
        let profile = profile_with_skills(None);
        let catalog = resolve_profile_skills(&profile, &ids(&["news", "news", "time"]));
        assert_eq!(catalog.enabled, ids(&["news", "time"]));
    }

    #[test]
    fn resolve_account_skills_dir_keeps_sub_account_isolated() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProfileStore::open_unified(dir.path()).unwrap();

        let parent = UserProfile {
            id: "dspfac".into(),
            name: "DSPFAC".into(),
            enabled: true,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: ProfileConfig {
                gateway: GatewaySettings::default(),
                ..Default::default()
            },
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let child = UserProfile {
            id: "dspfac--newsbot".into(),
            name: "Newsbot".into(),
            enabled: true,
            data_dir: None,
            parent_id: Some("dspfac".into()),
            public_subdomain: Some("newsbot".into()),
            config: ProfileConfig {
                gateway: GatewaySettings::default(),
                ..Default::default()
            },
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        store.save(&parent).unwrap();
        store.save(&child).unwrap();

        let parent_dir = resolve_account_skills_dir(&store, "dspfac").unwrap();
        let child_dir = resolve_account_skills_dir(&store, "dspfac--newsbot").unwrap();

        assert_ne!(parent_dir, child_dir);
        assert!(child_dir.to_string_lossy().contains("dspfac--newsbot"));
    }

    #[test]
    fn account_plugin_dirs_only_include_current_account_skills() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir
            .path()
            .join("profiles")
            .join("dspfac--newsbot")
            .join("data");
        let skills_dir = data_dir.join("skills");
        std::fs::create_dir_all(&skills_dir).unwrap();

        let dirs = build_account_plugin_dirs(&data_dir);
        assert_eq!(dirs, vec![skills_dir]);
    }

    #[test]
    fn push_runtime_plugin_env_carries_voice_dir_and_profile_id() {
        // Validates the contract that `mofa-fm` / `fm_tts` depend on:
        // `OCTOS_PROFILE_ID` for per-profile state and `OCTOS_VOICE_DIR`
        // pointing at the profile's `voice_profiles/` so yangmi.wav etc.
        // are findable. Also `OMINIX_API_URL` when provided so the
        // skill can reach the local TTS server.
        let data_dir = std::path::PathBuf::from("/tmp/profile-data");
        let octos_home = std::path::PathBuf::from("/home/user/.octos");
        let mut env = Vec::new();
        push_runtime_plugin_env(
            &mut env,
            &data_dir,
            &octos_home,
            Some("dspfac"),
            Some("http://127.0.0.1:8765"),
        );

        let map: std::collections::HashMap<_, _> = env.into_iter().collect();
        assert_eq!(
            map.get("OCTOS_DATA_DIR").map(String::as_str),
            Some("/tmp/profile-data")
        );
        assert_eq!(
            map.get("OCTOS_HOME").map(String::as_str),
            Some("/home/user/.octos")
        );
        assert_eq!(
            map.get("OCTOS_PROFILE_ID").map(String::as_str),
            Some("dspfac")
        );
        // Derive the expectation the way the product does (`Path::join`), so
        // the separator matches on Windows (`\`) as well as Unix (`/`).
        let expected_voice = data_dir
            .join("voice_profiles")
            .to_string_lossy()
            .to_string();
        assert_eq!(map.get("OCTOS_VOICE_DIR"), Some(&expected_voice));
        assert_eq!(
            map.get("OMINIX_API_URL").map(String::as_str),
            Some("http://127.0.0.1:8765")
        );
    }

    #[test]
    fn push_runtime_plugin_env_omits_optional_keys_when_absent() {
        let mut env = Vec::new();
        push_runtime_plugin_env(
            &mut env,
            std::path::Path::new("/p"),
            std::path::Path::new("/h"),
            None,
            None,
        );
        let keys: std::collections::HashSet<_> = env.into_iter().map(|(k, _)| k).collect();
        assert!(!keys.contains("OCTOS_PROFILE_ID"));
        assert!(!keys.contains("OMINIX_API_URL"));
        assert!(keys.contains("OCTOS_DATA_DIR"));
        assert!(keys.contains("OCTOS_HOME"));
        assert!(keys.contains("OCTOS_VOICE_DIR"));
    }
}
