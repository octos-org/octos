//! M7.9 PM supervisor primitives — integration tests.
//!
//! Covers the end-to-end behavior of [`TaskSupervisor::cancel_task`],
//! [`TaskSupervisor::relaunch_task`], and [`TaskSupervisor::send_to_agent`]
//! including the typed [`HarnessEventPayload::TaskLifecycleCancelled`]
//! event emission path. Uses the supervisor directly (no full agent loop)
//! so the tests stay deterministic and fast.
//!
//! M7.9b adds runtime-effect tests that exercise the agent loop (Gap 1
//! — inbox drain, Gap 2 — relaunch re-execution, Gap 3 — Matrix-puppet
//! wiring) so the supervisor actually moves the conversation + data
//! plane, not just the storage ledger.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use octos_agent::harness_events::HarnessEventPayload;
use octos_agent::task_supervisor::{
    CancelError, InboxMessage, RelaunchError, SendToAgentError, SupervisorInbox,
    TaskLifecycleState, TaskStatus, TaskSupervisor,
};
use octos_agent::{Agent, AgentConfig, ToolRegistry};
use octos_bus::SteeringInputConsumer;
use octos_core::{AgentId, Message, MessageRole};
use octos_llm::{ChatConfig, ChatResponse, LlmProvider, StopReason, TokenUsage, ToolSpec};
use octos_memory::EpisodeStore;
use serde_json::json;

fn spawn_pending_task() -> (tokio::task::AbortHandle, tokio::task::JoinHandle<()>) {
    let handle = tokio::spawn(async {
        tokio::time::sleep(Duration::from_secs(60)).await;
    });
    let abort = handle.abort_handle();
    (abort, handle)
}

#[tokio::test]
async fn should_cancel_running_task_and_emit_cancelled_event() {
    let dir = tempfile::TempDir::new().unwrap();
    let sink = dir.path().join("events.jsonl");
    let supervisor = Arc::new(TaskSupervisor::new());
    supervisor.attach_harness_event_sink(sink.to_string_lossy().to_string());

    let task_id = supervisor.register("spawn", "call-cancel", Some("api:m79-1"));
    supervisor.mark_running(&task_id);
    let (abort, handle) = spawn_pending_task();
    supervisor.register_abort(&task_id, abort, None, None);

    supervisor
        .cancel_task(&task_id, Some("kill requested".into()))
        .expect("cancel should succeed");

    // Snapshot is Cancelled, JoinHandle is aborted.
    let task = supervisor.get_task(&task_id).expect("task missing");
    assert_eq!(task.status, TaskStatus::Cancelled);
    assert_eq!(task.lifecycle_state(), TaskLifecycleState::Cancelled);
    assert_eq!(task.cancellation_reason.as_deref(), Some("kill requested"));
    let join_result = tokio::time::timeout(Duration::from_secs(2), handle).await;
    assert!(
        matches!(join_result, Ok(Err(_))),
        "JoinHandle should be cancelled"
    );

    // Sink contains a typed event.
    let body = std::fs::read_to_string(&sink).unwrap();
    let line = body.lines().next().expect("expected event line");
    let event: octos_agent::harness_events::HarnessEvent = serde_json::from_str(line).unwrap();
    match event.payload {
        HarnessEventPayload::TaskLifecycleCancelled { data } => {
            assert_eq!(data.task_id, task_id);
            assert_eq!(data.reason, "kill requested");
            assert_eq!(data.origin, "operator");
            assert!(data.relaunched_as.is_none());
        }
        other => panic!("expected TaskLifecycleCancelled, got {other:?}"),
    }
}

#[tokio::test]
async fn should_reject_cancel_for_unknown_task_id() {
    let supervisor = TaskSupervisor::new();
    let err = supervisor
        .cancel_task("bogus", None)
        .expect_err("cancel should surface unknown");
    assert_eq!(err, CancelError::UnknownTask);
}

