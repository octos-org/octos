//! Real Core restart durability for explicit session permission selections.
#![cfg(all(unix, feature = "api"))]

#[path = "support/serve_process.rs"]
mod serve_process;

use std::{process::Child, time::Duration};

use futures::{SinkExt, StreamExt};
use octos_cli::profiles::{ProfileStore, UserProfile};
use octos_core::SessionKey;
use serde_json::{Value, json};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message, client::IntoClientRequest},
};

const ADMIN: &str = "session-permission-restart-admin";
type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start(data: &std::path::Path, workspace: &std::path::Path, port: u16, solo: bool) -> Server {
    let mut command = serve_process::serve_command(port, data);
    command
        .arg("--host")
        .arg("127.0.0.1")
        .arg("--cwd")
        .arg(workspace)
        .env("OCTOS_AUTH_TOKEN", ADMIN)
        .env("OCTOS_NO_MODEL_DOWNLOAD", "1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if solo {
        command.arg("--solo");
    }
    let child = command.spawn().unwrap();
    assert!(serve_process::wait_for_port(port, Duration::from_secs(30)));
    Server(child)
}

async fn connect(port: u16) -> Socket {
    let mut request = format!("ws://127.0.0.1:{port}/api/ui-protocol/ws")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {ADMIN}").parse().unwrap());
    request.headers_mut().insert(
        "X-Octos-Ui-Features",
        "session.workspace_cwd.v1,runtime.policy_stamp.v1"
            .parse()
            .unwrap(),
    );
    connect_async(request).await.unwrap().0
}

async fn rpc(ws: &mut Socket, method: &str, params: Value) -> Value {
    ws.send(Message::Text(
        json!({"jsonrpc":"2.0","id":method,"method":method,"params":params})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let frame = ws.next().await.expect("socket closed").expect("frame");
            if let Message::Text(text) = frame {
                let value: Value = serde_json::from_str(&text).unwrap();
                if value["id"] == method {
                    return value;
                }
            }
        }
    })
    .await
    .expect("RPC response")
}

fn install_profile(data: &std::path::Path) -> std::path::PathBuf {
    let profile: UserProfile = serde_json::from_value(json!({
        "id": "ada",
        "name": "Ada",
        "enabled": true,
        "created_at": "2026-11-14T00:00:00Z",
        "updated_at": "2026-11-14T00:00:00Z",
        "config": {
            "llm": {"primary": {
                "family_id": "openai",
                "model_id": "offline-fixture",
                "route": {"base_url": "http://127.0.0.1:9/v1", "api_key_env": "FIXTURE_KEY"}
            }},
            "env_vars": {"FIXTURE_KEY": "offline"}
        }
    }))
    .unwrap();
    let store = ProfileStore::open_unified(data).unwrap();
    store.save(&profile).unwrap();
    store.resolve_data_dir(&profile)
}

