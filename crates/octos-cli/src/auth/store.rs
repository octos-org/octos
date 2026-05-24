//! Auth credential storage (mode 0600).

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use eyre::{Result, WrapErr};
use serde::{Deserialize, Serialize};

/// Environment variable that overrides the auth credential store path.
pub const AUTH_STORE_ENV: &str = "OCTOS_AUTH_STORE";

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

    /// Load the auth store from a config-provided path, falling back to the
    /// environment/default resolver when config does not provide one.
    pub fn load_from_config_path(path: Option<&str>) -> Result<Self> {
        let path = Self::store_path_with_config(path)?;
        Self::load_from_path(path)
    }

    /// Load the auth store from an explicit path (or create empty).
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

    /// Path priority:
    /// 1. `OCTOS_AUTH_STORE`
    /// 2. `OCTOS_HOME/auth.json`
    /// 3. `~/.octos/auth.json`
    fn store_path() -> Result<PathBuf> {
        Self::store_path_with_config(None)
    }

    /// Path priority:
    /// 1. `OCTOS_AUTH_STORE`
    /// 2. `auth_store_path` from config
    /// 3. `OCTOS_HOME/auth.json`
    /// 4. `~/.octos/auth.json`
    fn store_path_with_config(config_path: Option<&str>) -> Result<PathBuf> {
        let auth_store = std::env::var_os(AUTH_STORE_ENV);
        let octos_home = std::env::var_os("OCTOS_HOME");
        Self::store_path_from_parts(
            auth_store.as_deref(),
            config_path,
            octos_home.as_deref(),
            dirs::home_dir(),
        )
    }

    fn store_path_from_parts(
        auth_store: Option<&OsStr>,
        config_path: Option<&str>,
        octos_home: Option<&OsStr>,
        home: Option<PathBuf>,
    ) -> Result<PathBuf> {
        if let Some(path) = auth_store.filter(|value| !value.is_empty()) {
            return Ok(PathBuf::from(path));
        }
        if let Some(path) = config_path.map(str::trim).filter(|value| !value.is_empty()) {
            return Ok(PathBuf::from(path));
        }
        if let Some(home) = octos_home.filter(|value| !value.is_empty()) {
            return Ok(PathBuf::from(home).join("auth.json"));
        }
        let home = home.ok_or_else(|| eyre::eyre!("cannot determine home directory"))?;
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

    #[test]
    fn load_from_path_reads_custom_store() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("custom-auth.json");

        {
            let mut store = AuthStore::load_from_path(&path).unwrap();
            store
                .set(
                    "customtest",
                    AuthCredential {
                        access_token: "from-custom-store".to_string(),
                        refresh_token: None,
                        expires_at: None,
                        provider: "customtest".to_string(),
                        auth_method: "paste_token".to_string(),
                    },
                )
                .unwrap();
        }

        let store = AuthStore::load_from_path(&path).unwrap();
        assert_eq!(
            store
                .get("customtest")
                .map(|cred| cred.access_token.as_str()),
            Some("from-custom-store")
        );
    }

    #[test]
    fn store_path_prefers_auth_store_env_over_config() {
        let path = AuthStore::store_path_from_parts(
            Some(OsStr::new("/tmp/env-auth.json")),
            Some("/tmp/config-auth.json"),
            Some(OsStr::new("/tmp/octos-home")),
            Some(PathBuf::from("/tmp/home")),
        )
        .unwrap();

        assert_eq!(path, PathBuf::from("/tmp/env-auth.json"));
    }

    #[test]
    fn store_path_uses_config_path_before_octos_home() {
        let path = AuthStore::store_path_from_parts(
            None,
            Some(" /tmp/config-auth.json "),
            Some(OsStr::new("/tmp/octos-home")),
            Some(PathBuf::from("/tmp/home")),
        )
        .unwrap();

        assert_eq!(path, PathBuf::from("/tmp/config-auth.json"));
    }

    #[test]
    fn store_path_uses_octos_home_before_legacy_home() {
        let path = AuthStore::store_path_from_parts(
            None,
            None,
            Some(OsStr::new("/tmp/octos-home")),
            Some(PathBuf::from("/tmp/home")),
        )
        .unwrap();

        assert_eq!(path, PathBuf::from("/tmp/octos-home").join("auth.json"));
    }

    #[test]
    fn store_path_falls_back_to_legacy_home() {
        let path =
            AuthStore::store_path_from_parts(None, None, None, Some(PathBuf::from("/tmp/home")))
                .unwrap();

        assert_eq!(path, PathBuf::from("/tmp/home/.octos/auth.json"));
    }
}
