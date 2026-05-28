//! HTTP-backed tool.
//!
//! Dispatches each tool call as `POST <base_url>/tools/<tool_name>` with
//! body `{"args": <args>}`. Parses the SPEC-VENDOR-NODE-V1 response shape
//! (`ok`/`code`/`msg`/`data`) and maps it to `ToolResult`.
//!
//! Security: the constructor requires the base_url to use a literal
//! loopback IP (`127.0.0.0/8` or `::1`). Hostnames — including
//! `localhost` — are rejected because `reqwest` re-resolves DNS at
//! request time, and a hostname whose resolution changes after
//! construction would bypass the build-time guard. This bridge is
//! designed for co-located bridge processes only; cross-host transport
//! is out of scope.
//!
//! **Operator note:** skill manifests must use `127.0.0.1` or `[::1]`
//! for HTTP `base_url`. `localhost` and any other hostname will be
//! refused at skill registration time.

use std::time::Duration;

use async_trait::async_trait;
use eyre::{Result, eyre};
use serde_json::Value;
use url::Url;

use crate::tools::{Tool, ToolResult};

#[cfg(test)]
const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// Maximum HTTP response body size for the per-call tool path. Bridge tools
/// returning more than this are treated as an error (BRIDGE_HTTP) rather
/// than allowing OOM. 1 MiB matches the agent's general response budget.
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// A Tool that dispatches calls over HTTP to a local bridge process.
#[derive(Debug)]
pub struct HttpTool {
    name: String,
    description: String,
    base_url: String,
    input_schema: Value,
    client: reqwest::Client,
    /// Per-call timeout. Sourced from `DoraToolMapping::timeout_secs` at
    /// construction time. Retained for inspection / future per-call override
    /// pathways; the actual enforcement happens inside `client` via
    /// `reqwest::Client::builder().timeout(...)`.
    timeout: Duration,
}

impl HttpTool {
    /// Build an HttpTool. Fails if `base_url` is not a loopback HTTP URL.
    pub fn new(
        name: String,
        description: String,
        base_url: String,
        input_schema: Value,
        timeout_secs: u64,
    ) -> Result<Self> {
        validate_loopback_url(&base_url)?;
        let timeout = Duration::from_secs(timeout_secs);
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| eyre!("reqwest client build failed: {e}"))?;
        Ok(Self {
            name,
            description,
            base_url: base_url.trim_end_matches('/').to_string(),
            input_schema,
            client,
            timeout,
        })
    }

    /// Per-call timeout this tool was constructed with.
    #[allow(dead_code)] // exposed for diagnostics / future per-call override pathways
    pub(crate) fn timeout(&self) -> Duration {
        self.timeout
    }
}

fn validate_loopback_url(raw: &str) -> Result<()> {
    let url = Url::parse(raw).map_err(|e| eyre!("invalid base_url {raw:?}: {e}"))?;
    if url.scheme() != "http" {
        return Err(eyre!(
            "base_url {raw:?} must use http scheme (got {:?})",
            url.scheme()
        ));
    }
    // Use the typed Host enum so IPv6 addresses are handled correctly.
    // url::Host::Ipv4/Ipv6 avoids the bracket-stripping issue with host_str().
    match url
        .host()
        .ok_or_else(|| eyre!("base_url {raw:?} has no host"))?
    {
        url::Host::Domain(_) => {
            // Reject every hostname, including "localhost". reqwest re-resolves
            // DNS at request time; a hostname whose resolution later flips to
            // a non-loopback address would slip past any build-time DNS check.
            // PR #1260 review: close the TOCTOU window by requiring a literal
            // loopback IP.
            Err(eyre!(
                "base_url {raw:?} uses a hostname; only literal loopback IPs (127.0.0.0/8 or [::1]) are allowed"
            ))
        }
        url::Host::Ipv4(ip) => {
            if ip.is_loopback() {
                Ok(())
            } else {
                Err(eyre!(
                    "base_url {raw:?} resolves to non-loopback ip {ip}; only loopback allowed"
                ))
            }
        }
        url::Host::Ipv6(ip) => {
            if ip.is_loopback() {
                Ok(())
            } else {
                Err(eyre!(
                    "base_url {raw:?} resolves to non-loopback ip {ip}; only loopback allowed"
                ))
            }
        }
    }
}

