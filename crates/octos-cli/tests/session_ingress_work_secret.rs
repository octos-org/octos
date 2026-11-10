//! Issue #296: work-secret session ingress must authenticate a guest
//! WebSocket, bridge normal UI Protocol JSON-RPC frames, and close an
//! already-open socket after revocation.

#![cfg(feature = "api")]

use std::sync::Arc;

use chrono::Duration;
use futures::{SinkExt, StreamExt};
use octos_agent::bridge::work_secret::WorkSecretGrantStore;
use octos_cli::api::{AppState, build_router};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

fn state_with_work_secret_store(dir: &TempDir) -> (Arc<AppState>, Arc<WorkSecretGrantStore>) {
    let store = Arc::new(WorkSecretGrantStore::new(dir.path()));
    let state = Arc::new(AppState {
        work_secret_store: store.clone(),
        ..AppState::empty_for_tests()
    });
    (state, store)
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

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

fn issue_work_secret(store: &Arc<WorkSecretGrantStore>, session_id: &str, token: &str) {
    issue_work_secret_with_ttl(store, session_id, token, Duration::minutes(5));
}

fn issue_work_secret_with_ttl(
    store: &Arc<WorkSecretGrantStore>,
    session_id: &str,
    token: &str,
    ttl: Duration,
) {
    store
        .issue(session_id, token, "http://127.0.0.1:50080", ttl, None)
        .unwrap();
}

async fn connect_work_secret(
    state: Arc<AppState>,
    session_id: &str,
    token: &str,
) -> (tokio::task::JoinHandle<()>, WsStream) {
    let (addr, server) = spawn_api(state).await;
    let url = format!(
        "ws://{addr}/v1/session_ingress/ws/{session_id}?token={token}&ui_feature=auxiliary.rest_to_ws.v1"
    );
    let (ws, _response) = connect_async(url).await.unwrap();
    (server, ws)
}

async fn json_rpc(ws: &mut WsStream, value: Value, context: &str) -> Value {
    ws.send(Message::Text(value.to_string().into()))
        .await
        .unwrap();
    let response = tokio::time::timeout(std::time::Duration::from_secs(3), ws.next())
        .await
        .expect(context)
        .expect(context)
        .expect(context);
    let Message::Text(body) = response else {
        panic!("expected JSON-RPC text response, got {response:?}");
    };
    serde_json::from_str(&body).unwrap()
}

#[tokio::test]
async fn work_secret_ws_round_trip_and_revocation_close() {
    let dir = TempDir::new().unwrap();
    let session_id = "local:work-secret";
    let token = "guest-token";
    let (state, store) = state_with_work_secret_store(&dir);
    issue_work_secret(&store, session_id, token);
    let (server, mut ws) = connect_work_secret(state, session_id, token).await;

    let body = json_rpc(
        &mut ws,
        json!({
            "jsonrpc": "2.0",
            "id": "status-before-revoke",
            "method": "session/status.get",
            "params": { "session_id": session_id }
        }),
        "first session ingress response",
    )
    .await;
    assert_eq!(body["id"], "status-before-revoke");
    assert_eq!(body["result"]["status"]["active"], false);

    assert!(store.revoke_token(token).unwrap());

    ws.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": "status-after-revoke",
            "method": "session/status.get",
            "params": { "session_id": session_id }
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();

    let close = tokio::time::timeout(std::time::Duration::from_secs(3), ws.next())
        .await
        .expect("timed out waiting for revocation close")
        .expect("websocket ended without close frame")
        .expect("failed to read revocation close");
    let Message::Close(Some(frame)) = close else {
        panic!("expected policy-violation close frame, got {close:?}");
    };
    assert_eq!(frame.code, CloseCode::Policy);
    assert_eq!(frame.reason, "session_ingress_revoked");

    server.abort();
    let _ = server.await;
}

/// #1594: raw RPCs are dispatched before `route_rpc_command`, so they never
/// reach the typed session-scope gate. A work-secret must NOT be able to reach
/// the raw autonomy/profile surface (which resolves profile/session targets
/// from request params independently of the credential). The typed surface —
/// which IS scope-checked — must keep working over the same socket.
#[tokio::test]
async fn work_secret_ws_denies_raw_surface_but_allows_typed() {
    let dir = TempDir::new().unwrap();
    let session_id = "local:work-secret";
    let token = "guest-token";
    let (state, store) = state_with_work_secret_store(&dir);
    issue_work_secret(&store, session_id, token);
    let (server, mut ws) = connect_work_secret(state, session_id, token).await;

    // A raw autonomy method — even naming the granted session_id — is refused.
    let body = json_rpc(
        &mut ws,
        json!({
            "jsonrpc": "2.0",
            "id": "raw-goal-set",
            "method": "session/goal/set",
            "params": { "session_id": session_id, "goal": { "text": "escalate" } }
        }),
        "raw-method rejection",
    )
    .await;
    assert_eq!(body["id"], "raw-goal-set");
    assert_eq!(
        body["error"]["code"], -32600,
        "raw method must be rejected with INVALID_REQUEST, got {body}"
    );

    // The typed, scope-checked surface still works on the same socket.
    let body = json_rpc(
        &mut ws,
        json!({
            "jsonrpc": "2.0",
            "id": "typed-status",
            "method": "session/status.get",
            "params": { "session_id": session_id }
        }),
        "typed response",
    )
    .await;
    assert_eq!(body["id"], "typed-status");
    assert!(
        body["result"].is_object(),
        "typed session/status.get must still succeed, got {body}"
    );

    server.abort();
    let _ = server.await;
}

/// #1594 follow-up: the `client_hello` handshake happens before the raw deny
/// gate, so its advertised capabilities must not list the raw/global methods an
/// ingress credential is denied — otherwise advertised ≠ callable.
#[tokio::test]
async fn work_secret_ws_client_hello_advertises_only_callable_methods() {
    let dir = TempDir::new().unwrap();
    let session_id = "local:work-secret";
    let token = "guest-token";
    let (state, store) = state_with_work_secret_store(&dir);
    issue_work_secret(&store, session_id, token);
    let (server, mut ws) = connect_work_secret(state, session_id, token).await;

    let body = json_rpc(
        &mut ws,
        json!({
            "jsonrpc": "2.0",
            "id": "hello",
            "method": "client_hello",
            "params": {}
        }),
        "server_hello response",
    )
    .await;
    assert_eq!(body["result"]["type"], "server_hello");
    let methods: Vec<String> = body["result"]["capabilities"]["supported_methods"]
        .as_array()
        .expect("supported_methods array")
        .iter()
        .map(|m| m.as_str().unwrap_or_default().to_string())
        .collect();
    // Denied surfaces must NOT be advertised to an ingress credential.
    for denied in ["profile/llm/upsert", "session/list", "system/status.get"] {
        assert!(
            !methods.iter().any(|m| m == denied),
            "ingress client_hello must not advertise {denied}; got {methods:?}"
        );
    }
    // A session-scoped typed method the credential CAN call is still advertised.
    assert!(
        methods.iter().any(|m| m == "session/messages_page"),
        "ingress client_hello must still advertise session-scoped methods; got {methods:?}"
    );

    server.abort();
    let _ = server.await;
}

async fn failed_handshake_status(
    addr: &str,
    session_id: &str,
    token: &str,
) -> tokio_tungstenite::tungstenite::http::StatusCode {
    let url = format!(
        "ws://{addr}/v1/session_ingress/ws/{session_id}?token={token}&ui_feature=auxiliary.rest_to_ws.v1"
    );
    let error = match connect_async(url).await {
        Ok(_) => panic!("hostile work-secret handshake unexpectedly succeeded"),
        Err(error) => error,
    };
    match error {
        tokio_tungstenite::tungstenite::Error::Http(response) => response.status(),
        other => panic!("expected HTTP handshake refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn work_secret_ws_rejects_malformed_wrong_session_and_expiry() {
    let dir = TempDir::new().unwrap();
    let (state, store) = state_with_work_secret_store(&dir);
    issue_work_secret(&store, "local:owner", "owner-token");
    issue_work_secret_with_ttl(
        &store,
        "local:expired",
        "expired-token",
        Duration::milliseconds(20),
    );
    tokio::time::sleep(std::time::Duration::from_millis(60)).await;
    let (addr, server) = spawn_api(state).await;

    for (session, token, expected) in [
        (
            "local:owner",
            "malformed-token",
            tokio_tungstenite::tungstenite::http::StatusCode::UNAUTHORIZED,
        ),
        (
            "local:other-session",
            "owner-token",
            tokio_tungstenite::tungstenite::http::StatusCode::FORBIDDEN,
        ),
        (
            "local:expired",
            "expired-token",
            tokio_tungstenite::tungstenite::http::StatusCode::UNAUTHORIZED,
        ),
    ] {
        assert_eq!(
            failed_handshake_status(&addr, session, token).await,
            expected
        );
    }
    server.abort();
    let _ = server.await;
}
