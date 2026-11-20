//! Admin API handlers for profile and gateway management.

use std::path::{Path as FsPath, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use chrono::{DateTime, Utc};
use futures::StreamExt;
use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use super::router::AuthIdentity;
use super::{AppState, ominix_runtime};
use crate::profiles::{ProfileConfig, ProfileStore, UserProfile, mask_secrets};

const DEFAULT_SERVE_LOG_TAIL_N: usize = 200;
const MAX_SERVE_LOG_TAIL_N: usize = 5_000;
const SERVE_LOG_POLL_INTERVAL: Duration = Duration::from_secs(1);

static SERVE_LOG_BEARER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bBearer\s+[A-Za-z0-9_.+/=-]{12,}").unwrap());
static SERVE_LOG_QUERY_TOKEN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)((?:[?&]|\b)(?:session_ingress_token|auth_token|_token|token)=)[^&\s]+")
        .unwrap()
});
static SERVE_LOG_API_KEY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\b(?:sk-[A-Za-z0-9_-]{12,}|ghp_[A-Za-z0-9_]{12,}|github_pat_[A-Za-z0-9_]+|glpat-[A-Za-z0-9_-]{12,}|AKIA[0-9A-Z]{16})\b",
    )
    .unwrap()
});

/// Basic email format validation.
pub(crate) fn validate_email(email: &str) -> Result<(), String> {
    if email.len() > 254 {
        return Err("Email address too long (max 254 chars)".into());
    }
    let parts: Vec<&str> = email.splitn(2, '@').collect();
    if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() || !parts[1].contains('.') {
        return Err(format!("Invalid email format: {email}"));
    }
    Ok(())
}

// ── Request / Response types ──────────────────────────────────────────

const MODEL_BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(60 * 60 * 2);
const MODEL_BOOTSTRAP_SPACE_MARGIN_BYTES: u64 = 1024 * 1024 * 1024;

#[derive(Debug, Serialize)]
struct OminixModelBootstrapResult {
    id: String,
    role: String,
    ready: bool,
    action: String,
    status_before: String,
    status_after: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    size: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

#[derive(Deserialize)]
pub struct CreateProfileRequest {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub public_subdomain: Option<String>,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub data_dir: Option<String>,
    #[serde(default)]
    pub config: ProfileConfig,
}

#[derive(Deserialize)]
pub struct UpdateProfileRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub public_subdomain: Option<Option<String>>,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub data_dir: Option<Option<String>>,
    /// Parsed as opaque JSON on purpose: the typed round-trip happens in
    /// `merge_profile_config_from_body` *after* the raw patch is merged over
    /// the stored config (an invalid merged result is a 400), so a partial
    /// nested-section patch (e.g. `{"email":{"smtp_host":…}}` without the
    /// required `provider`) must still parse here (#1470).
    #[serde(default)]
    pub config: Option<serde_json::Value>,
    /// Set or update the email address for OTP login.
    #[serde(default)]
    pub email: Option<String>,
}

#[derive(Serialize)]
pub struct ProfileResponse {
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<super::ui_protocol_transport::ProfileRuntimeStatus>,
    #[serde(flatten)]
    pub profile: UserProfile,
    pub status: crate::process_manager::ProcessStatus,
    /// Login email address (from UserStore).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

impl ProfileResponse {
    pub fn from(profile: UserProfile, status: crate::process_manager::ProcessStatus) -> Self {
        Self {
            profile,
            status,
            email: None,
            runtime: None,
        }
    }
    pub(crate) fn with_runtime_transition(
        mut self,
        transition: Option<super::ui_protocol_transport::ProfileLlmRuntimeTransition>,
    ) -> Self {
        self.runtime = transition.map(|transition| transition.wire_status());
        self
    }
    pub fn with_email_lookup(mut self, user_store: Option<&crate::user_store::UserStore>) -> Self {
        self.email = user_store
            .and_then(|us| us.get(&self.profile.id).ok().flatten())
            .map(|u| u.email);
        self
    }
}

#[derive(Serialize)]
pub struct OverviewResponse {
    pub total_profiles: usize,
    pub running: usize,
    pub stopped: usize,
    pub profiles: Vec<ProfileResponse>,
}

#[derive(Serialize)]
pub struct ActionResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Serialize)]
pub struct BulkActionResponse {
    pub ok: bool,
    pub count: usize,
}

// ── Handlers ──────────────────────────────────────────────────────────

/// GET /api/admin/overview
pub async fn overview(
    State(state): State<Arc<AppState>>,
) -> Result<Json<OverviewResponse>, (StatusCode, String)> {
    let profiles = state
        .profile_store
        .as_ref()
        .ok_or((
            StatusCode::SERVICE_UNAVAILABLE,
            "admin not configured".into(),
        ))?
        .list()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let pm = state.process_manager.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;

    let mut running = 0;
    let mut items = Vec::with_capacity(profiles.len());
    for p in profiles {
        let status = pm.status(&p.id).await;
        if status.running {
            running += 1;
        }
        items.push(ProfileResponse {
            runtime: None,
            email: None,
            profile: mask_secrets(&p),
            status,
        });
    }

    let total = items.len();
    Ok(Json(OverviewResponse {
        total_profiles: total,
        running,
        stopped: total - running,
        profiles: items,
    }))
}

#[derive(Deserialize, Default)]
pub struct PaginationParams {
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

/// GET /api/admin/profiles
pub async fn list_profiles(
    State(state): State<Arc<AppState>>,
    Query(pagination): Query<PaginationParams>,
) -> Result<Json<Vec<ProfileResponse>>, (StatusCode, String)> {
    let store = state.profile_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;
    let pm = state.process_manager.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;

    let profiles = store
        .list()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let offset = pagination.offset.unwrap_or(0);
    let limit = pagination.limit.unwrap_or(100);
    let page = profiles
        .into_iter()
        .skip(offset)
        .take(limit)
        .collect::<Vec<_>>();

    let mut items = Vec::with_capacity(page.len());
    for p in page {
        let status = pm.status(&p.id).await;
        items.push(ProfileResponse {
            runtime: None,
            email: None,
            profile: mask_secrets(&p),
            status,
        });
    }
    Ok(Json(items))
}

/// GET /api/admin/profiles/:id
pub async fn get_profile(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<ProfileResponse>, (StatusCode, String)> {
    let store = state.profile_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;
    let pm = state.process_manager.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;

    let profile = store
        .get(&id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, format!("profile '{id}' not found")))?;

    let status = pm.status(&id).await;
    Ok(Json(
        ProfileResponse {
            runtime: None,
            email: None,
            profile: mask_secrets(&profile),
            status,
        }
        .with_email_lookup(state.user_store.as_deref()),
    ))
}

/// Move any keychain-backed secrets (e.g. the Vertex service-account JSON, which
/// carries a private key) out of plaintext profile config and into the OS
/// keychain. Shared by every profile/sub-account create/update save path so they
/// all uphold the same keychain-only contract as `PUT /api/my/profile`. A raw
/// secret on a non-macOS host is rejected rather than silently persisted.
///
/// Detection is by **content** (a raw service-account JSON), not just the
/// declared `VERTEX_SA_JSON` name — so a private key pasted under a custom env
/// var (e.g. a dashboard "Custom" provider deriving `VERTEX_API_KEY`) can't slip
/// past into plaintext config.
///
/// Relocation is per key and short-circuits on the first failure: keys
/// already relocated stay in the keychain when a later key fails. Those
/// entries are scoped per profile id, so they cannot leak across accounts,
/// and a same-id retry (after the caller's profile rollback) overwrites
/// them (#2316); the admin API deletion paths release what relocation left
/// behind (the CLI sub-account deletions still don't — #2702).
pub(crate) fn relocate_keychain_backed_secrets(
    env_vars: &mut std::collections::HashMap<String, String>,
    profile_id: &str,
) -> Result<(), (StatusCode, String)> {
    let keys: Vec<String> = env_vars
        .iter()
        .filter(|(key, value)| crate::auth::keychain::needs_keychain_relocation(key, value))
        .map(|(key, _)| key.clone())
        .collect();
    for key in keys {
        super::auth_handlers::relocate_secret_to_keychain(
            env_vars,
            &key,
            profile_id,
            crate::auth::keychain::is_available(),
            crate::auth::keychain::set_secret,
        )
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    }
    Ok(())
}

/// POST /api/admin/profiles
pub async fn create_profile(
    identity: Option<axum::Extension<AuthIdentity>>,
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateProfileRequest>,
) -> Result<(StatusCode, Json<ProfileResponse>), (StatusCode, String)> {
    let store = state.profile_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;
    let pm = state.process_manager.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;

    // Check for duplicates
    if store
        .get(&req.id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .is_some()
    {
        return Err((
            StatusCode::CONFLICT,
            format!("profile '{}' already exists", req.id),
        ));
    }

    let now = Utc::now();
    let mut profile = UserProfile {
        id: req.id,
        name: req.name,
        public_subdomain: req
            .public_subdomain
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty()),
        enabled: req.enabled,
        data_dir: req.data_dir,
        parent_id: None,
        config: req.config,
        created_at: now,
        updated_at: now,
    };

    // Relocate freshly-entered keychain-backed secrets (e.g. the Vertex SA
    // JSON, which carries a private key) out of plaintext config and into the
    // OS keychain before persisting — same contract as `PUT /api/my/profile`.
    let profile_id = profile.id.clone();
    relocate_keychain_backed_secrets(&mut profile.config.env_vars, &profile_id)?;

    store.save(&profile).map_err(|e| {
        tracing::error!(profile = %profile.id, error = %e, "failed to create profile");
        (StatusCode::BAD_REQUEST, e.to_string())
    })?;

    tracing::info!(profile = %profile.id, name = %profile.name, "profile created");
    let status = pm.status(&profile.id).await;
    let response = ProfileResponse {
        runtime: None,
        email: None,
        profile: mask_secrets(&profile),
        status,
    };
    super::admin_audit::record_admin_action(
        &state,
        identity.as_ref().map(|identity| &identity.0),
        "profile.create",
        profile.id.clone(),
        None,
        super::admin_audit::summary_value(&response.profile),
    )
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok((StatusCode::CREATED, Json(response)))
}

/// PUT /api/admin/profiles/:id
pub async fn update_profile(
    identity: Option<axum::Extension<AuthIdentity>>,
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: String,
) -> Result<Json<ProfileResponse>, (StatusCode, String)> {
    let req: UpdateProfileRequest = serde_json::from_str(&body).map_err(|e| {
        tracing::warn!(profile_id = %id, error = %e, body = %body, "failed to parse profile update request");
        (StatusCode::BAD_REQUEST, format!("Invalid request body: {e}"))
    })?;
    let store = state.profile_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;
    let pm = state.process_manager.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;

    let mut profile = store
        .get(&id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, format!("profile '{id}' not found")))?;
    let before_profile = profile.clone();

    if let Some(name) = req.name {
        profile.name = name;
    }
    if let Some(public_subdomain) = req.public_subdomain {
        match public_subdomain
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            Some(slug) => profile.public_subdomain = Some(slug.to_string()),
            None => {
                if profile.parent_id.is_some() {
                    return Err((
                        StatusCode::BAD_REQUEST,
                        "sub-accounts must keep a public subdomain".into(),
                    ));
                }
                profile.public_subdomain = None;
            }
        }
    }
    if let Some(enabled) = req.enabled {
        profile.enabled = enabled;
    }
    if let Some(data_dir) = req.data_dir {
        profile.data_dir = data_dir;
    }
    merge_profile_config_from_body(&mut profile.config, &body, false)
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    // Relocate freshly-entered keychain-backed secrets (e.g. the Vertex SA
    // JSON) into the OS keychain before persisting, so an admin edit can't
    // write a private key into plaintext profile config.
    relocate_keychain_backed_secrets(&mut profile.config.env_vars, &id)?;
    profile.updated_at = Utc::now();

    store.save_with_merge(&mut profile).map_err(|e| {
        tracing::error!(profile = %id, error = %e, "failed to update profile");
        (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })?;

    // Update or create User entry for OTP login
    if let Some(email) = &req.email {
        let email = email.trim().to_lowercase();
        if !email.is_empty() {
            validate_email(&email).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
            if let Some(user_store) = state.user_store.as_ref() {
                // Check if email is taken by a different user
                if let Ok(Some(existing)) = user_store.get_by_email(&email) {
                    if existing.id != id {
                        return Err((
                            StatusCode::CONFLICT,
                            format!("Email '{email}' is already registered to another account"),
                        ));
                    }
                }
                let user = match user_store.get(&id) {
                    Ok(Some(mut u)) => {
                        u.email = email;
                        u.name = profile.name.clone();
                        u
                    }
                    _ => crate::user_store::User {
                        id: id.clone(),
                        email,
                        name: profile.name.clone(),
                        role: crate::user_store::UserRole::User,
                        created_at: Utc::now(),
                        last_login_at: None,
                    },
                };
                user_store
                    .save(&user)
                    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            }
        }
    }

    tracing::info!(profile = %id, "profile updated");
    let status = pm.status(&id).await;
    let response = ProfileResponse {
        runtime: None,
        email: None,
        profile: mask_secrets(&profile),
        status,
    };
    let before_summary = super::admin_audit::summary_value(&mask_secrets(&before_profile));
    let after_summary = super::admin_audit::summary_value(&response.profile);
    super::admin_audit::record_admin_action(
        &state,
        identity.as_ref().map(|identity| &identity.0),
        "profile.update",
        id,
        before_summary,
        after_summary,
    )
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(response))
}

// Lives in `crate::profiles` (it only needs the profile store, and the CLI
// and gateway deletion paths compile without this crate's `api` feature);
// re-exported so the API call sites keep their `crate::api::admin::` path.
pub(crate) use crate::profiles::release_deleted_profiles_keychain_items;

