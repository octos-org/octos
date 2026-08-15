//! End-to-end tests for the `smart_home` skill BINARY: spawn the real
//! executable exactly the way the plugin loader does (`./smart_home
//! <tool_name>` with JSON on stdin, JSON on stdout), pointed at a real
//! TCP-bound mock bridge.
//!
//! This is the coverage layer the unit tests in `src/main.rs` can't provide:
//! the full argv/stdin/stdout tool contract, the env-first vs profile-JSON
//! config precedence, and the actual HTTP request the binary puts on the
//! wire (method, path, Authorization header, form body).
//!
//! The mock bridge is a hand-rolled single-request HTTP/1.1 server on a
//! `std::net::TcpListener` — no new dependencies, and the skill crate stays
//! outside the workspace dependency graph like the other app-skills.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};

/// One captured HTTP request: request line, headers, body.
struct CapturedRequest {
    request_line: String,
    headers: Vec<String>,
    body: String,
}

impl CapturedRequest {
    fn header(&self, name: &str) -> Option<&str> {
        let prefix = format!("{}:", name.to_ascii_lowercase());
        self.headers.iter().find_map(|h| {
            let lower = h.to_ascii_lowercase();
            lower
                .starts_with(&prefix)
                .then(|| h.split_once(':').map(|(_, v)| v.trim()))
                .flatten()
        })
    }
}

/// Serve exactly one HTTP request on `listener`, respond with `status_line` +
/// `body_json`, and hand back what the client sent.
fn serve_one(
    listener: TcpListener,
    status_line: &'static str,
    body_json: String,
) -> CapturedRequest {
    let (mut stream, _) = listener.accept().expect("bridge accept");
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    // Read until end of headers, then honor Content-Length.
    let header_end = loop {
        let n = stream.read(&mut chunk).expect("bridge read");
        assert!(n > 0, "client closed before sending a full request");
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default().to_string();
    let headers: Vec<String> = lines
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    let content_length: usize = headers
        .iter()
        .find_map(|h| {
            h.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(|v| v.trim().parse().expect("content-length"))
        })
        .unwrap_or(0);
    while buf.len() < header_end + content_length {
        let n = stream.read(&mut chunk).expect("bridge read body");
        assert!(n > 0, "client closed mid-body");
        buf.extend_from_slice(&chunk[..n]);
    }
    let body = String::from_utf8_lossy(&buf[header_end..header_end + content_length]).to_string();

    let response = format!(
        "{status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body_json}",
        body_json.len()
    );
    stream.write_all(response.as_bytes()).expect("bridge write");

    CapturedRequest {
        request_line,
        headers,
        body,
    }
}

/// Spawn the real skill binary with a scrubbed smart-home env plus `env`,
/// feed it `input` on stdin, and return (stdout-JSON, success flag).
fn run_skill(tool: &str, input: &str, env: &[(&str, String)]) -> (serde_json::Value, bool) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_smart_home"));
    cmd.arg(tool)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Start from a clean smart-home config surface so ambient
        // OCTOS_*/SMART_HOME_* vars on the test machine can't leak in.
        .env_remove("SMART_HOME_BRIDGE_URL")
        .env_remove("SMART_HOME_BRIDGE_TOKEN")
        .env_remove("OCTOS_HOME")
        .env_remove("OCTOS_PROFILE_ID");
    for (key, value) in env {
        cmd.env(key, value);
    }
    let mut child = cmd.spawn().expect("spawn skill binary");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write stdin");
    let output = child.wait_with_output().expect("wait for skill binary");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("skill stdout is not JSON ({e}): {stdout}"));
    let success = parsed["success"].as_bool().expect("success field");
    (parsed, success)
}

fn devices_body() -> String {
    serde_json::json!({
        "ok": true,
        "devices": [
            {"id": "light.lamp", "name": "Desk Lamp", "kind": "light", "on": true,
             "room": "Study", "brightness": 60.0},
            {"id": "tv.den", "name": "Den TV", "kind": "tv", "on": false, "room": "Den"}
        ]
    })
    .to_string()
}

#[test]
fn should_list_devices_via_env_config_with_bearer_auth() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let server = std::thread::spawn(move || serve_one(listener, "HTTP/1.1 200 OK", devices_body()));

    let (parsed, success) = run_skill(
        "smart_home_list_devices",
        "{}",
        &[
            ("SMART_HOME_BRIDGE_URL", format!("http://{addr}")),
            ("SMART_HOME_BRIDGE_TOKEN", "e2e-secret".to_string()),
        ],
    );
    let request = server.join().expect("bridge thread");

    assert!(success, "skill failed: {parsed}");
    assert_eq!(request.request_line, "GET /devices HTTP/1.1");
    assert_eq!(request.header("authorization"), Some("Bearer e2e-secret"));
    let output = parsed["output"].as_str().expect("output");
    assert!(output.contains("Desk Lamp"), "missing device: {output}");
    assert!(output.contains("light.lamp"), "missing id: {output}");
    assert!(output.contains("brightness: 60"), "missing field: {output}");
}

#[test]
fn should_filter_listed_devices_by_room_case_insensitively() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let server = std::thread::spawn(move || serve_one(listener, "HTTP/1.1 200 OK", devices_body()));

    let (parsed, success) = run_skill(
        "smart_home_list_devices",
        r#"{"room": "study"}"#,
        &[("SMART_HOME_BRIDGE_URL", format!("http://{addr}"))],
    );
    server.join().expect("bridge thread");

    assert!(success, "skill failed: {parsed}");
    let output = parsed["output"].as_str().expect("output");
    assert!(
        output.contains("Desk Lamp"),
        "room filter dropped match: {output}"
    );
    assert!(
        !output.contains("Den TV"),
        "room filter kept non-match: {output}"
    );
}

