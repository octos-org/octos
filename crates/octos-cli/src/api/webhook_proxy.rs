//! Webhook reverse proxy for Feishu and Twilio.
//!
//! Routes incoming webhook requests from a single public URL to the correct
//! profile's gateway process based on the profile ID in the URL path.
//!
//! ```text
//! POST /webhook/feishu/{profile_id}  →  127.0.0.1:{port}/webhook/event
//! POST /webhook/line/{profile_id}    →  127.0.0.1:{port}/line/webhook
//! POST /webhook/twilio/{profile_id}  →  127.0.0.1:{port}/twilio/webhook
//! ```

use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};

use super::AppState;

const WEBHOOK_UPSTREAM_TIMEOUT: Duration = Duration::from_secs(10);

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

/// Proxy LINE webhook events to the gateway's local webhook server.
pub async fn line_webhook_proxy(
    State(state): State<Arc<AppState>>,
    Path(profile_id): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    tracing::info!(profile = %profile_id, "webhook proxy: LINE event received");
    let body_bytes = match axum::body::to_bytes(body, 10 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => return json_error(StatusCode::BAD_REQUEST, "request body too large"),
    };
    proxy_to_gateway_with_bytes(state, profile_id, "/line/webhook", headers, body_bytes).await
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

    proxy_webhook_to_url(
        &state.http_client,
        &profile_id,
        &url,
        &headers,
        body_bytes,
        WEBHOOK_UPSTREAM_TIMEOUT,
    )
    .await
}

async fn proxy_webhook_to_url(
    http_client: &reqwest::Client,
    profile_id: &str,
    url: &str,
    headers: &HeaderMap,
    body_bytes: Bytes,
    timeout: Duration,
) -> Response {
    // Build upstream request preserving headers
    let mut req = http_client
        .post(url)
        .timeout(timeout)
        .body(body_bytes.to_vec());

    // Forward relevant headers
    for (name, value) in headers {
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
        Err(error) => return webhook_upstream_error_response(profile_id, url, timeout, error),
    };

    // Convert upstream response back to axum response
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let resp_headers = resp.headers().clone();
    let resp_body = match resp.bytes().await {
        Ok(b) => b,
        Err(error) => {
            return webhook_upstream_error_response(profile_id, url, timeout, error);
        }
    };

    let mut response = (status, resp_body.to_vec()).into_response();
    // Copy content-type from upstream
    if let Some(ct) = resp_headers.get("content-type") {
        response.headers_mut().insert("content-type", ct.clone());
    }

    response
}

fn webhook_upstream_error_response(
    profile_id: &str,
    url: &str,
    timeout: Duration,
    error: reqwest::Error,
) -> Response {
    let (status, message) = if error.is_timeout() {
        (
            StatusCode::GATEWAY_TIMEOUT,
            format!("upstream request timed out after {:?}", timeout),
        )
    } else {
        (
            StatusCode::BAD_GATEWAY,
            format!("upstream request failed: {error}"),
        )
    };

    tracing::error!(
        profile = %profile_id,
        url = %url,
        timeout_ms = timeout.as_millis(),
        is_timeout = error.is_timeout(),
        error = %error,
        "webhook proxy: upstream request failed"
    );

    json_error(status, &message)
}

/// Proxy a GET request to the gateway's API channel.
pub async fn api_get_proxy(state: &AppState, port: u16, path: &str) -> Response {
    let url = format!("http://127.0.0.1:{port}{path}");
    let resp = match state.http_client.get(&url).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(port, error = %e, "API GET proxy failed");
            return json_error(
                StatusCode::BAD_GATEWAY,
                &format!("gateway proxy failed: {e}"),
            );
        }
    };

    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let body = resp.bytes().await.unwrap_or_default();
    let mut response = (status, body.to_vec()).into_response();
    response
        .headers_mut()
        .insert("content-type", "application/json".parse().unwrap());
    response
}

/// Proxy a POST request (with optional JSON body) to the gateway's API
/// channel. The response status and body are forwarded verbatim — used
/// by the M7.9 cancel / restart-from-node endpoints so the API server
/// can hand control back to the gateway process that owns the supervisor.
pub async fn api_post_proxy_json(
    state: &AppState,
    port: u16,
    path: &str,
    body: serde_json::Value,
) -> Response {
    let url = format!("http://127.0.0.1:{port}{path}");
    let resp = match state.http_client.post(&url).json(&body).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(port, error = %e, "API POST proxy failed");
            return json_error(
                StatusCode::BAD_GATEWAY,
                &format!("gateway proxy failed: {e}"),
            );
        }
    };

    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let body = resp.bytes().await.unwrap_or_default();
    let mut response = (status, body.to_vec()).into_response();
    response
        .headers_mut()
        .insert("content-type", "application/json".parse().unwrap());
    response
}

/// Proxy a PATCH request with a JSON body to the gateway's API channel.
pub async fn api_patch_proxy(state: &AppState, port: u16, path: &str, body: String) -> Response {
    let url = format!("http://127.0.0.1:{port}{path}");
    let resp = match state
        .http_client
        .patch(&url)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(port, error = %e, "API PATCH proxy failed");
            return json_error(
                StatusCode::BAD_GATEWAY,
                &format!("gateway proxy failed: {e}"),
            );
        }
    };
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    status.into_response()
}

/// Proxy a DELETE request to the gateway's API channel.
pub async fn api_delete_proxy(state: &AppState, port: u16, path: &str) -> Response {
    let url = format!("http://127.0.0.1:{port}{path}");
    let resp = match state.http_client.delete(&url).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(port, error = %e, "API DELETE proxy failed");
            return json_error(
                StatusCode::BAD_GATEWAY,
                &format!("gateway proxy failed: {e}"),
            );
        }
    };

    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    status.into_response()
}

/// Return a JSON error response so Feishu/Lark doesn't complain about non-JSON.
fn json_error(status: StatusCode, message: &str) -> Response {
    let body = serde_json::json!({"error": message});
    (status, axum::Json(body)).into_response()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::body::Bytes;
    use axum::http::{HeaderMap, StatusCode};

    use super::proxy_webhook_to_url;

    // M9-α-5/α-6 (ADR PR #830 / audit issue #845): the SSE chat proxy
    // (`api_chat_proxy`, `parse_sync_chat_response_from_sse`,
    // `api_sse_get_proxy`) was deleted along with its tests
    // (`parse_sync_chat_response_from_sse_*`,
    // `should_passthrough_json_queued_ack`,
    // `should_keep_sse_when_upstream_is_sse`). Every chat-streaming
    // path now goes through `/api/ui-protocol/ws`; the WS bridge
    // owns its own coverage in `ui_protocol_alpha*_bridge.rs`.

    #[tokio::test]
    async fn proxy_webhook_to_url_times_out_hung_upstream() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (_socket, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(1)).await;
        });

        let response = proxy_webhook_to_url(
            &reqwest::Client::new(),
            "test-profile",
            &format!("http://{addr}/webhook/event"),
            &HeaderMap::new(),
            Bytes::from_static(br#"{"ok":true}"#),
            Duration::from_millis(50),
        )
        .await;

        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);

        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"], "upstream request timed out after 50ms");

        server.abort();
    }
}