/// DELETE /api/admin/profiles/:id
pub async fn delete_profile(
    identity: Option<axum::Extension<AuthIdentity>>,
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<ActionResponse>, (StatusCode, String)> {
    let store = state.profile_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;
    let pm = state.process_manager.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;

    // Load the profile before deleting so we can clean up its data directory
    let profile = store
        .get(&id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Stop the gateway if running
    let _ = pm.stop(&id).await;

    // Cascade: stop and delete all sub-accounts
    let subs = store.list_sub_accounts(&id).unwrap_or_default();
    for sub in &subs {
        let _ = pm.stop(&sub.id).await;
        // Clean up sub-account data directory
        let sub_data_dir = store.resolve_data_dir(sub);
        if sub_data_dir.exists() {
            if let Err(e) = std::fs::remove_dir_all(&sub_data_dir) {
                tracing::warn!(profile = %sub.id, dir = %sub_data_dir.display(), error = %e, "failed to clean up sub-account data directory");
            }
        }
        let _ = store.delete(&sub.id);
    }

    let deleted = store
        .delete(&id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    if !deleted {
        return Err((StatusCode::NOT_FOUND, format!("profile '{id}' not found")));
    }

    release_deleted_profiles_keychain_items(store, profile.iter().chain(subs.iter()));

    let before_summary = profile
        .as_ref()
        .and_then(|profile| super::admin_audit::summary_value(&mask_secrets(profile)));

    // Clean up data directory
    if let Some(profile) = profile {
        let data_dir = store.resolve_data_dir(&profile);
        if data_dir.exists() {
            if let Err(e) = std::fs::remove_dir_all(&data_dir) {
                tracing::warn!(profile = %id, dir = %data_dir.display(), error = %e, "failed to clean up data directory");
            }
        }
    }

    tracing::info!(profile = %id, "profile deleted");
    super::admin_audit::record_admin_action(
        &state,
        identity.as_ref().map(|identity| &identity.0),
        "profile.delete",
        id.clone(),
        before_summary,
        None,
    )
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(ActionResponse {
        ok: true,
        message: Some(format!("profile '{id}' deleted")),
    }))
}

/// POST /api/admin/profiles/:id/start
pub async fn start_gateway(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<ActionResponse>, (StatusCode, String)> {
    let store = state.profile_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;
    let pm = state.process_manager.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;

    let profile = store
        .get(&id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, format!("profile '{id}' not found")))?;

    // Validate LLM provider is configured (resolve inheritance for sub-accounts)
    let effective = crate::profiles::resolve_effective_profile(store, &profile)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    if effective.config.primary_provider().is_none() && effective.config.primary_model().is_none() {
        return Err((
            StatusCode::BAD_REQUEST,
            "Cannot start: LLM provider must be configured first".into(),
        ));
    }

    // TODO(#147): Sub-account start has a validation gap compared to self-service start.
    // The self-service handler (auth_handlers::start_my_gateway) does not resolve
    // effective profile for inherited LLM config, while this admin handler does.
    // Both paths should also validate that channel credentials are properly configured
    // (e.g. required env vars exist) before starting the gateway process.

    if let Err(e) = pm.start(&profile).await {
        tracing::error!(profile = %id, error = %e, "admin gateway failed to start");
        return Err((StatusCode::CONFLICT, e.to_string()));
    }

    tracing::info!(profile = %id, "admin gateway started");
    Ok(Json(ActionResponse {
        ok: true,
        message: Some(format!("gateway '{id}' started")),
    }))
}

/// POST /api/admin/profiles/:id/stop
pub async fn stop_gateway(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<ActionResponse>, (StatusCode, String)> {
    let pm = state.process_manager.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;

    let stopped = pm
        .stop(&id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    if !stopped {
        tracing::warn!(profile = %id, "stop requested but gateway not running");
        return Err((
            StatusCode::NOT_FOUND,
            format!("gateway '{id}' is not running"),
        ));
    }

    tracing::info!(profile = %id, "admin gateway stopped");
    Ok(Json(ActionResponse {
        ok: true,
        message: Some(format!("gateway '{id}' stopped")),
    }))
}

/// POST /api/admin/profiles/:id/restart
pub async fn restart_gateway(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<ActionResponse>, (StatusCode, String)> {
    let store = state.profile_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;
    let pm = state.process_manager.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;

    let profile = store
        .get(&id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, format!("profile '{id}' not found")))?;

    if let Err(e) = pm.restart(&profile).await {
        tracing::error!(profile = %id, error = %e, "admin gateway failed to restart");
        return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string()));
    }

    tracing::info!(profile = %id, "admin gateway restarted");
    Ok(Json(ActionResponse {
        ok: true,
        message: Some(format!("gateway '{id}' restarted")),
    }))
}

/// GET /api/admin/profiles/:id/status
pub async fn gateway_status(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<crate::process_manager::ProcessStatus>, (StatusCode, String)> {
    let pm = state.process_manager.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;

    Ok(Json(pm.status(&id).await))
}

/// GET /api/admin/profiles/:id/metrics — Provider QoS metrics.
pub async fn provider_metrics(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let pm = state.process_manager.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;

    match pm.read_metrics(&id).await {
        Some(metrics) => Ok(Json(metrics)),
        None => Ok(Json(serde_json::json!(null))),
    }
}

/// GET /api/admin/profiles/:id/logs — SSE log stream.
pub async fn gateway_logs(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<
    Sse<impl futures::Stream<Item = Result<Event, std::convert::Infallible>>>,
    (StatusCode, String),
> {
    let pm = state.process_manager.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;

    // Get buffered history first, then subscribe for live logs.
    let history = pm.log_history(&id).await;
    let rx = pm.subscribe_logs(&id).await.ok_or((
        StatusCode::NOT_FOUND,
        format!("gateway '{id}' is not running"),
    ))?;

    // Emit history lines first, then stream live.
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

#[derive(Debug, Clone, Deserialize)]
pub struct ServeLogsQuery {
    #[serde(default)]
    tail_n: Option<usize>,
    #[serde(default)]
    grep: Option<String>,
    #[serde(default)]
    since: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
struct ServeLogFilter {
    grep: Option<Regex>,
    since: Option<DateTime<Utc>>,
}

impl ServeLogFilter {
    fn from_query(query: &ServeLogsQuery) -> Result<Self, String> {
        let grep = match query
            .grep
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(pattern) => {
                let anchored = format!("^(?:{pattern})$");
                Some(
                    RegexBuilder::new(&anchored)
                        .case_insensitive(true)
                        .build()
                        .map_err(|error| format!("invalid grep regex: {error}"))?,
                )
            }
            None => None,
        };
        Ok(Self {
            grep,
            since: query.since,
        })
    }
}

/// GET /api/admin/serve/logs — SSE stream of the main daemon log.
pub async fn serve_logs(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ServeLogsQuery>,
) -> Result<
    Sse<impl futures::Stream<Item = Result<Event, std::convert::Infallible>>>,
    (StatusCode, String),
> {
    let store = state.profile_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;
    let filter = ServeLogFilter::from_query(&query).map_err(|error| {
        (
            StatusCode::BAD_REQUEST,
            format!("invalid serve log query: {error}"),
        )
    })?;
    let tail_n = query
        .tail_n
        .unwrap_or(DEFAULT_SERVE_LOG_TAIL_N)
        .min(MAX_SERVE_LOG_TAIL_N);
    let octos_home = store.octos_home_dir().to_path_buf();
    let log_path = serve_log_path_for_now(&octos_home);

    let replay = read_serve_log_replay(&log_path, tail_n, &filter)
        .await
        .map_err(|error| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to read serve log: {error}"),
            )
        })?;
    let initial_offset = tokio::fs::metadata(&log_path)
        .await
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    let history_stream = futures::stream::iter(
        replay
            .into_iter()
            .map(|line| Ok(Event::default().data(line))),
    );
    let live_stream = futures::stream::unfold(
        ServeLogTailState {
            octos_home,
            path: log_path,
            offset: initial_offset,
            pending: String::new(),
            filter,
        },
        |mut state| async move {
            loop {
                tokio::time::sleep(SERVE_LOG_POLL_INTERVAL).await;
                state.refresh_path_for_rotation();
                match read_new_serve_log_lines(&mut state).await {
                    Ok(lines) if lines.is_empty() => continue,
                    Ok(lines) => {
                        let events = futures::stream::iter(
                            lines
                                .into_iter()
                                .map(|line| Ok(Event::default().data(line)))
                                .collect::<Vec<Result<Event, std::convert::Infallible>>>(),
                        );
                        return Some((events, state));
                    }
                    Err(error) => {
                        let event: Result<Event, std::convert::Infallible> = Ok(Event::default()
                            .event("error")
                            .data(format!("failed to read serve log: {error}")));
                        let events = futures::stream::iter(vec![event]);
                        return Some((events, state));
                    }
                }
            }
        },
    )
    .flatten();

    Ok(Sse::new(history_stream.chain(live_stream)).keep_alive(KeepAlive::default()))
}

#[derive(Debug, Clone)]
struct ServeLogTailState {
    octos_home: PathBuf,
    path: PathBuf,
    offset: u64,
    pending: String,
    filter: ServeLogFilter,
}

impl ServeLogTailState {
    fn refresh_path_for_rotation(&mut self) {
        let next_path = serve_log_path_for_now(&self.octos_home);
        if next_path != self.path {
            self.path = next_path;
            self.offset = 0;
            self.pending.clear();
        }
    }
}

fn serve_log_path_for_now(octos_home: &FsPath) -> PathBuf {
    serve_log_path_for_instant(octos_home, Utc::now())
}

/// tracing_appender's DAILY rotation names files by the UTC date
/// (`OffsetDateTime::now_utc()`), so the tail side must resolve the current
/// log path with the same clock — using `Local` would tail a nonexistent
/// `serve.<local-date>.log` for part of each day on non-UTC hosts.
fn serve_log_path_for_instant(octos_home: &FsPath, now: DateTime<Utc>) -> PathBuf {
    serve_log_path_for_date(octos_home, now.date_naive())
}

fn serve_log_path_for_date(octos_home: &FsPath, date: chrono::NaiveDate) -> PathBuf {
    octos_home
        .join("logs")
        .join(format!("serve.{}.log", date.format("%Y-%m-%d")))
}

async fn read_serve_log_replay(
    path: &FsPath,
    tail_n: usize,
    filter: &ServeLogFilter,
) -> std::io::Result<Vec<String>> {
    match tokio::fs::read_to_string(path).await {
        Ok(content) => Ok(filter_serve_log_lines(content.lines(), tail_n, filter)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}

async fn read_new_serve_log_lines(state: &mut ServeLogTailState) -> std::io::Result<Vec<String>> {
    let mut file = match tokio::fs::File::open(&state.path).await {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };

    let len = file.metadata().await?.len();
    if len < state.offset {
        state.offset = 0;
        state.pending.clear();
    }
    file.seek(std::io::SeekFrom::Start(state.offset)).await?;

    let mut chunk = String::new();
    file.read_to_string(&mut chunk).await?;
    state.offset += chunk.len() as u64;
    if chunk.is_empty() {
        return Ok(Vec::new());
    }

    let mut text = std::mem::take(&mut state.pending);
    text.push_str(&chunk);
    if !text.ends_with('\n') {
        if let Some((complete, pending)) = text.rsplit_once('\n') {
            state.pending = pending.to_string();
            text = complete.to_string();
        } else {
            state.pending = text;
            return Ok(Vec::new());
        }
    }

    Ok(text
        .lines()
        .filter(|line| serve_log_line_matches(line, &state.filter))
        .map(redact_serve_log_line)
        .collect())
}

fn filter_serve_log_lines<'a>(
    lines: impl Iterator<Item = &'a str>,
    tail_n: usize,
    filter: &ServeLogFilter,
) -> Vec<String> {
    let mut matches = lines
        .filter(|line| serve_log_line_matches(line, filter))
        .map(redact_serve_log_line)
        .collect::<Vec<_>>();
    if matches.len() > tail_n {
        matches.drain(0..matches.len() - tail_n);
    }
    matches
}

fn serve_log_line_matches(line: &str, filter: &ServeLogFilter) -> bool {
    if let Some(since) = filter.since {
        if let Some(timestamp) = serve_log_line_timestamp(line) {
            if timestamp < since {
                return false;
            }
        }
    }
    filter.grep.as_ref().is_none_or(|grep| grep.is_match(line))
}

fn serve_log_line_timestamp(line: &str) -> Option<DateTime<Utc>> {
    let token = line.split_whitespace().next()?;
    DateTime::parse_from_rfc3339(token)
        .ok()
        .map(|timestamp| timestamp.with_timezone(&Utc))
}

fn redact_serve_log_line(line: &str) -> String {
    let after_bearer = SERVE_LOG_BEARER_RE.replace_all(line, "Bearer [credential-redacted]");
    let after_query_token =
        SERVE_LOG_QUERY_TOKEN_RE.replace_all(&after_bearer, "${1}[credential-redacted]");
    SERVE_LOG_API_KEY_RE
        .replace_all(&after_query_token, "[credential-redacted]")
        .into_owned()
}

/// GET /api/admin/profiles/:id/whatsapp/qr
pub async fn whatsapp_qr(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<crate::process_manager::BridgeQrInfo>, (StatusCode, String)> {
    let pm = state.process_manager.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;

    let info = pm.bridge_qr(&id).await.ok_or((
        StatusCode::NOT_FOUND,
        format!("no managed WhatsApp bridge for '{id}'"),
    ))?;

    Ok(Json(info))
}

/// POST /api/admin/test-provider or /api/my/test-provider
///
/// Verify an LLM provider/model/key combo works. Accepts either:
/// - `api_key`: raw key (for newly entered, unsaved keys)
/// - `api_key_env`: env var name to resolve from the user's saved profile
///   (used when the key is already saved and the frontend only has the masked value)
pub async fn test_provider(
    State(state): State<Arc<AppState>>,
    identity: Option<axum::Extension<super::router::AuthIdentity>>,
    Json(req): Json<TestProviderRequest>,
) -> Result<Json<TestProviderResponse>, (StatusCode, String)> {
    use octos_core::{Message, MessageRole};
    use octos_llm::{ChatConfig, LlmProvider};

    // Resolve the API key: prefer raw api_key, fall back to reading from saved profile
    let keyless = octos_llm::registry::is_keyless(&req.provider);
    let resolved = if let Some(ref key) = req.api_key {
        if !key.is_empty() && !key.contains("***") {
            Ok(key.clone())
        } else {
            resolve_saved_key(&state, &identity, &req)
        }
    } else {
        resolve_saved_key(&state, &identity, &req)
    };
    // Keyless local families (local/ollama/vllm) construct without a key —
    // both an EMPTY key and an UNRESOLVABLE key (no api_key/api_key_env in
    // the request at all) are fine for them. Dead-ending here blocked the
    // keyless onboarding flow entirely (red-team pass + live test).
    let api_key = match resolved {
        Ok(key) => key,
        Err(_) if keyless => String::new(),
        Err(error) => return Err(error),
    };

    if api_key.is_empty() && !keyless {
        return Ok(Json(TestProviderResponse {
            ok: false,
            message: String::new(),
            error: Some("No API key provided".into()),
        }));
    }

    // Link-local (cloud metadata) targets are never model servers — refuse
    // before any outbound request (adversarial review, octos#2097).
    if req
        .base_url
        .as_deref()
        .is_some_and(base_url_targets_link_local)
    {
        return Ok(Json(TestProviderResponse {
            ok: false,
            message: String::new(),
            error: Some("base_url targets a link-local/metadata address — refused".into()),
        }));
    }

    let provider: Arc<dyn LlmProvider> = {
        let params = octos_llm::registry::CreateParams {
            // Empty means "keyless family" — let the factory apply its own
            // fallback instead of sending an empty Bearer token.
            api_key: (!api_key.is_empty()).then(|| api_key.clone()),
            model: Some(req.model.clone()),
            base_url: req.base_url.clone(),
            model_hints: None,
            llm_timeout_secs: None,
            llm_connect_timeout_secs: None,
        };
        match octos_llm::registry::lookup(&req.provider) {
            Some(entry) => (entry.create)(params)
                .map_err(|e| (StatusCode::BAD_REQUEST, format!("provider error: {e:#}")))?,
            None => {
                // Unknown provider — assume OpenAI-compatible with custom base URL.
                let url = req
                    .base_url
                    .as_deref()
                    .unwrap_or("https://api.openai.com/v1");
                Arc::new(
                    octos_llm::openai::OpenAIProvider::new(&api_key, &req.model)
                        .with_base_url(url)
                        .with_provider_label(&req.provider),
                )
            }
        }
    };

    let messages = vec![Message {
        role: MessageRole::User,
        content: "Say OK".into(),
        media: vec![],
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: None,
        client_message_id: None,
        thread_id: None,
        timestamp: chrono::Utc::now(),
        source: None,
        participant: None,
    }];
    // Gemini 2.5+ "thinking" models consume tokens on internal reasoning,
    // so 16 tokens is too small — they return empty content.  Use 128 for
    // Gemini and keep 16 for everyone else (fast, cheap connectivity check).
    // Resolve aliases through the registry: the web settings UI historically
    // sends `google`, which is the registered alias for `gemini`. Treating the
    // alias as an unrelated provider left the connectivity probe with only 16
    // output tokens and caused thinking-capable Gemini models to return a
    // truncated/empty candidate that was then reported as a connection error.
    let canonical_provider = octos_llm::registry::lookup(&req.provider)
        .map(|entry| entry.name)
        .unwrap_or(req.provider.as_str());
    let max_tokens = if canonical_provider == "gemini" || canonical_provider == "vertex" {
        128
    } else {
        16
    };
    let config = ChatConfig {
        max_tokens: Some(max_tokens),
        temperature: Some(0.0),
        ..Default::default()
    };

    match tokio::time::timeout(
        std::time::Duration::from_secs(30),
        provider.chat(&messages, &[], &config),
    )
    .await
    {
        Ok(Ok(resp)) => {
            tracing::info!(provider = %req.provider, model = %req.model, "test-provider succeeded");
            Ok(Json(TestProviderResponse {
                ok: true,
                message: resp.content.unwrap_or_default(),
                error: None,
            }))
        }
        Ok(Err(e)) => {
            tracing::warn!(provider = %req.provider, model = %req.model, error = %e, "test-provider failed");
            Ok(Json(TestProviderResponse {
                ok: false,
                message: String::new(),
                error: Some(format!("{e:#}")),
            }))
        }
        Err(_) => {
            tracing::warn!(provider = %req.provider, model = %req.model, "test-provider timed out");
            Ok(Json(TestProviderResponse {
                ok: false,
                message: String::new(),
                error: Some("Request timed out after 30 seconds".into()),
            }))
        }
    }
}

/// POST /api/my/provider-models — fetch available models from a provider's API.
pub async fn provider_models(
    State(state): State<Arc<AppState>>,
    identity: Option<axum::Extension<super::router::AuthIdentity>>,
    Json(req): Json<TestProviderRequest>,
) -> Result<Json<Vec<String>>, (StatusCode, String)> {
    let keyless = octos_llm::registry::is_keyless(&req.provider);
    let resolved = if let Some(ref key) = req.api_key {
        if !key.is_empty() && !key.contains("***") {
            Ok(key.clone())
        } else {
            resolve_saved_key(&state, &identity, &req)
        }
    } else {
        resolve_saved_key(&state, &identity, &req)
    };
    // Keyless local families (local/ollama/vllm) list models without a key —
    // their /v1/models answers unauthenticated (octos#2096 review round).
    let api_key = match resolved {
        Ok(key) => key,
        Err(_) if keyless => String::new(),
        Err(error) => return Err(error),
    };
    if api_key.is_empty() && !keyless {
        return Err((StatusCode::BAD_REQUEST, "No API key".into()));
    }
    // Link-local (cloud metadata) targets are never model servers — refuse
    // before any outbound request (adversarial review, octos#2097).
    if req
        .base_url
        .as_deref()
        .is_some_and(base_url_targets_link_local)
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "base_url targets a link-local/metadata address".into(),
        ));
    }
    // Protocol-aware discovery shared with the AppUI `profile/llm/
    // fetch_models` surface — the strategy resolves from the route (api_type
    // override, then the family's declared protocol — per-model for families
    // like r9s that pick the wire protocol by model name), never from the
    // literal family id, so the two clients cannot drift.
    let route = octos_llm::discovery::resolve_model_discovery(
        Some(&req.provider),
        req.api_type.as_deref(),
        (!req.model.trim().is_empty()).then_some(req.model.trim()),
        req.base_url.as_deref(),
    );
    let outcome = octos_llm::discovery::discover_models(
        &route,
        &api_key,
        req.base_url.as_deref(),
        Some(&req.provider),
    )
    .await;
    match outcome {
        // Success — including an empty catalog, which is data, not an error.
        octos_llm::discovery::DiscoveryOutcome::Discovered(models) => Ok(Json(models)),
        // Advisory: this family has no model-list endpoint. Not an error —
        // manual model-id entry, Test, and Save stay fully available, and the
        // dashboard treats an empty list as "nothing to suggest".
        octos_llm::discovery::DiscoveryOutcome::Unsupported(_) => Ok(Json(Vec::new())),
        other => Err((
            if other.status_label() == "rate_limited" {
                StatusCode::TOO_MANY_REQUESTS
            } else {
                StatusCode::BAD_GATEWAY
            },
            format!(
                "{}: {}",
                other.status_label(),
                other.message().unwrap_or_default()
            ),
        )),
    }
}

/// Fetch available models from a provider's /v1/models endpoint.
/// Whether a caller-supplied `base_url` targets a LINK-LOCAL address —
/// 169.254/16 (cloud metadata endpoints like 169.254.169.254), fe80::/10, or
/// their IPv4-mapped forms. These are never legitimate model-server addresses,
/// while loopback and RFC1918 ARE (local/ollama/vllm servers), so this is
/// deliberately narrower than the agent-side `tools/ssrf.rs` checker (which
/// blocks loopback too and would break the local family). Literal-IP check
/// only: a hostname passes (the probe endpoints require auth; this closes the
/// sharpest credential-theft target, not every probe vector).
fn base_url_targets_link_local(base_url: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(base_url) else {
        return false;
    };
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(v4)) => v4.is_link_local(),
        Ok(std::net::IpAddr::V6(v6)) => {
            (v6.segments()[0] & 0xffc0) == 0xfe80
                || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_link_local())
        }
        Err(_) => false,
    }
}