// Serializes real fixed-port child processes for the whole async lifecycle.
#[allow(clippy::await_holding_lock)]
async fn exercise_mode(solo: bool) {
    let _serial = serve_process::serial_guard();
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&data).unwrap();
    std::fs::create_dir(&workspace).unwrap();
    let profile_data_dir = install_profile(&data);
    let port = serve_process::find_free_port();
    let session = SessionKey::with_profile("ada", "api", "durable");
    let legacy = SessionKey::with_profile("ada", "api", "legacy");
    let server = start(&data, &workspace, port, solo);
    let mut ws = connect(port).await;
    for id in [&session, &legacy] {
        let opened = rpc(
            &mut ws,
            "session/open",
            json!({"session_id":id,"profile_id":"ada","cwd":workspace}),
        )
        .await;
        assert!(opened.get("error").is_none(), "solo={solo}: {opened}");
    }
    let approval = if solo { "never" } else { "on-request" };
    let updated = rpc(
        &mut ws,
        "permission/profile/set",
        json!({"session_id":session,"update":{"mode":"read_only","network":"deny","approval_policy":approval}}),
    )
    .await;
    assert_eq!(updated["result"]["applied"], true, "solo={solo}: {updated}");
    drop(ws);
    drop(server);

    let server = start(&data, &workspace, port, solo);
    let mut ws = connect(port).await;
    let reopened = rpc(
        &mut ws,
        "session/open",
        json!({"session_id":session,"profile_id":"ada","cwd":workspace}),
    )
    .await;
    assert!(reopened.get("error").is_none(), "solo={solo}: {reopened}");
    let listed = rpc(
        &mut ws,
        "permission/profile/list",
        json!({"session_id":session}),
    )
    .await;
    assert_eq!(
        listed["result"]["current"],
        json!({"mode":"read_only","network":"deny"}),
        "solo={solo}: restart lost the explicit selection"
    );
    let status = rpc(
        &mut ws,
        "session/status/read",
        json!({"session_id":session}),
    )
    .await;
    assert_eq!(
        status["result"]["runtime_policy_stamp"]["approval_policy"], approval,
        "solo={solo}: {status}"
    );
    assert_eq!(
        status["result"]["runtime_policy_stamp"]["network"], "blocked",
        "solo={solo}: {status}"
    );
    let legacy_open = rpc(
        &mut ws,
        "session/open",
        json!({"session_id":legacy,"profile_id":"ada","cwd":workspace}),
    )
    .await;
    assert!(
        legacy_open.get("error").is_none(),
        "solo={solo}: {legacy_open}"
    );
    let legacy_list = rpc(
        &mut ws,
        "permission/profile/list",
        json!({"session_id":legacy}),
    )
    .await;
    assert_eq!(
        legacy_list["result"]["current"],
        json!({"mode":"workspace_write","network":"allow"}),
        "solo={solo}: legacy no-record behavior changed: {legacy_list}"
    );

    let write_fail = SessionKey::with_profile("ada", "api", "write-fail");
    let opened = rpc(
        &mut ws,
        "session/open",
        json!({"session_id":write_fail,"profile_id":"ada","cwd":workspace}),
    )
    .await;
    assert!(opened.get("error").is_none(), "solo={solo}: {opened}");
    let write_path = octos_cli::runtime::session_permissions::permission_record_path(
        &profile_data_dir,
        Some("ada"),
        &write_fail,
    );
    let write_parent = write_path.parent().unwrap();
    std::fs::create_dir_all(write_parent.parent().unwrap()).unwrap();
    std::fs::write(write_parent, "not a directory").unwrap();
    let failed = rpc(
        &mut ws,
        "permission/profile/set",
        json!({"session_id":write_fail,"update":{"mode":"read_only","network":"deny","approval_policy":approval}}),
    )
    .await;
    assert_eq!(failed["error"]["data"]["kind"], "session_permission_io");
    let blocked = rpc(
        &mut ws,
        "permission/profile/list",
        json!({"session_id":write_fail}),
    )
    .await;
    assert_eq!(blocked["error"]["data"]["kind"], "session_permission_io");
    let blocked_open = rpc(
        &mut ws,
        "session/open",
        json!({"session_id":write_fail,"profile_id":"ada","cwd":workspace}),
    )
    .await;
    assert!(blocked_open.get("error").is_some());

    let record_path = octos_cli::runtime::session_permissions::permission_record_path(
        &profile_data_dir,
        Some("ada"),
        &session,
    );
    drop(ws);
    drop(server);
    std::fs::write(&record_path, "{not-json").unwrap();
    let server = start(&data, &workspace, port, solo);
    let mut ws = connect(port).await;
    let rejected_open = rpc(
        &mut ws,
        "session/open",
        json!({"session_id":session,"profile_id":"ada","cwd":workspace}),
    )
    .await;
    assert_eq!(
        rejected_open["error"]["data"]["kind"],
        "session_permission_invalid"
    );
    let corrupt = rpc(
        &mut ws,
        "permission/profile/list",
        json!({"session_id":session}),
    )
    .await;
    assert!(corrupt.get("error").is_some(), "solo={solo}: {corrupt}");
    let corrupt_status = rpc(
        &mut ws,
        "session/status/read",
        json!({"session_id":session}),
    )
    .await;
    assert!(
        corrupt_status.get("error").is_some(),
        "solo={solo}: {corrupt_status}"
    );
    drop(ws);
    drop(server);

    std::fs::write(
        &record_path,
        json!({"version":1,"profile_id":"ada","session_id":session,"mode":"read_only","network":"deny"})
            .to_string(),
    )
    .unwrap();
    let _server = start(&data, &workspace, port, solo);
    let mut ws = connect(port).await;
    let rejected_open = rpc(
        &mut ws,
        "session/open",
        json!({"session_id":session,"profile_id":"ada","cwd":workspace}),
    )
    .await;
    assert_eq!(
        rejected_open["error"]["data"]["kind"],
        "session_permission_invalid"
    );
    let missing = rpc(
        &mut ws,
        "permission/profile/list",
        json!({"session_id":session}),
    )
    .await;
    assert!(missing.get("error").is_some(), "solo={solo}: {missing}");
}

#[tokio::test]
async fn should_preserve_explicit_permissions_across_real_core_restart() {
    exercise_mode(true).await;
    exercise_mode(false).await;
}

// The same serial process guard intentionally spans the two async servers.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn should_revalidate_persisted_relaxations_when_restarting_non_solo() {
    let _serial = serve_process::serial_guard();
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&data).unwrap();
    std::fs::create_dir(&workspace).unwrap();
    install_profile(&data);
    let port = serve_process::find_free_port();
    let session = SessionKey::with_profile("ada", "api", "cross-mode");
    let server = start(&data, &workspace, port, true);
    let mut ws = connect(port).await;
    let opened = rpc(
        &mut ws,
        "session/open",
        json!({"session_id":session,"profile_id":"ada","cwd":workspace}),
    )
    .await;
    assert!(opened.get("error").is_none(), "{opened}");
    let updated = rpc(
        &mut ws,
        "permission/profile/set",
        json!({"session_id":session,"update":{"mode":"workspace_write","network":"allow","approval_policy":"never"}}),
    )
    .await;
    assert_eq!(updated["result"]["applied"], true, "{updated}");
    drop(ws);
    drop(server);

    let _server = start(&data, &workspace, port, false);
    let mut ws = connect(port).await;
    let reopened = rpc(
        &mut ws,
        "session/open",
        json!({"session_id":session,"profile_id":"ada","cwd":workspace}),
    )
    .await;
    assert_eq!(
        reopened["error"]["data"]["kind"], "permission_profile_disallowed",
        "{reopened}"
    );
}
