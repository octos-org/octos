//! M7.9 PM supervisor primitives — integration tests.
//!
//! Covers the end-to-end behavior of [`TaskSupervisor::cancel_task`],
//! [`TaskSupervisor::relaunch_task`], and [`TaskSupervisor::send_to_agent`]
//! including the typed [`HarnessEventPayload::TaskLifecycleCancelled`]
//! event emission path. Uses the supervisor directly (no full agent loop)
//! so the tests stay deterministic and fast.

use std::sync::Arc;
use std::time::Duration;

use octos_agent::harness_events::HarnessEventPayload;
use octos_agent::task_supervisor::{
    CancelError, InboxMessage, RelaunchError, SendToAgentError, SupervisorInbox, TaskLifecycleState,
    TaskStatus, TaskSupervisor,
};
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
    assert!(matches!(join_result, Ok(Err(_))), "JoinHandle should be cancelled");

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
    let err = supervisor.cancel_task(&id, None).expect_err("second cancel should fail");
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
        assert_eq!(data.relaunched_as.as_deref(), Some(plan.new_task_id.as_str()));
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

    let plan_b = supervisor.relaunch_task(&a, json!({"task": "two"})).unwrap();
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
    assert_eq!(c.parent_task_id.as_deref(), Some(plan_b.new_task_id.as_str()));
    let b = supervisor.get_task(&plan_b.new_task_id).unwrap();
    assert_eq!(b.parent_task_id.as_deref(), Some(a.as_str()));
    assert_eq!(b.status, TaskStatus::Cancelled);
}