/// Merge a request body's `config` object into an existing profile config.
///
/// Only keys present in `config` are overwritten; absent keys are preserved.
/// This lets callers send `{"config":{"model":"x"}}` without wiping
/// channels/env_vars, while dashboards can still send a full config object.
/// Merge a `{ "config": {...} }` request body into `config`. RFC-7396 semantics:
/// keys the client omits are preserved. `env_vars_authoritative` selects how a
/// provided `env_vars` map is treated:
/// - `true` — self-service `/api/my/*`, where the dashboard sends the COMPLETE
///   desired map (including `{}` to clear): replace `env_vars` wholesale so the
///   client can drop keys / clear secrets (a deep-merge can only add, never
///   remove).
/// - `false` — admin tool `admin_update_profile`, which sends only the keys the
///   operator supplied: deep-merge `env_vars` so a partial update never deletes
///   unrelated secrets.
///
/// Either way, masked/empty display values are restored per-key downstream by
/// `ProfileStore::save_with_merge`, so round-tripping masked secrets does not
/// lose them.
pub(crate) fn merge_profile_config_from_body(
    config: &mut ProfileConfig,
    body: &str,
    env_vars_authoritative: bool,
) -> Result<(), String> {
    let raw: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
    let Some(config_patch) = raw.get("config") else {
        return Ok(());
    };
    if config_patch.is_null() {
        return Ok(());
    }
    if !config_patch.is_object() {
        return Err("config must be an object".into());
    }
    let mut existing = serde_json::to_value(&mut *config).unwrap_or(serde_json::json!({}));
    json_merge(&mut existing, config_patch);
    if env_vars_authoritative {
        if let Some(env_vars) = config_patch.get("env_vars").filter(|v| v.is_object()) {
            if let Some(existing_obj) = existing.as_object_mut() {
                existing_obj.insert("env_vars".to_string(), env_vars.clone());
            }
        }
    }
    // The request body parses `config` as opaque JSON (see UpdateProfileRequest),
    // so this typed round-trip is the only validation gate: an invalid merged
    // result must surface as an error instead of silently dropping the patch.
    match serde_json::from_value(existing) {
        Ok(merged) => {
            *config = merged;
            Ok(())
        }
        Err(e) => Err(format!("invalid config: {e}")),
    }
}

/// Recursively merge `patch` into `target` (RFC 7396 JSON Merge Patch).
/// Only keys present in `patch` are overwritten; absent keys are preserved.
fn json_merge(target: &mut serde_json::Value, patch: &serde_json::Value) {
    if let (Some(target_obj), Some(patch_obj)) = (target.as_object_mut(), patch.as_object()) {
        for (key, value) in patch_obj {
            if value.is_object() && target_obj.get(key).is_some_and(|v| v.is_object()) {
                // Recursively merge nested objects (e.g. gateway settings)
                json_merge(target_obj.get_mut(key).unwrap(), value);
            } else {
                target_obj.insert(key.clone(), value.clone());
            }
        }
    } else {
        *target = patch.clone();
    }
}

/// Resolve an API key from the user's saved profile by env var name.
fn resolve_saved_key(
    state: &AppState,
    identity: &Option<axum::Extension<super::router::AuthIdentity>>,
    req: &TestProviderRequest,
) -> Result<String, (StatusCode, String)> {
    let env_name = match &req.api_key_env {
        Some(name) if !name.is_empty() => name,
        _ => {
            return Err((
                StatusCode::BAD_REQUEST,
                "No api_key or api_key_env provided".into(),
            ));
        }
    };

    // Get the user's profile from the store
    let ps = state.profile_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "profile store not configured".into(),
    ))?;

    let profile_id = if let Some(ref pid) = req.profile_id {
        pid.clone()
    } else {
        match identity {
            Some(axum::Extension(super::router::AuthIdentity::User { id, .. })) => id.clone(),
            Some(axum::Extension(super::router::AuthIdentity::Admin)) => {
                super::auth_handlers::ADMIN_PROFILE_ID.into()
            }
            None => {
                return Err((StatusCode::UNAUTHORIZED, "not authenticated".into()));
            }
        }
    };

    let profile = ps
        .get(&profile_id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "profile not found".into()))?;

    let raw = profile
        .config
        .env_vars
        .get(env_name)
        .cloned()
        .unwrap_or_default();
    // Resolve a `keychain:` marker to the real secret (e.g. a Vertex SA JSON
    // stored in the OS keychain); plain values pass through unchanged.
    Ok(crate::auth::keychain::resolve_value(env_name, &raw).unwrap_or_default())
}

#[derive(Deserialize)]
pub struct TestProviderRequest {
    pub provider: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    /// Route protocol override for model discovery (`"anthropic"` switches the
    /// listing strategy); absent means the family's declared protocol.
    #[serde(default)]
    pub api_type: Option<String>,
    #[serde(default)]
    pub profile_id: Option<String>,
}

#[derive(Serialize)]
pub struct TestProviderResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// POST /api/my/test-search
///
/// Verify a web search API key works. Makes a minimal search request.
pub async fn test_search(
    State(state): State<Arc<AppState>>,
    identity: Option<axum::Extension<super::router::AuthIdentity>>,
    Json(req): Json<TestSearchRequest>,
) -> Result<Json<TestSearchResponse>, (StatusCode, String)> {
    // Resolve the API key
    let api_key = if let Some(ref key) = req.api_key {
        if !key.is_empty() && !key.contains("***") {
            key.clone()
        } else {
            resolve_saved_search_key(&state, &identity, &req)?
        }
    } else {
        resolve_saved_search_key(&state, &identity, &req)?
    };

    if api_key.is_empty() {
        return Ok(Json(TestSearchResponse {
            ok: false,
            message: String::new(),
            error: Some("No API key provided".into()),
        }));
    }

    let client = reqwest::Client::new();
    let query = "test";

    let result = match req.provider.as_str() {
        "tavily" => {
            let body = serde_json::json!({
                "query": query,
                "max_results": 1,
                "include_answer": false,
            });
            let resp = client
                .post("https://api.tavily.com/search")
                .header("Content-Type", "application/json")
                .header("Authorization", format!("Bearer {api_key}"))
                .json(&body)
                .send()
                .await
                .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
            if resp.status().is_success() {
                Ok("Tavily Search API connected successfully".to_string())
            } else {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                Err(format!("Tavily API error ({status}): {body}"))
            }
        }
        "perplexity" => {
            let body = serde_json::json!({
                "model": "sonar",
                "messages": [{"role": "user", "content": query}],
                "max_tokens": 32
            });
            let resp = client
                .post("https://api.perplexity.ai/chat/completions")
                .header("Authorization", format!("Bearer {api_key}"))
                .header("Content-Type", "application/json")
                .json(&body)
                .send()
                .await
                .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
            if resp.status().is_success() {
                Ok("Perplexity Sonar API connected successfully".to_string())
            } else {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                Err(format!("Perplexity API error ({status}): {body}"))
            }
        }
        "brave" => {
            let resp = client
                .get("https://api.search.brave.com/res/v1/web/search")
                .header("X-Subscription-Token", &api_key)
                .header("Accept", "application/json")
                .query(&[("q", query), ("count", "1")])
                .send()
                .await
                .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
            if resp.status().is_success() {
                Ok("Brave Search API connected successfully".to_string())
            } else {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                Err(format!("Brave Search API error ({status}): {body}"))
            }
        }
        "you" => {
            let resp = client
                .get("https://ydc-index.io/v1/search")
                .header("X-API-Key", &api_key)
                .query(&[("query", query), ("count", "1")])
                .send()
                .await
                .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
            if resp.status().is_success() {
                Ok("You.com Search API connected successfully".to_string())
            } else {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                Err(format!("You.com API error ({status}): {body}"))
            }
        }
        "serper" => {
            let body = serde_json::json!({
                "q": query,
                "num": 1,
            });
            let resp = client
                .post("https://google.serper.dev/search")
                .header("X-API-KEY", &api_key)
                .header("Content-Type", "application/json")
                .json(&body)
                .send()
                .await
                .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
            if resp.status().is_success() {
                Ok("Serper Search API connected successfully".to_string())
            } else {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                Err(format!("Serper API error ({status}): {body}"))
            }
        }
        other => {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("Unknown search provider: {other}"),
            ));
        }
    };

    match result {
        Ok(msg) => Ok(Json(TestSearchResponse {
            ok: true,
            message: msg,
            error: None,
        })),
        Err(err) => Ok(Json(TestSearchResponse {
            ok: false,
            message: String::new(),
            error: Some(err),
        })),
    }
}

fn default_search_api_env(provider: &str) -> Option<&'static str> {
    match provider {
        "tavily" => Some("TAVILY_API_KEY"),
        "perplexity" => Some("PERPLEXITY_API_KEY"),
        "brave" => Some("BRAVE_API_KEY"),
        "you" => Some("YDC_API_KEY"),
        "serper" => Some("SERPER_API_KEY"),
        _ => None,
    }
}

fn resolve_profile_secret_with_keychain<F>(
    env_name: &str,
    stored_value: Option<&str>,
    mut keychain_lookup: F,
) -> Option<String>
where
    F: FnMut(&str) -> Option<String>,
{
    let secret = match stored_value {
        Some(value) if value == crate::auth::KEYCHAIN_MARKER => keychain_lookup(env_name),
        Some(value) if !value.trim().is_empty() => Some(value.to_string()),
        _ => None,
    };

    secret
        .or_else(|| std::env::var(env_name).ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn resolve_profile_secret(env_name: &str, stored_value: Option<&str>) -> Option<String> {
    resolve_profile_secret_with_keychain(env_name, stored_value, |name| {
        crate::auth::keychain::get_secret(name).ok().flatten()
    })
}

fn resolve_saved_search_key(
    state: &AppState,
    identity: &Option<axum::Extension<super::router::AuthIdentity>>,
    req: &TestSearchRequest,
) -> Result<String, (StatusCode, String)> {
    let env_name = match req
        .api_key_env
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .or_else(|| default_search_api_env(req.provider.as_str()))
    {
        Some(name) => name,
        _ => {
            return Err((
                StatusCode::BAD_REQUEST,
                "No api_key or api_key_env provided".into(),
            ));
        }
    };

    let ps = state.profile_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "profile store not configured".into(),
    ))?;

    let profile_id = resolve_test_search_profile_id(identity, req.profile_id.as_deref())?;

    let profile = ps
        .get(&profile_id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "profile not found".into()))?;

    let stored = profile.config.env_vars.get(env_name).map(String::as_str);
    Ok(resolve_profile_secret(env_name, stored).unwrap_or_default())
}

#[derive(Deserialize)]
pub struct TestSearchRequest {
    /// Search provider: "tavily", "perplexity", "brave", "you", "serper"
    pub provider: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Optional profile id whose saved env vars should be used.
    ///
    /// Admin dashboard pages use this when testing a non-admin profile. Regular
    /// users may only reference their own profile or child profiles.
    #[serde(default)]
    pub profile_id: Option<String>,
}

fn resolve_test_search_profile_id(
    identity: &Option<axum::Extension<super::router::AuthIdentity>>,
    requested_profile_id: Option<&str>,
) -> Result<String, (StatusCode, String)> {
    let requested_profile_id = requested_profile_id
        .map(str::trim)
        .filter(|id| !id.is_empty());

    match identity {
        Some(axum::Extension(super::router::AuthIdentity::Admin)) => Ok(requested_profile_id
            .unwrap_or(super::auth_handlers::ADMIN_PROFILE_ID)
            .to_string()),
        Some(axum::Extension(super::router::AuthIdentity::User { id, .. })) => {
            let Some(requested) = requested_profile_id else {
                return Ok(id.clone());
            };
            let child_prefix = format!("{id}--");
            if requested == id || requested.starts_with(&child_prefix) {
                Ok(requested.to_string())
            } else {
                Err((
                    StatusCode::FORBIDDEN,
                    "cannot test search keys for another profile".into(),
                ))
            }
        }
        None => Err((StatusCode::UNAUTHORIZED, "not authenticated".into())),
    }
}

#[derive(Serialize)]
pub struct TestSearchResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// POST /api/admin/start-all
pub async fn start_all(
    State(state): State<Arc<AppState>>,
) -> Result<Json<BulkActionResponse>, (StatusCode, String)> {
    let store = state.profile_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;
    let pm = state.process_manager.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;

    let profiles = store
        .list()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    tracing::info!("start-all requested");
    let mut started = 0;
    for p in &profiles {
        if p.enabled {
            match pm.start(p).await {
                Ok(()) => started += 1,
                Err(e) => tracing::warn!(profile = %p.id, error = %e, "start-all: failed to start"),
            }
        }
    }

    tracing::info!(count = started, "start-all completed");
    Ok(Json(BulkActionResponse {
        ok: true,
        count: started,
    }))
}

/// POST /api/admin/stop-all
pub async fn stop_all(
    State(state): State<Arc<AppState>>,
) -> Result<Json<BulkActionResponse>, (StatusCode, String)> {
    let pm = state.process_manager.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;

    tracing::info!("stop-all requested");
    let count = pm.stop_all().await;
    tracing::info!(count = count, "stop-all completed");
    Ok(Json(BulkActionResponse { ok: true, count }))
}

// ── Sub-account endpoints ────────────────────────────────────────────

#[derive(Deserialize)]
pub struct CreateSubAccountRequest {
    pub sub_account_id: String,
    pub name: String,
    pub public_subdomain: String,
    /// Optional email address for OTP login to the web client.
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub channels: Vec<crate::profiles::ChannelCredentials>,
    #[serde(default)]
    pub gateway: Option<crate::profiles::GatewaySettings>,
    #[serde(default)]
    pub env_vars: std::collections::HashMap<String, String>,
}

/// GET /api/admin/profiles/:id/accounts — List sub-accounts for a profile.
pub async fn list_sub_accounts(
    State(state): State<Arc<AppState>>,
    Path(parent_id): Path<String>,
) -> Result<Json<Vec<ProfileResponse>>, (StatusCode, String)> {
    let store = state.profile_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;
    let pm = state.process_manager.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;

    let subs = store
        .list_sub_accounts(&parent_id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let mut items = Vec::with_capacity(subs.len());
    for s in subs {
        let status = pm.status(&s.id).await;
        items.push(ProfileResponse {
            runtime: None,
            email: None,
            profile: mask_secrets(&s),
            status,
        });
    }
    Ok(Json(items))
}

/// Validate that channel credentials have the required fields populated.
/// Returns an error message if any channel is missing required fields.
pub(crate) fn validate_channel_credentials(
    channels: &[crate::profiles::ChannelCredentials],
) -> Result<(), String> {
    use crate::profiles::ChannelCredentials;
    for ch in channels {
        #[allow(clippy::collapsible_match)]
        match ch {
            ChannelCredentials::Telegram { token_env, .. } => {
                if token_env.is_empty() {
                    return Err("Telegram channel: token_env must be non-empty".into());
                }
            }
            ChannelCredentials::WeChat { token_env, .. } => {
                if token_env.is_empty() {
                    return Err("WeChat channel: token_env must be non-empty".into());
                }
            }
            ChannelCredentials::Feishu { app_id_env, .. } => {
                if app_id_env.is_empty() {
                    return Err("Feishu channel: app_id_env must be non-empty".into());
                }
            }
            ChannelCredentials::DingTalk {
                webhook_url_env,
                secret_env,
                ..
            } if webhook_url_env.is_empty() && secret_env.is_empty() => {
                return Err(
                    "DingTalk channel: webhook_url_env or secret_env must be non-empty".into(),
                );
            }
            _ => {}
        }
    }
    Ok(())
}

/// The secret-relocation hook applied to a profile's env vars before they are
/// persisted — the signature of [`relocate_keychain_backed_secrets`]. Carried
/// as a parameter so tests can drive the failure path without writing to the
/// developer's keychain.
pub(crate) type RelocateSecretsHook =
    fn(&mut std::collections::HashMap<String, String>, &str) -> Result<(), (StatusCode, String)>;

/// Roll back a just-created sub-account profile after a post-persist
/// creation step failed: the store saved the profile before these steps ran,
/// so deleting its registry record keeps the id retryable instead of
/// stranding it behind "already exists" when the same request is retried
/// (#1472, #2316). A rollback failure is logged, never reported over the
/// original error.
pub(crate) fn rollback_sub_account(store: &ProfileStore, sub_id: &str) {
    if let Err(rollback) = store.delete(sub_id) {
        tracing::error!(
            profile = %sub_id,
            error = %rollback,
            "failed to roll back sub-account after creation failure"
        );
    }
}

/// Apply freshly supplied sub-account env vars, relocating keychain-backed
/// secrets (e.g. the Vertex SA JSON) into the OS keychain before the save so a
/// sub-account never writes a private key to plaintext config. The store has
/// already persisted the fresh profile by the time this runs, so a failure —
/// relocation or the final save — rolls the profile back: otherwise the API
/// would report "creation failed" while leaving a sub-account whose id can
/// never be retried ("already exists", #1472, #2316).
pub(crate) fn apply_sub_account_env_vars(
    store: &ProfileStore,
    sub: &mut UserProfile,
    env_vars: std::collections::HashMap<String, String>,
    relocate: RelocateSecretsHook,
) -> Result<(), (StatusCode, String)> {
    sub.config.env_vars = env_vars;
    let sub_id = sub.id.clone();
    if let Err(e) = relocate(&mut sub.config.env_vars, &sub_id) {
        // Roll the just-created profile back: the sub-account was saved
        // keychain-less moments ago and nothing else references it yet, so
        // removing it restores the pre-request state instead of stranding a
        // half-configured id behind "already exists".
        rollback_sub_account(store, &sub.id);
        return Err(e);
    }
    sub.updated_at = Utc::now();
    if let Err(e) = store.save(sub) {
        // Same stranded-id shape: the env-less profile is already on disk,
        // so a failed save must not leave it behind either.
        rollback_sub_account(store, &sub.id);
        return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string()));
    }
    Ok(())
}

