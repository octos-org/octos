use super::*;
use async_trait::async_trait;
use octos_bus::Channel;

/// #2053 — scale a test's WAITING budget on Windows, where the runners
/// routinely miss fixed-duration waits that pass everywhere else
/// (`test_speculative_overflow_concurrent` failed `check-windows` while the
/// diff under test could not affect it; a plain re-run went green).
///
/// Apply this to DEADLINES only — the upper bound on how long a test is
/// willing to wait. Never apply it to a duration that drives behaviour (a
/// mock's response delay, a sleep sized against a production patience
/// window): scaling a stimulus changes what the test proves, while scaling a
/// deadline costs a passing run nothing and still fails a broken one, just
/// later.
fn waiting_budget(base: Duration) -> Duration {
    #[cfg(windows)]
    {
        base * 4
    }
    #[cfg(not(windows))]
    {
        base
    }
}
use octos_llm::{AdaptiveConfig, ChatConfig, ChatResponse, StopReason, TokenUsage, ToolSpec};
use std::sync::atomic::AtomicUsize;

fn test_context_manager(key: &SessionKey) -> Arc<StdMutex<ContextManager>> {
    Arc::new(StdMutex::new(context_manager_from_history(key, &[])))
}

/// Cross-turn cancel regression (codex P2): a `spawn_only` task spawned in
/// turn 1 polls turn-1's cancel token. When turn 2 registers a fresh
/// supervisor for the SAME session, the store must keep turn-1's supervisor
/// (still alive — the live worker holds it via `Arc<ToolRegistry>`)
/// reachable, so cancel fires the token the worker actually polls rather
/// than a later supervisor's useless fresh one. The old `HashMap::insert`
/// evicted turn-1's supervisor, leaving the task uncancellable.
#[test]
fn cancel_task_reaches_earlier_turn_supervisor_after_a_later_turn_registers() {
    let dir = tempfile::TempDir::new().unwrap();
    let data_dir = dir.path().join("profile-data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let session_key = SessionKey::new("api", "session");

    let sup1 = Arc::new(TaskSupervisor::new());
    let task_id = sup1.register("run_pipeline", "call-1", Some("api:session"));
    sup1.mark_running(&task_id);
    let live_token = sup1.cancel_token(&task_id);
    assert!(!live_token.is_cancelled());

    let store = SessionTaskQueryStore::default();
    store.register(&session_key, &sup1, &data_dir);

    // Turn 2's fresh supervisor for the same session (used to evict sup1).
    let sup2 = Arc::new(TaskSupervisor::new());
    store.register(&session_key, &sup2, &data_dir);

    store
        .cancel_task(&task_id)
        .expect("task spawned under the earlier supervisor must still cancel");
    assert!(
        live_token.is_cancelled(),
        "cancel must fire the live (turn-1) supervisor's token"
    );
}

#[test]
fn session_task_query_store_hides_absolute_output_paths() {
    let dir = tempfile::TempDir::new().unwrap();
    let data_dir = dir.path().join("profile-data");
    let workspace = data_dir
        .join("users")
        .join("api%3Asession")
        .join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let output = workspace.join("voice.mp3");
    std::fs::write(&output, b"audio").unwrap();

    let supervisor = Arc::new(TaskSupervisor::new());
    let task_ledger_path = data_dir.join("tasks.jsonl");
    supervisor.enable_persistence(&task_ledger_path).unwrap();
    let task_id = supervisor.register_with_lineage(
        "fm_tts",
        "call-1",
        Some("api:session"),
        Some(task_ledger_path.to_str().unwrap()),
    );
    supervisor.mark_running(&task_id);
    supervisor.mark_runtime_state(
        &task_id,
        octos_agent::TaskRuntimeState::DeliveringOutputs,
        Some("send_file".to_string()),
    );
    supervisor.mark_completed(&task_id, vec![output.to_string_lossy().to_string()]);

    let store = SessionTaskQueryStore::default();
    let session_key = SessionKey::new("api", "session");
    store.register(&session_key, &supervisor, &data_dir);

    let payload = store.query_json(&session_key.to_string());
    let tasks = payload.as_array().unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["lifecycle_state"], "ready");
    assert_eq!(tasks[0]["runtime_state"], "completed");
    assert_eq!(tasks[0]["runtime_detail"], "send_file");
    let files = tasks[0]["output_files"].as_array().unwrap();
    assert_eq!(files.len(), 1);
    let handle = files[0].as_str().unwrap();
    assert!(handle.starts_with("pf/"));
    assert!(!handle.starts_with("/"));
    assert_eq!(tasks[0]["parent_session_key"], "api:session");
    assert!(
        tasks[0]["child_session_key"]
            .as_str()
            .unwrap()
            .starts_with("api:session#child-")
    );
    assert!(tasks[0]["task_ledger_path"].is_null());
}

// ── Mock providers for speculative overflow tests ────────────────────

/// Mock LLM provider with configurable delay per call.
/// Returns scripted responses in FIFO order.
struct DelayedMockProvider {
    responses: std::sync::Mutex<Vec<(Duration, ChatResponse)>>,
    call_count: AtomicUsize,
    name: String,
}

impl DelayedMockProvider {
    fn new(name: &str, responses: Vec<(Duration, ChatResponse)>) -> Self {
        Self {
            responses: std::sync::Mutex::new(responses),
            call_count: AtomicUsize::new(0),
            name: name.to_string(),
        }
    }
}

#[async_trait]
impl LlmProvider for DelayedMockProvider {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[ToolSpec],
        _config: &ChatConfig,
    ) -> eyre::Result<ChatResponse> {
        self.call_count.fetch_add(1, Ordering::Relaxed);
        let (delay, response) = {
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                return Ok(ChatResponse {
                    content: Some("(no more scripted responses)".into()),
                    reasoning_content: None,
                    tool_calls: vec![],
                    stop_reason: StopReason::EndTurn,
                    usage: TokenUsage::default(),
                    provider_index: None,
                });
            }
            responses.remove(0)
        };
        tokio::time::sleep(delay).await;
        Ok(response)
    }

    fn context_window(&self) -> u32 {
        128_000
    }

    fn model_id(&self) -> &str {
        &self.name
    }

    fn provider_name(&self) -> &str {
        &self.name
    }
}

struct ErrorMockProvider {
    name: String,
    error: String,
}

impl ErrorMockProvider {
    fn new(name: &str, error: &str) -> Self {
        Self {
            name: name.to_string(),
            error: error.to_string(),
        }
    }
}

#[async_trait]
impl LlmProvider for ErrorMockProvider {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[ToolSpec],
        _config: &ChatConfig,
    ) -> eyre::Result<ChatResponse> {
        Err(eyre::eyre!(self.error.clone()))
    }

    fn context_window(&self) -> u32 {
        128_000
    }

    fn model_id(&self) -> &str {
        &self.name
    }

    fn provider_name(&self) -> &str {
        &self.name
    }
}

fn make_response(text: &str) -> ChatResponse {
    ChatResponse {
        content: Some(text.to_string()),
        reasoning_content: None,
        tool_calls: vec![],
        stop_reason: StopReason::EndTurn,
        usage: TokenUsage {
            input_tokens: 50,
            output_tokens: 10,
            ..Default::default()
        },
        provider_index: None,
    }
}

fn make_inbound(content: &str) -> ActorMessage {
    ActorMessage::Inbound {
        message: InboundMessage {
            channel: "cli".to_string(),
            chat_id: "test".to_string(),
            sender_id: "user".to_string(),
            content: content.to_string(),
            timestamp: chrono::Utc::now(),
            media: vec![],
            metadata: serde_json::json!({}),
            message_id: None,
            origin: octos_core::MessageOrigin::ExternalUser,
        },
        image_media: vec![],
        attachment_media: vec![],
        attachment_prompt: None,
    }
}

/// Unique per-test session key derived from `TempDir`. The dedicated profile
/// also keeps AppUI's global continuation drain from stealing this actor's
/// recovery turns; the orchestrator honors that barrier for non-actor drains.
fn test_session_key(dir: &std::path::Path) -> SessionKey {
    let tag = dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("test");
    SessionKey::with_profile("session-actor-tests", "cli", tag)
}