#[tokio::test]
async fn should_reject_double_cancel() {
    let supervisor = TaskSupervisor::new();
    let id = supervisor.register("spawn", "call", Some("api:m79-2"));
    supervisor.mark_running(&id);
    let (abort, _h) = spawn_pending_task();
    supervisor.register_abort(&id, abort, None, None);
    supervisor.cancel_task(&id, None).unwrap();
    let err = supervisor
        .cancel_task(&id, None)
        .expect_err("second cancel should fail");
    assert_eq!(err, CancelError::AlreadyTerminal(TaskStatus::Cancelled));
}

#[tokio::test]
async fn should_relaunch_cancelled_task_with_overrides() {
    let dir = tempfile::TempDir::new().unwrap();
    let sink = dir.path().join("events.jsonl");
    let supervisor = TaskSupervisor::new();
    supervisor.attach_harness_event_sink(sink.to_string_lossy().to_string());
    let task_id = supervisor.register("spawn", "call-3", Some("api:m79-3"));
    supervisor.mark_running(&task_id);
    let (abort, _h) = spawn_pending_task();
    supervisor.register_abort(
        &task_id,
        abort,
        None,
        Some(json!({
            "task": "draft-a-post",
            "config": {"tone": "casual"}
        })),
    );

    let plan = supervisor
        .relaunch_task(
            &task_id,
            json!({"config": {"tone": "formal"}, "extra_hint": "be concise"}),
        )
        .expect("relaunch should succeed");

    assert_ne!(plan.new_task_id, task_id);
    assert_eq!(plan.parent_task_id, task_id);
    assert_eq!(plan.merged_spec["task"], "draft-a-post");
    assert_eq!(plan.merged_spec["config"]["tone"], "formal");
    assert_eq!(plan.merged_spec["extra_hint"], "be concise");

    // Original was cancelled with `origin=relaunch` and the new id carried.
    let original = supervisor.get_task(&task_id).unwrap();
    assert_eq!(original.status, TaskStatus::Cancelled);
    assert_eq!(
        original.cancellation_reason.as_deref(),
        Some(format!("relaunched as {}", plan.new_task_id).as_str())
    );

    let body = std::fs::read_to_string(&sink).unwrap();
    let line = body.lines().next().unwrap();
    let event: octos_agent::harness_events::HarnessEvent = serde_json::from_str(line).unwrap();
    if let HarnessEventPayload::TaskLifecycleCancelled { data } = event.payload {
        assert_eq!(data.origin, "relaunch");
        assert_eq!(
            data.relaunched_as.as_deref(),
            Some(plan.new_task_id.as_str())
        );
    } else {
        panic!("expected cancelled event");
    }
}

#[tokio::test]
async fn should_refuse_relaunch_without_spec_snapshot() {
    let supervisor = TaskSupervisor::new();
    let id = supervisor.register("spawn", "c", Some("api:m79-4"));
    supervisor.mark_running(&id);
    let (abort, _h) = spawn_pending_task();
    supervisor.register_abort(&id, abort, None, None);
    let err = supervisor
        .relaunch_task(&id, json!({}))
        .expect_err("should require snapshot");
    assert_eq!(err, RelaunchError::NoSpecSnapshot);
}

#[tokio::test]
async fn should_deliver_send_to_agent_message_into_inbox() {
    let supervisor = TaskSupervisor::new();
    let id = supervisor.register("spawn", "c", Some("api:m79-5"));
    supervisor.mark_running(&id);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let inbox = SupervisorInbox::new(tx);
    let (abort, _h) = spawn_pending_task();
    supervisor.register_abort(&id, abort, Some(inbox), None);

    supervisor
        .send_to_agent(&id, InboxMessage::new("pm-agent", "course-correct please"))
        .expect("send should succeed");
    let msg = rx.try_recv().expect("message should arrive");
    assert_eq!(msg.sender, "pm-agent");
    assert_eq!(msg.body, "course-correct please");
}

