use super::*;

#[tokio::test]
async fn input_eof_closes_an_idle_acp_session_without_waiting_for_output_to_close() {
    use futures::StreamExt;
    let data = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let factory = TestAgentFactory::new(
        Arc::new(BarrierLlm {
            calls: std::sync::atomic::AtomicUsize::new(1),
            entered: Arc::new(tokio::sync::Notify::new()),
            release: Arc::new(tokio::sync::Notify::new()),
            seen: Arc::new(Mutex::new(Vec::new())),
        }),
        data.path().to_owned(),
        cwd.path().to_owned(),
    );
    let (mut client, server) = agent_client_protocol::Channel::duplex();
    let serving = tokio::spawn(serve(Arc::new(factory), server));
    for (id, method, params) in [
        (
            1,
            "initialize",
            serde_json::json!({"protocolVersion":1, "clientCapabilities":{}}),
        ),
        (
            2,
            "session/new",
            serde_json::json!({"cwd":cwd.path(), "mcpServers":[]}),
        ),
    ] {
        client
            .tx
            .unbounded_send(Ok(serde_json::from_value(serde_json::json!({
                "jsonrpc":"2.0", "id":id, "method":method, "params":params,
            }))
            .unwrap()))
            .unwrap();
        loop {
            let frame = tokio::time::timeout(std::time::Duration::from_secs(10), client.rx.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let frame = serde_json::to_value(frame).unwrap();
            if frame["id"] == id {
                assert!(frame.get("error").is_none(), "{frame}");
                break;
            }
        }
    }
    client.tx.close_channel();
    tokio::time::timeout(std::time::Duration::from_secs(3), serving)
        .await
        .expect("stdin EOF must not wait for the idle output pump")
        .unwrap()
        .unwrap();
}

#[test]
fn canonical_hydration_replays_reasoning_before_answer_including_reasoning_only_rows() {
    for answer in ["answer", ""] {
        let message = serde_json::from_value(serde_json::json!({
            "seq": 1, "role": "assistant", "content": answer,
            "persisted_at": "2026-09-04T00:00:00Z", "reasoning_content": "thought",
        }))
        .unwrap();
        let updates = replay_message(message);
        assert!(matches!(&updates[0], SessionUpdate::AgentThoughtChunk(_)));
        assert_eq!(updates.len(), if answer.is_empty() { 1 } else { 2 });
        if !answer.is_empty() {
            assert!(matches!(&updates[1], SessionUpdate::AgentMessageChunk(_)));
        }
    }
}

#[test]
fn late_cancel_ack_cannot_cancel_the_next_prompt() {
    let cancellation = PromptCancellation::default();
    let first = cancellation.begin();
    let pending_cancel = cancellation.current();
    let second = cancellation.begin();
    pending_cancel.store(true, Ordering::Release);
    assert!(first.load(Ordering::Acquire));
    assert!(!second.load(Ordering::Acquire));
}

struct BarrierLlm {
    calls: std::sync::atomic::AtomicUsize,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    /// One entry per `chat()` call: the `content` of every incoming message,
    /// so a test can assert what history a later turn was handed.
    seen: Arc<Mutex<Vec<Vec<String>>>>,
}

#[async_trait::async_trait]
impl octos_llm::LlmProvider for BarrierLlm {
    async fn chat(
        &self,
        messages: &[octos_core::Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &octos_llm::ChatConfig,
    ) -> eyre::Result<octos_llm::ChatResponse> {
        self.seen
            .lock()
            .await
            .push(messages.iter().map(|m| m.content.clone()).collect());
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            // Announce that the turn is executing, then wait for the test
            // to deliver the cancel and release us.
            self.entered.notify_one();
            self.release.notified().await;
        }
        Ok(octos_llm::ChatResponse {
            content: Some("done".to_string()),
            reasoning_content: None,
            tool_calls: vec![],
            stop_reason: octos_llm::StopReason::EndTurn,
            usage: octos_llm::TokenUsage::default(),
            provider_index: None,
        })
    }
    fn provider_name(&self) -> &str {
        "barrier-mock"
    }
    fn model_id(&self) -> &str {
        "barrier-1"
    }
}

/// Test transport: expose the octos ACP agent (backed by `factory`) as a
/// `ConnectTo<Client>` so an in-process client can drive it. The session
/// map is caller-supplied so tests can observe cancel flags directly.
struct CancelTestTransport {
    factory: TestAgentFactory,
    sessions: Sessions,
}

impl agent_client_protocol::ConnectTo<Client> for CancelTestTransport {
    async fn connect_to(
        self,
        client: impl agent_client_protocol::ConnectTo<AcpAgentRole> + 'static,
    ) -> std::result::Result<(), AcpError> {
        serve_with_sessions(Arc::new(self.factory), self.sessions, client).await
    }
}

/// Wait until `session/cancel` has been DISPATCHED for `sid` (its shutdown
/// flag is observably set). `send_notification` only queues the frame; on a
/// loaded runner the barrier release could otherwise win the race against
/// the dispatch loop, the turn would complete un-cancelled, and the test
/// would flake to `EndTurn` (seen repeatedly on CI under load).
async fn wait_for_cancel_dispatch(sessions: &Sessions, sid: &SessionId) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let flagged = sessions
            .lock()
            .await
            .get(sid)
            .and_then(|slot| slot.get())
            .map(|s| s.cancelled.current().load(Ordering::Acquire))
            .unwrap_or(false);
        if flagged {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "session/cancel was not dispatched within 10s"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

/// A `session/cancel` delivered WHILE the turn is in flight must yield
/// `StopReason::Cancelled`. With the reset moved onto the dispatch loop
/// (`handle_prompt`), the cancel that arrives after the prompt is accepted
/// sets the flag and it survives into the turn, which reports `Cancelled`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn should_report_cancelled_when_session_cancel_arrives_during_turn() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cwd = tmp.path().to_path_buf();
    let memory_dir = tmp.path().join("memory");
    std::fs::create_dir_all(&memory_dir).unwrap();

    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let llm: Arc<dyn octos_llm::LlmProvider> = Arc::new(BarrierLlm {
        calls: std::sync::atomic::AtomicUsize::new(0),
        entered: entered.clone(),
        release: release.clone(),
        seen: Arc::new(Mutex::new(Vec::new())),
    });
    let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));
    let sessions_for_canceller = sessions.clone();
    let transport = CancelTestTransport {
        factory: TestAgentFactory::new(llm, memory_dir, cwd.clone()),
        sessions,
    };

    let stop_reason: Arc<Mutex<Option<StopReason>>> = Arc::new(Mutex::new(None));
    let stop_for_main = stop_reason.clone();
    let prompt_cwd = cwd.clone();

    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        Client
            .builder()
            .name("octos-acp-cancel-client")
            .on_receive_notification(
                async move |_notif: SessionNotification, _cx: ConnectionTo<AcpAgentRole>| Ok(()),
                on_receive_notification!(),
            )
            .connect_with(
                transport,
                |connection: ConnectionTo<AcpAgentRole>| async move {
                    connection
                        .send_request(InitializeRequest::new(
                            agent_client_protocol::schema::ProtocolVersion::V1,
                        ))
                        .block_task()
                        .await?;
                    let new_session = connection
                        .send_request(NewSessionRequest::new(prompt_cwd.clone()))
                        .block_task()
                        .await?;
                    let session_id = new_session.session_id;

                    // Concurrently: once the turn is executing, deliver a cancel,
                    // wait until it has been dispatched (flag observably set),
                    // then release the barrier so the LLM call returns.
                    let cancel_conn = connection.clone();
                    let cancel_sid = session_id.clone();
                    let canceller = tokio::spawn(async move {
                        entered.notified().await;
                        cancel_conn
                            .send_notification(CancelNotification::new(cancel_sid.clone()))
                            .expect("send cancel");
                        wait_for_cancel_dispatch(&sessions_for_canceller, &cancel_sid).await;
                        release.notify_one();
                    });

                    let prompt = connection
                        .send_request(PromptRequest::new(
                            session_id.clone(),
                            vec![ContentBlock::Text(
                                agent_client_protocol::schema::v1::TextContent::new("hello"),
                            )],
                        ))
                        .block_task()
                        .await?;
                    canceller.await.expect("canceller joins");
                    *stop_for_main.lock().await = Some(prompt.stop_reason);
                    Ok(())
                },
            ),
    )
    .await
    .expect("ACP request cycle must complete within 20 seconds")
    .expect("ACP client run completes");

    let got = *stop_reason.lock().await;
    assert!(
        matches!(got, Some(StopReason::Cancelled)),
        "cancel during the turn must yield Cancelled, got {got:?}"
    );
}