/// Create the User entry that lets a fresh sub-account log in via OTP. Runs
/// after the profile store has persisted the sub-account, so every failure
/// (invalid email, already-registered email, user-store save) rolls the
/// profile back — otherwise the request reports an error while the
/// sub-account id stays on disk, unretryable behind "already exists" (#2316).
pub(crate) fn create_sub_account_user_entry(
    state: &AppState,
    store: &ProfileStore,
    sub: &UserProfile,
    email: &str,
) -> Result<(), (StatusCode, String)> {
    let email = email.trim().to_lowercase();
    if email.is_empty() {
        return Ok(());
    }
    let outcome = (|| -> Result<(), (StatusCode, String)> {
        validate_email(&email).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
        if let Some(user_store) = state.user_store.as_ref() {
            // Check if email is already taken
            if let Ok(Some(_existing)) = user_store.get_by_email(&email) {
                return Err((
                    StatusCode::CONFLICT,
                    format!("Email '{email}' is already registered to another account"),
                ));
            }
            let user = crate::user_store::User {
                id: sub.id.clone(),
                email: email.clone(),
                name: sub.name.clone(),
                role: crate::user_store::UserRole::User,
                created_at: Utc::now(),
                last_login_at: None,
            };
            user_store
                .save(&user)
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        }
        Ok(())
    })();
    if outcome.is_err() {
        rollback_sub_account(store, &sub.id);
    }
    outcome
}

/// POST /api/admin/profiles/:id/accounts — Create a sub-account.
pub async fn create_sub_account(
    State(state): State<Arc<AppState>>,
    Path(parent_id): Path<String>,
    Json(req): Json<CreateSubAccountRequest>,
) -> Result<(StatusCode, Json<ProfileResponse>), (StatusCode, String)> {
    let store = state.profile_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;
    let pm = state.process_manager.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;

    // Validate channel credentials if any are provided
    if !req.channels.is_empty() {
        validate_channel_credentials(&req.channels).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    }

    let mut sub = store
        .create_sub_account(
            &parent_id,
            &req.sub_account_id,
            &req.public_subdomain,
            &req.name,
            req.channels,
            req.gateway.unwrap_or_default(),
        )
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;

    // Set channel-specific env vars if provided
    if !req.env_vars.is_empty() {
        apply_sub_account_env_vars(
            store,
            &mut sub,
            req.env_vars,
            relocate_keychain_backed_secrets,
        )?;
    }

    // Create a User entry so the sub-account can log in via OTP
    if let Some(email) = &req.email {
        create_sub_account_user_entry(&state, store, &sub, email)?;
    }

    let status = pm.status(&sub.id).await;
    Ok((
        StatusCode::CREATED,
        Json(ProfileResponse {
            runtime: None,
            email: None,
            profile: mask_secrets(&sub),
            status,
        }),
    ))
}

// ── System metrics endpoint ──────────────────────────────────────────

/// GET /api/admin/system/metrics — return system resource metrics (CPU, memory, disk).
pub async fn system_metrics(
    State(state): State<Arc<AppState>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    use sysinfo::{Disks, System};

    let include_procs = params.get("procs").map(|v| v == "1").unwrap_or(false);

    let mut sys = state.sysinfo.lock().await;
    // task-sysinfo-proc-stat-fd-budget: refresh only what this endpoint
    // renders; processes (without per-thread tasks) only when asked for.
    crate::sysinfo_budget::refresh_metrics(&mut sys, include_procs);

    // CPU info
    let cpu_count = sys.cpus().len();
    let cpu_usage: f32 = if cpu_count > 0 {
        sys.cpus().iter().map(|c| c.cpu_usage()).sum::<f32>() / cpu_count as f32
    } else {
        0.0
    };
    let cpu_brand = sys
        .cpus()
        .first()
        .map(|c| c.brand().to_string())
        .unwrap_or_default();

    // Memory
    let total_memory = sys.total_memory();
    let used_memory = sys.used_memory();
    let available_memory = sys.available_memory();
    let total_swap = sys.total_swap();
    let used_swap = sys.used_swap();

    // Disks
    let disks = Disks::new_with_refreshed_list();
    let disk_info: Vec<serde_json::Value> = disks
        .iter()
        .map(|d| {
            serde_json::json!({
                "name": d.name().to_string_lossy(),
                "mount_point": d.mount_point().to_string_lossy(),
                "total_bytes": d.total_space(),
                "available_bytes": d.available_space(),
                "used_bytes": d.total_space().saturating_sub(d.available_space()),
                "file_system": String::from_utf8_lossy(d.file_system().as_encoded_bytes()),
            })
        })
        .collect();

    // Top processes (only when requested via ?procs=1)
    let top_processes: Vec<serde_json::Value> = if include_procs {
        let mut procs: Vec<_> = sys
            .processes()
            .values()
            .map(|p| {
                (
                    p.pid().as_u32(),
                    p.name().to_string_lossy().to_string(),
                    (p.cpu_usage() * 10.0).round() / 10.0,
                    p.memory(),
                )
            })
            .collect();
        procs.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
        procs.truncate(10);
        procs
            .into_iter()
            .map(|(pid, name, cpu, mem)| {
                serde_json::json!({
                    "pid": pid,
                    "name": name,
                    "cpu_percent": cpu,
                    "memory_bytes": mem,
                })
            })
            .collect()
    } else {
        Vec::new()
    };

    // Platform
    let hostname = System::host_name().unwrap_or_default();
    let os_name = System::name().unwrap_or_default();
    let os_version = System::os_version().unwrap_or_default();
    let uptime = System::uptime();

    Ok(Json(serde_json::json!({
        "cpu": {
            "usage_percent": (cpu_usage * 10.0).round() / 10.0,
            "core_count": cpu_count,
            "brand": cpu_brand,
        },
        "memory": {
            "total_bytes": total_memory,
            "used_bytes": used_memory,
            "available_bytes": available_memory,
        },
        "swap": {
            "total_bytes": total_swap,
            "used_bytes": used_swap,
        },
        "disks": disk_info,
        "top_processes": top_processes,
        "platform": {
            "hostname": hostname,
            "os": os_name,
            "os_version": os_version,
            "uptime_secs": uptime,
        },
    })))
}

/// GET /api/admin/operator/summary — summarize runtime observability counters.
pub async fn operator_summary(
    State(state): State<Arc<AppState>>,
) -> Result<Json<super::metrics::OperatorSummary>, StatusCode> {
    let pm = state
        .process_manager
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    let mut sources = Vec::new();
    if let Some(ref handle) = state.metrics_handle {
        sources.push(super::metrics::OperatorSummarySourceInput {
            scope: "serve".to_string(),
            profile_id: None,
            running: true,
            scrape_status: "local".to_string(),
            scrape_error: None,
            api_port: None,
            pid: None,
            started_at: None,
            uptime_secs: None,
            metrics_text: Some(handle.render()),
        });
    }

    let mut statuses = pm.all_statuses().await.into_iter().collect::<Vec<_>>();
    statuses.sort_by(|left, right| left.0.cmp(&right.0));
    for (profile_id, status) in statuses {
        // `all_statuses()` also surfaces non-running profiles (configuration
        // errors). They have no gateway to scrape — report them as their own
        // state instead of a running gateway with a missing API port.
        if !status.running {
            let scrape_status = match status.status {
                crate::process_manager::ProcessState::ConfigurationError => "configuration_error",
                _ => "not_running",
            };
            sources.push(super::metrics::OperatorSummarySourceInput {
                scope: "gateway".to_string(),
                profile_id: Some(profile_id),
                running: false,
                scrape_status: scrape_status.to_string(),
                scrape_error: status.error,
                api_port: None,
                pid: None,
                started_at: None,
                uptime_secs: None,
                metrics_text: None,
            });
            continue;
        }

        let api_port = pm.api_port(&profile_id).await;
        let (scrape_status, scrape_error, metrics_text) = match api_port {
            Some(port) => match scrape_gateway_metrics(&state.http_client, port).await {
                Ok(rendered) => {
                    let scrape_status = if rendered.trim().is_empty() {
                        "empty".to_string()
                    } else {
                        "scraped".to_string()
                    };
                    (scrape_status, None, Some(rendered))
                }
                Err(error) => ("failed".to_string(), Some(error), None),
            },
            None => ("missing_api_port".to_string(), None, None),
        };

        sources.push(super::metrics::OperatorSummarySourceInput {
            scope: "gateway".to_string(),
            profile_id: Some(profile_id),
            running: true,
            scrape_status,
            scrape_error,
            api_port,
            pid: status.pid,
            started_at: status.started_at,
            uptime_secs: status.uptime_secs,
            metrics_text,
        });
    }

    Ok(Json(super::metrics::build_operator_summary_from_sources(
        sources,
    )))
}

async fn scrape_gateway_metrics(client: &reqwest::Client, port: u16) -> Result<String, String> {
    let url = format!("http://127.0.0.1:{port}/metrics");
    let response = client
        .get(url)
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .map_err(|error| error.to_string())?;

    if !response.status().is_success() {
        return Err(format!("http {}", response.status()));
    }

    response.text().await.map_err(|error| error.to_string())
}

// ── Operator harness task aggregation (`M4.5`) ───────────────────────

/// Upper bound on sessions scanned per gateway. Bounded to keep fan-out
/// predictable even on profiles with thousands of sessions.
const MAX_SESSIONS_PER_GATEWAY: usize = 32;

/// GET /api/admin/operator/tasks — aggregate active harness background tasks
/// from every running gateway. The dashboard consumes this to render the
/// operator harness view with lifecycle state, child session, artifacts,
/// validator outcome, retries, and failure cause per task.
pub async fn operator_tasks(
    State(state): State<Arc<AppState>>,
) -> Result<Json<super::metrics::OperatorTasksResponse>, StatusCode> {
    let pm = state
        .process_manager
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    let mut statuses = pm.all_statuses().await.into_iter().collect::<Vec<_>>();
    statuses.sort_by(|left, right| left.0.cmp(&right.0));

    let mut inputs: Vec<super::metrics::OperatorTaskInput> = Vec::new();
    let mut sources: Vec<super::metrics::OperatorTaskSource> = Vec::new();

    for (profile_id, status) in statuses {
        // Non-running profiles (configuration errors) have no gateway to
        // scrape tasks from — surface them as their own state instead of a
        // running gateway with a missing API port.
        if !status.running {
            let source_status = match status.status {
                crate::process_manager::ProcessState::ConfigurationError => "configuration_error",
                _ => "not_running",
            };
            sources.push(super::metrics::OperatorTaskSource {
                profile_id: profile_id.clone(),
                status: source_status.into(),
                error: status.error,
                api_port: None,
                session_count: 0,
                task_count: 0,
            });
            continue;
        }

        let api_port = pm.api_port(&profile_id).await;
        let Some(port) = api_port else {
            sources.push(super::metrics::OperatorTaskSource {
                profile_id: profile_id.clone(),
                status: "missing_api_port".into(),
                error: None,
                api_port: None,
                session_count: 0,
                task_count: 0,
            });
            continue;
        };

        match scrape_gateway_tasks(&state.http_client, port).await {
            Ok((session_count, task_rows)) => {
                let task_count = task_rows.len();
                inputs.extend(
                    task_rows
                        .into_iter()
                        .map(|row| task_row_to_input(&profile_id, row)),
                );
                sources.push(super::metrics::OperatorTaskSource {
                    profile_id: profile_id.clone(),
                    status: "ok".into(),
                    error: None,
                    api_port: Some(port),
                    session_count,
                    task_count,
                });
            }
            Err(error) => {
                sources.push(super::metrics::OperatorTaskSource {
                    profile_id: profile_id.clone(),
                    status: "failed".into(),
                    error: Some(error),
                    api_port: Some(port),
                    session_count: 0,
                    task_count: 0,
                });
            }
        }
    }

    Ok(Json(super::metrics::build_operator_tasks_response(
        inputs,
        sources,
        chrono::Utc::now(),
        super::metrics::DEFAULT_TASK_STALE_SECS,
    )))
}

/// Scrape background tasks from one gateway via its API channel.
///
/// Calls `GET /sessions` to enumerate sessions then fans out to
/// `GET /sessions/<id>/tasks` for each. Returns (session_count, task_rows).
async fn scrape_gateway_tasks(
    client: &reqwest::Client,
    port: u16,
) -> Result<(usize, Vec<serde_json::Value>), String> {
    let sessions_url = format!("http://127.0.0.1:{port}/sessions");
    let response = client
        .get(sessions_url)
        .timeout(Duration::from_secs(3))
        .send()
        .await
        .map_err(|error| error.to_string())?;

    if !response.status().is_success() {
        return Err(format!("sessions http {}", response.status()));
    }

    let sessions: Vec<serde_json::Value> = response
        .json::<Vec<serde_json::Value>>()
        .await
        .map_err(|error| error.to_string())?;
    let session_count = sessions.len();

    let mut tasks = Vec::new();
    for session in sessions.into_iter().take(MAX_SESSIONS_PER_GATEWAY) {
        let Some(id) = session.get("id").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let encoded = octos_bus::session::encode_path_component(id);
        let tasks_url = format!("http://127.0.0.1:{port}/sessions/{encoded}/tasks");
        let resp = match client
            .get(tasks_url)
            .timeout(Duration::from_secs(3))
            .send()
            .await
        {
            Ok(r) => r,
            Err(_) => continue,
        };
        if !resp.status().is_success() {
            continue;
        }
        let Ok(rows) = resp.json::<Vec<serde_json::Value>>().await else {
            continue;
        };
        tasks.extend(rows);
    }

    Ok((session_count, tasks))
}

fn task_row_to_input(
    profile_id: &str,
    row: serde_json::Value,
) -> super::metrics::OperatorTaskInput {
    let s = |value: &serde_json::Value, key: &str| {
        value
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned)
    };
    let session_key_full = s(&row, "session_key")
        .or_else(|| s(&row, "parent_session_key"))
        .unwrap_or_default();
    // session_key looks like "profile:api:session_id". Extract the last segment.
    let session_id = session_key_full
        .rsplit_once(':')
        .map(|(_prefix, tail)| tail.to_string())
        .unwrap_or_else(|| session_key_full.clone());

    let output_files = row
        .get("output_files")
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|value| value.as_str().map(ToOwned::to_owned))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    super::metrics::OperatorTaskInput {
        profile_id: profile_id.to_string(),
        session_id,
        task_id: s(&row, "id").unwrap_or_default(),
        tool_name: s(&row, "tool_name").unwrap_or_default(),
        lifecycle_state: s(&row, "lifecycle_state").unwrap_or_else(|| "unknown".into()),
        runtime_state: s(&row, "runtime_state"),
        workflow_kind: s(&row, "workflow_kind"),
        current_phase: s(&row, "current_phase"),
        child_session_key: s(&row, "child_session_key"),
        child_terminal_state: s(&row, "child_terminal_state"),
        child_join_state: s(&row, "child_join_state"),
        child_failure_action: s(&row, "child_failure_action"),
        output_files,
        error: s(&row, "error"),
        started_at: s(&row, "started_at"),
        updated_at: s(&row, "updated_at"),
        completed_at: s(&row, "completed_at"),
    }
}

// ── Monitor control endpoints ────────────────────────────────────────

fn profile_monitor_status_json(
    profile: UserProfile,
    system_watchdog: bool,
    system_alerts: bool,
) -> serde_json::Value {
    let watchdog_override = profile.config.gateway.watchdog_enabled;
    let alerts_override = profile.config.gateway.alerts_enabled;
    serde_json::json!({
        "id": profile.id,
        "name": profile.name,
        "enabled": profile.enabled,
        "watchdog_enabled": watchdog_override.unwrap_or(system_watchdog),
        "watchdog_override": watchdog_override,
        "alerts_enabled": alerts_override.unwrap_or(system_alerts),
        "alerts_override": alerts_override,
    })
}

/// GET /api/admin/monitor/status — returns system defaults plus profile overrides.
pub async fn monitor_status(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let watchdog = state
        .watchdog_enabled
        .as_ref()
        .map(|a| a.load(std::sync::atomic::Ordering::Relaxed))
        .unwrap_or(false);
    let alerts = state
        .alerts_enabled
        .as_ref()
        .map(|a| a.load(std::sync::atomic::Ordering::Relaxed))
        .unwrap_or(false);
    let profiles = if let Some(store) = state.profile_store.as_ref() {
        store
            .list()
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
            .into_iter()
            .map(|profile| profile_monitor_status_json(profile, watchdog, alerts))
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };

    Ok(Json(serde_json::json!({
        "watchdog_enabled": watchdog,
        "alerts_enabled": alerts,
        "profiles": profiles,
    })))
}

#[derive(Deserialize)]
pub struct MonitorToggleRequest {
    pub enabled: bool,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileMonitorOverrideRequest {
    Inherit,
    Enabled,
    Disabled,
}

impl ProfileMonitorOverrideRequest {
    fn as_option_bool(self) -> Option<bool> {
        match self {
            Self::Inherit => None,
            Self::Enabled => Some(true),
            Self::Disabled => Some(false),
        }
    }
}

#[derive(Deserialize)]
pub struct ProfileMonitorUpdateRequest {
    #[serde(default)]
    pub watchdog: Option<ProfileMonitorOverrideRequest>,
    #[serde(default)]
    pub alerts: Option<ProfileMonitorOverrideRequest>,
}

/// POST /api/admin/monitor/watchdog — toggle watchdog.
pub async fn toggle_watchdog(
    identity: Option<axum::Extension<AuthIdentity>>,
    State(state): State<Arc<AppState>>,
    Json(req): Json<MonitorToggleRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let before = state
        .watchdog_enabled
        .as_ref()
        .map(|flag| flag.load(std::sync::atomic::Ordering::Relaxed))
        .unwrap_or(false);
    if let Some(ref flag) = state.watchdog_enabled {
        flag.store(req.enabled, std::sync::atomic::Ordering::Relaxed);
    }
    super::admin_audit::record_admin_action(
        &state,
        identity.as_ref().map(|identity| &identity.0),
        "monitor.watchdog.toggle",
        "monitor.watchdog",
        Some(serde_json::json!({ "enabled": before })),
        Some(serde_json::json!({ "enabled": req.enabled })),
    )
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(
        serde_json::json!({ "ok": true, "watchdog_enabled": req.enabled }),
    ))
}

