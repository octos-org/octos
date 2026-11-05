//! `smart_home/*` WS methods' backend logic: resolve the caller's profile,
//! then talk to their configured smart-home bridge via
//! [`crate::api::smart_home_bridge`]. Function shape (`State` + `HeaderMap` +
//! `Extension<AuthIdentity>`) mirrors `cron_panel`/`memory_panel` for the
//! profile-resolution prefix, reached only over the UI Protocol
//! (`smart_home/*` methods, gated on `smart_home.v1`) via the thin
//! `handle_smart_home_*` wrappers in `ui_protocol.rs` — there is no
//! registered REST route for these (same WS-only convention `cron_panel`'s
//! module doc describes for its own retired `/api/my/cron*` routes).
//! `device_id`/`params`/`quality` are taken as plain arguments rather than
//! `AxumPath`/`Json` extractors since they always come from an
//! already-deserialized WS Params struct, never a raw HTTP request.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use serde_json::{Value, json};

use super::AppState;
use super::router::AuthIdentity;
use super::smart_home_bridge::{self, BridgeConfig, BridgeError};
use crate::api::auth_handlers::resolve_my_profile_id;

type PanelError = (StatusCode, Json<Value>);

fn err(status: StatusCode, reason: &str) -> PanelError {
    (status, Json(json!({ "ok": false, "reason": reason })))
}

fn bridge_error_response(error: BridgeError) -> PanelError {
    match error {
        BridgeError::NotConfigured => err(StatusCode::NOT_FOUND, "not_configured"),
        BridgeError::Request(detail) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "ok": false, "reason": "bridge_unreachable", "detail": detail })),
        ),
        BridgeError::Bridge(detail) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "ok": false, "reason": "bridge_error", "detail": detail })),
        ),
    }
}

/// Typed application boundary for read-only smart-home status and device
/// discovery. Control methods keep the same bridge resolver through the thin
/// compatibility adapter below; no connection or token is copied across profiles.
pub(crate) struct SmartHomeQueryService<'a> {
    state: &'a AppState,
}

#[derive(Debug)]
pub(crate) enum SmartHomeQueryError {
    ProfilesUnavailable,
    Auth(StatusCode),
    ProfileReadFailed,
    ProfileNotFound,
    NotConfigured,
    BridgeUnreachable(String),
    Bridge(String),
}

impl SmartHomeQueryError {
    pub(crate) fn status(&self) -> StatusCode {
        match self {
            Self::ProfilesUnavailable => StatusCode::SERVICE_UNAVAILABLE,
            Self::Auth(status) => *status,
            Self::ProfileReadFailed => StatusCode::INTERNAL_SERVER_ERROR,
            Self::ProfileNotFound | Self::NotConfigured => StatusCode::NOT_FOUND,
            Self::BridgeUnreachable(_) | Self::Bridge(_) => StatusCode::BAD_GATEWAY,
        }
    }

    pub(crate) fn reason(&self) -> &'static str {
        match self {
            Self::ProfilesUnavailable => "profiles_unavailable",
            Self::Auth(_) => "auth",
            Self::ProfileReadFailed => "profile_read_failed",
            Self::ProfileNotFound => "profile_not_found",
            Self::NotConfigured => "not_configured",
            Self::BridgeUnreachable(_) => "bridge_unreachable",
            Self::Bridge(_) => "bridge_error",
        }
    }

    fn body(&self) -> Value {
        let mut body = json!({ "ok": false, "reason": self.reason() });
        let detail = match self {
            Self::BridgeUnreachable(detail) | Self::Bridge(detail) => Some(detail),
            _ => None,
        };
        if let Some(detail) = detail {
            body["detail"] = json!(detail);
        }
        body
    }

    fn into_panel_error(self) -> PanelError {
        (self.status(), Json(self.body()))
    }

    fn from_bridge(error: BridgeError) -> Self {
        match error {
            BridgeError::NotConfigured => Self::NotConfigured,
            BridgeError::Request(detail) => Self::BridgeUnreachable(detail),
            BridgeError::Bridge(detail) => Self::Bridge(detail),
        }
    }
}

