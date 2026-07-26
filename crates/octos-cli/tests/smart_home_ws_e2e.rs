//! Genuine end-to-end coverage for the `smart_home/*` UI Protocol WS
//! methods (backend half of the smart-home device control/state
//! migration): a real TCP-bound `octos serve` router — NOT
//! `tower::oneshot`, which cannot perform a WS upgrade at all (see
//! `ws_token_url_decode.rs`) — driven by a real `tokio-tungstenite` WS
//! client, backed by a real `wiremock` HTTP server standing in for a
//! self-hosted smart-home bridge.
//!
//! Unlike `smart_home_panel.rs`'s unit tests (which call the
//! `my_smart_home_*` handlers directly as plain async functions) and
//! `ws_token_url_decode.rs` (which only asserts on the HTTP status of a
//! WS upgrade attempt), these tests exercise the FULL real path for
//! every method: WS admin-token auth -> JSON-RPC dispatch ->
//! per-profile bridge-config resolution -> real HTTP call to the bridge
//! -> JSON-RPC response serialized back over the socket.

#![cfg(feature = "api")]

use std::sync::Arc;
use std::time::Duration;

use futures::{Sink, SinkExt, Stream, StreamExt};
use octos_cli::api::{AppState, build_router};
use octos_cli::profiles::{ProfileConfig, ProfileStore, SmartHomeConfig, UserProfile};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const ADMIN_TOKEN: &str = "e2e-smart-home-admin-token";
/// `resolve_my_profile_id` maps `AuthIdentity::Admin` to this fixed
/// profile ID (`auth_handlers::ADMIN_PROFILE_ID`, private to the crate) —
/// pre-saving a profile under this ID lets the test control its
/// `smart_home` config instead of relying on the auto-created default.
const ADMIN_PROFILE_ID: &str = "admin";

fn admin_profile(smart_home: Option<SmartHomeConfig>) -> UserProfile {
    UserProfile {
        id: ADMIN_PROFILE_ID.into(),
        name: ADMIN_PROFILE_ID.into(),
        enabled: true,
        data_dir: None,
        parent_id: None,
        public_subdomain: None,
        config: ProfileConfig {
            smart_home,
            ..ProfileConfig::default()
        },
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    }
}

/// Build an `AppState` whose admin identity (the bootstrap `auth_token`
/// path in `resolve_identity`) resolves to a profile with the given
/// smart-home bridge config already saved.
fn state_with_admin_bridge(dir: &TempDir, smart_home: Option<SmartHomeConfig>) -> Arc<AppState> {
    let store = Arc::new(ProfileStore::open_unified(dir.path()).unwrap());
    store.save(&admin_profile(smart_home)).unwrap();
    Arc::new(AppState {
        profile_store: Some(store),
        auth_token: Some(ADMIN_TOKEN.into()),
        ..AppState::empty_for_tests()
    })
}

async fn spawn_api(state: Arc<AppState>) -> (String, tokio::task::JoinHandle<()>) {
    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app.into_make_service())
            .await
            .unwrap();
    });
    tokio::task::yield_now().await;
    (format!("127.0.0.1:{}", addr.port()), server)
}

async fn connect_ui_protocol(
    addr: &str,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let url =
        format!("ws://{addr}/api/ui-protocol/ws?token={ADMIN_TOKEN}&ui_feature=smart_home.v1");
    let (ws, _response) = connect_async(url).await.unwrap();
    ws
}

/// Send one JSON-RPC request and return the parsed response body.
/// Generic over the WS stream type so every test can share one helper
/// without spelling out `WebSocketStream<MaybeTlsStream<TcpStream>>`.
async fn rpc<S>(ws: &mut S, id: &str, method_name: &str, params: Value) -> Value
where
    S: Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
    S: Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    ws.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method_name,
            "params": params,
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();

    let next = tokio::time::timeout(Duration::from_secs(3), ws.next())
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {method_name} response"));
    let Some(next) = next else {
        panic!("websocket ended before {method_name} response");
    };
    let response = match next {
        Ok(msg) => msg,
        Err(e) => panic!("failed to read {method_name} response: {e}"),
    };
    let Message::Text(body) = response else {
        panic!("expected JSON-RPC text response for {method_name}, got {response:?}");
    };
    serde_json::from_str(&body).unwrap()
}

