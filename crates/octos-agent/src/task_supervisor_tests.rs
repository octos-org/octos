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

// ── issue #1920/#1930: monotonic heartbeat reaper ──────────────────────

fn live_reaper_task() -> (Arc<TaskSupervisor>, String, TaskTerminalGuard) {
    let supervisor = Arc::new(TaskSupervisor::new());
    let id = supervisor.register("spawn", "call-reaper", Some("api:sess"));
    supervisor.mark_running(&id);
    let guard = TaskTerminalGuard::new(Arc::clone(&supervisor), id.clone());
    (supervisor, id, guard)
}

#[test]
fn reaper_fails_stuck_live_task_and_cancels_token() {
    let (supervisor, id, _guard) = live_reaper_task();
    let token = supervisor.cancel_token(&id);
    let reaped = supervisor.reap_stuck_tasks(
        tokio::time::Instant::now() + Duration::from_secs(61),
        Duration::from_secs(60),
    );
    assert_eq!(reaped, vec![id.clone()]);
    assert_eq!(supervisor.get_task(&id).unwrap().status, TaskStatus::Failed);
    assert!(token.is_cancelled());
    assert!(
        supervisor
            .reap_stuck_tasks(
                tokio::time::Instant::now() + Duration::from_secs(62),
                Duration::from_secs(60),
            )
            .is_empty(),
        "terminal tasks are not reaped twice"
    );
}

#[test]
fn reaper_spares_task_with_fresh_heartbeat() {
    let (supervisor, id, _guard) = live_reaper_task();
    assert!(
        supervisor
            .reap_stuck_tasks(tokio::time::Instant::now(), Duration::from_secs(60))
            .is_empty()
    );
    assert_eq!(
        supervisor.get_task(&id).unwrap().status,
        TaskStatus::Running
    );
}

#[test]
fn reaper_never_touches_terminal_tasks() {
    let (supervisor, id, _guard) = live_reaper_task();
    supervisor.mark_completed(&id, vec![]);
    assert!(
        supervisor
            .reap_stuck_tasks(
                tokio::time::Instant::now() + Duration::from_secs(3600),
                Duration::from_secs(60),
            )
            .is_empty()
    );
    assert_eq!(
        supervisor.get_task(&id).unwrap().status,
        TaskStatus::Completed
    );
}

#[test]
fn reaper_skips_active_task_without_live_worker() {
    let supervisor = Arc::new(TaskSupervisor::new());
    let id = supervisor.register("spawn", "call-not-live", Some("api:sess"));
    supervisor.mark_running(&id);
    assert!(!is_task_live(&id));
    assert!(
        supervisor
            .reap_stuck_tasks(
                tokio::time::Instant::now() + Duration::from_secs(3600),
                Duration::from_secs(60),
            )
            .is_empty(),
        "the terminal guard, not this reaper, owns dropped workers"
    );
}

#[test]
fn reaper_liveness_is_independent_of_wall_clock_timestamp_skew() {
    let (supervisor, id, _guard) = live_reaper_task();
    let displayed = Utc::now() - chrono::Duration::days(365);
    supervisor
        .tasks
        .lock()
        .unwrap()
        .get_mut(&id)
        .unwrap()
        .updated_at = displayed;
    assert!(
        supervisor
            .reap_stuck_tasks(tokio::time::Instant::now(), Duration::from_secs(60))
            .is_empty(),
        "a past UTC timestamp must not make a fresh monotonic heartbeat stale"
    );
    supervisor
        .tasks
        .lock()
        .unwrap()
        .get_mut(&id)
        .unwrap()
        .updated_at = Utc::now() + chrono::Duration::days(365);
    assert!(
        supervisor
            .reap_stuck_tasks(tokio::time::Instant::now(), Duration::from_secs(60))
            .is_empty(),
        "a future UTC timestamp must not grant immunity either"
    );
}

