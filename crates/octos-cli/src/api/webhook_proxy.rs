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

/// Streaming SSE proxy for API channel requests.
///
/// Forwards `POST /api/chat` to the gateway's API channel HTTP server and
/// streams the SSE response back to the web client.
pub async fn api_chat_proxy(
    state: &AppState,
    port: u16,
    profile_id: Option<&str>,
    message: &str,
    session_id: Option<&str>,
    topic: Option<&str>,
    media: &[String],
    attach_only: bool,
    stream: bool,
) -> Response {
    let url = format!("http://127.0.0.1:{port}/chat");
    let body = serde_json::json!({
        "message": message,
        "session_id": session_id,
        "topic": topic,
        "media": media,
        "target_profile_id": profile_id,
        "attach_only": attach_only,
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

    if stream {
        let stream = resp.bytes_stream();
        return match Response::builder()
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
        };
    }

    match resp.bytes().await {
        Ok(body) => match parse_sync_chat_response_from_sse(&body) {
            Ok(payload) => (StatusCode::OK, axum::Json(payload)).into_response(),
            Err(error) => json_error(StatusCode::BAD_GATEWAY, &error),
        },
        Err(error) => json_error(
            StatusCode::BAD_GATEWAY,
            &format!("failed to read gateway SSE response: {error}"),
        ),
    }
}

fn parse_sync_chat_response_from_sse(body: &[u8]) -> Result<serde_json::Value, String> {
    let text = std::str::from_utf8(body)
        .map_err(|error| format!("gateway SSE response was not valid UTF-8: {error}"))?;

    let mut content = String::new();
    let mut saw_done = false;
    let mut input_tokens: u32 = 0;
    let mut output_tokens: u32 = 0;

    for frame in text.split("\n\n") {
        for line in frame.lines() {
            let data = if let Some(raw) = line.strip_prefix("data:") {
                raw.trim()
            } else if let Some(raw) = line.strip_prefix("data: ") {
                raw.trim()
            } else {
                continue;
            };
            if data.is_empty() || data == "[DONE]" {
                continue;
            }

            let event: serde_json::Value = serde_json::from_str(data)
                .map_err(|error| format!("failed to parse gateway SSE event: {error}"))?;
            match event.get("type").and_then(|value| value.as_str()) {
                Some("token") => {
                    if let Some(text) = event.get("text").and_then(|value| value.as_str()) {
                        content.push_str(text);
                    }
                }
                Some("replace") => {
                    if let Some(text) = event.get("text").and_then(|value| value.as_str()) {
                        content = text.to_string();
                    }
                }
                Some("done") => {
                    saw_done = true;
                    if let Some(text) = event.get("content").and_then(|value| value.as_str()) {
                        if !text.is_empty() {
                            content = text.to_string();
                        }
                    }
                    input_tokens = event
                        .get("tokens_in")
                        .and_then(|value| value.as_u64())
                        .and_then(|value| u32::try_from(value).ok())
                        .unwrap_or(0);
                    output_tokens = event
                        .get("tokens_out")
                        .and_then(|value| value.as_u64())
                        .and_then(|value| u32::try_from(value).ok())
                        .unwrap_or(0);
                }
                Some("error") => {
                    let message = event
                        .get("message")
                        .and_then(|value| value.as_str())
                        .unwrap_or("gateway SSE returned an error event");
                    return Err(message.to_string());
                }
                _ => {}
            }
        }
    }

    if !saw_done {
        return Err("gateway SSE response ended without a done event".to_string());
    }

    Ok(serde_json::json!({
        "content": content,
        "input_tokens": input_tokens,
        "output_tokens": output_tokens,
    }))
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

/// Proxy a GET request that returns SSE from the gateway's API channel.
pub async fn api_sse_get_proxy(state: &AppState, port: u16, path: &str) -> Response {
    let url = format!("http://127.0.0.1:{port}{path}");
    let resp = match state.http_client.get(&url).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(port, error = %e, "API SSE GET proxy failed");
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

    let stream = resp.bytes_stream();
    match Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(Body::from_stream(stream))
    {
        Ok(response) => response,
        Err(error) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("failed to build SSE response: {error}"),
        ),
    }
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

    use super::{parse_sync_chat_response_from_sse, proxy_webhook_to_url};

    #[test]
    fn parse_sync_chat_response_from_sse_uses_done_metadata() {
        let body = br#"data: {"type":"thinking","iteration":0}

data: {"type":"replace","text":"partial"}

data: {"type":"done","content":"final answer","tokens_in":12,"tokens_out":34}

"#;

        let parsed = parse_sync_chat_response_from_sse(body).unwrap();
        assert_eq!(parsed["content"], "final answer");
        assert_eq!(parsed["input_tokens"], 12);
        assert_eq!(parsed["output_tokens"], 34);
    }

    #[test]
    fn parse_sync_chat_response_from_sse_falls_back_to_streamed_content() {
        let body = br#"data: {"type":"token","text":"hel"}

data: {"type":"token","text":"lo"}

data: {"type":"done","content":"","tokens_in":1,"tokens_out":2}

"#;

        let parsed = parse_sync_chat_response_from_sse(body).unwrap();
        assert_eq!(parsed["content"], "hello");
    }

    #[test]
    fn parse_sync_chat_response_from_sse_errors_without_done() {
        let body = br#"data: {"type":"replace","text":"partial only"}

"#;

        let error = parse_sync_chat_response_from_sse(body).unwrap_err();
        assert!(error.contains("done event"));
    }

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
