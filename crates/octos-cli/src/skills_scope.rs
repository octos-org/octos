use std::path::{Path, PathBuf};

use eyre::Result;
use octos_agent::SkillsLoader;

use crate::profiles::ProfileStore;

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
    std::env::var("OMINIX_API_URL").ok().or_else(|| {
        let home = std::env::var_os("HOME")?;
        let discovery = std::path::Path::new(&home).join(".ominix").join("api_url");
        std::fs::read_to_string(discovery)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
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
    use crate::profiles::{GatewaySettings, ProfileConfig, UserProfile};

    #[test]
    fn resolve_account_skills_dir_keeps_sub_account_isolated() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProfileStore::open(dir.path()).unwrap();

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
        assert_eq!(
            map.get("OCTOS_VOICE_DIR").map(String::as_str),
            Some("/tmp/profile-data/voice_profiles")
        );
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
