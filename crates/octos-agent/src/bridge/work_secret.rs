//! Short-lived external session ingress credentials.

use std::path::{Path, PathBuf};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Duration, Utc};
use eyre::{Result, WrapErr};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const WORK_SECRET_VERSION: u8 = 1;
const FILE_NAME: &str = "work_secrets.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkSecret {
    pub version: u8,
    pub session_ingress_token: String,
    pub api_base_url: String,
}

impl WorkSecret {
    pub fn new(api_base_url: impl Into<String>, session_ingress_token: impl Into<String>) -> Self {
        Self {
            version: WORK_SECRET_VERSION,
            session_ingress_token: session_ingress_token.into(),
            api_base_url: api_base_url.into(),
        }
    }

    pub fn encode(&self) -> Result<String> {
        let body = serde_json::to_vec(self)?;
        Ok(URL_SAFE_NO_PAD.encode(body))
    }

    pub fn decode(encoded: &str) -> Result<Self> {
        let body = URL_SAFE_NO_PAD
            .decode(encoded.trim())
            .wrap_err("work secret is not valid base64url")?;
        let secret: Self =
            serde_json::from_slice(&body).wrap_err("work secret is not valid JSON")?;
        if secret.version != WORK_SECRET_VERSION {
            eyre::bail!(
                "unsupported work secret version {} (expected {})",
                secret.version,
                WORK_SECRET_VERSION
            );
        }
        if secret.session_ingress_token.trim().is_empty() {
            eyre::bail!("work secret missing session_ingress_token");
        }
        if secret.api_base_url.trim().is_empty() {
            eyre::bail!("work secret missing api_base_url");
        }
        Ok(secret)
    }

