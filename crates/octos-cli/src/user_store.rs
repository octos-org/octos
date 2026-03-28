//! User management for multi-user dashboard deployments.
//!
//! Each user is stored as an individual JSON file in `{data_dir}/users/`.
//! The user's `id` field doubles as their profile ID (1:1 mapping).

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use eyre::{Result, WrapErr, bail};
use serde::{Deserialize, Serialize};

/// User role for access control.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UserRole {
    Admin,
    User,
}

/// A registered user.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    /// Unique identifier (slug: lowercase alphanumeric + hyphens).
    /// Also serves as the profile ID.
    pub id: String,
    /// Email address (used for OTP login).
    pub email: String,
    /// Display name.
    pub name: String,
    /// Role-based access control.
    pub role: UserRole,
    /// When this user was created.
    pub created_at: DateTime<Utc>,
    /// When this user last logged in.
    #[serde(default)]
    pub last_login_at: Option<DateTime<Utc>>,
}

/// Manages user storage as individual JSON files.
pub struct UserStore {
    users_dir: PathBuf,
}

impl UserStore {
    /// Open (or create) the user store at `data_dir/users/`.
    pub fn open(data_dir: &Path) -> Result<Self> {
        let users_dir = data_dir.join("users");
        std::fs::create_dir_all(&users_dir)
            .wrap_err_with(|| format!("failed to create users dir: {}", users_dir.display()))?;
        Ok(Self { users_dir })
    }

    /// List all users sorted by name.
    pub fn list(&self) -> Result<Vec<User>> {
        let mut users = Vec::new();
        let entries = match std::fs::read_dir(&self.users_dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(users),
            Err(e) => return Err(e).wrap_err("failed to read users directory"),
        };
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "json") {
                match std::fs::read_to_string(&path) {
                    Ok(content) => match serde_json::from_str::<User>(&content) {
                        Ok(user) => users.push(user),
                        Err(e) => {
                            tracing::warn!(path = %path.display(), error = %e, "skipping invalid user");
                        }
                    },
                    Err(e) => {
                        tracing::warn!(path = %path.display(), error = %e, "failed to read user");
                    }
                }
            }
        }
        users.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(users)
    }

    /// Get a single user by ID.
    pub fn get(&self, id: &str) -> Result<Option<User>> {
        validate_user_id(id)?;
        let path = self.user_path(id);
        if !path.exists() {
            return Ok(None);
        }
        let content = std::fs::read_to_string(&path)
            .wrap_err_with(|| format!("failed to read user: {id}"))?;
        let user = serde_json::from_str(&content)
            .wrap_err_with(|| format!("failed to parse user: {id}"))?;
        Ok(Some(user))
    }

    /// Find a user by email address.
    pub fn get_by_email(&self, email: &str) -> Result<Option<User>> {
        let email_lower = email.to_lowercase();
        let users = self.list()?;
        Ok(users
            .into_iter()
            .find(|u| u.email.to_lowercase() == email_lower))
    }

    /// Save a user (create or update).
    pub fn save(&self, user: &User) -> Result<()> {
        validate_user_id(&user.id)?;

        let path = self.user_path(&user.id);
        let content = serde_json::to_string_pretty(user).wrap_err("failed to serialize user")?;

        // Atomic write: write to temp file then rename
        let tmp_path = path.with_extension("json.tmp");
        std::fs::write(&tmp_path, &content)
            .wrap_err_with(|| format!("failed to write user: {}", tmp_path.display()))?;
        std::fs::rename(&tmp_path, &path)
            .wrap_err_with(|| format!("failed to rename user file: {}", path.display()))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            if let Err(e) = std::fs::set_permissions(&path, perms) {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "failed to set restrictive permissions on user file"
                );
            }
        }

        Ok(())
    }

    /// Delete a user by ID.
    pub fn delete(&self, id: &str) -> Result<bool> {
        validate_user_id(id)?;
        let path = self.user_path(id);
        if !path.exists() {
            return Ok(false);
        }
        std::fs::remove_file(&path).wrap_err_with(|| format!("failed to delete user: {id}"))?;
        Ok(true)
    }

    fn user_path(&self, id: &str) -> PathBuf {
        self.users_dir.join(format!("{id}.json"))
    }
}

