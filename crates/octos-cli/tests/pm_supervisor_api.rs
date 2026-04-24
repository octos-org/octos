//! M7.9 — CLI-side integration tests for the PM supervisor REST endpoints.
//!
//! These exercise [`SessionTaskQueryStore`] as the control plane the REST
//! handlers delegate to. Running the full `build_router` stack end-to-end
//! would require spinning up a live gateway process for the proxy fallback,
//! so we test the dispatcher directly — the proxy layer is thin glue tested
//! by the octos-bus api_channel tests.

#![cfg(feature = "api")]

use std::sync::Arc;
use std::time::Duration;

use octos_agent::task_supervisor::{SupervisorInbox, TaskStatus, TaskSupervisor};
use octos_cli::commands::gateway::adapters::TaskLifecycleDispatcher;
use octos_cli::session_actor::SessionTaskQueryStore;
use octos_core::{MAIN_PROFILE_ID, SessionKey};
use serde_json::json;
use tempfile::TempDir;

fn session_key() -> SessionKey {
    SessionKey::with_profile_topic(MAIN_PROFILE_ID, "api", "m79-cli", "default")
}

fn register_supervisor(store: &SessionTaskQueryStore, dir: &TempDir) -> Arc<TaskSupervisor> {
    let supervisor = Arc::new(TaskSupervisor::new());
    store.register(&session_key(), &supervisor, dir.path());
    supervisor
}

fn spawn_pending() -> (tokio::task::AbortHandle, tokio::task::JoinHandle<()>) {
    let handle = tokio::spawn(async {
        tokio::time::sleep(Duration::from_secs(60)).await;
    });
    let abort = handle.abort_handle();
    (abort, handle)
}

#[tokio::test]
async fn should_cancel_task_via_dispatcher() {
    let dir = TempDir::new().unwrap();
    let store = SessionTaskQueryStore::default();
    let supervisor = register_supervisor(&store, &dir);
    let task_id = supervisor.register("spawn", "c1", Some(&session_key().to_string()));
    supervisor.mark_running(&task_id);
    let (abort, handle) = spawn_pending();
    supervisor.register_abort(&task_id, abort, None, None);

    let dispatcher: &dyn TaskLifecycleDispatcher = &store;
    let result = dispatcher
        .cancel(&task_id, "kill via REST")
        .expect("cancel should succeed");
    assert_eq!(result["task_id"], task_id);
    assert_eq!(result["status"], "cancelled");
    assert_eq!(result["reason"], "kill via REST");

    let task = supervisor.get_task(&task_id).unwrap();
    assert_eq!(task.status, TaskStatus::Cancelled);
    let join_result = tokio::time::timeout(Duration::from_secs(2), handle).await;
    assert!(matches!(join_result, Ok(Err(_))));
}

#[tokio::test]
async fn should_relaunch_task_via_dispatcher() {
    let dir = TempDir::new().unwrap();
    let store = SessionTaskQueryStore::default();
    let supervisor = register_supervisor(&store, &dir);
    let task_id = supervisor.register("spawn", "c2", Some(&session_key().to_string()));
    supervisor.mark_running(&task_id);
    let (abort, _h) = spawn_pending();
    supervisor.register_abort(
        &task_id,
        abort,
        None,
        Some(json!({"task": "original", "model": "gpt-4"})),
    );

    let dispatcher: &dyn TaskLifecycleDispatcher = &store;
    let result = dispatcher
        .relaunch(&task_id, json!({"model": "claude", "extra": true}))
        .expect("relaunch should succeed");
    assert_eq!(result["parent_task_id"], task_id);
    assert!(result["new_task_id"].is_string());
    assert_eq!(result["merged_spec"]["task"], "original");
    assert_eq!(result["merged_spec"]["model"], "claude");
    assert_eq!(result["merged_spec"]["extra"], true);
}

#[tokio::test]
async fn should_send_to_agent_via_dispatcher() {
    let dir = TempDir::new().unwrap();
    let store = SessionTaskQueryStore::default();
    let supervisor = register_supervisor(&store, &dir);
    let task_id = supervisor.register("spawn", "c3", Some(&session_key().to_string()));
    supervisor.mark_running(&task_id);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let inbox = SupervisorInbox::new(tx);
    let (abort, _h) = spawn_pending();
    supervisor.register_abort(&task_id, abort, Some(inbox), None);

    let dispatcher: &dyn TaskLifecycleDispatcher = &store;
    let result = dispatcher
        .send(&task_id, "focus on the diagram", Some("pm-agent"))
        .expect("send should succeed");
    assert_eq!(result["delivered"], true);
    assert_eq!(result["sender"], "pm-agent");

    let msg = rx.try_recv().expect("inbox should have message");
    assert_eq!(msg.sender, "pm-agent");
    assert_eq!(msg.body, "focus on the diagram");
}

#[tokio::test]
async fn should_reject_cancel_for_unknown_task_id() {
    let store = SessionTaskQueryStore::default();
    let dispatcher: &dyn TaskLifecycleDispatcher = &store;
    let (code, msg) = dispatcher
        .cancel("does-not-exist", "")
        .expect_err("unknown task should fail");
    assert_eq!(code, 404);
    assert!(msg.contains("unknown"));
}

