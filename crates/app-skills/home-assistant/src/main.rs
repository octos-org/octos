//! Home Assistant skill: read and control a Home Assistant smart home via its
//! REST API (https://developers.home-assistant.io/docs/api/rest/).
//!
//! Protocol: `./ha_bridge <tool_name>` with JSON on stdin, JSON on stdout.
//!
//! Configuration comes from two environment variables:
//!   HA_URL   — base URL incl. scheme/host/port (no trailing `/api`)
//!   HA_TOKEN — long-lived access token (sent as `Authorization: Bearer ...`)

use std::collections::BTreeMap;
use std::io::Read;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Map, Value};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let tool_name = args.get(1).map(|s| s.as_str()).unwrap_or("unknown");

    let mut buf = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut buf) {
        fail(&format!("Failed to read stdin: {e}"));
    }

    match tool_name {
        "ha_get_states" => handle_get_states(&buf),
        "ha_call_service" => handle_call_service(&buf),
        "ha_list_entities" => handle_list_entities(&buf),
        _ => fail(&format!(
            "Unknown tool '{tool_name}'. Expected: ha_get_states, ha_call_service, ha_list_entities"
        )),
    }
}

fn fail(msg: &str) -> ! {
    println!("{}", json!({"output": msg, "success": false}));
    std::process::exit(1);
}

fn ok(output: String) {
    println!("{}", json!({"output": output, "success": true}));
}

/// Read and validate the HA_URL / HA_TOKEN env vars.
/// Returns (base_url_without_trailing_slash, token).
fn read_config() -> Result<(String, String), String> {
    let url = std::env::var("HA_URL")
        .ok()
        .filter(|s| !s.trim().is_empty());
    let token = std::env::var("HA_TOKEN")
        .ok()
        .filter(|s| !s.trim().is_empty());
    match (url, token) {
        (Some(u), Some(t)) => Ok((normalize_base_url(&u), t)),
        _ => Err("HA_URL and HA_TOKEN env vars must be set".to_string()),
    }
}

/// Trim whitespace and any trailing slashes from the configured base URL.
fn normalize_base_url(url: &str) -> String {
    url.trim().trim_end_matches('/').to_string()
}

fn http_client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(15))
        .connect_timeout(Duration::from_secs(5))
        .build()
        .expect("failed to build HTTP client")
}

/// Perform a GET against `<base>/api/<path>` and return the parsed JSON body.
fn ha_get(
    client: &reqwest::blocking::Client,
    base: &str,
    token: &str,
    path: &str,
) -> Result<Value, String> {
    let url = format!("{base}/api/{path}");
    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .map_err(|e| format!("Request to {url} failed: {e}"))?;
    let status = resp.status();
    let text = resp.text().unwrap_or_default();
    if !status.is_success() {
        return Err(http_error(status.as_u16(), &text));
    }
    serde_json::from_str(&text).map_err(|e| format!("Failed to parse response from {url}: {e}"))
}

/// Perform a POST against `<base>/api/<path>` with a JSON body.
fn ha_post(
    client: &reqwest::blocking::Client,
    base: &str,
    token: &str,
    path: &str,
    body: &Value,
) -> Result<Value, String> {
    let url = format!("{base}/api/{path}");
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .json(body)
        .send()
        .map_err(|e| format!("Request to {url} failed: {e}"))?;
    let status = resp.status();
    let text = resp.text().unwrap_or_default();
    if !status.is_success() {
        return Err(http_error(status.as_u16(), &text));
    }
    if text.trim().is_empty() {
        return Ok(json!([]));
    }
    serde_json::from_str(&text).map_err(|e| format!("Failed to parse response from {url}: {e}"))
}

/// Map an HTTP error status to a friendly message, including the body text.
fn http_error(status: u16, body: &str) -> String {
    let hint = match status {
        401 => "invalid or missing HA_TOKEN",
        403 => "forbidden",
        404 => "unknown entity or endpoint",
        400 => "bad request body",
        405 => "method not allowed",
        _ => "request failed",
    };
    let body = body.trim();
    if body.is_empty() {
        format!("Home Assistant returned HTTP {status} ({hint})")
    } else {
        format!("Home Assistant returned HTTP {status} ({hint}): {body}")
    }
}

// ----- entity-state parsing & formatting --------------------------------------

#[derive(Deserialize)]
struct State {
    entity_id: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    attributes: Map<String, Value>,
}

impl State {
    fn friendly_name(&self) -> Option<&str> {
        self.attributes
            .get("friendly_name")
            .and_then(|v| v.as_str())
    }

    fn unit(&self) -> Option<&str> {
        self.attributes
            .get("unit_of_measurement")
            .and_then(|v| v.as_str())
    }