/// Build a SessionActor with configurable queue mode and optional adaptive router.
///
/// Generic setup used by queue mode, auto-escalation, and other tests.
/// `adaptive_router` controls whether speculative overflow is available.
/// `pre_seed_baseline`: if true, pre-seeds 5×500ms to establish responsiveness baseline.
async fn setup_actor_with_mode(
    agent_provider: Arc<dyn LlmProvider>,
    queue_mode: QueueMode,
    adaptive_router: Option<Arc<AdaptiveRouter>>,
    pre_seed_baseline: bool,
    dir: &tempfile::TempDir,
    start: Option<oneshot::Receiver<()>>,
    startup: Option<(ActorPublish, Arc<ConstructionTicket>, Arc<Semaphore>)>,
) -> (
    mpsc::Sender<ActorMessage>,
    mpsc::Receiver<OutboundMessage>,
    JoinHandle<()>,
    Arc<Mutex<SessionManager>>,
) {
    let session_mgr = Arc::new(Mutex::new(
        SessionManager::open(&dir.path().join("sessions")).unwrap(),
    ));
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let tools = octos_agent::ToolRegistry::with_builtins(dir.path());

    let agent = Agent::new(AgentId::new("test-mode"), agent_provider, tools, memory).with_config(
        AgentConfig {
            save_episodes: false,
            max_iterations: 1,
            ..Default::default()
        },
    );

    let (inbox_tx, inbox_rx) = mpsc::channel(32);
    let (out_tx, out_rx) = mpsc::channel(64);

    let mut responsiveness = ResponsivenessObserver::new();
    if pre_seed_baseline {
        for _ in 0..5 {
            responsiveness.record(Duration::from_millis(500));
        }
    }

    let (publish, ticket, construction_semaphore) = match startup {
        Some((publish, ticket, semaphore)) => (Some(publish), Some(ticket), Some(semaphore)),
        None => (None, None, None),
    };
    let actor = SessionActor {
        start,
        publish,
        ticket,
        construction_semaphore,
        session_key: test_session_key(dir.path()),
        channel: "cli".to_string(),
        chat_id: "test".to_string(),
        tenant_id: None,
        inbox: inbox_rx,
        self_tx: inbox_tx.clone(),
        pending_approvals: HumanPendingApprovalStore::default(),
        approvals_audit: Arc::new(crate::approvals_audit::ApprovalsAuditLog::new(
            dir.path(),
            crate::approvals_audit::ApprovalsAuditConfig::from_env(),
        )),
        agent: Arc::new(agent),
        hooks: None,
        hook_context: None,
        session_handle: Arc::new(Mutex::new(SessionHandle::open(
            dir.path(),
            &test_session_key(dir.path()),
        ))),
        out_tx,
        status_indicator: None,
        sender_user_id: None,
        user_status_config: UserStatusConfig::default(),
        data_dir: dir.path().to_path_buf(),
        usage_ledger: None,
        session_usage: Default::default(),
        usage_profile_id: "test-profile".to_string(),
        max_history: Arc::new(std::sync::atomic::AtomicUsize::new(50)),
        idle_timeout: Duration::from_secs(60),
        session_timeout: Duration::from_secs(120),
        semaphore: Arc::new(Semaphore::new(10)),
        global_shutdown: Arc::new(AtomicBool::new(false)),
        cancelled: Arc::new(AtomicBool::new(false)),
        queue_mode,
        responsiveness,
        adaptive_router,
        lane_routing: None,
        memory_store: None,
        active_overflow_tasks: Arc::new(std::sync::atomic::AtomicU32::new(0)),
        overflow_cancelled: Arc::new(AtomicBool::new(false)),
        active_sessions: Arc::new(RwLock::new(ActiveSessionStore::open(dir.path()).unwrap())),
        user_workspace: dir.path().join("workspace"),
        cron_tool: None,
        persistent_retry_state: Arc::new(StdMutex::new(LoopRetryState::default())),
        context_manager: test_context_manager(&test_session_key(dir.path())),
        retry_state_path: None,
        recovered_tasks: Arc::new(StdMutex::new(std::collections::HashSet::new())),
        consecutive_recovery_turns: Arc::new(StdMutex::new(0)),
        current_command_cmid: None,
        last_turn_total_tokens: 0,
        goal_verifier_llm: None,
    };

    let handle = tokio::spawn(actor.run());
    (inbox_tx, out_rx, handle, session_mgr)
}

/// Build a minimal SessionActor with speculative mode + adaptive router.
///
/// `agent_provider` is used by the Agent for primary calls.
/// `router_providers` are used by the AdaptiveRouter for overflow calls.
/// These MUST be separate instances (separate response queues).
async fn setup_speculative_actor(
    agent_provider: Arc<dyn LlmProvider>,
    router_providers: Vec<Arc<dyn LlmProvider>>,
    dir: &tempfile::TempDir,
) -> (
    mpsc::Sender<ActorMessage>,
    mpsc::Receiver<OutboundMessage>,
    JoinHandle<()>,
    Arc<Mutex<SessionManager>>,
) {
    // AdaptiveRouter with separate providers for overflow (serve_overflow only).
    let router = Arc::new(
        AdaptiveRouter::new(router_providers, &[], AdaptiveConfig::default())
            .with_adaptive_config(AdaptiveMode::Hedge, false),
    );
    setup_actor_with_mode(
        agent_provider,
        QueueMode::Speculative,
        Some(router),
        true,
        dir,
        None,
        None,
    )
    .await
}

#[tokio::test]
async fn test_speculative_overflow_concurrent() {
    let dir = tempfile::TempDir::new().unwrap();

    // Agent provider: 5 fast warmups + 1 slow (12s) primary call
    // + 1 fast overflow response (serve_overflow now uses the agent)
    let agent_llm = Arc::new(DelayedMockProvider::new(
        "agent",
        vec![
            (Duration::from_millis(200), make_response("warmup1")),
            (Duration::from_millis(200), make_response("warmup2")),
            (Duration::from_millis(200), make_response("warmup3")),
            (Duration::from_millis(200), make_response("warmup4")),
            (Duration::from_millis(200), make_response("warmup5")),
            // Slow call that triggers overflow (12s > 10s patience)
            (
                Duration::from_secs(12),
                make_response("slow primary answer"),
            ),
            // Overflow agent task (runs concurrently with slow primary)
            (
                Duration::from_millis(500),
                make_response("overflow answer: 1961"),
            ),
            (Duration::from_millis(200), make_response("post-overflow")),
        ],
    ));

    // Router providers (separate instances, used ONLY by serve_overflow)
    let router_a: Arc<dyn LlmProvider> = Arc::new(DelayedMockProvider::new(
        "router-a",
        vec![
            (
                Duration::from_millis(500),
                make_response("router-a overflow"),
            ),
            (Duration::from_millis(500), make_response("router-a extra")),
        ],
    ));
    let router_b: Arc<dyn LlmProvider> = Arc::new(DelayedMockProvider::new(
        "router-b",
        vec![
            (
                Duration::from_millis(100),
                make_response("overflow answer: 1961"),
            ),
            (Duration::from_millis(100), make_response("router-b extra")),
        ],
    ));

    let (tx, mut rx, handle, _session_mgr) =
        setup_speculative_actor(agent_llm, vec![router_a, router_b], &dir).await;

    // ── Phase 1: Warm-up (5 fast messages to establish baseline) ──
    for i in 0..5 {
        tx.send(make_inbound(&format!("warmup {i}"))).await.unwrap();
        // Wait for response
        let resp = tokio::time::timeout(waiting_budget(Duration::from_secs(5)), rx.recv())
            .await
            .expect("warmup response timeout")
            .expect("channel closed");
        assert!(!resp.content.is_empty(), "warmup {i} got empty response");
    }

    // ── Phase 2: Send slow request, then overflow ──
    tx.send(make_inbound("Do a complex multi-step analysis"))
        .await
        .unwrap();

    // Wait 11s for patience (10s) to be exceeded, then send overflow
    tokio::time::sleep(Duration::from_secs(11)).await;

    tx.send(make_inbound("What is 37 * 53?")).await.unwrap();

    // ── Phase 3: Collect all responses ──
    // We expect 2 user-facing responses: overflow answer + slow primary
    // answer (in some order). Skip metadata-only outbounds (the
    // user-message session_result emission added by #616 fix carries
    // routing metadata in `_session_result` but no body).
    let mut responses = Vec::new();
    let deadline = tokio::time::Instant::now() + waiting_budget(Duration::from_secs(15));
    while responses.len() < 2 {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(msg)) => {
                let is_user_session_result = msg
                    .metadata
                    .get("_session_result")
                    .and_then(|r| r.get("role"))
                    .and_then(|v| v.as_str())
                    == Some("user");
                if !is_user_session_result {
                    responses.push(msg.content);
                }
            }
            Ok(None) => break,
            Err(_) => break, // timeout
        }
    }

    assert!(
        responses.len() >= 2,
        "expected at least 2 responses (overflow + primary), got {}: {:?}",
        responses.len(),
        responses
    );

    // One should be the overflow answer, one the primary (with ⬆️ marker)
    let has_overflow = responses
        .iter()
        .any(|r| r.contains("1961") || r.contains("overflow"));
    let has_primary = responses
        .iter()
        .any(|r| r.contains("slow primary") || r.contains("primary"));

    assert!(
        has_overflow,
        "overflow response not found in: {responses:?}"
    );
    assert!(has_primary, "primary response not found in: {responses:?}");

    // ── Phase 4: Verify history is sorted by timestamp ──
    {
        // Reload from disk (actor writes via its own SessionHandle to per-user dir)
        let handle = SessionHandle::open(dir.path(), &test_session_key(dir.path()));
        let session = handle.session();
        let messages = &session.messages;
        assert!(
            messages.len() >= 4,
            "expected at least 4 messages in history (warmups + primary + overflow), got {}",
            messages.len()
        );

        // Verify timestamps are sorted
        for window in messages.windows(2) {
            assert!(
                window[0].timestamp <= window[1].timestamp,
                "history not sorted: {:?} > {:?} (contents: '{}' vs '{}')",
                window[0].timestamp,
                window[1].timestamp,
                &window[0].content[..window[0].content.len().min(50)],
                &window[1].content[..window[1].content.len().min(50)],
            );
        }
    }

    // Clean shutdown
    drop(tx);
    let _ = tokio::time::timeout(waiting_budget(Duration::from_secs(5)), handle).await;
}

