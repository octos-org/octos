//! Email OTP authentication and session management.
//!
//! Provides Larksuite-style email verification: user enters email, receives a
//! 6-digit code, verifies it, and gets a session token. Sessions are stored
//! in-memory with configurable expiry.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::{DateTime, Duration, Utc};
use eyre::{Result, bail};
use octos_core::constant_time_eq;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::user_store::{UserRole, UserStore};

pub use octos_services::config::{DashboardAuthConfig, SmtpConfig};

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

#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TestEmail {
    pub to: String,
    pub subject: String,
    pub html: String,
}

/// `(candidate_id, authorized_for_candidate) -> taken`. See
/// [`AuthManager::with_id_taken_probe`].
pub type IdTakenProbe = Arc<dyn Fn(&str, bool) -> bool + Send + Sync>;

/// OTP and session manager with optional disk persistence.
pub struct AuthManager {
    pending_otps: RwLock<HashMap<String, PendingOtp>>,
    sessions: RwLock<HashMap<String, ActiveSession>>,
    smtp_config: RwLock<Option<SmtpConfig>>,
    /// Pre-resolved SMTP password from the selected profile email settings or
    /// keychain. Preferred over process env so stale launch env cannot shadow
    /// dashboard-configured SMTP credentials.
    smtp_password: Option<String>,
    session_expiry_hours: u64,
    /// Whether unknown emails auto-register on first OTP verify. Stored as
    /// AtomicBool so the admin SMTP-save endpoint can flip it without a
    /// server restart (matches the hot-reload behaviour of `smtp_config`).
    /// Read via `allow_self_registration()`, write via
    /// `set_allow_self_registration()`.
    allow_self_registration_flag: AtomicBool,
    /// Static tokens that bypass OTP (for E2E testing).
    pub static_tokens: Vec<String>,
    user_store: Arc<UserStore>,
    /// Optional probe for registration ids that are taken OUTSIDE the
    /// user store — `(candidate, authorized_for_candidate) -> taken`.
    /// The serve bootstrap wires this to
    /// `ProfileStore::id_reserved_for_registration`, so a generated id
    /// can never claim an admin-created (but unclaimed) profile:
    /// /api/auth/verify treats an existing profile under the new
    /// user's id as that user's own (codex #1613 r6). The `authorized`
    /// flag is true ONLY for the exact allowlist-derived id (suffix
    /// candidates from collisions carry no authorization — r8 P1),
    /// and even then the store keeps unloadable records reserved so a
    /// claim can't clobber a corrupt profile file (r8 P2). `None`
    /// (tests, solo) checks the user store only.
    id_taken_probe: Option<IdTakenProbe>,
    /// Path to persist sessions. `None` = in-memory only (tests).
    sessions_path: Option<PathBuf>,
    /// Data directory used to load the SMTP password from `smtp_secret.json`.
    data_dir: Option<PathBuf>,
    #[cfg(test)]
    sent_emails: RwLock<Vec<TestEmail>>,
}

impl AuthManager {
    pub fn new(config: Option<DashboardAuthConfig>, user_store: Arc<UserStore>) -> Self {
        let (smtp_config, session_expiry_hours, allow_self_registration, static_tokens) =
            match config {
                Some(c) => (
                    c.smtp,
                    c.session_expiry_hours,
                    c.allow_self_registration,
                    c.static_tokens,
                ),
                None => (None, 24, false, Vec::new()),
            };
        Self {
            pending_otps: RwLock::new(HashMap::new()),
            sessions: RwLock::new(HashMap::new()),
            smtp_config: RwLock::new(smtp_config),
            smtp_password: None,
            session_expiry_hours,
            allow_self_registration_flag: AtomicBool::new(allow_self_registration),
            static_tokens,
            user_store,
            sessions_path: None,
            data_dir: None,
            id_taken_probe: None,
            #[cfg(test)]
            sent_emails: RwLock::new(Vec::new()),
        }
    }

