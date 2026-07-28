//! Smart-home skill: list and control devices via the profile's configured
//! bridge (e.g. Home Assistant).
//!
//! Protocol: `./smart_home <tool_name>` with JSON on stdin, JSON on stdout.
//!
//! Deliberately does not proxy through the running octos server: like
//! `account-manager`, this reads the profile JSON directly from
//! `$OCTOS_HOME/profiles/<id>.json` and talks to the bridge itself,
//! mirroring the wire contract in
//! `crates/octos-cli/src/api/smart_home_bridge.rs` (`GET {base}/devices`,
//! `POST {base}/devices/{id}` form-encoded, Bearer auth, same
//! token/token_env resolution precedence). Camera streaming is deliberately
//! NOT exposed here: an LLM tool call can't consume a live video stream, so
//! that stays a human-driven, WS-only feature in octos-web
//! (`smart_home/camera.*`).

use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Map, Value};

#[derive(Deserialize)]
struct UserProfile {
    #[serde(default)]
    config: ProfileConfig,
}

#[derive(Deserialize, Default)]
struct ProfileConfig {
    #[serde(default)]
    smart_home: Option<SmartHomeConfig>,
    #[serde(default)]
    env_vars: HashMap<String, String>,
}

#[derive(Deserialize, Default)]
struct SmartHomeConfig {
    #[serde(default)]
    bridge_url: Option<String>,
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    token_env: Option<String>,
}

struct BridgeConfig {
    base_url: String,
    token: Option<String>,
}

#[derive(Deserialize)]
struct ListDevicesInput {
    #[serde(default)]
    room: Option<String>,
}

#[derive(Deserialize)]
struct ControlDeviceInput {
    device_id: String,
    params: Value,
}

#[derive(Deserialize, Default)]
struct DeviceListResponse {
    #[serde(default)]
    devices: Vec<Value>,
    #[serde(default)]
    ok: Option<bool>,
    #[serde(default)]
    error: Option<String>,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let tool_name = args.get(1).map(|s| s.as_str()).unwrap_or("unknown");

    let mut buf = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut buf) {
        fail(&format!("Failed to read stdin: {e}"));
    }

    match tool_name {
        "smart_home_list_devices" => handle_list_devices(&buf),
        "smart_home_control_device" => handle_control_device(&buf),
        _ => fail(&format!(
            "Unknown tool '{tool_name}'. Expected: smart_home_list_devices, smart_home_control_device"
        )),
    }
}

fn fail(msg: &str) -> ! {
    println!("{}", json!({"output": msg, "success": false}));
    std::process::exit(1);
}

fn http_client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(6))
        .connect_timeout(Duration::from_secs(3))
        .build()
        .expect("failed to build HTTP client")
}

fn home_dir() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| std::env::var("USERPROFILE").ok().map(PathBuf::from))
}

/// Reads `$OCTOS_HOME/profiles/$OCTOS_PROFILE_ID.json` directly (same
/// convention as the `account-manager` skill) and resolves the profile's
/// smart-home bridge config from it.
fn resolve_bridge() -> Result<BridgeConfig, String> {
    let octos_home = match std::env::var("OCTOS_HOME") {
        Ok(v) if !v.is_empty() => PathBuf::from(v),
        _ => match home_dir() {
            Some(h) => h.join(".octos"),
            None => {
                return Err("OCTOS_HOME is not set and cannot determine home directory".to_string())
            }
        },
    };

    let profile_id = match std::env::var("OCTOS_PROFILE_ID") {
        Ok(v) if !v.is_empty() => v,
        _ => {
            return Err(
                "OCTOS_PROFILE_ID is not set — this tool must be run from a gateway".to_string(),
            )
        }
    };

    let profile_path = octos_home
        .join("profiles")
        .join(format!("{profile_id}.json"));
    let content = std::fs::read_to_string(&profile_path)
        .map_err(|e| format!("cannot read profile '{profile_id}': {e}"))?;
    let profile: UserProfile = serde_json::from_str(&content)
        .map_err(|e| format!("cannot parse profile '{profile_id}': {e}"))?;

    resolve_bridge_config(
        &profile.config.smart_home.unwrap_or_default(),
        &profile.config.env_vars,
    )
    .ok_or_else(|| {
        "smart-home bridge is not configured for this profile. Configure it in Settings first."
            .to_string()
    })
}