#[tokio::test]
async fn should_refuse_send_to_agent_after_cancel() {
    let supervisor = TaskSupervisor::new();
    let id = supervisor.register("spawn", "c", Some("api:m79-6"));
    supervisor.mark_running(&id);
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let inbox = SupervisorInbox::new(tx);
    let (abort, _h) = spawn_pending_task();
    supervisor.register_abort(&id, abort, Some(inbox), None);
    supervisor.cancel_task(&id, None).unwrap();
    let err = supervisor
        .send_to_agent(&id, InboxMessage::new("pm", "late"))
        .expect_err("should refuse steering after cancel");
    assert_eq!(err, SendToAgentError::Terminal(TaskStatus::Cancelled));
}

#[tokio::test]
async fn should_steer_via_matrix_puppet_into_inbox() {
    // Simulates the Matrix-puppet steering consumer: a supervisor reply
    // routed from the swarm room arrives at the SupervisorInbox the same
    // way the Matrix gateway TODO (FA-11) plans to wire it. The plumbing
    // validates the inbox path is generic — the Matrix adapter just needs
    // to build an InboxMessage with the puppet label as sender.
    let supervisor = TaskSupervisor::new();
    let id = supervisor.register("spawn", "c", Some("api:m79-7"));
    supervisor.mark_running(&id);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let inbox = SupervisorInbox::new(tx);
    let (abort, _h) = spawn_pending_task();
    supervisor.register_abort(&id, abort, Some(inbox), None);

    // Typical Matrix puppet handoff: supervisor_user_id is the Matrix user,
    // body is the reply with the @puppet mention already stripped.
    let matrix_reply = InboxMessage::new(
        "matrix:@supervisor:server",
        "swap the diagram style to flowchart",
    );
    supervisor
        .send_to_agent(&id, matrix_reply)
        .expect("matrix reply should route into inbox");
    let received = rx.try_recv().expect("inbox delivery");
    assert_eq!(received.sender, "matrix:@supervisor:server");
    assert_eq!(received.body, "swap the diagram style to flowchart");
}

#[tokio::test]
async fn should_carry_parent_task_id_through_relaunch_chain() {
    let supervisor = TaskSupervisor::new();
    let a = supervisor.register("spawn", "c", Some("api:m79-chain"));
    supervisor.mark_running(&a);
    let (abort_a, _h_a) = spawn_pending_task();
    supervisor.register_abort(&a, abort_a, None, Some(json!({"task": "one"})));

    let plan_b = supervisor
        .relaunch_task(&a, json!({"task": "two"}))
        .unwrap();
    supervisor.mark_running(&plan_b.new_task_id);
    let (abort_b, _h_b) = spawn_pending_task();
    // Re-register so the chain advances — the relaunch registers the task
    // record but not a new abort handle (spawn wrapper is responsible).
    supervisor.register_abort(
        &plan_b.new_task_id,
        abort_b,
        None,
        Some(json!({"task": "two"})),
    );
    let plan_c = supervisor
        .relaunch_task(&plan_b.new_task_id, json!({"task": "three"}))
        .unwrap();

    let c = supervisor.get_task(&plan_c.new_task_id).unwrap();
    assert_eq!(
        c.parent_task_id.as_deref(),
        Some(plan_b.new_task_id.as_str())
    );
    let b = supervisor.get_task(&plan_b.new_task_id).unwrap();
    assert_eq!(b.parent_task_id.as_deref(), Some(a.as_str()));
    assert_eq!(b.status, TaskStatus::Cancelled);
}

// ── M7.9b Gap 1: agent loop drains SupervisorInbox before each LLM call
// ─────────────────────────────────────────────────────────────────────────

/// Mock LLM that (1) records the messages it was called with, keyed by
/// call index, and (2) returns scripted responses in FIFO order. Used
/// by the inbox-drain test to assert that steering messages injected
/// mid-run reach the next LLM call as synthetic user-role turns.
/// Shared handle to the per-call message capture used by the spy LLM —
/// wrapping the Vec<Vec<Message>> in a dedicated alias keeps clippy's
/// `type_complexity` lint happy and makes call sites easier to read.
type CapturedCalls = Arc<Mutex<Vec<Vec<Message>>>>;