    fn domain(&self) -> &str {
        self.entity_id
            .split_once('.')
            .map(|(d, _)| d)
            .unwrap_or("unknown")
    }

    /// Single-line human-readable summary: "friendly (entity_id): state [unit]".
    fn summary_line(&self) -> String {
        let unit = self.unit().map(|u| format!(" {u}")).unwrap_or_default();
        match self.friendly_name() {
            Some(name) if name != self.entity_id => {
                format!("{name} ({}): {}{unit}", self.entity_id, self.state)
            }
            _ => format!("{}: {}{unit}", self.entity_id, self.state),
        }
    }

    /// True if the free-text needle matches entity_id or friendly_name (case-insensitive).
    fn matches(&self, needle_lc: &str) -> bool {
        self.entity_id.to_lowercase().contains(needle_lc)
            || self
                .friendly_name()
                .map(|n| n.to_lowercase().contains(needle_lc))
                .unwrap_or(false)
    }
}

const MAX_LINES: usize = 100;

// ----- ha_get_states ----------------------------------------------------------

#[derive(Deserialize, Default)]
struct GetStatesInput {
    #[serde(default)]
    entity_id: Option<String>,
}

/// Decide whether `s` looks like an exact entity_id (`<domain>.<object>`).
fn looks_like_entity_id(s: &str) -> bool {
    match s.split_once('.') {
        Some((domain, object)) => {
            !domain.is_empty()
                && !object.is_empty()
                && !s.contains(' ')
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
        }
        None => false,
    }
}

/// Build the human-readable output for a filtered/listed set of states.
fn render_states(states: &[State], filter: Option<&str>) -> String {
    let filter_lc = filter.map(|f| f.to_lowercase());
    let mut matched: Vec<&State> = match &filter_lc {
        Some(needle) => states.iter().filter(|s| s.matches(needle)).collect(),
        None => states.iter().collect(),
    };
    matched.sort_by(|a, b| a.entity_id.cmp(&b.entity_id));

    if matched.is_empty() {
        return match filter {
            Some(f) => format!("No entities match '{f}'."),
            None => "No entities found.".to_string(),
        };
    }

    let total = matched.len();
    let mut lines: Vec<String> = matched
        .iter()
        .take(MAX_LINES)
        .map(|s| s.summary_line())
        .collect();
    if total > MAX_LINES {
        lines.push(format!(
            "... and {} more (refine the filter to narrow down)",
            total - MAX_LINES
        ));
    }
    lines.join("\n")
}

fn handle_get_states(input_json: &str) {
    let input: GetStatesInput = parse_input(input_json);
    let (base, token) = match read_config() {
        Ok(v) => v,
        Err(e) => fail(&e),
    };
    let client = http_client();

    let arg = input
        .entity_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    // Exact-id fast path.
    if let Some(id) = arg {
        if looks_like_entity_id(id) {
            match ha_get(&client, &base, &token, &format!("states/{id}")) {
                Ok(v) => {
                    if let Ok(state) = serde_json::from_value::<State>(v) {
                        ok(state.summary_line());
                        return;
                    }
                    // Fall through to substring filtering if it wasn't a single state.
                }
                Err(e) => {
                    // Unknown exact id -> fall back to substring filtering rather than hard-fail.
                    if !e.contains("HTTP 404") {
                        fail(&e);
                    }
                }
            }
        }
    }

    let value = match ha_get(&client, &base, &token, "states") {
        Ok(v) => v,
        Err(e) => fail(&e),
    };
    let states: Vec<State> = match serde_json::from_value(value) {
        Ok(s) => s,
        Err(e) => fail(&format!("Failed to parse states list: {e}")),
    };
    ok(render_states(&states, arg));
}

// ----- ha_call_service --------------------------------------------------------

#[derive(Deserialize)]
struct CallServiceInput {
    domain: String,
    service: String,
    #[serde(default)]
    entity_id: Option<Value>,
    #[serde(default)]
    data: Option<Map<String, Value>>,
}

/// Build the POST body merging entity_id + the extra `data` params.
fn build_service_body(entity_id: &Option<Value>, data: &Option<Map<String, Value>>) -> Value {
    let mut body = Map::new();
    if let Some(eid) = entity_id {
        if !eid.is_null() {
            body.insert("entity_id".to_string(), eid.clone());
        }
    }
    if let Some(extra) = data {
        for (k, v) in extra {
            body.insert(k.clone(), v.clone());
        }
    }
    Value::Object(body)
}

