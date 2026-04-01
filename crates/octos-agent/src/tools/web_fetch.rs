//! Web fetch tool for retrieving URL content.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use reqwest::Client;
use reqwest::redirect::Policy;
use serde::Deserialize;

use super::{Tool, ToolResult};

/// Maximum number of redirects to follow (with SSRF validation per hop).
const MAX_REDIRECTS: usize = 10;

pub struct WebFetchTool {
    config: Option<Arc<super::tool_config::ToolConfigStore>>,
}

impl WebFetchTool {
    pub fn new() -> Self {
        Self { config: None }
    }

    pub fn with_config(mut self, config: Arc<super::tool_config::ToolConfigStore>) -> Self {
        self.config = Some(config);
        self
    }
}

impl Default for WebFetchTool {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Deserialize)]
struct Input {
    url: String,
    #[serde(default)]
    extract_mode: Option<String>,
    #[serde(default)]
    max_chars: Option<usize>,
}

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn description(&self) -> &str {
        "Fetch a URL and extract its content as markdown or plain text."
    }

    fn tags(&self) -> &[&str] {
        &["web"]
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "The URL to fetch"
                },
                "extract_mode": {
                    "type": "string",
                    "enum": ["markdown", "text"],
                    "description": "Output format: 'markdown' (default) or 'text'"
                },
                "max_chars": {
                    "type": "integer",
                    "description": "Maximum characters to return (default: 50000)"
                }
            },
            "required": ["url"]
        })
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let input: Input =
            serde_json::from_value(args.clone()).wrap_err("invalid web_fetch input")?;

        let (cfg_extract_mode, cfg_max_chars) = match &self.config {
            Some(c) => (
                c.get_str("web_fetch", "extract_mode").await,
                c.get_usize("web_fetch", "max_chars").await,
            ),
            None => (None, None),
        };
        let extract_mode = input
            .extract_mode
            .or(cfg_extract_mode)
            .unwrap_or_else(|| "markdown".to_string());
        let max_chars = input.max_chars.or(cfg_max_chars).unwrap_or(50_000);

        if !input.url.starts_with("http://") && !input.url.starts_with("https://") {
            return Ok(ToolResult {
                output: "URL must start with http:// or https://".to_string(),
                success: false,
                ..Default::default()
            });
        }

        // SSRF-safe fetch: validate initial URL, disable auto-redirects,
        // and re-validate each redirect hop against SSRF rules.
        let response = match ssrf_safe_fetch(&input.url).await {
            Ok(r) => r,
            Err(msg) => {
                return Ok(ToolResult {
                    output: msg,
                    success: false,
                    ..Default::default()
                });
            }
        };

        let status = response.status();
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let final_url = response.url().to_string();

        if !status.is_success() {
            return Ok(ToolResult {
                output: format!("HTTP {status} for {}", input.url),
                success: false,
                ..Default::default()
            });
        }

        // Cap response body to prevent OOM on huge responses.
        // Reject early if Content-Length exceeds limit, then stream-read
        // up to MAX_BODY_BYTES to avoid buffering unbounded data.
        const MAX_BODY_BYTES: usize = 5_000_000;
        if let Some(len) = response.content_length() {
            if len > MAX_BODY_BYTES as u64 {
                return Ok(ToolResult {
                    output: format!("Response too large ({} bytes, max {})", len, MAX_BODY_BYTES),
                    success: false,
                    ..Default::default()
                });
            }
        }
        let body = {
            let mut buf = Vec::with_capacity(MAX_BODY_BYTES.min(256_000));
            let mut stream = response.bytes_stream();
            use futures::StreamExt;
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.wrap_err("error reading response stream")?;
                buf.extend_from_slice(&chunk);
                if buf.len() > MAX_BODY_BYTES {
                    buf.truncate(MAX_BODY_BYTES);
                    break;
                }
            }
            String::from_utf8_lossy(&buf).into_owned()
        };

        let is_html = content_type.contains("text/html");
        let mut content = if is_html {
            match extract_mode.as_str() {
                "text" => extract_text(&body),
                _ => extract_markdown(&body),
            }
        } else {
            body
        };

        octos_core::truncate_utf8(&mut content, max_chars, "\n\n... (content truncated)");

        let mut output = format!("URL: {final_url}\n");
        if final_url != input.url {
            output.push_str(&format!("Redirected from: {}\n", input.url));
        }
        output.push_str(&format!("Content-Type: {content_type}\n"));
        output.push_str(&format!("Length: {} chars\n\n", content.len()));
        output.push_str(&content);

        Ok(ToolResult {
            output,
            success: true,
            ..Default::default()
        })
    }
}