#[test]
fn should_control_device_via_env_config_posting_form_body() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let server = std::thread::spawn(move || {
        serve_one(listener, "HTTP/1.1 200 OK", r#"{"ok": true}"#.to_string())
    });

    let (parsed, success) = run_skill(
        "smart_home_control_device",
        r#"{"device_id": "light.lamp", "params": {"on": true, "brightness": 40}}"#,
        &[
            ("SMART_HOME_BRIDGE_URL", format!("http://{addr}")),
            ("SMART_HOME_BRIDGE_TOKEN", "e2e-secret".to_string()),
        ],
    );
    let request = server.join().expect("bridge thread");

    assert!(success, "skill failed: {parsed}");
    assert_eq!(request.request_line, "POST /devices/light.lamp HTTP/1.1");
    assert_eq!(request.header("authorization"), Some("Bearer e2e-secret"));
    assert_eq!(
        request.header("content-type"),
        Some("application/x-www-form-urlencoded")
    );
    assert!(
        request.body.contains("on=true"),
        "form body: {}",
        request.body
    );
    assert!(
        request.body.contains("brightness=40"),
        "form body: {}",
        request.body
    );
    let output = parsed["output"].as_str().expect("output");
    assert!(output.contains("light.lamp"), "output: {output}");
}

#[test]
fn should_fall_back_to_profile_json_when_env_config_absent() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let server = std::thread::spawn(move || serve_one(listener, "HTTP/1.1 200 OK", devices_body()));

    let octos_home = tempfile::tempdir().expect("tempdir");
    let profiles_dir = octos_home.path().join("profiles");
    std::fs::create_dir_all(&profiles_dir).expect("mkdir profiles");
    std::fs::write(
        profiles_dir.join("e2e-user.json"),
        serde_json::json!({
            "id": "e2e-user",
            "name": "E2E",
            "config": {
                "smart_home": {
                    "bridge_url": format!("http://{addr}"),
                    "token_env": "SH_TOKEN"
                },
                "env_vars": {"SH_TOKEN": "profile-secret"}
            }
        })
        .to_string(),
    )
    .expect("write profile");

    let (parsed, success) = run_skill(
        "smart_home_list_devices",
        "{}",
        &[
            (
                "OCTOS_HOME",
                octos_home.path().to_string_lossy().to_string(),
            ),
            ("OCTOS_PROFILE_ID", "e2e-user".to_string()),
        ],
    );
    let request = server.join().expect("bridge thread");

    assert!(success, "skill failed: {parsed}");
    assert_eq!(request.request_line, "GET /devices HTTP/1.1");
    assert_eq!(
        request.header("authorization"),
        Some("Bearer profile-secret")
    );
}

#[test]
fn should_prefer_env_config_over_profile_json_when_both_present() {
    // Env points at a live mock bridge; the profile points at a port that
    // refuses connections. Success proves env won.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let server = std::thread::spawn(move || serve_one(listener, "HTTP/1.1 200 OK", devices_body()));

    let octos_home = tempfile::tempdir().expect("tempdir");
    let profiles_dir = octos_home.path().join("profiles");
    std::fs::create_dir_all(&profiles_dir).expect("mkdir profiles");
    std::fs::write(
        profiles_dir.join("e2e-user.json"),
        serde_json::json!({
            "id": "e2e-user",
            "name": "E2E",
            "config": {"smart_home": {"bridge_url": "http://127.0.0.1:1"}}
        })
        .to_string(),
    )
    .expect("write profile");

    let (parsed, success) = run_skill(
        "smart_home_list_devices",
        "{}",
        &[
            ("SMART_HOME_BRIDGE_URL", format!("http://{addr}")),
            (
                "OCTOS_HOME",
                octos_home.path().to_string_lossy().to_string(),
            ),
            ("OCTOS_PROFILE_ID", "e2e-user".to_string()),
        ],
    );
    server.join().expect("bridge thread");

    assert!(success, "env config did not take precedence: {parsed}");
}

#[test]
fn should_report_clear_error_when_no_bridge_configured_anywhere() {
    let octos_home = tempfile::tempdir().expect("tempdir");
    let profiles_dir = octos_home.path().join("profiles");
    std::fs::create_dir_all(&profiles_dir).expect("mkdir profiles");
    std::fs::write(
        profiles_dir.join("e2e-user.json"),
        serde_json::json!({"id": "e2e-user", "name": "E2E", "config": {}}).to_string(),
    )
    .expect("write profile");

    let (parsed, success) = run_skill(
        "smart_home_list_devices",
        "{}",
        &[
            (
                "OCTOS_HOME",
                octos_home.path().to_string_lossy().to_string(),
            ),
            ("OCTOS_PROFILE_ID", "e2e-user".to_string()),
        ],
    );

    assert!(!success);
    let output = parsed["output"].as_str().expect("output");
    assert!(
        output.contains("not configured"),
        "error should name the missing config: {output}"
    );
}

#[test]
fn should_surface_bridge_reported_error_body() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let server = std::thread::spawn(move || {
        serve_one(
            listener,
            "HTTP/1.1 502 Bad Gateway",
            r#"{"ok": false, "error": "hub unreachable", "devices": []}"#.to_string(),
        )
    });

    let (parsed, success) = run_skill(
        "smart_home_list_devices",
        "{}",
        &[("SMART_HOME_BRIDGE_URL", format!("http://{addr}"))],
    );
    server.join().expect("bridge thread");

    assert!(!success);
    let output = parsed["output"].as_str().expect("output");
    assert!(output.contains("hub unreachable"), "output: {output}");
}
