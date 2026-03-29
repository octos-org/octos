//! Admin API handlers for profile and gateway management.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use chrono::Utc;
use futures::StreamExt;
use serde::{Deserialize, Serialize};

use super::AppState;
use crate::profiles::{ProfileConfig, UserProfile, mask_secrets};

/// Basic email format validation.
fn validate_email(email: &str) -> Result<(), String> {
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

#[derive(Deserialize)]
pub struct CreateProfileRequest {
    pub id: String,
    pub name: String,
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
    pub enabled: Option<bool>,
    #[serde(default)]
    pub data_dir: Option<Option<String>>,
    #[serde(default)]
    pub config: Option<ProfileConfig>,
    /// Set or update the email address for OTP login.
    #[serde(default)]
    pub email: Option<String>,
}

#[derive(Serialize)]
pub struct ProfileResponse {
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
        }
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
            email: None,
            profile: mask_secrets(&profile),
            status,
        }
        .with_email_lookup(state.user_store.as_deref()),
    ))
}

/// POST /api/admin/profiles
pub async fn create_profile(
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
    let profile = UserProfile {
        id: req.id,
        name: req.name,
        enabled: req.enabled,
        data_dir: req.data_dir,
        parent_id: None,
        config: req.config,
        created_at: now,
        updated_at: now,
    };

    store.save(&profile).map_err(|e| {
        tracing::error!(profile = %profile.id, error = %e, "failed to create profile");
        (StatusCode::BAD_REQUEST, e.to_string())
    })?;

    tracing::info!(profile = %profile.id, name = %profile.name, "profile created");
    let status = pm.status(&profile.id).await;
    Ok((
        StatusCode::CREATED,
        Json(ProfileResponse {
            email: None,
            profile: mask_secrets(&profile),
            status,
        }),
    ))
}

/// PUT /api/admin/profiles/:id
pub async fn update_profile(
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

    if let Some(name) = req.name {
        profile.name = name;
    }
    if let Some(enabled) = req.enabled {
        profile.enabled = enabled;
    }
    if let Some(data_dir) = req.data_dir {
        profile.data_dir = data_dir;
    }
    // Merge config: parse the raw JSON "config" object and overlay only the
    // keys that are explicitly present, preserving all other existing fields.
    // This lets the admin tool send `{"config":{"model":"x"}}` without wiping
    // channels/env_vars, while the dashboard can still send a full config object.
    {
        let raw: serde_json::Value = serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
        if let Some(config_patch) = raw.get("config") {
            if config_patch.is_object() {
                let mut existing =
                    serde_json::to_value(&profile.config).unwrap_or(serde_json::json!({}));
                json_merge(&mut existing, config_patch);
                if let Ok(merged) = serde_json::from_value(existing) {
                    profile.config = merged;
                }
            }
        }
    }
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
    Ok(Json(ProfileResponse {
        email: None,
        profile: mask_secrets(&profile),
        status,
    }))
}

/// DELETE /api/admin/profiles/:id
pub async fn delete_profile(
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
    if let Ok(subs) = store.list_sub_accounts(&id) {
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
    }

    let deleted = store
        .delete(&id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    if !deleted {
        return Err((StatusCode::NOT_FOUND, format!("profile '{id}' not found")));
    }

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
    if effective.config.provider.is_none() && effective.config.model.is_none() {
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
    let api_key = if let Some(ref key) = req.api_key {
        if !key.is_empty() && !key.contains("***") {
            key.clone()
        } else {
            resolve_saved_key(&state, &identity, &req)?
        }
    } else {
        resolve_saved_key(&state, &identity, &req)?
    };

    if api_key.is_empty() {
        return Ok(Json(TestProviderResponse {
            ok: false,
            message: String::new(),
            error: Some("No API key provided".into()),
        }));
    }

    let provider: Arc<dyn LlmProvider> = {
        let params = octos_llm::registry::CreateParams {
            api_key: Some(api_key.clone()),
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
        timestamp: chrono::Utc::now(),
    }];
    // Gemini 2.5+ "thinking" models consume tokens on internal reasoning,
    // so 16 tokens is too small — they return empty content.  Use 128 for
    // Gemini and keep 16 for everyone else (fast, cheap connectivity check).
    let max_tokens = if req.provider == "gemini" { 128 } else { 16 };
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
    let api_key = if let Some(ref key) = req.api_key {
        if !key.is_empty() && !key.contains("***") {
            key.clone()
        } else {
            resolve_saved_key(&state, &identity, &req)?
        }
    } else {
        resolve_saved_key(&state, &identity, &req)?
    };
    if api_key.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "No API key".into()));
    }
    let models = fetch_provider_models(&req.provider, &api_key, req.base_url.as_deref())
        .await
        .unwrap_or_default();
    Ok(Json(models))
}