/// Summarize the array of changed-state objects returned by a service call.
fn render_service_result(value: &Value) -> String {
    let states: Vec<State> = serde_json::from_value(value.clone()).unwrap_or_default();
    if states.is_empty() {
        return "Service called. No state changes reported.".to_string();
    }
    let mut lines = vec![format!(
        "Service called. {} entit{} changed:",
        states.len(),
        if states.len() == 1 { "y" } else { "ies" }
    )];
    for s in states.iter().take(MAX_LINES) {
        lines.push(format!("  {}", s.summary_line()));
    }
    lines.join("\n")
}

fn handle_call_service(input_json: &str) {
    let input: CallServiceInput = match serde_json::from_str(input_json) {
        Ok(v) => v,
        Err(e) => fail(&format!("Invalid input: {e}")),
    };
    let domain = input.domain.trim();
    let service = input.service.trim();
    if domain.is_empty() || service.is_empty() {
        fail("'domain' and 'service' must not be empty");
    }

    let (base, token) = match read_config() {
        Ok(v) => v,
        Err(e) => fail(&e),
    };
    let client = http_client();

    let body = build_service_body(&input.entity_id, &input.data);
    let path = format!("services/{domain}/{service}");
    match ha_post(&client, &base, &token, &path, &body) {
        Ok(v) => ok(render_service_result(&v)),
        Err(e) => fail(&e),
    }
}

// ----- ha_list_entities -------------------------------------------------------

#[derive(Deserialize, Default)]
struct ListEntitiesInput {
    #[serde(default)]
    domain: Option<String>,
}

/// Group states by domain and render a compact per-domain overview.
fn render_entity_list(states: &[State], domain_filter: Option<&str>) -> String {
    let filter_lc = domain_filter.map(|d| d.trim().to_lowercase());

    let mut by_domain: BTreeMap<String, Vec<&State>> = BTreeMap::new();
    for s in states {
        let domain = s.domain().to_string();
        if let Some(f) = &filter_lc {
            if &domain != f {
                continue;
            }
        }
        by_domain.entry(domain).or_default().push(s);
    }

    if by_domain.is_empty() {
        return match domain_filter {
            Some(d) => format!("No entities in domain '{}'.", d.trim()),
            None => "No entities found.".to_string(),
        };
    }

    let mut lines = Vec::new();
    for (domain, mut entities) in by_domain {
        entities.sort_by(|a, b| a.entity_id.cmp(&b.entity_id));
        lines.push(format!("{} ({}):", domain, entities.len()));
        for s in entities {
            lines.push(format!("  {}", s.summary_line()));
        }
    }
    lines.join("\n")
}

fn handle_list_entities(input_json: &str) {
    let input: ListEntitiesInput = parse_input(input_json);
    let (base, token) = match read_config() {
        Ok(v) => v,
        Err(e) => fail(&e),
    };
    let client = http_client();

    let value = match ha_get(&client, &base, &token, "states") {
        Ok(v) => v,
        Err(e) => fail(&e),
    };
    let states: Vec<State> = match serde_json::from_value(value) {
        Ok(s) => s,
        Err(e) => fail(&format!("Failed to parse states list: {e}")),
    };
    let domain = input
        .domain
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    ok(render_entity_list(&states, domain));
}

// ----- helpers ----------------------------------------------------------------

