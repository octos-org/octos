//! Email OTP authentication and session management.
//!
//! Provides Larksuite-style email verification: user enters email, receives a
//! 6-digit code, verifies it, and gets a session token. Sessions are stored
//! in-memory with configurable expiry.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use eyre::{Result, bail};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::user_store::{UserRole, UserStore};

/// SMTP configuration for sending OTP emails.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SmtpConfig {
    /// SMTP server hostname.
    pub host: String,
    /// SMTP server port (465 for implicit TLS, 587 for STARTTLS).
    #[serde(default = "default_smtp_port")]
    pub port: u16,
    /// SMTP username.
    pub username: String,
    /// Env var name holding the SMTP password.
    pub password_env: String,
    /// Sender email address.
    pub from_address: String,
}

fn default_smtp_port() -> u16 {
    465
}

/// Dashboard authentication configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DashboardAuthConfig {
    /// SMTP configuration for sending OTP emails.
    pub smtp: SmtpConfig,
    /// Session expiry in hours.
    #[serde(default = "default_session_hours")]
    pub session_expiry_hours: u64,
    /// Whether to allow self-registration (unknown emails auto-create users).
    #[serde(default)]
    pub allow_self_registration: bool,
}

fn default_session_hours() -> u64 {
    24
}

struct PendingOtp {
    code: String,
    created_at: DateTime<Utc>,
    attempts: u32,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ActiveSession {
    pub user_id: String,
    pub role: UserRole,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

/// OTP and session manager with optional disk persistence.
pub struct AuthManager {
    pending_otps: RwLock<HashMap<String, PendingOtp>>,
    sessions: RwLock<HashMap<String, ActiveSession>>,
    smtp_config: Option<SmtpConfig>,
    /// Pre-resolved SMTP password (from profile env_vars or keychain).
    /// Used as fallback when the process environment doesn't have the password.
    smtp_password: Option<String>,
    session_expiry_hours: u64,
    pub allow_self_registration: bool,
    user_store: Arc<UserStore>,
    /// Path to persist sessions. `None` = in-memory only (tests).
    sessions_path: Option<PathBuf>,
}

impl AuthManager {
    pub fn new(config: Option<DashboardAuthConfig>, user_store: Arc<UserStore>) -> Self {
        let (smtp_config, session_expiry_hours, allow_self_registration) = match config {
            Some(c) => (
                Some(c.smtp),
                c.session_expiry_hours,
                c.allow_self_registration,
            ),
            None => (None, 24, true), // dev mode: allow self-registration
        };
        Self {
            pending_otps: RwLock::new(HashMap::new()),
            sessions: RwLock::new(HashMap::new()),
            smtp_config,
            smtp_password: None,
            session_expiry_hours,
            allow_self_registration,
            user_store,
            sessions_path: None,
        }
    }

    /// Set an explicit SMTP password (e.g. resolved from profile env_vars).
    /// Used as fallback when the process environment doesn't have the password.
    pub fn with_smtp_password(mut self, password: String) -> Self {
        self.smtp_password = Some(password);
        self
    }

    /// Set the path for session persistence and load any existing sessions.
    pub fn with_sessions_path(mut self, path: PathBuf) -> Self {
        if let Ok(data) = std::fs::read_to_string(&path) {
            if let Ok(saved) = serde_json::from_str::<HashMap<String, ActiveSession>>(&data) {
                let now = Utc::now();
                let active: HashMap<String, ActiveSession> = saved
                    .into_iter()
                    .filter(|(_, s)| now < s.expires_at)
                    .collect();
                let count = active.len();
                self.sessions = RwLock::new(active);
                tracing::info!(count, path = %path.display(), "restored persisted sessions");
            }
        }
        self.sessions_path = Some(path);
        self
    }

    /// Persist current sessions to disk (best-effort).
    fn persist_sessions_blocking(sessions: &HashMap<String, ActiveSession>, path: &PathBuf) {
        if let Ok(data) = serde_json::to_string(sessions) {
            let _ = std::fs::write(path, data);
        }
    }

    /// Generate and send OTP to email. Returns Ok(true) if sent, Ok(false) if rate-limited.
    pub async fn send_otp(&self, email: &str) -> Result<bool> {
        let email_lower = email.to_lowercase();
        let now = Utc::now();

        // When self-registration is off, only send OTP if user exists
        if !self.allow_self_registration {
            let user = self.user_store.get_by_email(&email_lower)?;
            if user.is_none() {
                // Return Ok(true) to avoid email enumeration — caller shows same message
                tracing::warn!(email = %email_lower, "OTP skipped — user not found and self-registration is disabled");
                return Ok(true);
            }
        }

        // Generate 6-digit code
        let code = generate_otp_code();

        // Atomic rate-limit check + insert under a single write lock
        {
            let mut otps = self.pending_otps.write().await;
            if let Some(existing) = otps.get(&email_lower) {
                let elapsed = now.signed_duration_since(existing.created_at);
                if elapsed < Duration::seconds(60) {
                    return Ok(false);
                }
            }
            // Insert pending OTP only after rate-limit passes
            otps.insert(
                email_lower.clone(),
                PendingOtp {
                    code: code.clone(),
                    created_at: now,
                    attempts: 0,
                },
            );
        }

        // Send email — if it fails, remove the OTP so user isn't stuck rate-limited
        if let Some(ref smtp) = self.smtp_config {
            if let Err(e) = self.send_otp_email(smtp, &email_lower, &code).await {
                let mut otps = self.pending_otps.write().await;
                otps.remove(&email_lower);
                return Err(e);
            }
        } else {
            // Dev mode: log to console — email will NOT be delivered
            tracing::debug!(email = %email_lower, code = %code, "OTP code (no SMTP configured) — configure dashboard_auth.smtp to send emails");
            tracing::warn!(email = %email_lower, "OTP generated (code redacted, enable debug logging to see) — configure dashboard_auth.smtp to send emails");
        }

        Ok(true)
    }

    /// Verify OTP code. Returns session token on success.
    pub async fn verify_otp(&self, email: &str, code: &str) -> Result<Option<String>> {
        let email_lower = email.to_lowercase();
        let now = Utc::now();

        let valid = {
            let mut otps = self.pending_otps.write().await;
            let otp = match otps.get_mut(&email_lower) {
                Some(otp) => otp,
                None => return Ok(None),
            };

            // Check expiry (5 minutes)
            if now.signed_duration_since(otp.created_at) > Duration::minutes(5) {
                otps.remove(&email_lower);
                return Ok(None);
            }

            // Check max attempts
            otp.attempts += 1;
            if otp.attempts > 3 {
                otps.remove(&email_lower);
                return Ok(None);
            }

            // Verify code (constant-time comparison)
            let code_matches = constant_time_eq(code.as_bytes(), otp.code.as_bytes());
            if code_matches {
                otps.remove(&email_lower);
            }
            code_matches
        };

        if !valid {
            return Ok(None);
        }

        // Check if user exists (or create if self-registration enabled)
        let user = self.user_store.get_by_email(&email_lower)?;
        let user = match user {
            Some(u) => u,
            None => {
                if !self.allow_self_registration {
                    bail!("no account found for this email");
                }
                // Auto-create user
                let id = crate::user_store::email_to_user_id(&email_lower);
                // Ensure unique ID
                let mut final_id = id.clone();
                let mut suffix = 1u32;
                while self.user_store.get(&final_id)?.is_some() {
                    final_id = format!("{id}-{suffix}");
                    suffix += 1;
                }
                let new_user = crate::user_store::User {
                    id: final_id,
                    email: email_lower.clone(),
                    name: email_lower.split('@').next().unwrap_or("User").to_string(),
                    role: UserRole::User,
                    created_at: now,
                    last_login_at: Some(now),
                };
                self.user_store.save(&new_user)?;
                new_user
            }
        };

        // Update last_login_at
        let mut updated_user = user.clone();
        updated_user.last_login_at = Some(now);
        self.user_store.save(&updated_user)?;

        // Generate session token
        let token = generate_session_token();
        let session = ActiveSession {
            user_id: user.id.clone(),
            role: user.role.clone(),
            created_at: now,
            expires_at: now + Duration::hours(self.session_expiry_hours as i64),
        };

        {
            let mut sessions = self.sessions.write().await;
            sessions.insert(token.clone(), session);
            if let Some(ref path) = self.sessions_path {
                Self::persist_sessions_blocking(&sessions, path);
            }
        }

        Ok(Some(token))
    }

    /// Validate a session token. Returns (user_id, role) if valid.
    /// Re-reads user role from disk to reflect role changes / deletions.
    pub async fn validate_session(&self, token: &str) -> Option<(String, UserRole)> {
        let user_id = {
            let sessions = self.sessions.read().await;
            let session = sessions.get(token)?;
            if Utc::now() > session.expires_at {
                drop(sessions);
                // Eagerly remove expired session
                let mut sessions = self.sessions.write().await;
                sessions.remove(token);
                if let Some(ref path) = self.sessions_path {
                    Self::persist_sessions_blocking(&sessions, path);
                }
                return None;
            }
            session.user_id.clone()
        };

        // Re-check that user still exists and get current role
        match self.user_store.get(&user_id) {
            Ok(Some(user)) => Some((user.id, user.role)),
            _ => {
                // User deleted — revoke session
                let mut sessions = self.sessions.write().await;
                sessions.remove(token);
                None
            }
        }
    }

    /// Revoke a session token.
    pub async fn revoke_session(&self, token: &str) {
        let mut sessions = self.sessions.write().await;
        sessions.remove(token);
        if let Some(ref path) = self.sessions_path {
            Self::persist_sessions_blocking(&sessions, path);
        }
    }

    /// Clean up expired OTPs and sessions.
    pub async fn cleanup(&self) {
        let now = Utc::now();

        {
            let mut otps = self.pending_otps.write().await;
            otps.retain(|_, otp| now.signed_duration_since(otp.created_at) < Duration::minutes(10));
        }

        {
            let mut sessions = self.sessions.write().await;
            let before = sessions.len();
            sessions.retain(|_, session| now < session.expires_at);
            if sessions.len() != before {
                if let Some(ref path) = self.sessions_path {
                    Self::persist_sessions_blocking(&sessions, path);
                }
            }
        }
    }

    /// Send OTP email via SMTP.
    #[cfg(feature = "api")]
    async fn send_otp_email(&self, smtp: &SmtpConfig, email: &str, code: &str) -> Result<()> {
        use lettre::message::header::ContentType;
        use lettre::transport::smtp::authentication::Credentials;
        use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

        let password = std::env::var(&smtp.password_env)
            .ok()
            .or_else(|| self.smtp_password.clone())
            .ok_or_else(|| {
                eyre::eyre!(
                    "SMTP password not found in env var '{}' or profile env_vars",
                    smtp.password_env,
                )
            })?;

        let email_msg = Message::builder()
            .from(smtp.from_address.parse().map_err(|e| eyre::eyre!("invalid from address: {e}"))?)
            .to(email.parse().map_err(|e| eyre::eyre!("invalid to address: {e}"))?)
            .subject("Your octos login code")
            .header(ContentType::TEXT_HTML)
            .body(format!(
                r#"<div style="font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif; max-width: 480px; margin: 0 auto; padding: 40px 20px;">
                    <h2 style="color: #1a1a2e; margin-bottom: 8px;">octos Login</h2>
                    <p style="color: #666; margin-bottom: 24px;">Use this code to sign in. It expires in 5 minutes.</p>
                    <div style="background: #f5f5f5; border-radius: 8px; padding: 24px; text-align: center; margin-bottom: 24px;">
                        <span style="font-size: 32px; font-weight: bold; letter-spacing: 8px; color: #1a1a2e;">{code}</span>
                    </div>
                    <p style="color: #999; font-size: 12px;">If you didn't request this code, you can safely ignore this email.</p>
                </div>"#
            ))
            .map_err(|e| eyre::eyre!("failed to build email: {e}"))?;

        let creds = Credentials::new(smtp.username.clone(), password);

        let mailer = if smtp.port == 465 {
            AsyncSmtpTransport::<Tokio1Executor>::relay(&smtp.host)
                .map_err(|e| eyre::eyre!("SMTP relay error: {e}"))?
                .credentials(creds)
                .build()
        } else {
            AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&smtp.host)
                .map_err(|e| eyre::eyre!("SMTP STARTTLS error: {e}"))?
                .credentials(creds)
                .build()
        };

        mailer
            .send(email_msg)
            .await
            .map_err(|e| eyre::eyre!("failed to send OTP email: {e}"))?;
        tracing::info!(email = %email, "OTP email sent");
        Ok(())
    }

    #[cfg(not(feature = "api"))]
    async fn send_otp_email(&self, _smtp: &SmtpConfig, email: &str, _code: &str) -> Result<()> {
        tracing::info!(email = %email, "OTP sent (code redacted, lettre not available)");
        Ok(())
    }
}

/// Generate a 6-digit numeric OTP code.
#[cfg(feature = "api")]
fn generate_otp_code() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let code: u32 = rng.gen_range(100_000..1_000_000);
    format!("{code:06}")
}