/// Fetch available models from a provider's /v1/models endpoint.
async fn fetch_provider_models(
    provider: &str,
    api_key: &str,
    base_url: Option<&str>,
) -> Option<Vec<String>> {
    let base = base_url
        .map(|u| u.trim_end_matches('/').to_string())
        .or_else(|| {
            octos_llm::registry::lookup(provider)
                .and_then(|e| e.default_base_url.map(|u| u.to_string()))
        })?;
    let base_trimmed = base.trim_end_matches("/v1").trim_end_matches("/v1/");
    let url = if provider == "anthropic" {
        format!("{base}/v1/models")
    } else {
        format!("{base_trimmed}/v1/models")
    };
    let client = reqwest::Client::new();
    let mut req = client.get(&url).timeout(std::time::Duration::from_secs(10));
    if provider == "anthropic" {
        req = req
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01");
    } else {
        req = req.header("Authorization", format!("Bearer {api_key}"));
    }
    let resp = req.send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body: serde_json::Value = resp.json().await.ok()?;
    body.get("data").and_then(|d| d.as_array()).map(|arr| {
        let mut ids: Vec<String> = arr
            .iter()
            .filter_map(|m| m.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()))
            .collect();
        ids.sort();
        ids
    })
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

    Ok(profile
        .config
        .env_vars
        .get(env_name)
        .cloned()
        .unwrap_or_default())
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

fn resolve_saved_search_key(
    state: &AppState,
    identity: &Option<axum::Extension<super::router::AuthIdentity>>,
    req: &TestSearchRequest,
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

    let ps = state.profile_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "profile store not configured".into(),
    ))?;

    let profile_id: String = match identity {
        Some(axum::Extension(super::router::AuthIdentity::User { id, .. })) => id.clone(),
        Some(axum::Extension(super::router::AuthIdentity::Admin)) => {
            super::auth_handlers::ADMIN_PROFILE_ID.into()
        }
        None => {
            return Err((StatusCode::UNAUTHORIZED, "not authenticated".into()));
        }
    };

    let profile = ps
        .get(&profile_id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "profile not found".into()))?;

    Ok(profile
        .config
        .env_vars
        .get(env_name)
        .cloned()
        .unwrap_or_default())
}

#[derive(Deserialize)]
pub struct TestSearchRequest {
    /// Search provider: "perplexity", "brave", "you"
    pub provider: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub api_key_env: Option<String>,
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
    pub name: String,
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
            email: None,
            profile: mask_secrets(&s),
            status,
        });
    }
    Ok(Json(items))
}

