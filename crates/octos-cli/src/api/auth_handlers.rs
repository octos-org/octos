//! Authentication and user self-service API handlers.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, Weak};

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::AppState;
use super::admin::ProfileResponse;
use super::handlers::response_path_for_profile_file;
use crate::profiles::{ChannelCredentials, UserProfile, is_display_secret_value, mask_secrets};
use crate::user_store::{User, UserRole};

use super::router::AuthIdentity;

/// In-memory rate limiter for OTP send requests: email -> (count, window_start).
/// Allows at most 3 requests per 5-minute window per email address.
static OTP_RATE_LIMIT: LazyLock<Mutex<HashMap<String, (u32, std::time::Instant)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
const OTP_RATE_LIMIT_WINDOW: std::time::Duration = std::time::Duration::from_secs(300);
const OTP_RATE_LIMIT_MAX_KEYS: usize = 4096;

fn otp_rate_limit_exceeded(
    limits: &mut HashMap<String, (u32, std::time::Instant)>,
    rate_limit_key: String,
    now: std::time::Instant,
) -> bool {
    // Unknown addresses are deliberately rate-limited too, but they are
    // attacker-controlled. Prune expired buckets before enforcing a hard cap
    // so unique probes cannot grow this process-global map without bound.
    if limits.len() >= OTP_RATE_LIMIT_MAX_KEYS {
        limits.retain(|_, (_, started_at)| {
            now.saturating_duration_since(*started_at) < OTP_RATE_LIMIT_WINDOW
        });
    }
    if !limits.contains_key(&rate_limit_key) && limits.len() >= OTP_RATE_LIMIT_MAX_KEYS {
        return true;
    }

    let entry = limits.entry(rate_limit_key).or_insert((0, now));
    if now.saturating_duration_since(entry.1) >= OTP_RATE_LIMIT_WINDOW {
        *entry = (0, now);
    }
    if entry.0 >= 3 {
        return true;
    }
    entry.0 += 1;
    false
}

pub(crate) fn is_top_level_profile_id(state: &AppState, profile_id: &str) -> bool {
    state
        .profile_store
        .as_ref()
        .and_then(|store| store.get(profile_id).ok().flatten())
        .map(|profile| profile.parent_id.is_none())
        .unwrap_or(false)
}

pub(crate) fn scoped_host_allows_profile_id(
    _state: &AppState,
    scoped_profile_id: &str,
    candidate_profile_id: &str,
) -> bool {
    scoped_profile_id == candidate_profile_id
}

fn request_host(headers: &HeaderMap) -> Option<String> {
    let raw = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get("host"))?
        .to_str()
        .ok()?
        .split(',')
        .next()?
        .trim()
        .to_ascii_lowercase();
    if raw.is_empty() {
        return None;
    }
    Some(strip_port_from_host(&raw).to_string())
}

/// The endpoint URL a scanning client should dial: original authority
/// (host AND port — `request_host` strips the port, which would send
/// scanners to :80/:443, codex P2) plus `X-Forwarded-Proto` when a
/// reverse proxy supplies it, else https with a loopback http fallback.
fn request_endpoint(headers: &HeaderMap) -> Option<String> {
    let authority = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get("host"))?
        .to_str()
        .ok()?
        .split(',')
        .next()?
        .trim()
        .to_ascii_lowercase();
    if authority.is_empty() {
        return None;
    }
    let forwarded_proto = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next())
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|p| p == "http" || p == "https");
    let scheme = forwarded_proto.unwrap_or_else(|| {
        let host_only = strip_port_from_host(&authority);
        if host_only == "localhost" || host_only.starts_with("127.") || host_only == "::1" {
            "http".to_string()
        } else {
            "https".to_string()
        }
    });
    Some(format!("{scheme}://{authority}"))
}

fn strip_port_from_host(host: &str) -> &str {
    if let Some(stripped) = host.strip_prefix('[') {
        return stripped.split(']').next().unwrap_or(host);
    }

    if host.matches(':').count() == 1 {
        return host.split(':').next().unwrap_or(host);
    }

    host
}

fn is_local_request_host(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

fn resolve_routed_profile_id_candidate(state: &AppState, candidate: &str) -> Option<String> {
    let candidate = candidate.trim();
    if candidate.is_empty()
        || matches!(
            candidate,
            "www" | "app" | "admin" | "api" | "crew" | "octos"
        )
    {
        return None;
    }

    state
        .profile_store
        .as_ref()
        .and_then(|store| store.resolve_routable_profile_id(candidate).ok().flatten())
}

fn resolve_trusted_local_profile_id_candidate(state: &AppState, candidate: &str) -> Option<String> {
    let candidate = candidate.trim();
    if candidate.is_empty() {
        return None;
    }

    resolve_routed_profile_id_candidate(state, candidate).or_else(|| {
        state
            .profile_store
            .as_ref()
            .and_then(|store| store.get(candidate).ok().flatten())
            .map(|profile| profile.id)
    })
}

fn host_scoped_profile_id(state: &AppState, headers: &HeaderMap) -> Option<String> {
    let host = request_host(headers)?;
    if is_local_request_host(&host) {
        return None;
    }

    let candidate = host.split('.').next()?;
    resolve_routed_profile_id_candidate(state, candidate)
}

fn trusted_auth_scope_profile_id(state: &AppState, headers: &HeaderMap) -> Option<String> {
    if let Some(profile_id) = host_scoped_profile_id(state, headers) {
        return Some(profile_id);
    }

    let host = request_host(headers)?;
    if !is_local_request_host(&host) {
        return None;
    }

    headers
        .get("x-profile-id")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(|candidate| resolve_trusted_local_profile_id_candidate(state, candidate))
}

fn resolve_scoped_login_user(
    state: &AppState,
    scoped_profile_id: &str,
    email: &str,
) -> Option<User> {
    let normalized = email.trim().to_lowercase();
    if scoped_profile_id.trim().is_empty() {
        return None;
    }

    let user_store = state.user_store.as_ref()?;
    let matches: Vec<User> = user_store
        .list()
        .ok()?
        .into_iter()
        .filter(|user| user.email.trim().to_lowercase() == normalized)
        .filter(|user| scoped_host_allows_profile_id(state, scoped_profile_id, &user.id))
        .collect();

    if matches.len() == 1 {
        matches.into_iter().next()
    } else {
        if matches.len() > 1 {
            tracing::warn!(
                email = %normalized,
                scoped_profile_id = %scoped_profile_id,
                count = matches.len(),
                "multiple scoped profiles share the same login email"
            );
        }
        None
    }
}

#[derive(Clone)]
enum RootLoginTarget {
    Registered(User),
    Allowlisted,
}

fn resolve_root_login_target(state: &AppState, email: &str) -> Option<RootLoginTarget> {
    let normalized = email.trim().to_lowercase();
    let user_store = state.user_store.as_ref()?;
    let matches: Vec<User> = user_store
        .list()
        .ok()?
        .into_iter()
        .filter(|user| user.email.trim().to_lowercase() == normalized)
        .collect();

    let top_level_matches: Vec<User> = matches
        .iter()
        .filter(|user| is_top_level_profile_id(state, &user.id))
        .cloned()
        .collect();

    if top_level_matches.len() == 1 {
        return top_level_matches
            .into_iter()
            .next()
            .map(RootLoginTarget::Registered);
    }

    if top_level_matches.len() > 1 {
        tracing::warn!(
            email = %normalized,
            count = top_level_matches.len(),
            "multiple top-level profiles share the same login email"
        );
        return None;
    }

    if !matches.is_empty() {
        tracing::warn!(
            email = %normalized,
            count = matches.len(),
            "root login rejected because email is only registered to scoped profiles"
        );
        return None;
    }

    match state.allowlist_store.as_ref() {
        Some(store) => match store.contains(&normalized) {
            Ok(true) => Some(RootLoginTarget::Allowlisted),
            Ok(false) => None,
            Err(error) => {
                tracing::warn!(email = %normalized, error = %error, "failed to read login allowlist");
                None
            }
        },
        None => None,
    }
}

pub(crate) fn is_login_ready_email(email: &str) -> bool {
    let normalized = email.trim().to_lowercase();
    !normalized.is_empty() && normalized != ADMIN_PLACEHOLDER_EMAIL
}

fn is_bootstrap_mode(state: &AppState) -> bool {
    let has_ready_user = state
        .user_store
        .as_ref()
        .and_then(|store| store.list().ok())
        .map(|users| {
            users.iter().any(|user| {
                is_login_ready_email(&user.email) && is_top_level_profile_id(state, &user.id)
            })
        })
        .unwrap_or(false);
    !has_ready_user
}

fn scoped_auth_target(state: &AppState, profile_id: &str) -> Option<ScopedAuthTarget> {
    if profile_id.is_empty() {
        return None;
    }

    let profile = state
        .profile_store
        .as_ref()
        .and_then(|store| store.get(profile_id).ok().flatten())?;
    let email_login_enabled = state
        .user_store
        .as_ref()
        .and_then(|store| store.list().ok())
        .map(|users| {
            users.iter().any(|user| {
                is_login_ready_email(&user.email)
                    && scoped_host_allows_profile_id(state, profile_id, &user.id)
            })
        })
        .unwrap_or(false);

    Some(ScopedAuthTarget {
        id: profile.id,
        name: profile.name,
        email_login_enabled,
    })
}

// ── Request / Response types ──────────────────────────────────────────

#[derive(Deserialize)]
pub struct SendCodeRequest {
    pub email: String,
}

#[derive(Serialize)]
pub struct SendCodeResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Deserialize)]
pub struct VerifyRequest {
    pub email: String,
    pub code: String,
}

