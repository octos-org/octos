use super::*;

// ── M8.9: spawn_only failure recovery signals ───────────────────────────

use std::sync::Mutex as StdMutex;

fn collect_failure_signals(
    supervisor: &TaskSupervisor,
) -> Arc<StdMutex<Vec<SpawnOnlyFailureSignal>>> {
    let collected = Arc::new(StdMutex::new(Vec::new()));
    let captured = Arc::clone(&collected);
    supervisor.set_on_failure_signal(move |signal| {
        captured
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(signal.clone());
    });
    collected
}

// ── F004 B2: TaskSupervisor → ToolProgress bridge ─────────────────────

// ────────── M7.9 cancel / relaunch primitives (W2) ──────────

#[test]
fn cancel_running_task_transitions_to_cancelled_and_fires_token() {
    let supervisor = TaskSupervisor::new();
    let task_id = supervisor.register("run_pipeline", "call-cancel-1", Some("session-A"));
    supervisor.mark_running(&task_id);
    let token = supervisor.cancel_token(&task_id);
    assert!(!token.is_cancelled());

    supervisor.cancel(&task_id).expect("cancel should succeed");

    let task = supervisor.get_task(&task_id).expect("task still tracked");
    assert_eq!(task.status, TaskStatus::Cancelled);
    assert_eq!(task.runtime_state, TaskRuntimeState::Cancelled);
    assert_eq!(task.lifecycle_state(), TaskLifecycleState::Cancelled);
    assert!(token.is_cancelled());
    assert!(task.completed_at.is_some());
}

#[test]
fn diagnostic_error_preserves_real_owner_failure_and_delayed_ack_recovery() {
    let supervisor = TaskSupervisor::new();
    let failures = collect_failure_signals(&supervisor);
    let terminals = Arc::new(StdMutex::new(Vec::<TerminalEvent>::new()));
    let sink = Arc::clone(&terminals);
    supervisor.set_on_terminal(move |event| {
        sink.lock().unwrap().push(event.clone());
    });
    let id = supervisor.register("spawn_subagent", "call-real-failure", Some("api:session"));
    supervisor.mark_running(&id);
    let event = crate::harness_errors::HarnessError::Internal {
        message: "recoverable diagnostic".into(),
    }
    .to_event("api:session", id.clone(), None, None);
    supervisor.apply_harness_event(&id, &event).unwrap();

    // A late acknowledgement must not flush a stashed diagnostic failure.
    supervisor.mark_synth_ack_emitted("call-real-failure");
    assert!(failures.lock().unwrap().is_empty());
    assert!(terminals.lock().unwrap().is_empty());
    supervisor.mark_failed(&id, "worker actually stopped".into());
    supervisor.mark_failed(&id, "duplicate owner mark".into());
    let task = supervisor.get_task(&id).unwrap();
    assert_eq!(task.status, TaskStatus::Failed);
    assert!(!task.failed_by_observer);
    let failures = failures.lock().unwrap();
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].error_message, "worker actually stopped");
    let terminals = terminals.lock().unwrap();
    assert_eq!(terminals.len(), 1);
    assert!(terminals[0].is_failure());
}

#[test]
fn cancel_token_catches_cancel_between_precheck_and_notify_park() {
    let token = Arc::new(TaskCancelToken::new());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        let canceller = token.clone();
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            token.cancelled_after_first_check(move || canceller.cancel()),
        )
        .await
        .expect("cancelled() must not miss a cancel fired before Notified is parked");
    });
    assert!(token.is_cancelled());
}