/// Mirrors `octos_cli::api::smart_home_bridge::resolve_bridge_config`
/// exactly: an explicit `token` wins; otherwise `token_env` is looked up in
/// the profile's OWN `config.env_vars` map (never the OS process
/// environment).
fn resolve_bridge_config(
    smart_home: &SmartHomeConfig,
    env_vars: &HashMap<String, String>,
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

fn base(config: &BridgeConfig) -> String {
    config.base_url.trim_end_matches('/').to_string()
}

fn apply_auth(
    builder: reqwest::blocking::RequestBuilder,
    config: &BridgeConfig,
) -> reqwest::blocking::RequestBuilder {
    match &config.token {
        Some(token) if !token.is_empty() => builder.bearer_auth(token),
        _ => builder,
    }
}

/// Percent-encode a path segment (RFC 3986 unreserved set passes through
/// unchanged).
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

fn json_value_to_form_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

fn handle_list_devices(input_json: &str) {
    let input: ListDevicesInput = if input_json.trim().is_empty() {
        ListDevicesInput { room: None }
    } else {
        match serde_json::from_str(input_json) {
            Ok(v) => v,
            Err(e) => fail(&format!("Invalid input: {e}")),
        }
    };

    let bridge = match resolve_bridge() {
        Ok(b) => b,
        Err(msg) => fail(&msg),
    };

    let client = http_client();
    let url = format!("{}/devices", base(&bridge));
    let resp = match apply_auth(client.get(&url), &bridge).send() {
        Ok(r) => r,
        Err(e) => fail(&format!("Bridge request failed: {e}")),
    };
    let status = resp.status();
    let data: DeviceListResponse = match resp.json() {
        Ok(v) => v,
        Err(e) => fail(&format!("Invalid bridge response: {e}")),
    };
    if !status.is_success() {
        fail(&data.error.unwrap_or_else(|| format!("HTTP {status}")));
    }
    if data.ok == Some(false) {
        fail(&data.error.unwrap_or_else(|| "bridge error".to_string()));
    }

    let room_filter = input.room.as_ref().map(|r| r.to_lowercase());
    let devices: Vec<&Value> = data
        .devices
        .iter()
        .filter(|d| match &room_filter {
            None => true,
            Some(room) => d
                .get("room")
                .and_then(|v| v.as_str())
                .map(|r| r.to_lowercase() == *room)
                .unwrap_or(false),
        })
        .collect();

    if devices.is_empty() {
        let msg = match &input.room {
            Some(room) => format!("No devices found in room '{room}'."),
            None => "No devices found.".to_string(),
        };
        println!("{}", json!({"output": msg, "success": true}));
        return;
    }

    let lines: Vec<String> = devices.iter().map(|d| format_device(d)).collect();
    println!("{}", json!({"output": lines.join("\n"), "success": true}));
}

fn format_device(device: &Value) -> String {
    let id = device.get("id").and_then(|v| v.as_str()).unwrap_or("?");
    let name = device.get("name").and_then(|v| v.as_str()).unwrap_or("?");
    let kind = device.get("kind").and_then(|v| v.as_str()).unwrap_or("");
    let on = device.get("on").and_then(|v| v.as_bool()).unwrap_or(false);
    let state = if on { "on" } else { "off" };

    let mut parts = vec![if kind.is_empty() {
        format!("{name} (id: {id}) — {state}")
    } else {
        format!("{name} (id: {id}, {kind}) — {state}")
    }];

    if let Some(room) = device.get("room").and_then(|v| v.as_str()) {
        parts.push(format!("room: {room}"));
    }
    if device.get("online").and_then(|v| v.as_bool()) == Some(false) {
        parts.push("OFFLINE".to_string());
    }
    for key in [
        "brightness",
        "volume",
        "temperature",
        "humidity",
        "position",
        "speed",
    ] {
        if let Some(n) = device.get(key).and_then(|v| v.as_f64()) {
            parts.push(format!("{key}: {n}"));
        }
    }
    if let Some(color) = device.get("color").and_then(|v| v.as_str()) {
        parts.push(format!("color: {color}"));
    }
    if let Some(mode) = device.get("mode").and_then(|v| v.as_str()) {
        parts.push(format!("mode: {mode}"));
    }
    if device.get("muted").and_then(|v| v.as_bool()) == Some(true) {
        parts.push("muted".to_string());
    }

    parts.join(" | ")
}

fn handle_control_device(input_json: &str) {
    let input: ControlDeviceInput = match serde_json::from_str(input_json) {
        Ok(v) => v,
        Err(e) => fail(&format!("Invalid input: {e}")),
    };

    if input.device_id.trim().is_empty() {
        fail("'device_id' must not be empty");
    }
    let params: Map<String, Value> = match input.params {
        Value::Object(map) if !map.is_empty() => map,
        Value::Object(_) => fail("'params' must not be empty, e.g. {\"on\": true}"),
        _ => fail("'params' must be a JSON object, e.g. {\"on\": true}"),
    };

    let bridge = match resolve_bridge() {
        Ok(b) => b,
        Err(msg) => fail(&msg),
    };

    let client = http_client();
    let url = format!(
        "{}/devices/{}",
        base(&bridge),
        encode_path_segment(&input.device_id)
    );
    let form: Vec<(String, String)> = params
        .iter()
        .map(|(k, v)| (k.clone(), json_value_to_form_string(v)))
        .collect();

    let resp = match apply_auth(client.post(&url), &bridge).form(&form).send() {
        Ok(r) => r,
        Err(e) => fail(&format!("Bridge request failed: {e}")),
    };
    let status = resp.status();
    if !status.is_success() {
        let error = resp
            .json::<Value>()
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(String::from))
            .unwrap_or_else(|| format!("HTTP {status}"));
        fail(&error);
    }

    let params_display = params
        .iter()
        .map(|(k, v)| format!("{k}={}", json_value_to_form_string(v)))
        .collect::<Vec<_>>()
        .join(", ");
    println!(
        "{}",
        json!({
            "output": format!("Sent to {}: {params_display}", input.device_id),
            "success": true
        })
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_bridge_config_returns_none_when_bridge_url_absent() {
        let smart_home = SmartHomeConfig::default();
        let env_vars = HashMap::new();
        assert!(resolve_bridge_config(&smart_home, &env_vars).is_none());
    }

    #[test]
    fn resolve_bridge_config_prefers_explicit_token_over_token_env() {
        let smart_home = SmartHomeConfig {
            bridge_url: Some("http://localhost:8787".to_string()),
            token: Some("explicit-token".to_string()),
            token_env: Some("SH_TOKEN".to_string()),
        };
        let mut env_vars = HashMap::new();
        env_vars.insert("SH_TOKEN".to_string(), "env-token".to_string());
        let resolved = resolve_bridge_config(&smart_home, &env_vars).unwrap();
        assert_eq!(resolved.token.as_deref(), Some("explicit-token"));
    }

    #[test]
    fn resolve_bridge_config_falls_back_to_token_env_from_profile_env_vars() {
        let smart_home = SmartHomeConfig {
            bridge_url: Some("http://localhost:8787".to_string()),
            token: None,
            token_env: Some("SH_TOKEN".to_string()),
        };
        let mut env_vars = HashMap::new();
        env_vars.insert("SH_TOKEN".to_string(), "resolved-token".to_string());
        let resolved = resolve_bridge_config(&smart_home, &env_vars).unwrap();
        assert_eq!(resolved.base_url, "http://localhost:8787");
        assert_eq!(resolved.token.as_deref(), Some("resolved-token"));
    }

    #[test]
    fn resolve_bridge_config_none_token_when_token_env_name_not_in_profile_env_vars() {
        // Proves the OS process environment is never consulted: even though
        // token_env names "SH_TOKEN", only the PROFILE's own env_vars map is
        // checked (std::env::var is never called for this lookup), so an
        // empty profile env_vars map must resolve to no token.
        let smart_home = SmartHomeConfig {
            bridge_url: Some("http://localhost:8787".to_string()),
            token: None,
            token_env: Some("SH_TOKEN".to_string()),
        };
        let env_vars = HashMap::new();
        let resolved = resolve_bridge_config(&smart_home, &env_vars).unwrap();
        assert_eq!(resolved.token, None);
    }

    #[test]
    fn json_value_to_form_string_passes_strings_through_unchanged() {
        assert_eq!(json_value_to_form_string(&json!("hello")), "hello");
    }

    #[test]
    fn json_value_to_form_string_stringifies_bools_and_numbers() {
        assert_eq!(json_value_to_form_string(&json!(true)), "true");
        assert_eq!(json_value_to_form_string(&json!(50)), "50");
        assert_eq!(json_value_to_form_string(&json!(21.5)), "21.5");
    }

    #[test]
    fn encode_path_segment_percent_encodes_reserved_bytes() {
        assert_eq!(encode_path_segment("a b"), "a%20b");
        assert_eq!(
            encode_path_segment("light.living_room-1"),
            "light.living_room-1"
        );
    }

    #[test]
    fn format_device_includes_name_id_kind_room_and_state() {
        let device = json!({
            "id": "tv1",
            "name": "Living Room TV",
            "kind": "tv",
            "on": true,
            "room": "Living Room"
        });
        let line = format_device(&device);
        assert!(line.contains("Living Room TV"));
        assert!(line.contains("tv1"));
        assert!(line.contains("tv"));
        assert!(line.contains("on"));
        assert!(line.contains("Living Room"));
    }

    #[test]
    fn format_device_shows_off_state_when_on_is_false() {
        let device = json!({"id": "l1", "name": "Lamp", "on": false});
        let line = format_device(&device);
        assert!(line.contains("— off"));
    }

    #[test]
    fn format_device_flags_offline_devices() {
        let device = json!({"id": "d1", "name": "Sensor", "on": false, "online": false});
        let line = format_device(&device);
        assert!(line.contains("OFFLINE"));
    }

    #[test]
    fn format_device_includes_present_numeric_fields() {
        let device = json!({"id": "l1", "name": "Lamp", "on": true, "brightness": 60});
        let line = format_device(&device);
        assert!(line.contains("brightness: 60"));
    }

    #[test]
    fn format_device_omits_absent_optional_fields() {
        let device = json!({"id": "l1", "name": "Lamp", "on": true});
        let line = format_device(&device);
        assert!(!line.contains("brightness"));
        assert!(!line.contains("OFFLINE"));
        assert!(!line.contains("muted"));
    }
}
