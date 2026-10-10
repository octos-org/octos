//! #2736 — the chat surface arms itself from the same token chain as the
//! admin shell (channel config → OCTOS_AUTH_TOKEN → config.json). These
//! probes drive the real `Channel::start()` server over real TCP with the
//! env fallback pivoted, so they live in their own test binary: the env
//! guard here never races the lib test process.

#![cfg(feature = "api")]

use std::ffi::OsStr;
use std::sync::{Arc, Mutex};

use octos_bus::{ApiChannel, Channel, SessionManager};
use tokio::sync::mpsc;

/// Serializes env pivots across the tests in this binary.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Saves the listed vars on creation, applies the pivot, restores on drop.
/// The lock is held for the guard's lifetime so concurrent tests in this
/// binary queue up instead of racing the process environment.
struct EnvGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
    saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl EnvGuard {
    #[allow(unsafe_code)]
    fn pivot(vars: &[(&'static str, Option<&OsStr>)]) -> Self {
        // Poisoning is recovered: a panicking test's guard still restores
        // the saved vars on unwind, so the next waiter sees clean state.
        let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved = vars
            .iter()
            .map(|(k, _)| (*k, std::env::var_os(k)))
            .collect();
        for (key, value) in vars {
            // SAFETY: callers hold ENV_LOCK for the guard's lifetime.
            unsafe {
                match value {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
        }
        Self { _lock: lock, saved }
    }
}

impl Drop for EnvGuard {
    #[allow(unsafe_code)]
    fn drop(&mut self) {
        for (key, value) in &self.saved {
            // SAFETY: same ENV_LOCK discipline as `pivot`.
            unsafe {
                match value {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}

/// Spawn the real channel server on an OS-assigned port and return its
/// base URL. The server task dies with the test's tokio runtime, which
/// also releases the listener. Mirrors the bind-probe-drop dance of the
/// metrics route test.
async fn spawn_server(data_dir: &std::path::Path, channel_token: Option<&str>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let sessions = SessionManager::open(data_dir).unwrap();
    let channel = ApiChannel::new(
        port,
        channel_token.map(str::to_string),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        Arc::new(tokio::sync::Mutex::new(sessions)),
        None,
    );
    let (inbound_tx, _inbound_rx) = mpsc::channel(1);
    let _server = tokio::spawn(async move { channel.start(inbound_tx).await });
    // The listener is bound inside start(); give it a beat before probing.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    format!("http://127.0.0.1:{port}")
}

async fn post_chat(base: &str, auth: Option<&str>) -> (reqwest::StatusCode, String) {
    let client = reqwest::Client::new();
    let mut request = client
        .post(format!("{base}/chat"))
        .header("content-type", "application/json")
        .body(r#"{"message":"hi"}"#);
    if let Some(token) = auth {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let response = request.send().await.unwrap();
    let status = response.status();
    let body = response.text().await.unwrap();
    (status, body)
}

async fn stream_status(base: &str, auth: Option<&str>) -> reqwest::StatusCode {
    let client = reqwest::Client::new();
    let mut request = client.get(format!("{base}/sessions/web-e2e/events/stream"));
    if let Some(token) = auth {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    request.send().await.unwrap().status()
}

/// One probe against the real server: `(method, path, content-type, body)`.
async fn probe(
    base: &str,
    method: &str,
    path: &str,
    content_type: &str,
    body: &str,
    auth: Option<&str>,
) -> reqwest::StatusCode {
    let client = reqwest::Client::new();
    let method: reqwest::Method = method.parse().unwrap();
    let mut request = client.request(method, format!("{base}{path}"));
    if !content_type.is_empty() {
        request = request.header("content-type", content_type);
    }
    if !body.is_empty() {
        request = request.body(body.to_string());
    }
    if let Some(token) = auth {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    request.send().await.unwrap().status()
}

/// Every route on the session surface except /chat, the SSE stream, and
/// the admin shell (those three carry their own e2e probes). One row per
/// route of the real router table.
const SESSION_SURFACE: &[(&str, &str, &str, &str)] = &[
    ("GET", "/metrics", "", ""),
    ("GET", "/sessions", "", ""),
    ("GET", "/sessions/web-e2e/messages", "", ""),
    ("GET", "/sessions/web-e2e/status", "", ""),
    ("GET", "/sessions/web-e2e/tasks", "", ""),
    ("DELETE", "/sessions/web-e2e", "", ""),
    (
        "PATCH",
        "/sessions/web-e2e/title",
        "application/json",
        r#"{"title":"t"}"#,
    ),
    ("POST", "/tasks/t1/cancel", "", ""),
    ("POST", "/tasks/t1/restart-from-node", "", ""),
    ("GET", "/files/attachment.txt", "", ""),
    (
        "POST",
        "/upload",
        "multipart/form-data; boundary=octose2eprobe",
        "",
    ),
];

fn isolated_data_dir(tmp: &std::path::Path) -> std::path::PathBuf {
    let data_dir = tmp.join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    data_dir
}

/// The issue scenario: a deployment armed via OCTOS_AUTH_TOKEN with no
/// channel-level token must refuse unauthenticated and wrong-token
/// requests on both gated endpoints, and accept the env token.
#[tokio::test]
async fn env_token_arms_chat_and_stream_endpoints() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = isolated_data_dir(tmp.path());
    let _env = EnvGuard::pivot(&[
        (
            "OCTOS_AUTH_TOKEN",
            Some(OsStr::new("octos-env-secret-token")),
        ),
        ("OCTOS_DATA_DIR", Some(data_dir.as_os_str())),
        ("HOME", Some(tmp.path().as_os_str())),
    ]);

    let base = spawn_server(&data_dir, None).await;

    let expected = "octos-env-secret-token";
    let filler = "x".repeat(expected.len());
    for auth in [None, Some("wrong-token"), Some(filler.as_str())] {
        let (status, body) = post_chat(&base, auth).await;
        assert_eq!(status, reqwest::StatusCode::UNAUTHORIZED, "body: {body}");
        assert_eq!(body, "invalid auth token");
        assert_eq!(
            stream_status(&base, auth).await,
            reqwest::StatusCode::UNAUTHORIZED
        );
    }

    // The correct token passes the gate: the request proceeds to the next
    // validation error (missing thread_id) instead of the auth guard.
    let (status, body) = post_chat(&base, Some(expected)).await;
    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST, "body: {body}");
    assert_eq!(body, "thread_id is required");
    assert_eq!(
        stream_status(&base, Some(expected)).await,
        reqwest::StatusCode::OK
    );
}

/// Same chain, config.json leg: no env token, but the top-level
/// config.json auth_token must arm the chat surface too.
#[tokio::test]
async fn config_json_token_arms_chat_and_stream_endpoints() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = isolated_data_dir(tmp.path());
    std::fs::write(
        data_dir.join("config.json"),
        r#"{"auth_token": "octos-config-secret-token"}"#,
    )
    .unwrap();
    let _env = EnvGuard::pivot(&[
        ("OCTOS_AUTH_TOKEN", None),
        ("OCTOS_DATA_DIR", Some(data_dir.as_os_str())),
        ("HOME", Some(tmp.path().as_os_str())),
    ]);

    let base = spawn_server(&data_dir, None).await;

    let expected = "octos-config-secret-token";
    for auth in [None, Some("wrong-token")] {
        let (status, body) = post_chat(&base, auth).await;
        assert_eq!(status, reqwest::StatusCode::UNAUTHORIZED, "body: {body}");
        assert_eq!(
            stream_status(&base, auth).await,
            reqwest::StatusCode::UNAUTHORIZED
        );
    }
    let (status, body) = post_chat(&base, Some(expected)).await;
    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST, "body: {body}");
    assert_eq!(body, "thread_id is required");
    assert_eq!(
        stream_status(&base, Some(expected)).await,
        reqwest::StatusCode::OK
    );
}

/// Precedence over the wire: when both the channel config and the env
/// carry a token, the channel's own token is the one enforced.
#[tokio::test]
async fn channel_token_wins_over_env_token() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = isolated_data_dir(tmp.path());
    let _env = EnvGuard::pivot(&[
        (
            "OCTOS_AUTH_TOKEN",
            Some(OsStr::new("octos-env-secret-token")),
        ),
        ("OCTOS_DATA_DIR", Some(data_dir.as_os_str())),
        ("HOME", Some(tmp.path().as_os_str())),
    ]);

    let base = spawn_server(&data_dir, Some("octos-channel-secret-token")).await;

    // The env token is not accepted; the channel token is.
    let (status, _) = post_chat(&base, Some("octos-env-secret-token")).await;
    assert_eq!(status, reqwest::StatusCode::UNAUTHORIZED);
    let (status, body) = post_chat(&base, Some("octos-channel-secret-token")).await;
    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST, "body: {body}");
    assert_eq!(body, "thread_id is required");
}

/// An empty channel token arms nothing: it falls through the chain to the
/// env leg instead of arming a compare that would reject every bearer.
#[tokio::test]
async fn empty_channel_token_falls_through_to_env_token() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = isolated_data_dir(tmp.path());
    let _env = EnvGuard::pivot(&[
        (
            "OCTOS_AUTH_TOKEN",
            Some(OsStr::new("octos-env-secret-token")),
        ),
        ("OCTOS_DATA_DIR", Some(data_dir.as_os_str())),
        ("HOME", Some(tmp.path().as_os_str())),
    ]);

    let base = spawn_server(&data_dir, Some("")).await;

    // The env token is the one enforced: accepted on the gate, while the
    // missing bearer is refused.
    let (status, body) = post_chat(&base, Some("octos-env-secret-token")).await;
    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST, "body: {body}");
    assert_eq!(body, "thread_id is required");
    let (status, _) = post_chat(&base, None).await;
    assert_eq!(status, reqwest::StatusCode::UNAUTHORIZED);
}

/// Negative control: with no token configured anywhere the gate stays
/// open — a tokenless dev/test channel must keep working, and this pins
/// that the new resolution never invents a token out of nothing.
#[tokio::test]
async fn unarmed_channel_stays_open() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = isolated_data_dir(tmp.path());
    let _env = EnvGuard::pivot(&[
        ("OCTOS_AUTH_TOKEN", None),
        ("OCTOS_DATA_DIR", Some(data_dir.as_os_str())),
        ("HOME", Some(tmp.path().as_os_str())),
    ]);

    let base = spawn_server(&data_dir, None).await;

    let (status, _) = post_chat(&base, None).await;
    assert_ne!(status, reqwest::StatusCode::UNAUTHORIZED);
    assert_eq!(stream_status(&base, None).await, reqwest::StatusCode::OK);
    // #2751: the rest of the surface shares the property.
    assert_eq!(
        probe(&base, "GET", "/sessions", "", "", None).await,
        reqwest::StatusCode::OK
    );
    assert_eq!(
        probe(&base, "GET", "/metrics", "", "", None).await,
        reqwest::StatusCode::OK
    );
}

/// #2751: with the env token armed, the WHOLE session surface refuses
/// unauthenticated callers over real TCP — not just /chat and the SSE
/// stream — while the correct token passes the gate.
#[tokio::test]
async fn env_token_arms_the_whole_session_surface() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = isolated_data_dir(tmp.path());
    let _env = EnvGuard::pivot(&[
        (
            "OCTOS_AUTH_TOKEN",
            Some(OsStr::new("octos-surface-secret-token")),
        ),
        ("OCTOS_DATA_DIR", Some(data_dir.as_os_str())),
        ("HOME", Some(tmp.path().as_os_str())),
    ]);

    let base = spawn_server(&data_dir, None).await;
    let expected = "octos-surface-secret-token";

    let mut still_open = Vec::new();
    for (method, path, content_type, body) in SESSION_SURFACE {
        if probe(&base, method, path, content_type, body, None).await
            != reqwest::StatusCode::UNAUTHORIZED
        {
            still_open.push(format!("{method} {path}"));
        }
    }
    assert!(
        still_open.is_empty(),
        "surface routes reachable without a token on an armed deployment: {}",
        still_open.join(", ")
    );

    // The correct token passes the gate: exact 200 on the read routes,
    // a clean idempotent delete, and no 401 anywhere else.
    assert_eq!(
        probe(&base, "GET", "/sessions", "", "", Some(expected)).await,
        reqwest::StatusCode::OK
    );
    assert_eq!(
        probe(&base, "GET", "/metrics", "", "", Some(expected)).await,
        reqwest::StatusCode::OK
    );
    assert_eq!(
        probe(&base, "DELETE", "/sessions/web-e2e", "", "", Some(expected)).await,
        reqwest::StatusCode::NO_CONTENT
    );
    for (method, path, content_type, body) in SESSION_SURFACE {
        let status = probe(&base, method, path, content_type, body, Some(expected)).await;
        assert_ne!(
            status,
            reqwest::StatusCode::UNAUTHORIZED,
            "{method} {path} must accept the configured token"
        );
    }
}

// ------------------------------------------------------------------
// #2328 — the loopback guard at the wire: raw HTTP/1.1 on a fresh TCP
// connection to the real listener, exactly the bytes a rebound page
// (Host: evil.com) and a cross-site page (Origin: https://evil.com)
// put on the wire — no client library to rewrite either header.
// ------------------------------------------------------------------

/// One raw request; returns the status code and the response head.
async fn raw_request(port: u16, host: &str, origin: Option<&str>) -> (u16, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let origin_line = origin
        .map(|value| format!("origin: {value}\r\n"))
        .unwrap_or_default();
    let request = format!(
        "GET /metrics HTTP/1.1\r\nhost: {host}\r\n{origin_line}connection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut head = Vec::new();
    stream.read_to_end(&mut head).await.unwrap();
    let head = String::from_utf8_lossy(&head).into_owned();
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    (status, head)
}

#[tokio::test]
async fn the_wire_refuses_a_rebound_host_and_a_cross_site_origin() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = isolated_data_dir(tmp.path());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let sessions = SessionManager::open(&data_dir).unwrap();
    let channel = ApiChannel::new(
        port,
        None,
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        Arc::new(tokio::sync::Mutex::new(sessions)),
        None,
    );
    let (inbound_tx, _inbound_rx) = mpsc::channel(1);
    let _server = tokio::spawn(async move { channel.start(inbound_tx).await });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // The rebinding vector: the browser resolves evil.com to 127.0.0.1,
    // the Host header still names evil.com.
    let (status, head) = raw_request(port, "evil.com", None).await;
    assert_eq!(status, 421, "rebound Host must not reach a route: {head}");

    // The CSRF vector: a cross-site page POSTs to the loopback directly.
    let (status, head) = raw_request(port, "127.0.0.1:1", Some("https://evil.com")).await;
    assert_eq!(status, 403, "cross-site Origin must not reach a route: {head}");

    // What curl and scripts send: the loopback Host, no Origin.
    let (status, head) = raw_request(port, "127.0.0.1:1", None).await;
    assert_eq!(status, 200, "local callers must keep working: {head}");
}