/// M8.10 follow-up (#649) PERSISTENCE regression: when a late-arriving
/// `BackgroundResult` carries an `originating_thread_id`, the PERSISTED
/// JSONL row for the assistant message must carry that thread_id —
/// NOT whatever `derive_thread_id_for_new_message`'s "most recent user
/// in history" fallback would pick.
///
/// PR #664 stamped `thread_id` on the wire-side `OutboundMessage.metadata`
/// so the live SSE event routed correctly, but `persist_assistant_message`
/// kept building the message via `Message::assistant(content)` (no
/// `thread_id`). On the canonical persist path, `add_message_with_seq`
/// derives `thread_id` from the most recent USER message in history —
/// for a deep-research result that arrives after Q3, that's Q3's cmid,
/// not Q1's. Reload from JSONL therefore mis-pairs the assistant under
/// the WRONG bubble.
///
/// This test pre-seeds three users (Q1/Q2/Q3) into the on-disk session
/// transcript, sends a late `BackgroundResult` carrying Q1's cmid as
/// `originating_thread_id`, and verifies the persisted JSONL row picks
/// up Q1's cmid — proving the new pre-stamp short-circuits the
/// derivation fallback before it can mis-attribute.
#[tokio::test]
async fn late_background_result_persists_with_originating_thread_id_not_derived_from_latest_user() {
    let dir = tempfile::TempDir::new().unwrap();
    let session_key = test_session_key(dir.path());

    // Pre-seed three user messages, each with its own client_message_id,
    // through the canonical persist path so the JSONL has the same
    // shape the actor would observe on reload. After this loop the
    // disk transcript is [Q1, Q2, Q3] — Q3 is the "most recent user".
    let originating_cmid = "originating-A-deep-research-Q1";
    let later_cmids = ["B-stocks-Q2", "C-voices-Q3"];
    {
        let user_a =
            Message::user("Q1: kick off deep research").with_client_message_id(originating_cmid);
        octos_bus::session::persist_message_through_canonical_path(
            dir.path(),
            &session_key,
            user_a,
        )
        .await
        .expect("persist Q1");
        for cmid in later_cmids {
            let user = Message::user(format!("user msg {cmid}")).with_client_message_id(cmid);
            octos_bus::session::persist_message_through_canonical_path(
                dir.path(),
                &session_key,
                user,
            )
            .await
            .expect("persist later user");
        }
    }

    // Spawn the actor — its `SessionHandle::open` will load the three
    // pre-seeded users so the actor's in-memory mirror agrees with disk.
    let agent_llm = Arc::new(DelayedMockProvider::new("agent", vec![]));
    let (tx, mut rx, handle, _session_mgr) = setup_actor_with_mode(
        agent_llm,
        QueueMode::Followup,
        None,
        false,
        &dir,
        None,
        None,
    )
    .await;

    // Drive the late background result. `originating_thread_id` is Q1 —
    // pre-fix, derivation in `add_message_with_seq` would pick Q3
    // because Q3 is the most recent user. Post-fix, the persist helper
    // pre-stamps Q1 onto the assistant message so the derivation
    // fallback is skipped.
    let (ack_tx, ack_rx) = oneshot::channel();
    tx.send(ActorMessage::BackgroundResult {
        task_label: "deep_research".to_string(),
        content: "Deep research findings for Q1.".to_string(),
        kind: BackgroundResultKind::Report,
        media: vec![],
        originating_thread_id: Some(originating_cmid.to_string()),
        task_id: None,
        tool_call_id: None,
        terminal_status: None,
        ack: Some(ack_tx),
    })
    .await
    .unwrap();

    assert!(
        tokio::time::timeout(Duration::from_secs(2), ack_rx)
            .await
            .expect("ack timeout")
            .expect("actor ack"),
        "background result must be persisted"
    );

    // Drain one outbound (the wire fanout) to keep the channel from
    // back-pressuring the actor; we already pin wire behaviour in the
    // sibling test so we only need the metadata as a sanity check.
    let outbound = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("outbound timeout")
        .expect("outbound message");
    assert_eq!(
        outbound.metadata.get("thread_id").and_then(|v| v.as_str()),
        Some(originating_cmid),
        "wire metadata still must agree with persistence (sibling \
             contract); got metadata = {}",
        outbound.metadata,
    );

    // Reload the session JSONL from disk and find the persisted
    // assistant message. Its `thread_id` MUST equal Q1's cmid — NOT
    // Q3's (which is what the derivation fallback would have chosen).
    let session_handle = SessionHandle::open(dir.path(), &session_key);
    let session = session_handle.session();
    let assistant_messages: Vec<&Message> = session
        .messages
        .iter()
        .filter(|m| {
            m.role == MessageRole::Assistant && m.content.contains("Deep research findings")
        })
        .collect();
    assert_eq!(
        assistant_messages.len(),
        1,
        "expected exactly one persisted assistant message for the \
             background result; got messages = {:?}",
        session.messages,
    );
    let persisted_assistant = assistant_messages[0];
    assert_eq!(
        persisted_assistant.thread_id.as_deref(),
        Some(originating_cmid),
        "PERSISTED assistant message must carry originating thread_id \
             (Q1's cmid={originating_cmid:?}) so reload pairs it under the \
             correct user bubble; got thread_id={:?}. The derive fallback \
             would have picked Q3's cmid={:?} which is the bug.",
        persisted_assistant.thread_id,
        later_cmids.last(),
    );

    drop(tx);
    let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
}

#[tokio::test]
async fn test_agent_error_persists_to_history() {
    let dir = tempfile::TempDir::new().unwrap();
    let agent_llm = Arc::new(ErrorMockProvider::new("agent", "scripted failure"));

    let (tx, mut rx, handle, _session_mgr) = setup_actor_with_mode(
        agent_llm,
        QueueMode::Followup,
        None,
        false,
        &dir,
        None,
        None,
    )
    .await;

    tx.send(make_inbound("cause failure")).await.unwrap();

    let outbound = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("error response")
        .expect("outbound error message");
    assert_eq!(outbound.content, "Error: scripted failure");

    let session_handle = SessionHandle::open(dir.path(), &test_session_key(dir.path()));
    let session = session_handle.session();
    assert!(
        session
            .messages
            .iter()
            .any(|message| message.role == MessageRole::Assistant
                && message.content == "Error: scripted failure"),
        "error message not found in session history: {:?}",
        session
            .messages
            .iter()
            .map(|message| (message.role, message.content.clone()))
            .collect::<Vec<_>>()
    );

    drop(tx);
    let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
}

// ── Queue mode tests ─────────────────────────────────────────────────

// ── Auto-escalation tests ────────────────────────────────────────────

// ── Track B: dispatch profile routing tests ────────────────────────────

/// Helper: a minimal but REAL [`ActorFactory`], plus its outbound channel
/// halves. Callers supply the [`SessionTaskQueryStore`] so they can keep a
/// handle on the supervisors `ActorFactory::spawn` registers, and the profile
/// id so the per-profile wiring branches are exercised.
async fn build_minimal_actor_factory(
    dir: &tempfile::TempDir,
    task_query_store: SessionTaskQueryStore,
    profile_id: Option<String>,
) -> (
    ActorFactory,
    mpsc::Sender<OutboundMessage>,
    mpsc::Receiver<OutboundMessage>,
) {
    let provider: Arc<dyn LlmProvider> = Arc::new(DelayedMockProvider::new(
        "test",
        (0..20)
            .map(|_| (Duration::from_millis(100), make_response("ok")))
            .collect(),
    ));
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let session_mgr = Arc::new(Mutex::new(
        SessionManager::open(&dir.path().join("sessions")).unwrap(),
    ));
    let (out_tx, out_rx) = mpsc::channel(64);
    let tools = octos_agent::ToolRegistry::with_builtins(dir.path());
    let (spawn_tx, _spawn_rx) = mpsc::channel(32);

    let factory = ActorFactory {
        agent_config: AgentConfig {
            save_episodes: false,
            max_iterations: 1,
            ..Default::default()
        },
        llm: provider.clone(),
        llm_for_compaction: provider.clone(),
        llm_strong: provider.clone(),
        goal_verifier_llm: None,
        memory,
        memory_inject_tokens: 2500,
        memory_refresh_enabled: true,
        system_prompt: Arc::new(std::sync::RwLock::new(
            crate::commands::gateway::prompt::GatewayPromptParts {
                uses_bundled_base: false,
                pre_memory: "default prompt".to_string(),
                post_memory: String::new(),
            },
        )),
        hooks: None,
        hook_context_template: None,
        data_dir: dir.path().to_path_buf(),
        usage_ledger: None,
        session_mgr,
        out_tx: out_tx.clone(),
        spawn_inbound_tx: spawn_tx,
        cron_service: None,
        tool_registry_factory: Arc::new(SnapshotToolRegistryFactory::new(tools)),
        pipeline_factory: None,
        max_history: Arc::new(std::sync::atomic::AtomicUsize::new(50)),
        idle_timeout: Duration::from_secs(60),
        session_timeout: Duration::from_secs(120),
        shutdown: Arc::new(AtomicBool::new(false)),
        cwd: dir.path().to_path_buf(),
        sandbox_config: octos_agent::SandboxConfig::default(),
        provider_policy: None,
        tool_policy: None,
        worker_prompt: None,
        provider_router: None,
        embedder: None,
        active_sessions: Arc::new(RwLock::new(ActiveSessionStore::open(dir.path()).unwrap())),
        pending_messages: PendingMessages::default(),
        queue_mode: QueueMode::Followup,
        adaptive_router: None,
        lane_routing: None,
        memory_store: None,
        recall: None,
        profile_id,
        plugin_dirs: Vec::new(),
        plugin_extra_env: Vec::new(),
        plugin_require_signed: false,
        task_query_store,
        subagent_output_router: Arc::new(octos_agent::SubAgentOutputRouter::new(
            dir.path().join("subagent-outputs"),
        )),
    };

    (factory, out_tx, out_rx)
}

#[tokio::test]
async fn test_profile_session_keys_are_persisted_separately() {
    let dir = tempfile::TempDir::new().unwrap();
    let weather_key = SessionKey::with_profile("weather", "matrix", "!room:localhost");
    let news_key = SessionKey::with_profile("news", "matrix", "!room:localhost");

    let mut weather = SessionHandle::open(dir.path(), &weather_key);
    weather
        .add_message(Message::user("weather message"))
        .await
        .unwrap();

    let mut news = SessionHandle::open(dir.path(), &news_key);
    news.add_message(Message::user("news message"))
        .await
        .unwrap();

    let weather = SessionHandle::open(dir.path(), &weather_key);
    let news = SessionHandle::open(dir.path(), &news_key);

    assert_eq!(weather.get_history(10).len(), 1);
    assert_eq!(news.get_history(10).len(), 1);
    assert_eq!(weather.get_history(10)[0].content, "weather message");
    assert_eq!(news.get_history(10)[0].content, "news message");
}