#[test]
fn enable_persistence_reaps_orphan_running_tasks_at_startup() {
    // The bug: when the runtime crashes mid-task, the JSONL ledger has a
    // non-terminal entry for the in-flight task (Running / ResolvingOutputs
    // / etc) but no Completed/Failed event. On restart, the supervisor
    // restored that state verbatim — leaving the task forever
    // non-terminal because no live worker is backing it anymore.
    //
    // The fix: after replay, any task whose runtime_state is non-terminal
    // is reaped — marked Failed("orphaned across restart") — so callers
    // observing the supervisor see a clean state.

    let dir = tempfile::TempDir::new().unwrap();
    let ledger_path = dir.path().join("tasks.jsonl");

    // Phase 1: simulate a previous run that registered two tasks. Task A
    // is left mid-flight (Running). Task B reached terminal Completed.
    let supervisor = TaskSupervisor::new();
    supervisor.enable_persistence(&ledger_path).unwrap();
    let task_a = supervisor.register_with_lineage("search", "call-a", Some("api:session"), None);
    supervisor.mark_running(&task_a);
    let task_b = supervisor.register_with_lineage("fm_tts", "call-b", Some("api:session"), None);
    supervisor.mark_completed(&task_b, vec!["/tmp/voice.mp3".to_string()]);
    // Drop the first supervisor — its in-flight worker for task_a is gone.
    drop(supervisor);

    // Phase 2: a fresh supervisor replays the ledger and must reap the
    // orphaned non-terminal task.
    let restored = TaskSupervisor::new();
    restored.enable_persistence(&ledger_path).unwrap();

    let reaped = restored
        .get_task(&task_a)
        .expect("orphan task must still be tracked after reap");
    assert_eq!(
        reaped.status,
        TaskStatus::Failed,
        "non-peer orphan keeps the genuine-Failed verdict (#27c scope)"
    );
    assert_eq!(reaped.runtime_state, TaskRuntimeState::Failed);
    let error = reaped.error.as_deref().unwrap_or("");
    assert!(
        error.contains("orphaned") || error.contains("restart"),
        "orphan task error must mention orphan/restart, got {error:?}"
    );
    assert!(
        reaped.completed_at.is_some(),
        "a genuine-Failed orphan carries the terminal completed_at timestamp"
    );

    let surviving = restored
        .get_task(&task_b)
        .expect("completed task must still be tracked after reap");
    assert_eq!(
        surviving.status,
        TaskStatus::Completed,
        "terminal tasks must not be reaped"
    );
    assert_eq!(surviving.runtime_state, TaskRuntimeState::Completed);

    // Idempotency: a third supervisor replaying the same ledger must see
    // task_a still Parked (#27c — the sweep appended a Parked event, which
    // replays as Parked; a Parked task has no live worker in ANY process,
    // so re-sweeping is idempotent and leaves it re-attachable).
    let restored_again = TaskSupervisor::new();
    restored_again.enable_persistence(&ledger_path).unwrap();
    let reread = restored_again
        .get_task(&task_a)
        .expect("orphan task still tracked on second replay");
    assert_eq!(reread.status, TaskStatus::Failed);
    let reread_error = reread.error.as_deref().unwrap_or("");
    assert!(
        reread_error.contains("orphaned") || reread_error.contains("restart"),
        "orphan task error must persist across replay, got {reread_error:?}"
    );
    // The completed task is unaffected on replay.
    let reread_b = restored_again
        .get_task(&task_b)
        .expect("completed task still tracked on second replay");
    assert_eq!(reread_b.status, TaskStatus::Completed);

    // Cancelled tasks must also be respected as terminal — they should
    // not be reaped a second time. Add a cancelled task to the ledger,
    // reload, and assert the cancellation survives.
    let cancel_supervisor = restored_again;
    let task_c = cancel_supervisor.register_with_lineage(
        "run_pipeline",
        "call-c",
        Some("api:session"),
        None,
    );
    cancel_supervisor.mark_running(&task_c);
    cancel_supervisor
        .cancel(&task_c)
        .expect("cancel should succeed");
    drop(cancel_supervisor);
    let final_reload = TaskSupervisor::new();
    final_reload.enable_persistence(&ledger_path).unwrap();
    let cancelled = final_reload
        .get_task(&task_c)
        .expect("cancelled task still tracked after reload");
    assert_eq!(
        cancelled.status,
        TaskStatus::Cancelled,
        "cancelled tasks must not be reaped"
    );
    assert_eq!(cancelled.runtime_state, TaskRuntimeState::Cancelled);
}

/// The callback is invoked with NO supervisor locks held (cloned out of its
/// own mutex first, like `notify_change`), so user code that re-enters the
/// supervisor — the octos-cli closure resolves goal bindings and may read
/// task state — cannot deadlock.
#[test]
fn on_register_callback_may_reenter_the_supervisor_without_deadlock() {
    let supervisor = Arc::new(TaskSupervisor::new());
    let reentrant = Arc::clone(&supervisor);
    let observed = Arc::new(std::sync::Mutex::new(Option::<usize>::None));
    let observed_c = observed.clone();
    supervisor.set_on_register(move |task| {
        // Re-enter through read paths that take the `tasks` mutex.
        let all = reentrant.get_all_tasks();
        assert!(reentrant.get_task(&task.id).is_some());
        *observed_c.lock().unwrap() = Some(all.len());
    });

    supervisor.register("web_probe", "call-reenter", Some("api:sess-re"));

    assert_eq!(
        *observed.lock().unwrap(),
        Some(1),
        "the re-entrant read observed the freshly inserted task"
    );
}
