//! Real stdio admission and workspace-tool boundaries of UPCR-2026-RB85220B5.
#![cfg(all(unix, feature = "api"))]

#[path = "support/serve_process.rs"]
mod serve_process;

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

fn command(home: &Path, instance: Option<&Path>, workspace: &Path) -> Command {
    let mut cmd = Command::new(serve_process::octos_binary());
    cmd.args([
        "serve",
        "--stdio",
        "--host-scoped-profile",
        "allowed",
        "--data-dir",
    ])
    .arg(home)
    .current_dir(workspace)
    .env("OCTOS_NO_MODEL_DOWNLOAD", "1")
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    for key in ["OCTOS_HOME", "OCTOS_DATA_DIR", "OCTOS_INSTANCE_DATA_DIR"] {
        cmd.env_remove(key);
    }
    if let Some(instance) = instance {
        cmd.arg("--instance-data-dir").arg(instance);
    }
    cmd
}

#[test]
fn should_reject_host_scope_without_network_fence_or_independent_instance() {
    let _serial = serve_process::serial_guard();
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir(&home).unwrap();
    for (name, instance, no_network, diagnostic) in [
        (
            "network",
            Some(dir.path().join("network")),
            false,
            "requires stdio, an instance directory, and --no-network",
        ),
        (
            "missing",
            None,
            true,
            "requires stdio, an instance directory, and --no-network",
        ),
        (
            "same",
            Some(home.clone()),
            true,
            "requires stdio, an instance directory, and --no-network",
        ),
        (
            "nested",
            Some(home.join("nested")),
            true,
            "must be separate canonical directories",
        ),
    ] {
        let mut cmd = command(&home, instance.as_deref(), dir.path());
        if no_network {
            cmd.arg("--no-network");
        }
        let child = cmd.spawn().unwrap();
        let (_, mut cleanup) = serve_process::Cleanup::with_child(child, &dir.path().join(name));
        let child = cleanup.child.as_mut().unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "{name}: unsafe server did not exit"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        let stderr = std::io::read_to_string(child.stderr.take().unwrap()).unwrap();
        assert!(!status.success(), "{name}: unsafe startup accepted");
        assert!(stderr.contains(diagnostic), "{name}: {stderr}");
    }
}

struct StdioClient {
    cleanup: serve_process::Cleanup,
    frames: mpsc::Receiver<Value>,
    stderr: Arc<Mutex<String>>,
    deadline: Instant,
}

impl StdioClient {
    fn start(mut command: Command, instance: &Path) -> Self {
        let mut child = command.spawn().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr_pipe = child.stderr.take().unwrap();
        let stderr = Arc::new(Mutex::new(String::new()));
        let sink = stderr.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr_pipe).lines() {
                sink.lock().unwrap().push_str(&line.unwrap());
            }
        });
        let (tx, frames) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let frame = serde_json::from_str(&line.unwrap()).expect("stdio JSON frame");
                if tx.send(frame).is_err() {
                    break;
                }
            }
        });
        let (_, cleanup) = serve_process::Cleanup::with_child(child, instance);
        Self {
            cleanup,
            frames,
            stderr,
            deadline: Instant::now() + Duration::from_secs(60),
        }
    }

    fn receive(&self) -> Value {
        self.frames
            .recv_timeout(self.deadline.saturating_duration_since(Instant::now()))
            .unwrap_or_else(|error| {
                panic!(
                    "stdio response missing: {error}; stderr: {}",
                    self.stderr.lock().unwrap()
                )
            })
    }

    fn rpc(&mut self, id: &str, method: &str, params: Value) -> Value {
        let stdin = self.cleanup.child.as_mut().unwrap().stdin.as_mut().unwrap();
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params})
        )
        .unwrap();
        stdin.flush().unwrap();
        loop {
            let frame = self.receive();
            if frame["id"] == id {
                return frame;
            }
        }
    }

    fn ok(&mut self, id: &str, method: &str, params: Value) -> Value {
        let response = self.rpc(id, method, params);
        assert!(response.get("error").is_none(), "{method}: {response}");
        response["result"].clone()
    }
}