#[derive(Serialize)]
pub struct VerifyResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<User>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PortalKind {
    BootstrapAdmin,
    Admin,
    Owner,
    SubAccount,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileRelationship {
    SelfProfile,
    ManagedChild,
    AdminManaged,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileApiScope {
    SelfService,
    SubAccount,
    Admin,
}

#[derive(Clone, Serialize)]
pub struct AccessibleProfileSummary {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    pub relationship: ProfileRelationship,
    pub api_scope: ProfileApiScope,
    pub route_base: String,
    pub can_manage_sub_accounts: bool,
}

#[derive(Clone, Serialize)]
pub struct PortalState {
    pub kind: PortalKind,
    pub home_profile_id: String,
    pub home_route: String,
    pub can_access_admin_portal: bool,
    pub can_manage_users: bool,
    pub sub_account_limit: usize,
    pub accessible_profiles: Vec<AccessibleProfileSummary>,
}

#[derive(Clone, Serialize)]
pub struct ScopedAuthTarget {
    pub id: String,
    pub name: String,
    pub email_login_enabled: bool,
}

#[derive(Serialize)]
pub struct AuthStatusResponse {
    pub bootstrap_mode: bool,
    pub email_login_enabled: bool,
    pub admin_token_login_enabled: bool,
    pub allow_self_registration: bool,
    /// True when this host advertises the no-password solo login path
    /// (`POST /api/auth/solo` / `/api/auth/solo/create`): a Local-mode
    /// deployment with profile + user stores. The SPA reads this to show
    /// the "continue without a password" affordance. Mirrors the TUI's
    /// `profile/local/create` capability gate (`supports_local_solo_profile_create`).
    /// The flag does NOT mean auth is bypassed — the endpoints still
    /// enforce a loopback peer at request time.
    pub local_solo_enabled: bool,
    /// When solo is advertised: whether a solo owner already exists. The
    /// SPA uses this to pick the first-run (create form) vs returning
    /// (one-click continue) experience WITHOUT a doomed solo-login round
    /// trip. Absent when solo is not advertised at all.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub solo_profile_exists: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scoped_profile: Option<ScopedAuthTarget>,
}

#[derive(Serialize)]
pub struct MeResponse {
    pub user: User,
    pub profile: Option<ProfileResponse>,
    pub portal: PortalState,
    /// If the request was made on a tenant subdomain (i.e.
    /// `host_scoped_profile_id` resolves), this is the tenant's profile
    /// summary. The dashboard uses this to hide admin-global navigation
    /// when an admin is operating in a tenant scope (Option Y, #315).
    /// `None` when no tenant subdomain is in scope (root domain, direct
    /// IP, or localhost).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scoped_profile: Option<ScopedAuthTarget>,
}

#[derive(Serialize)]
pub struct ActionResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

// ── Public auth endpoints (no auth required) ──────────────────────────

/// POST /api/auth/send-code
pub async fn send_code(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<SendCodeRequest>,
) -> Result<Json<SendCodeResponse>, StatusCode> {
    let auth_mgr = state
        .auth_manager
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    // Server-state precheck: if SMTP isn't configured the OTP code path will
    // silently log to the console (otp.rs::send_otp falls through to a debug
    // log when smtp_config is None). Surface that to the caller as a clear,
    // non-enumerating error — this branches on server state, not on whether
    // any specific email is registered, so it can't be used for account
    // enumeration. Operator fix is to configure SMTP via the wizard or
    // directly via POST /api/admin/smtp.
    if !auth_mgr.smtp_configured().await {
        tracing::warn!(
            email = %req.email,
            "send_code rejected — dashboard_auth.smtp is not configured on this server",
        );
        return Ok(Json(SendCodeResponse {
            ok: false,
            message: Some(
                "Email login is not available on this server because SMTP is not configured. Contact the administrator.".into(),
            ),
        }));
    }
    let requested_email = req.email.trim().to_lowercase();
    let scoped_profile_id = trusted_auth_scope_profile_id(&state, &headers);
    let scoped_login_target = scoped_profile_id
        .as_deref()
        .and_then(|profile_id| resolve_scoped_login_user(&state, profile_id, &requested_email));
    let root_login_target = if scoped_profile_id.is_none() {
        match resolve_root_login_target(&state, &requested_email) {
            Some(target) => Some(target),
            None if auth_mgr.allow_self_registration() => Some(RootLoginTarget::Allowlisted),
            None => None,
        }
    } else {
        None
    };

    // Rate-limit every request, including unknown/uninvited addresses. Keeping
    // this before the eligibility exits prevents a fast, unlimited probe path.
    // Max 3 requests per email per 5-minute window.
    {
        let mut limits = OTP_RATE_LIMIT.lock().unwrap_or_else(|e| e.into_inner());
        let rate_limit_key = scoped_login_target
            .as_ref()
            .map(|user| format!("{requested_email}::{}", user.id))
            .or_else(|| {
                root_login_target.as_ref().and_then(|target| match target {
                    RootLoginTarget::Registered(user) => {
                        Some(format!("{requested_email}::{}", user.id))
                    }
                    RootLoginTarget::Allowlisted => None,
                })
            })
            .unwrap_or_else(|| requested_email.clone());
        if otp_rate_limit_exceeded(&mut limits, rate_limit_key, std::time::Instant::now()) {
            tracing::warn!(email = %req.email, "OTP rate limit exceeded");
            // Return generic success to avoid leaking rate-limit state
            return Ok(Json(SendCodeResponse {
                ok: true,
                message: Some("Verification code sent to your email".into()),
            }));
        }
    }

    if scoped_profile_id.is_some() && scoped_login_target.is_none() {
        tracing::warn!(
            email = %requested_email,
            scoped_profile = ?scoped_profile_id,
            "OTP skipped — email does not match scoped profile"
        );
        delay_ineligible_otp_response().await;
        return Ok(Json(SendCodeResponse {
            ok: true,
            message: Some("Verification code sent to your email".into()),
        }));
    }
    if scoped_profile_id.is_none() && root_login_target.is_none() {
        tracing::warn!(email = %requested_email, "OTP skipped — email is not registered to a profile");
        // The body is identical to an eligible request. A short jittered delay
        // also reduces the otherwise-obvious microseconds-vs-SMTP timing gap.
        // It cannot perfectly reproduce arbitrary SMTP latency, so the edge
        // rate limit remains part of the public deployment's defence in depth.
        delay_ineligible_otp_response().await;
        return Ok(Json(SendCodeResponse {
            ok: true,
            message: Some("Verification code sent to your email".into()),
        }));
    }

    tracing::info!(email = %requested_email, "login OTP requested");
    let send_result = if let Some(target) = scoped_login_target.as_ref() {
        auth_mgr
            .send_otp_for_user(&requested_email, &target.id)
            .await
    } else {
        match root_login_target.as_ref() {
            Some(RootLoginTarget::Registered(user)) => {
                auth_mgr.send_otp_for_user(&requested_email, &user.id).await
            }
            Some(RootLoginTarget::Allowlisted) => {
                auth_mgr
                    .send_otp_with_registration(&requested_email, true)
                    .await
            }
            None => Ok(true),
        }
    };
    match send_result {
        Ok(true) => Ok(Json(SendCodeResponse {
            ok: true,
            message: Some("Verification code sent to your email".into()),
        })),
        Ok(false) => Ok(Json(SendCodeResponse {
            ok: true, // Don't reveal rate-limit state to prevent enumeration
            message: Some("Verification code sent to your email".into()),
        })),
        Err(e) => {
            // Log but don't leak internal errors
            tracing::warn!(error = %e, "send_otp failed");
            Ok(Json(SendCodeResponse {
                ok: true,
                message: Some("Verification code sent to your email".into()),
            }))
        }
    }
}

async fn delay_ineligible_otp_response() {
    let jitter_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_millis() as u64
        % 201;
    tokio::time::sleep(std::time::Duration::from_millis(350 + jitter_ms)).await;
}

/// GET /api/auth/status
pub async fn auth_status(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<AuthStatusResponse>, StatusCode> {
    let scoped_profile = trusted_auth_scope_profile_id(&state, &headers)
        .and_then(|profile_id| scoped_auth_target(&state, &profile_id));
    let global_email_login_enabled = state
        .user_store
        .as_ref()
        .and_then(|store| store.list().ok())
        .map(|users| {
            users.iter().any(|user| {
                is_login_ready_email(&user.email) && is_top_level_profile_id(&state, &user.id)
            })
        })
        .unwrap_or(false);
    let user_based_enabled = scoped_profile
        .as_ref()
        .map(|profile| profile.email_login_enabled)
        .unwrap_or(global_email_login_enabled);
    // Email login is only "enabled" if the server can actually deliver mail.
    // Without SMTP, send_otp silently logs the code to the server console and
    // returns success — leaving the dashboard happy to show the email form
    // but the user never receiving anything. Surfacing the SMTP state here
    // lets the dashboard hide the email form / display a clear notice.
    // Server-state, not user-state, so no enumeration risk.
    let smtp_ready = match state.auth_manager.as_ref() {
        Some(mgr) => mgr.smtp_configured().await,
        None => false,
    };
    let email_login_enabled = user_based_enabled && smtp_ready;

    Ok(Json(AuthStatusResponse {
        bootstrap_mode: is_bootstrap_mode(&state),
        email_login_enabled,
        admin_token_login_enabled: state.auth_token.is_some(),
        allow_self_registration: state
            .auth_manager
            .as_ref()
            .map(|m| m.allow_self_registration())
            .unwrap_or(false),
        // Advertise solo only when supported — which now requires the
        // explicit opt-in (a hosted Local-mode fleet daemon behind Caddy never
        // sets it), so the SPA never offers the no-password path there. See
        // `supports_local_solo_profile_create` / `crate::api::solo_auth`.
        local_solo_enabled: crate::api::ui_protocol_transport::supports_local_solo_profile_create(
            &state,
        ),
        solo_profile_exists:
            if crate::api::ui_protocol_transport::supports_local_solo_profile_create(&state) {
                Some(crate::api::solo_auth::resolve_solo_user(&state).is_some())
            } else {
                None
            },
        scoped_profile,
    }))
}

/// POST /api/auth/verify
pub async fn verify(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<VerifyRequest>,
) -> Result<Json<VerifyResponse>, StatusCode> {
    let auth_mgr = state
        .auth_manager
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let requested_email = req.email.trim().to_lowercase();
    let scoped_profile_id = trusted_auth_scope_profile_id(&state, &headers);
    let scoped_login_target = scoped_profile_id
        .as_deref()
        .and_then(|profile_id| resolve_scoped_login_user(&state, profile_id, &requested_email));
    let root_login_target = if scoped_profile_id.is_none() {
        resolve_root_login_target(&state, &requested_email)
    } else {
        None
    };

    if scoped_profile_id.is_some() {
        if scoped_login_target.is_none() {
            return Ok(Json(VerifyResponse {
                ok: false,
                token: None,
                user: None,
                message: Some("Invalid or expired code".into()),
            }));
        }
    } else if root_login_target.is_none() && !auth_mgr.allow_self_registration() {
        return Ok(Json(VerifyResponse {
            ok: false,
            token: None,
            user: None,
            message: Some("Invalid or expired code".into()),
        }));
    }

    let verify_result = if let Some(target) = scoped_login_target.as_ref() {
        auth_mgr
            .verify_otp_for_user(&requested_email, &req.code, &target.id)
            .await
    } else {
        match root_login_target.as_ref() {
            Some(RootLoginTarget::Registered(user)) => {
                auth_mgr
                    .verify_otp_for_user(&requested_email, &req.code, &user.id)
                    .await
            }
            Some(RootLoginTarget::Allowlisted) => {
                // Allowlist provenance authorizes claiming a
                // pre-provisioned profile under the derived id — the
                // profile probe must not bump the invitee to `<id>-1`
                // (codex #1613 r7 P1).
                auth_mgr
                    .verify_otp_with_authorized_claim(&requested_email, &req.code)
                    .await
            }
            None if auth_mgr.allow_self_registration() => {
                auth_mgr
                    .verify_otp_with_registration(&requested_email, &req.code, true)
                    .await
            }
            None => Ok(None),
        }
    };

    match verify_result {
        Ok(Some(token)) => {
            tracing::info!(email = %requested_email, "user logged in");
            let user_store = state
                .user_store
                .as_ref()
                .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
            let user = match (scoped_login_target.as_ref(), root_login_target.as_ref()) {
                (Some(target), _) => user_store.get(&target.id).ok().flatten(),
                (None, Some(RootLoginTarget::Registered(user))) => Some(user.clone()),
                (None, Some(RootLoginTarget::Allowlisted)) => {
                    user_store.get_by_email(&requested_email).ok().flatten()
                }
                (None, None) => user_store.get_by_email(&requested_email).ok().flatten(),
            };

            if matches!(root_login_target, Some(RootLoginTarget::Allowlisted)) {
                if let (Some(allowlist_store), Some(user)) =
                    (state.allowlist_store.as_ref(), user.as_ref())
                {
                    if let Err(error) = allowlist_store.claim(&requested_email, &user.id) {
                        tracing::warn!(email = %requested_email, user_id = %user.id, error = %error, "failed to claim allowlist entry");
                    }
                }
            }

            // Auto-create profile if user has none
            if let Some(ref user) = user {
                if let Some(ref profile_store) = state.profile_store {
                    if profile_store.get(&user.id).unwrap_or(None).is_none() {
                        let profile = crate::profiles::UserProfile {
                            id: user.id.clone(),
                            name: user.name.clone(),
                            public_subdomain: None,
                            enabled: false,
                            data_dir: None,
                            parent_id: None,
                            config: crate::profiles::ProfileConfig::default(),
                            created_at: chrono::Utc::now(),
                            updated_at: chrono::Utc::now(),
                        };
                        if let Err(e) = profile_store.save(&profile) {
                            tracing::warn!(user_id = %user.id, error = %e, "failed to auto-create profile");
                        }
                    }
                }
            }

            Ok(Json(VerifyResponse {
                ok: true,
                token: Some(token),
                user,
                message: None,
            }))
        }
        Ok(None) => Ok(Json(VerifyResponse {
            ok: false,
            token: None,
            user: None,
            message: Some("Invalid or expired code".into()),
        })),
        Err(e) => {
            tracing::warn!(error = %e, "verify_otp error");
            Ok(Json(VerifyResponse {
                ok: false,
                token: None,
                user: None,
                message: Some("Invalid or expired code".into()),
            }))
        }
    }
}

/// POST /api/auth/logout
pub async fn logout(
    State(state): State<Arc<AppState>>,
    req: axum::http::Request<axum::body::Body>,
) -> Result<Json<ActionResponse>, StatusCode> {
    let auth_mgr = state
        .auth_manager
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    if let Some(token) = extract_bearer_token(&req) {
        auth_mgr.revoke_session(&token).await;
        tracing::info!("user logged out");
    }

    Ok(Json(ActionResponse {
        ok: true,
        message: None,
    }))
}

/// GET /api/auth/me
pub async fn me(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<AuthIdentity>,
) -> Result<Json<MeResponse>, StatusCode> {
    // Determine the active tenant scope (if any). The dashboard reads
    // this to gate admin-global UI when an admin is operating on a
    // tenant subdomain (Option Y, #315).
    let scoped_profile = host_scoped_profile_id(&state, &headers)
        .and_then(|profile_id| scoped_auth_target(&state, &profile_id));

    // Handle admin token first — bootstrap admin still needs a real persisted principal.
    if matches!(&identity, AuthIdentity::Admin) {
        let user = if let Some(ref user_store) = state.user_store {
            ensure_admin_user(user_store)?
        } else {
            User {
                id: ADMIN_PROFILE_ID.into(),
                email: ADMIN_PLACEHOLDER_EMAIL.into(),
                name: "Admin".into(),
                role: UserRole::Admin,
                created_at: chrono::Utc::now(),
                last_login_at: None,
            }
        };
        let profile = if let Some(ref ps) = state.profile_store {
            ensure_admin_profile(ps).ok();
            if let Ok(Some(p)) = ps.get(ADMIN_PROFILE_ID) {
                let status = if let Some(ref pm) = state.process_manager {
                    pm.status(&p.id).await
                } else {
                    crate::process_manager::ProcessStatus::stopped()
                };
                Some(ProfileResponse {
                    email: None,
                    profile: mask_secrets(&p),
                    status,
                })
            } else {
                None
            }
        } else {
            None
        };
        let portal = build_portal_state(&state, &identity, &user)?;

        return Ok(Json(MeResponse {
            user,
            profile,
            portal,
            scoped_profile,
        }));
    }

    let user_id = match &identity {
        AuthIdentity::Admin => unreachable!(),
        AuthIdentity::User { id, .. } => id.clone(),
    };

    // E2E test user: return a synthetic user without database lookup
    if user_id == "e2e-test" {
        return Ok(Json(MeResponse {
            user: User {
                id: "e2e-test".into(),
                email: "e2e@test.local".into(),
                name: "E2E Test".into(),
                role: UserRole::User,
                created_at: chrono::Utc::now(),
                last_login_at: None,
            },
            profile: None,
            portal: PortalState {
                kind: PortalKind::Owner,
                home_profile_id: "e2e-test".into(),
                home_route: "/my".into(),
                can_access_admin_portal: false,
                can_manage_users: false,
                sub_account_limit: crate::profiles::MAX_SUB_ACCOUNTS_PER_PARENT,
                accessible_profiles: vec![AccessibleProfileSummary {
                    id: "e2e-test".into(),
                    name: "E2E Test".into(),
                    parent_id: None,
                    relationship: ProfileRelationship::SelfProfile,
                    api_scope: ProfileApiScope::SelfService,
                    route_base: "/my".into(),
                    can_manage_sub_accounts: true,
                }],
            },
            scoped_profile,
        }));
    }

    let user_store = state
        .user_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    let user = user_store
        .get(&user_id)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    let profile = if let Some(ref ps) = state.profile_store {
        if let Ok(Some(p)) = ps.get(&user.id) {
            let status = if let Some(ref pm) = state.process_manager {
                pm.status(&p.id).await
            } else {
                crate::process_manager::ProcessStatus::stopped()
            };
            Some(ProfileResponse {
                email: None,
                profile: mask_secrets(&p),
                status,
            })
        } else {
            None
        }
    } else {
        None
    };
    let portal = build_portal_state(&state, &identity, &user)?;

    Ok(Json(MeResponse {
        user,
        profile,
        portal,
        scoped_profile,
    }))
}

// ── User self-service endpoints (/api/my/*) ───────────────────────────

/// GET /api/my/profile
pub async fn my_profile(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<AuthIdentity>,
) -> Result<Json<ProfileResponse>, StatusCode> {
    let ps = state
        .profile_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    let profile = resolve_my_profile(&identity, ps, &state, &headers)?;

    let status = if let Some(ref pm) = state.process_manager {
        pm.status(&profile.id).await
    } else {
        crate::process_manager::ProcessStatus::stopped()
    };

    // `with_email_lookup` is the only thing that reads the address out of the
    // user store. Building the literal with `email: None` instead left the
    // settings page's "Email (for OTP login)" field blank for everyone — on GET,
    // and again right after a save on PUT.
    Ok(Json(
        ProfileResponse::from(mask_secrets(&profile), status)
            .with_email_lookup(state.user_store.as_deref()),
    ))
}

#[derive(Deserialize, Default)]
pub struct MyProfileQrQuery {
    #[serde(default)]
    pub include_secrets: bool,
    /// Endpoint URL to embed; defaults to the request host.
    #[serde(default)]
    pub endpoint: Option<String>,
}

/// Whether an env-var value may be resolved into a QR export for
/// `profile_id`: plain values always; `keychain:` markers ONLY when they
/// point at the profile's own scoped account (`VAR::profile_id`). Bare
/// (`keychain:VAR`) and foreign-scoped markers are refused — both can
/// address keychain items the profile does not own.
pub(crate) fn marker_allowed_for_export(raw: &str, var: &str, profile_id: &str) -> bool {
    if !crate::auth::keychain::is_marker(raw) {
        return true;
    }
    raw == crate::auth::keychain::marker_for(&crate::auth::keychain::scoped_account(
        var, profile_id,
    ))
}

/// Build the QR wire payload from a stored [`crate::profiles::UserProfile`].
///
/// Secrets are the profile's OWN `env_vars` entries referenced by its
/// LLM routes (`api_key_env` on primary + fallbacks) — never the host
/// process env or auth store, so a tenant export can only ever carry
/// tenant-scoped credentials. Sub-account inheritance is NOT resolved
/// here: the payload mirrors the profile as stored.
pub(crate) fn payload_from_user_profile(
    profile: &crate::profiles::UserProfile,
    include_secrets: bool,
) -> eyre::Result<crate::profile_qr::ProfileQrPayload> {
    use eyre::WrapErr;

    let mut payload = crate::profile_qr::ProfileQrPayload::new(&profile.id);
    payload.name = Some(profile.name.clone());
    if let Some(ref llm) = profile.config.llm {
        payload.llm = Some(serde_json::to_value(llm).wrap_err("serialize llm config")?);
    }
    if let Some(ref memory) = profile.config.memory {
        payload.memory = Some(serde_json::to_value(memory).wrap_err("serialize memory config")?);
    }
    payload.voice_default = profile.config.voice_default.clone();

    if include_secrets {
        if let Some(ref llm) = profile.config.llm {
            let routes = llm
                .primary
                .iter()
                .chain(llm.fallbacks.iter())
                .filter_map(|selection| selection.route.as_ref());
            for route in routes {
                let Some(ref var) = route.api_key_env else {
                    continue;
                };
                let Some(raw) = profile.config.env_vars.get(var) else {
                    continue;
                };
                if !marker_allowed_for_export(raw, var, &profile.id) {
                    // A profile may persist an ARBITRARY keychain marker
                    // suffix ("keychain:VERTEX_SA_JSON::admin"); resolving
                    // it here would exfiltrate another tenant's (or the
                    // host's) keychain item through the QR (codex P1).
                    continue;
                }
                let Some(value) = crate::auth::keychain::resolve_value(var, raw)
                    .filter(|value| !value.is_empty())
                else {
                    continue;
                };
                payload.secrets.entry(var.clone()).or_insert(value);
            }
        }
    }

    Ok(payload)
}

/// GET /api/my/profile/qr
///
/// Export the caller's profile as an `OCTOS1:`/`OCTOS1E:` payload for
/// client-side QR rendering. With `include_secrets=true` the payload is
/// ALWAYS PIN-wrapped (`OCTOS1E`) — there is no plain-secrets override
/// over the API — and the one-time PIN is returned beside the payload,
/// so a screenshotted or logged QR image alone reveals nothing.
pub async fn my_profile_qr(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<AuthIdentity>,
    Query(query): Query<MyProfileQrQuery>,
) -> Result<
    (
        axum::response::AppendHeaders<[(axum::http::HeaderName, &'static str); 2]>,
        Json<serde_json::Value>,
    ),
    (StatusCode, String),
> {
    let ps = state.profile_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "profile store not configured".into(),
    ))?;
    let profile = resolve_my_profile(&identity, ps, &state, &headers)
        .map_err(|s| (s, "profile not found".into()))?;

    let mut payload = payload_from_user_profile(&profile, query.include_secrets)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    payload.endpoint = query.endpoint.or_else(|| request_endpoint(&headers));

    let (encoded, pin) = if payload.has_secrets() {
        // The Argon2id profile is deliberately heavy (64 MiB, t=3) — run
        // it OFF the async workers, and bound concurrent secret exports
        // so parallel requests can't pin N×64 MiB + all Tokio workers
        // (codex round-2 P2).
        let _permit = SECRET_EXPORT_LIMIT.try_acquire().map_err(|_| {
            (
                StatusCode::TOO_MANY_REQUESTS,
                "too many concurrent secret exports; retry shortly".to_string(),
            )
        })?;
        let pin = crate::profile_qr::generate_pin();
        let sealed_payload = payload.clone();
        let sealed_pin = pin.clone();
        let encoded = tokio::task::spawn_blocking(move || {
            crate::profile_qr::encode_encrypted(&sealed_payload, &sealed_pin)
        })
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        (encoded, Some(pin))
    } else {
        let encoded = crate::profile_qr::encode_plain(&payload, false)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        (encoded, None)
    };

    // The response body carries everything needed to decrypt the exported
    // keys (payload + transfer secret) — it must never land in a shared
    // cache or be persisted by an intermediary (codex round-2 P2).
    Ok((
        axum::response::AppendHeaders([
            (axum::http::header::CACHE_CONTROL, "no-store"),
            (axum::http::header::PRAGMA, "no-cache"),
        ]),
        Json(serde_json::json!({
            "payload": encoded,
            "pin": pin,
            "profile_id": profile.id,
        })),
    ))
}

/// Bounded concurrency for PIN-wrapped profile exports: each runs a
/// 64 MiB Argon2id derivation on the blocking pool.
static SECRET_EXPORT_LIMIT: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

/// GET /api/my/profile/skills
pub async fn my_profile_skills(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<AuthIdentity>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let store = state.profile_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;
    let profile_id = resolve_my_profile_id(&identity, store, &state, &headers)
        .map_err(|s| (s, "profile not found".into()))?;
    let skills_dir = crate::commands::skills::resolve_profile_skills_dir(store, &profile_id)
        .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;
    let skills = crate::commands::skills::list_skills(&skills_dir)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(serde_json::json!({ "skills": skills })))
}

#[derive(Deserialize, Default)]
pub struct MySkillRegistryQuery {
    #[serde(default)]
    pub q: Option<String>,
}

/// GET /api/my/profile/skills/registry
pub async fn my_profile_skill_registry(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<AuthIdentity>,
    Query(query): Query<MySkillRegistryQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let store = state.profile_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;
    let profile_id = resolve_my_profile_id(&identity, store, &state, &headers)
        .map_err(|s| (s, "profile not found".into()))?;
    // Validate this profile has a resolvable skills scope.
    crate::commands::skills::resolve_profile_skills_dir(store, &profile_id)
        .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;

    let q = query.q;
    let packages = tokio::task::spawn_blocking(move || {
        crate::commands::skills::search_registry(q.as_deref(), None)
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;

    Ok(Json(serde_json::json!({ "packages": packages })))
}

/// POST /api/my/profile/skills
pub async fn install_my_profile_skill(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<AuthIdentity>,
    Json(req): Json<super::admin::InstallSkillRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let store = state.profile_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;
    let profile_id = resolve_my_profile_id(&identity, store, &state, &headers)
        .map_err(|s| (s, "profile not found".into()))?;
    let skills_dir = crate::commands::skills::resolve_profile_skills_dir(store, &profile_id)
        .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;

    let result = tokio::task::spawn_blocking(move || {
        crate::commands::skills::install_skill(&skills_dir, &req.repo, req.force, &req.branch)
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;

    Ok(Json(serde_json::json!({
        "ok": true,
        "installed": result.installed,
        "skipped": result.skipped,
        "deps_installed": result.deps_installed,
    })))
}

/// DELETE /api/my/profile/skills/:name
pub async fn remove_my_profile_skill(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<AuthIdentity>,
    Path(name): Path<String>,
) -> Result<Json<super::admin::ActionResponse>, (StatusCode, String)> {
    let store = state.profile_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;
    let profile_id = resolve_my_profile_id(&identity, store, &state, &headers)
        .map_err(|s| (s, "profile not found".into()))?;
    let skills_dir = crate::commands::skills::resolve_profile_skills_dir(store, &profile_id)
        .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;

    // remove_skill builds its own current-thread tokio runtime via block_on
    // to drive the optional shutdown lifecycle phase — that panics if invoked
    // from inside the axum runtime. Defer to spawn_blocking so the new
    // runtime constructs on a separate OS thread (mirrors install_skill).
    let name_for_remove = name.clone();
    tokio::task::spawn_blocking(move || {
        crate::commands::skills::remove_skill(&skills_dir, &name_for_remove)
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;

    Ok(Json(super::admin::ActionResponse {
        ok: true,
        message: Some(format!("Removed skill: {name}")),
    }))
}

/// Move a freshly-entered keychain-backed secret out of `env_vars` and into the
/// OS keychain, replacing the stored value with the keychain marker so the
/// secret never lands in the profile config on disk.
///
/// Only a raw, freshly-entered value (JSON object, i.e. starts with `{`) is
/// relocated. A keychain marker, a masked display value, or an empty string all
/// mean "unchanged" and are left untouched. Keychain-backed config is
/// macOS-only: a raw secret on another OS is rejected rather than silently
/// persisted in plaintext.
///
/// The secret is stored under a **profile-scoped** keychain account
/// (`<key>::<profile_id>`) and the stored marker carries that account, so two
/// profiles saving different secrets under the same env var never overwrite a
/// single shared keychain item (each profile reads back its own credential).
///
/// `store_available` (a secret-store backend exists on this host) and
/// `set_secret` are injected for testability.
pub(crate) fn relocate_secret_to_keychain(
    env_vars: &mut HashMap<String, String>,
    key: &str,
    profile_id: &str,
    store_available: bool,
    set_secret: impl Fn(&str, &str) -> eyre::Result<()>,
) -> Result<(), String> {
    let Some(value) = env_vars.get(key) else {
        return Ok(());
    };
    let value = value.trim().to_string();
    // Markers / masked / empty values mean "leave as configured".
    if !value.starts_with('{') {
        return Ok(());
    }
    if !store_available {
        return Err(format!(
            "{key}: keychain-backed credential storage is unavailable on this host (no secret store backend)"
        ));
    }
    let account = crate::auth::keychain::scoped_account(key, profile_id);
    set_secret(&account, &value).map_err(|e| format!("failed to store {key} in keychain: {e}"))?;
    env_vars.insert(key.to_string(), crate::auth::keychain::marker_for(&account));
    Ok(())
}

pub async fn update_my_profile(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<AuthIdentity>,
    body: String,
) -> Result<Json<ProfileResponse>, (StatusCode, String)> {
    let runtime_config_changed = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|value| value.get("config").cloned())
        .is_some();
    let req: super::admin::UpdateProfileRequest = serde_json::from_str(&body).map_err(|e| {
        tracing::warn!(error = %e, body = %body, "failed to parse my profile update request");
        (
            StatusCode::BAD_REQUEST,
            format!("Invalid request body: {e}"),
        )
    })?;
    let ps = state.profile_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;

    let mut profile = resolve_my_profile(&identity, ps, &state, &headers)
        .map_err(|s| (s, "profile not found".into()))?;

    // Apply updates (same logic as admin::update_profile but scoped)
    if let Some(name) = req.name {
        profile.name = name;
    }
    if let Some(public_subdomain) = req.public_subdomain {
        if profile.parent_id.is_some() {
            return Err((
                StatusCode::FORBIDDEN,
                "sub-accounts cannot change their own public subdomain".into(),
            ));
        }
        profile.public_subdomain = public_subdomain
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned);
    }
    if let Some(enabled) = req.enabled {
        profile.enabled = enabled;
    }
    super::admin::merge_profile_config_from_body(&mut profile.config, &body, true)
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    // Relocate freshly-entered keychain-backed secrets (e.g. the Vertex SA JSON,
    // including a private key pasted under a custom env name) into the OS
    // keychain before persisting. Uses the shared content-detecting helper so
    // this path can't diverge from the others.
    let profile_id = profile.id.clone();
    super::admin::relocate_keychain_backed_secrets(&mut profile.config.env_vars, &profile_id)?;
    profile.updated_at = chrono::Utc::now();

    ps.save_with_merge(&mut profile).map_err(|e| {
        tracing::error!(profile = %profile.id, error = %e, "failed to save user profile");
        (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })?;

    if runtime_config_changed {
        crate::api::ui_protocol_transport::refresh_profile_runtime_after_profile_update(
            &state,
            &profile.id,
            Some(profile.updated_at.to_rfc3339()),
        )
        .await;
    }

    tracing::info!(profile = %profile.id, "user profile updated");
    let status = if let Some(ref pm) = state.process_manager {
        pm.status(&profile.id).await
    } else {
        crate::process_manager::ProcessStatus::stopped()
    };

    // `with_email_lookup` is the only thing that reads the address out of the
    // user store. Building the literal with `email: None` instead left the
    // settings page's "Email (for OTP login)" field blank for everyone — on GET,
    // and again right after a save on PUT.
    Ok(Json(
        ProfileResponse::from(mask_secrets(&profile), status)
            .with_email_lookup(state.user_store.as_deref()),
    ))
}

// ── Voice selection endpoints ────────────────────────────────────────

#[derive(Serialize)]
pub struct VoicesResponse {
    /// Voices the engine can actually synthesize (ref audio present).
    pub voices: Vec<octos_llm::ominix::VoiceInfo>,
    /// This user's currently effective reply voice (live override > persisted
    /// per-profile default > serve default).
    pub current: String,
}

/// GET /api/voices — list synthesizable voices + this user's current choice.
///
/// Reads the platform registry (`~/.OminiX/models/voices.json`). A missing or
/// unreadable registry degrades to a single-entry list of the current voice
/// rather than failing the request.
pub async fn list_voices(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<AuthIdentity>,
) -> Result<Json<VoicesResponse>, StatusCode> {
    let ps = state
        .profile_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let profile = resolve_my_profile(&identity, ps, &state, &headers)?;
    let profile_id = profile.id.clone();

    let registry_path = crate::api::voices::registry_path();
    let (mut voices, registry_default) =
        match octos_llm::ominix::VoicesRegistry::load(&registry_path) {
            // Scope the listing to this tenant: shared presets + voices this
            // profile owns. A clone cloned by another tenant must not appear.
            Ok(reg) => (
                reg.synthesizable_visible(|ref_audio| {
                    crate::api::voices::voice_visible_to(&profile_id, ref_audio)
                }),
                reg.default_voice,
            ),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    path = %registry_path.display(),
                    "voices.json unavailable; returning current default only"
                );
                (Vec::new(), String::new())
            }
        };

    // Fallback chain for the "current" voice when no live override is set: the
    // bootstrapped per-profile default (already overlays the serve default),
    // then the registry default, then the first listed voice.
    let runtime_default = state
        .profiles
        .get(&profile_id)
        .map(|r| r.voice.default_voice.clone())
        .filter(|s| !s.is_empty());
    let fallback = runtime_default
        .or_else(|| {
            profile
                .config
                .voice_default
                .clone()
                .filter(|s| !s.is_empty())
        })
        .or_else(|| Some(registry_default).filter(|s| !s.is_empty()))
        .or_else(|| voices.first().map(|v| v.id.clone()))
        .unwrap_or_default();
    let current = crate::api::voices::resolve_reply_voice(&profile_id, &fallback);

    // Degrade gracefully: an unreadable registry still shows the current voice.
    if voices.is_empty() && !current.is_empty() {
        voices.push(octos_llm::ominix::VoiceInfo {
            id: current.clone(),
            aliases: Vec::new(),
        });
    }

    Ok(Json(VoicesResponse { voices, current }))
}

#[derive(Deserialize)]
pub struct SetVoiceRequest {
    pub voice: String,
}

#[derive(Serialize)]
pub struct SetVoiceResponse {
    pub ok: bool,
    pub voice: String,
}

/// PUT /api/my/voice — set this user's sticky reply-voice default.
///
/// Validates the voice against the registry, persists the canonical id to the
/// profile (sticky across restarts), and records a live override so the switch
/// takes effect on the next turn without a runtime reload.
pub async fn set_my_voice(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<AuthIdentity>,
    Json(req): Json<SetVoiceRequest>,
) -> Result<Json<SetVoiceResponse>, (StatusCode, String)> {
    let ps = state.profile_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;
    let mut profile = resolve_my_profile(&identity, ps, &state, &headers)
        .map_err(|s| (s, "profile not found".into()))?;

    // Validate + canonicalise (id or alias → canonical id) against the registry.
    let registry_path = crate::api::voices::registry_path();
    let registry = octos_llm::ominix::VoicesRegistry::load(&registry_path).map_err(|e| {
        tracing::warn!(error = %e, path = %registry_path.display(), "voice registry unavailable");
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "voice registry unavailable".into(),
        )
    })?;
    // Resolve only within this tenant's visible set (shared presets + voices it
    // owns), so a tenant can't select a voice cloned by another profile.
    let canonical = registry
        .resolve_visible(req.voice.trim(), |ref_audio| {
            crate::api::voices::voice_visible_to(&profile.id, ref_audio)
        })
        .ok_or((
            StatusCode::BAD_REQUEST,
            format!("unknown voice: {}", req.voice),
        ))?;

    // Persist (sticky per-user) then set the live override (instant effect).
    profile.config.voice_default = Some(canonical.clone());
    profile.updated_at = chrono::Utc::now();
    ps.save_with_merge(&mut profile).map_err(|e| {
        tracing::error!(profile = %profile.id, error = %e, "failed to persist voice choice");
        (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })?;
    crate::api::voices::set_override(&profile.id, &canonical);

    tracing::info!(profile = %profile.id, voice = %canonical, "reply voice updated");
    Ok(Json(SetVoiceResponse {
        ok: true,
        voice: canonical,
    }))
}

// ── Voice pipeline readiness ──────────────────────────────────────────

/// Readiness of a single voice-pipeline leg.
#[derive(Serialize)]
pub struct VoiceLeg {
    pub ready: bool,
    /// Human-readable status for the UI to surface the failing leg precisely.
    pub detail: String,
}

/// Readiness of the ASR leg, including which route the host resolves to.
#[derive(Serialize)]
pub struct VoiceAsrLeg {
    pub ready: bool,
    /// Effective route: `"private"`, `"external"` (`ASR_API_URL`), or `"ominix"`.
    pub mode: String,
    pub detail: String,
}

/// Readiness of the TTS leg, including which route the profile resolves to.
#[derive(Serialize)]
pub struct VoiceTtsLeg {
    pub ready: bool,
    /// Effective route for this profile: `"cloud"` or `"local"`.
    pub mode: String,
    pub detail: String,
}

/// Aggregated voice-assistant pre-flight readiness for one tenant.
#[derive(Serialize)]
pub struct VoiceReadiness {
    /// All legs ready → a voice turn can complete end to end.
    pub ready: bool,
    pub asr: VoiceAsrLeg,
    pub llm: VoiceLeg,
    pub tts: VoiceTtsLeg,
}

async fn external_asr_readiness(client: &reqwest::Client, base_url: &str) -> VoiceAsrLeg {
    let response = client
        .get(format!("{}/health", base_url.trim_end_matches('/')))
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await;

    match response {
        Ok(response) if response.status().is_success() => VoiceAsrLeg {
            ready: true,
            mode: "external".into(),
            detail: "External ASR ready".into(),
        },
        Ok(response)
            if matches!(
                response.status(),
                StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
            ) =>
        {
            VoiceAsrLeg {
                ready: true,
                mode: "external".into(),
                detail: "External ASR configured (health endpoint not provided)".into(),
            }
        }
        Ok(response) => VoiceAsrLeg {
            ready: false,
            mode: "external".into(),
            detail: format!(
                "External ASR health check returned HTTP {}",
                response.status()
            ),
        },
        Err(error) => VoiceAsrLeg {
            ready: false,
            mode: "external".into(),
            detail: if error.is_timeout() {
                "External ASR health check timed out".into()
            } else {
                "External ASR is unreachable".into()
            },
        },
    }
}

fn voice_readiness_needs_ominix(
    private_asr_configured: bool,
    asr_route: &crate::skills_scope::AsrRoute,
    tts_route: crate::api::voice_turn::TtsRoute,
) -> bool {
    (!private_asr_configured && matches!(asr_route, crate::skills_scope::AsrRoute::Ominix(_)))
        || tts_route == crate::api::voice_turn::TtsRoute::Local
}

const MAX_SPEECH_SYNTHESIS_CHARS: usize = 4_000;
const MAX_CONCURRENT_SPEECH_SYNTHESIS_PER_PROFILE: usize = 1;
const MAX_SPEECH_SYNTHESIS_REQUESTS_PER_WINDOW: u32 = 20;
const MAX_SPEECH_SYNTHESIS_CHARS_PER_WINDOW: usize = 20_000;
const SPEECH_SYNTHESIS_QUOTA_WINDOW: std::time::Duration = std::time::Duration::from_secs(60);

static SPEECH_SYNTHESIS_LIMITS: LazyLock<Mutex<HashMap<String, Weak<tokio::sync::Semaphore>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static SPEECH_SYNTHESIS_QUOTAS: LazyLock<Mutex<HashMap<String, SpeechSynthesisQuota>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Debug)]
pub(crate) struct SpeechSynthesisError {
    status: StatusCode,
    message: String,
    retry_after_seconds: Option<u64>,
}

impl SpeechSynthesisError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            retry_after_seconds: None,
        }
    }

    fn rate_limited(message: impl Into<String>, retry_after_seconds: u64) -> Self {
        Self {
            status: StatusCode::TOO_MANY_REQUESTS,
            message: message.into(),
            retry_after_seconds: Some(retry_after_seconds.max(1)),
        }
    }
}

impl IntoResponse for SpeechSynthesisError {
    fn into_response(self) -> Response {
        let mut response = (self.status, self.message).into_response();
        if let Some(seconds) = self.retry_after_seconds
            && let Ok(value) = HeaderValue::from_str(&seconds.to_string())
        {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
        response
    }
}

#[derive(Debug)]
struct SpeechSynthesisQuota {
    started_at: std::time::Instant,
    requests: u32,
    characters: usize,
}

impl SpeechSynthesisQuota {
    fn new(started_at: std::time::Instant) -> Self {
        Self {
            started_at,
            requests: 0,
            characters: 0,
        }
    }

    fn consume(
        &mut self,
        characters: usize,
        now: std::time::Instant,
    ) -> Result<(), SpeechSynthesisError> {
        let elapsed = now.saturating_duration_since(self.started_at);
        if elapsed >= SPEECH_SYNTHESIS_QUOTA_WINDOW {
            *self = Self::new(now);
        }
        if self.requests >= MAX_SPEECH_SYNTHESIS_REQUESTS_PER_WINDOW
            || self.characters.saturating_add(characters) > MAX_SPEECH_SYNTHESIS_CHARS_PER_WINDOW
        {
            let remaining = SPEECH_SYNTHESIS_QUOTA_WINDOW
                .saturating_sub(now.saturating_duration_since(self.started_at));
            let retry_after_seconds = remaining.as_secs() + u64::from(remaining.subsec_nanos() > 0);
            return Err(SpeechSynthesisError::rate_limited(
                format!(
                    "speech synthesis quota exceeded; limit is {MAX_SPEECH_SYNTHESIS_REQUESTS_PER_WINDOW} requests or {MAX_SPEECH_SYNTHESIS_CHARS_PER_WINDOW} characters per minute"
                ),
                retry_after_seconds,
            ));
        }
        self.requests += 1;
        self.characters += characters;
        Ok(())
    }
}

#[derive(Deserialize)]
pub(crate) struct SpeechSynthesisRequest {
    pub text: String,
}

fn validate_synthesis_text(text: &str) -> Result<&str, SpeechSynthesisError> {
    let text = text.trim();
    if text.is_empty() {
        return Err(SpeechSynthesisError::new(
            StatusCode::BAD_REQUEST,
            "text must not be empty",
        ));
    }
    if text.chars().count() > MAX_SPEECH_SYNTHESIS_CHARS {
        return Err(SpeechSynthesisError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("text exceeds {MAX_SPEECH_SYNTHESIS_CHARS} characters"),
        ));
    }
    Ok(text)
}