/// Validate a URL against SSRF rules, build a pinned client, and fetch.
/// Redirects are followed manually with SSRF validation on each hop.
/// DNS failures are treated as blocked (fail-closed).
async fn ssrf_safe_fetch(initial_url: &str) -> Result<reqwest::Response, String> {
    let mut current_url = initial_url.to_string();

    for _ in 0..MAX_REDIRECTS {
        // Validate the URL and resolve DNS (fail-closed on DNS error).
        let check = super::ssrf::check_ssrf_with_addrs(&current_url).await?;

        let parsed = reqwest::Url::parse(&current_url).map_err(|_| "Invalid URL".to_string())?;
        let host = parsed
            .host_str()
            .ok_or_else(|| "URL has no host".to_string())?
            .to_string();

        // Build a per-request client with redirects disabled and DNS pinned.
        let mut builder = Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent("octos/0.1 (web-fetch-tool)")
            .redirect(Policy::none());
        for addr in &check.resolved_addrs {
            builder = builder.resolve(&host, *addr);
        }
        let client = builder
            .build()
            .map_err(|e| format!("HTTP client error: {e}"))?;

        let response = client
            .get(&current_url)
            .send()
            .await
            .map_err(|e| format!("Failed to fetch URL: {e}"))?;

        if !response.status().is_redirection() {
            return Ok(response);
        }

        let location = response
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| "Redirect with no Location header".to_string())?;
        // Resolve relative redirects against the current URL.
        current_url = parsed
            .join(location)
            .map_err(|_| format!("Invalid redirect URL: {location}"))?
            .to_string();
    }

    Err(format!("Too many redirects (max {MAX_REDIRECTS})"))
}

fn extract_markdown(html: &str) -> String {
    htmd::convert(html).unwrap_or_else(|_| extract_text(html))
}

fn extract_text(html: &str) -> String {
    let mut result = String::with_capacity(html.len());
    let mut in_tag = false;

    for c in html.chars() {
        if c == '<' {
            in_tag = true;
            continue;
        }
        if c == '>' {
            in_tag = false;
            result.push(' ');
            continue;
        }
        if !in_tag {
            result.push(c);
        }
    }

    result.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_text() {
        let html = "<h1>Hello</h1><p>World <b>bold</b></p>";
        let text = extract_text(html);
        assert_eq!(text, "Hello World bold");
    }

    #[test]
    fn test_extract_text_with_whitespace() {
        let html = "<div>\n  <p>  spaced  </p>\n</div>";
        let text = extract_text(html);
        assert_eq!(text, "spaced");
    }

    #[tokio::test]
    async fn test_invalid_url_scheme() {
        let tool = WebFetchTool::new();
        let result = tool
            .execute(&serde_json::json!({"url": "ftp://example.com"}))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.output.contains("http://"));
    }

    #[tokio::test]
    async fn test_invalid_input() {
        let tool = WebFetchTool::new();
        let result = tool.execute(&serde_json::json!({})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_dns_rebind_localhost() {
        // "localhost" should be caught by hostname check before DNS
        let tool = WebFetchTool::new();
        let result = tool
            .execute(&serde_json::json!({"url": "http://localhost/test"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.output.contains("private"));
    }

    #[test]
    fn test_extract_markdown() {
        let html = "<h1>Title</h1><p>Paragraph</p>";
        let md = extract_markdown(html);
        assert!(md.contains("Title"));
        assert!(md.contains("Paragraph"));
    }

    #[tokio::test]
    async fn test_ssrf_redirect_to_private_ip_blocked() {
        // A redirect to a private IP must be blocked.
        // We test the ssrf_safe_fetch function directly with localhost.
        let result = ssrf_safe_fetch("http://127.0.0.1/secret").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("private"));
    }

    #[tokio::test]
    async fn test_ssrf_dns_failure_blocks_request() {
        // DNS failure must fail closed, not fall through to an unpinned client.
        let result =
            ssrf_safe_fetch("https://this-domain-does-not-exist-ssrf-test.invalid/foo").await;
        assert!(result.is_err(), "DNS failure should block request");
        let err = result.unwrap_err();
        assert!(
            err.contains("DNS resolution failed") || err.contains("fail closed"),
            "error should indicate DNS failure: {err}"
        );
    }

    #[tokio::test]
    async fn test_ssrf_metadata_endpoint_blocked() {
        let result = ssrf_safe_fetch("http://169.254.169.254/latest/meta-data/").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("private"));
    }
}