/// Parse an optional-field input struct, tolerating empty/whitespace stdin.
fn parse_input<T: for<'de> Deserialize<'de> + Default>(input_json: &str) -> T {
    if input_json.trim().is_empty() {
        return T::default();
    }
    match serde_json::from_str(input_json) {
        Ok(v) => v,
        Err(e) => fail(&format!("Invalid input: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn states_fixture() -> Vec<State> {
        let raw = json!([
            {"entity_id":"sun.sun","state":"below_horizon",
             "attributes":{"azimuth":336.34,"friendly_name":"Sun"}},
            {"entity_id":"light.kitchen","state":"on",
             "attributes":{"friendly_name":"Kitchen Light","brightness":200}},
            {"entity_id":"light.bedroom","state":"off",
             "attributes":{"friendly_name":"Bedroom Lamp"}},
            {"entity_id":"sensor.outside_temp","state":"18.5",
             "attributes":{"friendly_name":"Outside Temperature","unit_of_measurement":"°C"}}
        ]);
        serde_json::from_value(raw).unwrap()
    }

    #[test]
    fn should_normalize_trailing_slashes_on_base_url() {
        assert_eq!(
            normalize_base_url("https://ha.local:8123/"),
            "https://ha.local:8123"
        );
        assert_eq!(
            normalize_base_url("https://ha.local:8123///"),
            "https://ha.local:8123"
        );
        assert_eq!(normalize_base_url("  http://ha:8123  "), "http://ha:8123");
        assert_eq!(
            normalize_base_url("https://ha.local:8123"),
            "https://ha.local:8123"
        );
    }

    #[test]
    fn should_detect_exact_entity_ids() {
        assert!(looks_like_entity_id("light.kitchen"));
        assert!(looks_like_entity_id("sensor.outside_temp"));
        assert!(!looks_like_entity_id("kitchen"));
        assert!(!looks_like_entity_id("light"));
        assert!(!looks_like_entity_id("kitchen light"));
        assert!(!looks_like_entity_id(".kitchen"));
        assert!(!looks_like_entity_id("light."));
    }

    #[test]
    fn should_extract_domain_and_friendly_name() {
        let states = states_fixture();
        assert_eq!(states[0].domain(), "sun");
        assert_eq!(states[1].domain(), "light");
        assert_eq!(states[1].friendly_name(), Some("Kitchen Light"));
        assert_eq!(states[3].unit(), Some("°C"));
    }

    #[test]
    fn should_format_summary_line_with_unit() {
        let states = states_fixture();
        assert_eq!(
            states[1].summary_line(),
            "Kitchen Light (light.kitchen): on"
        );
        assert_eq!(
            states[3].summary_line(),
            "Outside Temperature (sensor.outside_temp): 18.5 °C"
        );
    }

    #[test]
    fn should_filter_states_by_entity_id_and_friendly_name_case_insensitively() {
        let states = states_fixture();
        // matches entity_id substring
        let out = render_states(&states, Some("light."));
        assert!(out.contains("light.kitchen"));
        assert!(out.contains("light.bedroom"));
        assert!(!out.contains("sun.sun"));
        // matches friendly_name substring, case-insensitive
        let out2 = render_states(&states, Some("LAMP"));
        assert!(out2.contains("light.bedroom"));
        assert!(!out2.contains("light.kitchen"));
    }

    #[test]
    fn should_report_no_match_when_filter_matches_nothing() {
        let states = states_fixture();
        assert_eq!(
            render_states(&states, Some("zzz")),
            "No entities match 'zzz'."
        );
    }

    #[test]
    fn should_build_service_body_merging_entity_id_and_data() {
        let mut data = Map::new();
        data.insert("brightness_pct".to_string(), json!(60));
        data.insert("color_name".to_string(), json!("red"));
        let body = build_service_body(&Some(json!("light.kitchen")), &Some(data));
        assert_eq!(body["entity_id"], json!("light.kitchen"));
        assert_eq!(body["brightness_pct"], json!(60));
        assert_eq!(body["color_name"], json!("red"));
    }

    #[test]
    fn should_build_service_body_with_array_entity_id_and_no_data() {
        let body = build_service_body(&Some(json!(["light.a", "light.b"])), &None);
        assert_eq!(body["entity_id"], json!(["light.a", "light.b"]));
        assert_eq!(body.as_object().unwrap().len(), 1);
    }

    #[test]
    fn should_build_empty_service_body_when_no_target() {
        let body = build_service_body(&None, &None);
        assert_eq!(body, json!({}));
    }

    #[test]
    fn should_summarize_empty_service_response() {
        assert_eq!(
            render_service_result(&json!([])),
            "Service called. No state changes reported."
        );
    }

    #[test]
    fn should_summarize_changed_entities_in_service_response() {
        let resp = json!([
            {"entity_id":"light.kitchen","state":"on",
             "attributes":{"friendly_name":"Kitchen Light"}}
        ]);
        let out = render_service_result(&resp);
        assert!(out.contains("1 entity changed"));
        assert!(out.contains("Kitchen Light (light.kitchen): on"));
    }

    #[test]
    fn should_group_entities_by_domain() {
        let states = states_fixture();
        let out = render_entity_list(&states, None);
        assert!(out.contains("light (2):"));
        assert!(out.contains("sensor (1):"));
        assert!(out.contains("sun (1):"));
        assert!(out.contains("Kitchen Light (light.kitchen): on"));
    }

    #[test]
    fn should_filter_entity_list_to_single_domain() {
        let states = states_fixture();
        let out = render_entity_list(&states, Some("light"));
        assert!(out.contains("light (2):"));
        assert!(!out.contains("sensor"));
        assert!(!out.contains("sun"));
    }

    #[test]
    fn should_report_no_entities_for_unknown_domain() {
        let states = states_fixture();
        assert_eq!(
            render_entity_list(&states, Some("climate")),
            "No entities in domain 'climate'."
        );
    }

    #[test]
    fn should_map_http_errors_with_hints() {
        assert!(http_error(401, "").contains("invalid or missing HA_TOKEN"));
        assert!(http_error(404, "Not found").contains("unknown entity"));
        assert!(http_error(404, "Not found").contains("Not found"));
        assert!(http_error(400, "bad body").contains("bad request body"));
    }
}
