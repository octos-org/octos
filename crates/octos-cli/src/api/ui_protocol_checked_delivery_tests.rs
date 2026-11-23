//! Exercise the host side of the checked multi-page delivery contract across
//! actual turns, a process restart and a replay of the exact peer publication.
use super::*;
use sha2::{Digest, Sha256};

#[derive(Default)]
struct DeliveryModel {
    calls: AtomicUsize,
    owner_steps: AtomicUsize,
    peer_steps: AtomicUsize,
    start_review: tokio::sync::Notify,
    review_finished: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl octos_llm::LlmProvider for DeliveryModel {
    async fn chat(
        &self,
        messages: &[Message],
        _: &[octos_llm::ToolSpec],
        _: &octos_llm::ChatConfig,
    ) -> eyre::Result<ChatResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let prompt = &messages
            .iter()
            .rfind(|m| m.role == MessageRole::User)
            .unwrap()
            .content;
        let last_tool = || {
            messages
                .iter()
                .rfind(|m| m.role == MessageRole::Tool)
                .map(|m| m.content.as_str())
                .unwrap()
        };
        let mut tool = None;
        let mut content = "DELIVERY COMPLETE";
        if prompt.contains("CLOSE_CHECKED_DELIVERY") {
            tool = match self.owner_steps.fetch_add(1, Ordering::SeqCst) {
                0 => {
                    self.start_review.notify_one();
                    self.review_finished.notified().await;
                    Some(("peer_checkpoint_inbox", json!({})))
                }
                1 => {
                    let listing = last_tool();
                    let receipt = listing
                        .split("external/peer_checkpoint_handoff/")
                        .skip(1)
                        .find_map(|s| {
                            s.split_once("/final-revision")
                                .and_then(|(id, _)| uuid::Uuid::parse_str(id).ok())
                        })
                        .expect("missing final review receipt");
                    Some((
                        "peer_checkpoint_inbox",
                        json!({"acknowledge":[format!("external/peer_checkpoint_handoff/{receipt}/final-revision")]}),
                    ))
                }
                2 => Some(("peer_gather", json!({"slugs":["reviewer"]}))),
                3 => {
                    assert!(last_tool().contains("REVIEW PUBLISHED"), "{}", last_tool());
                    None
                }
                other => panic!("unexpected continuation of finished owner turn: {other}"),
            };
        } else if prompt.contains("PUBLISH_FINAL_REVIEW") {
            content = "REVIEW PUBLISHED";
            if self.peer_steps.fetch_add(1, Ordering::SeqCst) == 0 {
                tool = Some((
                    "peer_publish",
                    json!({"handoff_id":"final-revision","message":"review/verdict.json"}),
                ));
            }
        } else {
            assert!(
                prompt.contains("FRESH_USER_REQUEST"),
                "unexpected model turn: {prompt}"
            );
        }
        let tool_calls = tool
            .map(|(name, arguments)| octos_core::ToolCall {
                id: uuid::Uuid::new_v4().to_string(),
                name: "run_tool".into(),
                arguments: json!({"name":name,"arguments":arguments}),
                metadata: None,
            })
            .into_iter()
            .collect::<Vec<_>>();
        Ok(ChatResponse {
            content: Some(content.into()),
            reasoning_content: None,
            stop_reason: if tool_calls.is_empty() {
                StopReason::EndTurn
            } else {
                StopReason::ToolUse
            },
            tool_calls,
            usage: Default::default(),
            provider_index: None,
        })
    }
    fn model_id(&self) -> &str {
        "checked-delivery-test"
    }
    fn provider_name(&self) -> &str {
        "stub"
    }
}

