//! Webhook reverse proxy for Feishu and Twilio.
//!
//! Routes incoming webhook requests from a single public URL to the correct
//! profile's gateway process based on the profile ID in the URL path.
//!
//! ```text
//! POST /webhook/feishu/{profile_id}  →  127.0.0.1:{port}/webhook/event
//! POST /webhook/twilio/{profile_id}  →  127.0.0.1:{port}/twilio/webhook
//! ```

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};

use super::AppState;

/// Proxy Feishu/Lark webhook events to the gateway's local webhook server.
pub async fn feishu_webhook_proxy(
    State(state): State<Arc<AppState>>,
    Path(profile_id): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    // Read body first so we can inspect it for url_verification
    let body_bytes = match axum::body::to_bytes(body, 10 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => return json_error(StatusCode::BAD_REQUEST, "request body too large"),
    };

    // Handle Feishu url_verification challenge at the proxy level.
    // This allows webhook URL verification in the Lark console even if
    // the gateway hasn't started yet or is in websocket mode.
    if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&body_bytes) {
        if json.get("type").and_then(|v| v.as_str()) == Some("url_verification") {
            let challenge = json.get("challenge").and_then(|v| v.as_str()).unwrap_or("");
            tracing::info!(
                profile = %profile_id,
                "webhook proxy: handling url_verification challenge directly"
            );
            return axum::Json(serde_json::json!({"challenge": challenge})).into_response();
        }
    }

    proxy_to_gateway_with_bytes(state, profile_id, "/webhook/event", headers, body_bytes).await
}

/// Proxy Twilio webhook events to the gateway's local webhook server.
pub async fn twilio_webhook_proxy(
    State(state): State<Arc<AppState>>,
    Path(profile_id): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    tracing::info!(profile = %profile_id, "webhook proxy: twilio event received");
    let body_bytes = match axum::body::to_bytes(body, 10 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => return json_error(StatusCode::BAD_REQUEST, "request body too large"),
    };
    proxy_to_gateway_with_bytes(state, profile_id, "/twilio/webhook", headers, body_bytes).await
}

/// Forward a request to the gateway's local webhook server.
async fn proxy_to_gateway_with_bytes(
    state: Arc<AppState>,
    profile_id: String,
    upstream_path: &str,
    headers: HeaderMap,
    body_bytes: Bytes,
) -> Response {
    let pm = match state.process_manager.as_ref() {
        Some(pm) => pm,
        None => {
            return json_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "process manager not available",
            );
        }
    };

    let port = match pm.webhook_port(&profile_id).await {
        Some(port) => port,
        None => {
            return json_error(
                StatusCode::BAD_GATEWAY,
                &format!(
                    "no webhook port for profile '{profile_id}' (gateway not running or not in webhook mode)"
                ),
            );
        }
    };

    let url = format!("http://127.0.0.1:{port}{upstream_path}");

    // Build upstream request preserving headers
    let mut req = state.http_client.post(&url).body(body_bytes.to_vec());

    // Forward relevant headers
    for (name, value) in &headers {
        // Skip hop-by-hop headers
        let n = name.as_str();
        if matches!(
            n,
            "host" | "connection" | "transfer-encoding" | "keep-alive"
        ) {
            continue;
        }
        if let Ok(v) = value.to_str() {
            req = req.header(name.clone(), v);
        }
    }

    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(
                profile = %profile_id,
                url = %url,
                error = %e,
                "webhook proxy: upstream request failed"
            );
            return json_error(
                StatusCode::BAD_GATEWAY,
                &format!("upstream request failed: {e}"),
            );
        }
    };

    // Convert upstream response back to axum response
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let resp_headers = resp.headers().clone();
    let resp_body = match resp.bytes().await {
        Ok(b) => b,
        Err(_) => return json_error(StatusCode::BAD_GATEWAY, "failed to read upstream response"),
    };

    let mut response = (status, resp_body.to_vec()).into_response();
    // Copy content-type from upstream
    if let Some(ct) = resp_headers.get("content-type") {
        response.headers_mut().insert("content-type", ct.clone());
    }

    response
}

/// Streaming SSE proxy for API channel requests.
///
/// Forwards `POST /api/chat` to the gateway's API channel HTTP server and
/// streams the SSE response back to the web client.
pub async fn api_chat_proxy(
    state: &AppState,
    port: u16,
    message: &str,
    session_id: Option<&str>,
) -> Response {
    let url = format!("http://127.0.0.1:{port}/chat");
    let body = serde_json::json!({
        "message": message,
        "session_id": session_id,
    });

    let resp = match state
        .http_client
        .post(&url)
        .header("content-type", "application/json")
        .body(body.to_string())
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(port, error = %e, "API chat proxy: upstream request failed");
            return json_error(
                StatusCode::BAD_GATEWAY,
                &format!("gateway proxy failed: {e}"),
            );
        }
    };

    let status = resp.status();
    if !status.is_success() {
        let err_body = resp.text().await.unwrap_or_default();
        return json_error(
            StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
            &err_body,
        );
    }

    // Stream the SSE response body directly back to the client
    let stream = resp.bytes_stream();
    match Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(Body::from_stream(stream))
    {
        Ok(r) => r,
        Err(e) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("failed to build SSE response: {e}"),
        ),
    }
}

/// Return a JSON error response so Feishu/Lark doesn't complain about non-JSON.
fn json_error(status: StatusCode, message: &str) -> Response {
    let body = serde_json::json!({"error": message});
    (status, axum::Json(body)).into_response()
}
