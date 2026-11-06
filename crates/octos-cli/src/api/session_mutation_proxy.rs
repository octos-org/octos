//! Shared-store mutations keep both serve's locks until the gateway replies.
use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use octos_bus::SessionManager;
use octos_core::SessionKey;
use tokio::sync::{Mutex, OwnedMutexGuard};

use super::AppState;
use crate::process_manager::GatewayApiEndpoint;

pub(super) const WAIT_LIMIT: Duration = Duration::from_secs(3);

pub(super) async fn lock_sessions(
    sessions: Arc<Mutex<SessionManager>>,
) -> Result<OwnedMutexGuard<SessionManager>, tokio::time::error::Elapsed> {
    tokio::time::timeout(WAIT_LIMIT, sessions.lock_owned()).await
}

pub(super) fn busy_response() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "session storage is busy; please retry",
    )
        .into_response()
}

pub(super) async fn delegate(
    state: Arc<AppState>,
    mut sessions: OwnedMutexGuard<SessionManager>,
    key: SessionKey,
    endpoint: &GatewayApiEndpoint,
    title: Option<String>,
) -> Response {
    // Match local writer lock ordering: manager first, then per-key persistence.
    // The latter also protects send_file/background writers using SessionHandle.
    let persist = match tokio::time::timeout(
        WAIT_LIMIT,
        octos_bus::session::lock_session_persistence(&key),
    )
    .await
    {
        Ok(guard) => guard,
        Err(_) => return busy_response(), // No request has been sent.
    };
    let port = endpoint.port;
    let token = endpoint.token.clone();
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let _persist = persist;
        let path = format!(
            "/sessions/{}",
            super::handlers::encode_api_session_path_id(&key.0)
        );
        let deleting = title.is_none();
        let response = match title {
            Some(title) => {
                super::webhook_proxy::api_patch_proxy(
                    &state,
                    port,
                    &format!("{path}/title"),
                    serde_json::json!({"title": title}).to_string(),
                    token.as_deref(),
                )
                .await
            }
            None => {
                super::webhook_proxy::api_delete_proxy(&state, port, &path, token.as_deref()).await
            }
        };
        sessions.invalidate_cache(&key);
        let response = if deleting && response.status().is_success() {
            match super::ui_protocol_reasoning_effort::clear_reasoning_effort(
                &sessions.data_dir(),
                &key,
            ) {
                Ok(()) => response,
                Err(error) => {
                    tracing::error!(session_key = %key, %error, "delegated session settings deletion failed");
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "could not delete session settings",
                    )
                        .into_response()
                }
            }
        } else {
            response
        };
        // A client deadline is not cancellation of a mutation already sent to
        // another process. Retain BOTH guards past that deadline, through its
        // actual response and cache/sidecar cleanup. Retried calls time out
        // acquiring the manager, without queuing another remote mutation.
        if let Err(response) = result_tx.send(response) {
            tracing::warn!(session_key = %key, status = %response.status(),
                "gateway session mutation settled after caller stopped waiting");
        }
    });
    match tokio::time::timeout(WAIT_LIMIT, result_rx).await {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => {
            tracing::error!(%error, "gateway session mutation worker stopped without an outcome");
            (
                StatusCode::BAD_GATEWAY,
                "session mutation outcome is unknown; refresh before retrying",
            )
                .into_response()
        }
        Err(_) => (
            StatusCode::GATEWAY_TIMEOUT,
            "gateway session mutation outcome is not yet known; refresh before retrying",
        )
            .into_response(),
    }
}