impl<'a> SmartHomeQueryService<'a> {
    pub(crate) fn new(state: &'a AppState) -> Self {
        Self { state }
    }

    async fn resolve_bridge(
        &self,
        identity: &AuthIdentity,
        headers: &HeaderMap,
    ) -> Result<Option<BridgeConfig>, SmartHomeQueryError> {
        let ps = self
            .state
            .profile_store
            .as_ref()
            .ok_or(SmartHomeQueryError::ProfilesUnavailable)?;
        let profile_id = resolve_my_profile_id(identity, ps, self.state, headers)
            .map_err(SmartHomeQueryError::Auth)?;
        let profile = ps
            .get(&profile_id)
            .map_err(|_| SmartHomeQueryError::ProfileReadFailed)?
            .ok_or(SmartHomeQueryError::ProfileNotFound)?;
        let smart_home = profile.config.smart_home.unwrap_or_default();
        Ok(smart_home_bridge::resolve_bridge_config(
            &smart_home,
            &profile.config.env_vars,
        ))
    }

    pub(crate) async fn status(
        &self,
        identity: &AuthIdentity,
        headers: &HeaderMap,
    ) -> Result<Value, SmartHomeQueryError> {
        let bridge = self.resolve_bridge(identity, headers).await?;
        Ok(json!({ "configured": bridge.is_some() }))
    }

    pub(crate) async fn devices(
        &self,
        identity: &AuthIdentity,
        headers: &HeaderMap,
    ) -> Result<Value, SmartHomeQueryError> {
        let bridge = self
            .resolve_bridge(identity, headers)
            .await?
            .ok_or(SmartHomeQueryError::NotConfigured)?;
        smart_home_bridge::fetch_devices(&self.state.http_client, &bridge)
            .await
            .map(|devices| serde_json::to_value(devices).unwrap_or_else(|_| json!({})))
            .map_err(SmartHomeQueryError::from_bridge)
    }
}

/// Resolve the caller's `BridgeConfig`, if their profile has one configured.
async fn resolve_my_bridge(
    state: &AppState,
    identity: &AuthIdentity,
    headers: &HeaderMap,
) -> Result<Option<BridgeConfig>, PanelError> {
    SmartHomeQueryService::new(state)
        .resolve_bridge(identity, headers)
        .await
        .map_err(SmartHomeQueryError::into_panel_error)
}

async fn resolve_my_configured_bridge(
    state: &AppState,
    identity: &AuthIdentity,
    headers: &HeaderMap,
) -> Result<BridgeConfig, PanelError> {
    resolve_my_bridge(state, identity, headers)
        .await?
        .ok_or_else(|| bridge_error_response(BridgeError::NotConfigured))
}

/// Typed application boundary for smart-home control and camera execute paths.
/// Query helpers stay on their existing resolver; this service adapts that same
/// resolver and preserves every side effect, status, reason, and bridge detail.
pub(crate) struct SmartHomeCommandService<'a> {
    state: &'a AppState,
}

#[derive(Debug)]
pub(crate) enum SmartHomeCommandError {
    ProfilesUnavailable,
    Auth(StatusCode),
    ProfileReadFailed,
    ProfileNotFound,
    NotConfigured,
    BridgeUnreachable(String),
    Bridge(String),
    Other {
        status: StatusCode,
        reason: String,
        detail: Option<String>,
    },
}

impl SmartHomeCommandError {
    pub(crate) fn status(&self) -> StatusCode {
        match self {
            Self::ProfilesUnavailable => StatusCode::SERVICE_UNAVAILABLE,
            Self::Auth(status) => *status,
            Self::ProfileReadFailed => StatusCode::INTERNAL_SERVER_ERROR,
            Self::ProfileNotFound | Self::NotConfigured => StatusCode::NOT_FOUND,
            Self::BridgeUnreachable(_) | Self::Bridge(_) => StatusCode::BAD_GATEWAY,
            Self::Other { status, .. } => *status,
        }
    }