// The PPTX package has an independent integration test for the actual build,
// review and output bytes. This fixture is the host's input contract.
fn install_checked_snapshot(work: &Path, owner: &SessionKey) {
    let control = work.join(".octos");
    std::fs::create_dir_all(&control).unwrap();
    let policy_path = control.join("continuation.json");
    if policy_path.exists() {
        // A full synthetic native assembly fixture can be used with the same
        // boundary test. The caller supplies a fresh, task-local workspace.
        let mut policy: Value =
            serde_json::from_slice(&std::fs::read(&policy_path).unwrap()).unwrap();
        policy["owner_agent"] = json!(format!("session:{}", owner.0));
        std::fs::write(policy_path, policy.to_string()).unwrap();
        assert!(!control.join("continuation-state.json").exists());
        return;
    }
    let paths = [
        "final/deck.pptx",
        "final/render/slide-001.png",
        "final/qa.json",
        "review/verdict.json",
    ];
    let mut artifacts = Vec::new();
    for file in paths {
        let path = work.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let bytes = format!("host contract fixture: {file}");
        std::fs::write(path, &bytes).unwrap();
        artifacts.push(json!({"path":file,"sha256":format!("{:x}",Sha256::digest(bytes))}));
    }
    let names = ["package", "copy", "artwork", "readability"];
    std::fs::write(policy_path,json!({"schema_version":1,"task_id":"assembled",
        "owner_agent":format!("session:{}",owner.0),"evidence_file":".octos/iteration-evidence.json","stagnant_attempts":3,
        "required_checks":names.map(|id|json!({"id":id,"epsilon":0})),"deliverables":paths}).to_string()).unwrap();
    std::fs::write(
        control.join("iteration-evidence.json"),
        json!({"schema_version":1,"task_id":"assembled",
        "phase":"checked","attempt":1,"scope":["alpha","beta"],"artifacts":artifacts,
        "checks":names.map(|id|json!({"id":id,"status":"pass","remaining":0}))})
        .to_string(),
    )
    .unwrap();
}