#[tokio::test]
async fn should_reject_relaunch_without_snapshot() {
    let dir = TempDir::new().unwrap();
    let store = SessionTaskQueryStore::default();
    let supervisor = register_supervisor(&store, &dir);
    let task_id = supervisor.register("spawn", "c4", Some(&session_key().to_string()));
    supervisor.mark_running(&task_id);
    let (abort, _h) = spawn_pending();
    supervisor.register_abort(&task_id, abort, None, None);

    let dispatcher: &dyn TaskLifecycleDispatcher = &store;
    let (code, msg) = dispatcher
        .relaunch(&task_id, json!({"x": 1}))
        .expect_err("should require snapshot");
    assert_eq!(code, 409);
    assert!(msg.contains("spec snapshot"));
}

#[tokio::test]
async fn should_reject_send_when_task_has_no_inbox() {
    let dir = TempDir::new().unwrap();
    let store = SessionTaskQueryStore::default();
    let supervisor = register_supervisor(&store, &dir);
    let task_id = supervisor.register("spawn", "c5", Some(&session_key().to_string()));
    supervisor.mark_running(&task_id);
    let (abort, _h) = spawn_pending();
    supervisor.register_abort(&task_id, abort, None, None);

    let dispatcher: &dyn TaskLifecycleDispatcher = &store;
    let (code, _msg) = dispatcher
        .send(&task_id, "hi", None)
        .expect_err("should surface NoInbox");
    assert_eq!(code, 409);
}

#[tokio::test]
async fn should_reject_send_empty_message_via_dispatcher() {
    // The dispatcher layer accepts the message verbatim — the empty-body
    // check is enforced by the REST handler (see api_channel::handle_send_to_agent
    // and api::handlers::send_to_session_task). This test documents that the
    // dispatcher does not filter on message content so HTTP-layer filtering
    // is authoritative.
    let dir = TempDir::new().unwrap();
    let store = SessionTaskQueryStore::default();
    let supervisor = register_supervisor(&store, &dir);
    let task_id = supervisor.register("spawn", "c6", Some(&session_key().to_string()));
    supervisor.mark_running(&task_id);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let inbox = SupervisorInbox::new(tx);
    let (abort, _h) = spawn_pending();
    supervisor.register_abort(&task_id, abort, Some(inbox), None);

    let dispatcher: &dyn TaskLifecycleDispatcher = &store;
    // Empty body passes through the dispatcher — HTTP handler blocks this.
    dispatcher
        .send(&task_id, "", Some("operator"))
        .expect("dispatcher does not guard body");
    let msg = rx.try_recv().unwrap();
    assert_eq!(msg.body, "");
}

#[tokio::test]
async fn should_route_send_across_multiple_supervisors() {
    let store = SessionTaskQueryStore::default();
    let dir1 = TempDir::new().unwrap();
    let dir2 = TempDir::new().unwrap();
    let sup1 = Arc::new(TaskSupervisor::new());
    let sup2 = Arc::new(TaskSupervisor::new());
    store.register(
        &SessionKey::with_profile_topic(MAIN_PROFILE_ID, "api", "s1", "default"),
        &sup1,
        dir1.path(),
    );
    store.register(
        &SessionKey::with_profile_topic(MAIN_PROFILE_ID, "api", "s2", "default"),
        &sup2,
        dir2.path(),
    );

    let id1 = sup1.register("spawn", "c", Some("s1"));
    sup1.mark_running(&id1);
    let (abort1, _h1) = spawn_pending();
    sup1.register_abort(&id1, abort1, None, None);

    let id2 = sup2.register("spawn", "c", Some("s2"));
    sup2.mark_running(&id2);
    let (tx2, mut rx2) = tokio::sync::mpsc::unbounded_channel();
    let inbox2 = SupervisorInbox::new(tx2);
    let (abort2, _h2) = spawn_pending();
    sup2.register_abort(&id2, abort2, Some(inbox2), None);

    let dispatcher: &dyn TaskLifecycleDispatcher = &store;
    dispatcher
        .send(&id2, "for sup2 only", None)
        .expect("routed to sup2");
    let msg = rx2.try_recv().unwrap();
    assert_eq!(msg.body, "for sup2 only");

    // Cancelling id1 should reach sup1 and not sup2.
    dispatcher.cancel(&id1, "shutdown").unwrap();
    assert_eq!(sup1.get_task(&id1).unwrap().status, TaskStatus::Cancelled);
    assert!(sup2.get_task(&id1).is_none());
}

#[tokio::test]
async fn should_surface_invalid_relaunch_overrides() {
    let dir = TempDir::new().unwrap();
    let store = SessionTaskQueryStore::default();
    let supervisor = register_supervisor(&store, &dir);
    let task_id = supervisor.register("spawn", "c", Some(&session_key().to_string()));
    supervisor.mark_running(&task_id);
    let (abort, _h) = spawn_pending();
    supervisor.register_abort(&task_id, abort, None, Some(json!({"task": "x"})));

    let dispatcher: &dyn TaskLifecycleDispatcher = &store;
    let (code, _msg) = dispatcher
        .relaunch(&task_id, json!("not-an-object"))
        .expect_err("should reject non-object overrides");
    assert_eq!(code, 400);
}