#[async_trait]
impl Tool for HttpTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> Value {
        self.input_schema.clone()
    }

    fn tags(&self) -> &[&str] {
        &["http", "bridge"]
    }

    async fn execute(&self, args: &Value) -> Result<ToolResult> {
        let url = format!("{}/tools/{}", self.base_url, self.name);
        let body = serde_json::json!({ "args": args });
        let resp = match self.client.post(&url).json(&body).send().await {
            Ok(r) => r,
            Err(e) => return Ok(error_result(format!("BRIDGE_HTTP: {e}"))),
        };
        let status = resp.status();
        if !status.is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            return Ok(error_result(format!(
                "BRIDGE_HTTP {}: {}",
                status.as_u16(),
                body_text
            )));
        }
        // Pre-flight Content-Length check: short-circuit before reading the
        // body when the server advertises an over-cap size.
        if let Some(declared) = resp.content_length() {
            if declared as usize > MAX_RESPONSE_BYTES {
                return Ok(error_result(format!(
                    "BRIDGE_HTTP response too large: declared size {declared} > {MAX_RESPONSE_BYTES} bytes"
                )));
            }
        }
        // Post-read check: also enforce on the actual byte count to handle
        // chunked responses without `Content-Length` and lying servers.
        let bytes = match resp.bytes().await {
            Ok(b) => b,
            Err(e) => return Ok(error_result(format!("BRIDGE_HTTP read: {e}"))),
        };
        if bytes.len() > MAX_RESPONSE_BYTES {
            return Ok(error_result(format!(
                "BRIDGE_HTTP response too large: {} > {} bytes",
                bytes.len(),
                MAX_RESPONSE_BYTES
            )));
        }
        let envelope: Value = match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(e) => return Ok(error_result(format!("BRIDGE_HTTP parse: {e}"))),
        };
        let ok = envelope.get("ok").and_then(Value::as_bool).unwrap_or(false);
        // SPEC-V1 says `code` is a string, but some vendors emit numeric codes.
        // Accept both: string passes through; integer becomes its decimal form.
        let code = envelope
            .get("code")
            .and_then(|v| {
                v.as_str()
                    .map(str::to_string)
                    .or_else(|| v.as_i64().map(|n| n.to_string()))
            })
            .unwrap_or_else(|| "0".to_string());
        let msg = envelope
            .get("msg")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let data = envelope.get("data").cloned().unwrap_or(Value::Null);

        let output = if ok {
            data.to_string()
        } else {
            serde_json::json!({"code": code, "msg": msg, "data": data}).to_string()
        };
        Ok(ToolResult {
            success: ok,
            output,
            ..Default::default()
        })
    }
}

