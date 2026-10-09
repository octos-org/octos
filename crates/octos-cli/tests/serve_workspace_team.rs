//! Real-process shared-server contract. Uses a local deterministic model stub;
//! no external API credentials or model calls.
#![cfg(all(unix, feature = "api"))]

use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::process::{Child, Command, Stdio};
use std::time::Duration;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message, client::IntoClientRequest},
};

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
async fn frame(ws: &mut Socket) -> Value {
    tokio::time::timeout(Duration::from_secs(45), async {
        loop {
            match ws.next().await.expect("connected").expect("frame") {
                Message::Text(text) => return serde_json::from_str(&text).unwrap(),
                Message::Ping(data) => ws.send(Message::Pong(data)).await.unwrap(),
                Message::Close(_) => panic!("unexpected close"),
                _ => {}
            }
        }
    })
    .await
    .expect("frame timeout")
}
async fn rpc(ws: &mut Socket, method: &str, params: Value) -> Value {
    ws.send(Message::Text(
        json!({"jsonrpc":"2.0","id":method,"method":method,"params":params})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    loop {
        let value = frame(ws).await;
        if value["id"] == method {
            assert!(value.get("error").is_none(), "{value}");
            return value["result"].clone();
        }
    }
}
async fn connect(record: &Value) -> Socket {
    let mut request = record["endpoint"]
        .as_str()
        .unwrap()
        .into_client_request()
        .unwrap();
    request.headers_mut().insert(
        "authorization",
        format!("Bearer {}", record["auth_token"].as_str().unwrap())
            .parse()
            .unwrap(),
    );
    request.headers_mut().insert(
        "X-Octos-Ui-Features",
        "session.workspace_cwd.v1".parse().unwrap(),
    );
    connect_async(request).await.unwrap().0
}

async fn completed(ws: &mut Socket, session: &octos_core::SessionKey) {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let value = frame(ws).await;
            if value["method"] == "projection/envelope"
                && value["params"]["payload"]["type"] == "turn_terminal"
            {
                let (base, topic) = session.0.split_once('#').expect("topic-scoped session");
                assert_eq!(value["params"]["session_id"], base);
                assert_eq!(value["params"]["topic"], topic);
                assert_eq!(
                    value["params"]["payload"]["data"]["outcome"], "completed",
                    "{value}"
                );
                return;
            }
        }
    })
    .await
    .expect("turn completion timeout");
}