fn acquire_speech_synthesis_permit(
    profile_id: &str,
) -> Result<tokio::sync::OwnedSemaphorePermit, SpeechSynthesisError> {
    let semaphore = {
        let mut limits = SPEECH_SYNTHESIS_LIMITS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        limits.retain(|_, limit| limit.strong_count() > 0);
        if let Some(limit) = limits.get(profile_id).and_then(Weak::upgrade) {
            limit
        } else {
            let limit = Arc::new(tokio::sync::Semaphore::new(
                MAX_CONCURRENT_SPEECH_SYNTHESIS_PER_PROFILE,
            ));
            limits.insert(profile_id.to_owned(), Arc::downgrade(&limit));
            limit
        }
    };

    semaphore.try_acquire_owned().map_err(|_| {
        SpeechSynthesisError::rate_limited(
            "speech synthesis already in progress for this profile",
            1,
        )
    })
}

fn consume_speech_synthesis_quota(
    profile_id: &str,
    characters: usize,
) -> Result<(), SpeechSynthesisError> {
    let now = std::time::Instant::now();
    let mut quotas = SPEECH_SYNTHESIS_QUOTAS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    quotas.retain(|_, quota| {
        now.saturating_duration_since(quota.started_at) < SPEECH_SYNTHESIS_QUOTA_WINDOW
    });
    quotas
        .entry(profile_id.to_owned())
        .or_insert_with(|| SpeechSynthesisQuota::new(now))
        .consume(characters, now)
}

fn speech_audio_content_type(path: &std::path::Path) -> &'static str {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("mp3") => "audio/mpeg",
        Some("ogg") | Some("opus") => "audio/ogg",
        Some("pcm") => "audio/pcm",
        _ => "audio/wav",
    }
}

/// POST /api/voice/synthesize — synthesize text with the caller's voice profile.
///
/// The caller's effective profile runtime supplies the provider, credentials,
/// and selected voice. The endpoint is intentionally independent of any
/// product-specific playback or content model.
pub(crate) async fn synthesize_speech(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<AuthIdentity>,
    Json(request): Json<SpeechSynthesisRequest>,
) -> Result<Response, SpeechSynthesisError> {
    let text = validate_synthesis_text(&request.text)?;
    let profile_store = state.profile_store.as_ref().ok_or_else(|| {
        SpeechSynthesisError::new(StatusCode::SERVICE_UNAVAILABLE, "profile store unavailable")
    })?;
    let profile_id = resolve_my_profile_id(&identity, profile_store, &state, &headers)
        .map_err(|status| SpeechSynthesisError::new(status, "profile unavailable"))?;
    let _synthesis_permit = acquire_speech_synthesis_permit(&profile_id)?;
    consume_speech_synthesis_quota(&profile_id, text.chars().count())?;
    let runtime = crate::api::ui_protocol_transport::ensure_session_profile_runtime(
        &state,
        Some(&profile_id),
    )
    .await
    .map_err(|error| SpeechSynthesisError::new(StatusCode::SERVICE_UNAVAILABLE, error.message))?
    .ok_or_else(|| {
        SpeechSynthesisError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "profile runtime unavailable",
        )
    })?;
    let voice = crate::api::voices::resolve_reply_voice(&profile_id, &runtime.voice.default_voice);
    let output_dir = tempfile::tempdir().map_err(|error| {
        tracing::error!(%error, "failed to create speech synthesis temp directory");
        SpeechSynthesisError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to prepare speech synthesis",
        )
    })?;
    let audio_path = crate::api::voice_turn::synthesize_reply(
        text,
        &voice,
        &runtime.voice.tts_provider,
        runtime.voice.cloud.as_ref(),
        output_dir.path(),
    )
    .await
    .ok_or_else(|| {
        SpeechSynthesisError::new(
            StatusCode::BAD_GATEWAY,
            "configured TTS provider failed to synthesize speech",
        )
    })?;
    let content_type = speech_audio_content_type(&audio_path);
    let audio = tokio::fs::read(&audio_path).await.map_err(|error| {
        tracing::error!(%error, path = %audio_path.display(), "failed to read synthesized speech");
        SpeechSynthesisError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to read synthesized speech",
        )
    })?;
    let mut response = Response::new(Body::from(audio));
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

/// GET /api/voice/readiness — per-tenant pre-flight for the voice assistant.
///
/// Confirms the whole pipeline can run under THIS profile's current config:
/// - **ASR**: the private browser ASR service when configured; otherwise
///   `ASR_API_URL` health when explicitly configured; otherwise OMiniX.
/// - **LLM**: the profile's provider chain is constructed (running runtime with
///   a named provider).
/// - **TTS**: the *chosen* route is actually usable — cloud credentials resolve
///   for `cloud`/`volcano` (and `auto` when configured); otherwise the on-device
///   GPT-SoVITS MODEL is ready AND the profile's *effective* reply voice (live
///   override > per-profile default) is synthesizable. Mirrors
///   `voice_turn::synthesize_reply`'s routing + voice resolution so the check
///   matches what a real turn would do.
pub async fn voice_readiness(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<AuthIdentity>,
) -> Result<Json<VoiceReadiness>, StatusCode> {
    let ps = state
        .profile_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let profile = resolve_my_profile(&identity, ps, &state, &headers)?;
    let profile_id = profile.id.clone();

    // ── LLM leg ──
    // The provider chain is built at bootstrap; a running runtime with a named
    // provider means the LLM is wired for this tenant. Resolve via the same
    // path session/turn handling uses — including dynamically bootstrapped
    // runtimes (onboarding, `profile/llm/upsert`) that live outside
    // `state.profiles` — so readiness can't report "not started" while voice
    // turns actually work.
    let rt = crate::api::ui_protocol_transport::ensure_session_profile_runtime(
        &state,
        Some(&profile_id),
    )
    .await
    .ok()
    .flatten();
    let llm = VoiceLeg {
        ready: rt
            .as_ref()
            .map(|r| !r.provider_name.is_empty())
            .unwrap_or(false),
        detail: match rt.as_ref() {
            Some(r) if !r.provider_name.is_empty() => format!("LLM provider: {}", r.provider_name),
            Some(_) => "LLM provider not configured".into(),
            None => "Profile runtime not started".into(),
        },
    };

    // ── TTS leg (route-aware) ──
    let (provider, cloud) = rt
        .as_ref()
        .map(|r| (r.voice.tts_provider.clone(), r.voice.cloud.clone()))
        .unwrap_or_else(|| ("auto".to_string(), None));
    let cloud_configured = crate::api::voice_turn::cloud_tts_configured(cloud.as_ref());
    let tts_route = crate::api::voice_turn::classify_tts_route(&provider, cloud_configured);
    let private_asr = super::private_asr::readiness(&state.http_client).await;
    let asr_route = crate::skills_scope::discover_asr_route();
    let ominix_runtime =
        if voice_readiness_needs_ominix(private_asr.is_some(), &asr_route, tts_route) {
            Some(crate::api::ominix_runtime::runtime_status(&state.http_client).await)
        } else {
            None
        };

    // ── ASR leg (route-aware) ──
    let asr = if let Some(private) = private_asr {
        VoiceAsrLeg {
            ready: private.ready,
            mode: "private".into(),
            detail: private.detail,
        }
    } else {
        match &asr_route {
            crate::skills_scope::AsrRoute::External(url) => {
                external_asr_readiness(&state.http_client, url).await
            }
            crate::skills_scope::AsrRoute::Ominix(_) => {
                let runtime = ominix_runtime
                    .as_ref()
                    .expect("OMiniX ASR route requires an OMiniX runtime probe");
                let health_healthy = runtime.health.healthy;
                let ready =
                    crate::api::ominix_runtime::asr_ready(health_healthy, &runtime.voice_models);
                VoiceAsrLeg {
                    ready,
                    mode: "ominix".into(),
                    detail: if ready {
                        "OMiniX ASR ready".into()
                    } else if !health_healthy {
                        "OMiniX voice engine unavailable".into()
                    } else {
                        "OMiniX ASR model not ready".into()
                    },
                }
            }
        }
    };

    // ── TTS leg (route-aware) ──
    let tts = match tts_route {
        crate::api::voice_turn::TtsRoute::Cloud => VoiceTtsLeg {
            ready: cloud_configured,
            mode: "cloud".into(),
            detail: if cloud_configured {
                "Volcano cloud TTS configured".into()
            } else {
                "Cloud TTS selected but credentials missing (appid + VOLC_TTS_TOKEN)".into()
            },
        },
        crate::api::voice_turn::TtsRoute::Local => {
            let runtime = ominix_runtime
                .as_ref()
                .expect("local TTS route requires an OMiniX runtime probe");
            let health_healthy = runtime.health.healthy;
            // The on-device leg needs the TTS MODEL itself: a ready ASR plus
            // a present voice with a missing GPT-SoVITS model would report
            // ready here and then fail inside `synthesize_reply`.
            let engine_ok =
                crate::api::ominix_runtime::tts_engine_ready(health_healthy, &runtime.voice_models);
            // The voice a real turn would synthesize with: live override
            // (PUT /api/my/voice) wins, else the bootstrapped per-profile
            // default (which already overlays the serve default). Mirrors the
            // `resolve_reply_voice` call in the voice-turn path; when the
            // runtime isn't started (LLM leg already not-ready) fall back to
            // the persisted per-profile selection the bootstrap would apply.
            let effective_voice = crate::api::voices::resolve_reply_voice(
                &profile_id,
                rt.as_ref()
                    .map(|r| r.voice.default_voice.as_str())
                    .unwrap_or_else(|| profile.config.voice_default.as_deref().unwrap_or_default()),
            );
            let voice_ok = local_tts_voice_available(&profile_id, &effective_voice);
            let local_ok = engine_ok && voice_ok;
            VoiceTtsLeg {
                ready: local_ok,
                mode: "local".into(),
                detail: if local_ok {
                    "On-device GPT-SoVITS ready".into()
                } else if !health_healthy {
                    "Voice engine unavailable".into()
                } else if !engine_ok {
                    "On-device TTS model not ready".into()
                } else if !effective_voice.is_empty() {
                    format!("Selected voice '{effective_voice}' not available on device")
                } else {
                    "No on-device voice available".into()
                },
            }
        }
    };

    let ready = asr.ready && llm.ready && tts.ready;
    Ok(Json(VoiceReadiness {
        ready,
        asr,
        llm,
        tts,
    }))
}

/// Whether this tenant can synthesize on-device with its EFFECTIVE reply
/// voice.
///
/// `effective_voice` is the voice a real turn would hand `synthesize_reply`
/// (live override > bootstrapped per-profile default — see
/// `resolve_reply_voice`). When it is non-empty, THAT specific voice must be
/// synthesizable and visible to the tenant — some *other* voice existing is
/// not enough, because the turn would still pass the missing selected voice
/// to the engine and fail. Only when no selection resolves anywhere (empty
/// effective voice) does this degrade to "any visible synthesizable voice",
/// matching the engine-side default an empty voice name gets on a real turn.
/// Visibility mirrors `GET /api/voices` scoping (shared presets + voices this
/// profile owns).
fn local_tts_voice_available(profile_id: &str, effective_voice: &str) -> bool {
    let registry_path = crate::api::voices::registry_path();
    match octos_llm::ominix::VoicesRegistry::load(&registry_path) {
        Ok(reg) => voice_available_in_registry(&reg, profile_id, effective_voice),
        Err(_) => false,
    }
}

/// Pure core of [`local_tts_voice_available`] over a loaded registry, so the
/// selected-voice semantics are unit-testable without touching
/// `~/.OminiX/models/voices.json`.
fn voice_available_in_registry(
    reg: &octos_llm::ominix::VoicesRegistry,
    profile_id: &str,
    effective_voice: &str,
) -> bool {
    if effective_voice.is_empty() {
        !reg.synthesizable_visible(|ref_audio| {
            crate::api::voices::voice_visible_to(profile_id, ref_audio)
        })
        .is_empty()
    } else {
        reg.resolve_visible(effective_voice, |ref_audio| {
            crate::api::voices::voice_visible_to(profile_id, ref_audio)
        })
        .is_some()
    }
}

// ── Matrix invite review endpoints ────────────────────────────────────

#[derive(Clone, Debug)]
struct MatrixUserChannelConfig {
    homeserver: String,
    user_id: Option<String>,
    access_token: Option<String>,
    password: Option<String>,
    device_name: Option<String>,
}

#[derive(Clone, Debug)]
struct MatrixResolvedLogin {
    access_token: String,
    user_id: String,
    device_id: Option<String>,
    logout_after: bool,
}

#[derive(Clone, Debug, Serialize)]
struct MatrixSyncProbe {
    joined_rooms: usize,
    pending_invites: usize,
    has_next_batch: bool,
    pending_invite_details: Vec<MatrixSyncInviteSummary>,
}

#[derive(Clone, Debug, Serialize)]
struct MatrixSyncInviteSummary {
    room_id: String,
    room_name: Option<String>,
    canonical_alias: Option<String>,
    inviter: Option<String>,
    membership_event_id: Option<String>,
}

#[derive(Deserialize, Default)]
pub struct MatrixInviteActionRequest {
    #[serde(default)]
    pub channel_index: Option<usize>,
    #[serde(default)]
    pub add_to_allowed_rooms: Option<bool>,
}

#[derive(Deserialize, Default)]
pub struct MatrixTestConnectionRequest {
    #[serde(default)]
    pub channel_index: Option<usize>,
    #[serde(default)]
    pub channel: Option<MatrixTestChannelDraft>,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct MatrixTestChannelDraft {
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub homeserver: Option<String>,
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub access_token: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub device_name: Option<String>,
}

fn matrix_user_channel_config(
    profile: &UserProfile,
    channel_index: usize,
) -> Result<MatrixUserChannelConfig, (StatusCode, String)> {
    let Some(channel) = profile.config.channels.get(channel_index) else {
        return Err((
            StatusCode::NOT_FOUND,
            format!("Matrix channel index {channel_index} not found"),
        ));
    };

    let ChannelCredentials::Matrix {
        homeserver,
        mode,
        user_id,
        access_token,
        password,
        device_name,
        ..
    } = channel
    else {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("Channel index {channel_index} is not a Matrix channel"),
        ));
    };

    if !mode.eq_ignore_ascii_case("user") {
        return Err((
            StatusCode::BAD_REQUEST,
            "Matrix invite review is only supported for user-account mode".into(),
        ));
    }

    let has_token = !access_token.trim().is_empty();
    let has_password_login = !user_id.trim().is_empty() && !password.trim().is_empty();
    if !has_token && !has_password_login {
        return Err((
            StatusCode::BAD_REQUEST,
            "Matrix user channel requires an access token or user_id + password".into(),
        ));
    }
    if !has_token {
        validate_matrix_user_id(user_id)?;
    }

    let homeserver = if homeserver.trim().is_empty() {
        "http://localhost:6167".to_string()
    } else {
        homeserver.trim_end_matches('/').to_string()
    };

    Ok(MatrixUserChannelConfig {
        homeserver,
        user_id: non_empty_string(user_id),
        access_token: non_empty_string(access_token),
        password: non_empty_string(password),
        device_name: non_empty_string(device_name),
    })
}

fn matrix_test_channel_config(
    profile: &UserProfile,
    request: &MatrixTestConnectionRequest,
) -> Result<MatrixUserChannelConfig, (StatusCode, String)> {
    let draft = request.channel.as_ref();
    if let Some(mode) = draft.and_then(|channel| non_empty_string_opt(channel.mode.as_deref())) {
        if !mode.eq_ignore_ascii_case("user") {
            return Err((
                StatusCode::BAD_REQUEST,
                "Matrix connection test is only supported for user-account mode".into(),
            ));
        }
    }

    let mut config = match request
        .channel_index
        .or_else(|| first_matrix_user_channel_index(profile))
    {
        Some(channel_index) => matrix_user_channel_config(profile, channel_index)?,
        None => MatrixUserChannelConfig {
            homeserver: "http://localhost:6167".into(),
            user_id: None,
            access_token: None,
            password: None,
            device_name: None,
        },
    };

    if let Some(channel) = draft {
        if let Some(value) = non_empty_string_opt(channel.homeserver.as_deref()) {
            config.homeserver = value.trim_end_matches('/').to_string();
        }
        if let Some(value) = non_empty_string_opt(channel.user_id.as_deref()) {
            config.user_id = Some(value);
        }
        apply_matrix_draft_secret(&mut config.access_token, channel.access_token.as_deref());
        apply_matrix_draft_secret(&mut config.password, channel.password.as_deref());
        if let Some(value) = non_empty_string_opt(channel.device_name.as_deref()) {
            config.device_name = Some(value);
        }
    }

    if config.access_token.is_none() && (config.user_id.is_none() || config.password.is_none()) {
        return Err((
            StatusCode::BAD_REQUEST,
            "Matrix user channel requires an access token or user_id + password".into(),
        ));
    }
    if config.access_token.is_none() {
        if let Some(user_id) = config.user_id.as_deref() {
            validate_matrix_user_id(user_id)?;
        }
    }

    Ok(config)
}

fn first_matrix_user_channel_index(profile: &UserProfile) -> Option<usize> {
    profile
        .config
        .channels
        .iter()
        .enumerate()
        .find_map(|(idx, channel)| match channel {
            ChannelCredentials::Matrix { mode, .. } if mode.eq_ignore_ascii_case("user") => {
                Some(idx)
            }
            _ => None,
        })
}

fn non_empty_string(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn non_empty_string_opt(value: Option<&str>) -> Option<String> {
    value.and_then(non_empty_string)
}

fn apply_matrix_draft_secret(target: &mut Option<String>, draft: Option<&str>) {
    let Some(value) = draft else {
        return;
    };
    let trimmed = value.trim();
    if is_display_secret_value(trimmed) {
        return;
    }
    if trimmed.is_empty() {
        *target = None;
    } else {
        *target = Some(trimmed.to_string());
    }
}

fn validate_matrix_user_id(user_id: &str) -> Result<(), (StatusCode, String)> {
    let trimmed = user_id.trim();
    if is_matrix_full_user_id(trimmed) || is_matrix_login_localpart(trimmed) {
        return Ok(());
    }
    Err((
        StatusCode::BAD_REQUEST,
        "Matrix login user must be a localpart like octos or a full Matrix ID like @octos:octos.meldry.com; do not use octos:octos.meldry.com without @.".into(),
    ))
}

fn is_matrix_full_user_id(value: &str) -> bool {
    let Some(rest) = value.strip_prefix('@') else {
        return false;
    };
    let Some((localpart, server_name)) = rest.split_once(':') else {
        return false;
    };
    !localpart.is_empty() && !server_name.trim().is_empty()
}

fn is_matrix_login_localpart(value: &str) -> bool {
    !value.is_empty()
        && !value.contains(':')
        && !value.starts_with('@')
        && value.bytes().all(|byte| {
            matches!(
                byte,
                b'a'..=b'z'
                    | b'A'..=b'Z'
                    | b'0'..=b'9'
                    | b'.'
                    | b'_'
                    | b'='
                    | b'-'
                    | b'/'
                    | b'+'
            )
        })
}

fn matrix_percent_encode_path(s: &str) -> String {
    let mut encoded = String::with_capacity(s.len() * 3);
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char)
            }
            _ => {
                encoded.push('%');
                encoded.push_str(&format!("{byte:02X}"));
            }
        }
    }
    encoded
}

const MATRIX_ERROR_BODY_MAX_BYTES: usize = 2048;

fn matrix_api_url(homeserver: &str, path: &str) -> String {
    format!("{}{}", homeserver.trim_end_matches('/'), path)
}

/// HTTP client for Matrix admin calls (whoami/login/join/leave/sync?timeout=0).
///
/// All of these are immediate requests — unlike the gateway's long-poll `/sync`
/// — so a bounded timeout keeps a slow or unreachable user-supplied homeserver
/// from holding an axum worker open indefinitely.
fn matrix_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("failed to build Matrix HTTP client")
}

async fn matrix_error_body(resp: reqwest::Response) -> String {
    let mut buf = Vec::new();
    let mut truncated = resp
        .content_length()
        .map(|len| len > MATRIX_ERROR_BODY_MAX_BYTES as u64)
        .unwrap_or(false);
    let mut stream = resp.bytes_stream();

    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(chunk) => {
                if buf.len() + chunk.len() > MATRIX_ERROR_BODY_MAX_BYTES {
                    let remaining = MATRIX_ERROR_BODY_MAX_BYTES.saturating_sub(buf.len());
                    buf.extend_from_slice(&chunk[..remaining]);
                    truncated = true;
                    break;
                }
                buf.extend_from_slice(&chunk);
            }
            Err(_) => {
                truncated = true;
                break;
            }
        }
    }

    sanitize_matrix_error_body(&String::from_utf8_lossy(&buf), truncated)
}

fn sanitize_matrix_error_body(raw: &str, truncated: bool) -> String {
    let mut out = String::new();
    let mut last_space = false;
    for ch in raw.chars() {
        let ch = if ch.is_control() { ' ' } else { ch };
        if ch.is_whitespace() {
            if !last_space {
                out.push(' ');
                last_space = true;
            }
        } else {
            out.push(ch);
            last_space = false;
        }
    }

    let mut out = out.trim().to_string();
    if truncated {
        if out.is_empty() {
            out.push_str("[truncated]");
        } else {
            out.push_str(" ... [truncated]");
        }
    }
    out
}

async fn resolve_matrix_login(
    http: &reqwest::Client,
    config: &MatrixUserChannelConfig,
) -> Result<MatrixResolvedLogin, (StatusCode, String)> {
    if let Some(token) = config.access_token.as_deref() {
        let url = matrix_api_url(&config.homeserver, "/_matrix/client/v3/account/whoami");
        let resp = http
            .get(&url)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = matrix_error_body(resp).await;
            return Err((
                StatusCode::BAD_GATEWAY,
                format!("Matrix whoami failed (status={status}): {body}"),
            ));
        }
        let payload: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
        let user_id = payload
            .get("user_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .or_else(|| config.user_id.clone())
            .ok_or((
                StatusCode::BAD_GATEWAY,
                "Matrix whoami response missing user_id".into(),
            ))?;
        let device_id = payload
            .get("device_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        return Ok(MatrixResolvedLogin {
            access_token: token.to_string(),
            user_id,
            device_id,
            logout_after: false,
        });
    }

    let user_id = config
        .user_id
        .as_deref()
        .ok_or((StatusCode::BAD_REQUEST, "Matrix user_id is required".into()))?;
    let password = config.password.as_deref().ok_or((
        StatusCode::BAD_REQUEST,
        "Matrix password is required".into(),
    ))?;
    let body = json!({
        "type": "m.login.password",
        "identifier": { "type": "m.id.user", "user": user_id },
        "password": password,
        "initial_device_display_name": config.device_name.as_deref().unwrap_or("octos"),
    });
    let url = matrix_api_url(&config.homeserver, "/_matrix/client/v3/login");
    let resp = http
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = matrix_error_body(resp).await;
        return Err((StatusCode::BAD_GATEWAY, matrix_login_error(status, &body)));
    }
    let payload: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
    let token = payload
        .get("access_token")
        .and_then(serde_json::Value::as_str)
        .ok_or((
            StatusCode::BAD_GATEWAY,
            "Matrix login response missing access_token".into(),
        ))?;
    let user_id = payload
        .get("user_id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| user_id.to_string());
    let device_id = payload
        .get("device_id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    Ok(MatrixResolvedLogin {
        access_token: token.to_string(),
        user_id,
        device_id,
        logout_after: true,
    })
}

fn matrix_login_error(status: reqwest::StatusCode, body: &str) -> String {
    let server_error = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .filter(|message| !message.trim().is_empty())
        .unwrap_or_else(|| body.trim().to_string());
    let lower = server_error.to_ascii_lowercase();
    if lower.contains("delegated authentication") || lower.contains("oidc") {
        return format!(
            "Matrix password login is not available on this homeserver (status={status}): \
             it uses delegated authentication/OIDC. Paste a valid Matrix access token in \
             Access token, or enable password login on the homeserver."
        );
    }
    if server_error.is_empty() {
        format!("Matrix login failed (status={status})")
    } else {
        format!("Matrix login failed (status={status}): {server_error}")
    }
}

async fn logout_matrix_login(
    http: &reqwest::Client,
    homeserver: &str,
    login: &MatrixResolvedLogin,
) {
    if !login.logout_after {
        return;
    }
    let url = matrix_api_url(homeserver, "/_matrix/client/v3/logout");
    if let Err(e) = http
        .post(&url)
        .bearer_auth(&login.access_token)
        .send()
        .await
    {
        tracing::warn!(error = %e, "Matrix logout after invite action failed");
    }
}

async fn matrix_join_room(
    http: &reqwest::Client,
    config: &MatrixUserChannelConfig,
    login: &MatrixResolvedLogin,
    room_id: &str,
) -> Result<(), (StatusCode, String)> {
    let path = format!(
        "/_matrix/client/v3/rooms/{}/join",
        matrix_percent_encode_path(room_id)
    );
    let url = matrix_api_url(&config.homeserver, &path);
    let resp = http
        .post(&url)
        .bearer_auth(&login.access_token)
        .json(&json!({}))
        .send()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = matrix_error_body(resp).await;
        return Err((
            StatusCode::BAD_GATEWAY,
            format!("Matrix join room failed (status={status}): {body}"),
        ));
    }
    Ok(())
}

async fn matrix_leave_room(
    http: &reqwest::Client,
    config: &MatrixUserChannelConfig,
    login: &MatrixResolvedLogin,
    room_id: &str,
) -> Result<(), (StatusCode, String)> {
    let path = format!(
        "/_matrix/client/v3/rooms/{}/leave",
        matrix_percent_encode_path(room_id)
    );
    let url = matrix_api_url(&config.homeserver, &path);
    let resp = http
        .post(&url)
        .bearer_auth(&login.access_token)
        .json(&json!({}))
        .send()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = matrix_error_body(resp).await;
        return Err((
            StatusCode::BAD_GATEWAY,
            format!("Matrix reject invite failed (status={status}): {body}"),
        ));
    }
    Ok(())
}

