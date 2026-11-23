//! The singleton scheduler is configured only in a fresh test process.
use super::*;
use crate::autonomy::supervisor_store::{ContinuationStatus, SupervisorStore};
use octos_llm::{ChatResponse, StopReason};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio_tungstenite::tungstenite::Message as Frame;

#[path = "ui_protocol_checked_delivery_tests.rs"]
mod checked_delivery;

#[derive(Default)]
struct HandoffModel {
    activity_calls: AtomicUsize,
    blocked_calls: AtomicUsize,
    activity_blocked: tokio::sync::Notify,
    inbox_calls: AtomicUsize,
    owner_ready: tokio::sync::Notify,
    allow_inbox: tokio::sync::Notify,
    maker_calls: AtomicUsize,
    supervisor_calls: AtomicUsize,
    reviewed: AtomicBool,
    hold_sibling: tokio::sync::Notify,
    sibling_entered: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl octos_llm::LlmProvider for HandoffModel {
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[octos_llm::ToolSpec],
        _: &octos_llm::ChatConfig,
    ) -> eyre::Result<ChatResponse> {
        let user = messages
            .iter()
            .rfind(|m| m.role == MessageRole::User)
            .map(|m| m.content.as_str())
            .unwrap_or_default();
        let mut calls = Vec::new();
        if user.contains("IMPORT_WORKER_RETURNS_WITHOUT_RECEIPT") {
            self.activity_calls.fetch_add(1, Ordering::SeqCst);
        } else if user.contains("import-e2e/partition-a") && user.contains("is blocked") {
            self.blocked_calls.fetch_add(1, Ordering::SeqCst);
            self.activity_blocked.notify_one();
        } else if user.contains("WAIT_SIBLING") {
            self.sibling_entered.notify_one();
            self.hold_sibling.notified().await;
        } else if user.contains("RECONCILE_ACCOUNTED_CHECKPOINT") {
            let (tool, arguments) = match self.inbox_calls.fetch_add(1, Ordering::SeqCst) {
                0 => {
                    self.owner_ready.notify_one();
                    self.allow_inbox.notified().await;
                    ("discover_tools", json!({"name":"peer_checkpoint_inbox"}))
                }
                1 => (
                    "run_tool",
                    json!({"name":"peer_checkpoint_inbox","arguments":{}}),
                ),
                2 => {
                    let listing = &messages
                        .iter()
                        .rev()
                        .find(|m| m.role == MessageRole::Tool)
                        .unwrap()
                        .content;
                    assert!(
                        listing.contains("already-bound.json") && listing.contains("page-03.json"),
                        "{listing}"
                    );
                    let id = listing
                        .split("external/peer_checkpoint_handoff/")
                        .skip(1)
                        .filter_map(|part| part.split_once("/old-revision").map(|(stage, _)| stage))
                        .find(|stage| uuid::Uuid::parse_str(stage).is_ok())
                        .unwrap();
                    let receipt = format!("external/peer_checkpoint_handoff/{id}/old-revision");
                    (
                        "run_tool",
                        json!({"name":"peer_checkpoint_inbox","arguments":{"acknowledge":[receipt]}}),
                    )
                }
                _ => ("", Value::Null),
            };
            if !tool.is_empty() {
                calls.push(octos_core::ToolCall {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: tool.into(),
                    arguments,
                    metadata: None,
                });
            }
        } else if user.contains("MAKE_HANDOFF") {
            let call = self.maker_calls.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                assert!(
                    tools.iter().any(|t| t.name == "discover_tools"),
                    "missing discovery gateway"
                );
                calls.push(octos_core::ToolCall {
                    id: "discover-publish".into(),
                    name: "discover_tools".into(),
                    arguments: json!({"name":"peer_publish"}),
                    metadata: None,
                });
            } else if call < 4 {
                assert!(
                    messages
                        .iter()
                        .any(|m| m.role == MessageRole::Tool && m.content.contains("handoff_id")),
                    "registered peer_publish does not advertise handoff_id"
                );
                let (path, revision) = if call == 1 {
                    ("already-bound.json", "old-revision")
                } else {
                    ("review/page-03.json", "page-03-r01")
                };
                calls.push(octos_core::ToolCall {
                    id:format!("publish-{call}"), name:"run_tool".into(),
                    arguments:json!({"name":"peer_publish","arguments":{"message":path,"handoff_id":revision}}),
                    metadata:None,
                });
            } else {
                assert!(
                    messages
                        .iter()
                        .any(|m| m.role == MessageRole::Tool
                            && m.content.contains("already accepted")),
                    "missing deduplication receipt"
                );
            }
        } else if user.contains("Your staged peer published a checkpoint") {
            assert!(user.contains("review/page-03.json"));
            assert!(user.contains("page-03-r01"));
            if let Some(result) = messages
                .iter()
                .rev()
                .take_while(|m| m.role != MessageRole::User)
                .find(|m| m.role == MessageRole::Tool)
            {
                assert!(
                    result.content.contains("FIXTURE_REVIEW_ARTIFACT"),
                    "read failed: {}",
                    result.content
                );
                self.reviewed.store(true, Ordering::SeqCst);
            } else {
                assert_eq!(
                    self.supervisor_calls.fetch_add(1, Ordering::SeqCst),
                    0,
                    "unexpected duplicate supervisor request: {:?}",
                    messages
                        .iter()
                        .map(|m| (&m.role, &m.tool_call_id))
                        .collect::<Vec<_>>()
                );
                calls.push(octos_core::ToolCall {
                    id: "review-page".into(),
                    name: "run_tool".into(),
                    arguments: json!({"name":"read_file","arguments":{"path":"review/page-03.json"}}),
                    metadata: None,
                });
            }
        }
        let stop_reason = if calls.is_empty() {
            StopReason::EndTurn
        } else {
            StopReason::ToolUse
        };
        Ok(ChatResponse {
            content: Some("fixture reply".into()),
            reasoning_content: None,
            tool_calls: calls,
            stop_reason,
            usage: Default::default(),
            provider_index: None,
        })
    }
    fn model_id(&self) -> &str {
        "checkpoint-test"
    }
    fn provider_name(&self) -> &str {
        "stub"
    }
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn rpc(socket: &mut Socket, method: &str, params: Value) -> Value {
    let id = uuid::Uuid::new_v4().to_string();
    socket
        .send(Frame::Text(
            json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        while let Some(frame) = socket.next().await {
            let Frame::Text(text) = frame.unwrap() else {
                continue;
            };
            let row: Value = serde_json::from_str(&text).unwrap();
            if row["id"] == id {
                assert!(row.get("error").is_none(), "{method}: {row}");
                return row["result"].clone();
            }
        }
        panic!("socket closed before {method}")
    })
    .await
    .expect("RPC timed out")
}

