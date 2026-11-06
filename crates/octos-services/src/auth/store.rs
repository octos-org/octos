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
        // Harden an existing store on open: a credential file created by an
        // older octos (or copied in by hand) may be 0644. `save()` always
        // writes 0600, but a file we only ever READ would stay world/group
        // readable forever — tighten it best-effort whenever we open it.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            if let Ok(meta) = std::fs::metadata(&path) {
                if meta.permissions().mode() & 0o077 != 0 {
                    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
                }
            }
        }
        let data = Self::auth_data_from_read(auth_home, &path, std::fs::read_to_string(&path))?;
        Ok(Self { path, data })
    }

    fn auth_data_from_read(
        auth_home: &Path,
        path: &Path,
        result: std::io::Result<String>,
    ) -> Result<AuthData> {
        match result {
            Ok(content) => serde_json::from_str(&content).map_err(|error| {
                // serde data errors may quote the offending credential value.
                // Preserve actionable JSON category/location without content.
                eyre::eyre!(
                    "failed to parse auth store at {}: JSON {:?} error at line {} column {}",
                    path.display(),
                    error.classify(),
                    error.line(),
                    error.column()
                )
            }),
            // Windows maps a non-directory path component to NotFound too.
            // Only a store beneath a real directory (or an entirely absent
            // home) is genuinely missing; an existing non-directory home or
            // an unreadable home must retain the original read failure.
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    && Self::auth_store_may_be_absent(auth_home) =>
            {
                Ok(AuthData::default())
            }
            Err(error) => Err(error)
                .wrap_err_with(|| format!("failed to read auth store at {}", path.display())),
        }
    }

    fn auth_store_may_be_absent(auth_home: &Path) -> bool {
        match std::fs::metadata(auth_home) {
            Ok(metadata) => metadata.is_dir(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
            Err(_) => false,
        }
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
    fn should_not_treat_windows_not_found_under_file_auth_home_as_missing_store() {
        let tmp = TempDir::new().unwrap();
        let auth_home = tmp.path().join("auth-home");
        std::fs::write(&auth_home, b"not-a-directory").unwrap();
        let path = auth_home.join("auth.json");

        // Windows reports ERROR_PATH_NOT_FOUND as NotFound when a path
        // component is a regular file. That is a broken auth-home layout,
        // not an absent credential store.
        let error = AuthStore::auth_data_from_read(
            &auth_home,
            &path,
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "simulated Windows path-not-found",
            )),
        )
        .unwrap_err();
        let detail = format!("{error:#}");
        assert!(detail.contains("failed to read auth store"), "{detail}");
        assert!(detail.contains(&path.display().to_string()), "{detail}");
    }

    #[test]
    fn should_keep_not_found_as_missing_for_directory_or_absent_auth_home() {
        let tmp = TempDir::new().unwrap();
        for auth_home in [tmp.path().to_path_buf(), tmp.path().join("absent")] {
            let path = auth_home.join("auth.json");
            let data = AuthStore::auth_data_from_read(
                &auth_home,
                &path,
                Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "simulated missing auth.json",
                )),
            )
            .unwrap();
            assert!(data.credentials.is_empty());
        }
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