/// POST /api/admin/monitor/alerts — toggle alerts.
pub async fn toggle_alerts(
    identity: Option<axum::Extension<AuthIdentity>>,
    State(state): State<Arc<AppState>>,
    Json(req): Json<MonitorToggleRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let before = state
        .alerts_enabled
        .as_ref()
        .map(|flag| flag.load(std::sync::atomic::Ordering::Relaxed))
        .unwrap_or(false);
    if let Some(ref flag) = state.alerts_enabled {
        flag.store(req.enabled, std::sync::atomic::Ordering::Relaxed);
    }
    super::admin_audit::record_admin_action(
        &state,
        identity.as_ref().map(|identity| &identity.0),
        "monitor.alerts.toggle",
        "monitor.alerts",
        Some(serde_json::json!({ "enabled": before })),
        Some(serde_json::json!({ "enabled": req.enabled })),
    )
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(
        serde_json::json!({ "ok": true, "alerts_enabled": req.enabled }),
    ))
}

/// POST /api/admin/monitor/profiles/:id — set per-profile monitor overrides.
pub async fn update_profile_monitor(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<ProfileMonitorUpdateRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let store = state.profile_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;

    let mut profile = store
        .get(&id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, format!("profile '{id}' not found")))?;

    if let Some(watchdog) = req.watchdog {
        profile.config.gateway.watchdog_enabled = watchdog.as_option_bool();
    }
    if let Some(alerts) = req.alerts {
        profile.config.gateway.alerts_enabled = alerts.as_option_bool();
    }
    profile.updated_at = Utc::now();
    store
        .save(&profile)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let watchdog = state
        .watchdog_enabled
        .as_ref()
        .map(|a| a.load(std::sync::atomic::Ordering::Relaxed))
        .unwrap_or(false);
    let alerts = state
        .alerts_enabled
        .as_ref()
        .map(|a| a.load(std::sync::atomic::Ordering::Relaxed))
        .unwrap_or(false);

    Ok(Json(profile_monitor_status_json(profile, watchdog, alerts)))
}

// ── Skill management ─────────────────────────────────────────────────

/// GET /api/admin/profiles/:id/skills — list installed skills for a profile.
pub async fn list_profile_skills(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let store = state.profile_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;
    let skills_dir = crate::commands::skills::resolve_profile_skills_dir(store, &id)
        .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;
    let skills = crate::commands::skills::list_skills(&skills_dir)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!({ "skills": skills })))
}

#[derive(Deserialize)]
pub struct InstallSkillRequest {
    /// Skill source: GitHub shorthand, full Git URL, or local path.
    pub repo: String,
    #[serde(default)]
    pub force: bool,
    #[serde(default = "default_branch")]
    pub branch: String,
}

fn default_branch() -> String {
    "main".to_string()
}

/// POST /api/admin/profiles/:id/skills — install a skill from GitHub shorthand, a full Git URL, or a local path.
pub async fn install_profile_skill(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<InstallSkillRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let store = state.profile_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;
    let skills_dir = crate::commands::skills::resolve_profile_skills_dir(store, &id)
        .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;

    // install_via_git is blocking (spawns git process)
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

/// DELETE /api/admin/profiles/:id/skills/:name — remove an installed skill.
pub async fn remove_profile_skill(
    State(state): State<Arc<AppState>>,
    Path((id, name)): Path<(String, String)>,
) -> Result<Json<ActionResponse>, (StatusCode, String)> {
    let store = state.profile_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;
    let skills_dir = crate::commands::skills::resolve_profile_skills_dir(store, &id)
        .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;

    // Defer to spawn_blocking so remove_skill's internal current-thread
    // tokio runtime doesn't try to construct inside the axum runtime
    // (mirrors install_skill).
    let name_for_remove = name.clone();
    tokio::task::spawn_blocking(move || {
        crate::commands::skills::remove_skill(&skills_dir, &name_for_remove)
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;

    Ok(Json(ActionResponse {
        ok: true,
        message: Some(format!("Removed skill: {name}")),
    }))
}

// ── Platform Skills ──────────────────────────────────────────────────

fn ominix_api_url() -> String {
    ominix_runtime::configured_api_url()
}

fn models_dir() -> std::path::PathBuf {
    ominix_runtime::models_dir()
}

/// GET /api/admin/platform-skills — list platform skills and their status.
pub async fn list_platform_skills(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let store = state.profile_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;
    let skills_dir = store
        .octos_home_dir()
        .join(octos_agent::bootstrap::PLATFORM_SKILLS_DIR);

    // List installed platform skills
    let installed = crate::commands::skills::list_skills(&skills_dir).unwrap_or_default();

    let ominix_runtime = ominix_runtime::runtime_status(&state.http_client).await;
    let ominix_url = ominix_runtime.url.clone();
    let ominix_healthy = ominix_runtime.health.healthy;
    let service_status = ominix_runtime.service_registered;

    // Check models against platform allowlist
    let mdir = models_dir();
    let allowlist = octos_llm::ominix::PlatformModels::load_or_create(store.octos_home_dir())
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to load platform model allowlist: {e}"),
            )
        })?;
    let asr_models: Vec<String> = allowlist
        .ids_for_role("asr")
        .into_iter()
        .filter(|id| mdir.join(id).exists())
        .map(|id| id.to_string())
        .collect();

    let tts_models: Vec<String> = allowlist
        .ids_for_role("tts")
        .into_iter()
        .filter(|id| mdir.join(id).exists())
        .map(|id| id.to_string())
        .collect();

    // Build platform skills list
    let mut skills = Vec::new();
    for &(name, _, _, _) in octos_agent::bundled_app_skills::PLATFORM_SKILLS {
        let is_installed = installed.iter().any(|s| s.name == name);
        skills.push(serde_json::json!({
            "name": name,
            "installed": is_installed,
        }));
    }

    Ok(Json(serde_json::json!({
        "platform_skills": skills,
        "skills_dir": skills_dir.display().to_string(),
        "ominix_api": {
            "url": ominix_url,
            "healthy": ominix_healthy,
            "service_registered": service_status,
            "runtime": ominix_runtime,
        },
        "models": {
            "dir": mdir.display().to_string(),
            "asr": asr_models,
            "tts": tts_models,
        }
    })))
}

/// GET /api/admin/platform-skills/ominix-api/runtime — detailed runtime status.
pub async fn platform_runtime_status(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let status = ominix_runtime::runtime_status(&state.http_client).await;
    Ok(Json(serde_json::to_value(status).unwrap_or_default()))
}

/// POST /api/admin/platform-skills/ominix-api/repair — repair local OMiniX runtime.
pub async fn platform_runtime_repair(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let response = ominix_runtime::repair_runtime(&state.http_client)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(serde_json::to_value(response).unwrap_or_default()))
}

/// POST /api/admin/platform-skills/ominix-api/install — install or repair local OMiniX runtime.
pub async fn platform_runtime_install(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let response = ominix_runtime::install_runtime(&state.http_client)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(serde_json::to_value(response).unwrap_or_default()))
}

/// POST /api/admin/platform-skills/ominix-api/bootstrap — install API and prepare core voice models.
pub async fn platform_runtime_bootstrap(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let store = state.profile_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;
    let octos_home = store.octos_home_dir();
    let mut actions = Vec::new();

    let mut allowlist =
        octos_llm::ominix::PlatformModels::load_or_create(octos_home).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to load platform model allowlist: {e}"),
            )
        })?;
    for (model_id, role) in ominix_runtime::DEFAULT_VOICE_MODELS {
        if allowlist.find(model_id).is_none() {
            allowlist
                .platform_models
                .push(octos_llm::ominix::PlatformModel {
                    id: (*model_id).to_string(),
                    role: (*role).to_string(),
                });
            actions.push(format!("enabled {model_id} for {role}"));
        }
    }
    allowlist.save(octos_home).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to save platform model allowlist: {e}"),
        )
    })?;

    let install_response = ominix_runtime::install_runtime(&state.http_client)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    actions.extend(install_response.actions.iter().cloned());

    if !install_response.status.health.healthy {
        return Ok(Json(serde_json::json!({
            "ok": false,
            "message": "OMiniX API could not be started; voice models were not downloaded",
            "status": install_response.status,
            "runtime": install_response,
            "actions": actions,
            "models": [],
        })));
    }

    let catalog = fetch_ominix_catalog()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e))?;
    let bytes_needed = missing_default_model_bytes(&catalog);
    if bytes_needed > 0 {
        if let Some(free) = available_space_bytes(&models_dir()) {
            let required = bytes_needed.saturating_add(MODEL_BOOTSTRAP_SPACE_MARGIN_BYTES);
            if free < required {
                return Ok(Json(serde_json::json!({
                    "ok": false,
                    "message": format!(
                        "Not enough disk space for core voice models: need about {}, available {}",
                        human_bytes(required),
                        human_bytes(free)
                    ),
                    "status": install_response.status,
                    "runtime": install_response,
                    "actions": actions,
                    "models": [],
                })));
            }
        }
    }

    let mut model_results = Vec::new();
    for (model_id, role) in ominix_runtime::DEFAULT_VOICE_MODELS {
        let result = bootstrap_voice_model(&state, model_id, role)
            .await
            .map_err(|e| (StatusCode::BAD_GATEWAY, e))?;
        actions.push(format!(
            "{} {} ({}) -> {}",
            result.action, result.id, result.role, result.status_after
        ));
        model_results.push(result);
    }

    let final_status = ominix_runtime::runtime_status(&state.http_client).await;
    let models_ready = model_results.iter().all(|model| model.ready);
    let ok = final_status.health.healthy && models_ready;
    let message = if ok {
        "OMiniX API and core voice models are ready".to_string()
    } else {
        "OMiniX API bootstrap completed, but some voice models are not ready".to_string()
    };

    Ok(Json(serde_json::json!({
        "ok": ok,
        "message": message,
        "actions": actions,
        "status": final_status,
        "models_ready": models_ready,
        "models": model_results,
    })))
}

/// POST /api/admin/platform-skills/:name/install — install/update a platform skill.
pub async fn install_platform_skill(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<Json<ActionResponse>, (StatusCode, String)> {
    let store = state.profile_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;
    let octos_home = store.octos_home_dir();

    if octos_agent::bootstrap::bootstrap_single_skill(octos_home, &name) {
        Ok(Json(ActionResponse {
            ok: true,
            message: Some(format!("Platform skill '{name}' installed")),
        }))
    } else {
        Err((
            StatusCode::NOT_FOUND,
            format!("Platform skill '{name}' not found or binary missing"),
        ))
    }
}

/// DELETE /api/admin/platform-skills/:name — remove a platform skill.
pub async fn remove_platform_skill(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<Json<ActionResponse>, (StatusCode, String)> {
    let store = state.profile_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;
    let skills_dir = store
        .octos_home_dir()
        .join(octos_agent::bootstrap::PLATFORM_SKILLS_DIR);

    // Defer to spawn_blocking so remove_skill's internal current-thread
    // tokio runtime doesn't try to construct inside the axum runtime
    // (mirrors install_skill).
    let name_for_remove = name.clone();
    tokio::task::spawn_blocking(move || {
        crate::commands::skills::remove_skill(&skills_dir, &name_for_remove)
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;

    Ok(Json(ActionResponse {
        ok: true,
        message: Some(format!("Platform skill '{name}' removed")),
    }))
}

/// GET /api/admin/platform-skills/:name/health — check backend health for a platform skill.
pub async fn platform_skill_health(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    match name.as_str() {
        "voice" | "asr" | "ominix-api" => {
            let runtime = ominix_runtime::runtime_status(&state.http_client).await;
            let status = if runtime.health.healthy {
                "healthy"
            } else if runtime.can_repair {
                "repairable"
            } else {
                "unreachable"
            };

            Ok(Json(serde_json::json!({
                "name": name,
                "status": status,
                "url": runtime.url,
                "detail": runtime.health,
                "runtime": runtime,
            })))
        }
        _ => Err((
            StatusCode::NOT_FOUND,
            format!("Unknown platform skill: {name}"),
        )),
    }
}

/// POST /api/admin/platform-skills/ominix-api/start
pub async fn platform_service_start(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let response = ominix_runtime::service_start(&state.http_client)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(serde_json::to_value(response).unwrap_or_default()))
}

/// POST /api/admin/platform-skills/ominix-api/stop
pub async fn platform_service_stop(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let response = ominix_runtime::service_stop(&state.http_client)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(serde_json::to_value(response).unwrap_or_default()))
}

/// POST /api/admin/platform-skills/ominix-api/restart
pub async fn platform_service_restart(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let response = ominix_runtime::service_restart(&state.http_client)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(serde_json::to_value(response).unwrap_or_default()))
}

/// GET /api/admin/platform-skills/ominix-api/logs
pub async fn platform_service_logs(
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let lines: usize = params
        .get("lines")
        .and_then(|v| v.parse().ok())
        .unwrap_or(50)
        .min(200);

    // Match the runtime's home resolution (api/ominix_runtime.rs): a custom
    // OCTOS_OMINIX_HOME relocates the whole OMiniX home, logs included.
    let home = std::env::var_os("OCTOS_OMINIX_HOME")
        .or_else(|| std::env::var_os("HOME"))
        .map(std::path::PathBuf::from)
        .or_else(dirs::home_dir)
        .unwrap_or_default();
    let (log_path, err_log_path) = platform_service_log_paths(&home);

    // The launchd plist routes stderr (startup failures, panics) to a
    // separate api.err.log; surface it alongside the main log or bind
    // failures and early crashes never reach the dashboard.
    let (err_total_lines, err_lines) = match tokio::fs::read_to_string(&err_log_path).await {
        Ok(c) => (c.lines().count(), last_lines(&c, lines)),
        Err(_) => (0, Vec::new()),
    };

    let content = match tokio::fs::read_to_string(&log_path).await {
        Ok(c) => c,
        Err(e) => {
            return Ok(Json(serde_json::json!({
                "log_path": log_path,
                "error": format!("Cannot read log file: {e}"),
                "lines": [],
                "err_log_path": err_log_path,
                "err_total_lines": err_total_lines,
                "err_lines": err_lines,
            })));
        }
    };

    Ok(Json(serde_json::json!({
        "log_path": log_path,
        "total_lines": content.lines().count(),
        "lines": last_lines(&content, lines),
        "err_log_path": err_log_path,
        "err_total_lines": err_total_lines,
        "err_lines": err_lines,
    })))
}

/// Main and stderr log paths under the OMiniX home. Prefers `api.log`,
/// falling back to the legacy `ominix-api.log` name.
fn platform_service_log_paths(home: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let dir = home.join(".ominix");
    let primary = dir.join("api.log");
    let main = if primary.exists() {
        primary
    } else {
        dir.join("ominix-api.log")
    };
    (main, dir.join("api.err.log"))
}

fn last_lines(content: &str, n: usize) -> Vec<String> {
    content
        .lines()
        .rev()
        .take(n)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(str::to_string)
        .collect()
}

// ── Model Management (proxy to ominix-api) ─────────────────────────

async fn fetch_ominix_catalog() -> Result<Vec<octos_llm::ominix::CatalogModel>, String> {
    octos_llm::ominix::OminixClient::new(&ominix_api_url())
        .fetch_catalog()
        .await
        .map_err(|e| format!("Failed to fetch ominix-api catalog: {e}"))
}

fn catalog_model<'a>(
    catalog: &'a [octos_llm::ominix::CatalogModel],
    model_id: &str,
) -> Option<&'a octos_llm::ominix::CatalogModel> {
    catalog.iter().find(|model| model.id == model_id)
}

fn catalog_status(catalog: &[octos_llm::ominix::CatalogModel], model_id: &str) -> String {
    catalog_model(catalog, model_id)
        .map(|model| model.status.clone())
        .unwrap_or_else(|| "unknown".to_string())
}

fn missing_default_model_bytes(catalog: &[octos_llm::ominix::CatalogModel]) -> u64 {
    ominix_runtime::DEFAULT_VOICE_MODELS
        .iter()
        .filter_map(|(model_id, _)| {
            let model = catalog_model(catalog, model_id)?;
            if ominix_runtime::is_ready_model_status(&model.status) {
                None
            } else {
                model.storage.total_size_bytes
            }
        })
        .sum()
}