#[test]
fn peer_checkpoint_handoff_registered_tool_wakes_only_owning_supervisor_over_oup() {
    run_isolated_handoff(false);
}

#[test]
fn workflow_activity_worker_exit_drives_bounded_recovery_over_oup() {
    run_isolated_handoff(true);
}

fn run_isolated_handoff(activity_only: bool) {
    const CHILD: &str = "OCTOS_TEST_PEER_HANDOFF_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let temp = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg(std::thread::current().name().unwrap())
            .arg("--nocapture")
            .env(CHILD, "1")
            .env("OCTOS_DATA_DIR", temp.path())
            .env("RUST_MIN_STACK", "8388608")
            .output()
            .unwrap();
        assert!(
            output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1 passed"),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(3)
        .thread_stack_size(8 * 1024 * 1024)
        .enable_all()
        .build()
        .unwrap()
        .block_on(exercise_handoff(activity_only));
}

async fn exercise_handoff(activity_only: bool) {
    // This function runs in a dedicated test process. A lost wake cannot be
    // hidden by a two-second periodic drain during the 40-second assertion.
    TEST_WORKFLOW_EVENTS_ONLY.store(activity_only, Ordering::Relaxed);
    let tmp = tempfile::tempdir().unwrap();
    let workspace = dunce::canonicalize(tmp.path()).unwrap();
    std::fs::create_dir(workspace.join("review")).unwrap();
    std::fs::write(
        workspace.join("review/page-03.json"),
        r#"{"verification":"FIXTURE_REVIEW_ARTIFACT"}"#,
    )
    .unwrap();
    let model = Arc::new(HandoffModel::default());
    let (state, runtime) =
        test_support::state_with_profile_llm(tmp.path(), MAIN_PROFILE_ID, model.clone()).await;
    let store = workspace.join("supervisor");
    default_agent_orchestrator()
        .configure_supervisor_store(&store)
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "ws://{}/api/ui-protocol/ws?ui_features=session.workspace_cwd.v1",
        listener.local_addr().unwrap()
    );
    let app = crate::api::build_router(state);
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap()
    });
    let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    let master = SessionKey::with_profile(MAIN_PROFILE_ID, "local", "handoff-e2e");
    let maker =
        SessionKey::with_profile_topic(MAIN_PROFILE_ID, "local", "handoff-e2e", "peer-maker");
    let sibling =
        SessionKey::with_profile_topic(MAIN_PROFILE_ID, "local", "handoff-e2e", "peer-sibling");
    rpc(
        &mut socket,
        "session/open",
        json!({"session_id":master, "profile_id":MAIN_PROFILE_ID, "cwd":workspace}),
    )
    .await;
    rpc(&mut socket,"peer/prepare",json!({"session_id":master, "profile_id":MAIN_PROFILE_ID, "cwd":workspace, "worktree":false,"names":["maker","sibling"],"n":2,"brief":"Work on your assignment"})).await;
    for peer in [&maker, &sibling] {
        rpc(
            &mut socket,
            "session/open",
            json!({"session_id":peer,"profile_id":MAIN_PROFILE_ID,"cwd":workspace}),
        )
        .await;
    }
    if activity_only {
        use octos_workflows::activity::{ActivityAttempt, Admission};
        let scheduler = default_agent_orchestrator();
        let root = &runtime.data_dir;
        let worker = PeerCheckpointHandoff::capture(
            &root.join("peers"),
            MAIN_PROFILE_ID,
            &maker,
            &workspace,
        )
        .unwrap();
        let stream = PathBuf::from("import-receipts.json");
        std::fs::write(root.join(&stream), br#"{"next":"import","events":[]}"#).unwrap();
        let id = "import-e2e/partition-a";
        let contract = workflow_activity::PeerActivity::bind(
            root,
            &worker,
            ActivityAttempt {
                operation: id.into(),
                attempt: 0,
                max_resumes: 1,
            },
            stream,
            "import",
            "committed",
            Admission::Awaited,
            Default::default(),
            Default::default(),
        )
        .unwrap();
        let start = std::time::Instant::now();
        scheduler
            .enqueue_peer_assignment(
                "import-e2e",
                &maker,
                MAIN_PROFILE_ID,
                &worker.slug,
                id,
                "IMPORT_WORKER_RETURNS_WITHOUT_RECEIPT",
                [],
                Some(serde_json::to_string(&contract).unwrap()),
                || Ok(()),
            )
            .into_callback_result(&worker.slug)
            .unwrap();
        tokio::time::timeout(Duration::from_secs(40), model.activity_blocked.notified())
            .await
            .expect("the host should continue the worker once, then notify the owner");
        assert_eq!(model.activity_calls.load(Ordering::SeqCst), 2);
        assert_eq!(model.blocked_calls.load(Ordering::SeqCst), 1);
        let durable = SupervisorStore::new(&store).load_state().unwrap();
        let attempts: Vec<_> = durable
            .continuations
            .values()
            .filter(|x| x.group_id == "import-e2e")
            .collect();
        assert_eq!(
            attempts.len(),
            4,
            "two worker turns and their two return events"
        );
        assert!(
            attempts
                .iter()
                .all(|x| x.status == ContinuationStatus::Completed)
        );
        assert!(
            attempts
                .iter()
                .any(|x| x.result.as_deref() == Some("workflow_activity_blocked"))
        );
        eprintln!(
            "event-only OUP fixture, admission through two worker returns and owner notification: {:?}",
            start.elapsed()
        );
        server.abort();
        return;
    }
    let turn_params = |s: &SessionKey, prompt| json!({"session_id":s,"turn_id":TurnId::new(),"input":[{"kind":"text","text":prompt}]});
    rpc(
        &mut socket,
        "turn/start",
        turn_params(&sibling, "WAIT_SIBLING"),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(20), model.sibling_entered.notified())
        .await
        .expect("sibling must be running");
    std::fs::create_dir_all(workspace.join(".octos")).unwrap();
    let report = b"checked blocker";
    std::fs::write(workspace.join("already-bound.json"), report).unwrap();
    use sha2::Digest as _;
    std::fs::write(workspace.join(".octos/continuation.json"),json!({
        "schema_version":1,"task_id":"blocked-fixture","owner_agent":format!("session:{}",master.0),
        "evidence_file":".octos/evidence.json","stagnant_attempts":3,"required_checks":[{"id":"package"}],
        "deliverables":["final.pptx"]
    }).to_string()).unwrap();
    std::fs::write(workspace.join(".octos/evidence.json"),json!({
        "schema_version":1,"task_id":"blocked-fixture","attempt":1,"phase":"blocked",
        "artifacts":[{"path":"already-bound.json","sha256":format!("{:x}",sha2::Sha256::digest(report))}],
        "checks":[{"id":"package","status":"fail"}]
    }).to_string()).unwrap();
    rpc(
        &mut socket,
        "turn/start",
        turn_params(&master, "RECONCILE_ACCOUNTED_CHECKPOINT"),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(20), model.owner_ready.notified())
        .await
        .unwrap();
    rpc(
        &mut socket,
        "turn/start",
        turn_params(&maker, "MAKE_HANDOFF"),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(20), async {
        while model.maker_calls.load(Ordering::SeqCst) < 5 {
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap();
    model.allow_inbox.notify_one();
    let durable = SupervisorStore::new(&store);
    tokio::time::timeout(Duration::from_secs(40), async {
        loop {
            let state = durable.load_state().unwrap();
            let wakes: Vec<_> = state
                .continuations
                .values()
                .filter(|c| {
                    c.group_id == crate::autonomy::agent_orchestrator::PEER_CHECKPOINT_HANDOFF_KIND
                })
                .collect();
            if model.maker_calls.load(Ordering::SeqCst) >= 4
                && wakes.len() == 2
                && wakes
                    .iter()
                    .all(|w| w.status == ContinuationStatus::Completed)
            {
                let old = wakes
                    .iter()
                    .find(|w| w.continuation_id.ends_with("/old-revision"))
                    .unwrap();
                assert_eq!(
                    old.result.as_deref(),
                    Some("acknowledged_by_owning_supervisor")
                );
                let evidence: Value = serde_json::from_slice(
                    &std::fs::read(workspace.join(".octos/continuation-state.json")).unwrap(),
                )
                .unwrap();
                assert_eq!(evidence["completion_blocked"], true);
                assert!(!workspace.join("final.pptx").exists());
                assert_eq!(wakes[0].metadata["session_id"], master.0);
                assert_eq!(
                    wakes[0].metadata["payload:envelope"]
                        .as_str()
                        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                        .unwrap()["publication"]["workspace"],
                    json!(workspace)
                );
                assert_eq!(model.supervisor_calls.load(Ordering::SeqCst), 1);
                assert!(model.reviewed.load(Ordering::SeqCst));
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("registered handoff reaches the owning supervisor while sibling remains active");
    assert!(
        runtime
            .data_dir
            .join("peers/maker/checkpoint.json")
            .is_file()
    );
    model.hold_sibling.notify_one();
    server.abort();
}