/// Derive a user ID (slug) from an email address.
/// Converts `alice@example.com` → `alice`.
/// If only numeric or too short, uses the full local part with domain.
pub fn email_to_user_id(email: &str) -> String {
    let local = email.split('@').next().unwrap_or(email);
    let slug: String = local
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    // Trim leading/trailing hyphens, collapse consecutive hyphens
    let slug = slug.trim_matches('-').to_string();
    let mut result = String::new();
    let mut last_was_hyphen = false;
    for c in slug.chars() {
        if c == '-' {
            if !last_was_hyphen {
                result.push(c);
                last_was_hyphen = true;
            }
        } else {
            result.push(c);
            last_was_hyphen = false;
        }
    }
    // Truncate to 63 chars to stay within slug limits
    let mut result: String = result.chars().take(63).collect();
    // Trim any trailing hyphen produced by truncation
    while result.ends_with('-') {
        result.pop();
    }
    if result.is_empty() {
        "user".into()
    } else if !result.starts_with(|c: char| c.is_ascii_alphanumeric()) {
        // Ensure the slug starts with an alphanumeric character
        format!("u{result}")
    } else {
        result
    }
}

/// Validate a user ID (slug format).
fn validate_user_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 64 {
        bail!("user ID must be 1-64 characters");
    }
    if !id
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        bail!("user ID must contain only lowercase letters, digits, and hyphens");
    }
    if id.starts_with('-') || id.ends_with('-') {
        bail!("user ID must not start or end with a hyphen");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_user_id() {
        assert!(validate_user_id("alice").is_ok());
        assert!(validate_user_id("team-bot").is_ok());
        assert!(validate_user_id("user123").is_ok());
        assert!(validate_user_id("").is_err());
        assert!(validate_user_id("-bad").is_err());
        assert!(validate_user_id("bad-").is_err());
        assert!(validate_user_id("UPPER").is_err());
        assert!(validate_user_id("has space").is_err());
        assert!(validate_user_id(&"a".repeat(65)).is_err());
    }

    #[test]
    fn test_email_to_user_id() {
        assert_eq!(email_to_user_id("alice@example.com"), "alice");
        assert_eq!(email_to_user_id("Bob.Smith@corp.co"), "bob-smith");
        assert_eq!(email_to_user_id("user+tag@test.com"), "user-tag");
        assert_eq!(email_to_user_id("...@test.com"), "user");
    }

    #[test]
    fn test_user_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = UserStore::open(dir.path()).unwrap();

        let user = User {
            id: "alice".into(),
            email: "alice@example.com".into(),
            name: "Alice".into(),
            role: UserRole::User,
            created_at: Utc::now(),
            last_login_at: None,
        };

        store.save(&user).unwrap();
        let loaded = store.get("alice").unwrap().unwrap();
        assert_eq!(loaded.email, "alice@example.com");
        assert_eq!(loaded.role, UserRole::User);

        let by_email = store.get_by_email("alice@example.com").unwrap().unwrap();
        assert_eq!(by_email.id, "alice");

        let users = store.list().unwrap();
        assert_eq!(users.len(), 1);

        assert!(store.delete("alice").unwrap());
        assert!(store.get("alice").unwrap().is_none());
    }

    #[test]
    fn test_user_file_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let store = UserStore::open(dir.path()).unwrap();
        let user = User {
            id: "perms".into(),
            email: "p@test.com".into(),
            name: "Perms".into(),
            role: UserRole::Admin,
            created_at: Utc::now(),
            last_login_at: None,
        };
        store.save(&user).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let meta = std::fs::metadata(store.user_path("perms")).unwrap();
            assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn test_user_serde_roundtrip() {
        let user = User {
            id: "test".into(),
            email: "test@example.com".into(),
            name: "Test User".into(),
            role: UserRole::Admin,
            created_at: Utc::now(),
            last_login_at: Some(Utc::now()),
        };
        let json = serde_json::to_string(&user).unwrap();
        let parsed: User = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, "test");
        assert_eq!(parsed.role, UserRole::Admin);
    }
}