async fn bootstrap_voice_model(
    state: &Arc<AppState>,
    model_id: &str,
    role: &str,
) -> Result<OminixModelBootstrapResult, String> {
    let catalog = fetch_ominix_catalog().await?;
    let before_status = catalog_status(&catalog, model_id);
    let size = catalog_model(&catalog, model_id)
        .and_then(|model| model.storage.total_size_display.clone());

    if ominix_runtime::is_ready_model_status(&before_status) {
        return Ok(OminixModelBootstrapResult {
            id: model_id.to_string(),
            role: role.to_string(),
            ready: true,
            action: "kept".to_string(),
            status_before: before_status.clone(),
            status_after: before_status,
            size,
            message: Some("already ready".to_string()),
        });
    }

    let url = format!(
        "{}/v1/models/download",
        ominix_api_url().trim_end_matches('/')
    );
    let response = state
        .http_client
        .post(&url)
        .json(&serde_json::json!({ "model_id": model_id }))
        .timeout(MODEL_BOOTSTRAP_TIMEOUT)
        .send()
        .await
        .map_err(|e| format!("ominix-api download request failed for {model_id}: {e}"))?;

    let status = response.status();
    let response_text = response.text().await.unwrap_or_default();
    let mut message = if response_text.trim().is_empty() {
        None
    } else {
        Some(response_text.clone())
    };
    if !(status.is_success() || status == StatusCode::CONFLICT) {
        return Ok(OminixModelBootstrapResult {
            id: model_id.to_string(),
            role: role.to_string(),
            ready: false,
            action: "download_failed".to_string(),
            status_before: before_status,
            status_after: "unknown".to_string(),
            size,
            message,
        });
    }
    if status == StatusCode::CONFLICT && message.is_none() {
        message = Some("already downloaded".to_string());
    }

    let refreshed = fetch_ominix_catalog().await?;
    let after_status = catalog_status(&refreshed, model_id);
    let ready = ominix_runtime::is_ready_model_status(&after_status);
    Ok(OminixModelBootstrapResult {
        id: model_id.to_string(),
        role: role.to_string(),
        ready,
        action: if ready {
            "downloaded".to_string()
        } else {
            "download_requested".to_string()
        },
        status_before: before_status,
        status_after: after_status,
        size,
        message,
    })
}

fn available_space_bytes(path: &std::path::Path) -> Option<u64> {
    let target = if path.exists() {
        path
    } else {
        path.parent().unwrap_or_else(|| std::path::Path::new("/"))
    };
    let output = std::process::Command::new("df")
        .args(["-Pk", &target.display().to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.lines().nth(1)?;
    let available_kib = line.split_whitespace().nth(3)?.parse::<u64>().ok()?;
    Some(available_kib.saturating_mul(1024))
}

fn human_bytes(bytes: u64) -> String {
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    let bytes_f = bytes as f64;
    if bytes_f >= GIB {
        format!("{:.1} GiB", bytes_f / GIB)
    } else if bytes_f >= MIB {
        format!("{:.0} MiB", bytes_f / MIB)
    } else {
        format!("{bytes} B")
    }
}

/// GET /api/admin/platform-skills/ominix-api/models — list platform models
///
/// Fetches the full catalog from ominix-api, filters to models listed in
/// `~/.octos/platform-models.json`, and returns them with role annotations.
pub async fn platform_models_catalog(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let store = state.profile_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;
    let allowlist = octos_llm::ominix::PlatformModels::load_or_create(store.octos_home_dir())
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to load platform model allowlist: {e}"),
            )
        })?;

    // Try fetching live catalog from ominix-api
    let ominix = octos_llm::ominix::OminixClient::new(&ominix_api_url());
    let models: Vec<serde_json::Value> = match ominix.platform_catalog(&allowlist).await {
        Ok(catalog) => catalog
            .into_iter()
            .map(|m| {
                let role = allowlist
                    .find(&m.id)
                    .map(|p| p.role.as_str())
                    .unwrap_or("unknown");
                let mut v = serde_json::to_value(&m).unwrap_or_default();
                v.as_object_mut()
                    .map(|o| o.insert("role".into(), role.into()));
                v
            })
            .collect(),
        Err(_) => {
            // Offline fallback: return allowlist entries with minimal info
            allowlist
                .platform_models
                .iter()
                .map(|pm| {
                    let local = models_dir().join(&pm.id);
                    serde_json::json!({
                        "id": pm.id,
                        "role": pm.role,
                        "status": if local.exists() { "ready" } else { "unknown" },
                        "source": "offline (ominix-api unreachable)",
                    })
                })
                .collect()
        }
    };

    Ok(Json(serde_json::json!({ "models": models })))
}

/// POST /api/admin/platform-skills/ominix-api/models/download — start model download
///
/// Accepts `model_id` (e.g. "qwen3-asr-1.7b") and validates it against the
/// platform allowlist before forwarding to ominix-api.
pub async fn platform_models_download(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let model_id = body
        .get("model_id")
        .and_then(|v| v.as_str())
        .ok_or((StatusCode::BAD_REQUEST, "missing model_id".into()))?;

    let store = state.profile_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;
    let allowlist = octos_llm::ominix::PlatformModels::load_or_create(store.octos_home_dir())
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to load platform model allowlist: {e}"),
            )
        })?;
    if allowlist.find(model_id).is_none() {
        let valid: Vec<&str> = allowlist
            .platform_models
            .iter()
            .map(|m| m.id.as_str())
            .collect();
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "Model '{model_id}' not in platform allowlist. Valid: {}",
                valid.join(", ")
            ),
        ));
    }

    // Forward model_id directly to ominix-api — it knows its own repo_ids
    let download_body = serde_json::json!({ "model_id": model_id });

    let url = format!(
        "{}/v1/models/download",
        ominix_api_url().trim_end_matches('/')
    );
    let resp = state
        .http_client
        .post(&url)
        .json(&download_body)
        .timeout(MODEL_BOOTSTRAP_TIMEOUT)
        .send()
        .await
        .map_err(|e| {
            (
                StatusCode::BAD_GATEWAY,
                format!("ominix-api unreachable: {e}"),
            )
        })?;

    let status = resp.status();
    let resp_body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("Invalid response: {e}")))?;

    if status.is_success() {
        Ok(Json(resp_body))
    } else {
        Err((
            StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
            serde_json::to_string(&resp_body).unwrap_or_default(),
        ))
    }
}

/// POST /api/admin/platform-skills/ominix-api/models/remove — remove a downloaded model
pub async fn platform_models_remove(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let url = format!(
        "{}/v1/models/remove",
        ominix_api_url().trim_end_matches('/')
    );
    let resp = state
        .http_client
        .post(&url)
        .json(&body)
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| {
            (
                StatusCode::BAD_GATEWAY,
                format!("ominix-api unreachable: {e}"),
            )
        })?;

    let status = resp.status();
    let resp_body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("Invalid response: {e}")))?;

    if status.is_success() {
        Ok(Json(resp_body))
    } else {
        Err((
            StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
            serde_json::to_string(&resp_body).unwrap_or_default(),
        ))
    }
}

// ── Platform Model Allowlist Management ──────────────────────────────

/// GET /api/admin/platform-skills/ominix-api/models/available — list ALL ominix-api models
///
/// Returns the full unfiltered catalog from ominix-api so the admin can see
/// what's available to enable for octos platform use.
pub async fn platform_models_available(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let store = state.profile_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;
    let allowlist = octos_llm::ominix::PlatformModels::load_or_create(store.octos_home_dir())
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to load platform model allowlist: {e}"),
            )
        })?;
    let ominix = octos_llm::ominix::OminixClient::new(&ominix_api_url());

    let catalog = ominix.fetch_catalog().await.map_err(|e| {
        (
            StatusCode::BAD_GATEWAY,
            format!("Failed to fetch ominix-api catalog: {e}"),
        )
    })?;

    let models: Vec<serde_json::Value> = catalog
        .into_iter()
        .map(|m| {
            let enabled = allowlist.find(&m.id).is_some();
            let role = allowlist.find(&m.id).map(|p| p.role.as_str()).unwrap_or("");
            let mut v = serde_json::to_value(&m).unwrap_or_default();
            if let Some(obj) = v.as_object_mut() {
                obj.insert("enabled_for_octos".into(), enabled.into());
                if enabled {
                    obj.insert("role".into(), role.into());
                }
            }
            v
        })
        .collect();

    Ok(Json(serde_json::json!({ "models": models })))
}

/// POST /api/admin/platform-skills/ominix-api/models/enable — add model to platform allowlist
///
/// Body: `{ "model_id": "qwen3-asr-1.7b", "role": "asr" }`
pub async fn platform_models_enable(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let model_id = body
        .get("model_id")
        .and_then(|v| v.as_str())
        .ok_or((StatusCode::BAD_REQUEST, "missing model_id".into()))?;
    let role = body.get("role").and_then(|v| v.as_str()).ok_or((
        StatusCode::BAD_REQUEST,
        "missing role (asr, tts, etc.)".into(),
    ))?;

    let store = state.profile_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;
    let octos_home = store.octos_home_dir();
    let mut allowlist =
        octos_llm::ominix::PlatformModels::load_or_create(octos_home).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to load platform model allowlist: {e}"),
            )
        })?;

    if allowlist.find(model_id).is_some() {
        return Ok(Json(serde_json::json!({
            "ok": true,
            "message": format!("Model '{model_id}' already in platform allowlist"),
        })));
    }

    allowlist
        .platform_models
        .push(octos_llm::ominix::PlatformModel {
            id: model_id.to_string(),
            role: role.to_string(),
        });
    allowlist.save(octos_home).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to save allowlist: {e}"),
        )
    })?;

    Ok(Json(serde_json::json!({
        "ok": true,
        "message": format!("Model '{model_id}' added to platform allowlist with role '{role}'"),
    })))
}

/// POST /api/admin/platform-skills/ominix-api/models/disable — remove model from platform allowlist
///
/// Body: `{ "model_id": "qwen3-asr-1.7b" }`
pub async fn platform_models_disable(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let model_id = body
        .get("model_id")
        .and_then(|v| v.as_str())
        .ok_or((StatusCode::BAD_REQUEST, "missing model_id".into()))?;

    let store = state.profile_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;
    let octos_home = store.octos_home_dir();
    let mut allowlist =
        octos_llm::ominix::PlatformModels::load_or_create(octos_home).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to load platform model allowlist: {e}"),
            )
        })?;

    let before = allowlist.platform_models.len();
    allowlist.platform_models.retain(|m| m.id != model_id);

    if allowlist.platform_models.len() == before {
        return Ok(Json(serde_json::json!({
            "ok": true,
            "message": format!("Model '{model_id}' was not in platform allowlist"),
        })));
    }

    allowlist.save(octos_home).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to save allowlist: {e}"),
        )
    })?;

    Ok(Json(serde_json::json!({
        "ok": true,
        "message": format!("Model '{model_id}' removed from platform allowlist"),
    })))
}

// ── System Update ────────────────────────────────────────────────────

/// POST /api/admin/system/version — check current and latest version
pub async fn system_version(
    State(_state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let current = crate::updater::Updater::current_version();
    let gh_token = body
        .get("github_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let updater = crate::updater::Updater::new(gh_token)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let latest = match updater.check_latest().await {
        Ok(info) => serde_json::json!({
            "tag": info.tag,
            "version": info.version,
            "published_at": info.published_at,
        }),
        Err(e) => {
            tracing::warn!(error = %e, "failed to check latest version");
            serde_json::json!(null)
        }
    };

    let current_semver = env!("CARGO_PKG_VERSION");
    let update_available = update_available_for(
        current_semver,
        latest.get("version").and_then(|v| v.as_str()),
    );

    Ok(Json(serde_json::json!({
        "current": current,
        "latest": latest,
        "update_available": update_available,
    })))
}

#[derive(Deserialize)]
pub struct UpdateRequest {
    #[serde(default = "default_version")]
    pub version: String,
    #[serde(default)]
    pub github_token: Option<String>,
}
fn default_version() -> String {
    "latest".to_string()
}

/// Whether `latest` is strictly newer than the running `current` release,
/// with full semver precedence: `2.0.3-rc.12 < 2.0.3-rc.13 < 2.0.3`. This is
/// deliberately pre-release-aware — unlike `octos_diagnostics`' planner, whose
/// `parse_version` strips pre-releases — because the admin channel installs
/// pinned rc tags and must keep the rc train flowing forward while still
/// refusing downgrades. Unparseable on either side means "can't tell" and
/// counts as up to date — never push a spurious (or backwards) update off an
/// unparseable version.
fn update_available_for(current: &str, latest: Option<&str>) -> bool {
    let Some(latest) = latest else {
        return false;
    };
    let (Ok(current), Ok(latest)) = (
        semver::Version::parse(current.trim().trim_start_matches('v')),
        semver::Version::parse(latest.trim().trim_start_matches('v')),
    ) else {
        return false;
    };
    latest > current
}

/// POST /api/admin/system/update — download and apply an update
pub async fn system_update(
    State(_state): State<Arc<AppState>>,
    Json(body): Json<UpdateRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let updater = crate::updater::Updater::new(body.github_token)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Resolve the release
    let release = if body.version == "latest" {
        updater.check_latest().await
    } else {
        let tag = if body.version.starts_with('v') {
            body.version.clone()
        } else {
            format!("v{}", body.version)
        };
        updater.check_version(&tag).await
    }
    .map_err(|e| (StatusCode::BAD_REQUEST, format!("Release not found: {e}")))?;

    // Refuse anything that is not strictly newer — an update channel that
    // accepts equal or older releases is a downgrade vector.
    let current = env!("CARGO_PKG_VERSION");
    let new_version = &release.version;
    if !update_available_for(current, Some(new_version)) {
        return Err((
            StatusCode::CONFLICT,
            format!("refusing update: {new_version} is not newer than the running {current}"),
        ));
    }

    // Perform the update
    let result = updater.update(&release).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Update failed: {e}"),
        )
    })?;

    // Schedule a restart after sending the response
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        tracing::info!("restarting service after update");
        // Get UID via `id -u` (safe, no unsafe block needed)
        let uid = std::process::Command::new("id")
            .arg("-u")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| "501".to_string());
        let label = format!("gui/{uid}/io.ominix.octos-serve");
        let status = std::process::Command::new("launchctl")
            .args(["kickstart", "-k", &label])
            .status();
        match status {
            Ok(s) if s.success() => tracing::info!("launchctl restart succeeded"),
            Ok(s) => {
                tracing::warn!(code = ?s.code(), "launchctl restart exited with error, trying exit");
                // Fallback: just exit and let launchd KeepAlive restart us
                std::process::exit(0);
            }
            Err(e) => {
                tracing::warn!(error = %e, "launchctl not available, exiting for restart");
                std::process::exit(0);
            }
        }
    });

    Ok(Json(serde_json::json!({
        "success": true,
        "old_version": result.old_version,
        "new_version": result.new_version,
        "binaries_updated": result.binaries_updated,
        "message": "Update complete. Restarting service...",
    })))
}

// ── Session & Cron diagnostic endpoints ──────────────────────────────

/// GET /api/admin/profiles/:id/sessions — List session files for a profile.
pub async fn list_sessions(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let pm = state.process_manager.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;
    let ps = pm.profile_store();
    let profile = ps
        .get(&id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, format!("profile '{id}' not found")))?;
    let data_dir = ps.resolve_data_dir(&profile);
    let sessions_dir = data_dir.join("sessions");

    // Helper to build a session JSON entry from a path and decoded key.
    let build_session_entry = |path: &std::path::Path, decoded_key: String, file_name: String| {
        let meta = std::fs::metadata(path).ok();
        let size_bytes = meta.as_ref().map(|m| m.len()).unwrap_or(0);
        let modified = meta.and_then(|m| m.modified().ok()).map(|t| {
            let dt: chrono::DateTime<Utc> = t.into();
            dt.to_rfc3339()
        });
        // Count lines (messages = lines - 1 for metadata line)
        let line_count = std::fs::File::open(path)
            .ok()
            .map(|f| {
                use std::io::BufRead;
                std::io::BufReader::new(f).lines().count()
            })
            .unwrap_or(0);
        let msg_count = line_count.saturating_sub(1);
        serde_json::json!({
            "key": decoded_key,
            "file": file_name,
            "messages": msg_count,
            "size_bytes": size_bytes,
            "modified": modified,
        })
    };

    // Use a map keyed by decoded_key so per-user entries take precedence.
    let mut session_map = std::collections::HashMap::<String, serde_json::Value>::new();

    // 1. Scan legacy flat sessions/ directory.
    if let Ok(entries) = std::fs::read_dir(&sessions_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let file_name = path
                .file_stem()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            let decoded_key = octos_bus::SessionManager::decode_filename(&file_name);
            let entry_val = build_session_entry(&path, decoded_key.clone(), file_name);
            session_map.insert(decoded_key, entry_val);
        }
    }

    // 2. Scan per-user layout: data_dir/users/*/sessions/*.jsonl
    let users_dir = data_dir.join("users");
    if let Ok(user_entries) = std::fs::read_dir(&users_dir) {
        for user_entry in user_entries.flatten() {
            let user_path = user_entry.path();
            if !user_path.is_dir() {
                continue;
            }
            let encoded_base_key = match user_path.file_name().and_then(|n| n.to_str()) {
                Some(name) => name.to_string(),
                None => continue,
            };
            let base_key = octos_bus::SessionManager::decode_filename(&encoded_base_key);

            let user_sessions_dir = user_path.join("sessions");
            if let Ok(sess_entries) = std::fs::read_dir(&user_sessions_dir) {
                for sess_entry in sess_entries.flatten() {
                    let path = sess_entry.path();
                    if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                        continue;
                    }
                    let topic = path
                        .file_stem()
                        .and_then(|n| n.to_str())
                        .unwrap_or("default")
                        .to_string();
                    let decoded_key = if topic == "default" {
                        base_key.clone()
                    } else {
                        format!("{}#{}", base_key, topic)
                    };
                    let file_label = format!("{}/{}", encoded_base_key, topic);
                    let entry_val = build_session_entry(&path, decoded_key.clone(), file_label);
                    // Per-user takes precedence over legacy.
                    session_map.insert(decoded_key, entry_val);
                }
            }
        }
    }

    let mut sessions: Vec<serde_json::Value> = session_map.into_values().collect();
    // Sort by modified descending (most recent first)
    sessions.sort_by(|a, b| {
        let ma = a.get("modified").and_then(|v| v.as_str()).unwrap_or("");
        let mb = b.get("modified").and_then(|v| v.as_str()).unwrap_or("");
        mb.cmp(ma)
    });

    Ok(Json(serde_json::json!({
        "profile_id": id,
        "count": sessions.len(),
        "sessions": sessions,
    })))
}

/// Query params for reading a session.
#[derive(Deserialize)]
pub struct ReadSessionQuery {
    /// Session key (percent-decoded)
    pub key: String,
    /// Max number of recent messages to return (default 50)
    #[serde(default = "default_session_lines")]
    pub lines: usize,
}

