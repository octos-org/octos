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
}
