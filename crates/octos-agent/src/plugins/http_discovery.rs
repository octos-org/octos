//! HTTP-based tool discovery for skills with `tool_discovery = Http { base_url }`.
//!
//! Implements `GET <base_url>/tools` to enumerate available tools, then
//! constructs `HttpTool` instances for each entry. Aborts on any non-2xx
//! response or network error so that install fails loudly rather than
//! silently registering a partial tool set.

use std::sync::Arc;
use std::time::Duration;

use eyre::{Result, eyre};
use serde::Deserialize;

use crate::tools::HttpTool;
use crate::tools::{Tool, ToolRegistry};

/// Timeout for the discovery HTTP call.
const DISCOVERY_TIMEOUT_SECS: u64 = 15;

/// A single tool entry returned by `GET <base_url>/tools`.
#[derive(Debug, Deserialize)]
pub struct HttpToolEntry {
    /// Tool name (snake_case, unique).
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// JSON Schema for tool arguments.
    #[serde(default = "default_schema")]
    pub input_schema: serde_json::Value,
}

fn default_schema() -> serde_json::Value {
    serde_json::json!({"type": "object"})
}

/// Fetch the tool catalog from `<base_url>/tools`.
///
/// Returns an error (causing install to fail) if:
/// - The HTTP request fails (network / timeout)
/// - The server returns a non-2xx status
/// - The response body is not a valid JSON array of tool entries
pub async fn fetch_http_tool_catalog(base_url: &str) -> Result<Vec<HttpToolEntry>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(DISCOVERY_TIMEOUT_SECS))
        .build()
        .map_err(|e| eyre!("failed to build HTTP client for discovery: {e}"))?;

    let url = format!("{}/tools", base_url.trim_end_matches('/'));
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| eyre!("HTTP tool discovery request to {url} failed: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(eyre!(
            "HTTP tool discovery at {url} returned {}: {}",
            status.as_u16(),
            body.trim()
        ));
    }

    let entries: Vec<HttpToolEntry> = resp
        .json()
        .await
        .map_err(|e| eyre!("failed to parse tool catalog from {url}: {e}"))?;

    Ok(entries)
}

/// Register all HTTP-discovered tools into `registry`.
///
/// Constructs an `HttpTool` per entry and registers it. Returns an error
/// if any entry fails validation (e.g., non-loopback base_url).
pub async fn register_http_tools(
    registry: &mut ToolRegistry,
    base_url: &str,
) -> Result<Vec<String>> {
    let entries = fetch_http_tool_catalog(base_url).await?;
    let mut names = Vec::with_capacity(entries.len());

    for entry in entries {
        let tool = HttpTool::new(
            entry.name.clone(),
            entry.description,
            base_url.to_string(),
            entry.input_schema,
        )?;
        names.push(entry.name.clone());
        registry.register_arc(Arc::new(tool) as Arc<dyn Tool>);
    }

    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn fetch_returns_entries_on_200() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/tools"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"name": "foo", "description": "A foo tool"},
                {"name": "bar", "description": "A bar tool", "input_schema": {"type": "object"}}
            ])))
            .mount(&server)
            .await;

        let entries = fetch_http_tool_catalog(&server.uri()).await.unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "foo");
        assert_eq!(entries[1].name, "bar");
    }

    #[tokio::test]
    async fn fetch_fails_on_500() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/tools"))
            .respond_with(ResponseTemplate::new(500).set_body_string("internal error"))
            .mount(&server)
            .await;

        let err = fetch_http_tool_catalog(&server.uri()).await.unwrap_err();
        assert!(err.to_string().contains("500"), "got: {err}");
    }

    #[tokio::test]
    async fn fetch_fails_on_non_array_body() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/tools"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"not": "an array"})))
            .mount(&server)
            .await;

        let err = fetch_http_tool_catalog(&server.uri()).await.unwrap_err();
        assert!(err.to_string().contains("parse"), "got: {err}");
    }
}