fn default_session_lines() -> usize {
    50
}

/// GET /api/admin/profiles/:id/sessions/read?key=...&lines=50 — Read session messages.
pub async fn read_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(query): Query<ReadSessionQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let pm = state.process_manager.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;
    let ps = pm.profile_store();
    let profile = ps
        .get(&id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, format!("profile '{id}' not found")))?;
    let data_dir = ps.resolve_data_dir(&profile);

    // Read session file directly (read-only, no side effects)
    let sm = octos_bus::SessionManager::open(&data_dir)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let key = octos_core::SessionKey(query.key.clone());
    let session = sm.load(&key).await.ok_or((
        StatusCode::NOT_FOUND,
        format!("session '{}' not found", query.key),
    ))?;

    let max_lines = query.lines.min(200);
    let messages = session.get_history(max_lines);
    let msg_json: Vec<serde_json::Value> = messages
        .iter()
        .map(|m| {
            let mut obj = serde_json::json!({
                "role": m.role.as_str(),
                "content": truncate_str(&m.content, 500),
            });
            if let Some(ref tc) = m.tool_calls {
                if !tc.is_empty() {
                    obj["tool_calls"] = serde_json::json!(
                        tc.iter()
                            .map(|t| {
                                serde_json::json!({
                                    "name": t.name,
                                    "arguments": truncate_str(&t.arguments.to_string(), 200),
                                })
                            })
                            .collect::<Vec<_>>()
                    );
                }
            }
            if let Some(ref name) = m.tool_call_id {
                obj["tool_call_id"] = serde_json::json!(name);
            }
            obj
        })
        .collect();

    Ok(Json(serde_json::json!({
        "profile_id": id,
        "session_key": query.key,
        "total_messages": session.messages.len(),
        "returned": msg_json.len(),
        "created_at": session.created_at.to_rfc3339(),
        "updated_at": session.updated_at.to_rfc3339(),
        "messages": msg_json,
    })))
}

/// Truncate a string to max_len chars, appending "..." if truncated.
/// Safe for multi-byte UTF-8 (truncates at char boundary).
pub(crate) fn truncate_str(s: &str, max_len: usize) -> String {
    if s.chars().count() <= max_len {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_len).collect();
        format!("{truncated}...")
    }
}

/// GET /api/admin/profiles/:id/cron — List cron jobs for a profile.
pub async fn list_cron_jobs(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let pm = state.process_manager.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;
    let ps = pm.profile_store();
    let profile = ps
        .get(&id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, format!("profile '{id}' not found")))?;
    let data_dir = ps.resolve_data_dir(&profile);
    let cron_path = data_dir.join("cron.json");

    if !cron_path.exists() {
        return Ok(Json(serde_json::json!({
            "profile_id": id,
            "count": 0,
            "jobs": [],
        })));
    }

    let content = tokio::fs::read_to_string(&cron_path).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to read cron.json: {e}"),
        )
    })?;
    let store: octos_bus::CronStore = serde_json::from_str(&content).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to parse cron.json: {e}"),
        )
    })?;

    let now_ms = Utc::now().timestamp_millis();
    let jobs: Vec<serde_json::Value> = store
        .jobs
        .iter()
        .map(|j| {
            let next_in = j.state.next_run_at_ms.map(|t| {
                let secs = (t - now_ms) / 1000;
                if secs < 0 {
                    "overdue".to_string()
                } else if secs < 60 {
                    format!("{secs}s")
                } else if secs < 3600 {
                    format!("{}m", secs / 60)
                } else {
                    format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
                }
            });
            let last_run = j.state.last_run_at_ms.map(|t| {
                chrono::DateTime::from_timestamp_millis(t)
                    .map(|dt| dt.to_rfc3339())
                    .unwrap_or_default()
            });
            serde_json::json!({
                "id": j.id,
                "name": j.name,
                "enabled": j.enabled,
                "schedule": j.schedule.for_display(),
                "message": truncate_str(&j.payload.message, 100),
                "channel": j.payload.channel,
                "last_run": last_run,
                "last_status": j.state.last_status,
                "next_in": next_in,
                "timezone": j.timezone,
            })
        })
        .collect();

    Ok(Json(serde_json::json!({
        "profile_id": id,
        "count": jobs.len(),
        "jobs": jobs,
    })))
}

/// GET /api/admin/profiles/:id/config-check — Check runtime config for a profile.
pub async fn config_check(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let pm = state.process_manager.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;
    let ps = pm.profile_store();
    let profile = ps
        .get(&id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, format!("profile '{id}' not found")))?;
    let data_dir = ps.resolve_data_dir(&profile);

    // Check which env vars are set (names only, not values)
    let env_var_names: Vec<String> = profile.config.env_vars.keys().cloned().collect();
    let env_var_refs = collect_env_var_refs(&profile.config);

    // Check email config
    let email_status = if let Some(ref email) = profile.config.email {
        let has_host = email.smtp_host.is_some();
        let has_user = email.username.is_some();
        let has_password = email.password.is_some() || email.password_env.is_some();
        let has_from = email.from_address.is_some();
        serde_json::json!({
            "configured": has_host && has_user && has_password,
            "smtp_host": has_host,
            "username": has_user,
            "password": has_password,
            "from_address": has_from,
            "smtp_port": email.smtp_port,
        })
    } else {
        serde_json::json!({ "configured": false })
    };

    // Check channels
    let channels: Vec<&str> = profile
        .config
        .channels
        .iter()
        .map(|c| match c {
            crate::profiles::ChannelCredentials::Telegram { .. } => "telegram",
            crate::profiles::ChannelCredentials::Discord { .. } => "discord",
            crate::profiles::ChannelCredentials::DingTalk { .. } => "dingtalk",
            crate::profiles::ChannelCredentials::Slack { .. } => "slack",
            crate::profiles::ChannelCredentials::WhatsApp { .. } => "whatsapp",
            crate::profiles::ChannelCredentials::Feishu { .. } => "feishu",
            crate::profiles::ChannelCredentials::Email { .. } => "email",
            crate::profiles::ChannelCredentials::Twilio { .. } => "twilio",
            crate::profiles::ChannelCredentials::Api { .. } => "api",
            crate::profiles::ChannelCredentials::WeComBot { .. } => "wecom-bot",
            crate::profiles::ChannelCredentials::Matrix { .. } => "matrix",
            crate::profiles::ChannelCredentials::QQBot { .. } => "qq-bot",
            crate::profiles::ChannelCredentials::WeChat { .. } => "wechat",
            crate::profiles::ChannelCredentials::Line { .. } => "line",
        })
        .collect();

    // Check LLM provider
    let provider = profile.config.primary_provider().unwrap_or("unknown");
    let model = profile.config.primary_model().unwrap_or("unknown");

    // Check skills
    let skills_dir = data_dir.join("skills");
    let installed_skills: Vec<String> = if skills_dir.exists() {
        std::fs::read_dir(&skills_dir)
            .ok()
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|e| e.path().is_dir())
                    .filter_map(|e| e.file_name().into_string().ok())
                    .collect()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    // Check data dir sizes
    let sessions_count = std::fs::read_dir(data_dir.join("sessions"))
        .ok()
        .map(|e| e.flatten().count())
        .unwrap_or(0);
    let has_cron = data_dir.join("cron.json").exists();

    // Check gateway running status
    let status = pm.status(&id).await;

    Ok(Json(serde_json::json!({
        "profile_id": id,
        "name": profile.name,
        "enabled": profile.enabled,
        "provider": provider,
        "model": model,
        "channels": channels,
        "email": email_status,
        "env_vars": env_var_names,
        "env_var_refs": env_var_refs,
        "env_var_semantics": {
            "provisioning": "user_supplied",
            "unset_status": "awaiting_user_secret",
            "note": "Clean installs do not pre-provision LLM or tool API keys."
        },
        "installed_skills": installed_skills,
        "sessions_count": sessions_count,
        "has_cron_jobs": has_cron,
        "gateway_status": {
            "running": status.running,
            "pid": status.pid,
            "uptime_secs": status.uptime_secs,
        },
    })))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct EnvVarReferenceStatus {
    name: String,
    surfaces: Vec<String>,
    configured: bool,
    status: &'static str,
    provisioning: &'static str,
}

fn collect_env_var_refs(config: &ProfileConfig) -> Vec<EnvVarReferenceStatus> {
    use std::collections::{BTreeMap, BTreeSet};

    let mut refs: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut insert_ref = |name: &str, surface: &str| {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return;
        }
        refs.entry(trimmed.to_string())
            .or_default()
            .insert(surface.to_string());
    };

    if let Some(primary) = config
        .llm
        .as_ref()
        .and_then(|llm| llm.primary.as_ref())
        .and_then(|selection| selection.route.as_ref())
        .and_then(|route| route.api_key_env.as_deref())
    {
        insert_ref(primary, "llm");
    }

    if let Some(llm) = config.llm.as_ref() {
        for fallback in &llm.fallbacks {
            if let Some(api_key_env) = fallback
                .route
                .as_ref()
                .and_then(|route| route.api_key_env.as_deref())
            {
                insert_ref(api_key_env, "llm");
            }
        }
    }

    if let Some(search) = config.search.as_ref() {
        for provider in search.providers.values() {
            if let Some(api_key_env) = provider.api_key_env.as_deref() {
                insert_ref(api_key_env, "tools");
            }
        }
    }

    if let Some(email) = config.email.as_ref() {
        if let Some(password_env) = email.password_env.as_deref() {
            insert_ref(password_env, "tools");
        }
        if let Some(feishu_secret_env) = email.feishu_app_secret_env.as_deref() {
            insert_ref(feishu_secret_env, "tools");
        }
    }

    for channel in &config.channels {
        match channel {
            crate::profiles::ChannelCredentials::Telegram { token_env, .. } => {
                insert_ref(token_env, "channels");
            }
            crate::profiles::ChannelCredentials::Discord { token_env, .. } => {
                insert_ref(token_env, "channels");
            }
            crate::profiles::ChannelCredentials::DingTalk {
                webhook_url_env,
                secret_env,
                ..
            } => {
                insert_ref(webhook_url_env, "channels");
                insert_ref(secret_env, "channels");
            }
            crate::profiles::ChannelCredentials::Slack {
                bot_token_env,
                app_token_env,
            } => {
                insert_ref(bot_token_env, "channels");
                insert_ref(app_token_env, "channels");
            }
            crate::profiles::ChannelCredentials::Feishu {
                app_id_env,
                app_secret_env,
                verification_token_env,
                encrypt_key_env,
                ..
            } => {
                insert_ref(app_id_env, "channels");
                insert_ref(app_secret_env, "channels");
                insert_ref(verification_token_env, "channels");
                insert_ref(encrypt_key_env, "channels");
            }
            crate::profiles::ChannelCredentials::Email {
                username_env,
                password_env,
                ..
            } => {
                insert_ref(username_env, "channels");
                insert_ref(password_env, "channels");
            }
            crate::profiles::ChannelCredentials::Twilio {
                account_sid_env,
                auth_token_env,
                ..
            } => {
                insert_ref(account_sid_env, "channels");
                insert_ref(auth_token_env, "channels");
            }
            crate::profiles::ChannelCredentials::WeComBot { secret_env, .. } => {
                insert_ref(secret_env, "channels");
            }
            crate::profiles::ChannelCredentials::QQBot {
                client_secret_env, ..
            } => {
                insert_ref(client_secret_env, "channels");
            }
            crate::profiles::ChannelCredentials::WeChat { token_env, .. } => {
                insert_ref(token_env, "channels");
            }
            crate::profiles::ChannelCredentials::Line {
                channel_secret_env,
                channel_access_token_env,
                ..
            } => {
                insert_ref(channel_secret_env, "channels");
                insert_ref(channel_access_token_env, "channels");
            }
            crate::profiles::ChannelCredentials::WhatsApp { .. }
            | crate::profiles::ChannelCredentials::Api { .. }
            | crate::profiles::ChannelCredentials::Matrix { .. } => {}
        }
    }

    refs.into_iter()
        .map(|(name, surfaces)| {
            let configured = config
                .env_vars
                .get(&name)
                .is_some_and(|value| !value.trim().is_empty());
            EnvVarReferenceStatus {
                name,
                surfaces: surfaces.into_iter().collect(),
                configured,
                status: if configured {
                    "set"
                } else {
                    "awaiting_user_secret"
                },
                provisioning: "user_supplied",
            }
        })
        .collect()
}

/// GET /api/admin/model-limits — returns model catalog (runtime source of truth).
pub async fn model_limits() -> Json<serde_json::Value> {
    // Read the runtime catalog from the profile data dir
    let home = std::env::var("HOME").unwrap_or_default();
    for base in &[
        format!("{home}/.octos/profiles"),
        format!("{home}/.crew/profiles"),
    ] {
        if let Ok(entries) = std::fs::read_dir(base) {
            for entry in entries.flatten() {
                let path = entry.path().join("data/model_catalog.json");
                if let Ok(content) = std::fs::read_to_string(&path) {
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) {
                        return Json(value);
                    }
                }
            }
        }
    }
    // Fallback to shared catalog
    let shared = format!("{home}/.octos/model_catalog.json");
    if let Ok(content) = std::fs::read_to_string(&shared) {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) {
            return Json(value);
        }
    }
    Json(serde_json::json!({"models": []}))
}

// ── Admin Shell API ─────────────────────────────────────────────────

/// Maximum command length (1MB).
const MAX_SHELL_COMMAND_LEN: usize = 1_048_576;

/// Default shell timeout in seconds.
const DEFAULT_SHELL_TIMEOUT: u64 = 30;

/// Maximum shell timeout in seconds.
const MAX_SHELL_TIMEOUT: u64 = 600;

#[derive(Deserialize)]
pub struct ShellRequest {
    pub command: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct ShellResponse {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub timed_out: bool,
}

/// POST /api/admin/shell — execute a shell command on the server.
///
/// Admin-only. Runs the command with timeout enforcement and returns
/// stdout, stderr, and exit code. No PTY — stdin/stdout only.
pub async fn admin_shell(
    Json(req): Json<ShellRequest>,
) -> Result<Json<ShellResponse>, (StatusCode, String)> {
    if req.command.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "command is required".into()));
    }
    if req.command.len() > MAX_SHELL_COMMAND_LEN {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("command exceeds {}KB limit", MAX_SHELL_COMMAND_LEN / 1024),
        ));
    }

    let timeout_secs = req
        .timeout_secs
        .unwrap_or(DEFAULT_SHELL_TIMEOUT)
        .clamp(1, MAX_SHELL_TIMEOUT);

    // Determine working directory
    let cwd = req.cwd.as_deref().unwrap_or(".");
    let cwd_path = std::path::Path::new(cwd);
    if !cwd_path.exists() {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("working directory does not exist: {cwd}"),
        ));
    }

    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c").arg(&req.command).current_dir(cwd_path);

    // Sanitize environment — remove dangerous env vars
    for var in octos_agent::sandbox::BLOCKED_ENV_VARS {
        cmd.env_remove(var);
    }

    let cmd_preview = octos_core::truncated_utf8(&req.command, 200, "...");
    tracing::info!(
        command = %cmd_preview,
        cwd = %cwd,
        timeout = timeout_secs,
        "admin shell: executing"
    );

    // Spawn child explicitly so we can kill it on timeout (dropping the
    // future does NOT kill the child — it becomes an orphan process).
    let child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| {
            tracing::error!(error = %e, "admin shell: failed to spawn");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to spawn command: {e}"),
            )
        })?;

    // Capture PID before wait_with_output() takes ownership
    let child_pid = child.id();

    match tokio::time::timeout(
        std::time::Duration::from_secs(timeout_secs),
        child.wait_with_output(),
    )
    .await
    {
        Ok(Ok(output)) => {
            let exit_code = output.status.code().unwrap_or(-1);
            tracing::info!(exit_code, "admin shell: complete");
            Ok(Json(ShellResponse {
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                exit_code,
                timed_out: false,
            }))
        }
        Ok(Err(e)) => {
            tracing::error!(error = %e, "admin shell: failed to execute");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to execute command: {e}"),
            ))
        }
        Err(_) => {
            // Kill the child process on timeout
            if let Some(pid) = child_pid {
                let _ = tokio::process::Command::new("kill")
                    .args(["-9", &pid.to_string()])
                    .output()
                    .await;
            }
            tracing::warn!(
                timeout = timeout_secs,
                "admin shell: timed out, process killed"
            );
            Ok(Json(ShellResponse {
                stdout: String::new(),
                stderr: format!("command timed out after {timeout_secs}s"),
                exit_code: -1,
                timed_out: true,
            }))
        }
    }
}

// ── Tenant tunnel management ────────────────────────────────────────

/// Tenant summary without secrets (for list responses).
#[derive(Serialize)]
pub struct TenantSummary {
    pub id: String,
    pub name: String,
    pub subdomain: String,
    pub ssh_port: u16,
    pub local_port: u16,
    pub status: crate::tenant::TenantStatus,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

impl From<crate::tenant::TenantConfig> for TenantSummary {
    fn from(t: crate::tenant::TenantConfig) -> Self {
        Self {
            id: t.id,
            name: t.name,
            subdomain: t.subdomain,
            ssh_port: t.ssh_port,
            local_port: t.local_port,
            status: t.status,
            created_at: t.created_at,
            updated_at: t.updated_at,
        }
    }
}

/// GET /api/admin/tenants — list all tunnel tenants (secrets masked).
pub async fn list_tenants(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<TenantSummary>>, (StatusCode, String)> {
    let store = state.tenant_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "tenant store not configured".into(),
    ))?;
    let tenants = store
        .list()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(tenants.into_iter().map(TenantSummary::from).collect()))
}

/// GET /api/admin/tenants/{id} — get a single tenant.
pub async fn get_tenant(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<crate::tenant::TenantConfig>, (StatusCode, String)> {
    let store = state.tenant_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "tenant store not configured".into(),
    ))?;
    let tenant = store
        .get(&id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, format!("tenant '{id}' not found")))?;
    Ok(Json(tenant))
}