/// Validate that channel credentials have the required fields populated.
/// Returns an error message if any channel is missing required fields.
fn validate_channel_credentials(
    channels: &[crate::profiles::ChannelCredentials],
) -> Result<(), String> {
    use crate::profiles::ChannelCredentials;
    for ch in channels {
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
            _ => {}
        }
    }
    Ok(())
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
            &req.name,
            req.channels,
            req.gateway.unwrap_or_default(),
        )
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;

    // Set channel-specific env vars if provided
    if !req.env_vars.is_empty() {
        sub.config.env_vars = req.env_vars;
        sub.updated_at = Utc::now();
        store
            .save(&sub)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }

    // Create a User entry so the sub-account can log in via OTP
    if let Some(email) = &req.email {
        let email = email.trim().to_lowercase();
        if !email.is_empty() {
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
                    email,
                    name: sub.name.clone(),
                    role: crate::user_store::UserRole::User,
                    created_at: Utc::now(),
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
        Json(ProfileResponse {
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
    sys.refresh_all();

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

// ── Monitor control endpoints ────────────────────────────────────────

/// GET /api/admin/monitor/status — returns watchdog/alerts status.
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

    Ok(Json(serde_json::json!({
        "watchdog_enabled": watchdog,
        "alerts_enabled": alerts,
    })))
}

#[derive(Deserialize)]
pub struct MonitorToggleRequest {
    pub enabled: bool,
}

/// POST /api/admin/monitor/watchdog — toggle watchdog.
pub async fn toggle_watchdog(
    State(state): State<Arc<AppState>>,
    Json(req): Json<MonitorToggleRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    if let Some(ref flag) = state.watchdog_enabled {
        flag.store(req.enabled, std::sync::atomic::Ordering::Relaxed);
    }
    Ok(Json(
        serde_json::json!({ "ok": true, "watchdog_enabled": req.enabled }),
    ))
}

/// POST /api/admin/monitor/alerts — toggle alerts.
pub async fn toggle_alerts(
    State(state): State<Arc<AppState>>,
    Json(req): Json<MonitorToggleRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    if let Some(ref flag) = state.alerts_enabled {
        flag.store(req.enabled, std::sync::atomic::Ordering::Relaxed);
    }
    Ok(Json(
        serde_json::json!({ "ok": true, "alerts_enabled": req.enabled }),
    ))
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
    pub repo: String,
    #[serde(default)]
    pub force: bool,
    #[serde(default = "default_branch")]
    pub branch: String,
}

fn default_branch() -> String {
    "main".to_string()
}

/// POST /api/admin/profiles/:id/skills — install a skill from GitHub.
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

    crate::commands::skills::remove_skill(&skills_dir, &name)
        .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;

    Ok(Json(ActionResponse {
        ok: true,
        message: Some(format!("Removed skill: {name}")),
    }))
}

// ── Platform Skills ──────────────────────────────────────────────────

fn ominix_api_url() -> String {
    std::env::var("OMINIX_API_URL").unwrap_or_else(|_| "http://localhost:8080".to_string())
}

fn models_dir() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    std::path::PathBuf::from(std::env::var("OMINIX_MODELS_DIR").unwrap_or_else(|_| {
        // Try both common locations
        let p1 = format!("{home}/.ominix/models");
        let p2 = format!("{home}/.OminiX/models");
        if std::path::Path::new(&p1).exists() {
            p1
        } else {
            p2
        }
    }))
}

/// GET /api/admin/platform-skills — list platform skills and their status.
pub async fn list_platform_skills(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let store = state.profile_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin not configured".into(),
    ))?;
    let skills_dir = store.octos_home_dir().join("skills");

    // List installed platform skills
    let installed = crate::commands::skills::list_skills(&skills_dir).unwrap_or_default();

    // Check ominix-api health
    let ominix_url = ominix_api_url();
    let health_url = format!("{}/health", ominix_url.trim_end_matches('/'));
    let ominix_healthy = state
        .http_client
        .get(&health_url)
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false);

    // Check launchd service status
    let service_status = tokio::process::Command::new("launchctl")
        .args(["list", "io.ominix.ominix-api"])
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false);

    // Check models against platform allowlist
    let mdir = models_dir();
    let allowlist = octos_llm::ominix::PlatformModels::load_or_create(store.octos_home_dir());
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
        },
        "models": {
            "dir": mdir.display().to_string(),
            "asr": asr_models,
            "tts": tts_models,
        }
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

    if octos_agent::bootstrap::bootstrap_single_skill(&octos_home, &name) {
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
    let skills_dir = store.octos_home_dir().join("skills");

    crate::commands::skills::remove_skill(&skills_dir, &name)
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
            let url = ominix_api_url();
            let health_url = format!("{}/health", url.trim_end_matches('/'));
            let result = state
                .http_client
                .get(&health_url)
                .timeout(std::time::Duration::from_secs(5))
                .send()
                .await;

            let (status, detail) = match result {
                Ok(resp) if resp.status().is_success() => {
                    let body: serde_json::Value = resp.json().await.unwrap_or_default();
                    ("healthy", body)
                }
                Ok(resp) => (
                    "error",
                    serde_json::json!({"http_status": resp.status().as_u16()}),
                ),
                Err(e) => ("unreachable", serde_json::json!({"error": e.to_string()})),
            };

            Ok(Json(serde_json::json!({
                "name": name,
                "status": status,
                "url": url,
                "detail": detail,
            })))
        }
        _ => Err((
            StatusCode::NOT_FOUND,
            format!("Unknown platform skill: {name}"),
        )),
    }
}

