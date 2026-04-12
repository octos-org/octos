//! Authentication and user self-service API handlers.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::{LazyLock, Mutex};

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::Json;
use futures::StreamExt;
use serde::{Deserialize, Serialize};

use super::admin::ProfileResponse;
use super::AppState;
use crate::profiles::mask_secrets;
use crate::user_store::{User, UserRole};

use super::router::AuthIdentity;

/// In-memory rate limiter for OTP send requests: email -> (count, window_start).
/// Allows at most 3 requests per 5-minute window per email address.
static OTP_RATE_LIMIT: LazyLock<Mutex<HashMap<String, (u32, std::time::Instant)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

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

fn host_scoped_profile_id(state: &AppState, headers: &HeaderMap) -> Option<String> {
    let host = request_host(headers)?;
    if is_local_request_host(&host) {
        return None;
    }

    let candidate = host.split('.').next()?.trim();
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
        .and_then(|store| store.get(candidate).ok().flatten())
        .map(|_| candidate.to_string())
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
        .map(ToOwned::to_owned)
}

fn resolve_root_login_user(state: &AppState, email: &str) -> Option<User> {
    let normalized = email.trim().to_lowercase();
    let user_store = state.user_store.as_ref()?;
    let matches: Vec<User> = user_store
        .list()
        .ok()?
        .into_iter()
        .filter(|user| user.email.trim().to_lowercase() == normalized)
        .filter(|user| is_top_level_profile_id(state, &user.id))
        .collect();

    if matches.len() == 1 {
        matches.into_iter().next()
    } else {
        if matches.len() > 1 {
            tracing::warn!(
                email = %normalized,
                count = matches.len(),
                "multiple top-level profiles share the same login email"
            );
        }
        None
    }
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

fn is_login_ready_email(email: &str) -> bool {
    !email.trim().is_empty()
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scoped_profile: Option<ScopedAuthTarget>,
}

#[derive(Serialize)]
pub struct MeResponse {
    pub user: User,
    pub profile: Option<ProfileResponse>,
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
    let requested_email = req.email.trim().to_lowercase();
    let scoped_profile_id = trusted_auth_scope_profile_id(&state, &headers);
    let login_target = if let Some(profile_id) = scoped_profile_id.as_deref() {
        resolve_scoped_login_user(&state, profile_id, &requested_email)
    } else {
        resolve_root_login_user(&state, &requested_email)
    };

    if scoped_profile_id.is_some() {
        if login_target.is_none() {
            tracing::warn!(
                email = %requested_email,
                scoped_profile = ?scoped_profile_id,
                "OTP skipped — email does not match scoped profile"
            );
            return Ok(Json(SendCodeResponse {
                ok: false,
                message: Some("This email is not registered for this account".into()),
            }));
        }
    } else if login_target.is_none() {
        tracing::warn!(email = %requested_email, "OTP skipped — email is not registered to a root profile");
        return Ok(Json(SendCodeResponse {
            ok: false,
            message: Some("This email is not registered for this account".into()),
        }));
    }

    // Rate-limit OTP sends: max 3 per email per 5-minute window.
    {
        let mut limits = OTP_RATE_LIMIT.lock().unwrap_or_else(|e| e.into_inner());
        let entry = limits
            .entry(requested_email.clone())
            .or_insert((0, std::time::Instant::now()));
        if entry.1.elapsed() > std::time::Duration::from_secs(300) {
            *entry = (0, std::time::Instant::now()); // reset after 5 min
        }
        if entry.0 >= 3 {
            tracing::warn!(email = %req.email, "OTP rate limit exceeded");
            // Return generic success to avoid leaking rate-limit state
            return Ok(Json(SendCodeResponse {
                ok: true,
                message: Some("Verification code sent to your email".into()),
            }));
        }
        entry.0 += 1;
    }

    tracing::info!(email = %requested_email, "login OTP requested");
    match auth_mgr.send_otp(&requested_email).await {
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
    let email_login_enabled = scoped_profile
        .as_ref()
        .map(|profile| profile.email_login_enabled)
        .unwrap_or(global_email_login_enabled);

    Ok(Json(AuthStatusResponse {
        bootstrap_mode: is_bootstrap_mode(&state),
        email_login_enabled,
        admin_token_login_enabled: state.auth_token.is_some(),
        allow_self_registration: false,
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
    let login_target = if let Some(profile_id) = scoped_profile_id.as_deref() {
        resolve_scoped_login_user(&state, profile_id, &requested_email)
    } else {
        resolve_root_login_user(&state, &requested_email)
    };

    if login_target.is_none() {
        return Ok(Json(VerifyResponse {
            ok: false,
            token: None,
            user: None,
            message: Some("Invalid or expired code".into()),
        }));
    }

    match auth_mgr.verify_otp(&requested_email, &req.code).await {
        Ok(Some(token)) => {
            tracing::info!(email = %requested_email, "user logged in");
            let user = login_target;

            // Auto-create profile if user has none
            if let Some(ref user) = user {
                if let Some(ref profile_store) = state.profile_store {
                    if profile_store.get(&user.id).unwrap_or(None).is_none() {
                        let profile = crate::profiles::UserProfile {
                            id: user.id.clone(),
                            name: user.name.clone(),
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
    axum::Extension(identity): axum::Extension<AuthIdentity>,
) -> Result<Json<MeResponse>, StatusCode> {
    // Handle admin token first — no user_store needed
    if matches!(&identity, AuthIdentity::Admin) {
        let profile = if let Some(ref ps) = state.profile_store {
            ensure_admin_profile(ps).ok();
            if let Ok(Some(p)) = ps.get(ADMIN_PROFILE_ID) {
                let status = if let Some(ref pm) = state.process_manager {
                    pm.status(&p.id).await
                } else {
                    crate::process_manager::ProcessStatus {
                        running: false,
                        pid: None,
                        started_at: None,
                        uptime_secs: None,
                    }
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

        return Ok(Json(MeResponse {
            user: User {
                id: ADMIN_PROFILE_ID.into(),
                email: "admin@localhost".into(),
                name: "Admin".into(),
                role: UserRole::Admin,
                created_at: chrono::Utc::now(),
                last_login_at: None,
            },
            profile,
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
                crate::process_manager::ProcessStatus {
                    running: false,
                    pid: None,
                    started_at: None,
                    uptime_secs: None,
                }
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

    Ok(Json(MeResponse { user, profile }))
}

// ── User self-service endpoints (/api/my/*) ───────────────────────────

/// GET /api/my/profile
pub async fn my_profile(
    State(state): State<Arc<AppState>>,
    axum::Extension(identity): axum::Extension<AuthIdentity>,
) -> Result<Json<ProfileResponse>, StatusCode> {
    let ps = state
        .profile_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    let profile = resolve_my_profile(&identity, ps)?;

    let status = if let Some(ref pm) = state.process_manager {
        pm.status(&profile.id).await
    } else {
        crate::process_manager::ProcessStatus {
            running: false,
            pid: None,
            started_at: None,
            uptime_secs: None,
        }
    };

    Ok(Json(ProfileResponse {
        email: None,
        profile: mask_secrets(&profile),
        status,
    }))
}

/// PUT /api/my/profile
pub async fn update_my_profile(
    State(state): State<Arc<AppState>>,
    axum::Extension(identity): axum::Extension<AuthIdentity>,
    body: String,
) -> Result<Json<ProfileResponse>, (StatusCode, String)> {
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

    let mut profile =
        resolve_my_profile(&identity, ps).map_err(|s| (s, "profile not found".into()))?;

    // Apply updates (same logic as admin::update_profile but scoped)
    if let Some(name) = req.name {
        profile.name = name;
    }
    if let Some(enabled) = req.enabled {
        profile.enabled = enabled;
    }
    if let Some(config) = req.config {
        profile.config = config;
    }
    profile.updated_at = chrono::Utc::now();

    ps.save_with_merge(&mut profile).map_err(|e| {
        tracing::error!(profile = %profile.id, error = %e, "failed to save user profile");
        (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })?;

    tracing::info!(profile = %profile.id, "user profile updated");
    let status = if let Some(ref pm) = state.process_manager {
        pm.status(&profile.id).await
    } else {
        crate::process_manager::ProcessStatus {
            running: false,
            pid: None,
            started_at: None,
            uptime_secs: None,
        }
    };

    Ok(Json(ProfileResponse {
        email: None,
        profile: mask_secrets(&profile),
        status,
    }))
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
    axum::Extension(identity): axum::Extension<AuthIdentity>,
) -> Result<Json<SoulResponse>, StatusCode> {
    let ps = state
        .profile_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let profile = resolve_my_profile(&identity, ps)?;
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
    let profile = resolve_my_profile(&identity, ps).map_err(|s| (s, "profile not found".into()))?;
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
    axum::Extension(identity): axum::Extension<AuthIdentity>,
) -> Result<Json<SoulResponse>, StatusCode> {
    let ps = state
        .profile_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let profile = resolve_my_profile(&identity, ps)?;
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

/// GET /api/my/content
pub async fn my_content(
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
    // Use X-Profile-Id header (from Caddy proxy) if available, otherwise resolve from identity
    let profile = if let Some(pid) = headers.get("x-profile-id").and_then(|v| v.to_str().ok()) {
        ps.get(pid)
            .map_err(|_| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "profile store error".into(),
                )
            })?
            .ok_or((StatusCode::NOT_FOUND, format!("profile '{pid}' not found")))?
    } else {
        resolve_my_profile(&identity, ps).map_err(|s| (s, "profile not found".into()))?
    };

    let catalog = mgr
        .get_catalog_with_scan(&profile.id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let cat = catalog.read().await;
    Ok(Json(cat.query(&query)))
}

/// GET /api/my/content/:id/thumbnail
pub async fn my_content_thumbnail(
    State(state): State<Arc<AppState>>,
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
    let profile = resolve_my_profile(&identity, ps)?;

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
    let profile = resolve_my_profile(&identity, ps)?;

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

/// DELETE /api/my/content/:id
pub async fn delete_my_content(
    State(state): State<Arc<AppState>>,
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
    let profile = resolve_my_profile(&identity, ps).map_err(|s| (s, "profile not found".into()))?;

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
pub struct BulkDeleteRequest {
    pub ids: Vec<String>,
}

/// POST /api/my/content/bulk-delete
pub async fn bulk_delete_my_content(
    State(state): State<Arc<AppState>>,
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
    let profile = resolve_my_profile(&identity, ps).map_err(|s| (s, "profile not found".into()))?;

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

    let profile = resolve_my_profile(&identity, ps)?;

    // Validate LLM provider is configured
    if profile.config.provider.is_none() && profile.config.model.is_none() {
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
    axum::Extension(identity): axum::Extension<AuthIdentity>,
) -> Result<Json<ActionResponse>, StatusCode> {
    let ps = state
        .profile_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let profile_id = resolve_my_profile_id(&identity, ps)?;
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

    let profile = resolve_my_profile(&identity, ps)?;

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
    axum::Extension(identity): axum::Extension<AuthIdentity>,
) -> Result<Json<crate::process_manager::ProcessStatus>, StatusCode> {
    let ps = state
        .profile_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let profile_id = resolve_my_profile_id(&identity, ps)?;
    let pm = state
        .process_manager
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    Ok(Json(pm.status(&profile_id).await))
}

/// GET /api/my/profile/logs
pub async fn my_gateway_logs(
    State(state): State<Arc<AppState>>,
    axum::Extension(identity): axum::Extension<AuthIdentity>,
) -> Result<Sse<impl futures::Stream<Item = Result<Event, std::convert::Infallible>>>, StatusCode> {
    let ps = state
        .profile_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let profile_id = resolve_my_profile_id(&identity, ps)?;
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
    axum::Extension(identity): axum::Extension<AuthIdentity>,
) -> Result<Json<crate::process_manager::BridgeQrInfo>, StatusCode> {
    let ps = state
        .profile_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let profile_id = resolve_my_profile_id(&identity, ps)?;
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
    axum::Extension(identity): axum::Extension<AuthIdentity>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let ps = state
        .profile_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let profile_id = resolve_my_profile_id(&identity, ps)?;
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
    axum::Extension(identity): axum::Extension<AuthIdentity>,
) -> Result<Json<Vec<crate::api::admin::ProfileResponse>>, StatusCode> {
    let ps = state
        .profile_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let profile_id = resolve_my_profile_id(&identity, ps)?;
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

/// Helper: resolve a sub-account owned by the current user.
fn resolve_my_sub_account(
    identity: &AuthIdentity,
    ps: &crate::profiles::ProfileStore,
    sub_id: &str,
) -> Result<crate::profiles::UserProfile, StatusCode> {
    let parent_id = resolve_my_profile_id(identity, ps)?;
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

    let sub = resolve_my_sub_account(&identity, ps, &sub_id)?;

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

    let _ = resolve_my_sub_account(&identity, ps, &sub_id)?;

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

/// Resolve the profile ID for "my" endpoints.
/// For regular users, returns their user ID. For admin token, returns the admin's own profile ID
/// (auto-creating the admin profile if it doesn't exist yet).
fn resolve_my_profile_id(
    identity: &AuthIdentity,
    ps: &crate::profiles::ProfileStore,
) -> Result<String, StatusCode> {
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
) -> Result<crate::profiles::UserProfile, StatusCode> {
    let id = resolve_my_profile_id(identity, ps)?;
    ps.get(&id)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)
}

/// The fixed profile ID used for token-based admin authentication.
/// This ensures the admin has its own separate profile, distinct from any user profiles.
pub const ADMIN_PROFILE_ID: &str = "admin";

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::otp::AuthManager;
    use crate::profiles::ProfileStore;
    use crate::user_store::UserStore;
    use axum::http::HeaderMap;
    use axum::http::Request;

    fn temp_profile_store() -> (tempfile::TempDir, ProfileStore) {
        let dir = tempfile::tempdir().unwrap();
        let ps = ProfileStore::open(dir.path()).unwrap();
        (dir, ps)
    }

    fn make_user_profile(id: &str, name: &str) -> crate::profiles::UserProfile {
        crate::profiles::UserProfile {
            id: id.into(),
            name: name.into(),
            enabled: true,
            data_dir: None,
            parent_id: None,
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
        let profile_store = Arc::new(ProfileStore::open(dir.path()).unwrap());
        let state = AppState {
            agent: None,
            sessions: None,
            broadcaster: Arc::new(crate::api::SseBroadcaster::new(8)),
            started_at: chrono::Utc::now(),
            auth_token: Some("bootstrap-token".into()),
            metrics_handle: None,
            profile_store: Some(profile_store.clone()),
            process_manager: None,
            user_store: Some(user_store.clone()),
            auth_manager: Some(Arc::new(AuthManager::new(None, user_store.clone()))),
            http_client: reqwest::Client::new(),
            config_path: None,
            watchdog_enabled: None,
            alerts_enabled: None,
            sysinfo: tokio::sync::Mutex::new(sysinfo::System::new_all()),
            tenant_store: None,
            tunnel_domain: None,
            frps_server: None,
            frps_port: None,
            deployment_mode: crate::config::DeploymentMode::Local,
            allow_admin_shell: false,
            content_catalog_mgr: None,
        };
        (dir, state, user_store, profile_store)
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
    fn should_return_admin_id_when_admin_identity() {
        let (_dir, ps) = temp_profile_store();
        // Create a user profile that would have been returned by the old "first" logic
        ps.save(&make_user_profile("guofoo", "Guo Foo")).unwrap();

        let identity = AuthIdentity::Admin;
        let result = resolve_my_profile_id(&identity, &ps).unwrap();
        assert_eq!(
            result, ADMIN_PROFILE_ID,
            "admin should get its own profile ID, not the first user's"
        );
    }

    #[test]
    fn should_return_user_id_when_user_identity() {
        let (_dir, ps) = temp_profile_store();
        ps.save(&make_user_profile("user123", "Test User")).unwrap();

        let identity = AuthIdentity::User {
            id: "user123".into(),
            role: UserRole::User,
        };
        let result = resolve_my_profile_id(&identity, &ps).unwrap();
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
        let (_dir, ps) = temp_profile_store();
        // Create user profile first — old code would return this
        ps.save(&make_user_profile("alice", "Alice")).unwrap();
        // Ensure admin profile exists
        ensure_admin_profile(&ps).unwrap();

        let identity = AuthIdentity::Admin;
        let profile = resolve_my_profile(&identity, &ps).unwrap();
        assert_eq!(profile.id, ADMIN_PROFILE_ID);
        assert_eq!(profile.name, "Admin");
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

    #[tokio::test]
    async fn scoped_send_code_rejects_wrong_email() {
        let (_dir, state, user_store, profile_store) = temp_app_state();
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

        let Json(resp) = send_code(
            State(Arc::new(state)),
            scoped_host_headers("tenant--assistant.example.test"),
            Json(SendCodeRequest {
                email: "wrong@example.com".into(),
            }),
        )
        .await
        .unwrap();

        assert!(!resp.ok);
        assert_eq!(
            resp.message.as_deref(),
            Some("This email is not registered for this account")
        );
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

// ---------------------------------------------------------------------------
// WeChat QR Login (user-scoped)
// ---------------------------------------------------------------------------

/// GET /api/my/profile/wechat/qr-start
pub async fn my_wechat_qr_start(
    State(state): State<Arc<AppState>>,
    axum::Extension(identity): axum::Extension<AuthIdentity>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    // Check if ProcessManager has a bridge with QR info
    if let Some(pm) = state.process_manager.as_ref() {
        let ps = state
            .profile_store
            .as_ref()
            .ok_or((StatusCode::SERVICE_UNAVAILABLE, "no profile store".into()))?;
        let profile_id = super::auth_handlers::resolve_my_profile_id(&identity, ps)
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
    axum::Extension(identity): axum::Extension<AuthIdentity>,
    Json(req): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let ps = state
        .profile_store
        .as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, "no profile store".into()))?;
    let profile_id = super::auth_handlers::resolve_my_profile_id(&identity, ps)
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