async fn matrix_probe_sync(
    http: &reqwest::Client,
    config: &MatrixUserChannelConfig,
    login: &MatrixResolvedLogin,
) -> Result<MatrixSyncProbe, (StatusCode, String)> {
    let url = matrix_api_url(&config.homeserver, "/_matrix/client/v3/sync?timeout=0");
    let resp = http
        .get(&url)
        .bearer_auth(&login.access_token)
        .send()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = matrix_error_body(resp).await;
        return Err((
            StatusCode::BAD_GATEWAY,
            format!("Matrix sync probe failed (status={status}): {body}"),
        ));
    }
    let payload: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
    let rooms = payload.get("rooms");
    let joined_rooms = rooms
        .and_then(|rooms| rooms.get("join"))
        .and_then(serde_json::Value::as_object)
        .map_or(0, serde_json::Map::len);
    let invite_rooms = rooms
        .and_then(|rooms| rooms.get("invite"))
        .and_then(serde_json::Value::as_object);
    let pending_invites = invite_rooms.map_or(0, serde_json::Map::len);
    let pending_invite_details = invite_rooms
        .map(|rooms| matrix_sync_invite_details(rooms, &login.user_id))
        .unwrap_or_default();
    let has_next_batch = payload
        .get("next_batch")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|value| !value.trim().is_empty());
    Ok(MatrixSyncProbe {
        joined_rooms,
        pending_invites,
        has_next_batch,
        pending_invite_details,
    })
}

fn matrix_sync_invite_details(
    invite_rooms: &serde_json::Map<String, serde_json::Value>,
    self_user_id: &str,
) -> Vec<MatrixSyncInviteSummary> {
    invite_rooms
        .iter()
        .map(|(room_id, room)| matrix_sync_invite_detail(room_id, room, self_user_id))
        .collect()
}

fn matrix_sync_invite_detail(
    room_id: &str,
    room: &serde_json::Value,
    self_user_id: &str,
) -> MatrixSyncInviteSummary {
    let mut summary = MatrixSyncInviteSummary {
        room_id: room_id.to_string(),
        room_name: None,
        canonical_alias: None,
        inviter: None,
        membership_event_id: None,
    };

    let Some(events) = room
        .get("invite_state")
        .and_then(|state| state.get("events"))
        .and_then(serde_json::Value::as_array)
    else {
        return summary;
    };

    for event in events {
        let event_type = event.get("type").and_then(serde_json::Value::as_str);
        let content = event.get("content").unwrap_or(&serde_json::Value::Null);
        match event_type {
            Some("m.room.name") if summary.room_name.is_none() => {
                summary.room_name = content
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string);
            }
            Some("m.room.canonical_alias") if summary.canonical_alias.is_none() => {
                summary.canonical_alias = content
                    .get("alias")
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string);
            }
            Some("m.room.member") => {
                let membership = content
                    .get("membership")
                    .and_then(serde_json::Value::as_str);
                let state_key = event.get("state_key").and_then(serde_json::Value::as_str);
                if membership == Some("invite")
                    && (state_key.is_none() || state_key == Some(self_user_id))
                {
                    summary.inviter = event
                        .get("sender")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string);
                    summary.membership_event_id = event
                        .get("event_id")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string);
                }
            }
            _ => {}
        }
    }

    summary
}

fn resolve_invite_channel_index(
    profile: &UserProfile,
    store: &octos_bus::MatrixInviteStore,
    room_id: &str,
    requested: Option<usize>,
) -> Result<usize, (StatusCode, String)> {
    if let Some(channel_index) = requested {
        return Ok(channel_index);
    }
    let invites = store
        .list(true)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if let Some(invite) = invites.iter().find(|invite| invite.room_id == room_id) {
        return Ok(invite.channel_index);
    }
    first_matrix_user_channel_index(profile).ok_or((
        StatusCode::NOT_FOUND,
        "No Matrix user-account channel configured".into(),
    ))
}

fn add_room_to_matrix_allowlist(
    profile: &mut UserProfile,
    channel_index: usize,
    room_id: &str,
) -> Result<bool, (StatusCode, String)> {
    let Some(channel) = profile.config.channels.get_mut(channel_index) else {
        return Err((
            StatusCode::NOT_FOUND,
            format!("Matrix channel index {channel_index} not found"),
        ));
    };
    let ChannelCredentials::Matrix { rooms, .. } = channel else {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("Channel index {channel_index} is not a Matrix channel"),
        ));
    };
    if rooms.iter().any(|room| room == room_id || room == "*") {
        return Ok(false);
    }
    rooms.push(room_id.to_string());
    Ok(true)
}

/// POST /api/my/profile/matrix/test
pub async fn test_my_matrix_connection(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<AuthIdentity>,
    Json(req): Json<MatrixTestConnectionRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let ps = state.profile_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;
    let profile = resolve_my_profile(&identity, ps, &state, &headers)
        .map_err(|s| (s, "profile not found".into()))?;
    let config = matrix_test_channel_config(&profile, &req)?;

    let http = matrix_http_client();
    let login = resolve_matrix_login(&http, &config).await?;
    let sync_result = matrix_probe_sync(&http, &config, &login).await;
    logout_matrix_login(&http, &config.homeserver, &login).await;
    let probe = sync_result?;

    Ok(Json(json!({
        "ok": true,
        "message": "Matrix connection is healthy",
        "homeserver": config.homeserver,
        "user_id": login.user_id,
        "device_id": login.device_id,
        "joined_rooms": probe.joined_rooms,
        "pending_invites": probe.pending_invites,
        "pending_invite_details": probe.pending_invite_details,
        "sync": {
            "ok": true,
            "has_next_batch": probe.has_next_batch,
        },
    })))
}

/// GET /api/my/profile/matrix/invites
pub async fn my_matrix_invites(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<AuthIdentity>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let ps = state.profile_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;
    let profile = resolve_my_profile(&identity, ps, &state, &headers)
        .map_err(|s| (s, "profile not found".into()))?;
    let data_dir = ps.resolve_data_dir(&profile);
    let store = octos_bus::MatrixInviteStore::for_profile_data_dir(&data_dir);
    let invites = store
        .list(false)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({ "invites": invites })))
}

/// POST /api/my/profile/matrix/invites/:room_id/accept
pub async fn accept_my_matrix_invite(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<AuthIdentity>,
    Path(room_id): Path<String>,
    Json(req): Json<MatrixInviteActionRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let ps = state.profile_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;
    let mut profile = resolve_my_profile(&identity, ps, &state, &headers)
        .map_err(|s| (s, "profile not found".into()))?;
    let data_dir = ps.resolve_data_dir(&profile);
    let store = octos_bus::MatrixInviteStore::for_profile_data_dir(&data_dir);
    let channel_index =
        resolve_invite_channel_index(&profile, &store, &room_id, req.channel_index)?;
    let config = matrix_user_channel_config(&profile, channel_index)?;

    let http = matrix_http_client();
    let login = resolve_matrix_login(&http, &config).await?;
    let join_result = matrix_join_room(&http, &config, &login, &room_id).await;
    logout_matrix_login(&http, &config.homeserver, &login).await;
    join_result?;

    let add_to_allowed_rooms = req.add_to_allowed_rooms.unwrap_or(true);
    let allowlist_updated = if add_to_allowed_rooms {
        add_room_to_matrix_allowlist(&mut profile, channel_index, &room_id)?
    } else {
        false
    };
    if allowlist_updated {
        profile.updated_at = chrono::Utc::now();
        ps.save_with_merge(&mut profile).map_err(|e| {
            tracing::error!(profile = %profile.id, error = %e, "failed to save Matrix room allowlist");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
        })?;
    }
    store
        .remove(channel_index, &room_id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({
        "ok": true,
        "message": if allowlist_updated {
            "Matrix invite accepted and room added to allowed rooms"
        } else {
            "Matrix invite accepted"
        },
        "allowlist_updated": allowlist_updated,
    })))
}

/// POST /api/my/profile/matrix/invites/:room_id/reject
pub async fn reject_my_matrix_invite(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<AuthIdentity>,
    Path(room_id): Path<String>,
    Json(req): Json<MatrixInviteActionRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let ps = state.profile_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;
    let profile = resolve_my_profile(&identity, ps, &state, &headers)
        .map_err(|s| (s, "profile not found".into()))?;
    let data_dir = ps.resolve_data_dir(&profile);
    let store = octos_bus::MatrixInviteStore::for_profile_data_dir(&data_dir);
    let channel_index =
        resolve_invite_channel_index(&profile, &store, &room_id, req.channel_index)?;
    let config = matrix_user_channel_config(&profile, channel_index)?;

    let http = matrix_http_client();
    let login = resolve_matrix_login(&http, &config).await?;
    let leave_result = matrix_leave_room(&http, &config, &login, &room_id).await;
    logout_matrix_login(&http, &config.homeserver, &login).await;
    leave_result?;

    store
        .remove(channel_index, &room_id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({
        "ok": true,
        "message": "Matrix invite rejected",
    })))
}

/// POST /api/my/profile/matrix/invites/:room_id/dismiss
pub async fn dismiss_my_matrix_invite(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<AuthIdentity>,
    Path(room_id): Path<String>,
    Json(req): Json<MatrixInviteActionRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let ps = state.profile_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;
    let profile = resolve_my_profile(&identity, ps, &state, &headers)
        .map_err(|s| (s, "profile not found".into()))?;
    let data_dir = ps.resolve_data_dir(&profile);
    let store = octos_bus::MatrixInviteStore::for_profile_data_dir(&data_dir);
    let channel_index =
        resolve_invite_channel_index(&profile, &store, &room_id, req.channel_index)?;
    let dismissed = store
        .dismiss(channel_index, &room_id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({
        "ok": true,
        "dismissed": dismissed,
        "message": "Matrix invite dismissed locally",
    })))
}

// ── Soul endpoints ───────────────────────────────────────────────────

#[derive(Serialize)]
pub struct SoulResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// GET /api/my/soul
pub async fn my_soul(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<AuthIdentity>,
) -> Result<Json<SoulResponse>, StatusCode> {
    let ps = state
        .profile_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let profile = resolve_my_profile(&identity, ps, &state, &headers)?;
    let data_dir = ps.resolve_data_dir(&profile);
    let content = crate::soul_service::read_soul(&data_dir);
    Ok(Json(SoulResponse {
        ok: true,
        content,
        message: None,
    }))
}

#[derive(Deserialize)]
pub struct UpdateSoulRequest {
    pub content: String,
}

/// PUT /api/my/soul
pub async fn update_my_soul(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<AuthIdentity>,
    Json(req): Json<UpdateSoulRequest>,
) -> Result<Json<SoulResponse>, (StatusCode, String)> {
    if req.content.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "content must not be empty".into()));
    }
    let ps = state.profile_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;
    let profile = resolve_my_profile(&identity, ps, &state, &headers)
        .map_err(|s| (s, "profile not found".into()))?;
    let data_dir = ps.resolve_data_dir(&profile);
    crate::soul_service::write_soul(&data_dir, &req.content)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    tracing::info!(profile = %profile.id, "soul updated via API");
    Ok(Json(SoulResponse {
        ok: true,
        content: Some(req.content.trim().to_string()),
        message: Some("Soul updated. Takes effect in new sessions.".into()),
    }))
}

/// DELETE /api/my/soul
pub async fn delete_my_soul(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<AuthIdentity>,
) -> Result<Json<SoulResponse>, StatusCode> {
    let ps = state
        .profile_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let profile = resolve_my_profile(&identity, ps, &state, &headers)?;
    let data_dir = ps.resolve_data_dir(&profile);
    crate::soul_service::remove_soul(&data_dir).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    tracing::info!(profile = %profile.id, "soul reset via API");
    Ok(Json(SoulResponse {
        ok: true,
        content: None,
        message: Some("Soul reset to default.".into()),
    }))
}

// ── Content catalog endpoints ────────────────────────────────────────

// Helper for `ui_protocol_transport::handle_content_list` (M12 Phase D-5).
// The REST route `GET /api/my/content` was retired in this milestone; the
// function survives as a private helper that the WS dispatcher calls
// directly to back the `content/list` RPC method. Downgraded to
// `pub(super)` so the public API surface no longer exposes a fn whose
// route was removed.
/// Helper backing the WS `content/list` RPC method (formerly `GET /api/my/content`).
pub(super) async fn my_content(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    axum::Extension(identity): axum::Extension<AuthIdentity>,
    axum::extract::Query(query): axum::extract::Query<crate::content_catalog::ContentQuery>,
) -> Result<Json<crate::content_catalog::ContentQueryResult>, (StatusCode, String)> {
    let ps = state
        .profile_store
        .as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, "not configured".into()))?;
    let mgr = state.content_catalog_mgr.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "content catalog not configured".into(),
    ))?;
    // Use X-Profile-Id header (from Caddy proxy) if available, otherwise resolve from identity.
    //
    // Codex P1 fix (PR #958 review): authorize the X-Profile-Id branch the
    // same way the host-scoped path does. Without `is_authorized_for_profile`
    // a bearer-authenticated user could pass any tenant id and read its
    // catalog, since the bearer auth completes before the middleware's
    // loopback-only X-Profile-Id check runs. The new check matches the
    // semantics enforced by `resolve_my_profile_id`'s host-scoped branch:
    // admin can target any tenant, users can target their own profile or
    // sub-accounts they own. Cross-tenant access returns 403.
    let profile = if let Some(pid) = headers.get("x-profile-id").and_then(|v| v.to_str().ok()) {
        if !is_authorized_for_profile(&state, &identity, pid) {
            tracing::warn!(
                identity = ?identity,
                requested_profile = %pid,
                "GET /api/my/content X-Profile-Id denied — identity not authorized for the profile"
            );
            return Err((StatusCode::FORBIDDEN, "forbidden".into()));
        }
        ps.get(pid)
            .map_err(|_| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "profile store error".into(),
                )
            })?
            .ok_or((StatusCode::NOT_FOUND, format!("profile '{pid}' not found")))?
    } else {
        resolve_my_profile(&identity, ps, &state, &headers)
            .map_err(|s| (s, "profile not found".into()))?
    };
    let data_dir = ps.resolve_data_dir(&profile);

    let catalog = mgr
        .get_catalog_with_scan(&profile.id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let cat = catalog.read().await;
    let result = cat.query(&query);
    let entries = result
        .entries
        .into_iter()
        .filter_map(|mut entry| {
            let handle =
                response_path_for_profile_file(&data_dir, std::path::Path::new(&entry.path))?;
            entry.path = handle;
            entry.thumbnail_path = entry
                .thumbnail_path
                .as_ref()
                .map(|_| "available".to_string());
            Some(entry)
        })
        .collect();
    Ok(Json(crate::content_catalog::ContentQueryResult {
        entries,
        total: result.total,
    }))
}

/// GET /api/my/content/:id/thumbnail
pub async fn my_content_thumbnail(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<AuthIdentity>,
    Path(id): Path<String>,
) -> Result<axum::response::Response, StatusCode> {
    use axum::body::Body;
    use axum::http::header;
    use axum::response::IntoResponse;

    let ps = state
        .profile_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let mgr = state
        .content_catalog_mgr
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let profile = resolve_my_profile(&identity, ps, &state, &headers)?;

    let catalog = mgr
        .get_catalog(&profile.id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let cat = catalog.read().await;
    let entry = cat.get(&id).ok_or(StatusCode::NOT_FOUND)?;
    let thumb_path = entry.thumbnail_path.as_ref().ok_or(StatusCode::NOT_FOUND)?;

    let data = tokio::fs::read(thumb_path)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;

    Ok(([(header::CONTENT_TYPE, "image/jpeg")], Body::from(data)).into_response())
}

/// GET /api/my/content/:id/body
pub async fn my_content_body(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<AuthIdentity>,
    Path(id): Path<String>,
) -> Result<axum::response::Response, StatusCode> {
    use axum::body::Body;
    use axum::http::header;
    use axum::response::IntoResponse;

    let ps = state
        .profile_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let mgr = state
        .content_catalog_mgr
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let profile = resolve_my_profile(&identity, ps, &state, &headers)?;

    let catalog = mgr
        .get_catalog(&profile.id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let cat = catalog.read().await;
    let entry = cat.get(&id).ok_or(StatusCode::NOT_FOUND)?;
    let path = &entry.path;

    let data = tokio::fs::read(path)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;

    // Determine content type from extension.
    let content_type = match entry.category {
        crate::content_catalog::ContentCategory::Report => {
            if entry.filename.ends_with(".md") {
                "text/markdown; charset=utf-8"
            } else {
                "text/plain; charset=utf-8"
            }
        }
        crate::content_catalog::ContentCategory::Image => "image/png",
        crate::content_catalog::ContentCategory::Audio => "audio/mpeg",
        crate::content_catalog::ContentCategory::Video => "video/mp4",
        _ => "application/octet-stream",
    };

    Ok(([(header::CONTENT_TYPE, content_type)], Body::from(data)).into_response())
}

// Helper for `ui_protocol_transport::handle_content_delete` (M12 Phase D-5).
// The REST route `DELETE /api/my/content/{id}` was retired in this
// milestone; the function survives as a private helper backing the
// `content/delete` WS RPC method.
/// Helper backing the WS `content/delete` RPC method (formerly `DELETE /api/my/content/{id}`).
pub(super) async fn delete_my_content(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<AuthIdentity>,
    Path(id): Path<String>,
) -> Result<Json<ActionResponse>, (StatusCode, String)> {
    let ps = state
        .profile_store
        .as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, "not configured".into()))?;
    let mgr = state.content_catalog_mgr.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "content catalog not configured".into(),
    ))?;
    let profile = resolve_my_profile(&identity, ps, &state, &headers)
        .map_err(|s| (s, "profile not found".into()))?;

    let catalog = mgr
        .get_catalog(&profile.id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let mut cat = catalog.write().await;
    let deleted = cat
        .delete(&id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(ActionResponse {
        ok: deleted,
        message: if deleted {
            Some("Content deleted.".into())
        } else {
            Some("Content not found.".into())
        },
    }))
}

#[derive(Deserialize)]
pub(super) struct BulkDeleteRequest {
    pub ids: Vec<String>,
}

// Helper for `ui_protocol_transport::handle_content_bulk_delete` (M12 Phase D-5).
// The REST route `POST /api/my/content/bulk-delete` was retired in this
// milestone; the function survives as a private helper backing the
// `content/bulk_delete` WS RPC method.
/// Helper backing the WS `content/bulk_delete` RPC method (formerly `POST /api/my/content/bulk-delete`).
pub(super) async fn bulk_delete_my_content(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<AuthIdentity>,
    Json(req): Json<BulkDeleteRequest>,
) -> Result<Json<ActionResponse>, (StatusCode, String)> {
    let ps = state
        .profile_store
        .as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, "not configured".into()))?;
    let mgr = state.content_catalog_mgr.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "content catalog not configured".into(),
    ))?;
    let profile = resolve_my_profile(&identity, ps, &state, &headers)
        .map_err(|s| (s, "profile not found".into()))?;

    let catalog = mgr
        .get_catalog(&profile.id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let mut cat = catalog.write().await;
    let deleted = cat
        .bulk_delete(&req.ids)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(ActionResponse {
        ok: true,
        message: Some(format!("{deleted} item(s) deleted.")),
    }))
}

/// POST /api/my/profile/start
pub async fn start_my_gateway(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<AuthIdentity>,
) -> Result<Json<ActionResponse>, StatusCode> {
    let ps = state
        .profile_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let pm = state
        .process_manager
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    let profile = resolve_my_profile(&identity, ps, &state, &headers)?;

    // Validate LLM provider is configured
    if profile.config.primary_provider().is_none() && profile.config.primary_model().is_none() {
        return Ok(Json(ActionResponse {
            ok: false,
            message: Some("Cannot start: LLM provider must be configured first".into()),
        }));
    }

    match pm.start(&profile).await {
        Ok(()) => {
            tracing::info!(profile = %profile.id, "user gateway started");
            Ok(Json(ActionResponse {
                ok: true,
                message: None,
            }))
        }
        Err(e) => {
            tracing::error!(profile = %profile.id, error = %e, "user gateway failed to start");
            Ok(Json(ActionResponse {
                ok: false,
                message: Some(e.to_string()),
            }))
        }
    }
}

/// POST /api/my/profile/stop
pub async fn stop_my_gateway(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<AuthIdentity>,
) -> Result<Json<ActionResponse>, StatusCode> {
    let ps = state
        .profile_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let profile_id = resolve_my_profile_id(&identity, ps, &state, &headers)?;
    let pm = state
        .process_manager
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    let stopped = pm.stop(&profile_id).await.unwrap_or(false);
    if stopped {
        tracing::info!(profile = %profile_id, "user gateway stopped");
        Ok(Json(ActionResponse {
            ok: true,
            message: None,
        }))
    } else {
        tracing::warn!(profile = %profile_id, "user stop requested but gateway not running");
        Ok(Json(ActionResponse {
            ok: false,
            message: Some("Gateway not running".into()),
        }))
    }
}

/// POST /api/my/profile/restart
pub async fn restart_my_gateway(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<AuthIdentity>,
) -> Result<Json<ActionResponse>, StatusCode> {
    let ps = state
        .profile_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let pm = state
        .process_manager
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    let profile = resolve_my_profile(&identity, ps, &state, &headers)?;

    match pm.restart(&profile).await {
        Ok(()) => {
            tracing::info!(profile = %profile.id, "user gateway restarted");
            Ok(Json(ActionResponse {
                ok: true,
                message: None,
            }))
        }
        Err(e) => {
            tracing::error!(profile = %profile.id, error = %e, "user gateway failed to restart");
            Ok(Json(ActionResponse {
                ok: false,
                message: Some(e.to_string()),
            }))
        }
    }
}

/// GET /api/my/profile/status
pub async fn my_gateway_status(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<AuthIdentity>,
) -> Result<Json<crate::process_manager::ProcessStatus>, StatusCode> {
    let ps = state
        .profile_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let profile_id = resolve_my_profile_id(&identity, ps, &state, &headers)?;
    let pm = state
        .process_manager
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    Ok(Json(pm.status(&profile_id).await))
}

/// GET /api/my/profile/logs
pub async fn my_gateway_logs(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<AuthIdentity>,
) -> Result<Sse<impl futures::Stream<Item = Result<Event, std::convert::Infallible>>>, StatusCode> {
    let ps = state
        .profile_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let profile_id = resolve_my_profile_id(&identity, ps, &state, &headers)?;
    let pm = state
        .process_manager
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    // Get buffered history first, then subscribe for live logs.
    let history = pm.log_history(&profile_id).await;
    let rx = pm
        .subscribe_logs(&profile_id)
        .await
        .ok_or(StatusCode::NOT_FOUND)?;

    let history_stream = futures::stream::iter(
        history
            .into_iter()
            .map(|line| Ok(Event::default().data(line))),
    );
    let live_stream = futures::stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(line) => {
                    let event: Result<Event, std::convert::Infallible> =
                        Ok(Event::default().data(line));
                    return Some((event, rx));
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
            }
        }
    });

    Ok(Sse::new(history_stream.chain(live_stream)).keep_alive(KeepAlive::default()))
}

