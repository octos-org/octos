//! `smart_home/*` WS methods' backend logic: resolve the caller's profile,
//! then talk to their configured smart-home bridge via
//! [`crate::api::smart_home_bridge`]. The typed services are reached only
//! over the UI Protocol
//! (`smart_home/*` methods, gated on `smart_home.v1`) via the thin
//! `handle_smart_home_*` wrappers in `ui_protocol.rs` — there is no
//! registered REST route for these (same WS-only convention `cron_panel`'s
//! module doc describes for its own retired `/api/my/cron*` routes).
//! `device_id`/`params`/`quality` are taken as plain arguments rather than
//! `AxumPath`/`Json` extractors since they always come from an
//! already-deserialized WS Params struct, never a raw HTTP request.

use axum::Json;
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

    fn panel_error(&self) -> PanelError {
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

/// Emit the bridge diagnostic server-side without changing the WS error body.
/// This is a real production reader for `detail` after the retired REST
/// handlers stopped consuming it.
pub(crate) fn log_smart_home_command_error(error: &SmartHomeCommandError, method: &'static str) {
    let (status, Json(body)) = error.panel_error();
    tracing::warn!(
        method,
        %status,
        reason = error.reason(),
        detail = ?body.get("detail"),
        "smart-home command failed"
    );
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