    pub(crate) fn reason(&self) -> &str {
        match self {
            Self::ProfilesUnavailable => "profiles_unavailable",
            Self::Auth(_) => "auth",
            Self::ProfileReadFailed => "profile_read_failed",
            Self::ProfileNotFound => "profile_not_found",
            Self::NotConfigured => "not_configured",
            Self::BridgeUnreachable(_) => "bridge_unreachable",
            Self::Bridge(_) => "bridge_error",
            Self::Other { reason, .. } => reason,
        }
    }

    fn body(&self) -> Value {
        let mut body = json!({ "ok": false, "reason": self.reason() });
        let detail = match self {
            Self::BridgeUnreachable(detail) | Self::Bridge(detail) => Some(detail),
            Self::Other { detail, .. } => detail.as_ref(),
            _ => None,
        };
        if let Some(detail) = detail {
            body["detail"] = json!(detail);
        }
        body
    }

    fn into_panel_error(self) -> PanelError {
        (self.status(), Json(self.body()))
    }

    fn from_panel((status, Json(body)): PanelError) -> Self {
        let reason = body
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let detail = body
            .get("detail")
            .and_then(Value::as_str)
            .map(str::to_owned);
        match reason.as_str() {
            "profiles_unavailable" => Self::ProfilesUnavailable,
            "auth" => Self::Auth(status),
            "profile_read_failed" => Self::ProfileReadFailed,
            "profile_not_found" => Self::ProfileNotFound,
            "not_configured" => Self::NotConfigured,
            "bridge_unreachable" => Self::BridgeUnreachable(detail.unwrap_or_default()),
            "bridge_error" => Self::Bridge(detail.unwrap_or_default()),
            _ => Self::Other {
                status,
                reason,
                detail,
            },
        }
    }

    fn from_bridge(error: BridgeError) -> Self {
        match error {
            BridgeError::NotConfigured => Self::NotConfigured,
            BridgeError::Request(detail) => Self::BridgeUnreachable(detail),
            BridgeError::Bridge(detail) => Self::Bridge(detail),
        }
    }
}

impl<'a> SmartHomeCommandService<'a> {
    pub(crate) fn new(state: &'a AppState) -> Self {
        Self { state }
    }

    async fn configured_bridge(
        &self,
        identity: &AuthIdentity,
        headers: &HeaderMap,
    ) -> Result<BridgeConfig, SmartHomeCommandError> {
        resolve_my_configured_bridge(self.state, identity, headers)
            .await
            .map_err(SmartHomeCommandError::from_panel)
    }

    pub(crate) async fn execute(
        &self,
        identity: &AuthIdentity,
        headers: &HeaderMap,
        device_id: &str,
        params: &serde_json::Map<String, Value>,
    ) -> Result<Value, SmartHomeCommandError> {
        let bridge = self.configured_bridge(identity, headers).await?;
        smart_home_bridge::send_device_command(&self.state.http_client, &bridge, device_id, params)
            .await
            .map(|()| json!({}))
            .map_err(SmartHomeCommandError::from_bridge)
    }

    pub(crate) async fn camera_start(
        &self,
        identity: &AuthIdentity,
        headers: &HeaderMap,
        device_id: &str,
        quality: Option<u32>,
    ) -> Result<Value, SmartHomeCommandError> {
        let bridge = self.configured_bridge(identity, headers).await?;
        smart_home_bridge::start_camera_stream(&self.state.http_client, &bridge, device_id, quality)
            .await
            .map(|info| serde_json::to_value(info).unwrap_or_else(|_| json!({})))
            .map_err(SmartHomeCommandError::from_bridge)
    }

