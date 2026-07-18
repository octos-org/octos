use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::Duration;

use chrono::{DateTime, Utc};
use eyre::{Result, WrapErr, bail};
use serde::{Deserialize, Serialize};

const ALLOWLIST_LOCK_RETRY_DELAY: Duration = Duration::from_millis(50);
const ALLOWLIST_LOCK_MAX_ATTEMPTS: usize = 40;
const ALLOWLIST_LOCK_STALE_AFTER: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AllowedLogin {
    pub email: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub created_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claimed_user_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claimed_at: Option<DateTime<Utc>>,
}

pub struct LoginAllowlistStore {
    path: PathBuf,
}

struct AllowlistWriteLock {
    path: PathBuf,
}

impl Drop for AllowlistWriteLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

impl LoginAllowlistStore {
    pub fn open(data_dir: &Path) -> Result<Self> {
        let path = data_dir.join("login_allowlist.json");
        if !path.exists() {
            std::fs::write(&path, "[]").wrap_err_with(|| {
                format!("failed to initialize allowlist store: {}", path.display())
            })?;
        }
        Ok(Self { path })
    }

    pub fn list(&self) -> Result<Vec<AllowedLogin>> {
        self.read_all()
    }

    pub fn get(&self, email: &str) -> Result<Option<AllowedLogin>> {
        let normalized = normalize_email(email);
        Ok(self
            .read_all()?
            .into_iter()
            .find(|entry| normalize_email(&entry.email) == normalized))
    }

    pub fn contains(&self, email: &str) -> Result<bool> {
        Ok(self.get(email)?.is_some())
    }

    pub fn save(&self, entry: &AllowedLogin) -> Result<()> {
        let normalized = normalize_email(&entry.email);
        let mut next = entry.clone();
        next.email = normalized.clone();
        self.update_entries(|entries| {
            entries.retain(|current| normalize_email(&current.email) != normalized);
            entries.push(next);
            Ok(((), true))
        })
    }

    pub fn delete(&self, email: &str) -> Result<bool> {
        let normalized = normalize_email(email);
        self.update_entries(|entries| {
            let before = entries.len();
            entries.retain(|entry| normalize_email(&entry.email) != normalized);
            let changed = entries.len() != before;
            Ok((changed, changed))
        })
    }

    pub fn claim(&self, email: &str, user_id: &str) -> Result<()> {
        let normalized = normalize_email(email);
        self.update_entries(|entries| {
            let mut changed = false;
            for entry in entries.iter_mut() {
                if normalize_email(&entry.email) == normalized {
                    entry.claimed_user_id = Some(user_id.to_string());
                    entry.claimed_at = Some(Utc::now());
                    changed = true;
                }
            }
            Ok(((), changed))
        })
    }

    fn read_all(&self) -> Result<Vec<AllowedLogin>> {
        let content = std::fs::read_to_string(&self.path)
            .wrap_err_with(|| format!("failed to read allowlist store: {}", self.path.display()))?;
        let mut items: Vec<AllowedLogin> =
            serde_json::from_str(&content).wrap_err("failed to parse allowlist store")?;
        items.sort_by(|a, b| a.email.cmp(&b.email));
        Ok(items)
    }

    fn update_entries<T>(
        &self,
        mutate: impl FnOnce(&mut Vec<AllowedLogin>) -> Result<(T, bool)>,
    ) -> Result<T> {
        let _lock = self.acquire_write_lock()?;
        let mut entries = self.read_all()?;
        let (result, changed) = mutate(&mut entries)?;
        if changed {
            self.write_all(&entries)?;
        }
        Ok(result)
    }

    fn acquire_write_lock(&self) -> Result<AllowlistWriteLock> {
        let lock_path = self.lock_path();
        for attempt in 0..ALLOWLIST_LOCK_MAX_ATTEMPTS {
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock_path)
            {
                Ok(mut file) => {
                    writeln!(file, "pid={}", std::process::id()).ok();
                    writeln!(file, "created_at={}", Utc::now().to_rfc3339()).ok();
                    return Ok(AllowlistWriteLock { path: lock_path });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if self.clear_stale_lock(&lock_path)? {
                        continue;
                    }
                    if attempt + 1 == ALLOWLIST_LOCK_MAX_ATTEMPTS {
                        bail!(
                            "timed out waiting for allowlist write lock: {}",
                            lock_path.display()
                        );
                    }
                    sleep(ALLOWLIST_LOCK_RETRY_DELAY);
                }
                Err(error) => {
                    return Err(error).wrap_err_with(|| {
                        format!("failed to create allowlist lock: {}", lock_path.display())
                    });
                }
            }
        }

        bail!(
            "timed out waiting for allowlist write lock: {}",
            lock_path.display()
        )
    }

    fn clear_stale_lock(&self, lock_path: &Path) -> Result<bool> {
        let metadata = match std::fs::metadata(lock_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(error).wrap_err_with(|| {
                    format!("failed to inspect allowlist lock: {}", lock_path.display())
                });
            }
        };

        let modified = match metadata.modified() {
            Ok(modified) => modified,
            Err(_) => return Ok(false),
        };
        let age = modified.elapsed().unwrap_or_default();
        if age <= ALLOWLIST_LOCK_STALE_AFTER {
            return Ok(false);
        }

        match std::fs::remove_file(lock_path) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error).wrap_err_with(|| {
                format!(
                    "failed to remove stale allowlist lock: {}",
                    lock_path.display()
                )
            }),
        }
    }

    fn lock_path(&self) -> PathBuf {
        self.path.with_extension("json.lock")
    }

    fn write_all(&self, entries: &[AllowedLogin]) -> Result<()> {
        let mut unique = BTreeMap::new();
        for entry in entries {
            unique.insert(normalize_email(&entry.email), entry.clone());
        }
        let sorted: Vec<AllowedLogin> = unique.into_values().collect();
        let content = serde_json::to_string_pretty(&sorted)
            .wrap_err("failed to serialize allowlist store")?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, content)
            .wrap_err_with(|| format!("failed to write allowlist temp file: {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path).wrap_err_with(|| {
            format!(
                "failed to move allowlist temp file into place: {}",
                self.path.display()
            )
        })?;
        Ok(())
    }
}

pub fn normalize_email(email: &str) -> String {
    email.trim().to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_claim_and_delete_normalize_email() {
        let dir = tempfile::tempdir().unwrap();
        let store = LoginAllowlistStore::open(dir.path()).unwrap();

        store
            .save(&AllowedLogin {
                email: " Test@Example.com ".into(),
                note: Some("owner".into()),
                created_at: Utc::now(),
                claimed_user_id: None,
                claimed_at: None,
            })
            .unwrap();

        let saved = store.get("test@example.com").unwrap().unwrap();
        assert_eq!(saved.email, "test@example.com");
        assert_eq!(store.list().unwrap().len(), 1);

        store.claim("TEST@example.com", "user-123").unwrap();
        let claimed = store.get("test@example.com").unwrap().unwrap();
        assert_eq!(claimed.claimed_user_id.as_deref(), Some("user-123"));
        assert!(claimed.claimed_at.is_some());

        assert!(store.delete(" TEST@example.com ").unwrap());
        assert!(store.get("test@example.com").unwrap().is_none());
    }
}