#[derive(Deserialize)]
pub struct CreateTenantRequest {
    pub name: String,
    #[serde(default = "default_local_port")]
    pub local_port: u16,
}

fn default_local_port() -> u16 {
    8080
}

/// POST /api/admin/tenants — create a new tunnel tenant.
pub async fn create_tenant(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateTenantRequest>,
) -> Result<Json<crate::tenant::TenantConfig>, (StatusCode, String)> {
    // Validate tenant name (must match TenantStore rules: lowercase alnum + hyphens,
    // no leading/trailing hyphens, max 64 chars)
    use std::sync::LazyLock;
    static TENANT_NAME_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"^[a-z0-9][a-z0-9-]{0,62}[a-z0-9]$|^[a-z0-9]$").unwrap()
    });
    if !TENANT_NAME_RE.is_match(&req.name) {
        return Err((StatusCode::BAD_REQUEST, "Tenant name must be 1-64 lowercase alphanumeric characters or hyphens, cannot start or end with a hyphen".to_string()));
    }

    let store = state.tenant_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "tenant store not configured".into(),
    ))?;

    // Check for duplicate
    if store
        .get(&req.name)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .is_some()
    {
        return Err((
            StatusCode::CONFLICT,
            format!("tenant '{}' already exists", req.name),
        ));
    }

    let ssh_port = store
        .next_ssh_port()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let now = chrono::Utc::now();
    let tenant = crate::tenant::TenantConfig {
        id: req.name.clone(),
        name: req.name.clone(),
        subdomain: req.name.clone(),
        tunnel_token: uuid::Uuid::new_v4().to_string(),
        ssh_port,
        local_port: req.local_port,
        auth_token: format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        ),
        owner: String::new(),
        status: crate::tenant::TenantStatus::Pending,
        created_at: now,
        updated_at: now,
    };

    store
        .save(&tenant)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(tenant))
}

/// DELETE /api/admin/tenants/{id} — delete a tenant.
pub async fn delete_tenant(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<ActionResponse>, (StatusCode, String)> {
    let store = state.tenant_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "tenant store not configured".into(),
    ))?;
    let deleted = store
        .delete(&id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if !deleted {
        return Err((StatusCode::NOT_FOUND, format!("tenant '{id}' not found")));
    }
    Ok(Json(ActionResponse {
        ok: true,
        message: Some(format!("tenant '{id}' deleted")),
    }))
}

/// The tunnel relay (frps) address this node hands out in setup scripts.
/// There is no built-in default: it must be set as `frps_server` in the
/// node config, otherwise tenant setup is refused with a clear error.
fn required_frps_server(state: &AppState) -> Result<&str, (StatusCode, String)> {
    state
        .frps_server
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or((
            StatusCode::SERVICE_UNAVAILABLE,
            "tunnel relay not configured: set `frps_server` in this node's config".into(),
        ))
}

/// GET /api/admin/tenants/{id}/setup-script — returns a bash one-liner that
/// installs octos + frpc on a fresh Mac Mini.
pub async fn tenant_setup_script(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<String, (StatusCode, String)> {
    let store = state.tenant_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "tenant store not configured".into(),
    ))?;
    let tenant = store
        .get(&id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, format!("tenant '{id}' not found")))?;

    let domain = state.tunnel_domain.as_deref().unwrap_or("octos-cloud.org");
    let server = required_frps_server(&state)?;
    let script = build_admin_tenant_setup_script(&tenant, domain, server);

    Ok(script)
}

fn build_admin_tenant_setup_script(
    tenant: &crate::tenant::TenantConfig,
    domain: &str,
    server: &str,
) -> String {
    let install_url = "https://github.com/octos-org/octos/releases/latest/download/install.sh";
    format!(
        r#"#!/usr/bin/env bash
# Setup script for {subdomain}.{domain}
# Downloads and runs install.sh with your tenant configuration pre-filled.
# Per-tenant tunnel token is embedded — no shared FRPS token needed.
set -euo pipefail

curl -fsSL "{install_url}" | bash -s -- \
    --tenant-name "{subdomain}" \
    --frps-token "{tunnel_token}" \
    --ssh-port {ssh_port} \
    --domain "{domain}" \
    --frps-server "{server}" \
    --auth-token "{auth_token}"
"#,
        subdomain = tenant.subdomain,
        domain = domain,
        server = server,
        ssh_port = tenant.ssh_port,
        install_url = install_url,
        tunnel_token = tenant.tunnel_token,
        auth_token = tenant.auth_token,
    )
}

// ── Self-service tenant registration (user-auth level) ──────────────

/// POST /api/register — create a tenant for the authenticated user.
///
/// Limited to one tenant per email. Accepts the same `CreateTenantRequest`
/// body as the admin endpoint but associates the tenant with the caller's
/// email and enforces a one-tenant-per-user limit.
pub async fn register_tenant(
    State(state): State<Arc<AppState>>,
    axum::Extension(identity): axum::Extension<super::router::AuthIdentity>,
    Json(req): Json<CreateTenantRequest>,
) -> Result<Json<RegisterResponse>, (StatusCode, String)> {
    // Self-registration is only available in cloud mode
    if !matches!(state.deployment_mode, crate::config::DeploymentMode::Cloud) {
        return Err((StatusCode::NOT_FOUND, "not found".into()));
    }

    let user_id = match &identity {
        super::router::AuthIdentity::Admin => {
            return Err((
                StatusCode::BAD_REQUEST,
                "admin token cannot self-register; use /api/admin/tenants instead".into(),
            ));
        }
        super::router::AuthIdentity::User { id, .. } => id.clone(),
    };

    // Validate tenant name (must match TenantStore rules: lowercase alnum + hyphens,
    // no leading/trailing hyphens, max 64 chars)
    use std::sync::LazyLock;
    static TENANT_NAME_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"^[a-z0-9][a-z0-9-]{0,62}[a-z0-9]$|^[a-z0-9]$").unwrap()
    });
    if !TENANT_NAME_RE.is_match(&req.name) {
        return Err((
            StatusCode::BAD_REQUEST,
            "Tenant name must be 1-64 lowercase alphanumeric characters or hyphens, cannot start or end with a hyphen".into(),
        ));
    }

    let store = state.tenant_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "tenant store not configured".into(),
    ))?;

    // Resolve user's email for legacy tenant matching
    let user_email = state
        .user_store
        .as_ref()
        .and_then(|us| us.get(&user_id).ok().flatten())
        .map(|u| u.email)
        .unwrap_or_default();
    let owner_ids: Vec<&str> = [user_id.as_str(), user_email.as_str()]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect();

    // One tenant per user
    let existing = store
        .find_by_owner(&owner_ids)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if !existing.is_empty() {
        return Err((
            StatusCode::CONFLICT,
            format!(
                "you already have a tenant: '{}'. Only one tenant per account.",
                existing[0].id
            ),
        ));
    }

    // Check for duplicate tenant name
    if store
        .get(&req.name)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .is_some()
    {
        return Err((
            StatusCode::CONFLICT,
            format!("tenant name '{}' is already taken", req.name),
        ));
    }

    // Fail before creating the tenant if this node has no tunnel relay.
    // (#2344: the value no longer leaves this handler — the registration
    // email and response no longer embed setup commands.)
    required_frps_server(&state)?;

    let ssh_port = store
        .next_ssh_port()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let now = chrono::Utc::now();
    let tenant = crate::tenant::TenantConfig {
        id: req.name.clone(),
        name: req.name.clone(),
        subdomain: req.name.clone(),
        tunnel_token: uuid::Uuid::new_v4().to_string(),
        ssh_port,
        local_port: req.local_port,
        auth_token: format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        ),
        owner: user_id.clone(),
        status: crate::tenant::TenantStatus::Pending,
        created_at: now,
        updated_at: now,
    };

    store
        .save(&tenant)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let domain = state.tunnel_domain.as_deref().unwrap_or("octos-cloud.org");
    let dashboard_url = format!("https://{}.{}", tenant.subdomain, domain);

    let mut email_sent = false;
    if !user_email.is_empty() {
        if let Some(auth_manager) = state.auth_manager.as_ref() {
            let (subject, html) = build_register_setup_email(&tenant, domain);
            match auth_manager
                .send_html_email(&user_email, &subject, &html)
                .await
            {
                Ok(true) => {
                    email_sent = true;
                }
                Ok(false) => { /* SMTP not configured — skip silently */ }
                Err(e) => {
                    tracing::warn!(
                        email = %user_email,
                        tenant = %tenant.id,
                        error = %e,
                        "failed to send managed tenant setup email"
                    );
                }
            }
        }
    }

    Ok(Json(RegisterResponse {
        id: tenant.id.clone(),
        subdomain: tenant.subdomain.clone(),
        ssh_port: tenant.ssh_port,
        auth_token: tenant.auth_token.clone(),
        dashboard_url,
        status: tenant.status.clone(),
        email_sent,
    }))
}

/// Response from POST /api/register — only fields the client needs.
/// Excludes tunnel_token and owner (internal/secret).
#[derive(Debug, Serialize)]
pub struct RegisterResponse {
    pub id: String,
    pub subdomain: String,
    pub ssh_port: u16,
    pub auth_token: String,
    pub dashboard_url: String,
    pub status: crate::tenant::TenantStatus,
    /// One-liner install command for macOS/Linux.
    /// Whether the setup details were emailed to the user.
    pub email_sent: bool,
}

/// GET /api/register/setup-script — returns the setup script for the
/// authenticated user's tenant.
pub async fn register_setup_script(
    State(state): State<Arc<AppState>>,
    axum::Extension(identity): axum::Extension<super::router::AuthIdentity>,
) -> Result<String, (StatusCode, String)> {
    if !matches!(state.deployment_mode, crate::config::DeploymentMode::Cloud) {
        return Err((StatusCode::NOT_FOUND, "not found".into()));
    }

    let user_id = match &identity {
        super::router::AuthIdentity::Admin => {
            return Err((
                StatusCode::BAD_REQUEST,
                "admin token cannot self-register; use /api/admin/tenants/{id}/setup-script instead"
                    .into(),
            ));
        }
        super::router::AuthIdentity::User { id, .. } => id.clone(),
    };

    let store = state.tenant_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "tenant store not configured".into(),
    ))?;

    // Resolve user's email for legacy tenant matching
    let user_email = state
        .user_store
        .as_ref()
        .and_then(|us| us.get(&user_id).ok().flatten())
        .map(|u| u.email)
        .unwrap_or_default();
    let owner_ids: Vec<&str> = [user_id.as_str(), user_email.as_str()]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect();

    let tenants = store
        .find_by_owner(&owner_ids)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let tenant = tenants.into_iter().next().ok_or((
        StatusCode::NOT_FOUND,
        "no tenant found for your account — register one first via POST /api/register".into(),
    ))?;

    let domain = state.tunnel_domain.as_deref().unwrap_or("octos-cloud.org");
    let server = required_frps_server(&state)?;
    let script = build_register_setup_script(&tenant, domain, server);

    Ok(script)
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn build_register_setup_script(
    tenant: &crate::tenant::TenantConfig,
    domain: &str,
    server: &str,
) -> String {
    let install_url = "https://github.com/octos-org/octos/releases/latest/download/install.sh";
    format!(
        r#"#!/usr/bin/env bash
# Setup script for {subdomain}.{domain}
# Downloads and runs install.sh as a managed tenant bootstrap.
# Per-tenant tunnel token is embedded — no shared FRPS token needed.
set -euo pipefail

curl -fsSL "{install_url}" | bash -s -- \
    --tunnel \
    --auth-token "{auth_token}" \
    --port {local_port} \
    --tenant-name "{subdomain}" \
    --frps-token "{tunnel_token}" \
    --ssh-port {ssh_port} \
    --domain "{domain}" \
    --frps-server "{server}"
"#,
        subdomain = tenant.subdomain,
        domain = domain,
        install_url = install_url,
        auth_token = tenant.auth_token,
        local_port = tenant.local_port,
        tunnel_token = tenant.tunnel_token,
        ssh_port = tenant.ssh_port,
        server = server,
    )
}

/// Minimal HTML escaping for values interpolated into email HTML.
fn build_register_setup_email(
    tenant: &crate::tenant::TenantConfig,
    domain: &str,
) -> (String, String) {
    let public_url = format!(
        "https://{}.{}",
        html_escape(&tenant.subdomain),
        html_escape(domain)
    );
    let subject = format!("octos setup for {}", tenant.subdomain);
    let html = format!(
        r#"<div style="font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif; max-width: 720px; margin: 0 auto; padding: 32px 20px;">
    <h2 style="color: #1a1a2e; margin-bottom: 8px;">Your octos machine setup</h2>
    <p style="color: #444; margin-bottom: 20px;">Your machine <strong>{public_url}</strong> has been registered. To install octos on it, sign in to the dashboard and download the machine setup from there — the setup script is delivered over your authenticated session and never through a link in email.</p>
    <div style="background: #f5f5f5; border-radius: 10px; padding: 20px; margin-bottom: 20px;">
        <p style="margin: 0 0 8px 0;"><strong>Machine name:</strong> {subdomain}</p>
        <p style="margin: 0 0 8px 0;"><strong>Public URL:</strong> {public_url}</p>
        <p style="margin: 0 0 8px 0;"><strong>SSH port:</strong> {ssh_port}</p>
    </div>
    <p style="color: #444; margin-bottom: 8px;">Next step:</p>
    <p style="color: #444; margin: 0;">Sign in at <strong>{public_url}</strong> and open <strong>Machine setup</strong> in the dashboard to download the install script.</p>
    <p style="color: #777; font-size: 13px; margin-top: 20px;">This email intentionally contains no credentials or install URLs (#2344).</p>
</div>"#,
        subdomain = html_escape(&tenant.subdomain),
        public_url = public_url,
        ssh_port = tenant.ssh_port,
    );
    (subject, html)
}

// ---------------------------------------------------------------------------
// WeChat QR Login
// ---------------------------------------------------------------------------

#[derive(serde::Serialize)]
pub struct WeChatQrStartResponse {
    pub qrcode_url: String,
    pub session_key: String,
}

/// GET /api/admin/profiles/{id}/wechat/qr-start
pub async fn wechat_qr_start(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<WeChatQrStartResponse>, (StatusCode, String)> {
    // Fail fast before the user scans a QR bound for a profile that does not
    // exist — the poll below would have nowhere to land the token.
    require_admin_profile(&state, &id)?;

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
        .ok_or((StatusCode::BAD_GATEWAY, "missing qrcode field".into()))?
        .to_string();
    let qrcode_url = body["qrcode_img_content"]
        .as_str()
        .ok_or((StatusCode::BAD_GATEWAY, "missing qrcode_img_content".into()))?
        .to_string();

    Ok(Json(WeChatQrStartResponse {
        qrcode_url,
        session_key: qrcode,
    }))
}

#[derive(serde::Deserialize)]
pub struct WeChatQrPollRequest {
    pub session_key: String,
}

#[derive(serde::Serialize)]
pub struct WeChatQrPollResponse {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bot_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bot_id: Option<String>,
}

/// Load the named profile or fail the request: a QR flow for a profile that
/// does not exist would report "confirmed" while the token lands nowhere.
fn require_admin_profile(
    state: &Arc<AppState>,
    id: &str,
) -> Result<Arc<crate::profiles::ProfileStore>, (StatusCode, String)> {
    let store = state.profile_store.clone().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;
    store
        .get(id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, format!("profile '{id}' not found")))?;
    Ok(store)
}

/// POST /api/admin/profiles/{id}/wechat/qr-poll
pub async fn wechat_qr_poll(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<WeChatQrPollRequest>,
) -> Result<Json<WeChatQrPollResponse>, (StatusCode, String)> {
    let store = require_admin_profile(&state, &id)?;

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(40))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let encoded_key: String = req
        .session_key
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
        encoded_key
    );
    let resp = client
        .get(&url)
        .header("iLink-App-ClientVersion", "1")
        .send()
        .await
        .map_err(|e| {
            if e.is_timeout() {
                return (StatusCode::OK, "".into());
            }
            (StatusCode::BAD_GATEWAY, format!("poll failed: {e}"))
        })?;
    let body: serde_json::Value = resp.json().await.map_err(|e| {
        (
            StatusCode::BAD_GATEWAY,
            format!("invalid poll response: {e}"),
        )
    })?;

    let status = body["status"].as_str().unwrap_or("wait").to_string();

    if status == "confirmed" {
        let bot_token = body["bot_token"].as_str().unwrap_or_default().to_string();
        let bot_id = body["ilink_bot_id"]
            .as_str()
            .unwrap_or_default()
            .to_string();

        // Save token to the profile being edited
        super::auth_handlers::persist_wechat_bot_token(&store, &id, &bot_token);

        // Don't expose bot_token to the client — it's already saved server-side
        return Ok(Json(WeChatQrPollResponse {
            status,
            bot_token: None,
            bot_id: Some(bot_id),
        }));
    }

    Ok(Json(WeChatQrPollResponse {
        status,
        bot_token: None,
        bot_id: None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serve_log_redaction_masks_credentials() {
        let filter = ServeLogFilter {
            grep: None,
            since: None,
        };
        let lines = filter_serve_log_lines(
            [
                "2026-05-24T10:00:03Z ERROR Authorization: Bearer abcdef0123456789ABCDEF0123 url=/x?token=secret123 key=sk-testsecret12345",
            ]
            .into_iter(),
            10,
            &filter,
        );
        assert_eq!(lines.len(), 1);
        let output = &lines[0];
        assert!(output.contains("Bearer [credential-redacted]"));
        assert!(output.contains("?token=[credential-redacted]"));
        assert!(!output.contains("abcdef0123456789ABCDEF0123"));
        assert!(!output.contains("secret123"));
        assert!(!output.contains("sk-testsecret12345"));
    }
}