/// GET /api/my/profile/whatsapp/qr
pub async fn my_whatsapp_qr(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<AuthIdentity>,
) -> Result<Json<crate::process_manager::BridgeQrInfo>, StatusCode> {
    let ps = state
        .profile_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let profile_id = resolve_my_profile_id(&identity, ps, &state, &headers)?;
    let pm = state
        .process_manager
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    pm.bridge_qr(&profile_id)
        .await
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

/// GET /api/my/profile/metrics
pub async fn my_provider_metrics(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<AuthIdentity>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let ps = state
        .profile_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let profile_id = resolve_my_profile_id(&identity, ps, &state, &headers)?;
    let pm = state
        .process_manager
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    match pm.read_metrics(&profile_id).await {
        Some(metrics) => Ok(Json(metrics)),
        None => Ok(Json(serde_json::json!(null))),
    }
}

/// GET /api/my/profile/accounts — List sub-accounts for the current user's profile.
pub async fn my_sub_accounts(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<AuthIdentity>,
) -> Result<Json<Vec<crate::api::admin::ProfileResponse>>, StatusCode> {
    let ps = state
        .profile_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let profile_id = resolve_my_profile_id(&identity, ps, &state, &headers)?;
    let pm = state
        .process_manager
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    let subs = ps
        .list_sub_accounts(&profile_id)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let mut items = Vec::with_capacity(subs.len());
    for s in subs {
        let status = pm.status(&s.id).await;
        items.push(crate::api::admin::ProfileResponse {
            email: None,
            profile: crate::profiles::mask_secrets(&s),
            status,
        });
    }
    Ok(Json(items))
}

fn resolve_my_managed_parent_profile(
    identity: &AuthIdentity,
    ps: &crate::profiles::ProfileStore,
    state: &AppState,
    headers: &HeaderMap,
) -> Result<crate::profiles::UserProfile, StatusCode> {
    let profile = resolve_my_profile(identity, ps, state, headers)?;
    if profile.parent_id.is_some() {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(profile)
}

/// GET /api/my/profile/accounts/:id — Return a sub-account managed by the current user.
pub async fn my_sub_account(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<AuthIdentity>,
    Path(sub_id): Path<String>,
) -> Result<Json<crate::api::admin::ProfileResponse>, StatusCode> {
    let ps = state
        .profile_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let pm = state
        .process_manager
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    let sub = resolve_my_sub_account(&identity, ps, &state, &headers, &sub_id)?;
    let status = pm.status(&sub.id).await;
    Ok(Json(crate::api::admin::ProfileResponse {
        email: None,
        profile: crate::profiles::mask_secrets(&sub),
        status,
    }))
}

/// POST /api/my/profile/accounts — Create a sub-account owned by the current user.
pub async fn create_my_sub_account(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<AuthIdentity>,
    Json(req): Json<crate::api::admin::CreateSubAccountRequest>,
) -> Result<(StatusCode, Json<crate::api::admin::ProfileResponse>), (StatusCode, String)> {
    let ps = state.profile_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;
    let pm = state.process_manager.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;

    let parent = resolve_my_managed_parent_profile(&identity, ps, &state, &headers)
        .map_err(|status| (status, "sub-accounts cannot create sub-accounts".into()))?;

    if !req.channels.is_empty() {
        super::admin::validate_channel_credentials(&req.channels)
            .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    }

    let mut sub = ps
        .create_sub_account(
            &parent.id,
            &req.sub_account_id,
            &req.public_subdomain,
            &req.name,
            req.channels,
            req.gateway.unwrap_or_default(),
        )
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;

    if !req.env_vars.is_empty() {
        sub.config.env_vars = req.env_vars;
        // Relocate keychain-backed secrets (e.g. the Vertex SA JSON) before
        // persisting so a sub-account never writes a private key to disk.
        let sub_id = sub.id.clone();
        super::admin::relocate_keychain_backed_secrets(&mut sub.config.env_vars, &sub_id)?;
        sub.updated_at = chrono::Utc::now();
        ps.save(&sub)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }

    if let Some(email) = &req.email {
        let email = email.trim().to_lowercase();
        if !email.is_empty() {
            super::admin::validate_email(&email).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
            if let Some(user_store) = state.user_store.as_ref() {
                if let Ok(Some(_existing)) = user_store.get_by_email(&email) {
                    return Err((
                        StatusCode::CONFLICT,
                        format!("Email '{email}' is already registered to another account"),
                    ));
                }
                let user = crate::user_store::User {
                    id: sub.id.clone(),
                    email,
                    name: sub.name.clone(),
                    role: crate::user_store::UserRole::User,
                    created_at: chrono::Utc::now(),
                    last_login_at: None,
                };
                user_store
                    .save(&user)
                    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            }
        }
    }

    let status = pm.status(&sub.id).await;
    Ok((
        StatusCode::CREATED,
        Json(crate::api::admin::ProfileResponse {
            email: None,
            profile: crate::profiles::mask_secrets(&sub),
            status,
        }),
    ))
}

/// PUT /api/my/profile/accounts/:id — Update a managed sub-account.
pub async fn update_my_sub_account(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<AuthIdentity>,
    Path(sub_id): Path<String>,
    body: String,
) -> Result<Json<crate::api::admin::ProfileResponse>, (StatusCode, String)> {
    let req: crate::api::admin::UpdateProfileRequest =
        serde_json::from_str(&body).map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                format!("Invalid request body: {e}"),
            )
        })?;
    let ps = state.profile_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;
    let pm = state.process_manager.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;

    let _parent = resolve_my_managed_parent_profile(&identity, ps, &state, &headers)
        .map_err(|status| (status, "sub-accounts cannot manage sub-accounts".into()))?;
    let mut sub = resolve_my_sub_account(&identity, ps, &state, &headers, &sub_id)
        .map_err(|status| (status, "sub-account not found".into()))?;

    if let Some(name) = req.name {
        sub.name = name;
    }
    if let Some(public_subdomain) = req.public_subdomain {
        match public_subdomain
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            Some(slug) => sub.public_subdomain = Some(slug.to_string()),
            None => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    "sub-accounts must keep a public subdomain".into(),
                ));
            }
        }
    }
    if let Some(enabled) = req.enabled {
        sub.enabled = enabled;
    }
    super::admin::merge_profile_config_from_body(&mut sub.config, &body, true)
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    // Relocate keychain-backed secrets (e.g. the Vertex SA JSON) before
    // persisting so a sub-account never writes a private key to disk.
    let sub_id = sub.id.clone();
    super::admin::relocate_keychain_backed_secrets(&mut sub.config.env_vars, &sub_id)?;
    sub.updated_at = chrono::Utc::now();

    ps.save_with_merge(&mut sub)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    if let Some(email) = &req.email {
        let email = email.trim().to_lowercase();
        if !email.is_empty() {
            super::admin::validate_email(&email).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
            if let Some(user_store) = state.user_store.as_ref() {
                if let Ok(Some(existing)) = user_store.get_by_email(&email) {
                    if existing.id != sub_id {
                        return Err((
                            StatusCode::CONFLICT,
                            format!("Email '{email}' is already registered to another account"),
                        ));
                    }
                }
                let user = match user_store.get(&sub_id) {
                    Ok(Some(mut u)) => {
                        u.email = email;
                        u.name = sub.name.clone();
                        u
                    }
                    _ => crate::user_store::User {
                        id: sub.id.clone(),
                        email,
                        name: sub.name.clone(),
                        role: crate::user_store::UserRole::User,
                        created_at: chrono::Utc::now(),
                        last_login_at: None,
                    },
                };
                user_store
                    .save(&user)
                    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            }
        }
    }

    let status = pm.status(&sub.id).await;
    Ok(Json(crate::api::admin::ProfileResponse {
        email: None,
        profile: crate::profiles::mask_secrets(&sub),
        status,
    }))
}

/// Helper: resolve a sub-account owned by the current user.
fn resolve_my_sub_account(
    identity: &AuthIdentity,
    ps: &crate::profiles::ProfileStore,
    state: &AppState,
    headers: &HeaderMap,
    sub_id: &str,
) -> Result<crate::profiles::UserProfile, StatusCode> {
    let parent_id = resolve_my_profile_id(identity, ps, state, headers)?;
    let sub = ps
        .get(sub_id)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    // Ensure the sub-account belongs to this user
    if sub.parent_id.as_deref() != Some(&parent_id) {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(sub)
}

/// POST /api/my/profile/accounts/:id/start — Start a sub-account gateway.
pub async fn start_my_sub_gateway(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<AuthIdentity>,
    Path(sub_id): Path<String>,
) -> Result<Json<ActionResponse>, StatusCode> {
    let ps = state
        .profile_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let pm = state
        .process_manager
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    let sub = resolve_my_sub_account(&identity, ps, &state, &headers, &sub_id)?;

    match pm.start(&sub).await {
        Ok(()) => Ok(Json(ActionResponse {
            ok: true,
            message: Some(format!("Gateway '{}' started", sub.id)),
        })),
        Err(e) => Ok(Json(ActionResponse {
            ok: false,
            message: Some(e.to_string()),
        })),
    }
}

/// POST /api/my/profile/accounts/:id/stop — Stop a sub-account gateway.
pub async fn stop_my_sub_gateway(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<AuthIdentity>,
    Path(sub_id): Path<String>,
) -> Result<Json<ActionResponse>, StatusCode> {
    let ps = state
        .profile_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let pm = state
        .process_manager
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    let _ = resolve_my_sub_account(&identity, ps, &state, &headers, &sub_id)?;

    match pm.stop(&sub_id).await {
        Ok(_) => Ok(Json(ActionResponse {
            ok: true,
            message: Some(format!("Gateway '{}' stopped", sub_id)),
        })),
        Err(e) => Ok(Json(ActionResponse {
            ok: false,
            message: Some(e.to_string()),
        })),
    }
}

// ── Helpers ───────────────────────────────────────────────────────────

/// Return `true` iff the authenticated identity is allowed to act as the
/// given profile id for `/api/my/*` endpoints.
///
/// Authorization rules:
/// - Admin token can act as any profile.
/// - A user session with `UserRole::Admin` can act as any profile
///   (matches the rest of the router which treats admin email sessions
///   as full admins for `/api/admin/*`). Without this carve-out, an
///   admin who logs in via OTP would 403 on tenant subdomains while
///   the bootstrap admin token would not — codex P2 (PR #958 review).
/// - A user can act as their own profile.
/// - A user (top-level account) can also act as any sub-account they own.
/// - Everyone else is denied (returns `false`).
pub(crate) fn is_authorized_for_profile(
    state: &AppState,
    identity: &AuthIdentity,
    profile_id: &str,
) -> bool {
    match identity {
        AuthIdentity::Admin => true,
        AuthIdentity::User {
            role: UserRole::Admin,
            ..
        } => true,
        AuthIdentity::User { id, .. } => {
            if id == profile_id {
                return true;
            }
            // Allow a top-level user to act as any of their sub-accounts.
            let Some(store) = state.profile_store.as_ref() else {
                return false;
            };
            match store.get(profile_id) {
                Ok(Some(profile)) => profile.parent_id.as_deref() == Some(id.as_str()),
                _ => false,
            }
        }
    }
}

/// Resolve the profile ID for "my" endpoints.
///
/// Server-side host-authoritative scoping (Option Y, closes #315):
/// 1. If the request `Host` / `X-Forwarded-Host` header resolves to a
///    tenant profile via `host_scoped_profile_id`, return that profile id
///    — but only after verifying the authenticated identity is allowed
///    to view it. If the identity is NOT authorized, return 403 rather
///    than silently falling through to the identity's default profile,
///    which would be both confusing and a cross-tenant data leak.
/// 2. Otherwise (no tenant subdomain, unknown host, or local request),
///    fall back to the identity-based default: admin token returns the
///    fixed admin profile id, user sessions return the user's own id.
///
/// For regular users, returns their user ID. For admin token, returns the admin's own profile ID
/// (auto-creating the admin profile if it doesn't exist yet).
pub(crate) fn resolve_my_profile_id(
    identity: &AuthIdentity,
    ps: &crate::profiles::ProfileStore,
    state: &AppState,
    headers: &HeaderMap,
) -> Result<String, StatusCode> {
    if let Some(scoped) = host_scoped_profile_id(state, headers) {
        if !is_authorized_for_profile(state, identity, &scoped) {
            tracing::warn!(
                identity = ?identity,
                scoped_profile = %scoped,
                "/api/my/* host-scope denied — identity not authorized for the tenant subdomain"
            );
            return Err(StatusCode::FORBIDDEN);
        }
        return Ok(scoped);
    }

    match identity {
        AuthIdentity::Admin => {
            ensure_admin_profile(ps)?;
            Ok(ADMIN_PROFILE_ID.into())
        }
        AuthIdentity::User { id, .. } => Ok(id.clone()),
    }
}

/// Resolve the full profile for "my" endpoints.
fn resolve_my_profile(
    identity: &AuthIdentity,
    ps: &crate::profiles::ProfileStore,
    state: &AppState,
    headers: &HeaderMap,
) -> Result<crate::profiles::UserProfile, StatusCode> {
    let id = resolve_my_profile_id(identity, ps, state, headers)?;
    ps.get(&id)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)
}

/// The fixed profile ID used for token-based admin authentication.
/// This ensures the admin has its own separate profile, distinct from any user profiles.
pub const ADMIN_PROFILE_ID: &str = "admin";
const ADMIN_PLACEHOLDER_EMAIL: &str = "admin@localhost";

fn extract_bearer_token(req: &axum::http::Request<axum::body::Body>) -> Option<String> {
    req.headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(String::from)
}

/// Ensure an admin profile exists in the store, creating one if needed.
fn ensure_admin_profile(ps: &crate::profiles::ProfileStore) -> Result<(), StatusCode> {
    if let Ok(Some(_)) = ps.get(ADMIN_PROFILE_ID) {
        return Ok(());
    }
    let profile = crate::profiles::UserProfile {
        id: ADMIN_PROFILE_ID.into(),
        name: "Admin".into(),
        public_subdomain: None,
        enabled: false,
        data_dir: None,
        parent_id: None,
        config: crate::profiles::ProfileConfig::default(),
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };
    ps.save(&profile).map_err(|e| {
        tracing::error!(error = %e, "failed to auto-create admin profile");
        StatusCode::INTERNAL_SERVER_ERROR
    })
}

fn ensure_admin_user(user_store: &crate::user_store::UserStore) -> Result<User, StatusCode> {
    let mut created = false;
    let mut user = match user_store.get(ADMIN_PROFILE_ID) {
        Ok(Some(current)) => current,
        Ok(None) => {
            created = true;
            User {
                id: ADMIN_PROFILE_ID.into(),
                email: ADMIN_PLACEHOLDER_EMAIL.into(),
                name: "Admin".into(),
                role: UserRole::Admin,
                created_at: chrono::Utc::now(),
                last_login_at: None,
            }
        }
        Err(e) => {
            tracing::error!(error = %e, "failed to load admin user");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };

    let mut changed = false;
    if user.role != UserRole::Admin {
        user.role = UserRole::Admin;
        changed = true;
    }
    if user.name.trim().is_empty() {
        user.name = "Admin".into();
        changed = true;
    }
    if user.email.trim().is_empty() {
        user.email = ADMIN_PLACEHOLDER_EMAIL.into();
        changed = true;
    }

    if created || changed {
        user_store.save(&user).map_err(|e| {
            tracing::error!(error = %e, "failed to persist admin user");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    }

    Ok(user)
}

fn build_portal_state(
    state: &AppState,
    identity: &AuthIdentity,
    user: &User,
) -> Result<PortalState, StatusCode> {
    let ps = state
        .profile_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    let mut all_profiles = ps.list().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    all_profiles.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));

    match identity {
        AuthIdentity::Admin => {
            ensure_admin_profile(ps)?;
            if !all_profiles
                .iter()
                .any(|profile| profile.id == ADMIN_PROFILE_ID)
            {
                if let Ok(Some(profile)) = ps.get(ADMIN_PROFILE_ID) {
                    all_profiles.push(profile);
                }
            }
            all_profiles.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));

            let accessible_profiles = all_profiles
                .into_iter()
                .map(|profile| {
                    let is_self_profile = profile.id == ADMIN_PROFILE_ID;
                    AccessibleProfileSummary {
                        id: profile.id.clone(),
                        name: profile.name,
                        parent_id: profile.parent_id.clone(),
                        relationship: if is_self_profile {
                            ProfileRelationship::SelfProfile
                        } else {
                            ProfileRelationship::AdminManaged
                        },
                        api_scope: if is_self_profile {
                            ProfileApiScope::SelfService
                        } else {
                            ProfileApiScope::Admin
                        },
                        route_base: if is_self_profile {
                            "/my".into()
                        } else {
                            format!("/profile/{}", profile.id)
                        },
                        can_manage_sub_accounts: profile.parent_id.is_none(),
                    }
                })
                .collect();

            Ok(PortalState {
                kind: if is_login_ready_email(&user.email) {
                    PortalKind::Admin
                } else {
                    PortalKind::BootstrapAdmin
                },
                home_profile_id: ADMIN_PROFILE_ID.into(),
                home_route: "/my".into(),
                can_access_admin_portal: true,
                can_manage_users: true,
                sub_account_limit: crate::profiles::MAX_SUB_ACCOUNTS_PER_PARENT,
                accessible_profiles,
            })
        }
        AuthIdentity::User {
            id,
            role: UserRole::Admin,
        } => {
            let accessible_profiles = all_profiles
                .into_iter()
                .map(|profile| {
                    let is_self_profile = profile.id == *id;
                    AccessibleProfileSummary {
                        id: profile.id.clone(),
                        name: profile.name,
                        parent_id: profile.parent_id.clone(),
                        relationship: if is_self_profile {
                            ProfileRelationship::SelfProfile
                        } else {
                            ProfileRelationship::AdminManaged
                        },
                        api_scope: if is_self_profile {
                            ProfileApiScope::SelfService
                        } else {
                            ProfileApiScope::Admin
                        },
                        route_base: if is_self_profile {
                            "/my".into()
                        } else {
                            format!("/profile/{}", profile.id)
                        },
                        can_manage_sub_accounts: profile.parent_id.is_none(),
                    }
                })
                .collect();

            Ok(PortalState {
                kind: PortalKind::Admin,
                home_profile_id: id.clone(),
                home_route: "/my".into(),
                can_access_admin_portal: true,
                can_manage_users: true,
                sub_account_limit: crate::profiles::MAX_SUB_ACCOUNTS_PER_PARENT,
                accessible_profiles,
            })
        }
        AuthIdentity::User {
            id,
            role: UserRole::User,
        } => {
            let own_profile = ps
                .get(id)
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
                .ok_or(StatusCode::NOT_FOUND)?;

            let is_sub_account = own_profile.parent_id.is_some();
            let mut accessible_profiles = vec![AccessibleProfileSummary {
                id: own_profile.id.clone(),
                name: own_profile.name.clone(),
                parent_id: own_profile.parent_id.clone(),
                relationship: ProfileRelationship::SelfProfile,
                api_scope: ProfileApiScope::SelfService,
                route_base: "/my".into(),
                can_manage_sub_accounts: own_profile.parent_id.is_none(),
            }];

            if own_profile.parent_id.is_none() {
                let mut children = ps
                    .list_sub_accounts(id)
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
                children.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
                accessible_profiles.extend(children.into_iter().map(|profile| {
                    AccessibleProfileSummary {
                        id: profile.id.clone(),
                        name: profile.name,
                        parent_id: profile.parent_id.clone(),
                        relationship: ProfileRelationship::ManagedChild,
                        api_scope: ProfileApiScope::SubAccount,
                        route_base: format!("/accounts/{}", profile.id),
                        can_manage_sub_accounts: false,
                    }
                }));
            }

            Ok(PortalState {
                kind: if is_sub_account {
                    PortalKind::SubAccount
                } else {
                    PortalKind::Owner
                },
                home_profile_id: id.clone(),
                home_route: "/my".into(),
                can_access_admin_portal: false,
                can_manage_users: false,
                sub_account_limit: crate::profiles::MAX_SUB_ACCOUNTS_PER_PARENT,
                accessible_profiles,
            })
        }
    }
}

// ---------------------------------------------------------------------------
// WeChat QR Login (user-scoped)
// ---------------------------------------------------------------------------

/// GET /api/my/profile/wechat/qr-start
pub async fn my_wechat_qr_start(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<AuthIdentity>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    // Check if ProcessManager has a bridge with QR info
    if let Some(pm) = state.process_manager.as_ref() {
        let ps = state
            .profile_store
            .as_ref()
            .ok_or((StatusCode::SERVICE_UNAVAILABLE, "no profile store".into()))?;
        let profile_id =
            super::auth_handlers::resolve_my_profile_id(&identity, ps, &state, &headers)
                .map_err(|_| (StatusCode::FORBIDDEN, "cannot resolve profile".into()))?;
        let key = format!("{}-wechat", profile_id);
        if let Some(info) = pm.bridge_qr(&key).await {
            if let Some(ref qr_url) = info.qr {
                return Ok(Json(serde_json::json!({
                    "qrcode_url": qr_url,
                    "session_key": "",
                    "bridge_managed": true
                })));
            }
        }
    }

    // Fallback: direct QR fetch
    let client = reqwest::Client::new();
    let url = "https://ilinkai.weixin.qq.com/ilink/bot/get_bot_qrcode?bot_type=3";
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("failed to fetch QR: {e}")))?;
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("invalid QR response: {e}")))?;
    let qrcode = body["qrcode"]
        .as_str()
        .ok_or((StatusCode::BAD_GATEWAY, "missing qrcode".into()))?;
    let qrcode_url = body["qrcode_img_content"]
        .as_str()
        .ok_or((StatusCode::BAD_GATEWAY, "missing qrcode_img_content".into()))?;

    Ok(Json(serde_json::json!({
        "qrcode_url": qrcode_url,
        "session_key": qrcode
    })))
}