#[tokio::test]
async fn test_persist_child_session_lifecycle_marks_orphaned_terminal_events() {
    let dir = tempfile::TempDir::new().unwrap();
    let parent = SessionKey::new("api", "missing-parent");
    let child = SessionKey("api:missing-parent#child-task-404".to_string());

    let completed = ChildSessionLifecyclePayload {
        kind: ChildSessionLifecycleKind::Completed,
        task_id: "task-404".to_string(),
        task_label: "Orphaned research".to_string(),
        instruction: "Research the missing context".to_string(),
        parent_session_key: parent.to_string(),
        child_session_key: child.to_string(),
        workflow_kind: Some("deep_research".to_string()),
        current_phase: Some("deliver_result".to_string()),
        output_files: vec!["/tmp/orphaned.md".to_string()],
        failure_action: None,
        error: None,
    };

    assert!(
        !persist_child_session_lifecycle(dir.path(), &completed)
            .await
            .unwrap()
    );

    let child_handle = SessionHandle::open(dir.path(), &child);
    let child_session = child_handle.session();
    assert_eq!(child_session.child_contracts.len(), 1);
    assert_eq!(
        child_session.child_contracts[0].join_state,
        Some(ChildSessionJoinState::Orphaned)
    );
    assert_eq!(
        child_session.child_contracts[0].terminal_state,
        Some(ChildSessionTerminalState::Completed)
    );
    assert!(
        child_session
            .messages
            .iter()
            .any(|message| message.content.contains("Join state: orphaned"))
    );
}

#[tokio::test]
async fn test_persist_child_session_lifecycle_repairs_join_when_terminal_arrives_first() {
    let dir = tempfile::TempDir::new().unwrap();
    let parent = SessionKey::new("api", "parent-session");
    let child = SessionKey("api:parent-session#child-task-555".to_string());

    let mut parent_handle = SessionHandle::open(dir.path(), &parent);
    parent_handle
        .add_message(Message::user("Start research"))
        .await
        .unwrap();

    let terminal = ChildSessionLifecyclePayload {
        kind: ChildSessionLifecycleKind::RetryableFailed,
        task_id: "task-555".to_string(),
        task_label: "Research retry".to_string(),
        instruction: "Research with flaky upstream".to_string(),
        parent_session_key: parent.to_string(),
        child_session_key: child.to_string(),
        workflow_kind: Some("deep_research".to_string()),
        current_phase: Some("research".to_string()),
        output_files: Vec::new(),
        failure_action: Some(ChildSessionFailureAction::Retry),
        error: Some("Upstream timed out".to_string()),
    };

    assert!(
        persist_child_session_lifecycle(dir.path(), &terminal)
            .await
            .unwrap()
    );

    let child_handle = SessionHandle::open(dir.path(), &child);
    let child_session = child_handle.session();
    assert_eq!(child_session.parent_key, Some(parent.clone()));
    assert!(
        child_session
            .messages
            .iter()
            .any(|message| message.content == "Start research"),
        "terminal-only join should still seed recent parent history"
    );
    assert_eq!(
        child_session.child_contracts[0].join_state,
        Some(ChildSessionJoinState::Joined)
    );
    assert_eq!(
        child_session.child_contracts[0].terminal_state,
        Some(ChildSessionTerminalState::RetryableFailure)
    );
    assert_eq!(
        child_session.child_contracts[0].failure_action,
        Some(PersistedChildSessionFailureAction::Retry)
    );
    assert!(
        child_session.messages.iter().any(|message| {
            message.content.contains("Failure action: retry")
                && message
                    .content
                    .contains("Next step: retry from the parent session")
        }),
        "retry policy note missing from terminal child session update"
    );
}

// ── M8.9: Runtime failure recovery ─────────────────────────────────

/// #2020 — enqueue a spawn_only failure recovery continuation onto the ONE
/// re-entry path (the master continuation queue) for `session_key`, the way
/// production does from `set_on_failure_signal` / the unified terminal sink.
fn enqueue_recovery_continuation(
    session_key: &SessionKey,
    signal: &octos_agent::SpawnOnlyFailureSignal,
) {
    default_agent_orchestrator().enqueue_spawn_only_failure_continuation(
        session_key,
        session_key.profile_id().unwrap_or(MAIN_PROFILE_ID),
        signal,
    );
}

/// A queued recovery continuation is drained on the actor's continuation
/// tick (2s), so recovery-turn assertions need more headroom than an
/// inbox-delivered message did.
const RECOVERY_DRAIN_DEADLINE: Duration = Duration::from_secs(20);

async fn drain_until(
    rx: &mut mpsc::Receiver<OutboundMessage>,
    needle: &str,
    seen: &mut Vec<String>,
) {
    let deadline = tokio::time::Instant::now() + RECOVERY_DRAIN_DEADLINE;
    while let Ok(Some(msg)) = tokio::time::timeout_at(deadline, rx.recv()).await {
        if msg.content.is_empty() {
            continue;
        }
        let matched = msg.content.contains(needle);
        seen.push(msg.content);
        if matched {
            return;
        }
    }
    panic!("timed out waiting for {needle:?}; saw: {seen:?}");
}

#[tokio::test]
async fn supervisor_failure_signal_generates_recovery_continuation_end_to_end() {
    // Full integration: install the failure-signal callback the gateway
    // wires in `spawn()`, trigger mark_failed, and assert the actor drains
    // the resulting continuation and runs the recovery turn.
    let dir = tempfile::TempDir::new().unwrap();
    let agent_llm = Arc::new(DelayedMockProvider::new(
        "agent",
        vec![(Duration::from_millis(50), make_response("recovery-handled"))],
    ));
    let (tx, mut rx, handle, _session_mgr) = setup_actor_with_mode(
        agent_llm,
        QueueMode::Followup,
        None,
        false,
        &dir,
        None,
        None,
    )
    .await;

    let session_key = test_session_key(dir.path());
    let supervisor = wire_supervisor_to_continuation_queue(&session_key);
    let task_id = supervisor.register_with_input(
        "fm_tts",
        "call-int-1",
        Some(session_key.to_string().as_str()),
        Some(serde_json::json!({"voice": "yangmi", "text": "hi"})),
    );
    // Synth-ack gate (feat/spawn-only-failure-feedback-loop): mark
    // the synth-ack as emitted so post-spawn failure produces a
    // SpawnOnlyFailureSignal. Production wires this from
    // `loop_runner.rs` when the synth-ack actually fires.
    supervisor.mark_synth_ack_emitted("call-int-1");
    supervisor.mark_failed(
        &task_id,
        "voice 'yangmi' not registered. available: vivian, serena.".into(),
    );

    let mut responses = Vec::new();
    drain_until(&mut rx, "recovery-handled", &mut responses).await;

    let session_handle = SessionHandle::open(dir.path(), &test_session_key(dir.path()));
    let session = session_handle.session();
    let prompt_present = session.messages.iter().any(|m| {
        m.role == MessageRole::User
            && m.content.contains("[system-internal]")
            && m.content.contains("fm_tts")
            && m.content.contains("vivian")
    });
    assert!(
        prompt_present,
        "synthetic recovery prompt should be in session history: {:?}",
        session.messages
    );

    drop(tx);
    let _ = tokio::time::timeout(Duration::from_secs(3), handle).await;
}

#[tokio::test]
async fn recovery_turn_preserves_originating_client_message_id_from_failure_signal() {
    // Issue #738: when the supervisor emits a `SpawnOnlyFailureSignal` with
    // `originating_client_message_id`, the synthetic recovery turn MUST
    // persist a user message whose `client_message_id` matches the
    // originating turn's cmid. Pre-#738 the recovery inbound stamped no
    // cmid, so `process_inbound` minted a fresh server UUIDv7 — leaving the
    // eventual successful retry's deliverables stranded under an orphan
    // thread_id with no DOM bubble in the SPA.
    //
    // #2020 re-homes this: the cmid now travels as continuation metadata
    // (`originating_client_message_id`) and is stamped back onto the inbound
    // by `synthetic_master_continuation_inbound`. Dropping that thread on
    // the way to the queue would silently reintroduce #738, so this test
    // guards the migrated path, not the retired one.
    const ORIGINATING_CMID: &str = "45756a8f-1234-4abc-8def-cafebabe0001";

    let dir = tempfile::TempDir::new().unwrap();
    let agent_llm = Arc::new(DelayedMockProvider::new(
        "agent",
        vec![(
            Duration::from_millis(50),
            make_response("recovery-handled-738"),
        )],
    ));
    let (tx, mut rx, handle, _session_mgr) = setup_actor_with_mode(
        agent_llm,
        QueueMode::Followup,
        None,
        false,
        &dir,
        None,
        None,
    )
    .await;

    let session_key = test_session_key(dir.path());
    let supervisor = wire_supervisor_to_continuation_queue(&session_key);

    // Register the failed task with the originating user turn's
    // cmid. The supervisor must thread it through to the failure
    // signal so the recovery turn inherits it.
    let task_id = supervisor.register_with_input_and_cmid(
        "deep_research",
        "call-738",
        Some(session_key.to_string().as_str()),
        Some(serde_json::json!({"query": "rust news"})),
        Some(ORIGINATING_CMID.to_string()),
    );
    supervisor.mark_synth_ack_emitted("call-738");
    supervisor.mark_failed(&task_id, "MiniMax 429 rate limited".into());

    let mut responses = Vec::new();
    drain_until(&mut rx, "recovery-handled-738", &mut responses).await;

    // The decisive assertion: the persisted user message for the
    // recovery turn must carry the originating cmid, NOT a freshly
    // minted server UUIDv7.
    let session_handle = SessionHandle::open(dir.path(), &test_session_key(dir.path()));
    let session = session_handle.session();
    let recovery_msg = session
        .messages
        .iter()
        .find(|m| {
            m.role == MessageRole::User
                && m.content.contains("[system-internal]")
                && m.content.contains("deep_research")
        })
        .expect("synthetic recovery user message must be persisted");
    assert_eq!(
        recovery_msg.client_message_id.as_deref(),
        Some(ORIGINATING_CMID),
        "recovery user message must inherit the originating cmid; got {:?}",
        recovery_msg.client_message_id,
    );

    drop(tx);
    let _ = tokio::time::timeout(Duration::from_secs(3), handle).await;
}

// ========================================================================
// M9-06 — terminal task lifecycle durability under actor inbox backpressure
// ========================================================================