#[tokio::test]
async fn serve_workspace_team_shared_identity_two_clients_message_and_disconnect() {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("repo");
    let data = tmp.path().join("state");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::create_dir(&data).unwrap();
    let model = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = model.local_addr().unwrap();
    let gate = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let app = axum::Router::new().route("/v1/chat/completions", axum::routing::post(move || {
        let gate = gate.clone();
        let calls = calls.clone();
        async move {
        // The first two requests must overlap; a global one-turn lock would
        // leave the first stuck here and fail the client timeout.
        if calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) < 2 { gate.wait().await; }
        tokio::time::sleep(Duration::from_millis(150)).await;
        let chunk = json!({"id":"fixture","object":"chat.completion.chunk","created":0,"model":"gpt-4o-mini","choices":[{"index":0,"delta":{"role":"assistant","content":"workspace fixture completed"},"finish_reason":null}]});
        let end = json!({"id":"fixture","object":"chat.completion.chunk","created":0,"model":"gpt-4o-mini","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]});
        ([ ("content-type", "text/event-stream") ], format!("data: {chunk}\n\ndata: {end}\n\ndata: [DONE]\n\n"))
    }}));
    let model_server = tokio::spawn(async move {
        axum::serve(model, app).await.unwrap();
    });
    let profile: octos_cli::profiles::UserProfile = serde_json::from_value(json!({
        "id":"team-e2e", "name":"Team fixture", "enabled":true,
        "created_at":"2026-10-08T00:00:00Z", "updated_at":"2026-10-08T00:00:00Z",
        "config":{"llm":{"primary":{"family_id":"openai","model_id":"gpt-4o-mini",
            "route":{"base_url":format!("http://{address}/v1"),"api_key_env":"WORKSPACE_TEAM_FIXTURE_KEY"}}},
            "env_vars":{"WORKSPACE_TEAM_FIXTURE_KEY":"fixture-only"}}
    })).unwrap();
    octos_cli::profiles::ProfileStore::open(&data, &data)
        .unwrap()
        .save(&profile)
        .unwrap();
    let log = std::fs::File::create(tmp.path().join("server.log")).unwrap();
    let mut server = Server(
        Command::new(env!("CARGO_BIN_EXE_octos"))
            .args([
                "serve",
                "--shared",
                "--solo",
                "--host",
                "127.0.0.1",
                "--port",
                "0",
                "--cwd",
            ])
            .arg(&workspace)
            .arg("--data-dir")
            .arg(&data)
            .arg("--instance-data-dir")
            .arg(&data)
            .env_remove("OCTOS_HOME")
            .env_remove("OCTOS_DATA_DIR")
            .env_remove("OCTOS_INSTANCE_DATA_DIR")
            .env_remove("OCTOS_AUTH_TOKEN")
            .env_remove("OCTOS_TEST_TOKEN")
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap(),
    );
    let record = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            if let Some(status) = server.0.try_wait().unwrap() {
                panic!(
                    "server exited {status}: {}",
                    std::fs::read_to_string(tmp.path().join("server.log")).unwrap()
                );
            }
            if let Ok(bytes) = std::fs::read(data.join("shared-instance.json")) {
                break serde_json::from_slice::<Value>(&bytes).unwrap();
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap();
    let mut a = connect(&record).await;
    let mut b = connect(&record).await;
    let identity = rpc(&mut a, "server/instance.get", json!({})).await;
    assert_eq!(identity["instance_id"], record["instance_id"]);
    assert!(identity.get("auth_token").is_none());
    let a_id = octos_core::SessionKey::with_profile_topic("team-e2e", "local", "tui", "coding-a");
    let b_id = octos_core::SessionKey::with_profile_topic("team-e2e", "local", "tui", "coding-b");
    for (socket, id) in [(&mut a, &a_id), (&mut b, &b_id)] {
        rpc(
            socket,
            "session/open",
            json!({"session_id":id,"profile_id":"team-e2e","cwd":workspace}),
        )
        .await;
    }
    let list = rpc(&mut a, "peer/team/list", json!({"session_id":a_id})).await;
    assert_eq!(list["members"].as_array().unwrap().len(), 2);
    let elected = rpc(
        &mut a,
        "peer/team/leader/set",
        json!({"session_id":a_id,"agent_id":"workspace-2","expected_revision":list["revision"]}),
    )
    .await;
    assert_eq!(elected["leader"], "workspace-2");
    // Separate conversations in the same folder must admit simultaneous turns.
    let (first, second) = tokio::join!(
        rpc(
            &mut a,
            "turn/start",
            json!({"session_id":a_id,"turn_id":uuid::Uuid::now_v7().to_string(),"input":[{"kind":"text","text":"First independent conversation."}]})
        ),
        rpc(
            &mut b,
            "turn/start",
            json!({"session_id":b_id,"turn_id":uuid::Uuid::now_v7().to_string(),"input":[{"kind":"text","text":"Second independent conversation."}]})
        )
    );
    assert_eq!(first["accepted"], true);
    assert_eq!(second["accepted"], true);
    tokio::join!(completed(&mut a, &a_id), completed(&mut b, &b_id));
    let request = json!({"session_id":a_id,"agent_id":"workspace-2","message":"Reply with one short fixture result.","occurrence_id":"first"});
    assert_eq!(
        rpc(&mut a, "peer/team/message", request.clone()).await["receipts"][0]["status"],
        "queued"
    );
    assert_eq!(
        rpc(&mut a, "peer/team/message", request).await["receipts"][0]["status"],
        "already_queued"
    );
    a.close(None).await.unwrap();
    // Recipient runs its queued continuation despite the sender disconnecting.
    completed(&mut b, &b_id).await;
    assert!(server.0.try_wait().unwrap().is_none());
    let refreshed = rpc(&mut b, "peer/team/list", json!({"session_id":b_id})).await;
    assert_eq!(refreshed["members"][0]["attached"], false);
    assert!(refreshed["members"][1]["result_turn_id"].is_string());
    b.close(None).await.unwrap();
    model_server.abort();
}
