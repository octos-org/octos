//! Auth credential storage.
//!
//! Defaults to `~/.octos/auth.json` and can be overridden with
//! `OCTOS_AUTH_STORE_PATH` or `auth_store_path` in config.

use std::collections::HashMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use eyre::{Result, WrapErr};
use serde::{Deserialize, Serialize};

/// Environment override for the auth store file path.
pub const AUTH_STORE_PATH_ENV: &str = "OCTOS_AUTH_STORE_PATH";

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
        Self::load_from_path(Self::store_path()?)
    }

    /// Load the auth store using config/env path resolution.
    pub fn load_for_config(config: &crate::config::Config) -> Result<Self> {
        Self::load_from_path(Self::store_path_for_config(Some(config))?)
    }

    /// Load the auth store from an explicit path.
    pub fn load_from_path(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
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

    /// Resolve the auth store path.
    ///
    /// Precedence: `OCTOS_AUTH_STORE_PATH` > `auth_store_path` in config >
    /// `~/.octos/auth.json`.
    fn store_path() -> Result<PathBuf> {
        Self::store_path_for_config(None)
    }

    fn store_path_for_config(config: Option<&crate::config::Config>) -> Result<PathBuf> {
        if let Some(path) = std::env::var_os(AUTH_STORE_PATH_ENV) {
            if !path.as_os_str().is_empty() {
                return Ok(PathBuf::from(path));
            }
        }

        if let Some(path) = config.and_then(|config| config.auth_store_path.as_ref()) {
            if !path.as_os_str().is_empty() {
                return Ok(path.clone());
            }
        }

        Self::default_store_path()
    }

    fn default_store_path() -> Result<PathBuf> {
        let home =
            dirs::home_dir().ok_or_else(|| eyre::eyre!("cannot determine home directory"))?;
        Ok(home.join(".octos").join("auth.json"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn auth_store_env_lock() -> &'static std::sync::Mutex<()> {
        use std::sync::{Mutex, OnceLock};
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

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
    fn load_from_path_reads_explicit_store_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("custom-auth.json");

        let mut store = AuthStore::load_from_path(&path).unwrap();
        store
            .set(
                "openai",
                AuthCredential {
                    access_token: "sk-custom-store".to_string(),
                    refresh_token: None,
                    expires_at: None,
                    provider: "openai".to_string(),
                    auth_method: "paste_token".to_string(),
                },
            )
            .unwrap();

        let reloaded = AuthStore::load_from_path(&path).unwrap();
        assert_eq!(
            reloaded
                .get("openai")
                .map(|cred| cred.access_token.as_str()),
            Some("sk-custom-store")
        );
    }

    #[test]
    #[allow(unsafe_code)]
    fn store_path_prefers_env_override() {
        let _guard = auth_store_env_lock().lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let expected = tmp.path().join("env-auth.json");
        let previous = std::env::var_os(AUTH_STORE_PATH_ENV);

        // SAFETY: serialized by `auth_store_env_lock` and restored below.
        unsafe { std::env::set_var(AUTH_STORE_PATH_ENV, &expected) };
        let resolved = AuthStore::store_path().unwrap();

        // SAFETY: serialized by `auth_store_env_lock`.
        match previous {
            Some(value) => unsafe { std::env::set_var(AUTH_STORE_PATH_ENV, value) },
            None => unsafe { std::env::remove_var(AUTH_STORE_PATH_ENV) },
        }

        assert_eq!(resolved, expected);
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