    pub fn session_ingress_ws_url(&self, session_id: &str) -> Result<String> {
        let mut base = self.api_base_url.trim().trim_end_matches('/').to_owned();
        if let Some(rest) = base.strip_prefix("https://") {
            base = format!("wss://{rest}");
        } else if let Some(rest) = base.strip_prefix("http://") {
            base = format!("ws://{rest}");
        } else if !(base.starts_with("ws://") || base.starts_with("wss://")) {
            eyre::bail!("api_base_url must start with http://, https://, ws://, or wss://");
        }
        Ok(format!(
            "{base}/v1/session_ingress/ws/{}",
            percent_encode_path_segment(session_id)
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkSecretGrantRecord {
    pub token_hash: String,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    pub api_base_url: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_at: Option<DateTime<Utc>>,
}

impl WorkSecretGrantRecord {
    pub fn active(&self, now: DateTime<Utc>) -> bool {
        self.revoked_at.is_none() && self.expires_at > now
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct WorkSecretGrantFile {
    #[serde(default)]
    grants: Vec<WorkSecretGrantRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkSecretValidationError {
    Missing,
    SessionMismatch,
    Expired,
    Revoked,
}

#[derive(Debug, Clone)]
pub struct WorkSecretGrantStore {
    path: PathBuf,
}

impl WorkSecretGrantStore {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            path: data_dir.join(FILE_NAME),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn issue(
        &self,
        session_id: impl Into<String>,
        token: &str,
        api_base_url: impl Into<String>,
        ttl: Duration,
        profile_id: Option<String>,
    ) -> Result<WorkSecretGrantRecord> {
        if token.trim().is_empty() {
            eyre::bail!("session ingress token cannot be empty");
        }
        if ttl <= Duration::zero() {
            eyre::bail!("ttl must be positive");
        }
        let now = Utc::now();
        let record = WorkSecretGrantRecord {
            token_hash: hash_token(token),
            session_id: session_id.into(),
            profile_id,
            api_base_url: api_base_url.into(),
            created_at: now,
            expires_at: now + ttl,
            revoked_at: None,
        };
        let mut file = self.load_file()?;
        file.grants.retain(|grant| {
            !(grant.token_hash == record.token_hash || grant.session_id == record.session_id)
        });
        file.grants.push(record.clone());
        self.save_file(&file)?;
        Ok(record)
    }

    pub fn validate(
        &self,
        session_id: &str,
        token: &str,
    ) -> std::result::Result<WorkSecretGrantRecord, WorkSecretValidationError> {
        let hash = hash_token(token);
        let file = self
            .load_file()
            .map_err(|_| WorkSecretValidationError::Missing)?;
        let Some(record) = file
            .grants
            .into_iter()
            .find(|grant| constant_time_eq(grant.token_hash.as_bytes(), hash.as_bytes()))
        else {
            return Err(WorkSecretValidationError::Missing);
        };
        if record.session_id != session_id {
            return Err(WorkSecretValidationError::SessionMismatch);
        }
        if record.revoked_at.is_some() {
            return Err(WorkSecretValidationError::Revoked);
        }
        if record.expires_at <= Utc::now() {
            return Err(WorkSecretValidationError::Expired);
        }
        Ok(record)
    }

    pub fn revoke_token(&self, token: &str) -> Result<bool> {
        let hash = hash_token(token);
        let mut file = self.load_file()?;
        let now = Utc::now();
        let mut changed = false;
        for grant in &mut file.grants {
            if constant_time_eq(grant.token_hash.as_bytes(), hash.as_bytes())
                && grant.revoked_at.is_none()
            {
                grant.revoked_at = Some(now);
                changed = true;
            }
        }
        if changed {
            self.save_file(&file)?;
        }
        Ok(changed)
    }

    fn load_file(&self) -> Result<WorkSecretGrantFile> {
        if !self.path.exists() {
            return Ok(WorkSecretGrantFile::default());
        }
        let body = std::fs::read_to_string(&self.path)
            .wrap_err_with(|| format!("failed to read {}", self.path.display()))?;
        serde_json::from_str(&body)
            .wrap_err_with(|| format!("failed to parse {}", self.path.display()))
    }

    fn save_file(&self, file: &WorkSecretGrantFile) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .wrap_err_with(|| format!("failed to create dir: {}", parent.display()))?;
        }
        let body = serde_json::to_string_pretty(file)?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, body)
            .wrap_err_with(|| format!("failed to write {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path)
            .wrap_err_with(|| format!("failed to rename into {}", self.path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            if let Err(error) = std::fs::set_permissions(&self.path, perms) {
                tracing::warn!(path = %self.path.display(), %error, "failed to chmod work_secrets.json");
            }
        }
        Ok(())
    }
}

pub fn hash_token(token: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(token.as_bytes()))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let len_eq = a.len() ^ b.len();
    let mut result = 0u8;
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        result |= x ^ y;
    }
    result == 0 && len_eq == 0
}

fn percent_encode_path_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b':' | b'@' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn work_secret_round_trips_as_base64url_json() {
        let secret = WorkSecret::new("http://127.0.0.1:50080", "token-123");
        let encoded = secret.encode().unwrap();
        assert!(!encoded.contains('='));
        assert_eq!(WorkSecret::decode(&encoded).unwrap(), secret);
    }

    #[test]
    fn work_secret_rejects_version_mismatch() {
        let encoded = URL_SAFE_NO_PAD
            .encode(br#"{"version":2,"session_ingress_token":"t","api_base_url":"http://x"}"#);
        let error = WorkSecret::decode(&encoded).unwrap_err().to_string();
        assert!(error.contains("unsupported work secret version"));
    }

    #[test]
    fn work_secret_builds_ws_url() {
        let secret = WorkSecret::new("https://api.example.test/", "token-123");
        assert_eq!(
            secret
                .session_ingress_ws_url("dspfac:local:tui#coding")
                .unwrap(),
            "wss://api.example.test/v1/session_ingress/ws/dspfac:local:tui%23coding"
        );
    }

    #[test]
    fn grant_store_validates_and_revokes_token() {
        let dir = tempfile::tempdir().unwrap();
        let store = WorkSecretGrantStore::new(dir.path());
        store
            .issue(
                "profile:local:demo",
                "plain-token",
                "http://127.0.0.1:50080",
                Duration::minutes(5),
                Some("profile".into()),
            )
            .unwrap();

        let grant = store.validate("profile:local:demo", "plain-token").unwrap();
        assert_eq!(grant.profile_id.as_deref(), Some("profile"));
        assert!(matches!(
            store.validate("profile:local:other", "plain-token"),
            Err(WorkSecretValidationError::SessionMismatch)
        ));

        assert!(store.revoke_token("plain-token").unwrap());
        assert!(matches!(
            store.validate("profile:local:demo", "plain-token"),
            Err(WorkSecretValidationError::Revoked)
        ));
    }
}