// ── Wave-4 B3 `/router` chat-command tests ──────────────────────────────

// ───────────────────────── Post-spawn failure feedback loop ──────────────────────────
//
// Tests for the spawn_only post-spawn failure → synthetic recovery
// turn pipeline (PR feat/spawn-only-failure-feedback-loop). The
// supervisor-side synth-ack gate has unit coverage in
// `task_supervisor::tests`; these are end-to-end against the running
// actor.

/// Wire a TaskSupervisor to the master continuation queue exactly the way
/// `SessionActor::spawn` does in production (#2020 — the gateway's
/// `set_on_failure_signal` used to push `ActorMessage::RecoveryHint` onto
/// the actor inbox instead). Returns the supervisor so tests can drive
/// `mark_synth_ack_emitted` + `mark_failed`.
fn wire_supervisor_to_continuation_queue(
    session_key: &SessionKey,
) -> Arc<octos_agent::TaskSupervisor> {
    let supervisor = Arc::new(octos_agent::TaskSupervisor::new());
    let failure_session_key = session_key.clone();
    supervisor.set_on_failure_signal(move |signal| {
        enqueue_recovery_continuation(&failure_session_key, signal);
    });
    supervisor
}

/// Test 5: when the LLM repeatedly retries a failing tool with new
/// `tool_call_id`s, the actor caps the chain at
/// MAX_CONSECUTIVE_RECOVERY_TURNS distinct recovery turns and emits a
/// final banner instead of dispatching another LLM turn. The
/// per-task `recovered_tasks` slot doesn't help here because each
/// retry has a fresh task_id; `consecutive_recovery_turns` is the
/// safeguard.
#[tokio::test]
async fn background_failure_recovery_capped_at_max_retries() {
    let dir = tempfile::TempDir::new().unwrap();
    // Provide more responses than the cap allows so we can detect
    // "did the third LLM turn happen?" — if the cap works, only the
    // first two responses ever reach the rx channel.
    let agent_llm = Arc::new(DelayedMockProvider::new(
        "agent",
        vec![
            (Duration::from_millis(50), make_response("recovery-1")),
            (Duration::from_millis(50), make_response("recovery-2")),
            (
                Duration::from_millis(50),
                make_response("recovery-3-MUST-NOT-RUN"),
            ),
        ],
    ));
    let (tx, mut rx, handle, _session_mgr) = setup_actor_with_mode(
        agent_llm,
        QueueMode::Followup,
        None,
        false,
        &dir,
        None,
        None,
    )
    .await;

    let supervisor = wire_supervisor_to_continuation_queue(&test_session_key(dir.path()));

    // Drain `rx` until `needle` arrives. Output does NOT prove the turn
    // finished: its in-flight marker may still be held during post-processing.
    // Wait for it to clear before the next failure so the actor owns the next
    // continuation at the turn boundary.
    async fn wait_for_turn_release(session_key: &SessionKey) {
        tokio::time::timeout(RECOVERY_DRAIN_DEADLINE, async {
            while default_agent_orchestrator().is_goal_dispatch_in_flight(session_key) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for continuation turn to release"));
    }

    let mut seen: Vec<String> = Vec::new();

    // Failure 0 → first recovery turn runs.
    let t0 = supervisor.register(
        "mofa_slides",
        "call-cap-0",
        Some(test_session_key(dir.path()).to_string().as_str()),
    );
    supervisor.mark_synth_ack_emitted("call-cap-0");
    supervisor.mark_failed(&t0, "fail #0".to_string());
    drain_until(&mut rx, "recovery-1", &mut seen).await;
    wait_for_turn_release(&test_session_key(dir.path())).await;

    // Failure 1 → second recovery turn runs; the consecutive-recovery
    // counter now sits at the cap.
    let t1 = supervisor.register(
        "mofa_slides",
        "call-cap-1",
        Some(test_session_key(dir.path()).to_string().as_str()),
    );
    supervisor.mark_synth_ack_emitted("call-cap-1");
    supervisor.mark_failed(&t1, "fail #1".to_string());
    drain_until(&mut rx, "recovery-2", &mut seen).await;
    wait_for_turn_release(&test_session_key(dir.path())).await;

    // Failure 2 → the cap kicks in: a final banner is emitted INSTEAD of a
    // third LLM turn. (If the cap regressed, `recovery-3-MUST-NOT-RUN`
    // would arrive here and the `!recovery-3` assertion below would fail.)
    let t2 = supervisor.register(
        "mofa_slides",
        "call-cap-2",
        Some(test_session_key(dir.path()).to_string().as_str()),
    );
    supervisor.mark_synth_ack_emitted("call-cap-2");
    supervisor.mark_failed(&t2, "fail #2".to_string());
    drain_until(
        &mut rx,
        "Background failure could not be recovered",
        &mut seen,
    )
    .await;

    // The first two recovery LLM turns must have run.
    assert!(
        seen.iter().any(|c| c.contains("recovery-1")),
        "first recovery should run, got: {seen:?}",
    );
    assert!(
        seen.iter().any(|c| c.contains("recovery-2")),
        "second recovery should run, got: {seen:?}",
    );
    // The third must NOT run — the cap intercepts it before the LLM.
    assert!(
        !seen.iter().any(|c| c.contains("recovery-3-MUST-NOT-RUN")),
        "third recovery beyond the cap must not invoke the LLM, got: {seen:?}",
    );

    drop(tx);
    let _ = tokio::time::timeout(Duration::from_secs(3), handle).await;
}

// -----------------------------------------------------------------
// Codex round-2 MAJOR 3 (PR #1327 review): gateway session scope
// wiring. Pins that `ActorFactory::spawn` (via
// `build_gateway_session_scope`) attaches a non-None SessionScope
// with the expected skill_read_zones for per-profile gateway
// actors. Pre-fix the agent's `ctx.session_scope` stayed `None`
// for every gateway-routed session, so `read_file` fell back to
// the workspace-only legacy resolver.
// -----------------------------------------------------------------

// ── Phase 4: human-approval bridge tests (ROBRIX-PHASE4 ADR) ────────────

// ---------------------------------------------------------------------------
// #2056 — WIRING PRESENCE for the goal-task-row observers.
//
// #2059 shipped with a stated hole: every production site and every effect
// test share ONE installer, so behaviour cannot drift — but DELETING an
// installer call from a production site was caught by nothing. This test
// closes that hole for the gateway site by driving the real
// `ActorFactory::spawn` (the function that contains the call at
// `session_actor.rs`'s supervisor-wiring block) and asserting on the EFFECT:
// the supervisor `spawn` registered must create and settle a goal-ledger task
// row. It is deliberately not a source grep and not a direct call to the
// installer — either would keep passing with the call site deleted.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// #48a — OLP observability: `fallback_switch` event rows from the failover
// forwarder. Best-effort, per-session, written on every REAL lane switch
// regardless of the client-notice debounce.
// ---------------------------------------------------------------------------

// ─────────────────────────────────────────────────────────────────────────
// evening 2026-09-10 recovery regressions (PR #2283 follow-up review):
// the durable session note must be recoverable across a restart and must
// not leave a phantom in-memory note when its own persist fails. These run
// against the CURRENT actor behavior first — both MUST fail (RED) before
// any behavior change.
// ─────────────────────────────────────────────────────────────────────────

/// T2 (persist failure): the note's own durable append FAILS through a real
/// I/O error — the canonical JSONL path is replaced by a DIRECTORY (appends
/// to a directory fail on every platform; the verifier ledger lives in a
/// SIBLING directory and is untouched). No phantom in-memory note remains;
/// after restoring the file, the same-evidence retry persists exactly ONE
/// note without extra provider calls.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_actor_note_persist_failure_no_phantom_and_retry_recovers() {
    use crate::autonomy::agent_orchestrator::{
        AgentOrchestrator as _, GoalSetRequest, default_agent_orchestrator,
    };

    struct CountingEmptyVerifier {
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }
    #[async_trait::async_trait]
    impl LlmProvider for CountingEmptyVerifier {
        async fn chat(
            &self,
            _m: &[octos_core::Message],
            _t: &[octos_llm::ToolSpec],
            _c: &octos_llm::ChatConfig,
        ) -> eyre::Result<octos_llm::ChatResponse> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(octos_llm::ChatResponse {
                content: None,
                reasoning_content: Some("thinking…".to_owned()),
                tool_calls: Vec::new(),
                stop_reason: octos_llm::StopReason::EndTurn,
                usage: octos_llm::TokenUsage {
                    input_tokens: 3,
                    output_tokens: 1,
                    ..Default::default()
                },
                provider_index: None,
            })
        }
        fn model_id(&self) -> &str {
            "empty-verifier"
        }
        fn provider_name(&self) -> &str {
            "empty-verifier"
        }
    }

    let dir = tempfile::TempDir::new().expect("temp dir");
    let orchestrator = default_agent_orchestrator();
    let key = octos_core::SessionKey("phantom-note-prof:api:phantom-note-actor".to_owned());
    orchestrator
        .set_goal(GoalSetRequest {
            session_id: key.clone(),
            profile_id: "phantom-note-prof".to_owned(),
            objective: "no phantom notes".to_owned(),
            status: Some("active".to_owned()),
            token_budget: None,
            transition_actor: None,
        })
        .expect("set active goal");

    let claim = "All tasks are complete. <goal:complete>";
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

    // Create the canonical JSONL on disk with the durable claim row, then
    // locate it (the tempdir hosts exactly one session).
    let cmid = octos_core::ClientMessageId::new("seed-claim-cmid");
    octos_bus::session::persist_message_through_canonical_path(
        dir.path(),
        &key,
        octos_core::Message::assistant_with_thread(claim, octos_core::ThreadId::rooted_at(&cmid)),
    )
    .await
    .expect("seed durable claim row");
    let users_dir = dir.path().join("users");
    let canonical = walkdir_find_session_jsonl(&users_dir).expect("canonical session file on disk");

    let (factory, _out_tx, _out_rx) = build_minimal_actor_factory(
        &dir,
        SessionTaskQueryStore::default(),
        Some("phantom-note-prof".to_owned()),
    )
    .await;
    let session_handle = octos_bus::session::SessionHandle::open(dir.path(), &key);
    let handle = Arc::new(tokio::sync::Mutex::new(session_handle));
    let tools = octos_agent::ToolRegistry::with_builtins(dir.path());
    let memory = factory.memory.clone();
    let agent = Arc::new(octos_agent::Agent::new(
        AgentId::new("phantom-note-agent"),
        factory.llm.clone(),
        tools,
        memory,
    ));
    let (proxy_tx, _proxy_rx) = mpsc::channel(64);
    let (_inbox_tx, inbox_rx) = mpsc::channel(8);
    let (self_tx, _self_rx) = mpsc::channel(8);
    let mut actor = crate::session_actor::tests::session_actor_for_goal_test(
        key.clone(),
        agent,
        handle,
        proxy_tx,
        inbox_rx,
        self_tx,
        dir.path().to_path_buf(),
        Some(std::sync::Arc::new(CountingEmptyVerifier {
            calls: calls.clone(),
        })),
    );

    // REAL I/O failure: rename the JSONL aside and put an empty DIRECTORY
    // at its path — appends to a directory fail everywhere; the verifier
    // ledger (sibling goal-verifier-ledgers/ dir) is unaffected.
    let backup = canonical.with_extension("jsonl.backup");
    std::fs::rename(&canonical, &backup).expect("rename jsonl aside");
    std::fs::create_dir(&canonical).expect("place dir at jsonl path");

    // First turn: fresh failure verdict + note append FAILS (target is a dir).
    actor
        .maybe_advance_goal_runtime_after_turn("phantom-note-prof", None, std::time::Instant::now())
        .await;
    let calls_after_failure = calls.load(std::sync::atomic::Ordering::SeqCst);
    assert!(
        calls_after_failure >= 1,
        "the failing turn really ran the verifier"
    );

    // (a) No PHANTOM in-memory note: the memory mirror must not contain a
    // note whose durable append failed.
    {
        let h = actor.session_handle.lock().await;
        let phantom = h
            .session()
            .messages
            .iter()
            .filter(|m| {
                m.role == octos_core::MessageRole::System
                    && m.content.contains("goal completion not verified")
            })
            .count();
        assert_eq!(
            phantom, 0,
            "a failed durable append must not leave a phantom in-memory note"
        );
    }

    // (b) Restore the file; the same-evidence replay retries and the
    // durable file ends with EXACTLY ONE note — with no additional
    // provider call.
    std::fs::remove_dir(&canonical).expect("remove dir placeholder");
    std::fs::rename(&backup, &canonical).expect("restore jsonl");
    actor
        .maybe_advance_goal_runtime_after_turn("phantom-note-prof", None, std::time::Instant::now())
        .await;
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        calls_after_failure,
        "the recovery retry replays the ledger verdict — no extra provider call"
    );
    let durable = octos_bus::session::SessionHandle::open(dir.path(), &key);
    let notes = durable
        .session()
        .messages
        .iter()
        .filter(|m| {
            m.role == octos_core::MessageRole::System
                && m.content.contains("goal completion not verified")
        })
        .count();
    assert_eq!(
        notes, 1,
        "after the persist failure resolves, exactly one durable note exists"
    );
}