#[test]
fn checked_delivery_completes_and_survives_restart_without_waking_old_work() {
    const PHASE: &str = "OCTOS_TEST_CHECKED_DELIVERY_PHASE";
    let Ok(phase) = std::env::var(PHASE) else {
        let temp = tempfile::tempdir().unwrap();
        for phase in ["deliver", "restart"] {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg(std::thread::current().name().unwrap())
                .arg("--nocapture")
                .env(PHASE, phase)
                .env("OCTOS_DATA_DIR", temp.path())
                .env("RUST_MIN_STACK", "8388608")
                .output()
                .unwrap();
            assert!(
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout).contains("1 passed"),
                "{phase}: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        return;
    };
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(3)
        .thread_stack_size(8 * 1024 * 1024)
        .enable_all()
        .build()
        .unwrap()
        .block_on(exercise(&phase));
}

async fn completed(state: &AppState, session: &SessionKey, turn: &TurnId) {
    let ledger = event_ledger(state).await;
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let (events, _) = ledger.snapshot_with_cursor(session, None).unwrap();
            for event in events {
                match event.event {
                    UiProtocolLedgerEvent::Notification(UiNotification::TurnCompleted(e))
                        if e.turn_id == *turn =>
                    {
                        assert!(
                            e.session_result.is_some(),
                            "completion needs a committed assistant message"
                        );
                        return;
                    }
                    UiProtocolLedgerEvent::Notification(UiNotification::TurnError(e))
                        if e.turn_id == *turn =>
                    {
                        panic!("{}: {}", e.code, e.message)
                    }
                    _ => {}
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("turn did not complete");
}

async fn exercise(phase: &str) {
    let data = std::path::PathBuf::from(std::env::var_os("OCTOS_DATA_DIR").unwrap());
    let work = data.join("work");
    std::fs::create_dir_all(&work).unwrap();
    let work = dunce::canonicalize(work).unwrap();
    let master = SessionKey::with_profile(MAIN_PROFILE_ID, "local", "checked-delivery");
    let reviewer = SessionKey::with_profile_topic(
        MAIN_PROFILE_ID,
        "local",
        "checked-delivery",
        "peer-reviewer",
    );
    let model = Arc::new(DeliveryModel::default());
    let (state, runtime) =
        test_support::state_with_profile_llm(&data, MAIN_PROFILE_ID, model.clone()).await;
    let store = data.join("supervisor");
    default_agent_orchestrator()
        .configure_supervisor_store(&store)
        .unwrap();
    let peers = runtime.data_dir.join("peers");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "ws://{}/api/ui-protocol/ws?ui_features=session.workspace_cwd.v1",
        listener.local_addr().unwrap()
    );
    let app = crate::api::build_router(state.clone());
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap()
    });
    let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    let open = |session: &SessionKey| json!({"session_id":session,"profile_id":MAIN_PROFILE_ID,"cwd":work});
    rpc(&mut socket, "session/open", open(&master)).await;
    if phase == "deliver" {
        rpc(&mut socket,"peer/prepare",json!({"session_id":master,"profile_id":MAIN_PROFILE_ID,"cwd":work,
            "worktree":false,"names":["reviewer"],"n":1,"brief":"Publish the already checked independent verdict."})).await;
    }
    rpc(&mut socket, "session/open", open(&reviewer)).await;
    let start = |session: &SessionKey, turn: &TurnId, prompt: &str| json!({"session_id":session,"turn_id":turn,"input":[{"kind":"text","text":prompt}]});
    if phase == "deliver" {
        install_checked_snapshot(&work, &master);
        let owner_turn = TurnId::new();
        rpc(
            &mut socket,
            "turn/start",
            start(&master, &owner_turn, "CLOSE_CHECKED_DELIVERY"),
        )
        .await;
        tokio::time::timeout(Duration::from_secs(15), model.start_review.notified())
            .await
            .unwrap();
        let review_turn = TurnId::new();
        rpc(
            &mut socket,
            "turn/start",
            start(&reviewer, &review_turn, "PUBLISH_FINAL_REVIEW"),
        )
        .await;
        completed(&state, &reviewer, &review_turn).await;
        model.review_finished.notify_one();
        completed(&state, &master, &owner_turn).await;
        assert_eq!(model.calls.load(Ordering::SeqCst), 6);
    } else {
        assert_eq!(phase, "restart");
        let recorded =
            PeerCheckpointHandoff::capture(&peers, MAIN_PROFILE_ID, &reviewer, &work).unwrap();
        let replay = recorded
            .publish(
                default_agent_orchestrator(),
                &peers,
                &reviewer,
                "replayed-turn",
                "review/verdict.json",
                "final-revision",
            )
            .unwrap();
        assert!(replay.contains("already accepted"), "{replay}");
        enqueue_boot_owed_peer_fleet_synthesis(MAIN_PROFILE_ID, &peers).await;
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        while !peer_result_was_consumed(&peers, "reviewer", &read_peer_consumption(&peers, &master))
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("peer consumption receipt must survive restart");
    let control = work.join(".octos");
    let observed: Value =
        serde_json::from_slice(&std::fs::read(control.join("continuation-state.json")).unwrap())
            .unwrap();
    let snapshot = std::fs::read(control.join("iteration-evidence.json")).unwrap();
    assert_eq!(
        observed["snapshot_digest"],
        format!("{:x}", Sha256::digest(snapshot))
    );
    assert_eq!(observed["completion_gap_attempts"], 0);
    assert_eq!(observed["completion_blocked"], false);
    let durable = SupervisorStore::new(&store).load_state().unwrap();
    assert_eq!(
        durable.continuations.len(),
        1,
        "only the original checkpoint should exist"
    );
    let receipt = durable.continuations.values().next().unwrap();
    assert_eq!(receipt.status, ContinuationStatus::Completed);
    assert_eq!(
        receipt.result.as_deref(),
        Some("acknowledged_by_owning_supervisor")
    );
    if phase == "restart" {
        assert_eq!(model.calls.load(Ordering::SeqCst), 0);
        let fresh = TurnId::new();
        rpc(
            &mut socket,
            "turn/start",
            start(&master, &fresh, "FRESH_USER_REQUEST"),
        )
        .await;
        completed(&state, &master, &fresh).await;
        assert_eq!(
            model.calls.load(Ordering::SeqCst),
            1,
            "fresh user input runs exactly once"
        );
    }
    server.abort();
}