/// POST /api/my/profile/wechat/qr-poll
pub async fn my_wechat_qr_poll(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<AuthIdentity>,
    Json(req): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let ps = state
        .profile_store
        .as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, "no profile store".into()))?;
    let profile_id = super::auth_handlers::resolve_my_profile_id(&identity, ps, &state, &headers)
        .map_err(|_| (StatusCode::FORBIDDEN, "cannot resolve profile".into()))?;

    let session_key = req["session_key"].as_str().unwrap_or_default();
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(40))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    let encoded: String = session_key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' || c == '~' {
                c.to_string()
            } else {
                format!("%{:02X}", c as u32)
            }
        })
        .collect();

    let url = format!(
        "https://ilinkai.weixin.qq.com/ilink/bot/get_qrcode_status?qrcode={}",
        encoded
    );
    let resp = client
        .get(&url)
        .header("iLink-App-ClientVersion", "1")
        .send()
        .await
        .map_err(|e| {
            if e.is_timeout() {
                return (StatusCode::OK, "timeout".into());
            }
            (StatusCode::BAD_GATEWAY, format!("poll failed: {e}"))
        })?;
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("parse error: {e}")))?;

    let status = body["status"].as_str().unwrap_or("wait");

    if status == "confirmed" {
        let bot_token = body["bot_token"].as_str().unwrap_or_default().to_string();
        let bot_id = body["ilink_bot_id"]
            .as_str()
            .unwrap_or_default()
            .to_string();

        if !bot_token.is_empty() {
            if let Ok(Some(mut profile)) = ps.get(&profile_id) {
                let has_wechat = profile
                    .config
                    .channels
                    .iter()
                    .any(|c| matches!(c, crate::profiles::ChannelCredentials::WeChat { .. }));
                if !has_wechat {
                    profile
                        .config
                        .channels
                        .push(crate::profiles::ChannelCredentials::WeChat {
                            token_env: "WECHAT_BOT_TOKEN".into(),
                            base_url: "https://ilinkai.weixin.qq.com".into(),
                        });
                }
                profile
                    .config
                    .env_vars
                    .insert("WECHAT_BOT_TOKEN".into(), bot_token.clone());
                let _ = ps.save(&profile);
                // Set env var so the running wechat channel picks it up on next reconnect
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    let _ = std::fs::OpenOptions::new()
                        .write(true)
                        .create(true)
                        .truncate(true)
                        .mode(0o600)
                        .open("/tmp/octos-wechat-token")
                        .and_then(|mut f| std::io::Write::write_all(&mut f, bot_token.as_bytes()));
                }
                #[cfg(not(unix))]
                {
                    std::fs::write("/tmp/octos-wechat-token", &bot_token).ok();
                }
            }
        }

        // Don't expose bot_token to client — already saved server-side
        return Ok(Json(serde_json::json!({
            "status": status,
            "bot_id": bot_id
        })));
    }

    Ok(Json(serde_json::json!({ "status": status })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn otp_rate_limit_prunes_expired_keys_and_stays_bounded() {
        let now = std::time::Instant::now();
        let mut limits = (0..OTP_RATE_LIMIT_MAX_KEYS)
            .map(|index| (format!("probe-{index}@example.com"), (1, now)))
            .collect::<HashMap<_, _>>();

        assert!(otp_rate_limit_exceeded(
            &mut limits,
            "overflow@example.com".into(),
            now
        ));
        assert_eq!(limits.len(), OTP_RATE_LIMIT_MAX_KEYS);
        assert!(!limits.contains_key("overflow@example.com"));

        limits.insert(
            "probe-0@example.com".into(),
            (
                1,
                now - OTP_RATE_LIMIT_WINDOW - std::time::Duration::from_secs(1),
            ),
        );
        assert!(!otp_rate_limit_exceeded(
            &mut limits,
            "replacement@example.com".into(),
            now
        ));
        assert_eq!(limits.len(), OTP_RATE_LIMIT_MAX_KEYS);
        assert!(limits.contains_key("replacement@example.com"));
        assert!(!limits.contains_key("probe-0@example.com"));
    }

    #[test]
    fn should_require_ominix_only_for_local_voice_legs() {
        use crate::api::voice_turn::TtsRoute;
        use crate::skills_scope::AsrRoute;

        let external = AsrRoute::External("http://127.0.0.1:8093".to_string());
        let ominix = AsrRoute::Ominix(Some("http://127.0.0.1:8081".to_string()));

        assert!(!voice_readiness_needs_ominix(
            false,
            &external,
            TtsRoute::Cloud
        ));
        assert!(voice_readiness_needs_ominix(
            false,
            &external,
            TtsRoute::Local
        ));
        assert!(voice_readiness_needs_ominix(
            false,
            &ominix,
            TtsRoute::Cloud
        ));
        assert!(voice_readiness_needs_ominix(
            false,
            &ominix,
            TtsRoute::Local
        ));
        assert!(!voice_readiness_needs_ominix(
            true,
            &ominix,
            TtsRoute::Cloud
        ));
        assert!(voice_readiness_needs_ominix(true, &ominix, TtsRoute::Local));
    }

    async fn external_asr_health_server(status: &str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let status = status.to_string();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 1024];
            let read = socket.read(&mut request).await.unwrap();
            assert!(
                String::from_utf8_lossy(&request[..read]).starts_with("GET /health "),
                "readiness must probe the external ASR health path"
            );
            let response =
                format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        format!("http://{address}")
    }

    #[tokio::test]
    async fn should_accept_external_asr_when_health_check_succeeds() {
        let url = external_asr_health_server("200 OK").await;
        let leg = external_asr_readiness(&reqwest::Client::new(), &url).await;

        assert!(leg.ready);
        assert_eq!(leg.mode, "external");
        assert_eq!(leg.detail, "External ASR ready");
    }

    #[tokio::test]
    async fn should_accept_external_asr_when_health_endpoint_is_not_implemented() {
        let url = external_asr_health_server("404 Not Found").await;
        let leg = external_asr_readiness(&reqwest::Client::new(), &url).await;

        assert!(leg.ready);
        assert_eq!(leg.mode, "external");
        assert!(leg.detail.contains("health endpoint not provided"));
    }

    #[tokio::test]
    async fn should_reject_external_asr_when_health_endpoint_reports_unavailable() {
        let url = external_asr_health_server("503 Service Unavailable").await;
        let leg = external_asr_readiness(&reqwest::Client::new(), &url).await;

        assert!(!leg.ready);
        assert_eq!(leg.mode, "external");
        assert!(leg.detail.contains("HTTP 503"));
    }

    #[test]
    fn speech_synthesis_rejects_blank_and_oversized_text() {
        assert_eq!(
            validate_synthesis_text("  ").unwrap_err().status,
            StatusCode::BAD_REQUEST
        );
        let oversized = "旁".repeat(MAX_SPEECH_SYNTHESIS_CHARS + 1);
        assert_eq!(
            validate_synthesis_text(&oversized).unwrap_err().status,
            StatusCode::PAYLOAD_TOO_LARGE
        );
    }

    #[test]
    fn speech_synthesis_trims_valid_text_and_maps_audio_mime() {
        assert_eq!(
            validate_synthesis_text("  先看这个圆。 \n").unwrap(),
            "先看这个圆。"
        );
        assert_eq!(
            speech_audio_content_type(std::path::Path::new("speech.mp3")),
            "audio/mpeg"
        );
        assert_eq!(
            speech_audio_content_type(std::path::Path::new("speech.wav")),
            "audio/wav"
        );
        assert_eq!(
            speech_audio_content_type(std::path::Path::new("speech.pcm")),
            "audio/pcm"
        );
        assert_eq!(
            speech_audio_content_type(std::path::Path::new("speech.ogg")),
            "audio/ogg"
        );
    }

    #[tokio::test]
    async fn speech_synthesis_limits_concurrency_per_profile() {
        let profile = format!("speech-limit-{}", uuid::Uuid::now_v7());
        let other_profile = format!("speech-limit-{}", uuid::Uuid::now_v7());

        let first = acquire_speech_synthesis_permit(&profile).expect("first call is admitted");
        assert_eq!(
            acquire_speech_synthesis_permit(&profile)
                .unwrap_err()
                .status,
            StatusCode::TOO_MANY_REQUESTS
        );
        let other = acquire_speech_synthesis_permit(&other_profile).expect("profiles are isolated");

        drop(first);
        let resumed =
            acquire_speech_synthesis_permit(&profile).expect("permit is reusable after completion");
        drop(resumed);
        drop(other);
    }

    #[test]
    fn speech_synthesis_quota_bounds_requests_and_characters_per_window() {
        let started_at = std::time::Instant::now();
        let mut request_quota = SpeechSynthesisQuota::new(started_at);
        for _ in 0..MAX_SPEECH_SYNTHESIS_REQUESTS_PER_WINDOW {
            request_quota.consume(1, started_at).unwrap();
        }
        let request_error = request_quota.consume(1, started_at).unwrap_err();
        assert_eq!(request_error.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(request_error.retry_after_seconds, Some(60));

        let mut character_quota = SpeechSynthesisQuota::new(started_at);
        character_quota
            .consume(MAX_SPEECH_SYNTHESIS_CHARS_PER_WINDOW, started_at)
            .unwrap();
        assert_eq!(
            character_quota.consume(1, started_at).unwrap_err().status,
            StatusCode::TOO_MANY_REQUESTS
        );

        request_quota
            .consume(1, started_at + SPEECH_SYNTHESIS_QUOTA_WINDOW)
            .expect("a new window restores the profile budget");
    }

    #[test]
    fn speech_synthesis_rate_limit_response_sets_retry_after() {
        let response = SpeechSynthesisError::rate_limited("speech synthesis quota exceeded", 17)
            .into_response();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers().get(header::RETRY_AFTER).unwrap(), "17");
    }
    use crate::login_allowlist::{AllowedLogin, LoginAllowlistStore};
    use crate::otp::AuthManager;
    use crate::profiles::ProfileStore;
    use crate::user_store::UserStore;
    use axum::http::HeaderMap;
    use axum::http::Request;

    // --- voice_available_in_registry (voice readiness, local TTS leg) ---

    /// Registry with: a shared preset `doubao` (ref exists, alias `vivian`),
    /// a shared preset `ghost` whose ref audio is missing, and a per-profile
    /// clone owned by tenant `other` (exists on disk, invisible to others).
    fn readiness_registry(dir: &std::path::Path) -> octos_llm::ominix::VoicesRegistry {
        std::fs::create_dir_all(dir.join("ref_audios")).unwrap();
        std::fs::write(dir.join("ref_audios/doubao_ref.wav"), b"fake").unwrap();
        let clone_dir = dir.join("profiles/other/data/voice_profiles");
        std::fs::create_dir_all(&clone_dir).unwrap();
        let clone_path = clone_dir.join("mine.wav");
        std::fs::write(&clone_path, b"fake").unwrap();
        let json = format!(
            r#"{{
              "default_voice": "doubao",
              "models_base_path": "{base}",
              "voices": {{
                "doubao": {{ "ref_audio": "ref_audios/doubao_ref.wav", "ref_text": "x", "aliases": ["vivian"] }},
                "ghost":  {{ "ref_audio": "ref_audios/ghost_ref.wav", "ref_text": "y", "aliases": [] }},
                "other-clone": {{ "ref_audio": "{clone}", "ref_text": "z", "aliases": [] }}
              }}
            }}"#,
            base = dir.to_string_lossy(),
            clone = clone_path.to_string_lossy()
        );
        octos_llm::ominix::VoicesRegistry::parse(&json).unwrap()
    }

    #[test]
    fn selected_voice_must_itself_be_synthesizable_not_just_any_voice() {
        let dir = tempfile::tempdir().unwrap();
        let reg = readiness_registry(dir.path());
        // The selected voice's ref audio is missing: NOT ready, even though
        // another synthesizable voice (doubao) exists — the false-positive
        // the readiness probe used to report.
        assert!(!voice_available_in_registry(&reg, "me", "ghost"));
        // The selected voice resolves (by id or alias) → ready.
        assert!(voice_available_in_registry(&reg, "me", "doubao"));
        assert!(voice_available_in_registry(&reg, "me", "vivian"));
        // Unknown selection → not ready.
        assert!(!voice_available_in_registry(&reg, "me", "nope"));
    }

    #[test]
    fn selected_voice_owned_by_another_tenant_is_not_ready() {
        let dir = tempfile::tempdir().unwrap();
        let reg = readiness_registry(dir.path());
        // Exists on disk but is another profile's clone: invisible → not ready.
        assert!(!voice_available_in_registry(&reg, "me", "other-clone"));
        // The owning tenant itself CAN use it.
        assert!(voice_available_in_registry(&reg, "other", "other-clone"));
    }

    #[test]
    fn empty_selection_falls_back_to_any_visible_synthesizable_voice() {
        let dir = tempfile::tempdir().unwrap();
        let reg = readiness_registry(dir.path());
        // No selection resolves anywhere → any visible synthesizable voice is
        // enough (mirrors the engine default an empty-voice turn gets).
        assert!(voice_available_in_registry(&reg, "me", ""));
        // ...but an empty/ref-less registry is still not ready.
        let empty = octos_llm::ominix::VoicesRegistry::parse(
            r#"{ "default_voice": "", "models_base_path": "", "voices": {} }"#,
        )
        .unwrap();
        assert!(!voice_available_in_registry(&empty, "me", ""));
    }

    // --- payload_from_user_profile (profile QR export) ---

    fn qr_profile_fixture() -> crate::profiles::UserProfile {
        let mut profile = crate::profiles::UserProfile {
            id: "ada".into(),
            name: "Ada".into(),
            public_subdomain: None,
            enabled: false,
            data_dir: None,
            parent_id: None,
            config: crate::profiles::ProfileConfig::default(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        profile.config.llm = Some(crate::profiles::LlmProfileConfig {
            primary: Some(crate::profiles::LlmModelSelectionConfig {
                family_id: Some("deepseek".into()),
                model_id: Some("deepseek-v4-pro".into()),
                route: Some(crate::profiles::LlmRouteConfig {
                    route_id: None,
                    label: None,
                    base_url: None,
                    api_key_env: Some("DEEPSEEK_API_KEY".into()),
                    api_type: None,
                }),
                model_hints: None,
                cost_per_m: None,
                strong: None,
                // #2166 typed inference defaults (unset here).
                temperature: None,
                top_p: None,
                reasoning_effort: None,
                context_window: None,
            }),
            fallbacks: vec![],
        });
        profile
            .config
            .env_vars
            .insert("DEEPSEEK_API_KEY".into(), "sk-profile-key".into());
        profile.config.env_vars.insert(
            "TELEGRAM_BOT_TOKEN".into(),
            "bot-token-must-not-leak".into(),
        );
        profile
    }

    #[test]
    fn qr_payload_without_secrets_masks_nothing_because_nothing_is_included() {
        let profile = qr_profile_fixture();
        let payload = payload_from_user_profile(&profile, false).unwrap();
        assert_eq!(payload.id, "ada");
        assert_eq!(payload.name.as_deref(), Some("Ada"));
        assert!(payload.llm.is_some());
        assert!(payload.secrets.is_empty());
        assert!(!payload.has_secrets());
    }

    #[test]
    fn qr_payload_with_secrets_includes_only_llm_route_referenced_vars() {
        let profile = qr_profile_fixture();
        let payload = payload_from_user_profile(&profile, true).unwrap();
        assert_eq!(payload.secrets["DEEPSEEK_API_KEY"], "sk-profile-key");
        assert!(
            !payload.secrets.contains_key("TELEGRAM_BOT_TOKEN"),
            "env vars not referenced by an LLM route must not ride along"
        );
    }

    #[test]
    fn qr_export_refuses_foreign_and_bare_keychain_markers() {
        // plain values export
        assert!(marker_allowed_for_export(
            "sk-plain",
            "DEEPSEEK_API_KEY",
            "ada"
        ));
        // own scoped marker exports
        assert!(marker_allowed_for_export(
            "keychain:DEEPSEEK_API_KEY::ada",
            "DEEPSEEK_API_KEY",
            "ada"
        ));
        // ANOTHER tenant's scoped account: refuse (exfiltration vector)
        assert!(!marker_allowed_for_export(
            "keychain:VERTEX_SA_JSON::admin",
            "VERTEX_SA_JSON",
            "ada"
        ));
        // bare marker addresses a host-level item: refuse
        assert!(!marker_allowed_for_export(
            "keychain:DEEPSEEK_API_KEY",
            "DEEPSEEK_API_KEY",
            "ada"
        ));
        // marker under a DIFFERENT var name than referenced: refuse
        assert!(!marker_allowed_for_export(
            "keychain:OTHER_VAR::ada",
            "DEEPSEEK_API_KEY",
            "ada"
        ));
    }

    #[test]
    fn qr_endpoint_keeps_port_and_honors_forwarded_proto() {
        let mut headers = HeaderMap::new();
        headers.insert("host", "localhost:50080".parse().unwrap());
        assert_eq!(
            request_endpoint(&headers).as_deref(),
            Some("http://localhost:50080"),
            "authority (incl. port) must survive; loopback defaults to http"
        );

        let mut headers = HeaderMap::new();
        headers.insert("host", "ada.crew.example.com".parse().unwrap());
        assert_eq!(
            request_endpoint(&headers).as_deref(),
            Some("https://ada.crew.example.com")
        );

        let mut headers = HeaderMap::new();
        headers.insert("host", "[::1]:50080".parse().unwrap());
        assert_eq!(
            request_endpoint(&headers).as_deref(),
            Some("http://[::1]:50080"),
            "bracketed IPv6 loopback is local, not https"
        );

        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-host",
            "ada.crew.example.com:8443".parse().unwrap(),
        );
        headers.insert("x-forwarded-proto", "http".parse().unwrap());
        assert_eq!(
            request_endpoint(&headers).as_deref(),
            Some("http://ada.crew.example.com:8443"),
            "reverse-proxy proto must win over the https default"
        );
    }

    #[test]
    fn qr_payload_secrets_round_trip_pin_wrapped() {
        let profile = qr_profile_fixture();
        let payload = payload_from_user_profile(&profile, true).unwrap();
        let encoded = crate::profile_qr::encode_encrypted(&payload, "246810").unwrap();
        let decoded = crate::profile_qr::decode(&encoded, Some("246810")).unwrap();
        assert_eq!(decoded, payload);
        assert!(crate::profile_qr::decode(&encoded, Some("000000")).is_err());
    }

    // --- relocate_secret_to_keychain ---

    use std::cell::RefCell;

    fn env_with(key: &str, val: &str) -> HashMap<String, String> {
        let mut m = HashMap::new();
        m.insert(key.to_string(), val.to_string());
        m
    }

    #[test]
    fn relocates_raw_json_to_keychain_on_macos() {
        let mut env = env_with("VERTEX_SA_JSON", r#"{"private_key":"x","project_id":"p"}"#);
        let stored: RefCell<Vec<(String, String)>> = RefCell::new(vec![]);
        let res = relocate_secret_to_keychain(&mut env, "VERTEX_SA_JSON", "alice", true, |n, s| {
            stored.borrow_mut().push((n.to_string(), s.to_string()));
            Ok(())
        });
        assert!(res.is_ok());
        // value replaced with a profile-scoped marker; raw JSON went to the
        // keychain under a profile-scoped account.
        assert_eq!(
            env.get("VERTEX_SA_JSON").unwrap(),
            "keychain:VERTEX_SA_JSON::alice"
        );
        assert_eq!(stored.borrow().len(), 1);
        assert_eq!(stored.borrow()[0].0, "VERTEX_SA_JSON::alice");
        assert!(stored.borrow()[0].1.contains("private_key"));
    }

    #[test]
    fn two_profiles_get_distinct_keychain_accounts() {
        // The core of the fix: two profiles saving a Vertex SA under the same
        // env var must land in DISTINCT keychain accounts (and persist distinct
        // markers), so neither overwrites nor resolves the other's private key.
        let json = r#"{"private_key":"x"}"#;
        let stored: RefCell<Vec<(String, String)>> = RefCell::new(vec![]);
        let mut alice = env_with("VERTEX_SA_JSON", json);
        let mut bob = env_with("VERTEX_SA_JSON", json);
        relocate_secret_to_keychain(&mut alice, "VERTEX_SA_JSON", "alice", true, |n, s| {
            stored.borrow_mut().push((n.to_string(), s.to_string()));
            Ok(())
        })
        .unwrap();
        relocate_secret_to_keychain(&mut bob, "VERTEX_SA_JSON", "bob", true, |n, s| {
            stored.borrow_mut().push((n.to_string(), s.to_string()));
            Ok(())
        })
        .unwrap();

        assert_eq!(stored.borrow()[0].0, "VERTEX_SA_JSON::alice");
        assert_eq!(stored.borrow()[1].0, "VERTEX_SA_JSON::bob");
        assert_eq!(
            alice.get("VERTEX_SA_JSON").unwrap(),
            "keychain:VERTEX_SA_JSON::alice"
        );
        assert_eq!(
            bob.get("VERTEX_SA_JSON").unwrap(),
            "keychain:VERTEX_SA_JSON::bob"
        );
        assert_ne!(alice.get("VERTEX_SA_JSON"), bob.get("VERTEX_SA_JSON"));
    }

    #[test]
    fn rejects_raw_json_on_non_macos() {
        let mut env = env_with("VERTEX_SA_JSON", r#"{"private_key":"x"}"#);
        let called = RefCell::new(false);
        let res =
            relocate_secret_to_keychain(&mut env, "VERTEX_SA_JSON", "alice", false, |_, _| {
                *called.borrow_mut() = true;
                Ok(())
            });
        assert!(res.is_err());
        assert!(!*called.borrow(), "must not write keychain off macOS");
        // value left untouched (not persisted plaintext silently).
        assert!(env.get("VERTEX_SA_JSON").unwrap().starts_with('{'));
    }

    #[test]
    fn leaves_keychain_marker_untouched() {
        let mut env = env_with("VERTEX_SA_JSON", crate::auth::KEYCHAIN_MARKER);
        let called = RefCell::new(false);
        let res = relocate_secret_to_keychain(&mut env, "VERTEX_SA_JSON", "alice", true, |_, _| {
            *called.borrow_mut() = true;
            Ok(())
        });
        assert!(res.is_ok());
        assert!(
            !*called.borrow(),
            "marker means unchanged — no keychain write"
        );
        assert_eq!(
            env.get("VERTEX_SA_JSON").unwrap(),
            crate::auth::KEYCHAIN_MARKER
        );
    }

    #[test]
    fn is_noop_when_key_absent_or_masked() {
        // absent key
        let mut empty = HashMap::new();
        assert!(
            relocate_secret_to_keychain(&mut empty, "VERTEX_SA_JSON", "alice", true, |_, _| Ok(()))
                .is_ok()
        );
        // masked / non-JSON value is treated as "unchanged"
        let mut masked = env_with("VERTEX_SA_JSON", "abcd***xyz");
        let called = RefCell::new(false);
        let res =
            relocate_secret_to_keychain(&mut masked, "VERTEX_SA_JSON", "alice", true, |_, _| {
                *called.borrow_mut() = true;
                Ok(())
            });
        assert!(res.is_ok());
        assert!(!*called.borrow());
        assert_eq!(masked.get("VERTEX_SA_JSON").unwrap(), "abcd***xyz");
    }

    fn temp_profile_store() -> (tempfile::TempDir, ProfileStore) {
        let dir = tempfile::tempdir().unwrap();
        let ps = ProfileStore::open_unified(dir.path()).unwrap();
        (dir, ps)
    }

    fn make_user_profile(id: &str, name: &str) -> crate::profiles::UserProfile {
        crate::profiles::UserProfile {
            id: id.into(),
            name: name.into(),
            enabled: true,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: crate::profiles::ProfileConfig::default(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    fn temp_app_state() -> (
        tempfile::TempDir,
        AppState,
        Arc<UserStore>,
        Arc<ProfileStore>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let user_store = Arc::new(UserStore::open(dir.path()).unwrap());
        let profile_store = Arc::new(ProfileStore::open_unified(dir.path()).unwrap());
        let allowlist_store = Arc::new(LoginAllowlistStore::open(dir.path()).unwrap());
        // Tests that exercise per-email send_code/verify branches expect the
        // SMTP precheck to pass; populate a synthetic config so the common
        // fixture stays past the early "SMTP not configured" return. Tests
        // that specifically check the no-SMTP behavior can clear this with
        // set_smtp_config(None).
        let auth_manager = Arc::new(AuthManager::new(
            Some(crate::otp::DashboardAuthConfig {
                smtp: Some(crate::otp::SmtpConfig {
                    host: "smtp.test.invalid".into(),
                    port: 465,
                    username: "test@test.invalid".into(),
                    password_env: "SMTP_PASSWORD".into(),
                    from_address: "test@test.invalid".into(),
                }),
                session_expiry_hours: 24,
                allow_self_registration: false,
                static_tokens: Vec::new(),
            }),
            user_store.clone(),
        ));
        let state = AppState {
            auth_token: Some("bootstrap-token".into()),
            solo_login_enabled: true,
            admin_token_store: Arc::new(crate::admin_token_store::AdminTokenStore::new(dir.path())),
            setup_state_store: Arc::new(crate::setup_state_store::SetupStateStore::new(dir.path())),
            metrics_handle: None,
            profile_store: Some(profile_store.clone()),
            user_store: Some(user_store.clone()),
            allowlist_store: Some(allowlist_store),
            auth_manager: Some(auth_manager),
            ..AppState::empty_for_tests()
        };
        (dir, state, user_store, profile_store)
    }

    #[tokio::test]
    async fn auth_status_advertises_local_solo_enabled_in_local_mode() {
        // Local deployment + profile/user stores ⇒ the no-password solo
        // login path is available, so /api/auth/status must advertise it.
        let (_dir, state, _user_store, _profile_store) = temp_app_state();
        assert_eq!(state.deployment_mode, crate::config::DeploymentMode::Local);
        let Json(status) = auth_status(State(Arc::new(state)), HeaderMap::new())
            .await
            .unwrap();
        assert!(status.local_solo_enabled);
    }

    #[tokio::test]
    async fn auth_status_hides_local_solo_in_tenant_mode() {
        // Tenant/cloud hosts are multi-tenant; the solo path must never be
        // advertised there (defense-in-depth alongside the request-time
        // loopback gate enforced by the handlers themselves).
        let (_dir, state, _user_store, _profile_store) = temp_app_state();
        let state = AppState {
            deployment_mode: crate::config::DeploymentMode::Tenant,
            ..state
        };
        let Json(status) = auth_status(State(Arc::new(state)), HeaderMap::new())
            .await
            .unwrap();
        assert!(!status.local_solo_enabled);
    }

    #[tokio::test]
    async fn auth_status_reports_solo_profile_exists_lifecycle() {
        // The SPA's first-run flow keys off this flag: `Some(false)` means
        // "solo is available but nobody has onboarded yet — show the create
        // form directly instead of a doomed solo-login round trip".
        let (_dir, state, _user_store, _profile_store) = temp_app_state();
        let state = Arc::new(state);

        let Json(status) = auth_status(State(state.clone()), HeaderMap::new())
            .await
            .unwrap();
        assert!(status.local_solo_enabled);
        assert_eq!(status.solo_profile_exists, Some(false));

        let _ = crate::api::solo_auth::solo_create(
            State(state.clone()),
            axum::extract::ConnectInfo(std::net::SocketAddr::from(([127, 0, 0, 1], 40000))),
            HeaderMap::new(),
            Json(octos_core::ui_protocol::ProfileLocalCreateParams {
                requested_id: None,
                name: "Ada".into(),
                username: "ada".into(),
                email: "ada@example.com".into(),
                make_default: None,
            }),
        )
        .await
        .unwrap();

        let Json(status) = auth_status(State(state), HeaderMap::new()).await.unwrap();
        assert_eq!(status.solo_profile_exists, Some(true));
    }

    #[tokio::test]
    async fn auth_status_omits_solo_profile_exists_when_solo_not_advertised() {
        // Tenant hosts never advertise solo, so the flag must be absent
        // (the SPA treats "not offered" and "offered but empty" differently).
        let (_dir, state, _user_store, _profile_store) = temp_app_state();
        let state = AppState {
            deployment_mode: crate::config::DeploymentMode::Tenant,
            ..state
        };
        let Json(status) = auth_status(State(Arc::new(state)), HeaderMap::new())
            .await
            .unwrap();
        assert!(!status.local_solo_enabled);
        assert_eq!(status.solo_profile_exists, None);
    }

    fn scoped_host_headers(host: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("Host", host.parse().unwrap());
        headers
    }

    fn localhost_scoped_headers(profile_id: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("Host", "localhost:3000".parse().unwrap());
        headers.insert("X-Profile-Id", profile_id.parse().unwrap());
        headers
    }

    #[test]
    fn matrix_percent_encode_path_encodes_room_ids() {
        assert_eq!(
            matrix_percent_encode_path("!room:example.org"),
            "%21room%3Aexample.org"
        );
    }

    #[test]
    fn matrix_sync_invite_details_include_room_and_inviter() {
        let invite_rooms = json!({
            "!ops:example.org": {
                "invite_state": {
                    "events": [
                        {
                            "type": "m.room.name",
                            "content": { "name": "Ops Room" }
                        },
                        {
                            "type": "m.room.canonical_alias",
                            "content": { "alias": "#ops:example.org" }
                        },
                        {
                            "type": "m.room.member",
                            "sender": "@alice:example.org",
                            "state_key": "@octos:example.org",
                            "event_id": "$invite1",
                            "content": { "membership": "invite" }
                        }
                    ]
                }
            }
        });
        let invite_rooms = invite_rooms.as_object().unwrap();

        let details = matrix_sync_invite_details(invite_rooms, "@octos:example.org");

        assert_eq!(details.len(), 1);
        assert_eq!(details[0].room_id, "!ops:example.org");
        assert_eq!(details[0].room_name.as_deref(), Some("Ops Room"));
        assert_eq!(
            details[0].canonical_alias.as_deref(),
            Some("#ops:example.org")
        );
        assert_eq!(details[0].inviter.as_deref(), Some("@alice:example.org"));
        assert_eq!(details[0].membership_event_id.as_deref(), Some("$invite1"));
    }

    #[test]
    fn matrix_error_body_sanitizes_and_marks_truncation() {
        let summary = sanitize_matrix_error_body("line1\nline2\t\u{0}secret", true);

        assert_eq!(summary, "line1 line2 secret ... [truncated]");
        assert!(!summary.contains('\n'));
        assert!(!summary.contains('\t'));
    }

    #[tokio::test]
    async fn matrix_http_client_does_not_follow_redirects() {
        use axum::Router;
        use axum::response::Redirect;
        use axum::routing::get;

        let app = Router::new()
            .route(
                "/redirect",
                get(|| async { Redirect::temporary("/target") }),
            )
            .route("/target", get(|| async { "target" }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let resp = matrix_http_client()
            .get(format!("http://{addr}/redirect"))
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), reqwest::StatusCode::TEMPORARY_REDIRECT);
    }

    #[test]
    fn matrix_accept_adds_room_to_allowlist_once() {
        let mut profile = make_user_profile("matrix-user", "Matrix User");
        profile.config.channels.push(ChannelCredentials::Matrix {
            homeserver: "https://matrix.example.org".into(),
            as_token: String::new(),
            hs_token: String::new(),
            server_name: String::new(),
            sender_localpart: "octos".into(),
            user_prefix: "octos_".into(),
            port: 8009,
            allowed_senders: Vec::new(),
            mention_only: true,
            mode: "user".into(),
            user_id: "@bot:example.org".into(),
            access_token: "syt_token".into(),
            password: String::new(),
            device_name: "octos".into(),
            rooms: Vec::new(),
            auto_join: "off".into(),
            auto_join_allowlist: Vec::new(),
            group_policy: "allowlist".into(),
            require_mention: true,
        });

        assert!(add_room_to_matrix_allowlist(&mut profile, 0, "!room:example.org").unwrap());
        assert!(!add_room_to_matrix_allowlist(&mut profile, 0, "!room:example.org").unwrap());
        let ChannelCredentials::Matrix { rooms, .. } = &profile.config.channels[0] else {
            panic!("expected matrix channel");
        };
        assert_eq!(rooms, &vec!["!room:example.org".to_string()]);
    }

    #[test]
    fn matrix_test_config_overlays_draft_values() {
        let mut profile = make_user_profile("matrix-user", "Matrix User");
        profile.config.channels.push(ChannelCredentials::Matrix {
            homeserver: "https://old.example.org".into(),
            as_token: String::new(),
            hs_token: String::new(),
            server_name: String::new(),
            sender_localpart: "octos".into(),
            user_prefix: "octos_".into(),
            port: 8009,
            allowed_senders: Vec::new(),
            mention_only: true,
            mode: "user".into(),
            user_id: "@old:example.org".into(),
            access_token: "old_token".into(),
            password: String::new(),
            device_name: "old-device".into(),
            rooms: Vec::new(),
            auto_join: "off".into(),
            auto_join_allowlist: Vec::new(),
            group_policy: "allowlist".into(),
            require_mention: true,
        });

        let request = MatrixTestConnectionRequest {
            channel_index: Some(0),
            channel: Some(MatrixTestChannelDraft {
                mode: Some("user".into()),
                homeserver: Some("https://new.example.org/".into()),
                user_id: Some("@new:example.org".into()),
                access_token: Some("new_token".into()),
                password: None,
                device_name: None,
            }),
        };

        let config = matrix_test_channel_config(&profile, &request).unwrap();
        assert_eq!(config.homeserver, "https://new.example.org");
        assert_eq!(config.user_id.as_deref(), Some("@new:example.org"));
        assert_eq!(config.access_token.as_deref(), Some("new_token"));
        assert_eq!(config.device_name.as_deref(), Some("old-device"));
    }

    #[test]
    fn matrix_test_config_ignores_masked_draft_secrets() {
        let mut profile = make_user_profile("matrix-user", "Matrix User");
        profile.config.channels.push(ChannelCredentials::Matrix {
            homeserver: "https://matrix.example.org".into(),
            as_token: String::new(),
            hs_token: String::new(),
            server_name: String::new(),
            sender_localpart: "octos".into(),
            user_prefix: "octos_".into(),
            port: 8009,
            allowed_senders: Vec::new(),
            mention_only: true,
            mode: "user".into(),
            user_id: "@bot:example.org".into(),
            access_token: "syt_real_access_token".into(),
            password: "real-password".into(),
            device_name: "old-device".into(),
            rooms: Vec::new(),
            auto_join: "off".into(),
            auto_join_allowlist: Vec::new(),
            group_policy: "allowlist".into(),
            require_mention: true,
        });

        let request = MatrixTestConnectionRequest {
            channel_index: Some(0),
            channel: Some(MatrixTestChannelDraft {
                mode: Some("user".into()),
                homeserver: None,
                user_id: None,
                access_token: Some("syt_***ken".into()),
                password: Some("***".into()),
                device_name: Some("new-device".into()),
            }),
        };

        let config = matrix_test_channel_config(&profile, &request).unwrap();
        assert_eq!(
            config.access_token.as_deref(),
            Some("syt_real_access_token")
        );
        assert_eq!(config.password.as_deref(), Some("real-password"));
        assert_eq!(config.device_name.as_deref(), Some("new-device"));
    }

    #[test]
    fn matrix_test_config_empty_access_token_clears_saved_token() {
        let mut profile = make_user_profile("matrix-user", "Matrix User");
        profile.config.channels.push(ChannelCredentials::Matrix {
            homeserver: "https://matrix.example.org".into(),
            as_token: String::new(),
            hs_token: String::new(),
            server_name: String::new(),
            sender_localpart: "octos".into(),
            user_prefix: "octos_".into(),
            port: 8009,
            allowed_senders: Vec::new(),
            mention_only: true,
            mode: "user".into(),
            user_id: "@bot:example.org".into(),
            access_token: "syt_real_access_token".into(),
            password: String::new(),
            device_name: "old-device".into(),
            rooms: Vec::new(),
            auto_join: "off".into(),
            auto_join_allowlist: Vec::new(),
            group_policy: "allowlist".into(),
            require_mention: true,
        });

        let request = MatrixTestConnectionRequest {
            channel_index: Some(0),
            channel: Some(MatrixTestChannelDraft {
                mode: Some("user".into()),
                homeserver: None,
                user_id: None,
                access_token: Some(String::new()),
                password: Some("new-password".into()),
                device_name: None,
            }),
        };

        let config = matrix_test_channel_config(&profile, &request).unwrap();
        assert_eq!(config.access_token, None);
        assert_eq!(config.password.as_deref(), Some("new-password"));
    }

    #[test]
    fn matrix_test_config_accepts_password_login_localpart() {
        let profile = make_user_profile("matrix-user", "Matrix User");
        let request = MatrixTestConnectionRequest {
            channel_index: None,
            channel: Some(MatrixTestChannelDraft {
                mode: Some("user".into()),
                homeserver: Some("https://matrix.example.org".into()),
                user_id: Some("octos".into()),
                access_token: None,
                password: Some("secret".into()),
                device_name: None,
            }),
        };

        let config = matrix_test_channel_config(&profile, &request).unwrap();
        assert_eq!(config.user_id.as_deref(), Some("octos"));
    }

    #[test]
    fn matrix_test_config_accepts_password_login_full_user_id() {
        let profile = make_user_profile("matrix-user", "Matrix User");
        let request = MatrixTestConnectionRequest {
            channel_index: None,
            channel: Some(MatrixTestChannelDraft {
                mode: Some("user".into()),
                homeserver: Some("https://matrix.example.org".into()),
                user_id: Some("@octos:octos.meldry.com".into()),
                access_token: None,
                password: Some("secret".into()),
                device_name: None,
            }),
        };

        let config = matrix_test_channel_config(&profile, &request).unwrap();
        assert_eq!(config.user_id.as_deref(), Some("@octos:octos.meldry.com"));
    }

    #[test]
    fn matrix_test_config_rejects_password_login_bare_localpart_with_server() {
        let profile = make_user_profile("matrix-user", "Matrix User");
        let request = MatrixTestConnectionRequest {
            channel_index: None,
            channel: Some(MatrixTestChannelDraft {
                mode: Some("user".into()),
                homeserver: Some("https://matrix.example.org".into()),
                user_id: Some("bot:example.org".into()),
                access_token: None,
                password: Some("secret".into()),
                device_name: None,
            }),
        };

        let err = matrix_test_channel_config(&profile, &request).unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(err.1.contains("localpart like octos"));
        assert!(
            err.1
                .contains("do not use octos:octos.meldry.com without @")
        );
    }

    #[test]
    fn matrix_login_error_explains_oidc_password_login_rejection() {
        let message = matrix_login_error(
            reqwest::StatusCode::FORBIDDEN,
            r#"{"errcode":"M_FORBIDDEN","error":"This server uses delegated authentication. Use the OIDC provider to log in."}"#,
        );

        assert!(message.contains("password login is not available"));
        assert!(message.contains("Access token"));
        assert!(!message.contains("M_FORBIDDEN"));
    }

    #[test]
    fn should_return_admin_id_when_admin_identity() {
        let (_dir, state, _user_store, profile_store) = temp_app_state();
        // Create a user profile that would have been returned by the old "first" logic
        profile_store
            .save(&make_user_profile("guofoo", "Guo Foo"))
            .unwrap();

        let identity = AuthIdentity::Admin;
        let result =
            resolve_my_profile_id(&identity, &profile_store, &state, &HeaderMap::new()).unwrap();
        assert_eq!(
            result, ADMIN_PROFILE_ID,
            "admin should get its own profile ID, not the first user's"
        );
    }

    #[test]
    fn should_return_user_id_when_user_identity() {
        let (_dir, state, _user_store, profile_store) = temp_app_state();
        profile_store
            .save(&make_user_profile("user123", "Test User"))
            .unwrap();

        let identity = AuthIdentity::User {
            id: "user123".into(),
            role: UserRole::User,
        };
        let result =
            resolve_my_profile_id(&identity, &profile_store, &state, &HeaderMap::new()).unwrap();
        assert_eq!(result, "user123");
    }

    #[test]
    fn should_auto_create_admin_profile_when_not_exists() {
        let (_dir, ps) = temp_profile_store();
        assert!(ps.get(ADMIN_PROFILE_ID).unwrap().is_none());

        ensure_admin_profile(&ps).unwrap();

        let profile = ps
            .get(ADMIN_PROFILE_ID)
            .unwrap()
            .expect("admin profile should exist");
        assert_eq!(profile.id, ADMIN_PROFILE_ID);
        assert_eq!(profile.name, "Admin");
    }

    #[test]
    fn should_not_overwrite_existing_admin_profile() {
        let (_dir, ps) = temp_profile_store();
        let mut admin = make_user_profile(ADMIN_PROFILE_ID, "Custom Admin Name");
        admin.enabled = true;
        ps.save(&admin).unwrap();

        ensure_admin_profile(&ps).unwrap();

        let profile = ps.get(ADMIN_PROFILE_ID).unwrap().unwrap();
        assert_eq!(
            profile.name, "Custom Admin Name",
            "should not overwrite existing profile"
        );
        assert!(profile.enabled);
    }

    #[test]
    fn should_resolve_admin_profile_not_first_user() {
        let (_dir, state, _user_store, profile_store) = temp_app_state();
        // Create user profile first — old code would return this
        profile_store
            .save(&make_user_profile("alice", "Alice"))
            .unwrap();
        // Ensure admin profile exists
        ensure_admin_profile(&profile_store).unwrap();

        let identity = AuthIdentity::Admin;
        let profile =
            resolve_my_profile(&identity, &profile_store, &state, &HeaderMap::new()).unwrap();
        assert_eq!(profile.id, ADMIN_PROFILE_ID);
        assert_eq!(profile.name, "Admin");
    }

    // ── Option Y host-authoritative scoping (issue #315) ──────────────

    #[test]
    fn host_scope_admin_falls_through_when_host_unknown() {
        // Admin viewing /api/my/* via an unmapped host (e.g. direct IP or
        // root domain) MUST still get the admin profile back. Without this
        // admin loses access to their own profile entirely.
        let (_dir, state, _user_store, profile_store) = temp_app_state();
        // No profiles besides what `ensure_admin_profile` will auto-create.
        let identity = AuthIdentity::Admin;
        // "localhost" is the canonical "no tenant subdomain" host.
        let result = resolve_my_profile_id(
            &identity,
            &profile_store,
            &state,
            &scoped_host_headers("localhost"),
        )
        .unwrap();
        assert_eq!(
            result, ADMIN_PROFILE_ID,
            "admin must keep its own profile when no tenant subdomain is in scope"
        );
    }

    #[test]
    fn host_scope_admin_resolves_to_tenant_when_host_matches() {
        // Admin visiting a tenant subdomain MUST be re-scoped to that
        // tenant's profile. Closes the original #315 bug where admin
        // unconditionally saw the global admin profile from any host.
        let (_dir, state, _user_store, profile_store) = temp_app_state();
        profile_store
            .save(&make_user_profile("tenant", "Tenant Owner"))
            .unwrap();

        let identity = AuthIdentity::Admin;
        let result = resolve_my_profile_id(
            &identity,
            &profile_store,
            &state,
            &scoped_host_headers("tenant.example.test"),
        )
        .unwrap();
        assert_eq!(result, "tenant");
    }

    #[test]
    fn host_scope_sub_account_on_own_tenant_subdomain() {
        // A user (logged in as their tenant profile via scoped OTP) visits
        // their own tenant subdomain. Host check should resolve to that
        // tenant id — which IS the user's id, so authorization passes.
        let (_dir, state, _user_store, profile_store) = temp_app_state();
        let mut tenant = make_user_profile("dspfac", "DSPFac");
        tenant.public_subdomain = Some("dspfac".into());
        profile_store.save(&tenant).unwrap();

        let identity = AuthIdentity::User {
            id: "dspfac".into(),
            role: UserRole::User,
        };
        let result = resolve_my_profile_id(
            &identity,
            &profile_store,
            &state,
            &scoped_host_headers("dspfac.example.test"),
        )
        .unwrap();
        assert_eq!(result, "dspfac");
    }

    #[test]
    fn host_scope_cross_tenant_user_access_denied() {
        // A user logged in under tenant A visits tenant B's subdomain.
        // Server MUST refuse with 403 rather than silently falling
        // through to tenant A's profile (which would be confusing) or,
        // worse, granting access to tenant B's data.
        let (_dir, state, _user_store, profile_store) = temp_app_state();
        profile_store
            .save(&make_user_profile("tenant-a", "Tenant A"))
            .unwrap();
        let mut tenant_b = make_user_profile("tenant-b", "Tenant B");
        tenant_b.public_subdomain = Some("tenantb".into());
        profile_store.save(&tenant_b).unwrap();

        let identity = AuthIdentity::User {
            id: "tenant-a".into(),
            role: UserRole::User,
        };
        let err = resolve_my_profile_id(
            &identity,
            &profile_store,
            &state,
            &scoped_host_headers("tenantb.example.test"),
        )
        .expect_err("cross-tenant access must be rejected");
        assert_eq!(err, StatusCode::FORBIDDEN);
    }

    #[test]
    fn host_scope_admin_can_access_any_tenant() {
        // The cross-tenant rule applies to user identities only.
        // Admin must still be allowed to view any tenant's profile via
        // host-scoped routing.
        let (_dir, state, _user_store, profile_store) = temp_app_state();
        let mut tenant_b = make_user_profile("tenant-b", "Tenant B");
        tenant_b.public_subdomain = Some("tenantb".into());
        profile_store.save(&tenant_b).unwrap();

        let identity = AuthIdentity::Admin;
        let result = resolve_my_profile_id(
            &identity,
            &profile_store,
            &state,
            &scoped_host_headers("tenantb.example.test"),
        )
        .unwrap();
        assert_eq!(result, "tenant-b");
    }

    #[test]
    fn host_scope_admin_role_user_can_access_any_tenant() {
        // Codex P2 (PR #958 review): an admin email-session
        // (UserRole::Admin) must have the same cross-tenant scope as a
        // bootstrap admin token. Otherwise an admin who logs in via OTP
        // would 403 on tenant subdomains while the bootstrap token
        // would not — inconsistent and breaks the day-to-day admin
        // workflow.
        let (_dir, state, _user_store, profile_store) = temp_app_state();
        let mut tenant_b = make_user_profile("tenant-b", "Tenant B");
        tenant_b.public_subdomain = Some("tenantb".into());
        profile_store.save(&tenant_b).unwrap();

        let identity = AuthIdentity::User {
            id: "admin-user".into(),
            role: UserRole::Admin,
        };
        let result = resolve_my_profile_id(
            &identity,
            &profile_store,
            &state,
            &scoped_host_headers("tenantb.example.test"),
        )
        .unwrap();
        assert_eq!(result, "tenant-b");
    }

    #[test]
    fn is_authorized_for_profile_table() {
        // The shared auth helper is consumed by both the host-scoped
        // path AND the X-Profile-Id branch in `my_content` (codex P1
        // fix). Lock the truth-table down with a focused unit test.
        let (_dir, state, _user_store, profile_store) = temp_app_state();
        profile_store
            .save(&make_user_profile("tenant", "Tenant"))
            .unwrap();
        let mut child = make_user_profile("tenant--child", "Child");
        child.parent_id = Some("tenant".into());
        profile_store.save(&child).unwrap();
        profile_store
            .save(&make_user_profile("other", "Other"))
            .unwrap();

        // Admin token: yes to everything.
        assert!(is_authorized_for_profile(
            &state,
            &AuthIdentity::Admin,
            "tenant"
        ));
        assert!(is_authorized_for_profile(
            &state,
            &AuthIdentity::Admin,
            "other"
        ));

        // Admin role (OTP-authenticated admin user): yes to everything.
        let admin_user = AuthIdentity::User {
            id: "any-admin".into(),
            role: UserRole::Admin,
        };
        assert!(is_authorized_for_profile(&state, &admin_user, "tenant"));
        assert!(is_authorized_for_profile(&state, &admin_user, "other"));

        // Regular user: own profile yes.
        let tenant_user = AuthIdentity::User {
            id: "tenant".into(),
            role: UserRole::User,
        };
        assert!(is_authorized_for_profile(&state, &tenant_user, "tenant"));
        // Sub-account they own: yes.
        assert!(is_authorized_for_profile(
            &state,
            &tenant_user,
            "tenant--child"
        ));
        // A different tenant: no.
        assert!(!is_authorized_for_profile(&state, &tenant_user, "other"));
        // A non-existent profile: no.
        assert!(!is_authorized_for_profile(&state, &tenant_user, "ghost"));
    }

    #[test]
    fn host_scope_parent_user_authorized_for_own_sub_account() {
        // A top-level user owns a sub-account; that sub-account has a
        // public subdomain. Parent visits the sub-account's subdomain
        // (e.g. via "Switch profile" / sub-account UI) → host check
        // resolves to the sub-account id; the parent IS authorized
        // because they own the sub. Verifies `is_authorized_for_profile`
        // walks the parent_id relationship.
        let (_dir, state, _user_store, profile_store) = temp_app_state();
        profile_store
            .save(&make_user_profile("tenant", "Tenant"))
            .unwrap();
        let mut child = make_user_profile("tenant--assistant", "Assistant");
        child.parent_id = Some("tenant".into());
        child.public_subdomain = Some("assistant".into());
        profile_store.save(&child).unwrap();

        let identity = AuthIdentity::User {
            id: "tenant".into(),
            role: UserRole::User,
        };
        let result = resolve_my_profile_id(
            &identity,
            &profile_store,
            &state,
            &scoped_host_headers("assistant.example.test"),
        )
        .unwrap();
        assert_eq!(result, "tenant--assistant");
    }

    #[test]
    fn trusted_auth_scope_prefers_host_over_stale_header() {
        let (_dir, state, _user_store, profile_store) = temp_app_state();
        profile_store
            .save(&make_user_profile("tenant", "Tenant Owner"))
            .unwrap();
        let mut child = make_user_profile("tenant--assistant", "Assistant");
        child.parent_id = Some("tenant".into());
        profile_store.save(&child).unwrap();

        let mut headers = scoped_host_headers("tenant.example.test");
        headers.insert("X-Profile-Id", "tenant--assistant".parse().unwrap());

        let scoped = trusted_auth_scope_profile_id(&state, &headers);
        assert_eq!(scoped.as_deref(), Some("tenant"));
    }

    #[test]
    fn trusted_auth_scope_allows_localhost_header_fallback() {
        let (_dir, state, _user_store, profile_store) = temp_app_state();
        let mut child = make_user_profile("tenant--assistant", "Assistant");
        child.parent_id = Some("tenant".into());
        profile_store.save(&child).unwrap();

        let scoped =
            trusted_auth_scope_profile_id(&state, &localhost_scoped_headers("tenant--assistant"));
        assert_eq!(scoped.as_deref(), Some("tenant--assistant"));
    }

    #[test]
    fn trusted_auth_scope_resolves_child_public_subdomain() {
        let (_dir, state, _user_store, profile_store) = temp_app_state();
        profile_store
            .save(&make_user_profile("tenant", "Tenant Owner"))
            .unwrap();
        let mut child = make_user_profile("tenant--assistant", "Assistant");
        child.parent_id = Some("tenant".into());
        child.public_subdomain = Some("assistant".into());
        profile_store.save(&child).unwrap();

        let scoped =
            trusted_auth_scope_profile_id(&state, &scoped_host_headers("assistant.example.test"));
        assert_eq!(scoped.as_deref(), Some("tenant--assistant"));
    }

    #[tokio::test]
    async fn top_level_user_can_change_own_public_subdomain() {
        let (_dir, state, _user_store, profile_store) = temp_app_state();
        profile_store
            .save(&make_user_profile("tenant", "Tenant Owner"))
            .unwrap();

        let Json(resp) = update_my_profile(
            State(Arc::new(state)),
            HeaderMap::new(),
            axum::Extension(AuthIdentity::User {
                id: "tenant".into(),
                role: UserRole::User,
            }),
            serde_json::json!({
                "public_subdomain": "tenant-host"
            })
            .to_string(),
        )
        .await
        .unwrap();

        assert_eq!(
            resp.profile.public_subdomain.as_deref(),
            Some("tenant-host")
        );
    }

    #[tokio::test]
    async fn my_profile_config_patch_preserves_existing_sections() {
        let (_dir, state, _user_store, profile_store) = temp_app_state();
        let mut profile = make_user_profile("tenant", "Tenant Owner");
        profile.config.plugins = crate::config::PluginsConfig {
            require_signed: true,
        };
        profile.config.home = Some(serde_json::json!({
            "settings": {
                "city": "Tokyo",
                "clock_format": "24h"
            },
            "events": [
                { "id": "dinner", "title": "Dinner" }
            ]
        }));
        profile_store.save(&profile).unwrap();

        let Json(resp) = update_my_profile(
            State(Arc::new(state)),
            HeaderMap::new(),
            axum::Extension(AuthIdentity::User {
                id: "tenant".into(),
                role: UserRole::User,
            }),
            serde_json::json!({
                "config": {
                    "home": {
                        "settings": {
                            "city": "Osaka"
                        }
                    }
                }
            })
            .to_string(),
        )
        .await
        .unwrap();

        assert!(resp.profile.config.plugins.require_signed);
        let home = resp.profile.config.home.expect("home config");
        assert_eq!(home["settings"]["city"], "Osaka");
        assert_eq!(home["settings"]["clock_format"], "24h");
        assert_eq!(home["events"][0]["title"], "Dinner");
    }

    #[tokio::test]
    async fn my_profile_llm_patch_bootstraps_on_demand_appui_runtime() {
        let (_dir, state, _user_store, profile_store) = temp_app_state();
        let mut profile = make_user_profile("tenant", "Tenant Owner");
        profile.enabled = false;
        profile_store.save(&profile).unwrap();
        let state = Arc::new(state);

        let _ = update_my_profile(
            State(state.clone()),
            HeaderMap::new(),
            axum::Extension(AuthIdentity::User {
                id: "tenant".into(),
                role: UserRole::User,
            }),
            serde_json::json!({
                "config": {
                    "llm": {
                        "primary": {
                            "family_id": "openai",
                            "model_id": "gpt-4o-mini",
                            "route": {
                                "api_key_env": "OCTOS_TEST_MY_PROFILE_LLM_KEY"
                            }
                        },
                        "fallbacks": []
                    },
                    "env_vars": {
                        "OCTOS_TEST_MY_PROFILE_LLM_KEY": "test-key"
                    }
                }
            })
            .to_string(),
        )
        .await
        .expect("profile update");

        let runtime = crate::api::ui_protocol_transport::resolve_session_profile_runtime(
            &state,
            Some("tenant"),
        )
        .expect("profile update should make the runtime live");
        assert_eq!(runtime.primary_model_id, "gpt-4o-mini");
    }

    // #1470: the strict `config: Option<ProfileConfig>` parse ran before the
    // raw body merge, so a partial nested-section patch for a section with
    // required fields (e.g. `email.provider`) 400'd before the merge could
    // preserve the stored values.
    #[tokio::test]
    async fn my_profile_config_patch_accepts_partial_nested_email() {
        let (_dir, state, _user_store, profile_store) = temp_app_state();
        let mut profile = make_user_profile("tenant", "Tenant Owner");
        profile.config.email = Some(crate::profiles::EmailSettings {
            provider: "smtp".into(),
            smtp_host: Some("smtp1.example.org".into()),
            smtp_port: Some(587),
            username: None,
            password_env: None,
            password: None,
            from_address: Some("bot@example.org".into()),
            feishu_app_id: None,
            feishu_app_secret_env: None,
            feishu_app_secret: None,
            feishu_from_address: None,
            feishu_region: None,
        });
        profile_store.save(&profile).unwrap();

        let Json(resp) = update_my_profile(
            State(Arc::new(state)),
            HeaderMap::new(),
            axum::Extension(AuthIdentity::User {
                id: "tenant".into(),
                role: UserRole::User,
            }),
            serde_json::json!({
                "config": {
                    "email": {
                        "smtp_host": "smtp2.example.org"
                    }
                }
            })
            .to_string(),
        )
        .await
        .unwrap();

        let email = resp.profile.config.email.expect("email config");
        assert_eq!(email.provider, "smtp");
        assert_eq!(email.smtp_host.as_deref(), Some("smtp2.example.org"));
        assert_eq!(email.smtp_port, Some(587));
        assert_eq!(email.from_address.as_deref(), Some("bot@example.org"));
    }

    // The flip side of accepting partial nested patches: a patch that merges
    // to an INVALID config (here a wrong-typed `smtp_port`) must 400 and
    // leave the stored config untouched — the post-merge validation error
    // must not be silently swallowed.
    #[tokio::test]
    async fn my_profile_config_patch_rejects_invalid_merged_config() {
        let (_dir, state, _user_store, profile_store) = temp_app_state();
        let mut profile = make_user_profile("tenant", "Tenant Owner");
        profile.config.email = Some(crate::profiles::EmailSettings {
            provider: "smtp".into(),
            smtp_host: Some("smtp1.example.org".into()),
            smtp_port: Some(587),
            username: None,
            password_env: None,
            password: None,
            from_address: None,
            feishu_app_id: None,
            feishu_app_secret_env: None,
            feishu_app_secret: None,
            feishu_from_address: None,
            feishu_region: None,
        });
        profile_store.save(&profile).unwrap();

        let err = update_my_profile(
            State(Arc::new(state)),
            HeaderMap::new(),
            axum::Extension(AuthIdentity::User {
                id: "tenant".into(),
                role: UserRole::User,
            }),
            serde_json::json!({
                "config": {
                    "email": {
                        "smtp_port": "not-a-port"
                    }
                }
            })
            .to_string(),
        )
        .await;
        let err = match err {
            Ok(_) => panic!("invalid merged config unexpectedly succeeded"),
            Err(err) => err,
        };
        assert_eq!(err.0, StatusCode::BAD_REQUEST);

        let stored = profile_store.get("tenant").unwrap().expect("profile");
        let email = stored.config.email.expect("email config");
        assert_eq!(email.smtp_host.as_deref(), Some("smtp1.example.org"));
        assert_eq!(email.smtp_port, Some(587));
    }

    // The config merge preserves sections the client omits (above), but a
    // provided `env_vars` map must still be able to DROP keys and clear the set
    // — otherwise secrets could never be removed via self-service. `env_vars` is
    // replaced wholesale when provided (see `merge_profile_config_from_body`).
    #[tokio::test]
    async fn my_profile_env_vars_can_drop_keys_and_clear() {
        let (_dir, state, _user_store, profile_store) = temp_app_state();
        let state = Arc::new(state);
        let mut profile = make_user_profile("tenant", "Tenant Owner");
        profile.config.env_vars.insert("KEEP".into(), "old".into());
        profile.config.env_vars.insert("DROP".into(), "gone".into());
        profile_store.save(&profile).unwrap();

        // A smaller map: KEEP updated, DROP omitted → removed. (Assert on the
        // persisted profile: the API response masks secret values.)
        let _ = update_my_profile(
            State(state.clone()),
            HeaderMap::new(),
            axum::Extension(AuthIdentity::User {
                id: "tenant".into(),
                role: UserRole::User,
            }),
            serde_json::json!({ "config": { "env_vars": { "KEEP": "new" } } }).to_string(),
        )
        .await
        .unwrap();
        let stored = profile_store.get("tenant").unwrap().expect("profile");
        assert_eq!(
            stored.config.env_vars.get("KEEP").map(String::as_str),
            Some("new")
        );
        assert!(
            !stored.config.env_vars.contains_key("DROP"),
            "a key omitted from the provided env_vars map must be removed"
        );

        // An explicit empty map clears everything.
        let _ = update_my_profile(
            State(state),
            HeaderMap::new(),
            axum::Extension(AuthIdentity::User {
                id: "tenant".into(),
                role: UserRole::User,
            }),
            serde_json::json!({ "config": { "env_vars": {} } }).to_string(),
        )
        .await
        .unwrap();
        let stored = profile_store.get("tenant").unwrap().expect("profile");
        assert!(
            stored.config.env_vars.is_empty(),
            "an explicit empty env_vars map must clear all entries"
        );
    }

    // Task #15: smart-home bridge config needs no dedicated REST route — it
    // rides the same generic `config` JSON-merge-patch every other
    // `ProfileConfig` section uses (see `my_profile_config_patch_preserves_existing_sections`
    // above). This proves the wire round trip end to end: the response masks
    // the token (never the URL), while the persisted profile keeps the real
    // token so the bridge client (`smart_home_bridge.rs`) can still use it.
    #[tokio::test]
    async fn my_profile_config_patch_applies_and_masks_smart_home_section() {
        let (_dir, state, _user_store, profile_store) = temp_app_state();
        profile_store
            .save(&make_user_profile("tenant", "Tenant Owner"))
            .unwrap();
        let state = Arc::new(state);

        let Json(resp) = update_my_profile(
            State(state.clone()),
            HeaderMap::new(),
            axum::Extension(AuthIdentity::User {
                id: "tenant".into(),
                role: UserRole::User,
            }),
            serde_json::json!({
                "config": {
                    "smart_home": {
                        "bridge_url": "http://192.168.1.50:8787",
                        "token": "supersecret-token-value"
                    }
                }
            })
            .to_string(),
        )
        .await
        .unwrap();

        let smart_home = resp.profile.config.smart_home.expect("smart_home present");
        assert_eq!(
            smart_home.bridge_url.as_deref(),
            Some("http://192.168.1.50:8787"),
            "bridge_url is not a secret and must round-trip in the clear"
        );
        let masked_token = smart_home.token.expect("token present in response");
        assert_ne!(
            masked_token, "supersecret-token-value",
            "the response must mask the token"
        );

        let stored = profile_store.get("tenant").unwrap().expect("profile");
        let stored_smart_home = stored.config.smart_home.expect("smart_home persisted");
        assert_eq!(
            stored_smart_home.token.as_deref(),
            Some("supersecret-token-value"),
            "the persisted profile must keep the real token, not the masked display value"
        );
    }

    #[tokio::test]
    async fn my_profile_rejects_service_account_json_under_custom_env_off_macos() {
        // Regression for the dashboard "Custom" bypass: a raw Vertex SA JSON
        // pasted under a CUSTOM env name (VERTEX_API_KEY, not the whitelisted
        // VERTEX_SA_JSON) via PUT /api/my/profile must never reach plaintext
        // config.
        //
        // #2234/45a contract (same shape as the admin.rs twin): the
        // availability predicate is `keychain::is_available()`, NOT
        // `cfg!(macos)`. On a store-backed host (linux file backend with an
        // INJECTED temp root — never the real keychain) the JSON is
        // legitimately relocated: the call succeeds and the slot becomes a
        // keychain marker, never the raw value. Hosts with NO backend keep
        // the rejection.
        let _secrets_root =
            crate::auth::keychain::test_override_secrets_root(tempfile::tempdir().unwrap().keep());
        let (_dir, state, _user_store, profile_store) = temp_app_state();
        profile_store
            .save(&make_user_profile("tenant", "Tenant Owner"))
            .unwrap();

        let res = update_my_profile(
            State(Arc::new(state)),
            HeaderMap::new(),
            axum::Extension(AuthIdentity::User {
                id: "tenant".into(),
                role: UserRole::User,
            }),
            serde_json::json!({
                "config": { "env_vars": {
                    "VERTEX_API_KEY": "{\"type\":\"service_account\",\"private_key\":\"x\"}"
                } }
            })
            .to_string(),
        )
        .await;

        let stored = profile_store.get("tenant").unwrap().expect("profile");
        let slot = stored
            .config
            .env_vars
            .get("VERTEX_API_KEY")
            .expect("slot present after either path");
        if crate::auth::keychain::is_available() {
            // Store-backed host: relocation succeeded, plaintext replaced by
            // a keychain marker.
            assert!(res.is_ok(), "store-backed host relocates the raw SA JSON");
            assert!(
                crate::auth::keychain::is_marker(slot),
                "slot must be a keychain marker, got: {slot}"
            );
            assert!(
                !slot.contains("private_key"),
                "the raw private key must never persist"
            );
        } else {
            // No backend: rejected, slot untouched (raw value not persisted).
            assert!(
                res.is_err(),
                "raw SA JSON under a custom env name must be rejected with no store"
            );
            assert!(
                !slot.contains("private_key"),
                "the private key must never be persisted to plaintext config"
            );
        }
    }

    // Replacing `env_vars` wholesale must NOT clobber a real secret when the UI
    // round-trips it masked: `save_with_merge` restores masked/empty values
    // per-key from the stored profile. Sections absent from the patch are still
    // preserved.
    #[tokio::test]
    async fn my_profile_env_vars_replace_preserves_masked_and_other_sections() {
        let (_dir, state, _user_store, profile_store) = temp_app_state();
        let mut profile = make_user_profile("tenant", "Tenant Owner");
        profile
            .config
            .env_vars
            .insert("SECRET".into(), "realval".into());
        profile.config.plugins = crate::config::PluginsConfig {
            require_signed: true,
        };
        profile_store.save(&profile).unwrap();

        let _ = update_my_profile(
            State(Arc::new(state)),
            HeaderMap::new(),
            axum::Extension(AuthIdentity::User {
                id: "tenant".into(),
                role: UserRole::User,
            }),
            serde_json::json!({ "config": { "env_vars": { "SECRET": "***" } } }).to_string(),
        )
        .await
        .unwrap();

        // Assert on the persisted profile (the response masks secret values).
        let stored = profile_store.get("tenant").unwrap().expect("profile");
        assert_eq!(
            stored.config.env_vars.get("SECRET").map(String::as_str),
            Some("realval"),
            "a masked value round-tripped by the UI must not overwrite the real secret"
        );
        assert!(
            stored.config.plugins.require_signed,
            "sections omitted from the patch must still be preserved"
        );
    }

    #[tokio::test]
    async fn sub_account_cannot_change_own_public_subdomain() {
        let (_dir, state, _user_store, profile_store) = temp_app_state();
        profile_store
            .save(&make_user_profile("tenant", "Tenant Owner"))
            .unwrap();
        let mut child = make_user_profile("tenant--assistant", "Assistant");
        child.parent_id = Some("tenant".into());
        child.public_subdomain = Some("assistant".into());
        profile_store.save(&child).unwrap();

        let err = update_my_profile(
            State(Arc::new(state)),
            HeaderMap::new(),
            axum::Extension(AuthIdentity::User {
                id: "tenant--assistant".into(),
                role: UserRole::User,
            }),
            serde_json::json!({
                "public_subdomain": "new-assistant"
            })
            .to_string(),
        )
        .await;

        let err = match err {
            Ok(_) => panic!("sub-account self-update unexpectedly succeeded"),
            Err(err) => err,
        };

        assert_eq!(err.0, StatusCode::FORBIDDEN);
        assert_eq!(
            err.1,
            "sub-accounts cannot change their own public subdomain"
        );
    }

    #[tokio::test]
    async fn managed_sub_account_config_patch_preserves_existing_sections() {
        let (_dir, state, _user_store, profile_store) = temp_app_state();
        let state = AppState {
            process_manager: Some(Arc::new(crate::process_manager::ProcessManager::new(
                profile_store.clone(),
            ))),
            ..state
        };
        profile_store
            .save(&make_user_profile("tenant", "Tenant Owner"))
            .unwrap();
        let mut child = make_user_profile("tenant--assistant", "Assistant");
        child.parent_id = Some("tenant".into());
        child.public_subdomain = Some("assistant".into());
        child.config.plugins = crate::config::PluginsConfig {
            require_signed: true,
        };
        child.config.home = Some(serde_json::json!({
            "settings": {
                "city": "Tokyo",
                "clock_format": "24h"
            },
            "events": [
                { "id": "school", "title": "School pickup" }
            ]
        }));
        profile_store.save(&child).unwrap();

        let Json(resp) = update_my_sub_account(
            State(Arc::new(state)),
            HeaderMap::new(),
            axum::Extension(AuthIdentity::User {
                id: "tenant".into(),
                role: UserRole::User,
            }),
            Path("tenant--assistant".into()),
            serde_json::json!({
                "config": {
                    "home": {
                        "settings": {
                            "city": "Kyoto"
                        }
                    }
                }
            })
            .to_string(),
        )
        .await
        .unwrap();

        assert!(resp.profile.config.plugins.require_signed);
        let home = resp.profile.config.home.expect("home config");
        assert_eq!(home["settings"]["city"], "Kyoto");
        assert_eq!(home["settings"]["clock_format"], "24h");
        assert_eq!(home["events"][0]["title"], "School pickup");
    }

    // #1470: same partial nested-section patch as above, but through the
    // parent-managed sub-account endpoint.
    #[tokio::test]
    async fn managed_sub_account_config_patch_accepts_partial_nested_email() {
        let (_dir, state, _user_store, profile_store) = temp_app_state();
        let state = AppState {
            process_manager: Some(Arc::new(crate::process_manager::ProcessManager::new(
                profile_store.clone(),
            ))),
            ..state
        };
        profile_store
            .save(&make_user_profile("tenant", "Tenant Owner"))
            .unwrap();
        let mut child = make_user_profile("tenant--assistant", "Assistant");
        child.parent_id = Some("tenant".into());
        child.public_subdomain = Some("assistant".into());
        child.config.email = Some(crate::profiles::EmailSettings {
            provider: "smtp".into(),
            smtp_host: Some("smtp1.example.org".into()),
            smtp_port: None,
            username: None,
            password_env: None,
            password: None,
            from_address: None,
            feishu_app_id: None,
            feishu_app_secret_env: None,
            feishu_app_secret: None,
            feishu_from_address: None,
            feishu_region: None,
        });
        profile_store.save(&child).unwrap();

        let Json(resp) = update_my_sub_account(
            State(Arc::new(state)),
            HeaderMap::new(),
            axum::Extension(AuthIdentity::User {
                id: "tenant".into(),
                role: UserRole::User,
            }),
            Path("tenant--assistant".into()),
            serde_json::json!({
                "config": {
                    "email": {
                        "smtp_host": "smtp2.example.org"
                    }
                }
            })
            .to_string(),
        )
        .await
        .unwrap();

        let email = resp.profile.config.email.expect("email config");
        assert_eq!(email.provider, "smtp");
        assert_eq!(email.smtp_host.as_deref(), Some("smtp2.example.org"));
    }

    #[tokio::test]
    async fn my_profile_skills_lists_current_user_skills() {
        let (_dir, state, _user_store, profile_store) = temp_app_state();
        profile_store
            .save(&make_user_profile("alice", "Alice"))
            .unwrap();

        let skills_dir =
            crate::commands::skills::resolve_profile_skills_dir(&profile_store, "alice").unwrap();
        let skill_dir = skills_dir.join("demo-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), "# Demo skill\n").unwrap();

        let Json(resp) = my_profile_skills(
            State(Arc::new(state)),
            HeaderMap::new(),
            axum::Extension(AuthIdentity::User {
                id: "alice".into(),
                role: UserRole::User,
            }),
        )
        .await
        .unwrap();

        let skills = resp
            .get("skills")
            .and_then(|value| value.as_array())
            .expect("skills array");
        assert_eq!(skills.len(), 1);
        assert_eq!(
            skills[0].get("name").and_then(|value| value.as_str()),
            Some("demo-skill")
        );
    }

    #[tokio::test]
    async fn install_my_profile_skill_allows_non_admin_users_for_own_profile() {
        let (dir, state, _user_store, profile_store) = temp_app_state();
        profile_store
            .save(&make_user_profile("alice", "Alice"))
            .unwrap();

        let local_skill_dir = dir.path().join("demo-local-skill");
        std::fs::create_dir_all(&local_skill_dir).unwrap();
        std::fs::write(local_skill_dir.join("SKILL.md"), "# Demo local skill\n").unwrap();

        let Json(resp) = install_my_profile_skill(
            State(Arc::new(state)),
            HeaderMap::new(),
            axum::Extension(AuthIdentity::User {
                id: "alice".into(),
                role: UserRole::User,
            }),
            Json(crate::api::admin::InstallSkillRequest {
                repo: local_skill_dir.to_string_lossy().to_string(),
                force: false,
                branch: "main".into(),
            }),
        )
        .await
        .unwrap();

        assert_eq!(resp.get("ok").and_then(|value| value.as_bool()), Some(true));

        let skills_dir =
            crate::commands::skills::resolve_profile_skills_dir(&profile_store, "alice").unwrap();
        assert!(
            skills_dir
                .join("demo-local-skill")
                .join("SKILL.md")
                .exists()
        );
    }

    #[tokio::test]
    async fn scoped_send_code_hides_whether_email_is_registered() {
        let (_dir, state, user_store, profile_store) = temp_app_state();
        let auth_mgr = state.auth_manager.as_ref().unwrap().clone();
        let mut child = make_user_profile("tenant--assistant", "Assistant");
        child.parent_id = Some("tenant".into());
        child.public_subdomain = Some("assistant".into());
        profile_store.save(&child).unwrap();
        user_store
            .save(&User {
                id: "tenant--assistant".into(),
                email: "assistant@example.com".into(),
                name: "Assistant".into(),
                role: UserRole::User,
                created_at: chrono::Utc::now(),
                last_login_at: None,
            })
            .unwrap();

        let Json(resp) = send_code(
            State(Arc::new(state)),
            scoped_host_headers("assistant.example.test"),
            Json(SendCodeRequest {
                email: "wrong@example.com".into(),
            }),
        )
        .await
        .unwrap();

        assert!(resp.ok);
        assert_eq!(
            resp.message.as_deref(),
            Some("Verification code sent to your email")
        );
        assert!(
            auth_mgr
                .test_pending_code("wrong@example.com", Some("tenant--assistant"))
                .await
                .is_none()
        );
        assert!(auth_mgr.test_sent_emails().await.is_empty());
    }

    #[tokio::test]
    async fn root_send_code_hides_whether_email_is_invited() {
        let (_dir, state, _user_store, _profile_store) = temp_app_state();
        let auth_mgr = state.auth_manager.as_ref().unwrap().clone();

        let Json(resp) = send_code(
            State(Arc::new(state)),
            HeaderMap::new(),
            Json(SendCodeRequest {
                email: "not-invited@example.com".into(),
            }),
        )
        .await
        .unwrap();

        assert!(resp.ok);
        assert_eq!(
            resp.message.as_deref(),
            Some("Verification code sent to your email")
        );
        assert!(
            auth_mgr
                .test_pending_code("not-invited@example.com", None)
                .await
                .is_none()
        );
        assert!(auth_mgr.test_sent_emails().await.is_empty());
    }

    #[tokio::test]
    async fn root_auth_status_ignores_sub_account_only_emails() {
        let (_dir, state, user_store, profile_store) = temp_app_state();
        profile_store
            .save(&make_user_profile("tenant", "Tenant Owner"))
            .unwrap();
        let mut child = make_user_profile("tenant--assistant", "Assistant");
        child.parent_id = Some("tenant".into());
        profile_store.save(&child).unwrap();
        user_store
            .save(&User {
                id: "tenant--assistant".into(),
                email: "assistant@example.com".into(),
                name: "Assistant".into(),
                role: UserRole::User,
                created_at: chrono::Utc::now(),
                last_login_at: None,
            })
            .unwrap();

        let Json(status) = auth_status(State(Arc::new(state)), HeaderMap::new())
            .await
            .unwrap();

        assert!(!status.email_login_enabled);
    }

    #[tokio::test]
    async fn root_allowlisted_email_can_complete_login_and_provision_user() {
        let (_dir, state, user_store, profile_store) = temp_app_state();
        let allowlist_store = state.allowlist_store.as_ref().unwrap().clone();
        allowlist_store
            .save(&AllowedLogin {
                email: "newuser@example.com".into(),
                note: Some("invited".into()),
                created_at: chrono::Utc::now(),
                claimed_user_id: None,
                claimed_at: None,
            })
            .unwrap();
        let auth_mgr = state.auth_manager.as_ref().unwrap().clone();
        let state = Arc::new(state);

        let Json(send_resp) = send_code(
            State(state.clone()),
            HeaderMap::new(),
            Json(SendCodeRequest {
                email: "newuser@example.com".into(),
            }),
        )
        .await
        .unwrap();
        assert!(send_resp.ok);

        let code = auth_mgr
            .test_pending_code("newuser@example.com", None)
            .await
            .expect("allowlisted root login should create a global OTP");

        let Json(verify_resp) = verify(
            State(state),
            HeaderMap::new(),
            Json(VerifyRequest {
                email: "newuser@example.com".into(),
                code,
            }),
        )
        .await
        .unwrap();

        assert!(verify_resp.ok);
        let user = verify_resp
            .user
            .expect("verify should return the provisioned user");
        assert_eq!(user.id, "newuser");

        let saved_user = user_store
            .get_by_email("newuser@example.com")
            .unwrap()
            .unwrap();
        assert_eq!(saved_user.id, "newuser");
        assert!(profile_store.get("newuser").unwrap().is_some());

        let allowlist_entry = allowlist_store.get("newuser@example.com").unwrap().unwrap();
        assert_eq!(allowlist_entry.claimed_user_id.as_deref(), Some("newuser"));
        assert!(allowlist_entry.claimed_at.is_some());
    }

    #[tokio::test]
    async fn root_self_registration_can_complete_login_and_provision_profile() {
        let (_dir, state, user_store, profile_store) = temp_app_state();
        let auth_mgr = state.auth_manager.as_ref().unwrap().clone();
        auth_mgr.set_allow_self_registration(true);
        let state = Arc::new(state);

        let Json(send_resp) = send_code(
            State(state.clone()),
            HeaderMap::new(),
            Json(SendCodeRequest {
                email: "selfreg@example.com".into(),
            }),
        )
        .await
        .unwrap();
        assert!(send_resp.ok);

        let code = auth_mgr
            .test_pending_code("selfreg@example.com", None)
            .await
            .expect("self-registration should create a global OTP");

        let Json(verify_resp) = verify(
            State(state.clone()),
            HeaderMap::new(),
            Json(VerifyRequest {
                email: "selfreg@example.com".into(),
                code,
            }),
        )
        .await
        .unwrap();

        assert!(verify_resp.ok);
        assert!(verify_resp.token.is_some());
        let user = verify_resp
            .user
            .expect("verify should return the self-registered user");
        assert_eq!(user.id, "selfreg");

        let saved_user = user_store
            .get_by_email("selfreg@example.com")
            .unwrap()
            .unwrap();
        assert_eq!(saved_user.id, "selfreg");
        assert!(profile_store.get("selfreg").unwrap().is_some());

        let Json(me_resp) = me(
            State(state),
            HeaderMap::new(),
            axum::Extension(AuthIdentity::User {
                id: "selfreg".into(),
                role: UserRole::User,
            }),
        )
        .await
        .unwrap();
        assert_eq!(me_resp.user.id, "selfreg");
        assert_eq!(me_resp.portal.home_profile_id, "selfreg");
    }

    #[tokio::test]
    async fn scoped_verify_uses_user_bound_otp() {
        let (_dir, state, user_store, profile_store) = temp_app_state();
        let mut child = make_user_profile("tenant--assistant", "Assistant");
        child.parent_id = Some("tenant".into());
        child.public_subdomain = Some("assistant".into());
        profile_store.save(&child).unwrap();
        user_store
            .save(&User {
                id: "tenant--assistant".into(),
                email: "assistant@example.com".into(),
                name: "Assistant".into(),
                role: UserRole::User,
                created_at: chrono::Utc::now(),
                last_login_at: None,
            })
            .unwrap();
        let auth_mgr = state.auth_manager.as_ref().unwrap().clone();
        let state = Arc::new(state);

        let Json(send_resp) = send_code(
            State(state.clone()),
            scoped_host_headers("assistant.example.test"),
            Json(SendCodeRequest {
                email: "assistant@example.com".into(),
            }),
        )
        .await
        .unwrap();
        assert!(send_resp.ok);
        assert!(
            auth_mgr
                .test_pending_code("assistant@example.com", None)
                .await
                .is_none(),
            "scoped host login should not use the global email OTP slot"
        );

        let code = auth_mgr
            .test_pending_code("assistant@example.com", Some("tenant--assistant"))
            .await
            .expect("scoped host login should bind the OTP to the profile user id");

        let Json(verify_resp) = verify(
            State(state),
            scoped_host_headers("assistant.example.test"),
            Json(VerifyRequest {
                email: "assistant@example.com".into(),
                code,
            }),
        )
        .await
        .unwrap();

        assert!(verify_resp.ok);
        let user = verify_resp
            .user
            .expect("verify should return the scoped user");
        assert_eq!(user.id, "tenant--assistant");
    }

    #[tokio::test]
    async fn send_code_returns_clear_error_when_smtp_unconfigured() {
        // Without SMTP, otp.rs::send_otp silently logs the OTP to the
        // server console and returns Ok(true). The handler used to forward
        // that as "Verification code sent to your email", leaving the user
        // staring at an empty inbox with no error indication. The precheck
        // now surfaces a clear server-state message — this is the
        // regression test that locks it in.
        let (_dir, state, _user_store, _profile_store) = temp_app_state();
        // Clear the synthetic SMTP that temp_app_state installs so the
        // precheck fires.
        state
            .auth_manager
            .as_ref()
            .unwrap()
            .set_smtp_config(None)
            .await;

        let resp = send_code(
            State(Arc::new(state)),
            HeaderMap::new(),
            Json(SendCodeRequest {
                email: "anyone@example.com".into(),
            }),
        )
        .await
        .unwrap();

        assert!(
            !resp.0.ok,
            "send_code must surface failure when SMTP is unconfigured (was masked by anti-enumeration always-success)"
        );
        let msg = resp.0.message.expect("message should be set");
        assert!(
            msg.contains("SMTP is not configured"),
            "message should explain the server-state issue, not a per-email issue: {msg}"
        );
        assert!(
            msg.contains("administrator"),
            "message should direct the user to contact admin: {msg}"
        );
    }

    #[tokio::test]
    async fn auth_status_email_login_disabled_when_smtp_unconfigured() {
        // /api/auth/status drives the dashboard's login-form rendering.
        // Reporting email_login_enabled=true while SMTP is missing leaves
        // the dashboard happy to show the email form for a login attempt
        // that can never succeed. With this change the flag honestly
        // reflects whether mail can actually be delivered.
        let (_dir, state, user_store, profile_store) = temp_app_state();
        // Add a top-level user so the user-based half of the AND would
        // otherwise be true.
        user_store
            .save(&User {
                id: "alice".into(),
                email: "alice@example.com".into(),
                name: "Alice".into(),
                role: UserRole::User,
                created_at: chrono::Utc::now(),
                last_login_at: None,
            })
            .unwrap();
        profile_store
            .save(&make_user_profile("alice", "Alice"))
            .unwrap();

        // First: SMTP configured (the temp_app_state default) → enabled.
        let Json(status_with_smtp) = auth_status(State(Arc::new(state)), HeaderMap::new())
            .await
            .unwrap();
        assert!(
            status_with_smtp.email_login_enabled,
            "with SMTP configured + a login-ready user, email login should be enabled"
        );

        // Second: clear SMTP → disabled even though the user still exists.
        let (_dir2, state2, user_store2, profile_store2) = temp_app_state();
        state2
            .auth_manager
            .as_ref()
            .unwrap()
            .set_smtp_config(None)
            .await;
        user_store2
            .save(&User {
                id: "alice".into(),
                email: "alice@example.com".into(),
                name: "Alice".into(),
                role: UserRole::User,
                created_at: chrono::Utc::now(),
                last_login_at: None,
            })
            .unwrap();
        profile_store2
            .save(&make_user_profile("alice", "Alice"))
            .unwrap();
        let Json(status_without_smtp) = auth_status(State(Arc::new(state2)), HeaderMap::new())
            .await
            .unwrap();
        assert!(
            !status_without_smtp.email_login_enabled,
            "without SMTP, email login must be disabled regardless of user state"
        );
    }

    #[test]
    fn send_code_request_deserialize() {
        let json = r#"{"email": "test@example.com"}"#;
        let req: SendCodeRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.email, "test@example.com");
    }

    #[test]
    fn send_code_response_serialize_with_message() {
        let resp = SendCodeResponse {
            ok: true,
            message: Some("sent".into()),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], true);
        assert_eq!(json["message"], "sent");
    }

    #[test]
    fn send_code_response_skip_none_message() {
        let resp = SendCodeResponse {
            ok: true,
            message: None,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], true);
        assert!(json.get("message").is_none());
    }

    #[test]
    fn verify_request_deserialize() {
        let json = r#"{"email": "a@b.com", "code": "123456"}"#;
        let req: VerifyRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.email, "a@b.com");
        assert_eq!(req.code, "123456");
    }

    #[test]
    fn verify_response_serialize_success() {
        let resp = VerifyResponse {
            ok: true,
            token: Some("tok123".into()),
            user: None,
            message: None,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], true);
        assert_eq!(json["token"], "tok123");
        // skip_serializing_if = None fields should be absent
        assert!(json.get("user").is_none());
        assert!(json.get("message").is_none());
    }

    #[test]
    fn verify_response_serialize_failure() {
        let resp = VerifyResponse {
            ok: false,
            token: None,
            user: None,
            message: Some("Invalid code".into()),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], false);
        assert!(json.get("token").is_none());
        assert_eq!(json["message"], "Invalid code");
    }

    #[test]
    fn action_response_serialize() {
        let resp = ActionResponse {
            ok: true,
            message: None,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], true);
        assert!(json.get("message").is_none());
    }

    #[test]
    fn action_response_with_message() {
        let resp = ActionResponse {
            ok: false,
            message: Some("error occurred".into()),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["message"], "error occurred");
    }

    #[test]
    fn extract_bearer_token_valid() {
        let req = Request::builder()
            .header("authorization", "Bearer my-secret-token")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(
            extract_bearer_token(&req),
            Some("my-secret-token".to_string())
        );
    }

    #[test]
    fn extract_bearer_token_missing_header() {
        let req = Request::builder().body(axum::body::Body::empty()).unwrap();
        assert_eq!(extract_bearer_token(&req), None);
    }

    #[test]
    fn extract_bearer_token_wrong_scheme() {
        let req = Request::builder()
            .header("authorization", "Basic abc123")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(extract_bearer_token(&req), None);
    }

    #[test]
    fn extract_bearer_token_empty_value() {
        let req = Request::builder()
            .header("authorization", "Bearer ")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(extract_bearer_token(&req), Some(String::new()));
    }
}