const OMINIX_PLIST: &str = "io.ominix.ominix-api";

/// POST /api/admin/platform-skills/ominix-api/start
pub async fn platform_service_start() -> Result<Json<ActionResponse>, (StatusCode, String)> {
    let plist_path = format!(
        "{}/Library/LaunchAgents/{OMINIX_PLIST}.plist",
        std::env::var("HOME").unwrap_or_default()
    );
    let output = tokio::process::Command::new("launchctl")
        .args(["load", &plist_path])
        .output()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    if output.status.success() {
        Ok(Json(ActionResponse {
            ok: true,
            message: Some("ominix-api service started".into()),
        }))
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Ok(Json(ActionResponse {
            ok: false,
            message: Some(format!("launchctl load failed: {stderr}")),
        }))
    }
}

/// POST /api/admin/platform-skills/ominix-api/stop
pub async fn platform_service_stop() -> Result<Json<ActionResponse>, (StatusCode, String)> {
    let plist_path = format!(
        "{}/Library/LaunchAgents/{OMINIX_PLIST}.plist",
        std::env::var("HOME").unwrap_or_default()
    );
    let output = tokio::process::Command::new("launchctl")
        .args(["unload", &plist_path])
        .output()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    if output.status.success() {
        Ok(Json(ActionResponse {
            ok: true,
            message: Some("ominix-api service stopped".into()),
        }))
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Ok(Json(ActionResponse {
            ok: false,
            message: Some(format!("launchctl unload failed: {stderr}")),
        }))
    }
}

/// POST /api/admin/platform-skills/ominix-api/restart
pub async fn platform_service_restart() -> Result<Json<ActionResponse>, (StatusCode, String)> {
    let plist_path = format!(
        "{}/Library/LaunchAgents/{OMINIX_PLIST}.plist",
        std::env::var("HOME").unwrap_or_default()
    );
    // Unload
    let _ = tokio::process::Command::new("launchctl")
        .args(["unload", &plist_path])
        .output()
        .await;

    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    // Load
    let output = tokio::process::Command::new("launchctl")
        .args(["load", &plist_path])
        .output()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    if output.status.success() {
        Ok(Json(ActionResponse {
            ok: true,
            message: Some("ominix-api service restarted".into()),
        }))
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Ok(Json(ActionResponse {
            ok: false,
            message: Some(format!("Restart failed: {stderr}")),
        }))
    }
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

    let home = std::env::var("HOME").unwrap_or_default();
    // Try both common log file names
    let log_path = {
        let p1 = format!("{home}/.ominix/api.log");
        let p2 = format!("{home}/.ominix/ominix-api.log");
        if std::path::Path::new(&p1).exists() {
            p1
        } else {
            p2
        }
    };

    let content = match tokio::fs::read_to_string(&log_path).await {
        Ok(c) => c,
        Err(e) => {
            return Ok(Json(serde_json::json!({
                "log_path": log_path,
                "error": format!("Cannot read log file: {e}"),
                "lines": [],
            })));
        }
    };

    let log_lines: Vec<&str> = content.lines().rev().take(lines).collect();
    let log_lines: Vec<&str> = log_lines.into_iter().rev().collect();

    Ok(Json(serde_json::json!({
        "log_path": log_path,
        "total_lines": content.lines().count(),
        "lines": log_lines,
    })))
}

