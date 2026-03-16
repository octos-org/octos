//! Auth credential storage at ~/.octos/auth.json (mode 0600).

use std::collections::HashMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use eyre::{Result, WrapErr};
use serde::{Deserialize, Serialize};

/// A stored authentication credential.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthCredential {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
    pub provider: String,
    /// "oauth", "device_code", or "paste_token".
    pub auth_method: String,
}

impl AuthCredential {
    /// Whether this credential has expired.
    pub fn is_expired(&self) -> bool {
        self.expires_at.is_some_and(|exp| exp < Utc::now())
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct AuthData {
    credentials: HashMap<String, AuthCredential>,
}

/// Manages persisted auth credentials.
pub struct AuthStore {
    path: PathBuf,
    data: AuthData,
}

impl AuthStore {
    /// Load the auth store from disk (or create empty).
    pub fn load() -> Result<Self> {
        let path = Self::store_path()?;
        let data = if path.exists() {
            let content = std::fs::read_to_string(&path).wrap_err("failed to read auth store")?;
            serde_json::from_str(&content).wrap_err("failed to parse auth store")?
        } else {
            AuthData::default()
        };
        Ok(Self { path, data })
    }

    /// Get credential for a provider.
    pub fn get(&self, provider: &str) -> Option<&AuthCredential> {
        self.data.credentials.get(provider)
    }

    /// Store a credential and persist to disk.
    pub fn set(&mut self, provider: &str, cred: AuthCredential) -> Result<()> {
        self.data.credentials.insert(provider.to_string(), cred);
        self.save()
    }

    /// Remove a credential and persist.
    pub fn remove(&mut self, provider: &str) -> Result<bool> {
        let removed = self.data.credentials.remove(provider).is_some();
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    /// Iterate over all stored credentials.
    pub fn list(&self) -> impl Iterator<Item = (&str, &AuthCredential)> {
        self.data.credentials.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Save to disk with restrictive permissions.
    fn save(&self) -> Result<()> {
        let dir = self
            .path
            .parent()
            .ok_or_else(|| eyre::eyre!("auth store path has no parent directory"))?;
        std::fs::create_dir_all(dir)?;

        let json = serde_json::to_string_pretty(&self.data)?;

        // Create file with 0600 permissions atomically (no race window)
        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&self.path)?;
            file.write_all(json.as_bytes())?;
        }

        #[cfg(not(unix))]
        {
            std::fs::write(&self.path, &json)?;
        }

        Ok(())
    }

    /// Path: ~/.octos/auth.json
    fn store_path() -> Result<PathBuf> {
        let home =
            dirs::home_dir().ok_or_else(|| eyre::eyre!("cannot determine home directory"))?;
        Ok(home.join(".octos").join("auth.json"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_store(dir: &TempDir) -> AuthStore {
        let path = dir.path().join("auth.json");
        AuthStore {
            path,
            data: AuthData::default(),
        }
    }

    #[test]
    fn test_set_and_get() {
        let tmp = TempDir::new().unwrap();
        let mut store = test_store(&tmp);

        let cred = AuthCredential {
            access_token: "sk-test-123".to_string(),
            refresh_token: None,
            expires_at: None,
            provider: "anthropic".to_string(),
            auth_method: "paste_token".to_string(),
        };

        store.set("anthropic", cred).unwrap();
        let got = store.get("anthropic").unwrap();
        assert_eq!(got.access_token, "sk-test-123");
        assert_eq!(got.auth_method, "paste_token");
    }

    #[test]
    fn test_remove() {
        let tmp = TempDir::new().unwrap();
        let mut store = test_store(&tmp);

        let cred = AuthCredential {
            access_token: "token".to_string(),
            refresh_token: None,
            expires_at: None,
            provider: "openai".to_string(),
            auth_method: "oauth".to_string(),
        };

        store.set("openai", cred).unwrap();
        assert!(store.get("openai").is_some());

        assert!(store.remove("openai").unwrap());
        assert!(store.get("openai").is_none());
        assert!(!store.remove("openai").unwrap());
    }

    #[test]
    fn test_is_expired() {
        let expired = AuthCredential {
            access_token: "t".to_string(),
            refresh_token: None,
            expires_at: Some(Utc::now() - chrono::Duration::hours(1)),
            provider: "test".to_string(),
            auth_method: "oauth".to_string(),
        };
        assert!(expired.is_expired());

        let valid = AuthCredential {
            access_token: "t".to_string(),
            refresh_token: None,
            expires_at: Some(Utc::now() + chrono::Duration::hours(1)),
            provider: "test".to_string(),
            auth_method: "oauth".to_string(),
        };
        assert!(!valid.is_expired());

        let no_expiry = AuthCredential {
            access_token: "t".to_string(),
            refresh_token: None,
            expires_at: None,
            provider: "test".to_string(),
            auth_method: "paste_token".to_string(),
        };
        assert!(!no_expiry.is_expired());
    }

    #[test]
    fn test_persistence() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("auth.json");

        // Write
        {
            let mut store = AuthStore {
                path: path.clone(),
                data: AuthData::default(),
            };
            store
                .set(
                    "test",
                    AuthCredential {
                        access_token: "persisted".to_string(),
                        refresh_token: Some("refresh".to_string()),
                        expires_at: None,
                        provider: "test".to_string(),
                        auth_method: "oauth".to_string(),
                    },
                )
                .unwrap();
        }

        // Read back
        {
            let content = std::fs::read_to_string(&path).unwrap();
            let data: AuthData = serde_json::from_str(&content).unwrap();
            assert_eq!(data.credentials["test"].access_token, "persisted");
            assert_eq!(
                data.credentials["test"].refresh_token.as_deref(),
                Some("refresh")
            );
        }
    }
}
