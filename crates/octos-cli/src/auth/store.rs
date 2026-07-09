//! Auth credential storage (mode 0600).
//!
//! The store path is `auth_home/auth.json`, where `auth_home` comes from the
//! canonical [`ConfigContext`](crate::config_context::ConfigContext) — GLOBAL
//! (XDG) unless `OCTOS_CONFIG_DIR` is set. The store NEVER recomputes its path
//! from the environment on its own; callers pass the resolved location via
//! [`AuthStore::open`] or [`AuthStore::at`]. There is intentionally no no-arg
//! `load()` so no site can diverge from the resolver.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use eyre::{Result, WrapErr};
use serde::{Deserialize, Serialize};

use crate::config_context::ConfigContext;

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
    /// Open the auth store at the context's `auth_home` (i.e.
    /// `auth_home/auth.json`). This is the canonical entrypoint.
    pub fn open(ctx: &ConfigContext) -> Result<Self> {
        Self::at(&ctx.auth_home)
    }

    /// Open the auth store at `auth_home/auth.json` (or create empty in memory).
    pub fn at(auth_home: &Path) -> Result<Self> {
        let path = auth_home.join("auth.json");
        let data = if path.exists() {
            // Harden an existing store on open: a credential file created by an
            // older octos (or copied in by hand) may be 0644. `save()` always
            // writes 0600, but a file we only ever READ would stay world/group
            // readable forever — tighten it best-effort whenever we open it.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                if let Ok(meta) = std::fs::metadata(&path) {
                    if meta.permissions().mode() & 0o077 != 0 {
                        let _ =
                            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
                    }
                }
            }
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
    fn at_resolves_auth_json_under_auth_home() {
        let tmp = TempDir::new().unwrap();
        let auth_home = tmp.path().join("xdg").join("octos");
        let store = AuthStore::at(&auth_home).unwrap();
        assert_eq!(store.path, auth_home.join("auth.json"));
    }

    #[cfg(unix)]
    #[test]
    fn save_writes_0600() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = TempDir::new().unwrap();
        let auth_home = tmp.path().join("octos");
        let mut store = AuthStore::at(&auth_home).unwrap();
        store
            .set(
                "anthropic",
                AuthCredential {
                    access_token: "tok".into(),
                    refresh_token: None,
                    expires_at: None,
                    provider: "anthropic".into(),
                    auth_method: "paste_token".into(),
                },
            )
            .unwrap();
        let mode = std::fs::metadata(auth_home.join("auth.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    #[cfg(unix)]
    fn at_tightens_loose_permissions_on_open() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = TempDir::new().unwrap();
        let auth_home = tmp.path().join("octos");
        std::fs::create_dir_all(&auth_home).unwrap();
        let path = auth_home.join("auth.json");
        // Simulate a legacy / hand-copied store that is world-readable.
        std::fs::write(&path, "{\"credentials\":{}}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        // Opening it must tighten the perms to 0600 (no plain-read leaving creds
        // world/group readable).
        let _store = AuthStore::at(&auth_home).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "at() must tighten a loose auth.json to 0600");
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