fn walkdir_find_session_jsonl(users_dir: &std::path::Path) -> Option<std::path::PathBuf> {
    // The test tempdir hosts exactly ONE session; the first .jsonl under
    // users/ IS the canonical file for it.
    fn visit(dir: &std::path::Path) -> Option<std::path::PathBuf> {
        let entries = std::fs::read_dir(dir).ok()?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(found) = visit(&path) {
                    return Some(found);
                }
            } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                return Some(path);
            }
        }
        None
    }
    visit(users_dir)
}

// ── T-0296: bounded dispatcher-owned actor construction ──────────────────

enum FakeBehavior {
    Ready,
    Block(std::sync::mpsc::Receiver<()>),
    Panic,
    Cancelled,
}

#[derive(Clone, Default)]
struct FakeFactory {
    behaviors: Arc<StdMutex<HashMap<String, VecDeque<FakeBehavior>>>>,
    entered: Arc<StdMutex<Vec<String>>>,
    inboxes: Arc<StdMutex<HashMap<String, Vec<mpsc::Receiver<ActorMessage>>>>>,
    gates: Arc<StdMutex<HashMap<String, Vec<oneshot::Receiver<()>>>>>,
    events: Arc<StdMutex<Vec<String>>>,
    active: Arc<AtomicUsize>,
    max_active: Arc<AtomicUsize>,
}

impl FakeFactory {
    fn queue(&self, key: &SessionKey, behavior: FakeBehavior) {
        self.behaviors
            .lock()
            .unwrap()
            .entry(key.0.clone())
            .or_default()
            .push_back(behavior);
    }

    fn entered(&self) -> usize {
        self.entered.lock().unwrap().len()
    }

    fn take_inbox(&self, key: &SessionKey, incarnation: usize) -> mpsc::Receiver<ActorMessage> {
        self.inboxes
            .lock()
            .unwrap()
            .get_mut(&key.0)
            .and_then(|inboxes| (incarnation < inboxes.len()).then(|| inboxes.remove(incarnation)))
            .expect("fake actor incarnation inbox")
    }

    fn take_gate(&self, key: &SessionKey, incarnation: usize) -> oneshot::Receiver<()> {
        self.gates
            .lock()
            .unwrap()
            .get_mut(&key.0)
            .and_then(|gates| (incarnation < gates.len()).then(|| gates.remove(incarnation)))
            .expect("fake actor incarnation gate")
    }
}

struct ActiveFactory(Arc<AtomicUsize>);
impl Drop for ActiveFactory {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl ActorSpawnFactory for FakeFactory {
    fn spawn(&self, params: SpawnParams<'_>) -> ActorParts {
        self.entered
            .lock()
            .unwrap()
            .push(params.session_key.0.clone());
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_active.fetch_max(active, Ordering::SeqCst);
        let _active = ActiveFactory(self.active.clone());
        let behavior = self
            .behaviors
            .lock()
            .unwrap()
            .get_mut(&params.session_key.0)
            .and_then(|behaviors| behaviors.pop_front())
            .unwrap_or(FakeBehavior::Ready);
        match behavior {
            FakeBehavior::Ready => {}
            FakeBehavior::Block(receiver) => receiver.recv().unwrap(),
            FakeBehavior::Panic => panic!("injected actor factory panic"),
            FakeBehavior::Cancelled => return ActorParts::cancelled(),
        }
        let (tx, rx) = mpsc::channel(ACTOR_INBOX_SIZE);
        let (start_tx, start_rx) = oneshot::channel();
        self.gates
            .lock()
            .unwrap()
            .entry(params.session_key.0.clone())
            .or_default()
            .push(start_rx);
        self.inboxes
            .lock()
            .unwrap()
            .entry(params.session_key.0)
            .or_default()
            .push(rx);
        ActorParts {
            tx,
            cancelled: false,
            actor: None,
            forwarder: None,
            start: Some(start_tx),
            test_events: Some(self.events.clone()),
        }
    }
}

fn fake_registry(factory: &FakeFactory) -> (ActorRegistry, mpsc::Receiver<OutboundMessage>) {
    let (out_tx, out_rx) = mpsc::channel(256);
    (
        ActorRegistry::new_with_factory(
            factory.clone(),
            Arc::new(Semaphore::new(128)),
            out_tx,
            PendingMessages::default(),
        ),
        out_rx,
    )
}

fn fake_params(key: &SessionKey, content: &str) -> DispatchParams<'static> {
    let ActorMessage::Inbound {
        message,
        image_media,
        attachment_media,
        attachment_prompt,
    } = make_inbound(content)
    else {
        unreachable!()
    };
    DispatchParams {
        message,
        image_media,
        attachment_media,
        attachment_prompt,
        session_key: key.clone(),
        reply_channel: "cli",
        reply_chat_id: "test",
        status_indicator: None,
        profile_id: None,
        tenant_id: None,
        system_prompt_override: None,
        sender_user_id: None,
    }
}

async fn finish_one(registry: &mut ActorRegistry) {
    let done = tokio::time::timeout(
        waiting_budget(Duration::from_secs(10)),
        registry.next_construction_done(),
    )
    .await
    .expect("construction result arrives")
    .expect("construction channel remains open");
    registry.finish_construction(done).await;
}

async fn next_outbound(rx: &mut mpsc::Receiver<OutboundMessage>) -> OutboundMessage {
    tokio::time::timeout(waiting_budget(Duration::from_secs(10)), rx.recv())
        .await
        .expect("outbound error arrives")
        .expect("outbound channel remains open")
}

fn actor_text(message: ActorMessage) -> String {
    let ActorMessage::Inbound { message, .. } = message else {
        panic!("expected inbound message")
    };
    message.content
}

