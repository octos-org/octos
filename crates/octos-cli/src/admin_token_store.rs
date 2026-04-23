//! Hashed admin auth token stored at `{data_dir}/admin_token.json`.
//! Replaces the static config/env bootstrap token once rotated.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use eyre::{Result, WrapErr};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const FILE_NAME: &str = "admin_token.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdminTokenRecord {
    /// 16 random bytes, base64 (URL-safe, no padding).
    pub salt: String,
    /// sha256(salt_bytes || token_bytes), base64 (URL-safe, no padding).
    pub hash: String,
    pub created_at: DateTime<Utc>,
    pub created_by: String,
}

impl AdminTokenRecord {
    pub fn from_plaintext(token: &str) -> Self {
        use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
        let mut salt_bytes = [0u8; 16];
        getrandom::getrandom(&mut salt_bytes).expect("getrandom failed");
        let salt = URL_SAFE_NO_PAD.encode(salt_bytes);
        let hash = hash_with_salt(&salt_bytes, token);
        Self {
            salt,
            hash,
            created_at: Utc::now(),
            created_by: "bootstrap-rotation".into(),
        }
    }

    pub fn verify(&self, token: &str) -> bool {
        use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
        let Ok(salt_bytes) = URL_SAFE_NO_PAD.decode(&self.salt) else {
            return false;
        };
        let expected = hash_with_salt(&salt_bytes, token);
        constant_time_eq::constant_time_eq(expected.as_bytes(), self.hash.as_bytes())
    }
}

fn hash_with_salt(salt: &[u8], token: &str) -> String {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    let mut hasher = Sha256::new();
    hasher.update(salt);
    hasher.update(token.as_bytes());
    URL_SAFE_NO_PAD.encode(hasher.finalize())
}

pub struct AdminTokenStore {
    path: PathBuf,
}

impl AdminTokenStore {
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

    pub fn load(&self) -> Result<Option<AdminTokenRecord>> {
        if !self.path.exists() {
            return Ok(None);
        }
        let body = std::fs::read_to_string(&self.path)
            .wrap_err_with(|| format!("failed to read {}", self.path.display()))?;
        let record = serde_json::from_str(&body)
            .wrap_err_with(|| format!("failed to parse {}", self.path.display()))?;
        Ok(Some(record))
    }

    pub fn save(&self, record: &AdminTokenRecord) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .wrap_err_with(|| format!("failed to create dir: {}", parent.display()))?;
        }
        let body = serde_json::to_string_pretty(record)?;
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
                tracing::warn!(path = %self.path.display(), error = %e, "failed to chmod admin_token.json");
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
    fn hashes_and_verifies_a_token() {
        let record = AdminTokenRecord::from_plaintext("my-strong-token-1234567890-abcde");
        assert!(record.verify("my-strong-token-1234567890-abcde"));
        assert!(!record.verify("wrong-token"));
    }

    #[test]
    fn salts_are_unique_per_record() {
        let a = AdminTokenRecord::from_plaintext("same-token-value-xyz-1234567890");
        let b = AdminTokenRecord::from_plaintext("same-token-value-xyz-1234567890");
        assert_ne!(a.salt, b.salt);
        assert_ne!(a.hash, b.hash);
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = TempDir::new().unwrap();
        let store = AdminTokenStore::new(dir.path());
        assert!(!store.exists());
        let record = AdminTokenRecord::from_plaintext("round-trip-token-abcdefghijkl-01");
        store.save(&record).unwrap();
        assert!(store.exists());
        let loaded = store.load().unwrap().unwrap();
        assert_eq!(loaded.hash, record.hash);
        assert_eq!(loaded.salt, record.salt);
        assert!(loaded.verify("round-trip-token-abcdefghijkl-01"));
    }

    #[test]
    fn clear_removes_file() {
        let dir = TempDir::new().unwrap();
        let store = AdminTokenStore::new(dir.path());
        store
            .save(&AdminTokenRecord::from_plaintext(
                "clear-me-token-1234567890-abcde",
            ))
            .unwrap();
        assert!(store.exists());
        store.clear().unwrap();
        assert!(!store.exists());
        store.clear().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn save_uses_0600_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let store = AdminTokenStore::new(dir.path());
        store
            .save(&AdminTokenRecord::from_plaintext(
                "perms-token-abcdefghijkl-012345",
            ))
            .unwrap();
        let mode = std::fs::metadata(store.path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }
}