// A local deterministic provider drives the real session's registry. It neither
// executes tools itself nor touches the owner's profile, model or credentials.
async fn model(
    axum::extract::State(observed): axum::extract::State<Arc<Mutex<Vec<Value>>>>,
    axum::Json(request): axum::Json<Value>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let results: Vec<Value> = request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "tool")
        .cloned()
        .collect();
    let calls = [
        (
            "call_read_inside",
            "read_file",
            json!({"path":"visible.txt"}),
        ),
        (
            "call_read_alias",
            "read_file",
            json!({"path":"alias/visible.txt"}),
        ),
        (
            "call_write_inside",
            "write_file",
            json!({"path":"created.txt", "content":"INSIDE_WRITE"}),
        ),
        (
            "call_read_escape",
            "read_file",
            json!({"path":"escape/secret.txt"}),
        ),
        ("call_list_inside", "list_dir", json!({"path":"."})),
        ("call_list_escape", "list_dir", json!({"path":"escape"})),
        (
            "call_write_escape",
            "write_file",
            json!({"path":"escape/secret.txt", "content":"OUTSIDE_WRITE"}),
        ),
    ];
    // One call per iteration: a denied read must not cancel a write sibling
    // before the write tool itself gets a chance to enforce its boundary.
    let message = if let Some((id, name, args)) = calls.get(results.len()) {
        json!({"role":"assistant", "tool_calls":[{
            "id":id,"type":"function","function":{"name":name,"arguments":args.to_string()}
        }]})
    } else {
        *observed.lock().unwrap() = results;
        json!({"role":"assistant", "content":"boundary probe complete"})
    };
    let finish = if message.get("tool_calls").is_some() {
        "tool_calls"
    } else {
        "stop"
    };
    if request["stream"] == true {
        let mut delta = message;
        if let Some(calls) = delta["tool_calls"].as_array_mut() {
            for (index, call) in calls.iter_mut().enumerate() {
                call["index"] = json!(index);
            }
        }
        let chunk = json!({
            "id": "probe", "object": "chat.completion.chunk", "model": "mock-model",
            "choices": [{"index": 0, "delta": delta, "finish_reason": null}]
        });
        let done = json!({
            "id": "probe", "object": "chat.completion.chunk", "model": "mock-model",
            "choices": [{"index": 0, "delta": {}, "finish_reason": finish}]
        });
        (
            [("content-type", "text/event-stream")],
            format!("data: {chunk}\n\ndata: {done}\n\ndata: [DONE]\n\n"),
        )
            .into_response()
    } else {
        axum::Json(json!({
            "id": "probe", "object": "chat.completion", "model": "mock-model",
            "choices": [{"index": 0, "message": message, "finish_reason": finish}]
        }))
        .into_response()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn should_confine_real_stdio_sessions_to_the_host_profile_and_workspace() {
    let dir = tempfile::tempdir().unwrap();
    // darwin: TMPDIR lives under the /var symlink; serve's fence needs canonical roots.
    let root = dir.path().canonicalize().unwrap();
    let home = root.join("home");
    let instance = root.join("instance");
    let workspace = root.join("workspace");
    let outside = root.join("outside");
    for path in [&home, &workspace, &outside] {
        std::fs::create_dir(path).unwrap();
    }
    std::fs::write(workspace.join("visible.txt"), "INSIDE_READ").unwrap();
    std::fs::write(outside.join("secret.txt"), "OUTSIDE_SECRET").unwrap();
    std::os::unix::fs::symlink(&outside, workspace.join("escape")).unwrap();
    // This alias passes canonical containment but must still fail the
    // descriptor-relative component walk (the ancestor-swap race boundary).
    std::os::unix::fs::symlink(&workspace, workspace.join("alias")).unwrap();
    let observed = Arc::new(Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = axum::Router::new()
        .route("/v1/chat/completions", axum::routing::post(model))
        .route(
            "/v1/models",
            axum::routing::get(|| async {
                axum::Json(json!({"object":"list", "data":[{"id":"mock-model", "object":"model"}]}))
            }),
        )
        .with_state(observed.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    std::fs::create_dir(home.join("profiles")).unwrap();
    for profile in ["allowed", "foreign"] {
        std::fs::write(home.join(format!("profiles/{profile}.json")), json!({
            "id":profile, "name":profile, "enabled":true,
            "created_at":"2026-11-13T00:00:00Z", "updated_at":"2026-11-13T00:00:00Z",
            "config":{"llm":{"primary":{"family_id":"local","model_id":"mock-model",
                "route":{"base_url":format!("http://127.0.0.1:{port}/v1"),"api_type":"openai"}}}}
        }).to_string()).unwrap();
    }
    let mut cmd = command(&home, Some(&instance), &workspace);
    cmd.args(["--no-network", "--solo"]);
    let mut client = StdioClient::start(cmd, &instance);
    let hello = client.ok("hello", "client_hello", json!({}));
    let features = hello["capabilities"]["supported_features"]
        .as_array()
        .unwrap();
    for feature in [
        "runtime.host_scoped_profile.v1",
        "runtime.host_scoped_workspace_io.v1",
        "runtime.host_scoped_search.v1",
        "runtime.host_scoped_vision.v1",
        "runtime.host_scoped_web.v1",
        "runtime.host_patch_worker.v1",
    ] {
        assert!(features.contains(&json!(feature)), "{hello}");
    }
    let foreign = client.rpc(
        "foreign",
        "session/open",
        json!({"session_id":"foreign:local:probe", "profile_id":"foreign", "cwd":workspace}),
    );
    assert_eq!(foreign["error"]["code"], -32120, "{foreign}");
    assert!(
        foreign["error"]["message"]
            .as_str()
            .unwrap()
            .contains("profile is outside the host scope"),
        "{foreign}"
    );
    let session = "allowed:local:probe";
    client.ok(
        "open",
        "session/open",
        json!({"session_id":session,"profile_id":"allowed","cwd":workspace}),
    );
    let turn = uuid::Uuid::now_v7().to_string();
    client.ok(
        "turn",
        "turn/start",
        json!({
            "session_id": session, "turn_id": turn,
            "input": [{"kind": "text", "text": "host-boundary probe"}]
        }),
    );
    let mut tool_results = std::collections::BTreeMap::new();
    loop {
        let frame = client.receive();
        if frame["params"]["payload"]["type"] == "tool_end" {
            let data = &frame["params"]["payload"]["data"];
            tool_results.insert(
                data["tool_call_id"].as_str().unwrap().to_owned(),
                data.clone(),
            );
        }
        if frame["params"]["payload"]["type"] == "turn_terminal" {
            assert_eq!(
                frame["params"]["payload"]["data"]["outcome"], "completed",
                "{frame}"
            );
            break;
        }
    }
    let results = observed.lock().unwrap().clone();
    let content = |id: &str| -> String {
        results
            .iter()
            .find(|message| message["tool_call_id"] == id)
            .unwrap_or_else(|| panic!("missing tool {id}: {results:?}"))["content"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    assert!(
        content("call_read_inside").contains("INSIDE_READ"),
        "{results:?}"
    );
    assert!(
        content("call_list_inside").contains("visible.txt"),
        "{results:?}"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("created.txt")).unwrap(),
        "INSIDE_WRITE"
    );
    for id in [
        "call_read_alias",
        "call_read_escape",
        "call_list_escape",
        "call_write_escape",
    ] {
        assert_eq!(
            tool_results[id]["status"], "error",
            "{id}: {:?}",
            tool_results[id]
        );
        assert!(!content(id).contains("OUTSIDE_SECRET"), "{id}: {results:?}");
    }
    assert_eq!(
        std::fs::read_to_string(outside.join("secret.txt")).unwrap(),
        "OUTSIDE_SECRET"
    );
    client.ok(
        "network",
        "permission/profile/set",
        json!({"session_id":session,"update":{"network":"allow"}}),
    );
    client.ok(
        "widened",
        "turn/start",
        json!({
            "session_id": session, "turn_id": uuid::Uuid::now_v7().to_string(),
            "input": [{"kind": "text", "text": "must not start"}]
        }),
    );
    loop {
        let frame = client.receive();
        let payload = &frame["params"]["payload"];
        assert_ne!(
            payload["type"], "tool_start",
            "widened turn executed a tool: {frame}"
        );
        if payload["type"] == "turn_terminal" {
            assert_eq!(payload["data"]["outcome"], "errored", "{frame}");
            assert_eq!(
                payload["data"]["error"]["code"], "runtime_unavailable",
                "{frame}"
            );
            assert!(
                payload["data"]["error"]
                    .to_string()
                    .contains("cannot widen filesystem or shell network permissions"),
                "{frame}"
            );
            break;
        }
    }
    drop(client);
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
}
