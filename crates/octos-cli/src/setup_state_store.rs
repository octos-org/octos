//! Persistent setup-wizard state stored at `{data_dir}/setup_state.json`.
//!
//! Tracks wizard completion/skip status and the furthest step reached so the
//! dashboard can resume or gate the first-run flow across sessions.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use eyre::{Result, WrapErr};
use serde::{Deserialize, Serialize};

const FILE_NAME: &str = "setup_state.json";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SetupState {
    #[serde(default)]
    pub wizard_completed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub wizard_skipped: bool,
    #[serde(default)]
    pub wizard_last_step_reached: u32,
}

pub struct SetupStateStore {
    path: PathBuf,
}

impl SetupStateStore {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            path: data_dir.join(FILE_NAME),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> Result<SetupState> {
        if !self.path.exists() {
            return Ok(SetupState::default());
        }
        let body = std::fs::read_to_string(&self.path)
            .wrap_err_with(|| format!("failed to read {}", self.path.display()))?;
        let state = serde_json::from_str(&body)
            .wrap_err_with(|| format!("failed to parse {}", self.path.display()))?;
        Ok(state)
    }

    pub fn update_last_step(&self, step: u32) -> Result<()> {
        let mut state = self.load()?;
        state.wizard_last_step_reached = step;
        self.save(&state)
    }

    pub fn mark_complete(&self) -> Result<()> {
        let mut state = self.load()?;
        state.wizard_completed_at = Some(Utc::now());
        state.wizard_skipped = false;
        self.save(&state)
    }

    pub fn mark_skipped(&self) -> Result<()> {
        let mut state = self.load()?;
        state.wizard_completed_at = Some(Utc::now());
        state.wizard_skipped = true;
        self.save(&state)
    }

    fn save(&self, state: &SetupState) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .wrap_err_with(|| format!("failed to create dir: {}", parent.display()))?;
        }
        let body = serde_json::to_string_pretty(state)?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, &body)
            .wrap_err_with(|| format!("failed to write {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path)
            .wrap_err_with(|| format!("failed to rename into {}", self.path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            if let Err(e) = std::fs::set_permissions(&self.path, perms) {
                tracing::warn!(
                    path = %self.path.display(),
                    error = %e,
                    "failed to chmod setup_state.json"
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn default_state_is_empty() {
        let dir = TempDir::new().unwrap();
        let store = SetupStateStore::new(dir.path());
        let state = store.load().unwrap();
        assert!(state.wizard_completed_at.is_none());
        assert!(!state.wizard_skipped);
        assert_eq!(state.wizard_last_step_reached, 0);
    }

    #[test]
    fn updates_last_step_round_trip() {
        let dir = TempDir::new().unwrap();
        let store = SetupStateStore::new(dir.path());
        store.update_last_step(3).unwrap();
        assert_eq!(store.load().unwrap().wizard_last_step_reached, 3);
    }

    #[test]
    fn mark_complete_and_mark_skip() {
        let dir = TempDir::new().unwrap();
        let store = SetupStateStore::new(dir.path());
        store.mark_complete().unwrap();
        let s = store.load().unwrap();
        assert!(s.wizard_completed_at.is_some());
        assert!(!s.wizard_skipped);

        let dir2 = TempDir::new().unwrap();
        let store2 = SetupStateStore::new(dir2.path());
        store2.mark_skipped().unwrap();
        let s2 = store2.load().unwrap();
        assert!(s2.wizard_completed_at.is_some());
        assert!(s2.wizard_skipped);
    }

    #[cfg(unix)]
    #[test]
    fn save_uses_0600_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let store = SetupStateStore::new(dir.path());
        store.update_last_step(1).unwrap();
        let mode = std::fs::metadata(store.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