    /// Attach a data directory so the manager can resolve the SMTP password
    /// from `{data_dir}/smtp_secret.json` in preference to the env var.
    pub fn with_data_dir(mut self, data_dir: PathBuf) -> Self {
        self.data_dir = Some(data_dir);
        self
    }

    /// Attach the external id-taken probe used by registration id
    /// generation (see the field docs — codex #1613 r6/r8).
    pub fn with_id_taken_probe(mut self, probe: IdTakenProbe) -> Self {
        self.id_taken_probe = Some(probe);
        self
    }

    /// Set an explicit SMTP password (resolved from profile email settings or
    /// keychain). Used as fallback when neither the file store nor the process
    /// environment has the password.
    pub fn with_smtp_password(mut self, password: String) -> Self {
        self.smtp_password = Some(password);
        self
    }

    // The real non-test SMTP transport uses this; the test transport records
    // messages without authenticating, so the method is intentionally unused
    // in the lib test target.
    #[cfg_attr(test, allow(dead_code))]
    fn resolved_smtp_password(&self, smtp: &SmtpConfig) -> Option<String> {
        resolve_smtp_password(self.smtp_password.as_deref(), &smtp.password_env, |name| {
            std::env::var(name).ok()
        })
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

    #[cfg(test)]
    pub async fn test_sent_emails(&self) -> Vec<TestEmail> {
        self.sent_emails.read().await.clone()
    }

    #[cfg(test)]
    pub async fn test_pending_code(&self, email: &str, user_id: Option<&str>) -> Option<String> {
        let key = Self::otp_key(&email.to_lowercase(), user_id);
        self.pending_otps
            .read()
            .await
            .get(&key)
            .map(|otp| otp.code.clone())
    }

    /// Persist current sessions to disk (best-effort).
    fn persist_sessions_blocking(sessions: &HashMap<String, ActiveSession>, path: &PathBuf) {
        if let Ok(data) = serde_json::to_string(sessions) {
            let _ = std::fs::write(path, data);
        }
    }

    fn otp_key(email: &str, user_id: Option<&str>) -> String {
        match user_id {
            Some(user_id) => format!("{email}::{user_id}"),
            None => email.to_string(),
        }
    }

    async fn store_pending_otp(&self, otp_key: &str) -> Result<bool> {
        let now = Utc::now();
        let code = generate_otp_code();
        let mut otps = self.pending_otps.write().await;
        if let Some(existing) = otps.get(otp_key) {
            let elapsed = now.signed_duration_since(existing.created_at);
            if elapsed < Duration::seconds(60) {
                return Ok(false);
            }
        }
        otps.insert(
            otp_key.to_string(),
            PendingOtp {
                code,
                created_at: now,
                attempts: 0,
            },
        );
        Ok(true)
    }

    async fn verify_pending_otp(&self, otp_key: &str, code: &str) -> Result<bool> {
        let now = Utc::now();
        let valid = {
            let mut otps = self.pending_otps.write().await;
            let otp = match otps.get_mut(otp_key) {
                Some(otp) => otp,
                None => return Ok(false),
            };

            if now.signed_duration_since(otp.created_at) > Duration::minutes(5) {
                otps.remove(otp_key);
                return Ok(false);
            }

            otp.attempts += 1;
            if otp.attempts > 3 {
                otps.remove(otp_key);
                return Ok(false);
            }

            let code_matches = constant_time_eq(code.as_bytes(), otp.code.as_bytes());
            if code_matches {
                otps.remove(otp_key);
            }
            code_matches
        };

        Ok(valid)
    }

    /// Send an arbitrary HTML email through the configured SMTP transport.
    ///
    /// Returns `Ok(true)` when an email was sent, `Ok(false)` when SMTP is not
    /// configured and the email was intentionally skipped.
    pub async fn send_html_email(&self, email: &str, subject: &str, html: &str) -> Result<bool> {
        let smtp = self.smtp_config.read().await.clone();
        let Some(smtp) = smtp else {
            tracing::warn!(email = %email, subject = %subject, "email skipped — no SMTP configured");
            return Ok(false);
        };

        self.deliver_html_email(&smtp, email, subject, html).await?;
        Ok(true)
    }

    /// Replace the in-memory SMTP config (used by the admin settings endpoint
    /// after writing new values to disk). Pass `None` to clear.
    pub async fn set_smtp_config(&self, cfg: Option<SmtpConfig>) {
        *self.smtp_config.write().await = cfg;
    }

    /// Whether SMTP delivery is currently usable. Returns true only when a
    /// host is set; password presence is checked at send time. Used by
    /// auth/status to drive `email_login_enabled` honestly and by send_code
    /// to short-circuit with a server-state error when SMTP isn't configured
    /// (vs. silently logging the OTP to the server console).
    pub async fn smtp_configured(&self) -> bool {
        match &*self.smtp_config.read().await {
            Some(cfg) => !cfg.host.trim().is_empty(),
            None => false,
        }
    }

    /// Read the current `allow_self_registration` flag. Used by send_code,
    /// verify, and auth_status to decide whether unknown emails can
    /// auto-register.
    pub fn allow_self_registration(&self) -> bool {
        self.allow_self_registration_flag.load(Ordering::Relaxed)
    }

    /// Live-update the `allow_self_registration` flag (admin save path).
    /// Takes effect immediately; no `octos serve` restart required.
    pub fn set_allow_self_registration(&self, value: bool) {
        self.allow_self_registration_flag
            .store(value, Ordering::Relaxed);
    }

    /// Generate and send OTP to email. Returns Ok(true) if sent, Ok(false) if rate-limited.
    pub async fn send_otp(&self, email: &str) -> Result<bool> {
        self.send_otp_with_registration(email, self.allow_self_registration())
            .await
    }

    pub async fn send_otp_with_registration(
        &self,
        email: &str,
        allow_registration: bool,
    ) -> Result<bool> {
        let email_lower = email.to_lowercase();

        if !allow_registration {
            let user = self.user_store.get_by_email(&email_lower)?;
            if user.is_none() {
                tracing::warn!(email = %email_lower, "OTP skipped — user not found and self-registration is disabled");
                return Ok(true);
            }
        }

        let otp_key = Self::otp_key(&email_lower, None);
        let should_send = self.store_pending_otp(&otp_key).await?;
        if !should_send {
            return Ok(false);
        }

        let smtp = self.smtp_config.read().await.clone();
        if let Some(smtp) = smtp {
            let code = {
                let otps = self.pending_otps.read().await;
                otps.get(&otp_key).map(|otp| otp.code.clone())
            };
            if let Some(code) = code {
                if let Err(e) = self.send_otp_email(&smtp, &email_lower, &code).await {
                    let mut otps = self.pending_otps.write().await;
                    otps.remove(&otp_key);
                    return Err(e);
                }
            } else {
                let mut otps = self.pending_otps.write().await;
                otps.remove(&otp_key);
                bail!("pending OTP missing before email send");
            }
        } else {
            let code = {
                let otps = self.pending_otps.read().await;
                otps.get(&otp_key)
                    .map(|otp| otp.code.clone())
                    .unwrap_or_default()
            };
            tracing::debug!(email = %email_lower, code = %code, "OTP code (no SMTP configured) — configure dashboard_auth.smtp to send emails");
            tracing::warn!(email = %email_lower, "OTP generated (code redacted, enable debug logging to see) — configure dashboard_auth.smtp to send emails");
        }

        Ok(true)
    }

    pub async fn send_otp_for_user(&self, email: &str, user_id: &str) -> Result<bool> {
        let email_lower = email.to_lowercase();
        let user = self
            .user_store
            .get(user_id)?
            .ok_or_else(|| eyre::eyre!("user '{user_id}' not found"))?;
        if user.email.to_lowercase() != email_lower {
            bail!("email '{email_lower}' is not registered to user '{user_id}'");
        }

        let otp_key = Self::otp_key(&email_lower, Some(user_id));
        let should_send = self.store_pending_otp(&otp_key).await?;
        if !should_send {
            return Ok(false);
        }

        let smtp = self.smtp_config.read().await.clone();
        if let Some(smtp) = smtp {
            let code = {
                let otps = self.pending_otps.read().await;
                otps.get(&otp_key).map(|otp| otp.code.clone())
            };
            if let Some(code) = code {
                if let Err(e) = self.send_otp_email(&smtp, &email_lower, &code).await {
                    let mut otps = self.pending_otps.write().await;
                    otps.remove(&otp_key);
                    return Err(e);
                }
            } else {
                let mut otps = self.pending_otps.write().await;
                otps.remove(&otp_key);
                bail!("pending OTP missing before email send");
            }
        } else {
            let code = {
                let otps = self.pending_otps.read().await;
                otps.get(&otp_key)
                    .map(|otp| otp.code.clone())
                    .unwrap_or_default()
            };
            tracing::debug!(email = %email_lower, user_id = %user_id, code = %code, "OTP code (no SMTP configured) — configure dashboard_auth.smtp to send emails");
            tracing::warn!(email = %email_lower, user_id = %user_id, "OTP generated (code redacted, enable debug logging to see) — configure dashboard_auth.smtp to send emails");
        }

        Ok(true)
    }

    /// Verify OTP code. Returns session token on success.
    /// Check if a code is a static token (bypasses OTP for E2E testing).
    pub fn is_static_token(&self, code: &str) -> bool {
        self.static_tokens
            .iter()
            .any(|t| !t.is_empty() && t == code)
    }

    pub async fn verify_otp(&self, email: &str, code: &str) -> Result<Option<String>> {
        self.verify_otp_with_registration(email, code, self.allow_self_registration())
            .await
    }

    /// Verify with ANONYMOUS registration semantics: a generated id
    /// must not claim an existing-but-unclaimed profile (the
    /// `id_taken_probe` applies — codex #1613 r6).
    pub async fn verify_otp_with_registration(
        &self,
        email: &str,
        code: &str,
        allow_registration: bool,
    ) -> Result<Option<String>> {
        self.verify_otp_registration_inner(email, code, allow_registration, false)
            .await
    }

    /// Verify with AUTHORIZED-CLAIM semantics (codex #1613 r7 P1): the
    /// caller has explicit provenance — an allowlist entry — that this
    /// email is entitled to its derived id, so an existing unclaimed
    /// profile under that id is the invitee's pre-provisioned profile,
    /// not a squat target. The profile probe is skipped; user-row
    /// collisions still advance the suffix (a user row means the id
    /// belongs to a DIFFERENT account).
    pub async fn verify_otp_with_authorized_claim(
        &self,
        email: &str,
        code: &str,
    ) -> Result<Option<String>> {
        self.verify_otp_registration_inner(email, code, true, true)
            .await
    }

    async fn verify_otp_registration_inner(
        &self,
        email: &str,
        code: &str,
        allow_registration: bool,
        authorized_claim: bool,
    ) -> Result<Option<String>> {
        let email_lower = email.to_lowercase();
        let now = Utc::now();

        // Static tokens bypass OTP verification entirely (E2E testing).
        let is_static = self.is_static_token(code);

        if !is_static
            && !self
                .verify_pending_otp(&Self::otp_key(&email_lower, None), code)
                .await?
        {
            return Ok(None);
        }

        let user = self.user_store.get_by_email(&email_lower)?;
        let user = match user {
            Some(u) => u,
            None => {
                if !allow_registration {
                    bail!("no account found for this email");
                }
                let id = crate::user_store::email_to_user_id(&email_lower);
                let mut final_id = id.clone();
                let mut suffix = 1u32;
                // A candidate is taken when a user row exists OR the
                // bootstrap probe says the id is occupied elsewhere (a
                // profile file on disk): self-registration must never
                // claim an admin-created-but-unclaimed profile, or
                // /api/auth/verify hands the existing profile to the
                // new user (codex #1613 r6). Allowlist provenance
                // authorizes claiming ONLY the exact derived id — an
                // invitee keeps their pre-provisioned profile (r7 P1)
                // — but a suffix candidate minted after a collision
                // carries no such authorization, or the invitee would
                // silently absorb some OTHER unclaimed profile
                // (`alice-1`, r8 P1). The probe itself still reserves
                // unloadable records even for authorized claims
                // (r8 P2 — see id_reserved_for_registration).
                loop {
                    let user_taken = self.user_store.get(&final_id)?.is_some();
                    let candidate_authorized = authorized_claim && final_id == id;
                    let probe_taken = self
                        .id_taken_probe
                        .as_ref()
                        .is_some_and(|probe| probe(&final_id, candidate_authorized));
                    if !user_taken && !probe_taken {
                        break;
                    }
                    // Reserve room for `-N` within the 63-char slug
                    // budget (matching email_to_user_id's own cap): a
                    // 63-char base id would otherwise mint a 65-char
                    // candidate that validate_user_id rejects, failing
                    // a VALID OTP with a generic error (codex #1613
                    // r7 P2). Trailing hyphens from the cut are
                    // trimmed so the candidate stays slug-shaped.
                    let suffix_str = suffix.to_string();
                    let max_base = 63usize.saturating_sub(suffix_str.len() + 1);
                    let base: String = id.chars().take(max_base).collect();
                    let base = base.trim_end_matches('-');
                    final_id = format!("{base}-{suffix_str}");
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

        let mut updated_user = user.clone();
        updated_user.last_login_at = Some(now);
        self.user_store.save(&updated_user)?;

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

    pub async fn verify_otp_for_user(
        &self,
        email: &str,
        code: &str,
        user_id: &str,
    ) -> Result<Option<String>> {
        let email_lower = email.to_lowercase();
        let is_static = self.is_static_token(code);
        let otp_key = Self::otp_key(&email_lower, Some(user_id));
        if !is_static && !self.verify_pending_otp(&otp_key, code).await? {
            return Ok(None);
        }

        let now = Utc::now();
        let user = self
            .user_store
            .get(user_id)?
            .ok_or_else(|| eyre::eyre!("user '{user_id}' not found"))?;
        if user.email.to_lowercase() != email_lower {
            bail!("email '{email_lower}' is not registered to user '{user_id}'");
        }

        let mut updated_user = user.clone();
        updated_user.last_login_at = Some(now);
        self.user_store.save(&updated_user)?;

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

    /// Mint a session token for a known user WITHOUT an OTP challenge.
    ///
    /// This backs the loopback-only solo (no-password) login path. It performs
    /// NO authorization of its own — the caller is responsible for gating
    /// (`deployment_mode == Local` AND a loopback peer). Treat a call to this
    /// as "the caller has already proven this is a local solo box."
    ///
    /// `user_id`/`role` are recorded on the session as-is; `validate_session`
    /// later re-reads the live role from the user store, so a stale role here
    /// cannot escalate privileges.
    pub async fn create_session_for_user(&self, user_id: &str, role: UserRole) -> Result<String> {
        let now = Utc::now();
        // Record the login on the user, matching the OTP paths, so admin /
        // user audit views show an accurate last_login_at after a solo login.
        if let Some(mut user) = self.user_store.get(user_id)? {
            user.last_login_at = Some(now);
            self.user_store.save(&user)?;
        }
        let token = generate_session_token();
        let session = ActiveSession {
            user_id: user_id.to_owned(),
            role,
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

        Ok(token)
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
        let html = format!(
            r#"<div style="font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif; max-width: 480px; margin: 0 auto; padding: 40px 20px;">
                    <h2 style="color: #1a1a2e; margin-bottom: 8px;">octos Login</h2>
                    <p style="color: #666; margin-bottom: 24px;">Use this code to sign in. It expires in 5 minutes.</p>
                    <div style="background: #f5f5f5; border-radius: 8px; padding: 24px; text-align: center; margin-bottom: 24px;">
                        <span style="font-size: 32px; font-weight: bold; letter-spacing: 8px; color: #1a1a2e;">{code}</span>
                    </div>
                    <p style="color: #999; font-size: 12px;">If you didn't request this code, you can safely ignore this email.</p>
                </div>"#
        );
        self.deliver_html_email(smtp, email, "Your octos login code", &html)
            .await
    }

    #[cfg(all(feature = "api", test))]
    async fn deliver_html_email(
        &self,
        _smtp: &SmtpConfig,
        email: &str,
        subject: &str,
        html: &str,
    ) -> Result<()> {
        self.sent_emails.write().await.push(TestEmail {
            to: email.to_string(),
            subject: subject.to_string(),
            html: html.to_string(),
        });
        Ok(())
    }

    #[cfg(all(feature = "api", not(test)))]
    async fn deliver_html_email(
        &self,
        smtp: &SmtpConfig,
        email: &str,
        subject: &str,
        html: &str,
    ) -> Result<()> {
        use lettre::message::header::ContentType;
        use lettre::transport::smtp::authentication::Credentials;
        use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

        let password = self
            .data_dir
            .as_deref()
            .and_then(|dir| {
                crate::smtp_secret_store::SmtpSecretStore::new(dir)
                    .load()
                    .unwrap_or_else(|e| {
                        tracing::warn!(error = %e, "failed to read smtp_secret.json; falling back to other sources");
                        None
                    })
            })
            .or_else(|| self.resolved_smtp_password(smtp))
            .ok_or_else(|| {
                eyre::eyre!(
                    "SMTP password not found in smtp_secret.json, env var '{}', or profile env_vars",
                    smtp.password_env,
                )
            })?;

        let email_msg = Message::builder()
            .from(
                smtp.from_address
                    .parse()
                    .map_err(|e| eyre::eyre!("invalid from address: {e}"))?,
            )
            .to(email
                .parse()
                .map_err(|e| eyre::eyre!("invalid to address: {e}"))?)
            .subject(subject)
            .header(ContentType::TEXT_HTML)
            .body(html.to_string())
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
            .map_err(|e| eyre::eyre!("failed to send email: {e}"))?;
        tracing::info!(email = %email, subject = %subject, "email sent");
        Ok(())
    }

    #[cfg(not(feature = "api"))]
    async fn send_otp_email(&self, _smtp: &SmtpConfig, email: &str, _code: &str) -> Result<()> {
        tracing::info!(email = %email, "OTP sent (code redacted, lettre not available)");
        Ok(())
    }

    #[cfg(not(feature = "api"))]
    async fn deliver_html_email(
        &self,
        _smtp: &SmtpConfig,
        email: &str,
        subject: &str,
        _html: &str,
    ) -> Result<()> {
        tracing::info!(email = %email, subject = %subject, "email sent (content redacted, lettre not available)");
        Ok(())
    }
}

#[cfg_attr(test, allow(dead_code))]
fn non_empty_secret(value: String) -> Option<String> {
    if value.trim().is_empty() {
        None
    } else {
        Some(value)
    }
}

#[cfg_attr(test, allow(dead_code))]
fn resolve_smtp_password(
    profile_password: Option<&str>,
    password_env: &str,
    get_env: impl FnOnce(&str) -> Option<String>,
) -> Option<String> {
    profile_password
        .map(str::to_string)
        .and_then(non_empty_secret)
        .or_else(|| get_env(password_env).and_then(non_empty_secret))
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