fn error_result(msg: String) -> ToolResult {
    ToolResult {
        success: false,
        output: msg,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{body_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn rejects_non_loopback_base_url() {
        let err = HttpTool::new(
            "test_tool".into(),
            "desc".into(),
            "http://example.com".into(),
            json!({"type": "object"}),
            DEFAULT_TIMEOUT_SECS,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("loopback"), "got: {msg}");
    }

    #[tokio::test]
    async fn rejects_non_http_scheme() {
        let err = HttpTool::new(
            "test_tool".into(),
            "desc".into(),
            "ftp://localhost/x".into(),
            json!({}),
            DEFAULT_TIMEOUT_SECS,
        )
        .unwrap_err();
        assert!(err.to_string().contains("http"));
    }

    #[tokio::test]
    async fn accepts_literal_loopback_ips() {
        for url in ["http://127.0.0.1:8765", "http://[::1]:8765"] {
            HttpTool::new(
                "t".into(),
                "d".into(),
                url.into(),
                json!({}),
                DEFAULT_TIMEOUT_SECS,
            )
            .unwrap_or_else(|e| panic!("rejected loopback url {url}: {e}"));
        }
    }

    #[tokio::test]
    async fn rejects_localhost_hostname_to_close_dns_toctou() {
        // SPEC-V1 review (PR #1260): even "localhost" must be a literal IP,
        // because reqwest re-resolves hostnames at request time and a
        // hostname's DNS may rebind to a non-loopback address between
        // construction and dispatch.
        let err = HttpTool::new(
            "t".into(),
            "d".into(),
            "http://localhost:8765".into(),
            json!({}),
            DEFAULT_TIMEOUT_SECS,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("literal") || msg.contains("hostname"),
            "expected hostname-rejection message, got: {msg}"
        );
    }

    #[tokio::test]
    async fn rejects_any_domain_hostname_even_if_it_resolves_to_loopback() {
        // Some hosts have /etc/hosts entries pointing custom names at 127.x;
        // accepting them would leave the same DNS-rebind TOCTOU open.
        let err = HttpTool::new(
            "t".into(),
            "d".into(),
            "http://example.com:8765".into(),
            json!({}),
            DEFAULT_TIMEOUT_SECS,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("literal") || msg.contains("hostname"),
            "expected hostname-rejection message, got: {msg}"
        );
    }

    #[tokio::test]
    async fn execute_posts_args_and_parses_ok_response() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/robot.heartbeat"))
            .and(body_json(json!({"args": {}})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true,
                "code": "0",
                "msg": "",
                "data": {"applied": true}
            })))
            .mount(&server)
            .await;

        let tool = HttpTool::new(
            "robot.heartbeat".into(),
            "heartbeat".into(),
            server.uri(),
            json!({"type": "object"}),
            DEFAULT_TIMEOUT_SECS,
        )
        .unwrap();
        let result = tool.execute(&json!({})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("\"applied\""));
    }

    #[tokio::test]
    async fn execute_surfaces_vendor_error_code() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/robot.estop"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": false,
                "code": "VENDOR_ERROR",
                "msg": "RPC timeout",
                "data": {}
            })))
            .mount(&server)
            .await;

        let tool = HttpTool::new(
            "robot.estop".into(),
            "estop".into(),
            server.uri(),
            json!({}),
            DEFAULT_TIMEOUT_SECS,
        )
        .unwrap();
        let result = tool.execute(&json!({})).await.unwrap();
        assert!(!result.success);
        assert!(result.output.contains("VENDOR_ERROR"));
        assert!(result.output.contains("RPC timeout"));
    }

    #[tokio::test]
    async fn execute_rejects_oversized_response() {
        let server = MockServer::start().await;
        let huge = "a".repeat(2 * 1024 * 1024); // 2 MiB > 1 MiB cap
        Mock::given(method("POST"))
            .and(path("/tools/big"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(huge.as_bytes(), "application/json"),
            )
            .mount(&server)
            .await;

        let tool = HttpTool::new(
            "big".into(),
            "d".into(),
            server.uri(),
            json!({}),
            DEFAULT_TIMEOUT_SECS,
        )
        .unwrap();
        let result = tool.execute(&json!({})).await.unwrap();
        assert!(!result.success, "expected failure on oversized body");
        let out_lc = result.output.to_lowercase();
        assert!(
            out_lc.contains("size")
                || out_lc.contains("too large")
                || out_lc.contains("bridge_http"),
            "expected size-related error, got: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn execute_handles_http_500() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/something"))
            .respond_with(ResponseTemplate::new(500).set_body_string("internal"))
            .mount(&server)
            .await;

        let tool = HttpTool::new(
            "something".into(),
            "desc".into(),
            server.uri(),
            json!({}),
            DEFAULT_TIMEOUT_SECS,
        )
        .unwrap();
        let result = tool.execute(&json!({})).await.unwrap();
        assert!(!result.success);
        assert!(result.output.contains("500") || result.output.contains("BRIDGE_HTTP"));
    }
}