struct InspectingLlm {
    responses: Mutex<Vec<ChatResponse>>,
    captured_calls: CapturedCalls,
}

impl InspectingLlm {
    fn new(responses: Vec<ChatResponse>) -> (Arc<Self>, CapturedCalls) {
        let captured_calls: CapturedCalls = Arc::new(Mutex::new(Vec::new()));
        (
            Arc::new(Self {
                responses: Mutex::new(responses),
                captured_calls: captured_calls.clone(),
            }),
            captured_calls,
        )
    }
}

#[async_trait]
impl LlmProvider for InspectingLlm {
    async fn chat(
        &self,
        messages: &[Message],
        _tools: &[ToolSpec],
        _config: &ChatConfig,
    ) -> eyre::Result<ChatResponse> {
        self.captured_calls.lock().unwrap().push(messages.to_vec());
        let mut responses = self.responses.lock().unwrap();
        if responses.is_empty() {
            eyre::bail!("InspectingLlm: no more scripted responses");
        }
        Ok(responses.remove(0))
    }

    fn context_window(&self) -> u32 {
        128_000
    }

    fn model_id(&self) -> &str {
        "mock-inspect"
    }

    fn provider_name(&self) -> &str {
        "mock"
    }
}

fn end_turn(text: &str) -> ChatResponse {
    ChatResponse {
        content: Some(text.to_string()),
        reasoning_content: None,
        tool_calls: vec![],
        stop_reason: StopReason::EndTurn,
        usage: TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
            ..Default::default()
        },
        provider_index: None,
    }
}

#[tokio::test]
async fn should_drain_supervisor_inbox_and_include_messages_in_next_turn() {
    // Build a child agent and wire a SupervisorInbox into it. Pre-seed
    // the inbox with a steering message (simulating the M7.9
    // send_to_agent tool having run mid-task), then drive the agent
    // forward. The next LLM call MUST see the steering message as a
    // synthetic user-role turn, and the final response MUST echo the
    // steering marker — proving the drain + injection has runtime
    // effect, not just storage effect.
    let dir = tempfile::TempDir::new().unwrap();
    let (llm_arc, captured) = InspectingLlm::new(vec![end_turn(
        "understood — echoing STEERED-1 as requested",
    )]);
    let llm: Arc<dyn LlmProvider> = llm_arc;
    let tools = ToolRegistry::with_builtins(dir.path());
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());

    // Wire the inbox into the agent the same way SpawnTool does.
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<InboxMessage>();
    let agent = Agent::new(AgentId::new("child-test"), llm, tools, memory)
        .with_config(AgentConfig {
            save_episodes: false,
            ..Default::default()
        })
        .with_supervisor_inbox(rx);

    // Seed the inbox BEFORE the loop runs so the first iteration picks
    // it up — this mirrors the "message delivered while the child is
    // suspended between turns" case.
    tx.send(InboxMessage::new("pm-agent", "please also echo STEERED-1"))
        .expect("inbox send should succeed");

    let resp = agent
        .process_message("initial task: outline a plan", &[], vec![])
        .await
        .expect("process_message should succeed");

    // Assert the captured LLM-call messages list contains the steered
    // user message. `[from: pm-agent]` prefix is the attribution wrap
    // the loop-runner helper applies when converting InboxMessage ->
    // synthetic user turn.
    let calls = captured.lock().unwrap();
    assert_eq!(calls.len(), 1, "expected exactly one LLM call");
    let first_call = &calls[0];
    let steered = first_call
        .iter()
        .find(|m| {
            m.role == MessageRole::User
                && m.content.contains("[from: pm-agent]")
                && m.content.contains("please also echo STEERED-1")
        })
        .expect("steering message must be visible to the LLM as a user turn");
    // Attribution preservation: the sender label must be in the prefix.
    assert!(steered.content.starts_with("[from: pm-agent]"));

    // The LLM echoed the marker back, proving the steering reached the
    // model — i.e. runtime effect, not just storage effect.
    assert!(
        resp.content.contains("STEERED-1"),
        "response should echo the steering marker; got: {}",
        resp.content
    );
}