// ── Model Management (proxy to ominix-api) ─────────────────────────

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
    let allowlist = octos_llm::ominix::PlatformModels::load_or_create(store.octos_home_dir());

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
    let allowlist = octos_llm::ominix::PlatformModels::load_or_create(store.octos_home_dir());
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
    let allowlist = octos_llm::ominix::PlatformModels::load_or_create(store.octos_home_dir());
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
    let mut allowlist = octos_llm::ominix::PlatformModels::load_or_create(&octos_home);

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
    allowlist.save(&octos_home).map_err(|e| {
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
    let mut allowlist = octos_llm::ominix::PlatformModels::load_or_create(&octos_home);

    let before = allowlist.platform_models.len();
    allowlist.platform_models.retain(|m| m.id != model_id);

    if allowlist.platform_models.len() == before {
        return Ok(Json(serde_json::json!({
            "ok": true,
            "message": format!("Model '{model_id}' was not in platform allowlist"),
        })));
    }

    allowlist.save(&octos_home).map_err(|e| {
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
    let update_available = latest
        .get("version")
        .and_then(|v| v.as_str())
        .is_some_and(|v| v != current_semver);

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
    let session = sm.load(&key).ok_or((
        StatusCode::NOT_FOUND,
        format!("session '{}' not found", query.key),
    ))?;

    let max_lines = query.lines.min(200);
    let messages = session.get_history(max_lines);
    let msg_json: Vec<serde_json::Value> =
        messages
            .iter()
            .map(|m| {
                let mut obj = serde_json::json!({
                    "role": m.role.as_str(),
                    "content": truncate_str(&m.content, 500),
                });
                if let Some(ref tc) = m.tool_calls {
                    if !tc.is_empty() {
                        obj["tool_calls"] =
                            serde_json::json!(tc.iter().map(|t| {
                        serde_json::json!({
                            "name": t.name,
                            "arguments": truncate_str(&t.arguments.to_string(), 200),
                        })
                    }).collect::<Vec<_>>());
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
fn truncate_str(s: &str, max_len: usize) -> String {
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
                "schedule": serde_json::to_value(&j.schedule).unwrap_or_default(),
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
        })
        .collect();

    // Check LLM provider
    let provider = profile.config.provider.as_deref().unwrap_or("unknown");
    let model = profile.config.model.as_deref().unwrap_or("unknown");

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
    let mut child = cmd
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
    // Validate tenant name - only allow safe characters for shell interpolation
    use std::sync::LazyLock;
    static TENANT_NAME_RE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"^[a-zA-Z0-9][a-zA-Z0-9_-]{0,62}$").unwrap());
    if !TENANT_NAME_RE.is_match(&req.name) {
        return Err((StatusCode::BAD_REQUEST, "Tenant name must be 1-63 alphanumeric characters, hyphens, or underscores, starting with alphanumeric".to_string()));
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
    let server = state.frps_server.as_deref().unwrap_or("163.192.33.32");
    let port = state.frps_port.unwrap_or(7000);

    // NOTE: frpc config is NOT embedded — the frps master token must be
    // provided as FRPS_TOKEN env var or argument when running the setup script.

    // Generate a self-contained setup script
    let script = format!(
        r#"#!/usr/bin/env bash
# Auto-generated setup script for tenant: {name}
# Subdomain: {subdomain}.{domain}
# SSH tunnel port: {ssh_port}
#
# Usage: curl -fsSL https://{domain}/api/admin/tenants/{id}/setup-script | bash
set -euo pipefail

SUBDOMAIN="{subdomain}"
FRPS_SERVER="{server}"
FRPS_PORT={port}
LOCAL_PORT={local_port}
SSH_PORT={ssh_port}
DOMAIN="{domain}"

# frps auth token — must be provided as env var or argument
FRPS_TOKEN="${{FRPS_TOKEN:-${{1:-}}}}"
if [ -z "$FRPS_TOKEN" ]; then
    echo "ERROR: frps auth token required."
    echo "Usage: FRPS_TOKEN=<token> bash setup.sh"
    echo "   or: bash setup.sh <token>"
    exit 1
fi

echo "==> Setting up octos tunnel for ${{SUBDOMAIN}}.${{DOMAIN}}"

# ── Install frpc ──────────────────────────────────────────────────────
FRPC_VERSION="0.61.1"
ARCH=$(uname -m)
case "$ARCH" in
    x86_64)  FRP_ARCH="amd64" ;;
    aarch64|arm64) FRP_ARCH="arm64" ;;
    *) echo "Unsupported arch: $ARCH"; exit 1 ;;
esac

OS=$(uname -s | tr '[:upper:]' '[:lower:]')

if [ ! -f /usr/local/bin/frpc ]; then
    echo "    Installing frpc v${{FRPC_VERSION}}..."
    TMPDIR=$(mktemp -d)
    trap 'rm -rf "$TMPDIR"' EXIT
    TARBALL="frp_${{FRPC_VERSION}}_${{OS}}_${{FRP_ARCH}}.tar.gz"
    curl -fsSL -o "$TMPDIR/$TARBALL" \
        "https://github.com/fatedier/frp/releases/download/v${{FRPC_VERSION}}/$TARBALL"
    tar -xzf "$TMPDIR/$TARBALL" -C "$TMPDIR"
    sudo install -m 0755 "$TMPDIR/frp_${{FRPC_VERSION}}_${{OS}}_${{FRP_ARCH}}/frpc" /usr/local/bin/frpc
    echo "    frpc installed"
else
    echo "    frpc already installed"
fi

# ── Write frpc config ────────────────────────────────────────────────
sudo mkdir -p /etc/frp
sudo tee /etc/frp/frpc.toml > /dev/null << FRPC_EOF
serverAddr = "$FRPS_SERVER"
serverPort = $FRPS_PORT
auth.method = "token"
auth.token = "$FRPS_TOKEN"
log.to = "/var/log/frpc.log"
log.level = "info"
log.maxDays = 7

[[proxies]]
name = "${{SUBDOMAIN}}-web"
type = "http"
localPort = $LOCAL_PORT
customDomains = ["${{SUBDOMAIN}}.$DOMAIN"]

[[proxies]]
name = "${{SUBDOMAIN}}-ssh"
type = "tcp"
localIP = "127.0.0.1"
localPort = 22
remotePort = $SSH_PORT
FRPC_EOF
echo "    frpc config written to /etc/frp/frpc.toml"

# ── Create launchd service (macOS) or systemd service (Linux) ────────
if [ "$OS" = "darwin" ]; then
    PLIST_PATH="$HOME/Library/LaunchAgents/io.octos.frpc.plist"
    mkdir -p "$HOME/Library/LaunchAgents"
    cat > "$PLIST_PATH" << PLIST_EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>io.octos.frpc</string>
    <key>ProgramArguments</key>
    <array>
        <string>/usr/local/bin/frpc</string>
        <string>-c</string>
        <string>/etc/frp/frpc.toml</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>/tmp/frpc.log</string>
    <key>StandardErrorPath</key>
    <string>/tmp/frpc.log</string>
</dict>
</plist>
PLIST_EOF
    launchctl unload "$PLIST_PATH" 2>/dev/null || true
    launchctl load "$PLIST_PATH"
    echo "    launchd service loaded (io.octos.frpc)"
else
    sudo tee /etc/systemd/system/frpc.service > /dev/null << SYSTEMD_EOF
[Unit]
Description=frpc tunnel client for octos
After=network.target

[Service]
Type=simple
ExecStart=/usr/local/bin/frpc -c /etc/frp/frpc.toml
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
SYSTEMD_EOF
    sudo systemctl daemon-reload
    sudo systemctl enable frpc
    sudo systemctl restart frpc
    echo "    systemd service started (frpc)"
fi

# ── Verify tunnel ────────────────────────────────────────────────────
echo ""
echo "==> Tunnel setup complete!"
echo "    Dashboard: https://${{SUBDOMAIN}}.${{DOMAIN}}"
echo "    SSH:       ssh -p ${{SSH_PORT}} $(whoami)@${{DOMAIN}}"
echo ""
echo "    Waiting 5s for tunnel to establish..."
sleep 5
if curl -sf --max-time 5 "http://localhost:${{LOCAL_PORT}}/api/status" > /dev/null 2>&1; then
    echo "    Local octos is running on port ${{LOCAL_PORT}}"
else
    echo "    NOTE: octos serve is not running on port ${{LOCAL_PORT}} yet"
    echo "    Start it with: octos serve --port ${{LOCAL_PORT}}"
fi
"#,
        name = tenant.name,
        subdomain = tenant.subdomain,
        domain = domain,
        server = server,
        port = port,
        local_port = tenant.local_port,
        ssh_port = tenant.ssh_port,
        id = tenant.id,
    );

    Ok(script)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_request_deserialize_minimal() {
        let json = r#"{"command": "echo hello"}"#;
        let req: ShellRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.command, "echo hello");
        assert!(req.cwd.is_none());
        assert!(req.timeout_secs.is_none());
    }

    #[test]
    fn shell_request_deserialize_full() {
        let json = r#"{"command": "ls", "cwd": "/tmp", "timeout_secs": 60}"#;
        let req: ShellRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.command, "ls");
        assert_eq!(req.cwd.as_deref(), Some("/tmp"));
        assert_eq!(req.timeout_secs, Some(60));
    }

    #[test]
    fn shell_response_serialize() {
        let resp = ShellResponse {
            stdout: "hello\n".into(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["stdout"], "hello\n");
        assert_eq!(json["exit_code"], 0);
        assert_eq!(json["timed_out"], false);
    }

    #[test]
    fn shell_constants() {
        assert_eq!(MAX_SHELL_COMMAND_LEN, 1_048_576);
        assert_eq!(DEFAULT_SHELL_TIMEOUT, 30);
        assert_eq!(MAX_SHELL_TIMEOUT, 600);
    }

    #[tokio::test]
    async fn shell_echo_command() {
        let req = ShellRequest {
            command: "echo hello".into(),
            cwd: Some("/tmp".into()),
            timeout_secs: Some(5),
        };
        let result = admin_shell(Json(req)).await.unwrap();
        assert_eq!(result.stdout.trim(), "hello");
        assert_eq!(result.exit_code, 0);
        assert!(!result.timed_out);
    }

    #[tokio::test]
    async fn shell_empty_command_rejected() {
        let req = ShellRequest {
            command: String::new(),
            cwd: None,
            timeout_secs: None,
        };
        let err = admin_shell(Json(req)).await.unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn shell_bad_cwd_rejected() {
        let req = ShellRequest {
            command: "echo hi".into(),
            cwd: Some("/nonexistent/path/xyz".into()),
            timeout_secs: None,
        };
        let err = admin_shell(Json(req)).await.unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn shell_captures_stderr() {
        let req = ShellRequest {
            command: "echo err >&2".into(),
            cwd: Some("/tmp".into()),
            timeout_secs: Some(5),
        };
        let result = admin_shell(Json(req)).await.unwrap();
        assert_eq!(result.stderr.trim(), "err");
        assert_eq!(result.exit_code, 0);
    }

    #[tokio::test]
    async fn shell_nonzero_exit_code() {
        let req = ShellRequest {
            command: "exit 42".into(),
            cwd: Some("/tmp".into()),
            timeout_secs: Some(5),
        };
        let result = admin_shell(Json(req)).await.unwrap();
        assert_eq!(result.exit_code, 42);
    }

    #[tokio::test]
    async fn shell_timeout() {
        let req = ShellRequest {
            command: "sleep 10".into(),
            cwd: Some("/tmp".into()),
            timeout_secs: Some(1),
        };
        let result = admin_shell(Json(req)).await.unwrap();
        assert!(result.timed_out);
        assert_eq!(result.exit_code, -1);
    }
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
    State(_state): State<Arc<AppState>>,
    Path(_id): Path<String>,
) -> Result<Json<WeChatQrStartResponse>, (StatusCode, String)> {
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

/// POST /api/admin/profiles/{id}/wechat/qr-poll
pub async fn wechat_qr_poll(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<WeChatQrPollRequest>,
) -> Result<Json<WeChatQrPollResponse>, (StatusCode, String)> {
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

        // Save token to profile
        if !bot_token.is_empty() {
            if let Some(_pm) = state.process_manager.as_ref() {
                // Write token with restrictive permissions from the start (no TOCTOU race)
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