async fn wait_for_factory_entries(factory: &FakeFactory, count: usize) {
    tokio::time::timeout(waiting_budget(Duration::from_secs(10)), async {
        while factory.entered() < count {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("factory entries reach the expected count");
}

#[tokio::test]
async fn starting_delete_fails_queue_and_ignores_late_construction() {
    let factory = FakeFactory::default();
    let (mut registry, mut out_rx) = fake_registry(&factory);
    let key = SessionKey("cli:starting-delete".into());
    let (release, blocked) = std::sync::mpsc::channel();
    factory.queue(&key, FakeBehavior::Block(blocked));
    registry.dispatch(fake_params(&key, "old")).await;
    wait_for_factory_entries(&factory, 1).await;
    tokio::time::timeout(
        waiting_budget(Duration::from_secs(10)),
        registry.remove_session(&key.0),
    )
    .await
    .expect("Starting deletion is not blocked by factory");
    assert!(next_outbound(&mut out_rx).await.content.contains("deleted"));
    release.send(()).unwrap();
    factory.queue(&key, FakeBehavior::Ready);
    finish_one(&mut registry).await;
    let mut old_inbox = factory.take_inbox(&key, 0);
    assert!(matches!(
        old_inbox.try_recv(),
        Err(mpsc::error::TryRecvError::Disconnected)
    ));
    registry.dispatch(fake_params(&key, "new")).await;
    finish_one(&mut registry).await;
    registry.dispatch(fake_params(&key, "after")).await;
    let mut inbox = factory.take_inbox(&key, 0);
    assert_eq!(actor_text(inbox.recv().await.unwrap()), "new");
    assert_eq!(actor_text(inbox.recv().await.unwrap()), "after");
}

#[tokio::test]
async fn starting_dispatch_releases_api_admission_for_delete() {
    let factory = FakeFactory::default();
    let (mut registry, mut out_rx) = fake_registry(&factory);
    let dir = tempfile::tempdir().unwrap();
    let sessions = Arc::new(Mutex::new(SessionManager::open(dir.path()).unwrap()));
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let shutdown = Arc::new(AtomicBool::new(false));
    let (deleted_tx, mut deleted_rx) = mpsc::unbounded_channel();
    let channel = Arc::new(
        octos_bus::ApiChannel::new(
            port,
            None,
            shutdown.clone(),
            sessions,
            Some("test-profile".into()),
        )
        .with_on_session_deleted(move |key| {
            let _ = deleted_tx.send(key.to_owned());
        }),
    );
    let (inbound_tx, mut inbound_rx) = mpsc::channel(8);
    let server = {
        let channel = channel.clone();
        tokio::spawn(async move { channel.start(inbound_tx).await })
    };
    for _ in 0..50 {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let _chat = client
        .post(format!("http://127.0.0.1:{port}/chat"))
        .json(&serde_json::json!({"message":"start", "session_id":"starting-delete", "thread_id":"t"}))
        .send()
        .await
        .unwrap();
    let inbound = tokio::time::timeout(waiting_budget(Duration::from_secs(10)), inbound_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let key = SessionKey::with_profile("test-profile", "api", "starting-delete");
    let admission = octos_bus::Channel::admit_dispatch(&*channel, &inbound, &key)
        .await
        .unwrap();
    let (release, blocked) = std::sync::mpsc::channel();
    factory.queue(&key, FakeBehavior::Block(blocked));
    registry
        .dispatch_admitted(fake_params(&key, "start"), Some(admission))
        .await;
    let response = tokio::time::timeout(
        waiting_budget(Duration::from_secs(2)),
        client
            .delete(format!("http://127.0.0.1:{port}/sessions/starting-delete"))
            .send(),
    )
    .await
    .expect("Starting construction must not hold the API deletion read lease")
    .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::NO_CONTENT);
    let deleted = tokio::time::timeout(waiting_budget(Duration::from_secs(2)), deleted_rx.recv())
        .await
        .unwrap()
        .unwrap();
    registry.remove_session(&deleted).await;
    assert!(next_outbound(&mut out_rx).await.content.contains("deleted"));
    release.send(()).unwrap();
    finish_one(&mut registry).await;
    assert_eq!(registry.len(), 0);
    let mut inbox = factory.take_inbox(&key, 0);
    assert!(matches!(
        inbox.try_recv(),
        Err(mpsc::error::TryRecvError::Disconnected)
    ));
    shutdown.store(true, Ordering::Release);
    server.abort();
}

#[tokio::test]
async fn construction_replays_fifo_before_actor_start_and_bounds_queue() {
    let factory = FakeFactory::default();
    let (mut registry, mut out_rx) = fake_registry(&factory);
    let key = SessionKey("cli:construction-fifo#peer-test".into());
    let (release, blocked) = std::sync::mpsc::channel();
    factory.queue(&key, FakeBehavior::Block(blocked));
    for i in 0..ACTOR_INBOX_SIZE {
        registry
            .dispatch(fake_params(&key, &format!("message-{i}")))
            .await;
    }
    registry.dispatch(fake_params(&key, "overflow")).await;
    let busy = next_outbound(&mut out_rx).await;
    assert!(busy.content.contains("retry"));
    assert_eq!(busy.metadata["retryable"], true);
    release.send(()).unwrap();
    tokio::time::timeout(
        waiting_budget(Duration::from_secs(10)),
        finish_one(&mut registry),
    )
    .await
    .expect("a full Starting queue replays without a closed-gate deadlock");
    let mut inbox = factory.take_inbox(&key, 0);
    for i in 0..ACTOR_INBOX_SIZE {
        assert_eq!(
            actor_text(inbox.recv().await.unwrap()),
            format!("message-{i}")
        );
    }
    let events = factory.events.lock().unwrap();
    let observed = events.iter().map(String::as_str).collect::<Vec<_>>();
    let mut expected = vec!["actor_start"];
    expected.extend(std::iter::repeat_n("queued", ACTOR_INBOX_SIZE));
    expected.extend(["gate_open", "peer_registered"]);
    assert_eq!(observed, expected);
    drop(events);
    let peer_key = peer_inbox_key(MAIN_PROFILE_ID, "test");
    assert!(
        peer_inbox_registry()
            .lock()
            .unwrap()
            .contains_key(&peer_key)
    );
    peer_inbox_registry().lock().unwrap().remove(&peer_key);
}

#[tokio::test]
async fn actor_run_waits_for_start_gate_before_inbox() {
    let dir = tempfile::TempDir::new().unwrap();
    let provider = Arc::new(DelayedMockProvider::new(
        "gated",
        vec![(Duration::from_millis(0), make_response("gated response"))],
    ));
    let (start_tx, start_rx) = oneshot::channel();
    let (inbox_tx, _out_rx, handle, _session_mgr) = setup_actor_with_mode(
        provider.clone(),
        QueueMode::Followup,
        None,
        false,
        &dir,
        Some(start_rx),
        None,
    )
    .await;
    inbox_tx
        .send(make_inbound("queued user message"))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(provider.call_count.load(Ordering::Relaxed), 0);
    start_tx.send(()).unwrap();
    tokio::time::timeout(waiting_budget(Duration::from_secs(10)), async {
        while provider.call_count.load(Ordering::Relaxed) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the queued message reaches the model only after the gate opens");
    handle.abort();
}

#[tokio::test]
async fn publish_panic_fails_queued_turn_retryable() {
    let dir = tempfile::TempDir::new().unwrap();
    let key = test_session_key(dir.path());
    let ticket = ConstructionTicket::register(&key).await.unwrap();
    let provider = Arc::new(DelayedMockProvider::new("publish-panic", vec![]));
    let (start_tx, start_rx) = oneshot::channel();
    let publish: ActorPublish = Box::new(|| panic!("injected publish panic"));
    let (inbox_tx, mut out_rx, handle, _session_mgr) = setup_actor_with_mode(
        provider.clone(),
        QueueMode::Followup,
        None,
        false,
        &dir,
        Some(start_rx),
        Some((publish, ticket, Arc::new(Semaphore::new(1)))),
    )
    .await;
    inbox_tx
        .send(make_inbound("queued during publish"))
        .await
        .unwrap();
    start_tx.send(()).unwrap();
    let error = next_outbound(&mut out_rx).await;
    assert_eq!(error.metadata["error_code"], "session_actor_starting");
    assert_eq!(error.metadata["retryable"], true);
    assert_eq!(error.metadata["truncated"], true);
    assert!(error.content.contains("failed to start"));
    assert_eq!(provider.call_count.load(Ordering::Relaxed), 0);
    tokio::time::timeout(waiting_budget(Duration::from_secs(2)), handle)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn delete_during_publish_cancels_before_model_and_allows_replacement() {
    let dir = tempfile::TempDir::new().unwrap();
    let key = test_session_key(dir.path());
    let ticket = ConstructionTicket::register(&key).await.unwrap();
    let old_ticket = ticket.clone();
    let phase = Arc::new(AtomicUsize::new(0));
    let publish_phase = phase.clone();
    let publish: ActorPublish = Box::new(move || {
        publish_phase.store(1, Ordering::Release);
        while publish_phase.load(Ordering::Acquire) != 2 {
            std::thread::yield_now();
        }
    });
    let provider = Arc::new(DelayedMockProvider::new("publish-delete", vec![]));
    let (start_tx, start_rx) = oneshot::channel();
    let (inbox_tx, mut out_rx, handle, _session_mgr) = setup_actor_with_mode(
        provider.clone(),
        QueueMode::Followup,
        None,
        false,
        &dir,
        Some(start_rx),
        Some((publish, ticket, Arc::new(Semaphore::new(1)))),
    )
    .await;
    inbox_tx
        .send(make_inbound("queued during delete"))
        .await
        .unwrap();
    start_tx.send(()).unwrap();
    tokio::time::timeout(waiting_budget(Duration::from_secs(2)), async {
        while phase.load(Ordering::Acquire) != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let deleting = {
        let key = key.clone();
        tokio::spawn(
            async move { octos_bus::construction_fence::begin_session_deletion(&key).await },
        )
    };
    tokio::task::yield_now().await;
    phase.store(2, Ordering::Release);
    let deletion = tokio::time::timeout(waiting_budget(Duration::from_secs(2)), deleting)
        .await
        .unwrap()
        .unwrap();
    let error = next_outbound(&mut out_rx).await;
    assert_eq!(error.metadata["retryable"], true);
    assert_eq!(error.metadata["truncated"], true);
    assert!(error.content.contains("deleted"));
    assert_eq!(provider.call_count.load(Ordering::Relaxed), 0);
    tokio::time::timeout(waiting_budget(Duration::from_secs(2)), handle)
        .await
        .unwrap()
        .unwrap();
    drop(deletion);
    assert!(old_ticket.is_cancelled());
    let replacement = ConstructionTicket::register(&key).await.unwrap();
    assert!(!replacement.is_cancelled());
}

#[tokio::test]
async fn publish_and_factory_share_construction_permits() {
    let factory = FakeFactory::default();
    let (mut registry, _out_rx) = fake_registry(&factory);
    let mut releases = Vec::new();
    for i in 0..MAX_CONCURRENT_FACTORIES {
        let key = SessionKey(format!("cli:shared-permit-{i}"));
        let (release, blocked) = std::sync::mpsc::channel();
        factory.queue(&key, FakeBehavior::Block(blocked));
        registry.dispatch(fake_params(&key, "first")).await;
        releases.push(release);
    }
    wait_for_factory_entries(&factory, MAX_CONCURRENT_FACTORIES).await;
    let dir = tempfile::TempDir::new().unwrap();
    let key = test_session_key(dir.path());
    let ticket = ConstructionTicket::register(&key).await.unwrap();
    let phase = Arc::new(AtomicUsize::new(0));
    let publish_phase = phase.clone();
    let publish: ActorPublish = Box::new(move || {
        publish_phase.store(1, Ordering::Release);
        while publish_phase.load(Ordering::Acquire) != 2 {
            std::thread::yield_now();
        }
    });
    let provider = Arc::new(DelayedMockProvider::new("shared-permit", vec![]));
    let (start_tx, start_rx) = oneshot::channel();
    let (_inbox_tx, _actor_out_rx, handle, _session_mgr) = setup_actor_with_mode(
        provider,
        QueueMode::Followup,
        None,
        false,
        &dir,
        Some(start_rx),
        Some((publish, ticket, registry.factory_semaphore.clone())),
    )
    .await;
    start_tx.send(()).unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(phase.load(Ordering::Acquire), 0);
    releases[0].send(()).unwrap();
    finish_one(&mut registry).await;
    tokio::time::timeout(waiting_budget(Duration::from_secs(2)), async {
        while phase.load(Ordering::Acquire) != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let extra = SessionKey("cli:shared-permit-extra".into());
    let (extra_release, extra_blocked) = std::sync::mpsc::channel();
    factory.queue(&extra, FakeBehavior::Block(extra_blocked));
    registry.dispatch(fake_params(&extra, "extra")).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(factory.entered(), MAX_CONCURRENT_FACTORIES);
    phase.store(2, Ordering::Release);
    wait_for_factory_entries(&factory, MAX_CONCURRENT_FACTORIES + 1).await;
    handle.abort();
    for release in releases.iter().skip(1) {
        release.send(()).unwrap();
    }
    extra_release.send(()).unwrap();
    for _ in 0..MAX_CONCURRENT_FACTORIES {
        finish_one(&mut registry).await;
    }
}

#[tokio::test]
async fn sixty_fifth_start_is_busy_and_only_eight_factories_run() {
    let factory = FakeFactory::default();
    let (mut registry, mut out_rx) = fake_registry(&factory);
    let mut keys = Vec::new();
    let mut releases = Vec::new();
    for i in 0..MAX_STARTING_ACTORS {
        let key = SessionKey(format!("cli:capacity-{i}"));
        let (release, blocked) = std::sync::mpsc::channel();
        factory.queue(&key, FakeBehavior::Block(blocked));
        registry.dispatch(fake_params(&key, "first")).await;
        keys.push(key);
        releases.push(release);
    }
    wait_for_factory_entries(&factory, MAX_CONCURRENT_FACTORIES).await;
    assert_eq!(
        factory.max_active.load(Ordering::SeqCst),
        MAX_CONCURRENT_FACTORIES
    );
    // A deleted Starting slot no longer appears in the map, but its
    // outstanding factory still consumes global capacity.
    registry.remove_session(&keys[0].0).await;
    assert!(next_outbound(&mut out_rx).await.content.contains("deleted"));
    let overflow = SessionKey("cli:capacity-overflow".into());
    registry.dispatch(fake_params(&overflow, "busy")).await;
    assert!(
        next_outbound(&mut out_rx)
            .await
            .content
            .contains("too many")
    );
    releases[0].send(()).unwrap();
    finish_one(&mut registry).await;
    wait_for_factory_entries(&factory, MAX_CONCURRENT_FACTORIES + 1).await;
    for release in releases.iter().skip(1) {
        release.send(()).unwrap();
        finish_one(&mut registry).await;
    }
    assert_eq!(
        factory.max_active.load(Ordering::SeqCst),
        MAX_CONCURRENT_FACTORIES
    );
}

#[tokio::test]
async fn closed_respawn_respects_outstanding_capacity() {
    let factory = FakeFactory::default();
    let (mut registry, mut out_rx) = fake_registry(&factory);
    let running = SessionKey("cli:closed-respawn".into());
    registry.dispatch(fake_params(&running, "running")).await;
    finish_one(&mut registry).await;
    drop(factory.take_inbox(&running, 0));
    let mut releases = Vec::new();
    for i in 0..MAX_STARTING_ACTORS {
        let key = SessionKey(format!("cli:respawn-capacity-{i}"));
        let (release, blocked) = std::sync::mpsc::channel();
        factory.queue(&key, FakeBehavior::Block(blocked));
        registry.dispatch(fake_params(&key, "first")).await;
        releases.push(release);
    }
    wait_for_factory_entries(&factory, MAX_CONCURRENT_FACTORIES).await;
    let entered_before_respawn = factory.entered();
    registry.dispatch(fake_params(&running, "respawn")).await;
    assert!(
        next_outbound(&mut out_rx)
            .await
            .content
            .contains("too many")
    );
    assert_eq!(factory.entered(), entered_before_respawn);
    for release in releases {
        release.send(()).unwrap();
        finish_one(&mut registry).await;
    }
}

#[tokio::test]
async fn cancelled_parts_fail_queue_and_allow_retry() {
    let factory = FakeFactory::default();
    let (mut registry, mut out_rx) = fake_registry(&factory);
    let key = SessionKey("cli:cancelled-parts".into());
    factory.queue(&key, FakeBehavior::Cancelled);
    registry.dispatch(fake_params(&key, "cancelled")).await;
    finish_one(&mut registry).await;
    let error = next_outbound(&mut out_rx).await;
    assert!(error.content.contains("cancelled"));
    assert_eq!(error.metadata["retryable"], true);
    assert_eq!(registry.len(), 0);
    registry.dispatch(fake_params(&key, "retry")).await;
    finish_one(&mut registry).await;
    assert_eq!(registry.len(), 1);
    assert_eq!(
        actor_text(factory.take_inbox(&key, 0).recv().await.unwrap()),
        "retry"
    );
}

#[tokio::test]
async fn factory_panic_fails_queue_and_dispatcher_recovers() {
    let factory = FakeFactory::default();
    let (mut registry, mut out_rx) = fake_registry(&factory);
    let key = SessionKey("cli:factory-panic".into());
    let other = SessionKey("cli:factory-survivor".into());
    factory.queue(&key, FakeBehavior::Panic);
    registry.dispatch(fake_params(&key, "panic")).await;
    finish_one(&mut registry).await;
    assert!(
        next_outbound(&mut out_rx)
            .await
            .content
            .contains("failed to start")
    );
    assert_eq!(registry.len(), 0);
    registry.dispatch(fake_params(&key, "retry")).await;
    registry.dispatch(fake_params(&other, "other")).await;
    finish_one(&mut registry).await;
    finish_one(&mut registry).await;
    assert_eq!(registry.len(), 2);
    assert_eq!(
        actor_text(factory.take_inbox(&key, 0).recv().await.unwrap()),
        "retry"
    );
    assert_eq!(
        actor_text(factory.take_inbox(&other, 0).recv().await.unwrap()),
        "other"
    );
}

#[tokio::test]
async fn shutdown_fails_starting_queue_and_discards_late_parts() {
    let factory = FakeFactory::default();
    let (mut registry, mut out_rx) = fake_registry(&factory);
    let key = SessionKey("cli:starting-shutdown".into());
    let (release, blocked) = std::sync::mpsc::channel();
    factory.queue(&key, FakeBehavior::Block(blocked));
    registry.dispatch(fake_params(&key, "pending")).await;
    wait_for_factory_entries(&factory, 1).await;
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        release.send(()).unwrap();
    });
    tokio::time::timeout(
        waiting_budget(Duration::from_secs(10)),
        registry.shutdown_all(),
    )
    .await
    .expect("shutdown drains the in-flight construction");
    assert!(
        next_outbound(&mut out_rx)
            .await
            .content
            .contains("shutting down")
    );
    let gate = factory.take_gate(&key, 0);
    assert!(
        tokio::time::timeout(waiting_budget(Duration::from_secs(2)), gate)
            .await
            .unwrap()
            .is_err()
    );
}

#[tokio::test]
async fn stale_generation_never_consumes_replacement_queue() {
    let factory = FakeFactory::default();
    let (mut registry, mut out_rx) = fake_registry(&factory);
    let key = SessionKey("cli:stale-generation".into());
    let (release_old, old_blocked) = std::sync::mpsc::channel();
    factory.queue(&key, FakeBehavior::Block(old_blocked));
    registry.dispatch(fake_params(&key, "old")).await;
    wait_for_factory_entries(&factory, 1).await;
    registry.remove_session(&key.0).await;
    next_outbound(&mut out_rx).await;
    let (release_new, new_blocked) = std::sync::mpsc::channel();
    factory.queue(&key, FakeBehavior::Block(new_blocked));
    registry.dispatch(fake_params(&key, "new")).await;
    wait_for_factory_entries(&factory, 2).await;
    release_old.send(()).unwrap();
    finish_one(&mut registry).await;
    let mut old_inbox = factory.take_inbox(&key, 0);
    assert!(matches!(
        old_inbox.try_recv(),
        Err(mpsc::error::TryRecvError::Disconnected)
    ));
    registry.dispatch(fake_params(&key, "after")).await;
    release_new.send(()).unwrap();
    finish_one(&mut registry).await;
    let mut inbox = factory.take_inbox(&key, 0);
    assert_eq!(actor_text(inbox.recv().await.unwrap()), "new");
    assert_eq!(actor_text(inbox.recv().await.unwrap()), "after");
}