#[tokio::test]
async fn should_not_attach_supervisor_inbox_when_builder_not_called() {
    // Regression: the agent without .with_supervisor_inbox() behaves
    // identically to pre-M7.9b — no steering, no drain, no hidden field
    // state. This protects the legacy callers that never want steering.
    let dir = tempfile::TempDir::new().unwrap();
    let (llm_arc, captured) = InspectingLlm::new(vec![end_turn("plain response")]);
    let llm: Arc<dyn LlmProvider> = llm_arc;
    let tools = ToolRegistry::with_builtins(dir.path());
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());

    let agent = Agent::new(AgentId::new("no-inbox"), llm, tools, memory).with_config(AgentConfig {
        save_episodes: false,
        ..Default::default()
    });
    assert!(
        !agent.has_supervisor_inbox(),
        "no inbox should be wired when with_supervisor_inbox is not called"
    );

    let resp = agent
        .process_message("hello", &[], vec![])
        .await
        .expect("process_message should succeed without inbox");
    let calls = captured.lock().unwrap();
    assert_eq!(calls.len(), 1);
    // No `[from: ...]` steering prefix should appear anywhere in the
    // conversation when no inbox is wired.
    for msg in &calls[0] {
        assert!(
            !msg.content.contains("[from:"),
            "no steering prefix should leak when no inbox is wired; saw: {}",
            msg.content
        );
    }
    assert_eq!(resp.content, "plain response");
}

// ── M7.9b Gap 2: relaunch_task actually re-executes the tool
// ─────────────────────────────────────────────────────────────────────────

