//! SMTP password stored at `{data_dir}/smtp_secret.json`.
//! Replaces the `SMTP_PASSWORD` environment variable so the plaintext
//! secret no longer has to live inside launchd plists / systemd units.

use std::path::{Path, PathBuf};

use eyre::{Result, WrapErr};
use serde::{Deserialize, Serialize};

const FILE_NAME: &str = "smtp_secret.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SmtpSecretRecord {
    password: String,
}

pub struct SmtpSecretStore {
    path: PathBuf,
}

impl SmtpSecretStore {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            path: data_dir.join(FILE_NAME),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn exists(&self) -> bool {
        self.path.exists()
    }

    pub fn load(&self) -> Result<Option<String>> {
        if !self.path.exists() {
            return Ok(None);
        }
        let body = std::fs::read_to_string(&self.path)
            .wrap_err_with(|| format!("failed to read {}", self.path.display()))?;
        let record: SmtpSecretRecord = serde_json::from_str(&body)
            .wrap_err_with(|| format!("failed to parse {}", self.path.display()))?;
        Ok(Some(record.password))
    }

    pub fn save(&self, password: &str) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .wrap_err_with(|| format!("failed to create dir: {}", parent.display()))?;
        }
        let record = SmtpSecretRecord {
            password: password.to_string(),
        };
        let body = serde_json::to_string_pretty(&record)?;
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
                tracing::warn!(path = %self.path.display(), error = %e, "failed to chmod smtp_secret.json");
            }
        }
        Ok(())
    }

    pub fn clear(&self) -> Result<()> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).wrap_err_with(|| format!("failed to delete {}", self.path.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn save_and_load_round_trip() {
        let dir = TempDir::new().unwrap();
        let store = SmtpSecretStore::new(dir.path());
        assert!(!store.exists());
        assert!(store.load().unwrap().is_none());
        store.save("hunter2-smtp-password").unwrap();
        assert!(store.exists());
        let loaded = store.load().unwrap().unwrap();
        assert_eq!(loaded, "hunter2-smtp-password");
    }

    #[test]
    fn save_overwrites_previous_value() {
        let dir = TempDir::new().unwrap();
        let store = SmtpSecretStore::new(dir.path());
        store.save("first").unwrap();
        store.save("second").unwrap();
        assert_eq!(store.load().unwrap().unwrap(), "second");
    }

    #[test]
    fn clear_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let store = SmtpSecretStore::new(dir.path());
        store.save("to-be-cleared").unwrap();
        assert!(store.exists());
        store.clear().unwrap();
        assert!(!store.exists());
        // Calling clear again on a missing file must succeed.
        store.clear().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn save_uses_0600_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let store = SmtpSecretStore::new(dir.path());
        store.save("perms-password").unwrap();
        let mode = std::fs::metadata(store.path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }
}