#[tokio::test]
async fn smart_home_status_get_reports_not_configured_without_bridge() {
    let dir = TempDir::new().unwrap();
    let state = state_with_admin_bridge(&dir, None);
    let (addr, server) = spawn_api(state).await;
    let mut ws = connect_ui_protocol(&addr).await;

    let body = rpc(
        &mut ws,
        "status",
        octos_core::ui_protocol::methods::SMART_HOME_STATUS_GET,
        json!({}),
    )
    .await;
    assert_eq!(body["result"]["configured"], json!(false));

    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn smart_home_status_get_reports_configured_with_bridge() {
    let dir = TempDir::new().unwrap();
    let state = state_with_admin_bridge(
        &dir,
        Some(SmartHomeConfig {
            bridge_url: Some("http://127.0.0.1:1".into()),
            token: None,
            token_env: None,
        }),
    );
    let (addr, server) = spawn_api(state).await;
    let mut ws = connect_ui_protocol(&addr).await;

    let body = rpc(
        &mut ws,
        "status",
        octos_core::ui_protocol::methods::SMART_HOME_STATUS_GET,
        json!({}),
    )
    .await;
    assert_eq!(body["result"]["configured"], json!(true));

    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn smart_home_device_list_round_trips_through_real_bridge() {
    let bridge = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/devices"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "source": "home_assistant",
            "devices": [
                {"id": "real_tv", "name": "Living Room TV", "kind": "tv", "on": true}
            ]
        })))
        .mount(&bridge)
        .await;

    let dir = TempDir::new().unwrap();
    let state = state_with_admin_bridge(
        &dir,
        Some(SmartHomeConfig {
            bridge_url: Some(bridge.uri()),
            token: None,
            token_env: None,
        }),
    );
    let (addr, server) = spawn_api(state).await;
    let mut ws = connect_ui_protocol(&addr).await;

    let body = rpc(
        &mut ws,
        "devices",
        octos_core::ui_protocol::methods::SMART_HOME_DEVICE_LIST,
        json!({}),
    )
    .await;
    assert_eq!(
        body["result"]["devices"]["devices"][0]["id"],
        json!("real_tv"),
        "expected the bridge's device list forwarded byte-for-byte over WS, got {body}"
    );

    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn smart_home_device_list_maps_not_configured_to_json_rpc_error() {
    let dir = TempDir::new().unwrap();
    let state = state_with_admin_bridge(&dir, None);
    let (addr, server) = spawn_api(state).await;
    let mut ws = connect_ui_protocol(&addr).await;

    let body = rpc(
        &mut ws,
        "devices-error",
        octos_core::ui_protocol::methods::SMART_HOME_DEVICE_LIST,
        json!({}),
    )
    .await;
    assert!(
        body["error"].is_object(),
        "expected a JSON-RPC error for an unconfigured bridge, got {body}"
    );
    assert_eq!(body["error"]["data"]["kind"], json!("not_found"));
    assert_eq!(
        body["error"]["data"]["resource_type"],
        json!("smart_home_device")
    );

    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn smart_home_device_command_posts_to_real_bridge() {
    let bridge = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/devices/real_tv"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .mount(&bridge)
        .await;

    let dir = TempDir::new().unwrap();
    let state = state_with_admin_bridge(
        &dir,
        Some(SmartHomeConfig {
            bridge_url: Some(bridge.uri()),
            token: None,
            token_env: None,
        }),
    );
    let (addr, server) = spawn_api(state).await;
    let mut ws = connect_ui_protocol(&addr).await;

    let body = rpc(
        &mut ws,
        "command",
        octos_core::ui_protocol::methods::SMART_HOME_DEVICE_COMMAND,
        json!({ "device_id": "real_tv", "params": { "action": "volume_up" } }),
    )
    .await;
    assert!(
        body["error"].is_null(),
        "expected smart_home/device.command to succeed, got {body}"
    );
    assert_eq!(body["result"], json!({}));

    // Confirm the bridge actually received the real request, proving the
    // WS param round-tripped through profile resolution and the HTTP
    // client rather than the mock accepting any POST.
    let received = bridge.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].method.as_str(), "POST");

    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn smart_home_camera_stream_start_and_stop_round_trip() {
    let bridge = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/cameras/cam1/stream"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "protocol": "rtc",
            "playback_url": "http://127.0.0.1:1984/stream.html?src=cam1"
        })))
        .mount(&bridge)
        .await;
    Mock::given(method("POST"))
        .and(path("/cameras/cam1/stop"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .mount(&bridge)
        .await;

    let dir = TempDir::new().unwrap();
    let state = state_with_admin_bridge(
        &dir,
        Some(SmartHomeConfig {
            bridge_url: Some(bridge.uri()),
            token: None,
            token_env: None,
        }),
    );
    let (addr, server) = spawn_api(state).await;
    let mut ws = connect_ui_protocol(&addr).await;

    let start_body = rpc(
        &mut ws,
        "stream-start",
        octos_core::ui_protocol::methods::SMART_HOME_CAMERA_STREAM_START,
        json!({ "device_id": "cam1", "quality": 2 }),
    )
    .await;
    assert_eq!(start_body["result"]["stream"]["protocol"], json!("rtc"));
    assert_eq!(
        start_body["result"]["stream"]["playback_url"],
        json!("http://127.0.0.1:1984/stream.html?src=cam1")
    );

    let stop_body = rpc(
        &mut ws,
        "stream-stop",
        octos_core::ui_protocol::methods::SMART_HOME_CAMERA_STREAM_STOP,
        json!({ "device_id": "cam1" }),
    )
    .await;
    assert!(
        stop_body["error"].is_null(),
        "expected clean stop, got {stop_body}"
    );

    server.abort();
    let _ = server.await;
}