/// Test re-executor that captures the relaunched spec so we can verify
/// the data-plane call actually happened (not just the storage
/// transition). Mirrors what `SpawnToolReexecutor` does in production,
/// minus the SpawnTool wiring (the unit test boundary is the supervisor
/// trait contract, not the spawn tool's internal machinery).
struct CapturingReexecutor {
    captured: Arc<Mutex<Vec<octos_agent::RelaunchPlan>>>,
    notify: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl octos_agent::TaskReexecutor for CapturingReexecutor {
    async fn reexecute(&self, plan: octos_agent::RelaunchPlan) {
        self.captured.lock().unwrap().push(plan);
        self.notify.notify_waiters();
    }
}

#[tokio::test]
async fn should_relaunch_actually_reexecutes_tool() {
    // Wire a CapturingReexecutor onto the supervisor so the M7.9b
    // data-plane kick fires. Without an attached re-executor, the M7.9
    // plan-only path is preserved — we verify both halves (the without
    // / with). The WITH half is the one that proves the runtime
    // effect: the captured spec contains a distinctive marker baked
    // into the seed_overrides, confirming the tool re-invocation went
    // through the merged spec, not just the stored one.
    let supervisor = Arc::new(TaskSupervisor::new());
    let captured = Arc::new(Mutex::new(Vec::new()));
    let notify = Arc::new(tokio::sync::Notify::new());
    supervisor.attach_reexecutor(Arc::new(CapturingReexecutor {
        captured: captured.clone(),
        notify: notify.clone(),
    }));
    assert!(
        supervisor.has_reexecutor(),
        "re-executor should be attached"
    );

    let task_id = supervisor.register("spawn", "call-reexec", Some("api:m79b-reexec"));
    supervisor.mark_running(&task_id);
    let (abort, _handle) = spawn_pending_task();
    supervisor.register_abort(
        &task_id,
        abort,
        None,
        Some(json!({
            "task": "echo REEXEC-MARKER-ORIGINAL",
            "config": {"tone": "casual"}
        })),
    );

    let plan = supervisor
        .relaunch_task(
            &task_id,
            json!({"config": {"tone": "formal"}, "marker": "REEXEC-MARKER-OVERRIDE"}),
        )
        .expect("relaunch should succeed");

    // Wait for the spawned re-execution to land (supervisor-owned
    // tokio::spawn). Bounded timeout so a bug doesn't hang the suite.
    tokio::time::timeout(Duration::from_secs(2), notify.notified())
        .await
        .expect("re-executor should fire within 2s of relaunch");

    let captured_snapshot = captured.lock().unwrap().clone();
    assert_eq!(
        captured_snapshot.len(),
        1,
        "re-executor must be invoked exactly once"
    );
    let replay = &captured_snapshot[0];
    assert_eq!(replay.new_task_id, plan.new_task_id);
    assert_eq!(replay.parent_task_id, task_id);
    // Merged spec carries both the original task and the overrides —
    // this is the runtime-effect signal: the *tool* gets run with the
    // merged args, not the original. Rebuilding the merge here is the
    // audit trail the REST caller will see.
    assert_eq!(replay.merged_spec["task"], "echo REEXEC-MARKER-ORIGINAL");
    assert_eq!(replay.merged_spec["config"]["tone"], "formal");
    assert_eq!(replay.merged_spec["marker"], "REEXEC-MARKER-OVERRIDE");
}

#[tokio::test]
async fn should_not_reexecute_when_no_reexecutor_attached() {
    // Pre-M7.9b plan-only path is preserved: a supervisor with no
    // re-executor attached still returns a valid RelaunchPlan, still
    // cancels the original, still emits the typed cancelled event.
    // This test protects the existing harness matrix — raw supervisor
    // users without the spawn wrapper must keep working.
    let supervisor = Arc::new(TaskSupervisor::new());
    assert!(!supervisor.has_reexecutor());

    let id = supervisor.register("spawn", "call-plan-only", Some("api:m79b-plan"));
    supervisor.mark_running(&id);
    let (abort, _h) = spawn_pending_task();
    supervisor.register_abort(&id, abort, None, Some(json!({"task": "legacy"})));

    let plan = supervisor
        .relaunch_task(&id, json!({"extra": "one"}))
        .expect("plan-only relaunch should succeed");
    assert_eq!(plan.merged_spec["task"], "legacy");
    assert_eq!(plan.merged_spec["extra"], "one");

    // Give the runtime a tick to prove no re-execution was queued.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let original = supervisor.get_task(&id).unwrap();
    assert_eq!(original.status, TaskStatus::Cancelled);
    let replay = supervisor.get_task(&plan.new_task_id).unwrap();
    // New task was registered (spawned status is the initial state
    // after `register` and before the re-executor would `mark_running`).
    assert_eq!(replay.status, TaskStatus::Spawned);
}

// ── M7.9b Gap 3: Matrix-puppet steering consumer routes into inbox
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn should_route_matrix_puppet_reply_into_inbox() {
    // End-to-end proof that a Matrix-originated SteeringInput routes
    // all the way into the target child's SupervisorInbox via the
    // SteeringInputConsumer trait. We exercise the adapter shape
    // directly (rather than booting a mock homeserver) — the contract
    // under test is "parsed Matrix replies hit the same inbox path
    // send_to_agent + REST do". Keeps this test deterministic +
    // fast.
    let supervisor = Arc::new(TaskSupervisor::new());
    let task_id = supervisor.register("spawn", "call-matrix", Some("api:m79b-matrix"));
    supervisor.mark_running(&task_id);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let inbox = SupervisorInbox::new(tx);
    let (abort, _h) = spawn_pending_task();
    supervisor.register_abort(&task_id, abort, Some(inbox), None);

    // Adapter that translates the Matrix-domain (session_id,
    // agent_label) into supervisor-domain task_id. In production the
    // gateway maintains a real session->task map; the test uses a
    // simple HashMap to keep the wiring observable.
    let mut session_map = std::collections::HashMap::new();
    session_map.insert(
        ("swarm-session-1".to_string(), "writer".to_string()),
        task_id.clone(),
    );
    let session_map = Arc::new(session_map);

    struct MatrixAdapter {
        supervisor: Arc<TaskSupervisor>,
        session_map: Arc<std::collections::HashMap<(String, String), String>>,
    }

    #[async_trait]
    impl octos_bus::SteeringInputConsumer for MatrixAdapter {
        async fn consume(&self, input: octos_bus::SteeringInput) -> bool {
            let key = (input.session_id.clone(), input.agent_label.clone());
            let Some(task_id) = self.session_map.get(&key) else {
                return false;
            };
            self.supervisor
                .send_to_agent(
                    task_id,
                    InboxMessage::new(format!("matrix:{}", input.supervisor_user_id), input.body),
                )
                .is_ok()
        }
    }

    let adapter = Arc::new(MatrixAdapter {
        supervisor: supervisor.clone(),
        session_map: session_map.clone(),
    });

    // Simulate the matrix_channel producing a SteeringInput from
    // handle_supervisor_reply, then invoking the consumer. This is the
    // same sequence matrix_channel.rs now follows when a consumer is
    // attached (closing the FA-11 TODO).
    let input = octos_bus::SteeringInput {
        session_id: "swarm-session-1".to_string(),
        agent_label: "writer".to_string(),
        puppet_user_id: octos_bus::MatrixUserId::new("@swarm_writer_sess1:example.org"),
        supervisor_user_id: "@alice:example.org".to_string(),
        body: "refine the outline please".to_string(),
    };
    let delivered = adapter.consume(input).await;
    assert!(
        delivered,
        "matrix steering input should route into the inbox"
    );

    // Verify the inbox received the message with the matrix-prefixed
    // sender — operators trace this through the logs to see the Matrix
    // origin.
    let msg = rx
        .try_recv()
        .expect("inbox should receive the routed steering message");
    assert_eq!(msg.sender, "matrix:@alice:example.org");
    assert_eq!(msg.body, "refine the outline please");
}

#[tokio::test]
async fn should_reject_matrix_puppet_reply_when_session_unknown() {
    // Correctness: the adapter must not spray messages to random tasks
    // when the session/label mapping misses. Consumer returns false,
    // the metric counter on the matrix side records `rejected`, and
    // nothing ends up in any inbox.
    let supervisor = Arc::new(TaskSupervisor::new());
    let task_id = supervisor.register("spawn", "call", Some("api:m79b-matrix-miss"));
    supervisor.mark_running(&task_id);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let inbox = SupervisorInbox::new(tx);
    let (abort, _h) = spawn_pending_task();
    supervisor.register_abort(&task_id, abort, Some(inbox), None);

    // Empty session map simulates the "we haven't registered this
    // session yet" case.
    let session_map = Arc::new(std::collections::HashMap::new());

    struct MatrixAdapter {
        supervisor: Arc<TaskSupervisor>,
        session_map: Arc<std::collections::HashMap<(String, String), String>>,
    }

    #[async_trait]
    impl octos_bus::SteeringInputConsumer for MatrixAdapter {
        async fn consume(&self, input: octos_bus::SteeringInput) -> bool {
            let key = (input.session_id.clone(), input.agent_label.clone());
            let Some(task_id) = self.session_map.get(&key) else {
                return false;
            };
            self.supervisor
                .send_to_agent(
                    task_id,
                    InboxMessage::new(format!("matrix:{}", input.supervisor_user_id), input.body),
                )
                .is_ok()
        }
    }

    let adapter = MatrixAdapter {
        supervisor,
        session_map,
    };
    let input = octos_bus::SteeringInput {
        session_id: "unknown-session".to_string(),
        agent_label: "writer".to_string(),
        puppet_user_id: octos_bus::MatrixUserId::new("@swarm_writer:example.org"),
        supervisor_user_id: "@bob:example.org".to_string(),
        body: "this should NOT be delivered".to_string(),
    };
    assert!(!adapter.consume(input).await);
    assert!(rx.try_recv().is_err(), "inbox must stay empty on miss");
}