#[cfg(not(feature = "api"))]
fn generate_otp_code() -> String {
    // Deterministic fallback — OTP is only meaningful with the API server.
    "000000".to_string()
}

/// Generate a 32-byte hex session token.
#[cfg(feature = "api")]
fn generate_session_token() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let mut bytes = [0u8; 32];
    rng.fill(&mut bytes);
    hex_encode(&bytes)
}

#[cfg(not(feature = "api"))]
fn generate_session_token() -> String {
    // Deterministic fallback — sessions are only meaningful with the API server.
    hex_encode(&[0u8; 32])
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Constant-time byte comparison (no length leak).
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let len_eq = a.len() ^ b.len();
    let mut result = 0u8;
    // Always iterate over both, using modular index to avoid early exit
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        result |= x ^ y;
    }
    result == 0 && len_eq == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(feature = "api")]
    fn test_generate_otp_code() {
        let code = generate_otp_code();
        assert_eq!(code.len(), 6);
        assert!(code.chars().all(|c| c.is_ascii_digit()));
        let n: u32 = code.parse().unwrap();
        assert!(n >= 100_000 && n < 1_000_000);
    }

    #[test]
    #[cfg(feature = "api")]
    fn test_generate_session_token() {
        let token = generate_session_token();
        assert_eq!(token.len(), 64);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_constant_time_eq() {
        assert!(constant_time_eq(b"123456", b"123456"));
        assert!(!constant_time_eq(b"123456", b"654321"));
        assert!(!constant_time_eq(b"short", b"longer"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn test_hex_encode() {
        assert_eq!(hex_encode(&[0x00, 0xff, 0x42]), "00ff42");
    }

    #[tokio::test]
    async fn test_otp_flow() {
        let dir = tempfile::tempdir().unwrap();
        let user_store = Arc::new(UserStore::open(dir.path()).unwrap());

        // Create a user first
        let user = crate::user_store::User {
            id: "alice".into(),
            email: "alice@example.com".into(),
            name: "Alice".into(),
            role: UserRole::User,
            created_at: Utc::now(),
            last_login_at: None,
        };
        user_store.save(&user).unwrap();

        let mgr = AuthManager::new(None, user_store);

        // Send OTP (no SMTP, logs to console)
        assert!(mgr.send_otp("alice@example.com").await.unwrap());

        // Get the code from pending_otps
        let code = {
            let otps = mgr.pending_otps.read().await;
            otps.get("alice@example.com").unwrap().code.clone()
        };

        // Wrong code
        assert!(
            mgr.verify_otp("alice@example.com", "000000")
                .await
                .unwrap()
                .is_none()
        );

        // Correct code
        let token = mgr.verify_otp("alice@example.com", &code).await.unwrap();
        assert!(token.is_some());

        // Validate session
        let token = token.unwrap();
        let (user_id, role) = mgr.validate_session(&token).await.unwrap();
        assert_eq!(user_id, "alice");
        assert_eq!(role, UserRole::User);

        // Revoke
        mgr.revoke_session(&token).await;
        assert!(mgr.validate_session(&token).await.is_none());
    }

    #[tokio::test]
    async fn test_otp_rate_limit() {
        let dir = tempfile::tempdir().unwrap();
        let user_store = Arc::new(UserStore::open(dir.path()).unwrap());
        let user = crate::user_store::User {
            id: "bob".into(),
            email: "bob@test.com".into(),
            name: "Bob".into(),
            role: UserRole::User,
            created_at: Utc::now(),
            last_login_at: None,
        };
        user_store.save(&user).unwrap();

        let mgr = AuthManager::new(None, user_store);

        // First send succeeds
        assert!(mgr.send_otp("bob@test.com").await.unwrap());
        // Second send within 60s is rate-limited
        assert!(!mgr.send_otp("bob@test.com").await.unwrap());
    }

    #[tokio::test]
    async fn test_otp_max_attempts() {
        let dir = tempfile::tempdir().unwrap();
        let user_store = Arc::new(UserStore::open(dir.path()).unwrap());
        let user = crate::user_store::User {
            id: "carol".into(),
            email: "carol@test.com".into(),
            name: "Carol".into(),
            role: UserRole::User,
            created_at: Utc::now(),
            last_login_at: None,
        };
        user_store.save(&user).unwrap();

        let mgr = AuthManager::new(None, user_store);
        mgr.send_otp("carol@test.com").await.unwrap();

        // Use wrong code 3 times
        for _ in 0..3 {
            assert!(
                mgr.verify_otp("carol@test.com", "000000")
                    .await
                    .unwrap()
                    .is_none()
            );
        }
        // 4th attempt fails (OTP removed after 3 attempts)
        assert!(
            mgr.verify_otp("carol@test.com", "000000")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn test_self_registration() {
        let dir = tempfile::tempdir().unwrap();
        let user_store = Arc::new(UserStore::open(dir.path()).unwrap());

        // Use None for smtp to trigger dev mode (console log instead of email)
        let mut mgr = AuthManager::new(None, user_store.clone());
        mgr.allow_self_registration = true;
        mgr.send_otp("newuser@example.com").await.unwrap();

        let code = {
            let otps = mgr.pending_otps.read().await;
            otps.get("newuser@example.com").unwrap().code.clone()
        };

        let token = mgr.verify_otp("newuser@example.com", &code).await.unwrap();
        assert!(token.is_some());

        // User should have been auto-created
        let user = user_store.get_by_email("newuser@example.com").unwrap();
        assert!(user.is_some());
        assert_eq!(user.unwrap().id, "newuser");
    }

    #[tokio::test]
    async fn test_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let user_store = Arc::new(UserStore::open(dir.path()).unwrap());
        let mgr = AuthManager::new(None, user_store);

        // Insert an expired session
        {
            let mut sessions = mgr.sessions.write().await;
            sessions.insert(
                "expired-token".into(),
                ActiveSession {
                    user_id: "test".into(),
                    role: UserRole::User,
                    created_at: Utc::now() - Duration::hours(25),
                    expires_at: Utc::now() - Duration::hours(1),
                },
            );
        }

        mgr.cleanup().await;

        let sessions = mgr.sessions.read().await;
        assert!(sessions.is_empty());
    }
}