/// After a turn is cancelled, a FRESH prompt on the same session (no cancel)
/// must return `EndTurn` — proving the stale-flag reset was RELOCATED onto
/// `handle_prompt` (run before each turn spawns), not simply deleted. If the
/// reset were gone entirely, the stale `true` from the cancelled turn would
/// leak and wrongly report `Cancelled` again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn should_report_end_turn_for_fresh_prompt_after_prior_turn_was_cancelled() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cwd = tmp.path().to_path_buf();
    let memory_dir = tmp.path().join("memory");
    std::fs::create_dir_all(&memory_dir).unwrap();

    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let seen: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let llm: Arc<dyn octos_llm::LlmProvider> = Arc::new(BarrierLlm {
        calls: std::sync::atomic::AtomicUsize::new(0),
        entered: entered.clone(),
        release: release.clone(),
        seen: seen.clone(),
    });
    let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));
    let sessions_for_canceller = sessions.clone();
    let transport = CancelTestTransport {
        factory: TestAgentFactory::new(llm, memory_dir, cwd.clone()),
        sessions,
    };

    let second_stop: Arc<Mutex<Option<StopReason>>> = Arc::new(Mutex::new(None));
    let second_for_main = second_stop.clone();
    let prompt_cwd = cwd.clone();

    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        Client
            .builder()
            .name("octos-acp-cancel-then-fresh-client")
            .on_receive_notification(
                async move |_notif: SessionNotification, _cx: ConnectionTo<AcpAgentRole>| Ok(()),
                on_receive_notification!(),
            )
            .connect_with(
                transport,
                |connection: ConnectionTo<AcpAgentRole>| async move {
                    connection
                        .send_request(InitializeRequest::new(
                            agent_client_protocol::schema::ProtocolVersion::V1,
                        ))
                        .block_task()
                        .await?;
                    let new_session = connection
                        .send_request(NewSessionRequest::new(prompt_cwd.clone()))
                        .block_task()
                        .await?;
                    let session_id = new_session.session_id;

                    // Turn 1: cancel it mid-flight (as in the test above),
                    // holding the barrier until the cancel is dispatched.
                    let cancel_conn = connection.clone();
                    let cancel_sid = session_id.clone();
                    let canceller = tokio::spawn(async move {
                        entered.notified().await;
                        cancel_conn
                            .send_notification(CancelNotification::new(cancel_sid.clone()))
                            .expect("send cancel");
                        wait_for_cancel_dispatch(&sessions_for_canceller, &cancel_sid).await;
                        release.notify_one();
                    });
                    let first = connection
                        .send_request(PromptRequest::new(
                            session_id.clone(),
                            vec![ContentBlock::Text(
                                agent_client_protocol::schema::v1::TextContent::new("first"),
                            )],
                        ))
                        .block_task()
                        .await?;
                    canceller.await.expect("canceller joins");
                    assert!(
                        matches!(first.stop_reason, StopReason::Cancelled),
                        "sanity: turn 1 should be Cancelled, got {:?}",
                        first.stop_reason
                    );

                    // Turn 2: no cancel. The BarrierLlm only blocks its FIRST call,
                    // so this returns immediately. Must be EndTurn — the stale flag
                    // from turn 1 must have been reset before turn 2 spawned.
                    let second = connection
                        .send_request(PromptRequest::new(
                            session_id.clone(),
                            vec![ContentBlock::Text(
                                agent_client_protocol::schema::v1::TextContent::new("second"),
                            )],
                        ))
                        .block_task()
                        .await?;
                    *second_for_main.lock().await = Some(second.stop_reason);
                    Ok(())
                },
            ),
    )
    .await
    .expect("ACP request cycle must complete within 20 seconds")
    .expect("ACP client run completes");

    let got = *second_stop.lock().await;
    assert!(
        matches!(got, Some(StopReason::EndTurn)),
        "fresh prompt after a cancelled turn must be EndTurn (reset relocated, \
             not deleted), got {got:?}"
    );

    // codex round-3 regression: the CANCELLED turn's aborted assistant reply
    // ("done") must NOT be persisted into history, so the fresh second turn's
    // chat() must not see it. Without the `!cancelled` guard, the partial
    // reply leaks into the next prompt's context.
    //
    // Select the fresh turn by the "second" user prompt it carries — a
    // cancelled first turn may retry (stream fallback while the shutdown flag
    // is set), so it is NOT necessarily snapshot index 1 (codex round-4).
    let snapshots = seen.lock().await;
    let fresh = snapshots
        .iter()
        .find(|snap| snap.iter().any(|c| c == "second"))
        .expect("the fresh second prompt must have reached the LLM");
    assert!(
        !fresh.iter().any(|c| c == "done"),
        "the cancelled turn's aborted assistant reply must NOT persist into the \
             fresh prompt's history; fresh-turn messages: {fresh:?}"
    );
}