#[test]
fn reaper_restored_live_task_gets_a_monotonic_grace_period() {
    let dir = tempfile::TempDir::new().unwrap();
    let ledger = dir.path().join("tasks.jsonl");
    let owner = Arc::new(TaskSupervisor::new());
    owner.enable_persistence(&ledger).unwrap();
    let id = owner.register("spawn", "call-restored", Some("api:restore"));
    owner.mark_running(&id);
    let _guard = TaskTerminalGuard::new(Arc::clone(&owner), id.clone());

    let restored = TaskSupervisor::new();
    restored.enable_persistence(&ledger).unwrap();
    assert!(is_task_live(&id));
    assert!(!restored.heartbeats.lock().unwrap().contains_key(&id));
    let first_seen = tokio::time::Instant::now();
    assert!(
        restored
            .reap_stuck_tasks(first_seen, Duration::from_secs(60))
            .is_empty(),
        "restoring a live row must not make it immediately reapable"
    );
    assert_eq!(restored.get_task(&id).unwrap().status, TaskStatus::Running);
    assert_eq!(
        restored.reap_stuck_tasks(
            first_seen + Duration::from_secs(61),
            Duration::from_secs(60)
        ),
        vec![id],
        "a restored row that never heartbeats is still reaped after its grace period"
    );
}

#[test]
fn dedicated_heartbeat_refreshes_only_monotonic_liveness() {
    let (supervisor, id, _guard) = live_reaper_task();
    let displayed = Utc::now() - chrono::Duration::days(1);
    supervisor
        .tasks
        .lock()
        .unwrap()
        .get_mut(&id)
        .unwrap()
        .updated_at = displayed;
    let start = tokio::time::Instant::now();
    supervisor.heartbeat(&id);
    assert!(
        supervisor
            .reap_stuck_tasks(start + Duration::from_secs(59), Duration::from_secs(60))
            .is_empty()
    );
    assert_eq!(
        supervisor.get_task(&id).unwrap().updated_at,
        displayed,
        "high-frequency heartbeat does not rewrite the persisted/display timestamp"
    );
    assert_eq!(
        supervisor.reap_stuck_tasks(start + Duration::from_secs(61), Duration::from_secs(60)),
        vec![id],
        "once the monotonic heartbeat actually stops, the task is still reapable"
    );
}

#[test]
fn reaper_concurrent_heartbeat_refresh_stays_live_until_it_stops() {
    let (supervisor, id, _guard) = live_reaper_task();
    let writers: Vec<_> = (0..4)
        .map(|_| {
            let supervisor = Arc::clone(&supervisor);
            let id = id.clone();
            std::thread::spawn(move || {
                for _ in 0..100 {
                    supervisor.heartbeat(&id);
                }
            })
        })
        .collect();
    for _ in 0..100 {
        assert!(
            supervisor
                .reap_stuck_tasks(tokio::time::Instant::now(), Duration::from_secs(60))
                .is_empty(),
            "a concurrently refreshed worker must not be reaped"
        );
    }
    for writer in writers {
        writer.join().unwrap();
    }
    assert_eq!(
        supervisor.reap_stuck_tasks(
            tokio::time::Instant::now() + Duration::from_secs(61),
            Duration::from_secs(60),
        ),
        vec![id],
        "stopping the writers must not leave the task permanently immune"
    );
}

#[tokio::test(start_paused = true)]
async fn start_reaper_loop_reaps_stuck_task_on_interval() {
    let (supervisor, id, _guard) = live_reaper_task();
    supervisor.set_reap_interval(Duration::from_millis(20));
    supervisor.set_stuck_timeout(Duration::from_millis(50));
    supervisor.start_reaper();
    supervisor.start_reaper();
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(120)).await;
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    assert_eq!(supervisor.get_task(&id).unwrap().status, TaskStatus::Failed);
    assert!(supervisor.cancel_token(&id).is_cancelled());
}

#[tokio::test]
async fn reaper_exits_when_last_supervisor_clone_drops() {
    let supervisor = Arc::new(TaskSupervisor::new());
    supervisor.set_reap_interval(Duration::from_millis(10));
    supervisor.start_reaper();
    let weak = Arc::downgrade(&supervisor);
    drop(supervisor);
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if weak.upgrade().is_none() {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("reaper kept the supervisor alive after its last external clone dropped");
}
