//! HTTP-backed tool.
//!
//! Dispatches each tool call as `POST <base_url>/tools/<tool_name>` with
//! body `{"args": <args>}`. Parses the SPEC-VENDOR-NODE-V1 response shape
//! (`ok`/`code`/`msg`/`data`) and maps it to `ToolResult`.
//!
//! Security: the constructor rejects any base_url that does not resolve
//! to a loopback host. This bridge is designed for co-located bridge
//! processes only; cross-host transport is out of scope.
//!
//! **Operator note:** prefer literal `127.0.0.1` or `[::1]` over `localhost`
//! or any other hostname in skill manifests. The constructor pins loopback
//! at build time via DNS resolution, but `reqwest` re-resolves at request
//! time, so a hostname whose DNS later rebinds to a public IP would bypass
//! the build-time guard. Literal IPs eliminate this TOCTOU window.

use std::net::ToSocketAddrs;
use std::time::Duration;

use async_trait::async_trait;
use eyre::{Result, eyre};
use serde_json::Value;
use url::Url;

use crate::tools::{Tool, ToolResult};

const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// A Tool that dispatches calls over HTTP to a local bridge process.
#[derive(Debug)]
pub struct HttpTool {
    name: String,
    description: String,
    base_url: String,
    input_schema: Value,
    client: reqwest::Client,
    #[allow(dead_code)] // reserved for per-call timeout override later
    timeout: Duration,
}

impl HttpTool {
    /// Build an HttpTool. Fails if `base_url` is not a loopback HTTP URL.
    pub fn new(
        name: String,
        description: String,
        base_url: String,
        input_schema: Value,
    ) -> Result<Self> {
        validate_loopback_url(&base_url)?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(DEFAULT_TIMEOUT_SECS))
            .build()
            .map_err(|e| eyre!("reqwest client build failed: {e}"))?;
        Ok(Self {
            name,
            description,
            base_url: base_url.trim_end_matches('/').to_string(),
            input_schema,
            client,
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
        })
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
        url::Host::Domain(hostname) => {
            if hostname == "localhost" {
                return Ok(());
            }
            // Resolve hostname via DNS; require ALL resolved addresses to be loopback.
            let port = url.port_or_known_default().unwrap_or(80);
            let mut any = false;
            for addr in (hostname, port)
                .to_socket_addrs()
                .map_err(|e| eyre!("base_url {raw:?}: DNS lookup failed: {e}"))?
            {
                any = true;
                if !addr.ip().is_loopback() {
                    return Err(eyre!(
                        "base_url {raw:?} resolves to non-loopback ip {}; only loopback allowed",
                        addr.ip()
                    ));
                }
            }
            if !any {
                return Err(eyre!("base_url {raw:?}: hostname resolved to no addresses"));
            }
            Ok(())
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
        let envelope: Value = match resp.json().await {
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
        )
        .unwrap_err();
        assert!(err.to_string().contains("http"));
    }

    #[tokio::test]
    async fn accepts_loopback_v4_and_v6_and_localhost() {
        for url in [
            "http://127.0.0.1:8765",
            "http://[::1]:8765",
            "http://localhost:8765",
        ] {
            HttpTool::new("t".into(), "d".into(), url.into(), json!({}))
                .unwrap_or_else(|e| panic!("rejected loopback url {url}: {e}"));
        }
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
        )
        .unwrap();
        let result = tool.execute(&json!({})).await.unwrap();
        assert!(!result.success);
        assert!(result.output.contains("VENDOR_ERROR"));
        assert!(result.output.contains("RPC timeout"));
    }

    #[tokio::test]
    async fn execute_handles_http_500() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/something"))
            .respond_with(ResponseTemplate::new(500).set_body_string("internal"))
            .mount(&server)
            .await;

        let tool =
            HttpTool::new("something".into(), "desc".into(), server.uri(), json!({})).unwrap();
        let result = tool.execute(&json!({})).await.unwrap();
        assert!(!result.success);
        assert!(result.output.contains("500") || result.output.contains("BRIDGE_HTTP"));
    }
}
