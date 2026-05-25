//! HTTP-based tool discovery for skills with `tool_discovery = Http { base_url }`.
//!
//! Implements `GET <base_url>/tools` to enumerate available tools, then
//! constructs `DoraToolBridge` instances for each entry and enrols them in
//! the [`crate::tools::robot_groups`] registry so that the standard
//! `group:robot:<tier>` ToolPolicy machinery applies.
//!
//! Aborts on any non-2xx response or network error so that install fails
//! loudly rather than silently registering a partial tool set.

use std::sync::Arc;
use std::time::Duration;

use eyre::{Result, eyre};
use serde::Deserialize;

use crate::tools::dora_bridge::{DoraToolBridge, DoraToolMapping, default_timeout};
use crate::tools::{Tool, ToolRegistry, robot_groups};

/// Timeout for the discovery HTTP call.
const DISCOVERY_TIMEOUT_SECS: u64 = 15;

/// Cap on the catalog response size to bound startup memory for a
/// misbehaving bridge. 1 MiB matches the per-call HTTP tool body cap.
const MAX_CATALOG_BYTES: usize = 1024 * 1024;

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
    /// New in SPEC-V1 1.1 — the bridge propagates safety_tier from the
    /// vendor advert. When absent, the loader falls back to manifest defaults
    /// (`tool_overrides[name]` then `required_safety_tier`).
    #[serde(default)]
    pub safety_tier: Option<crate::permissions::SafetyTier>,
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

    // Pre-flight Content-Length cap (best-effort: many servers don't advertise
    // a length on streamed bodies).
    if let Some(declared) = resp.content_length() {
        if declared as usize > MAX_CATALOG_BYTES {
            return Err(eyre!(
                "catalog response too large: declared size {declared} > {MAX_CATALOG_BYTES} bytes"
            ));
        }
    }

    let bytes = resp
        .bytes()
        .await
        .map_err(|e| eyre!("failed to read tool catalog from {url}: {e}"))?;
    if bytes.len() > MAX_CATALOG_BYTES {
        return Err(eyre!(
            "catalog response too large: {} > {} bytes",
            bytes.len(),
            MAX_CATALOG_BYTES
        ));
    }

    let entries: Vec<HttpToolEntry> = serde_json::from_slice(&bytes)
        .map_err(|e| eyre!("failed to parse tool catalog from {url}: {e}"))?;

    Ok(entries)
}

/// Fetch the HTTP tool catalog and register each entry as a [`DoraToolBridge`]
/// with the hybrid safety_tier resolved per:
///   catalog `safety_tier` > `manifest.tool_overrides[name]` > `manifest.required_safety_tier`.
///
/// Each registered bridge is also enrolled in [`robot_groups`] so the
/// existing `group:robot:<tier>` ToolPolicy machinery applies.
///
/// `manifest` is `None` when the caller has no manifest context (e.g. ad-hoc
/// callers / tests); in that case the per-tool fallback is
/// [`crate::permissions::SafetyTier::Observe`].
///
/// Returns the list of registered tool names. On HTTP/parse error, returns
/// an `Err` (no partial registration occurs).
pub async fn install_http_tools_from_catalog(
    registry: &mut ToolRegistry,
    base_url: &str,
    manifest: Option<&super::manifest::PluginManifest>,
) -> Result<Vec<String>> {
    let entries = fetch_http_tool_catalog(base_url).await?;
    let mut names = Vec::with_capacity(entries.len());

    for entry in entries {
        // Hybrid tier resolution: catalog > tool_overrides > manifest default.
        let tier = entry
            .safety_tier
            .or_else(|| manifest.and_then(|m| m.tool_overrides.get(&entry.name).copied()))
            .unwrap_or_else(|| {
                manifest
                    .map(|m| m.required_safety_tier)
                    .unwrap_or(crate::permissions::SafetyTier::Observe)
            });

        let mapping = DoraToolMapping {
            tool_name: entry.name.clone(),
            description: entry.description.clone(),
            bridge_base_url: base_url.to_string(),
            safety_tier: tier,
            input_schema: Some(entry.input_schema.clone()),
            timeout_secs: default_timeout(),
            concurrency_class: None,
            dora_node_id: None,
            dora_output_id: None,
        };
        let bridge = DoraToolBridge::new(mapping)
            .map_err(|e| eyre!("failed to construct HTTP bridge for {}: {e}", entry.name))?;

        // Register the tool first so that any duplicate-name rejection (future
        // behaviour) doesn't leave a stale tier mapping behind. Today
        // `register_arc` is infallible / overwrites, but the ordering is the
        // safer invariant to lock in.
        let tool_name = entry.name.clone();
        registry.register_arc(Arc::new(bridge) as Arc<dyn Tool>);
        robot_groups::with_registry_mut(|reg| {
            reg.insert(tool_name.clone(), tier);
        });

        names.push(tool_name);
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

    #[tokio::test]
    async fn fetch_rejects_oversized_catalog() {
        let server = MockServer::start().await;
        let huge = "a".repeat(2 * 1024 * 1024); // 2 MiB > 1 MiB cap
        Mock::given(method("GET"))
            .and(path("/tools"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(huge.as_bytes(), "application/json"),
            )
            .mount(&server)
            .await;

        let err = fetch_http_tool_catalog(&server.uri()).await.unwrap_err();
        let s = err.to_string().to_lowercase();
        assert!(
            s.contains("size") || s.contains("too large"),
            "expected size error, got: {err}"
        );
    }

    #[tokio::test]
    async fn fetch_includes_safety_tier_when_provided() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/tools"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"name": "robot.estop", "description": "Estop", "safety_tier": "emergency_override"},
                {"name": "robot.read", "description": "Read sensor"}
            ])))
            .mount(&server)
            .await;

        let entries = fetch_http_tool_catalog(&server.uri()).await.unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[0].safety_tier,
            Some(crate::permissions::SafetyTier::EmergencyOverride)
        );
        assert_eq!(entries[1].safety_tier, None);
    }
}
