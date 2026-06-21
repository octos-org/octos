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

#[tokio::test]
async fn work_secret_ws_round_trip_and_revocation_close() {
    let dir = TempDir::new().unwrap();
    let session_id = "local:work-secret";
    let token = "guest-token";
    let (state, store) = state_with_work_secret_store(&dir);
    store
        .issue(
            session_id,
            token,
            "http://127.0.0.1:50080",
            Duration::minutes(5),
            None,
        )
        .unwrap();
    let (addr, server) = spawn_api(state).await;

    let url = format!(
        "ws://{addr}/v1/session_ingress/ws/{session_id}?session_ingress_token={token}&ui_feature=auxiliary.rest_to_ws.v1"
    );
    let (mut ws, _response) = connect_async(url).await.unwrap();

    ws.send(Message::Text(
        json!({
            "jsonrpc": "2.0",
            "id": "status-before-revoke",
            "method": "session/status.get",
            "params": { "session_id": session_id }
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();

    let response = tokio::time::timeout(std::time::Duration::from_secs(3), ws.next())
        .await
        .expect("timed out waiting for session ingress response")
        .expect("websocket ended before first response")
        .expect("failed to read first response");
    let Message::Text(body) = response else {
        panic!("expected JSON-RPC text response, got {response:?}");
    };
    let body: Value = serde_json::from_str(&body).unwrap();
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
