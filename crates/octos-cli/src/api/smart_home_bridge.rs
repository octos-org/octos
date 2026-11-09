//! Smart-home bridge HTTP client.
//!
//! Talks to a user-configured, self-hosted smart-home bridge (e.g. a Home
//! Assistant instance fronted by a small REST shim) over plain HTTP, using
//! the exact contract octos-web's browser client used to call directly:
//! `GET /devices`, `POST /devices/{id}` (form-encoded), `POST
//! /cameras/{id}/stream` (form-encoded), `POST /cameras/{id}/stop`.
//!
//! This deliberately does not route through `octos_agent::tools::ssrf`: that
//! guard exists to stop the LLM agent being tricked into fetching
//! attacker-chosen internal URLs via `web_fetch`/`browser` tool calls. A
//! smart-home bridge URL is admin-configured (typed into the profile's own
//! settings), not agent-supplied — a different trust boundary, same
//! reasoning as the plain `reqwest::Client` used by
//! `ominix_runtime::probe_health` for another local/admin-configured service.
//!
//! Scope: self-hosted / same-LAN bridges only (see
//! `crate::profiles::SmartHomeConfig` doc comment).

use std::time::Duration;

use serde::{Deserialize, Serialize};

const BRIDGE_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct SmartHomeDevice {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub room: Option<String>,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub on: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub online: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub readonly: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub brightness: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub humidity: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub muted: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_capable: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_protocol: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DeviceListResponse {
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub devices: Vec<SmartHomeDevice>,
    #[serde(default)]
    pub ok: Option<bool>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CameraStreamInfo {
    #[serde(default)]
    pub ok: Option<bool>,
    #[serde(default)]
    pub protocol: Option<String>,
    #[serde(default)]
    pub playback_url: Option<String>,
    #[serde(default)]
    pub stream_url: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

/// Resolved bridge connection details for one profile.
#[derive(Debug, Clone)]
pub struct BridgeConfig {
    pub base_url: String,
    pub token: Option<String>,
}

#[derive(Debug)]
pub enum BridgeError {
    /// No bridge_url configured for this profile.
    NotConfigured,
    /// Transport-level failure (DNS, connect, timeout, malformed response).
    Request(String),
    /// The bridge answered but reported an error (non-2xx or `ok: false`).
    Bridge(String),
}

impl std::fmt::Display for BridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BridgeError::NotConfigured => write!(f, "smart-home bridge is not configured"),
            BridgeError::Request(msg) => write!(f, "{msg}"),
            BridgeError::Bridge(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for BridgeError {}

fn apply_auth(builder: reqwest::RequestBuilder, config: &BridgeConfig) -> reqwest::RequestBuilder {
    match &config.token {
        Some(token) if !token.is_empty() => builder.bearer_auth(token),
        _ => builder,
    }
}

fn base(config: &BridgeConfig) -> String {
    config.base_url.trim_end_matches('/').to_string()
}

/// Percent-encode a path segment (RFC 3986 unreserved set passes through
/// unchanged). Device IDs are backend-registered slugs in practice; this is
/// defensive since they're interpolated directly into the URL path.
fn encode_path_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn json_value_to_form_string(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

pub async fn fetch_devices(
    client: &reqwest::Client,
    config: &BridgeConfig,
) -> Result<DeviceListResponse, BridgeError> {
    let url = format!("{}/devices", base(config));
    let resp = apply_auth(client.get(&url), config)
        .timeout(BRIDGE_REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|e| BridgeError::Request(e.to_string()))?;
    let status = resp.status();
    let data: DeviceListResponse = resp
        .json()
        .await
        .map_err(|e| BridgeError::Request(format!("invalid bridge response: {e}")))?;
    if !status.is_success() {
        return Err(BridgeError::Bridge(
            data.error.unwrap_or_else(|| format!("HTTP {status}")),
        ));
    }
    if data.ok == Some(false) {
        return Err(BridgeError::Bridge(
            data.error.unwrap_or_else(|| "bridge error".to_string()),
        ));
    }
    Ok(data)
}

pub async fn send_device_command(
    client: &reqwest::Client,
    config: &BridgeConfig,
    device_id: &str,
    params: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), BridgeError> {
    let url = format!(
        "{}/devices/{}",
        base(config),
        encode_path_segment(device_id)
    );
    let form: Vec<(String, String)> = params
        .iter()
        .map(|(k, v)| (k.clone(), json_value_to_form_string(v)))
        .collect();
    let resp = apply_auth(client.post(&url), config)
        .timeout(BRIDGE_REQUEST_TIMEOUT)
        .form(&form)
        .send()
        .await
        .map_err(|e| BridgeError::Request(e.to_string()))?;
    let status = resp.status();
    if !status.is_success() {
        let error = resp
            .json::<serde_json::Value>()
            .await
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(String::from))
            .unwrap_or_else(|| format!("HTTP {status}"));
        return Err(BridgeError::Bridge(error));
    }
    Ok(())
}

pub async fn start_camera_stream(
    client: &reqwest::Client,
    config: &BridgeConfig,
    device_id: &str,
    quality: Option<u32>,
) -> Result<CameraStreamInfo, BridgeError> {
    let url = format!(
        "{}/cameras/{}/stream",
        base(config),
        encode_path_segment(device_id)
    );
    let form = vec![("quality".to_string(), quality.unwrap_or(2).to_string())];
    let resp = apply_auth(client.post(&url), config)
        .timeout(BRIDGE_REQUEST_TIMEOUT)
        .form(&form)
        .send()
        .await
        .map_err(|e| BridgeError::Request(e.to_string()))?;
    let status = resp.status();
    let data: CameraStreamInfo = resp
        .json()
        .await
        .map_err(|e| BridgeError::Request(format!("invalid bridge response: {e}")))?;
    if !status.is_success() {
        return Err(BridgeError::Bridge(
            data.error.unwrap_or_else(|| format!("HTTP {status}")),
        ));
    }
    Ok(data)
}

pub async fn stop_camera_stream(
    client: &reqwest::Client,
    config: &BridgeConfig,
    device_id: &str,
) -> Result<(), BridgeError> {
    let url = format!(
        "{}/cameras/{}/stop",
        base(config),
        encode_path_segment(device_id)
    );
    let resp = apply_auth(client.post(&url), config)
        .timeout(BRIDGE_REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|e| BridgeError::Request(e.to_string()))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(BridgeError::Bridge(format!("HTTP {status}")));
    }
    Ok(())
}

/// Resolve a profile's smart-home bridge config into a `BridgeConfig`,
/// resolving `token`/`token_env` the same way `SmartHomeConfig::to_env_vars`
/// does. Returns `None` when no bridge_url is configured.
pub fn resolve_bridge_config(
    smart_home: &crate::profiles::SmartHomeConfig,
    env_vars: &std::collections::HashMap<String, String>,
) -> Option<BridgeConfig> {
    let base_url = smart_home.bridge_url.clone()?;
    let token = smart_home.token.clone().or_else(|| {
        smart_home
            .token_env
            .as_ref()
            .and_then(|name| env_vars.get(name).cloned())
    });
    Some(BridgeConfig { base_url, token })
}