    pub(crate) async fn camera_stop(
        &self,
        identity: &AuthIdentity,
        headers: &HeaderMap,
        device_id: &str,
    ) -> Result<Value, SmartHomeCommandError> {
        let bridge = self.configured_bridge(identity, headers).await?;
        smart_home_bridge::stop_camera_stream(&self.state.http_client, &bridge, device_id)
            .await
            .map(|()| json!({}))
            .map_err(SmartHomeCommandError::from_bridge)
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub async fn my_smart_home_status(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<AuthIdentity>,
) -> Result<Json<Value>, PanelError> {
    SmartHomeQueryService::new(&state)
        .status(&identity, &headers)
        .await
        .map(Json)
        .map_err(SmartHomeQueryError::into_panel_error)
}

#[cfg_attr(not(test), allow(dead_code))]
pub async fn my_smart_home_devices(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<AuthIdentity>,
) -> Result<Json<Value>, PanelError> {
    SmartHomeQueryService::new(&state)
        .devices(&identity, &headers)
        .await
        .map(Json)
        .map_err(SmartHomeQueryError::into_panel_error)
}

#[cfg_attr(not(test), allow(dead_code))]
pub async fn my_smart_home_device_command(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<AuthIdentity>,
    device_id: String,
    params: serde_json::Map<String, Value>,
) -> Result<Json<Value>, PanelError> {
    SmartHomeCommandService::new(&state)
        .execute(&identity, &headers, &device_id, &params)
        .await
        .map(Json)
        .map_err(SmartHomeCommandError::into_panel_error)
}

#[cfg_attr(not(test), allow(dead_code))]
pub async fn my_smart_home_camera_stream_start(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<AuthIdentity>,
    device_id: String,
    quality: Option<u32>,
) -> Result<Json<Value>, PanelError> {
    SmartHomeCommandService::new(&state)
        .camera_start(&identity, &headers, &device_id, quality)
        .await
        .map(Json)
        .map_err(SmartHomeCommandError::into_panel_error)
}

#[cfg_attr(not(test), allow(dead_code))]
pub async fn my_smart_home_camera_stream_stop(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<AuthIdentity>,
    device_id: String,
) -> Result<Json<Value>, PanelError> {
    SmartHomeCommandService::new(&state)
        .camera_stop(&identity, &headers, &device_id)
        .await
        .map(Json)
        .map_err(SmartHomeCommandError::into_panel_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::{ProfileConfig, ProfileStore, SmartHomeConfig, UserProfile};
    use crate::user_store::UserRole;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn user_identity(id: &str) -> axum::Extension<AuthIdentity> {
        axum::Extension(AuthIdentity::User {
            id: id.into(),
            role: UserRole::User,
        })
    }

    fn make_profile(id: &str, smart_home: Option<SmartHomeConfig>) -> UserProfile {
        UserProfile {
            id: id.into(),
            name: id.into(),
            enabled: true,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: ProfileConfig {
                smart_home,
                ..ProfileConfig::default()
            },
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    fn temp_state(profile: &UserProfile) -> (tempfile::TempDir, Arc<AppState>) {
        let dir = tempfile::tempdir().unwrap();
        let profile_store = Arc::new(ProfileStore::open_unified(dir.path()).unwrap());
        profile_store.save(profile).unwrap();
        let state = Arc::new(AppState {
            profile_store: Some(profile_store),
            ..AppState::empty_for_tests()
        });
        (dir, state)
    }

    #[tokio::test]
    async fn my_smart_home_status_reports_not_configured_without_bridge() {
        let profile = make_profile("tenant", None);
        let (_dir, state) = temp_state(&profile);
        let Json(body) =
            my_smart_home_status(State(state), HeaderMap::new(), user_identity("tenant"))
                .await
                .unwrap();
        assert_eq!(body["configured"], json!(false));
    }

    #[tokio::test]
    async fn my_smart_home_status_reports_configured_with_bridge() {
        let profile = make_profile(
            "tenant",
            Some(SmartHomeConfig {
                bridge_url: Some("http://localhost:8787".into()),
                token: None,
                token_env: None,
            }),
        );
        let (_dir, state) = temp_state(&profile);
        let Json(body) =
            my_smart_home_status(State(state), HeaderMap::new(), user_identity("tenant"))
                .await
                .unwrap();
        assert_eq!(body["configured"], json!(true));
    }

    #[tokio::test]
    async fn my_smart_home_devices_returns_not_configured_error_without_bridge() {
        let profile = make_profile("tenant", None);
        let (_dir, state) = temp_state(&profile);
        let (status, Json(body)) =
            my_smart_home_devices(State(state), HeaderMap::new(), user_identity("tenant"))
                .await
                .unwrap_err();
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["reason"], json!("not_configured"));
    }

    #[tokio::test]
    async fn my_smart_home_devices_forwards_bridge_device_list() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/devices"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true,
                "devices": [{"id": "tv1", "name": "Living Room TV", "kind": "tv", "on": true}]
            })))
            .mount(&server)
            .await;
        let profile = make_profile(
            "tenant",
            Some(SmartHomeConfig {
                bridge_url: Some(server.uri()),
                token: None,
                token_env: None,
            }),
        );
        let (_dir, state) = temp_state(&profile);
        let Json(body) =
            my_smart_home_devices(State(state), HeaderMap::new(), user_identity("tenant"))
                .await
                .unwrap();
        assert_eq!(body["devices"][0]["id"], json!("tv1"));
    }

    #[tokio::test]
    async fn my_smart_home_devices_maps_bridge_unreachable_to_bad_gateway() {
        let profile = make_profile(
            "tenant",
            Some(SmartHomeConfig {
                bridge_url: Some("http://127.0.0.1:1".into()),
                token: None,
                token_env: None,
            }),
        );
        let (_dir, state) = temp_state(&profile);
        let (status, Json(body)) =
            my_smart_home_devices(State(state), HeaderMap::new(), user_identity("tenant"))
                .await
                .unwrap_err();
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(body["reason"], json!("bridge_unreachable"));
    }

    #[tokio::test]
    async fn my_smart_home_device_command_posts_to_bridge() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/devices/tv1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
            .mount(&server)
            .await;
        let profile = make_profile(
            "tenant",
            Some(SmartHomeConfig {
                bridge_url: Some(server.uri()),
                token: None,
                token_env: None,
            }),
        );
        let (_dir, state) = temp_state(&profile);
        let mut params = serde_json::Map::new();
        params.insert("on".into(), json!(true));
        let Json(_) = my_smart_home_device_command(
            State(state),
            HeaderMap::new(),
            user_identity("tenant"),
            "tv1".into(),
            params,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn my_smart_home_camera_stream_start_and_stop_round_trip() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/cameras/cam1/stream"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true,
                "protocol": "hls",
                "playback_url": "http://bridge/hls/cam1.m3u8"
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/cameras/cam1/stop"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
            .mount(&server)
            .await;
        let profile = make_profile(
            "tenant",
            Some(SmartHomeConfig {
                bridge_url: Some(server.uri()),
                token: None,
                token_env: None,
            }),
        );
        let (_dir, state) = temp_state(&profile);
        let Json(body) = my_smart_home_camera_stream_start(
            State(state.clone()),
            HeaderMap::new(),
            user_identity("tenant"),
            "cam1".into(),
            Some(2),
        )
        .await
        .unwrap();
        assert_eq!(body["protocol"], json!("hls"));

        let Json(_) = my_smart_home_camera_stream_stop(
            State(state),
            HeaderMap::new(),
            user_identity("tenant"),
            "cam1".into(),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn my_smart_home_devices_rejects_unknown_profile_identity() {
        let profile = make_profile("tenant", None);
        let (_dir, state) = temp_state(&profile);
        let (status, _) = my_smart_home_devices(
            State(state),
            HeaderMap::new(),
            user_identity("someone-else"),
        )
        .await
        .unwrap_err();
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}
