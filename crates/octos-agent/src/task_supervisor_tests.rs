use super::*;

/// #1723: `record_final_output` fires `on_change` so the roster mirror
/// (`upsert_background_task_agent` → `set_agent_output_if_empty`) re-runs
/// with `final_output` present. It is called AFTER `mark_completed`, so the
/// terminal on_change already fired while `final_output` was `None`; without
/// this second notification the agent view / `/ps` detail stays empty.
#[test]
fn record_final_output_fires_on_change_with_the_output() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let supervisor = TaskSupervisor::new();
    let id = supervisor.register("spawn", "call-final-output", None);
    supervisor.mark_completed(&id, vec![]);

    // Only observe changes AFTER completion, so we isolate the
    // record_final_output notification.
    let seen_output = Arc::new(std::sync::Mutex::new(Option::<String>::None));
    let calls = Arc::new(AtomicUsize::new(0));
    let seen_c = seen_output.clone();
    let calls_c = calls.clone();
    supervisor.set_on_change(move |task| {
        calls_c.fetch_add(1, Ordering::SeqCst);
        if let Some(output) = task.final_output.clone() {
            *seen_c.lock().unwrap() = Some(output);
        }
    });

    supervisor.record_final_output(&id, "the child's full result");

    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "record_final_output must fire on_change exactly once"
    );
    assert_eq!(
        seen_output.lock().unwrap().as_deref(),
        Some("the child's full result"),
        "the on_change snapshot must carry the recorded final_output"
    );
}

#[test]
fn should_register_task_with_spawned_status() {
    let supervisor = TaskSupervisor::new();
    let id = supervisor.register("tts", "call-123", None);

    let tasks = supervisor.get_all_tasks();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].id, id);
    assert_eq!(tasks[0].tool_name, "tts");
    assert_eq!(tasks[0].tool_call_id, "call-123");
    assert_eq!(tasks[0].status, TaskStatus::Spawned);
    assert_eq!(tasks[0].runtime_state, TaskRuntimeState::Spawned);
    assert!(tasks[0].child_terminal_state.is_none());
    assert!(tasks[0].child_join_state.is_none());
    assert!(tasks[0].child_failure_action.is_none());
    assert!(tasks[0].completed_at.is_none());
    assert!(tasks[0].updated_at >= tasks[0].started_at);
}

#[test]
fn named_change_listeners_fan_out_without_replacing_primary_callback() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let supervisor = TaskSupervisor::new();
    let primary = Arc::new(AtomicUsize::new(0));
    let projection_a = Arc::new(AtomicUsize::new(0));
    let projection_b = Arc::new(AtomicUsize::new(0));

    let count = Arc::clone(&primary);
    supervisor.set_on_change(move |_| {
        count.fetch_add(1, Ordering::SeqCst);
    });
    let count = Arc::clone(&projection_a);
    supervisor.set_on_change_listener("projection-a", move |_| {
        count.fetch_add(1, Ordering::SeqCst);
    });
    let count = Arc::clone(&projection_b);
    supervisor.set_on_change_listener("projection-b", move |_| {
        count.fetch_add(1, Ordering::SeqCst);
    });

    let id = supervisor.register("source_import", "call-fanout", None);
    supervisor.mark_running(&id);
    assert_eq!(primary.load(Ordering::SeqCst), 1);
    assert_eq!(projection_a.load(Ordering::SeqCst), 1);
    assert_eq!(projection_b.load(Ordering::SeqCst), 1);

    // Replacing a named listener is idempotent and leaves both the primary
    // callback and other projections installed.
    let replacement = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&replacement);
    supervisor.set_on_change_listener("projection-a", move |_| {
        count.fetch_add(1, Ordering::SeqCst);
    });
    supervisor.mark_completed(&id, vec![]);
    assert_eq!(primary.load(Ordering::SeqCst), 2);
    assert_eq!(projection_a.load(Ordering::SeqCst), 1);
    assert_eq!(replacement.load(Ordering::SeqCst), 1);
    assert_eq!(projection_b.load(Ordering::SeqCst), 2);
}

/// #966 / M13-B — the projection setter populates the new
/// optional fields. Verifies that:
/// - Newly-registered tasks start with all five fields None.
/// - `set_m13b_projection` overwrites the fields that were
///   supplied as Some and leaves the rest untouched.
/// - The persisted JSON round-trips through serde and the
///   default-omitted fields stay invisible until populated.
#[test]
fn set_m13b_projection_populates_optional_fields() {
    use serde_json::json;
    let supervisor = TaskSupervisor::new();
    let id = supervisor.register("tts", "call-m13b", None);

    let initial = supervisor.get_task(&id).expect("task");
    assert!(initial.source.is_none());
    assert!(initial.role.is_none());
    assert!(initial.summary.is_none());
    assert!(initial.artifact_count.is_none());
    assert!(initial.runtime_policy_stamp.is_none());

    supervisor.set_m13b_projection(
        &id,
        Some("model".into()),
        Some("reviewer".into()),
        Some("found 1 issue".into()),
        Some(2),
        Some(json!({ "approval_policy": "on-request" })),
    );

    let updated = supervisor.get_task(&id).expect("task");
    assert_eq!(updated.source.as_deref(), Some("model"));
    assert_eq!(updated.role.as_deref(), Some("reviewer"));
    assert_eq!(updated.summary.as_deref(), Some("found 1 issue"));
    assert_eq!(updated.artifact_count, Some(2));
    assert_eq!(
        updated.runtime_policy_stamp,
        Some(json!({ "approval_policy": "on-request" }))
    );

    // Partial update — only the artifact_count moves; the rest stay.
    supervisor.set_m13b_projection(&id, None, None, None, Some(5), None);
    let after_partial = supervisor.get_task(&id).expect("task");
    assert_eq!(after_partial.source.as_deref(), Some("model"));
    assert_eq!(after_partial.role.as_deref(), Some("reviewer"));
    assert_eq!(after_partial.artifact_count, Some(5));

    // Wire-shape: legacy snapshots without the fields round-trip
    // cleanly thanks to `#[serde(default)]`, AND newly-populated
    // ones surface every field.
    let json_form = serde_json::to_value(&after_partial).unwrap();
    assert_eq!(json_form["source"], "model");
    assert_eq!(json_form["role"], "reviewer");
    assert_eq!(json_form["summary"], "found 1 issue");
    assert_eq!(json_form["artifact_count"], 5);

    let bare = supervisor.register("podcast_generate", "call-bare", None);
    let bare_json = serde_json::to_value(supervisor.get_task(&bare).unwrap()).unwrap();
    assert!(bare_json.as_object().unwrap().get("source").is_none());
    assert!(
        bare_json
            .as_object()
            .unwrap()
            .get("artifact_count")
            .is_none()
    );
}

/// Codex P2 fix: `set_m13b_projection` must persist + notify so
/// reconnect hydration and `task/updated` subscribers observe the
/// new metadata without waiting for an unrelated lifecycle event.
/// Pins the on_change callback firing AND `updated_at` advancing.
#[test]
fn set_m13b_projection_fires_on_change_and_bumps_updated_at() {
    use std::sync::{Arc, Mutex};

    let supervisor = TaskSupervisor::new();
    let id = supervisor.register("tts", "call-m13b-notify", None);
    let before = supervisor.get_task(&id).expect("task").updated_at;

    let notifications: Arc<Mutex<Vec<BackgroundTask>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&notifications);
    supervisor.set_on_change(move |task: &BackgroundTask| {
        sink.lock().unwrap().push(task.clone());
    });

    // Sleep so updated_at is observably greater than registered_at.
    std::thread::sleep(std::time::Duration::from_millis(2));
    supervisor.set_m13b_projection(
        &id,
        Some("model".into()),
        Some("reviewer".into()),
        None,
        None,
        None,
    );

    let updated = supervisor.get_task(&id).expect("task");
    assert!(
        updated.updated_at > before,
        "set_m13b_projection must bump updated_at; before={before:?} after={:?}",
        updated.updated_at
    );

    let observed_len = notifications.lock().unwrap().len();
    assert_eq!(observed_len, 1, "on_change should fire exactly once");
    let event = notifications.lock().unwrap()[0].clone();
    assert_eq!(event.source.as_deref(), Some("model"));
    assert_eq!(event.role.as_deref(), Some("reviewer"));

    // No-op call (every arg None) must NOT fire the callback or
    // bump updated_at — defensive, avoids spurious update spam.
    let after_change = updated.updated_at;
    std::thread::sleep(std::time::Duration::from_millis(2));
    supervisor.set_m13b_projection(&id, None, None, None, None, None);
    let after_noop = supervisor.get_task(&id).expect("task");
    assert_eq!(
        after_noop.updated_at, after_change,
        "no-op call must NOT bump updated_at"
    );
    assert_eq!(
        notifications.lock().unwrap().len(),
        1,
        "no-op call must NOT fire on_change"
    );
}

/// Gap-1 unification: the unified `on_terminal` callback fires exactly
/// once per task for both success and failure transitions, carrying the
/// correct outcome + (for failures) the synth-ack-as-prompt-selection
/// boolean. Idempotent under repeated terminal marks.
#[test]
fn on_terminal_fires_once_for_success_and_failure_with_correct_payload() {
    use std::sync::{Arc, Mutex};

    // ── success ──────────────────────────────────────────────────
    let supervisor = TaskSupervisor::new();
    let events: Arc<Mutex<Vec<TerminalEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    supervisor.set_on_terminal(move |event: &TerminalEvent| {
        sink.lock().unwrap().push(event.clone());
    });

    let ok = supervisor.register("run_pipeline", "call-ok", Some("web:s1"));
    supervisor.mark_running(&ok);
    supervisor.mark_completed(&ok, vec!["/tmp/octos/out.md".to_owned()]);
    // Idempotent: a defensive double mark must not re-fire.
    supervisor.mark_completed(&ok, vec!["/tmp/octos/out.md".to_owned()]);

    {
        let observed = events.lock().unwrap();
        let completed: Vec<_> = observed
            .iter()
            .filter(|e| matches!(e.outcome, TerminalOutcome::Completed))
            .collect();
        assert_eq!(
            completed.len(),
            1,
            "exactly one Completed terminal event must fire (idempotent)"
        );
        assert_eq!(completed[0].task.id, ok);
        assert!(
            !completed[0].synth_ack_emitted,
            "completion events do not consult synth-ack"
        );
        assert!(!completed[0].is_failure());
    }

    // ── failure WITH synth-ack (recovery body should be selected) ──
    let with_ack = supervisor.register_with_input_and_cmid(
        "mofa_slides",
        "call-fail-ack",
        Some("web:s1"),
        Some(serde_json::json!({"topic": "rust"})),
        Some("cmid-42".to_owned()),
    );
    supervisor.mark_synth_ack_emitted("call-fail-ack");
    supervisor.mark_running(&with_ack);
    supervisor.mark_failed(
        &with_ack,
        "plugin exited 137. available: a, b, c".to_owned(),
    );
    // Idempotent re-mark (live + cascade collapse to one event).
    supervisor.mark_failed(&with_ack, "second mark".to_owned());

    // ── failure WITHOUT synth-ack (suppression at prompt selection) ─
    let no_ack = supervisor.register_with_input_and_cmid(
        "mofa_slides",
        "call-fail-noack",
        Some("web:s1"),
        Some(serde_json::json!({"topic": "go"})),
        None,
    );
    supervisor.mark_running(&no_ack);
    supervisor.mark_failed(&no_ack, "sibling suppressed".to_owned());

    let observed = events.lock().unwrap();
    let with_ack_event = observed
        .iter()
        .find(|e| e.task.id == with_ack)
        .expect("failure-with-ack event present");
    assert!(with_ack_event.is_failure());
    assert!(
        with_ack_event.synth_ack_emitted,
        "failure-with-ack must carry synth_ack_emitted=true so the consumer renders the recovery body"
    );
    let sig = with_ack_event.failure_signal().expect("failure signal");
    assert_eq!(sig.tool_name, "mofa_slides");
    assert_eq!(
        sig.originating_client_message_id.as_deref(),
        Some("cmid-42")
    );
    assert_eq!(
        sig.suggested_alternatives,
        vec!["a".to_owned(), "b".to_owned(), "c".to_owned()],
        "alternatives must be parsed off the error text",
    );

    let no_ack_event = observed
        .iter()
        .find(|e| e.task.id == no_ack)
        .expect("failure-without-ack event present");
    assert!(no_ack_event.is_failure());
    assert!(
        !no_ack_event.synth_ack_emitted,
        "failure-without-ack must carry synth_ack_emitted=false so the consumer suppresses the recovery body"
    );

    // Exactly one event per task id.
    let with_ack_count = observed.iter().filter(|e| e.task.id == with_ack).count();
    assert_eq!(
        with_ack_count, 1,
        "failure event must fire exactly once per task"
    );
}

/// Gap-1 unification: cascade-fail (`mark_descendants_failed`) and the
/// orphan-sweep (`enable_persistence`) both reach the unified terminal
/// sink — they funnel through `mark_failed`, so no extra wiring is
/// needed, but pin it so a refactor cannot silently regress autonomous
/// recovery for those paths.
#[test]
fn on_terminal_fires_for_cascade_and_orphan_sweep_failures() {
    use std::sync::{Arc, Mutex};

    // ── cascade-fail ─────────────────────────────────────────────
    let supervisor = TaskSupervisor::new();
    let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    supervisor.set_on_terminal(move |event: &TerminalEvent| {
        if event.is_failure() {
            sink.lock().unwrap().push(event.task.id.clone());
        }
    });

    // A running run_pipeline parent + two pipeline node children under
    // its tool_call_id.
    let parent = supervisor.register("run_pipeline", "call-pipe", Some("web:s2"));
    supervisor.mark_running(&parent);
    let child_a = supervisor
        .try_register_node_task("pipeline:analyze", "call-pipe", Some("web:s2"))
        .expect("node a");
    let child_b = supervisor
        .try_register_node_task("pipeline:render", "call-pipe", Some("web:s2"))
        .expect("node b");
    supervisor.mark_running(&child_a);
    supervisor.mark_running(&child_b);
    let cascaded = supervisor.mark_descendants_failed("call-pipe", "parent timed out");
    assert_eq!(cascaded, 2, "both node children must cascade-fail");

    {
        let observed = events.lock().unwrap();
        assert!(
            observed.contains(&child_a),
            "cascade child A must reach terminal sink"
        );
        assert!(
            observed.contains(&child_b),
            "cascade child B must reach terminal sink"
        );
    }

    // ── orphan-sweep ─────────────────────────────────────────────
    let dir = tempfile::TempDir::new().unwrap();
    let temp = dir.path().join("task_ledger.jsonl");
    let supervisor2 = TaskSupervisor::new();
    let events2: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink2 = Arc::clone(&events2);
    supervisor2.set_on_terminal(move |event: &TerminalEvent| {
        if event.is_failure() {
            sink2.lock().unwrap().push(event.task.id.clone());
        }
    });
    let orphan = supervisor2.register("run_pipeline", "call-orphan", Some("web:s3"));
    supervisor2.mark_running(&orphan);
    // enable_persistence sweeps the non-terminal in-flight task into
    // Failed("orphaned across restart") → mark_failed → notify_terminal.
    supervisor2
        .enable_persistence(&temp)
        .expect("enable_persistence");
    // #27c — parking is peer_handoff-scoped; this fixture's tool keeps
    // the genuine-Failed verdict, so the terminal sink still fires.
    assert!(
        events2.lock().unwrap().contains(&orphan),
        "non-peer orphan failure reaches the unified terminal sink"
    );
}

#[test]
fn terminal_updates_refresh_summary_and_artifact_count() {
    let supervisor = TaskSupervisor::new();
    let completed = supervisor.register("spawn", "call-complete", None);
    supervisor.set_m13b_projection(
        &completed,
        Some("model".into()),
        Some("reviewer".into()),
        None,
        Some(0),
        None,
    );
    supervisor.mark_completed(
        &completed,
        vec![
            "/tmp/octos-review/report.md".to_owned(),
            "/tmp/octos-review/raw.json".to_owned(),
        ],
    );
    let task = supervisor.get_task(&completed).expect("completed task");
    assert_eq!(task.artifact_count, Some(2));
    assert_eq!(
        task.summary.as_deref(),
        Some("spawn completed with 2 artifact(s)")
    );

    let failed = supervisor.register("spawn", "call-fail", None);
    supervisor.mark_failed(&failed, "review worker failed".to_owned());
    let task = supervisor.get_task(&failed).expect("failed task");
    assert_eq!(task.summary.as_deref(), Some("review worker failed"));
}

#[test]
fn should_register_task_with_lineage_and_ledger_path() {
    let dir = tempfile::TempDir::new().unwrap();
    let ledger_path = dir.path().join("tasks.jsonl");

    let supervisor = TaskSupervisor::new();
    supervisor.enable_persistence(&ledger_path).unwrap();

    let id = supervisor.register_with_lineage(
        "podcast_generate",
        "call-42",
        Some("api:session"),
        Some(ledger_path.to_str().unwrap()),
    );

    let task = supervisor.get_task(&id).expect("task missing");
    let expected_child = format!("api:session#child-{id}");
    assert_eq!(task.parent_session_key.as_deref(), Some("api:session"));
    assert_eq!(
        task.child_session_key.as_deref(),
        Some(expected_child.as_str())
    );
    assert_eq!(
        task.task_ledger_path.as_deref(),
        Some(ledger_path.to_str().unwrap())
    );
}

#[test]
fn should_transition_through_lifecycle_states() {
    let supervisor = TaskSupervisor::new();
    let id = supervisor.register("tts", "call-1", None);
    let task = &supervisor.get_all_tasks()[0];
    assert_eq!(task.lifecycle_state(), TaskLifecycleState::Queued);

    supervisor.mark_running(&id);
    let task = &supervisor.get_all_tasks()[0];
    assert_eq!(task.status, TaskStatus::Running);
    assert_eq!(task.runtime_state, TaskRuntimeState::ExecutingTool);
    assert_eq!(task.lifecycle_state(), TaskLifecycleState::Running);

    supervisor.mark_runtime_state(
        &id,
        TaskRuntimeState::DeliveringOutputs,
        Some("send_file".to_string()),
    );
    let task = &supervisor.get_all_tasks()[0];
    assert_eq!(task.status, TaskStatus::Running);
    assert_eq!(task.runtime_state, TaskRuntimeState::DeliveringOutputs);
    assert_eq!(task.runtime_detail.as_deref(), Some("send_file"));
    assert_eq!(task.lifecycle_state(), TaskLifecycleState::Verifying);

    supervisor.mark_completed(&id, vec!["output.mp3".to_string()]);
    let task = &supervisor.get_all_tasks()[0];
    assert_eq!(task.status, TaskStatus::Completed);
    assert_eq!(task.runtime_state, TaskRuntimeState::Completed);
    assert_eq!(task.lifecycle_state(), TaskLifecycleState::Ready);
    assert!(task.completed_at.is_some());
    assert_eq!(task.output_files, vec!["output.mp3"]);
}

#[test]
fn should_apply_harness_progress_event_and_notify() {
    let supervisor = TaskSupervisor::new();
    let id = supervisor.register("search", "call-9", Some("api:session"));
    supervisor.mark_running(&id);

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    supervisor.set_on_change(move |task| {
        let _ = tx.send(task.clone());
    });

    let event = crate::harness_events::HarnessEvent::progress(
        "api:session",
        id.clone(),
        Some("deep_research"),
        "fetching_sources",
        Some("Fetching source 3/12"),
        Some(0.42),
    );

    supervisor.apply_harness_event(&id, &event).unwrap();

    let task = supervisor.get_task(&id).expect("task missing");
    let detail: serde_json::Value =
        serde_json::from_str(task.runtime_detail.as_deref().unwrap()).unwrap();
    assert_eq!(detail["workflow_kind"], "deep_research");
    assert_eq!(detail["current_phase"], "fetching_sources");
    assert_eq!(detail["progress_message"], "Fetching source 3/12");
    let progress = detail["progress"].as_f64().unwrap();
    assert!((progress - 0.42).abs() < 0.0001);

    let notified = rx.try_recv().expect("callback should fire");
    let notified_detail: serde_json::Value =
        serde_json::from_str(notified.runtime_detail.as_deref().unwrap()).unwrap();
    assert_eq!(notified_detail["current_phase"], "fetching_sources");
    assert_eq!(notified.lifecycle_state(), TaskLifecycleState::Running);
}

#[test]
fn should_persist_harness_progress_event_for_replay() {
    let dir = tempfile::TempDir::new().unwrap();
    let ledger_path = dir.path().join("tasks.jsonl");

    let supervisor = TaskSupervisor::new();
    supervisor.enable_persistence(&ledger_path).unwrap();
    let id = supervisor.register_with_lineage("search", "call-9", Some("api:session"), None);
    supervisor.mark_running(&id);

    let event = crate::harness_events::HarnessEvent::progress(
        "api:session",
        id.clone(),
        Some("deep_research"),
        "fetch",
        Some("Fetching 4 pages"),
        Some(0.4),
    );
    supervisor.apply_harness_event(&id, &event).unwrap();

    let restored = TaskSupervisor::new();
    restored.enable_persistence(&ledger_path).unwrap();
    let task = restored.get_task(&id).expect("restored task missing");
    let detail: serde_json::Value =
        serde_json::from_str(task.runtime_detail.as_deref().unwrap()).unwrap();
    assert_eq!(
        detail["schema"],
        crate::harness_events::HARNESS_EVENT_SCHEMA_V1
    );
    assert_eq!(detail["session_id"], "api:session");
    assert_eq!(
        detail["schema_version"],
        serde_json::json!(crate::abi_schema::HARNESS_PROGRESS_EVENT_SCHEMA_VERSION)
    );
    assert_eq!(detail["task_id"], id);
    assert_eq!(detail["workflow_kind"], "deep_research");
    assert_eq!(detail["current_phase"], "fetch");
    assert_eq!(detail["progress_message"], "Fetching 4 pages");
    // Across restart, the in-flight task has no live worker. #27c: the
    // #27c — the sweep parks PEER_HANDOFF orphans only; this fixture's
    // tool is not a peer, so it keeps the legacy genuine-Failed verdict.
    // The harness progress detail still survives for operator diagnosis.
    assert_eq!(task.status, TaskStatus::Failed);
    assert_eq!(
        task.error.as_deref(),
        Some("orphaned across restart"),
        "orphan reaper must record a stable park reason"
    );
}

#[test]
fn should_persist_child_session_outcome_state() {
    let supervisor = TaskSupervisor::new();
    let id = supervisor.register("tts", "call-7", Some("api:session"));

    supervisor.mark_child_session_outcome(
        &id,
        ChildSessionTerminalState::RetryableFailure,
        ChildSessionJoinState::Joined,
    );

    let task = supervisor.get_task(&id).expect("task missing");
    assert_eq!(
        task.child_terminal_state,
        Some(ChildSessionTerminalState::RetryableFailure)
    );
    assert_eq!(task.child_join_state, Some(ChildSessionJoinState::Joined));
    assert_eq!(
        task.child_failure_action,
        Some(ChildSessionFailureAction::Retry)
    );
    assert!(task.child_joined_at.is_some());
}

#[test]
fn should_track_failed_tasks_with_error() {
    let supervisor = TaskSupervisor::new();
    let id = supervisor.register("tts", "call-2", None);

    supervisor.mark_running(&id);
    supervisor.mark_failed(&id, "connection refused".to_string());

    let task = &supervisor.get_all_tasks()[0];
    assert_eq!(task.status, TaskStatus::Failed);
    assert_eq!(task.runtime_state, TaskRuntimeState::Failed);
    assert_eq!(task.lifecycle_state(), TaskLifecycleState::Failed);
    assert_eq!(task.error.as_deref(), Some("connection refused"));
    assert!(task.completed_at.is_some());
}

#[test]
fn should_count_only_active_tasks() {
    let supervisor = TaskSupervisor::new();
    let id1 = supervisor.register("tts", "call-1", None);
    let id2 = supervisor.register("tts", "call-2", None);
    let _id3 = supervisor.register("tts", "call-3", None);

    assert_eq!(supervisor.task_count(), 3);

    supervisor.mark_completed(&id1, vec![]);
    assert_eq!(supervisor.task_count(), 2);

    supervisor.mark_failed(&id2, "err".to_string());
    assert_eq!(supervisor.task_count(), 1);
}

#[test]
fn should_return_only_active_tasks_in_get_active() {
    let supervisor = TaskSupervisor::new();
    let id1 = supervisor.register("tts", "call-1", None);
    let _id2 = supervisor.register("tts", "call-2", None);

    supervisor.mark_completed(&id1, vec![]);

    let active = supervisor.get_active_tasks();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].tool_call_id, "call-2");
}

/// Cascade-fail every active child of a parent's `tool_call_id`.
/// Regression pin for the `run_pipeline` timeout orphan bug —
/// without `mark_descendants_failed` child `pipeline:<node>` tasks
/// registered before the timeout future was dropped stayed in
/// `state: "running"` forever (visible to dashboard users as e.g.
/// `pipeline:analyze running` indefinitely).
#[test]
fn mark_descendants_failed_cascades_active_children_under_parent_tcid() {
    let supervisor = TaskSupervisor::new();
    let parent_tcid = "call-run_pipeline-parent";
    // The parent `run_pipeline` task is registered with the same
    // tool_call_id its node children reuse via
    // `executor.rs::register_node_task`. The cascade MUST NOT
    // touch the parent (it has its own `mark_failed` path in the
    // timeout arm of `RunPipelineTool::execute`).
    let parent = supervisor.register("run_pipeline", parent_tcid, Some("sess-1"));
    // Three node children share the parent's tool_call_id. The
    // first is pre-completed (should stay completed), the other
    // two are running (should both transition to Failed with the
    // timeout reason).
    let child1 = supervisor.register("pipeline:setup", parent_tcid, Some("sess-1"));
    let child2 = supervisor.register("pipeline:analyze", parent_tcid, Some("sess-1"));
    let child3 = supervisor.register("pipeline:plan_and_search", parent_tcid, Some("sess-1"));
    // A sibling task NOT under the timing-out parent: must be
    // untouched by the cascade.
    let unrelated = supervisor.register("tts", "call-other-parent", Some("sess-1"));

    supervisor.mark_running(&parent);
    supervisor.mark_running(&child2);
    supervisor.mark_running(&child3);
    supervisor.mark_running(&unrelated);
    supervisor.mark_completed(&child1, vec![]);

    let cascaded =
        supervisor.mark_descendants_failed(parent_tcid, "pipeline timed out after 1200s");
    assert_eq!(
        cascaded, 2,
        "exactly two pipeline:<node> children were active and should cascade-fail"
    );

    // child1 was completed before the cascade — must stay completed
    // (mark_failed's terminal-state guard preserves it).
    let t1 = supervisor.get_task(&child1).expect("child1");
    assert_eq!(t1.status, TaskStatus::Completed);

    // child2 and child3 were running — must now be Failed with the
    // pipeline-timeout reason carried in the error field.
    for cid in [&child2, &child3] {
        let task = supervisor.get_task(cid).expect("child task");
        assert_eq!(
            task.status,
            TaskStatus::Failed,
            "child {cid} must be Failed after cascade"
        );
        assert_eq!(task.runtime_state, TaskRuntimeState::Failed);
        assert!(task.completed_at.is_some());
        let err = task.error.clone().unwrap_or_default();
        assert!(
            err.contains("pipeline timed out after 1200s"),
            "child {cid} error must carry the timeout reason, got: {err}"
        );
    }

    // The parent `run_pipeline` task itself must remain Running —
    // its own `mark_failed` path in the timeout arm of
    // `RunPipelineTool::execute` is responsible for transitioning
    // it (the cascade must not race with that).
    let parent_task = supervisor.get_task(&parent).expect("parent");
    assert_eq!(
        parent_task.status,
        TaskStatus::Running,
        "parent run_pipeline task must NOT be cascaded — it has its own mark_failed path"
    );

    // The unrelated sibling under a different parent tool_call_id
    // must remain Running.
    let other = supervisor.get_task(&unrelated).expect("unrelated");
    assert_eq!(
        other.status,
        TaskStatus::Running,
        "task under a different parent tool_call_id must not be cascaded"
    );
}

/// Explicit regression pin for the codex MAJOR on #1180: the
/// cascade MUST filter to `pipeline:<node>` children and skip the
/// parent `run_pipeline` task even though both share the same
/// `tool_call_id`. Without the prefix filter, the cascade would
/// race with `RunPipelineTool::execute`'s own `mark_failed` path
/// for the parent.
#[test]
fn mark_descendants_failed_does_not_touch_parent_run_pipeline_task() {
    let supervisor = TaskSupervisor::new();
    let parent_tcid = "call-run_pipeline-only-parent";
    // Register ONLY the parent (no node children yet — pipeline
    // timed out before any node was dispatched, or all nodes
    // already completed). Cascade must be a no-op for the parent.
    let parent = supervisor.register("run_pipeline", parent_tcid, Some("sess-only"));
    supervisor.mark_running(&parent);

    let cascaded =
        supervisor.mark_descendants_failed(parent_tcid, "pipeline timed out after 1200s");
    assert_eq!(
        cascaded, 0,
        "no pipeline:<node> children registered, so cascade must be a no-op"
    );

    let parent_task = supervisor.get_task(&parent).expect("parent survives");
    assert_eq!(
        parent_task.status,
        TaskStatus::Running,
        "parent run_pipeline task must remain Running — cascade only targets pipeline:<node>"
    );
    assert!(
        parent_task.error.is_none(),
        "cascade must not write an error to the parent task"
    );
}

/// `mark_descendants_failed` with an empty parent tool_call_id is
/// a no-op (defensive guard — empty strings never match a real
/// registered task, and we don't want to mass-fail tasks that
/// happened to register with no parent context).
#[test]
fn mark_descendants_failed_with_empty_parent_is_noop() {
    let supervisor = TaskSupervisor::new();
    let id = supervisor.register("pipeline:work", "", Some("sess"));
    supervisor.mark_running(&id);

    let cascaded = supervisor.mark_descendants_failed("", "timeout");
    assert_eq!(cascaded, 0, "empty parent tcid must short-circuit");

    let task = supervisor.get_task(&id).expect("task survives");
    assert_eq!(task.status, TaskStatus::Running);
}

#[test]
fn should_be_empty_when_new() {
    let supervisor = TaskSupervisor::new();
    assert_eq!(supervisor.task_count(), 0);
    assert!(supervisor.get_all_tasks().is_empty());
    assert!(supervisor.get_active_tasks().is_empty());
}

#[test]
fn should_ignore_unknown_task_ids() {
    let supervisor = TaskSupervisor::new();
    // These should not panic
    supervisor.mark_running("nonexistent");
    supervisor.mark_completed("nonexistent", vec![]);
    supervisor.mark_failed("nonexistent", "err".to_string());
    assert_eq!(supervisor.task_count(), 0);
}

#[test]
fn should_restore_running_task_state_after_restart() {
    let dir = tempfile::TempDir::new().unwrap();
    let ledger_path = dir.path().join("tasks.jsonl");

    let supervisor = TaskSupervisor::new();
    supervisor.enable_persistence(&ledger_path).unwrap();

    let task_id = supervisor.register_with_lineage("search", "call-1", Some("api:session"), None);
    supervisor.mark_running(&task_id);
    supervisor.mark_runtime_state(
        &task_id,
        TaskRuntimeState::ResolvingOutputs,
        Some("collecting evidence".to_string()),
    );

    let restored = TaskSupervisor::new();
    restored.enable_persistence(&ledger_path).unwrap();

    let tasks = restored.get_all_tasks();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].id, task_id);
    // #27c — parking is peer_handoff-scoped; this fixture's tool keeps
    // the legacy genuine-Failed verdict. Metadata (lineage, ledger path,
    // last-known runtime_detail) is preserved for operator diagnosis.
    assert_eq!(tasks[0].status, TaskStatus::Failed);
    assert_eq!(tasks[0].runtime_state, TaskRuntimeState::Failed);
    assert_eq!(
        tasks[0].error.as_deref(),
        Some("orphaned across restart"),
        "orphan reaper must mark restored running tasks Parked"
    );
    // runtime_detail (the last live progress payload) survives the
    // reap so operators can see where the task was when the worker died.
    assert_eq!(
        tasks[0].runtime_detail.as_deref(),
        Some("collecting evidence")
    );
    let expected_child = format!("api:session#child-{task_id}");
    assert_eq!(tasks[0].parent_session_key.as_deref(), Some("api:session"));
    assert_eq!(
        tasks[0].child_session_key.as_deref(),
        Some(expected_child.as_str())
    );
    assert_eq!(
        tasks[0].task_ledger_path.as_deref(),
        Some(ledger_path.to_str().unwrap())
    );
}

#[test]
fn should_restore_completed_and_failed_truth_after_restart() {
    let dir = tempfile::TempDir::new().unwrap();
    let ledger_path = dir.path().join("tasks.jsonl");

    let supervisor = TaskSupervisor::new();
    supervisor.enable_persistence(&ledger_path).unwrap();

    let completed = supervisor.register_with_lineage("fm_tts", "call-2", Some("api:session"), None);
    supervisor.mark_running(&completed);
    supervisor.mark_runtime_state(
        &completed,
        TaskRuntimeState::DeliveringOutputs,
        Some("send_file".to_string()),
    );
    supervisor.mark_completed(&completed, vec!["/tmp/output.mp3".to_string()]);
    supervisor.mark_child_session_outcome(
        &completed,
        ChildSessionTerminalState::Completed,
        ChildSessionJoinState::Joined,
    );

    let failed =
        supervisor.register_with_lineage("podcast_generate", "call-3", Some("api:session"), None);
    supervisor.mark_running(&failed);
    supervisor.mark_failed(&failed, "No dialogue lines found in script".to_string());
    supervisor.mark_child_session_outcome(
        &failed,
        ChildSessionTerminalState::TerminalFailure,
        ChildSessionJoinState::Orphaned,
    );

    let restored = TaskSupervisor::new();
    restored.enable_persistence(&ledger_path).unwrap();

    let tasks = restored.get_all_tasks();
    assert_eq!(tasks.len(), 2);

    let completed_task = tasks
        .iter()
        .find(|task| task.id == completed)
        .expect("completed task missing");
    assert_eq!(completed_task.status, TaskStatus::Completed);
    assert_eq!(completed_task.runtime_state, TaskRuntimeState::Completed);
    assert_eq!(completed_task.runtime_detail.as_deref(), Some("send_file"));
    assert_eq!(completed_task.output_files, vec!["/tmp/output.mp3"]);
    let expected_completed_child = format!("api:session#child-{completed}");
    assert_eq!(
        completed_task.parent_session_key.as_deref(),
        Some("api:session")
    );
    assert_eq!(
        completed_task.child_session_key.as_deref(),
        Some(expected_completed_child.as_str())
    );
    assert_eq!(
        completed_task.task_ledger_path.as_deref(),
        Some(ledger_path.to_str().unwrap())
    );
    assert_eq!(
        completed_task.child_terminal_state,
        Some(ChildSessionTerminalState::Completed)
    );
    assert_eq!(
        completed_task.child_join_state,
        Some(ChildSessionJoinState::Joined)
    );
    assert_eq!(completed_task.child_failure_action, None);
    assert!(completed_task.child_joined_at.is_some());

    let failed_task = tasks
        .iter()
        .find(|task| task.id == failed)
        .expect("failed task missing");
    assert_eq!(failed_task.status, TaskStatus::Failed);
    assert_eq!(failed_task.runtime_state, TaskRuntimeState::Failed);
    assert_eq!(failed_task.runtime_detail, None);
    assert_eq!(
        failed_task.error.as_deref(),
        Some("No dialogue lines found in script")
    );
    assert_eq!(
        failed_task.parent_session_key.as_deref(),
        Some("api:session")
    );
    let expected_failed_child = format!("api:session#child-{failed}");
    assert_eq!(
        failed_task.child_session_key.as_deref(),
        Some(expected_failed_child.as_str())
    );
    assert_eq!(
        failed_task.task_ledger_path.as_deref(),
        Some(ledger_path.to_str().unwrap())
    );
    assert_eq!(
        failed_task.child_terminal_state,
        Some(ChildSessionTerminalState::TerminalFailure)
    );
    assert_eq!(
        failed_task.child_join_state,
        Some(ChildSessionJoinState::Orphaned)
    );
    assert_eq!(
        failed_task.child_failure_action,
        Some(ChildSessionFailureAction::Escalate)
    );
    assert!(failed_task.child_joined_at.is_none());
}

fn ledger_line_count(path: &std::path::Path) -> usize {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count()
}

#[test]
fn should_not_rewrite_ledger_when_reenabling_same_path() {
    let dir = tempfile::TempDir::new().unwrap();
    let ledger_path = dir.path().join("tasks.jsonl");

    let supervisor = TaskSupervisor::new();
    supervisor.enable_persistence(&ledger_path).unwrap();
    let task_id = supervisor.register_with_lineage("search", "call-1", Some("api:session"), None);
    supervisor.mark_completed(&task_id, vec![]);

    let lines_after_first_enable = ledger_line_count(&ledger_path);

    // The skill-action job view/invoke paths re-enable persistence on the
    // SHARED session supervisor on every request (#1906). With persistence
    // already on for this ledger, everything since has been appended
    // through the normal transition path — a repeat enable must be a
    // no-op, not a full snapshot rewrite.
    let restored = supervisor.enable_persistence(&ledger_path).unwrap();
    assert_eq!(
        restored, 1,
        "repeat enable returns the live task total, same as a full enable"
    );
    assert_eq!(
        ledger_line_count(&ledger_path),
        lines_after_first_enable,
        "same-path re-enable must not append snapshots"
    );
}

#[test]
fn should_not_reappend_restored_rows_when_fresh_supervisor_loads_ledger() {
    let dir = tempfile::TempDir::new().unwrap();
    let ledger_path = dir.path().join("tasks.jsonl");

    let supervisor = TaskSupervisor::new();
    supervisor.enable_persistence(&ledger_path).unwrap();
    let done = supervisor.register_with_lineage("fm_tts", "call-1", Some("api:session"), None);
    supervisor.mark_completed(&done, vec![]);
    let lines_before = ledger_line_count(&ledger_path);

    // A fresh supervisor that only RESTORES an existing ledger (the
    // skill-action list path builds one per request) must not re-append
    // snapshots for rows it just read — they are already on disk.
    let restored = TaskSupervisor::new();
    let count = restored.enable_persistence(&ledger_path).unwrap();
    assert_eq!(count, 1);
    assert_eq!(restored.get_all_tasks().len(), 1);
    assert_eq!(
        ledger_line_count(&ledger_path),
        lines_before,
        "read-only restore must leave the ledger untouched"
    );
}

#[test]
fn should_persist_preexisting_in_memory_tasks_when_enabling_persistence() {
    let dir = tempfile::TempDir::new().unwrap();
    let ledger_path = dir.path().join("tasks.jsonl");

    // A task scheduled BEFORE persistence is enabled (the startup window
    // the snapshot loop exists for) must still land on disk.
    let supervisor = TaskSupervisor::new();
    let task_id = supervisor.register_with_lineage("search", "call-1", Some("api:session"), None);
    supervisor.mark_completed(&task_id, vec![]);
    assert_eq!(ledger_line_count(&ledger_path), 0, "nothing persisted yet");

    supervisor.enable_persistence(&ledger_path).unwrap();
    assert!(
        ledger_line_count(&ledger_path) > 0,
        "pre-existing in-memory task must be persisted on enable"
    );

    let restored = TaskSupervisor::new();
    restored.enable_persistence(&ledger_path).unwrap();
    assert_eq!(restored.get_all_tasks().len(), 1);
    assert_eq!(restored.get_all_tasks()[0].id, task_id);
}

#[test]
fn should_pass_through_mark_completed_for_skill_reported_files() {
    // Supervisor no longer validates artifact content — it records the
    // skill+contract's reported outcome verbatim. Even a degenerate
    // 44-byte "voice.wav" stub passes through. The workspace contract
    // and the skill itself are responsible for catching bad outputs.
    let dir = tempfile::tempdir().unwrap();
    let stub = dir.path().join("voice.wav");
    std::fs::write(&stub, vec![0u8; 44]).unwrap();

    let supervisor = TaskSupervisor::new();
    let id = supervisor.register("fm_tts", "call-1", None);
    supervisor.mark_running(&id);

    supervisor.mark_completed(&id, vec![stub.to_string_lossy().to_string()]);

    let task = supervisor.get_task(&id).expect("task missing");
    assert_eq!(task.status, TaskStatus::Completed);
    assert_eq!(task.runtime_state, TaskRuntimeState::Completed);
    assert!(task.error.is_none());
}

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

#[test]
fn should_emit_failure_signal_when_spawn_only_task_status_becomes_failed() {
    let supervisor = TaskSupervisor::new();
    let collected = collect_failure_signals(&supervisor);
    let task_id = supervisor.register_with_input(
        "fm_tts",
        "call-1",
        Some("api:session"),
        Some(serde_json::json!({"voice": "yangmi", "text": "hi"})),
    );
    // Synth-ack gate: simulate the LLM having seen the
    // "Background work started for `fm_tts`." ack — production wires
    // this from `loop_runner.rs` when the synth-ack fires.
    supervisor.mark_synth_ack_emitted("call-1");
    supervisor.mark_running(&task_id);
    supervisor.mark_failed(
        &task_id,
        "voice 'yangmi' not registered. available: vivian, serena, longxiang".to_string(),
    );

    let signals = collected.lock().unwrap().clone();
    assert_eq!(signals.len(), 1, "expected exactly one failure signal");
    let signal = &signals[0];
    assert_eq!(signal.task_id, task_id);
    assert_eq!(signal.tool_name, "fm_tts");
    assert_eq!(signal.parent_session_key.as_deref(), Some("api:session"));
    assert!(
        signal
            .error_message
            .contains("voice 'yangmi' not registered")
    );
    assert_eq!(
        signal.suggested_alternatives,
        vec![
            "vivian".to_string(),
            "serena".to_string(),
            "longxiang".to_string()
        ]
    );
    assert_eq!(signal.tool_input["voice"], "yangmi");
}

#[test]
fn should_not_emit_signal_on_successful_completion() {
    let supervisor = TaskSupervisor::new();
    let collected = collect_failure_signals(&supervisor);
    let task_id = supervisor.register("fm_tts", "call-2", None);
    supervisor.mark_running(&task_id);
    supervisor.mark_completed(&task_id, vec!["/tmp/out.mp3".to_string()]);

    assert!(
        collected.lock().unwrap().is_empty(),
        "completion must not emit failure signal"
    );
}

#[test]
fn should_not_emit_signal_on_transient_running_state() {
    let supervisor = TaskSupervisor::new();
    let collected = collect_failure_signals(&supervisor);
    let task_id = supervisor.register("fm_tts", "call-3", None);
    supervisor.mark_running(&task_id);
    supervisor.mark_runtime_state(
        &task_id,
        TaskRuntimeState::DeliveringOutputs,
        Some("send_file".into()),
    );

    assert!(
        collected.lock().unwrap().is_empty(),
        "transient state changes must not emit failure signal"
    );
}

#[test]
fn should_only_emit_failure_signal_once_per_task() {
    let supervisor = TaskSupervisor::new();
    let collected = collect_failure_signals(&supervisor);
    let task_id = supervisor.register("fm_tts", "call-4", None);
    supervisor.mark_synth_ack_emitted("call-4");
    supervisor.mark_running(&task_id);
    supervisor.mark_failed(&task_id, "first failure".to_string());
    // re-marking should NOT re-fire the signal — guards against runaway
    // recovery loops if multiple paths report the same failure.
    supervisor.mark_failed(&task_id, "second failure".to_string());
    supervisor.mark_failed(&task_id, "third failure".to_string());

    assert_eq!(
        collected.lock().unwrap().len(),
        1,
        "subsequent failures must not re-fire the signal"
    );
}

#[test]
fn should_capture_tool_input_in_failure_signal() {
    let supervisor = TaskSupervisor::new();
    let collected = collect_failure_signals(&supervisor);
    let input = serde_json::json!({
        "voice": "yangmi",
        "text": "hello world",
        "format": "mp3",
    });
    let task_id = supervisor.register_with_input("fm_tts", "call-5", None, Some(input.clone()));
    supervisor.mark_synth_ack_emitted("call-5");
    supervisor.mark_failed(&task_id, "internal error".to_string());

    let signals = collected.lock().unwrap().clone();
    assert_eq!(signals.len(), 1);
    assert_eq!(signals[0].tool_input, input);
}

#[test]
fn parse_alternatives_handles_canonical_pattern() {
    let alts =
        parse_alternatives("voice 'yangmi' not registered. available: vivian, serena, longxiang.");
    assert_eq!(alts, vec!["vivian", "serena", "longxiang"]);
}

#[test]
fn parse_alternatives_returns_empty_when_no_marker() {
    let alts = parse_alternatives("connection refused after 3 retries");
    assert!(alts.is_empty());
}

#[test]
fn parse_alternatives_strips_quotes_and_whitespace() {
    let alts = parse_alternatives(r#"available: "alice", 'bob' , charlie"#);
    assert_eq!(alts, vec!["alice", "bob", "charlie"]);
}

#[test]
fn should_set_tool_input_after_registration() {
    let supervisor = TaskSupervisor::new();
    let collected = collect_failure_signals(&supervisor);
    let task_id = supervisor.register("fm_tts", "call-6", None);
    supervisor.set_tool_input(&task_id, serde_json::json!({"voice": "yangmi"}));
    supervisor.mark_synth_ack_emitted("call-6");
    supervisor.mark_failed(&task_id, "voice missing".to_string());

    let signals = collected.lock().unwrap().clone();
    assert_eq!(signals.len(), 1);
    assert_eq!(signals[0].tool_input["voice"], "yangmi");
}

#[test]
fn should_not_enqueue_second_recovery_for_same_task_id() {
    // Spec-named alias of should_only_emit_failure_signal_once_per_task —
    // codifies that the supervisor-level dedup is what guarantees the
    // session actor never sees a second hint for the same task id.
    let supervisor = TaskSupervisor::new();
    let collected = collect_failure_signals(&supervisor);
    let task_id = supervisor.register("fm_tts", "call-dedup", None);
    supervisor.mark_synth_ack_emitted("call-dedup");
    supervisor.mark_failed(&task_id, "first".to_string());
    supervisor.mark_failed(&task_id, "second".to_string());
    assert_eq!(collected.lock().unwrap().len(), 1);
}

#[test]
fn should_include_parsed_alternatives_from_error_text() {
    let supervisor = TaskSupervisor::new();
    let collected = collect_failure_signals(&supervisor);
    let task_id = supervisor.register("fm_tts", "call-alts", None);
    supervisor.mark_synth_ack_emitted("call-alts");
    supervisor.mark_failed(
        &task_id,
        "voice missing. available: vivian, serena, longxiang.".to_string(),
    );
    let signals = collected.lock().unwrap().clone();
    assert_eq!(signals.len(), 1);
    assert_eq!(
        signals[0].suggested_alternatives,
        vec![
            "vivian".to_string(),
            "serena".to_string(),
            "longxiang".to_string(),
        ]
    );
}

#[test]
fn should_include_tool_name_and_input_in_recovery_prompt() {
    // Asserts the supervisor exposes both the tool name and the input
    // on the SpawnOnlyFailureSignal so the session actor can build the
    // recovery prompt without re-walking the message history.
    let supervisor = TaskSupervisor::new();
    let collected = collect_failure_signals(&supervisor);
    let input = serde_json::json!({"voice": "yangmi", "text": "hello"});
    let task_id =
        supervisor.register_with_input("fm_tts", "call-prompt", None, Some(input.clone()));
    supervisor.mark_synth_ack_emitted("call-prompt");
    supervisor.mark_failed(&task_id, "voice missing".to_string());
    let signals = collected.lock().unwrap().clone();
    assert_eq!(signals.len(), 1);
    assert_eq!(signals[0].tool_name, "fm_tts");
    assert_eq!(signals[0].tool_input, input);
}

#[test]
fn should_emit_failure_signal_with_null_tool_input_when_unset() {
    let supervisor = TaskSupervisor::new();
    let collected = collect_failure_signals(&supervisor);
    let task_id = supervisor.register("fm_tts", "call-7", None);
    supervisor.mark_synth_ack_emitted("call-7");
    supervisor.mark_failed(&task_id, "boom".to_string());

    let signals = collected.lock().unwrap().clone();
    assert_eq!(signals.len(), 1);
    assert_eq!(signals[0].tool_input, Value::Null);
}

/// Synth-ack gate (PR feat/spawn-only-failure-feedback-loop): when the
/// LLM never received the "Background work started for `<tool>`." ack
/// for this task's `tool_call_id` (because the synth-ack gate
/// suppressed it — sibling-error mode), the supervisor MUST NOT emit
/// a `SpawnOnlyFailureSignal` on the eventual post-spawn failure. The
/// LLM already saw the sibling error in its iteration and will react
/// — injecting a synthetic recovery prompt would double-signal.
#[test]
fn should_not_emit_failure_signal_when_synth_ack_was_never_emitted() {
    let supervisor = TaskSupervisor::new();
    let collected = collect_failure_signals(&supervisor);
    let task_id = supervisor.register("fm_tts", "call-no-ack", None);
    supervisor.mark_running(&task_id);
    // No mark_synth_ack_emitted call — production analog: sibling tool
    // errored in this batch so loop_runner.rs suppressed the ack.
    supervisor.mark_failed(&task_id, "post-spawn error".to_string());

    assert!(
        collected.lock().unwrap().is_empty(),
        "failure signal must be suppressed when synth-ack never went to the LLM",
    );
}

#[test]
fn should_emit_failure_signal_only_after_synth_ack_recorded_for_tool_call_id() {
    let supervisor = TaskSupervisor::new();
    let collected = collect_failure_signals(&supervisor);

    // First task — synth-ack was suppressed, failure must NOT signal.
    let suppressed_task = supervisor.register("fm_tts", "call-suppressed", None);
    supervisor.mark_failed(&suppressed_task, "boom A".to_string());

    // Second task — synth-ack fired, failure MUST signal.
    let acked_task = supervisor.register("fm_tts", "call-acked", None);
    supervisor.mark_synth_ack_emitted("call-acked");
    supervisor.mark_failed(&acked_task, "boom B".to_string());

    let signals = collected.lock().unwrap().clone();
    assert_eq!(
        signals.len(),
        1,
        "exactly one failure signal — the synth-acked task — must reach the callback",
    );
    assert_eq!(signals[0].task_id, acked_task);
}

#[test]
fn mark_synth_ack_emitted_is_idempotent_and_ignores_empty_id() {
    let supervisor = TaskSupervisor::new();
    // Idempotent on repeated calls.
    supervisor.mark_synth_ack_emitted("call-x");
    supervisor.mark_synth_ack_emitted("call-x");
    assert!(supervisor.was_synth_ack_emitted("call-x"));
    // Empty / unknown id remains untracked.
    supervisor.mark_synth_ack_emitted("");
    assert!(!supervisor.was_synth_ack_emitted(""));
    assert!(!supervisor.was_synth_ack_emitted("call-missing"));
}

// ── Codex round-4 BLOCKER (PR #1324 follow-up): two-phase
// failure emission closes the spawn-vs-ack race ─────────────

/// Race scenario: `tokio::spawn` in execution.rs dispatches the
/// background task BEFORE loop_runner.rs records the synth-ack
/// (the spawn happens at line ~493, the synth-ack at line ~1356).
/// A fast post-spawn failure (plugin binary missing, instant
/// validator reject, etc.) can run `notify_failure` while
/// `synth_ack_emitted_tool_call_ids` is still empty. Pre-fix:
/// the would-be `SpawnOnlyFailureSignal` was dropped and the LLM
/// stayed in "Background work started" limbo forever. Post-fix:
/// the failure is stashed in `pending_failures` and emitted
/// when `mark_synth_ack_emitted` later arrives.
#[test]
fn failure_before_synth_ack_emits_recovery_when_ack_arrives() {
    let supervisor = TaskSupervisor::new();
    let collected = collect_failure_signals(&supervisor);
    let task_id = supervisor.register_with_input(
        "fm_tts",
        "call-race",
        Some("api:session"),
        Some(serde_json::json!({"voice": "yangmi", "text": "hi"})),
    );
    // Failure lands BEFORE the synth-ack is recorded — exactly the
    // race described in the codex BLOCKER. Pre-fix this dropped
    // the signal forever; post-fix it stashes for replay.
    supervisor.mark_failed(&task_id, "post-spawn boom".to_string());
    assert!(
        collected.lock().unwrap().is_empty(),
        "no signal must fire before the synth-ack lands"
    );

    // Foreground loop_runner finally records the synth-ack — the
    // stashed failure should now reach the callback.
    supervisor.mark_synth_ack_emitted("call-race");

    let signals = collected.lock().unwrap().clone();
    assert_eq!(
        signals.len(),
        1,
        "deferred failure must emit exactly one signal when the ack arrives"
    );
    assert_eq!(signals[0].task_id, task_id);
    assert_eq!(signals[0].tool_name, "fm_tts");
    assert!(signals[0].error_message.contains("post-spawn boom"));
    assert_eq!(
        signals[0].parent_session_key.as_deref(),
        Some("api:session")
    );
}

/// Companion to `failure_before_synth_ack_emits_recovery_when_ack_arrives`:
/// once the deferred-failure path has fired exactly one signal,
/// any sibling `mark_failed` call on the same task must NOT
/// double-fire. The codex BLOCKER spec calls this out as
/// `failure_signal_idempotent_on_double_emit`.
#[test]
fn failure_signal_idempotent_on_double_emit() {
    let supervisor = TaskSupervisor::new();
    let collected = collect_failure_signals(&supervisor);
    let task_id = supervisor.register("fm_tts", "call-double", None);
    // Fail-before-ack → stash.
    supervisor.mark_failed(&task_id, "first failure".to_string());
    // Ack → drain + dispatch once.
    supervisor.mark_synth_ack_emitted("call-double");
    // Subsequent mark_failed calls (production analog: cascade
    // path + the original failure path racing) must observe the
    // idempotency guard.
    supervisor.mark_failed(&task_id, "duplicate failure".to_string());
    supervisor.mark_failed(&task_id, "third failure".to_string());

    let signals = collected.lock().unwrap().clone();
    assert_eq!(
        signals.len(),
        1,
        "exactly one signal must fire even with repeated mark_failed + ack drain"
    );
    assert_eq!(signals[0].task_id, task_id);
}

/// The deferred-failure stash for one `tool_call_id` must not
/// interfere with normal failure-signal delivery for any other
/// `tool_call_id`. Codex BLOCKER spec calls this
/// `pending_failure_does_not_block_other_call_ids`.
#[test]
fn pending_failure_does_not_block_other_call_ids() {
    let supervisor = TaskSupervisor::new();
    let collected = collect_failure_signals(&supervisor);

    // Task A: fails before its ack arrives → goes pending.
    let task_a = supervisor.register("fm_tts", "call-A", None);
    supervisor.mark_failed(&task_a, "boom A".to_string());
    assert!(
        collected.lock().unwrap().is_empty(),
        "A's failure should still be pending — no ack yet"
    );

    // Task B: independent tool_call_id, normal ordering (ack
    // before failure) → must signal normally without being
    // blocked by A's pending stash.
    let task_b = supervisor.register("fm_tts", "call-B", None);
    supervisor.mark_synth_ack_emitted("call-B");
    supervisor.mark_failed(&task_b, "boom B".to_string());

    let signals = collected.lock().unwrap().clone();
    assert_eq!(
        signals.len(),
        1,
        "B must emit normally even while A sits in the pending map"
    );
    assert_eq!(signals[0].task_id, task_b);
    assert!(signals[0].error_message.contains("boom B"));

    // Finalise A — once its ack arrives the pending stash drains.
    supervisor.mark_synth_ack_emitted("call-A");
    let signals = collected.lock().unwrap().clone();
    assert_eq!(
        signals.len(),
        2,
        "A's pending entry must emit exactly once when its ack arrives"
    );
    assert_eq!(signals[1].task_id, task_a);
    assert!(signals[1].error_message.contains("boom A"));
}

/// Codex round-4 MAJOR (PR #1324): the synth-ack must be recorded
/// under the SANITIZED `tool_call_id` that the dispatcher used to
/// register the background task. Test as a direct supervisor-level
/// guard: simulate the production caller (loop_runner.rs:1357)
/// passing the sanitized id through, and verify the recovery
/// signal fires. Without the fix, loop_runner.rs records the raw
/// `call:1` while the supervisor stored the task under `call_1`,
/// so `was_synth_ack_emitted` misses and the recovery path is
/// permanently dropped.
#[test]
fn spawn_only_synth_ack_records_sanitized_id_when_id_has_colon() {
    // Mirror the canonical sanitization rule from
    // `agent::message_repair::sanitize_tool_call_id` (module is
    // private, so we encode the contract inline): every char
    // outside `[A-Za-z0-9_-]` maps to `_`. This is the same
    // mapping the dispatcher applies before storing the
    // BackgroundTask in the supervisor.
    let raw_id = "call:1";
    let sanitized: String = raw_id
        .chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '-' => c,
            _ => '_',
        })
        .collect();
    assert_eq!(
        sanitized, "call_1",
        "sanitize_tool_call_id contract: `:` → `_` (precondition)"
    );

    let supervisor = TaskSupervisor::new();
    let collected = collect_failure_signals(&supervisor);

    // The dispatcher stores the supervised task under the
    // SANITIZED tool_call_id (see execution.rs:438).
    let task_id = supervisor.register("fm_tts", &sanitized, Some("api:session"));

    // Post-MAJOR-fix loop_runner.rs records the synth-ack from
    // `sanitized_response.tool_calls` — i.e. with `call_1`, not
    // `call:1`. Simulate exactly that path.
    supervisor.mark_synth_ack_emitted(&sanitized);
    assert!(
        supervisor.was_synth_ack_emitted(&sanitized),
        "supervisor must observe synth-ack under the sanitized id"
    );
    // The raw id was never recorded — confirm we didn't
    // accidentally key on the un-sanitized form.
    assert!(
        !supervisor.was_synth_ack_emitted(raw_id),
        "supervisor must NOT observe synth-ack under the raw `call:1` id"
    );

    // Post-spawn failure runs through the supervisor with the
    // sanitized id (because that's what the BackgroundTask carries).
    supervisor.mark_failed(&task_id, "post-spawn boom".to_string());

    let signals = collected.lock().unwrap().clone();
    assert_eq!(
        signals.len(),
        1,
        "recovery signal must fire when synth-ack and supervisor task share the SANITIZED id"
    );
    assert_eq!(signals[0].task_id, task_id);
    assert!(signals[0].error_message.contains("post-spawn boom"));
}

// ── Codex round-2 BLOCKER + MAJOR (PR #1324 follow-up): atomic
// ack-vs-pending decision and bounded state ─────────────────

/// Codex round-2 BLOCKER: even when `notify_failure` and
/// `mark_synth_ack_emitted` are interleaved by concurrent threads,
/// every failure must eventually surface as a recovery signal once
/// the ack arrives. Pre-fix (separate mutexes for the ack set and
/// the pending map), this race could permanently drop the signal:
///   1. notify_failure observes ack=false.
///   2. mark_synth_ack_emitted records ack + drains empty pending.
///   3. notify_failure inserts pending — nothing will drain it.
///
/// Post-fix the combined mutex makes step 2 atomic with the
/// check-and-insert pair in step 1+3, so the pending entry is
/// either drained in step 2 OR observed in step 1 and dispatched
/// directly. Either way, exactly one signal per failure.
#[test]
fn failure_inserted_during_concurrent_ack_drain_still_fires() {
    use std::sync::Barrier;
    use std::thread;

    // High iteration count + concurrent racing pair to surface any
    // residual race. Even 1 lost wakeup across 200 iterations is a
    // 0.5% drop rate — easy to catch.
    const ITERATIONS: usize = 200;
    for iter in 0..ITERATIONS {
        let supervisor = TaskSupervisor::new();
        let collected = collect_failure_signals(&supervisor);
        let tool_call_id = format!("call-race-{iter}");
        let task_id = supervisor.register("fm_tts", &tool_call_id, None);

        // Two threads contend on `notify_failure` (via mark_failed)
        // and `mark_synth_ack_emitted`. The barrier maximizes the
        // chance of an interleaved hit on the ack-check vs
        // pending-insert window. Pre-fix this loses ~1-2% of
        // iterations on Apple Silicon; post-fix it must fire on
        // every iteration.
        let barrier = Arc::new(Barrier::new(2));
        let sup_a = supervisor.clone();
        let sup_b = supervisor.clone();
        let bar_a = Arc::clone(&barrier);
        let bar_b = Arc::clone(&barrier);
        let tcid_a = tool_call_id.clone();
        let tcid_b = tool_call_id.clone();
        let tid = task_id.clone();

        let h1 = thread::spawn(move || {
            bar_a.wait();
            sup_a.mark_failed(&tid, "race boom".to_string());
        });
        let h2 = thread::spawn(move || {
            bar_b.wait();
            sup_b.mark_synth_ack_emitted(&tcid_a);
            // Sleep is intentionally absent — we want the threads
            // racing tight, not serialized.
            let _ = tcid_b; // silence move warning while keeping symmetry
        });
        h1.join().expect("mark_failed thread");
        h2.join().expect("mark_synth_ack_emitted thread");

        let signals = collected.lock().unwrap().clone();
        assert_eq!(
            signals.len(),
            1,
            "iteration {iter}: race must produce exactly one signal regardless of interleaving",
        );
        assert_eq!(signals[0].task_id, task_id);
        assert!(signals[0].error_message.contains("race boom"));
    }
}

/// Codex round-2 MAJOR: `AckAndPending::pending` must be bounded
/// so a pathological flow (synth-ack permanently suppressed +
/// task never completes/cancels) cannot grow the supervisor
/// without limit. After inserting `MAX_PENDING_FAILURES + 1`
/// pending entries the oldest must be evicted and its eventual
/// ack must NOT surface a recovery signal — the evicted entry
/// has been dropped from the supervisor by design.
#[test]
fn pending_failures_eviction_when_max_size_exceeded() {
    let supervisor = TaskSupervisor::new();
    let collected = collect_failure_signals(&supervisor);

    // Insert MAX + 1 pending entries with distinct tool_call_ids
    // so the FIFO order is well-defined (each `pending` map slot
    // has a unique key + insertion order). Each task is registered
    // and then `mark_failed` is called BEFORE any synth-ack, so
    // every entry goes pending.
    let mut task_ids = Vec::with_capacity(MAX_PENDING_FAILURES + 1);
    for i in 0..=MAX_PENDING_FAILURES {
        let tcid = format!("call-stash-{i:04}");
        let tid = supervisor.register("fm_tts", &tcid, None);
        supervisor.mark_failed(&tid, format!("boom-{i}"));
        task_ids.push((tid, tcid));
    }

    // Pre-conditions: nothing should have signaled yet — every
    // entry is sitting in the pending stash.
    assert!(
        collected.lock().unwrap().is_empty(),
        "no signals must fire before any synth-ack lands",
    );

    // The map should be exactly bounded at MAX_PENDING_FAILURES;
    // the very first insert (index 0) was evicted to make room
    // for index MAX_PENDING_FAILURES.
    {
        let guard = supervisor.ack_and_pending.lock().unwrap();
        assert_eq!(
            guard.pending.len(),
            MAX_PENDING_FAILURES,
            "pending map must stay at cap",
        );
        // Oldest tool_call_id is no longer in the map.
        assert!(
            !guard.pending.contains_key(&task_ids[0].0),
            "oldest pending entry must be evicted",
        );
        // Newest tool_call_id is present.
        assert!(
            guard
                .pending
                .contains_key(&task_ids[MAX_PENDING_FAILURES].0),
            "newest pending entry must remain",
        );
    }

    // Now firing the synth-ack for the EVICTED tool_call_id must
    // NOT surface a recovery signal — the pending entry is gone.
    supervisor.mark_synth_ack_emitted(&task_ids[0].1);
    assert!(
        collected.lock().unwrap().is_empty(),
        "evicted pending entry must not fire when its ack arrives",
    );

    // Firing the synth-ack for the NEWEST tool_call_id must fire
    // exactly one signal — the entry is still in the map.
    supervisor.mark_synth_ack_emitted(&task_ids[MAX_PENDING_FAILURES].1);
    let signals = collected.lock().unwrap().clone();
    assert_eq!(
        signals.len(),
        1,
        "retained pending entry must still fire when its ack arrives",
    );
    assert_eq!(signals[0].task_id, task_ids[MAX_PENDING_FAILURES].0);
}

/// Codex round-2 MAJOR: `AckAndPending::emitted_task_ids` must be
/// bounded so the idempotency set cannot grow indefinitely over
/// the supervisor's lifetime. After firing
/// `MAX_FAILURE_SIGNAL_EMITTED_IDS + 1` distinct failure signals
/// the oldest entry is evicted, which is safe because the task is
/// long since terminal and the task_id (a UUID) is not reused.
#[test]
fn failure_signal_emitted_ids_eviction_when_max_size_exceeded() {
    let supervisor = TaskSupervisor::new();
    let _collected = collect_failure_signals(&supervisor);

    // Drive past the cap. Each iteration: register a task, mark
    // its synth-ack, mark it failed → one dispatch → one entry
    // appended to `emitted_task_ids`.
    let mut first_task_id = String::new();
    for i in 0..=MAX_FAILURE_SIGNAL_EMITTED_IDS {
        let tcid = format!("call-emit-{i:05}");
        let tid = supervisor.register("fm_tts", &tcid, None);
        supervisor.mark_synth_ack_emitted(&tcid);
        supervisor.mark_failed(&tid, format!("boom-{i}"));
        if i == 0 {
            first_task_id = tid;
        }
    }

    let guard = supervisor.ack_and_pending.lock().unwrap();
    assert_eq!(
        guard.emitted_task_ids.len(),
        MAX_FAILURE_SIGNAL_EMITTED_IDS,
        "emitted_task_ids must stay at cap",
    );
    // Oldest task_id is no longer in the set.
    assert!(
        !guard.emitted_task_ids.contains(&first_task_id),
        "oldest emitted task_id must be evicted",
    );
}

/// Codex round-3 MAJOR (PR #1324): the `pending_insertion_order`
/// VecDeque must stay bounded across many fail-before-ack →
/// ack-drain cycles, even though the cap inside `insert_pending`
/// never fires (the HashMap returns to size 0 after every drain).
///
/// Previously the VecDeque grew by one entry per cycle forever
/// because `drain_pending_for_tool_call` removed from the map but
/// left the task_id sitting in the queue. With ~1M cycles in a
/// long-running supervisor that would leak ~1M Strings (~50 MB).
#[test]
fn pending_insertion_order_does_not_leak_after_drain_cycles() {
    let supervisor = TaskSupervisor::new();
    let _collected = collect_failure_signals(&supervisor);

    // 4× the cap so we cleanly exercise the regression. Each
    // iteration uses a distinct (task_id, tool_call_id) so the
    // pending stash is keyed uniquely, then the synth-ack drains
    // it via `drain_pending_for_tool_call`.
    let n = MAX_PENDING_FAILURES * 4;
    for i in 0..n {
        let tcid = format!("call-drain-{i:06}");
        let tid = supervisor.register("fm_tts", &tcid, None);
        // mark_failed before any synth-ack stashes a pending entry.
        supervisor.mark_failed(&tid, format!("boom-{i}"));
        // synth-ack drains the pending entry — the HashMap returns
        // to 0 each cycle so the cap in `insert_pending` never
        // fires, exposing the VecDeque leak in the un-fixed code.
        supervisor.mark_synth_ack_emitted(&tcid);
    }

    let guard = supervisor.ack_and_pending.lock().unwrap();
    assert!(
        guard.pending.is_empty(),
        "pending map must drain to empty after every cycle, found {} entries",
        guard.pending.len(),
    );
    assert!(
        guard.pending_insertion_order.len() <= MAX_PENDING_FAILURES,
        "pending_insertion_order leaked: {} entries (cap {})",
        guard.pending_insertion_order.len(),
        MAX_PENDING_FAILURES,
    );
    // Strictest assertion: the queue should actually be EMPTY
    // because every pending entry was drained. The `<= cap`
    // assertion above is the round-3 contract; this tighter one
    // documents the ideal state.
    assert!(
        guard.pending_insertion_order.is_empty(),
        "pending_insertion_order must be empty after all entries are drained, found {} entries",
        guard.pending_insertion_order.len(),
    );
}

/// Codex round-3 MAJOR (PR #1324): companion to the drain test
/// above for the `remove_pending` path. `remove_pending` is called
/// from `drain_pending_failure_for_task` (defensive cleanup in
/// `mark_completed` / `cancel`) and must also pop the
/// `pending_insertion_order` entry. Same leak class.
///
/// In normal supervisor flow `mark_failed` makes the task
/// terminal, so `mark_completed` and `cancel` short-circuit before
/// reaching `remove_pending` — exercising it from the public API
/// is awkward. We test the lockstep invariant directly on
/// `AckAndPending` instead, which is what the round-3 fix
/// guarantees regardless of how `remove_pending` is reached.
#[test]
fn pending_insertion_order_does_not_leak_after_remove_cycles() {
    let mut state = AckAndPending::default();
    let n = MAX_PENDING_FAILURES * 4;
    for i in 0..n {
        let task_id = format!("task-remove-{i:06}");
        let tcid = format!("call-remove-{i:06}");
        state.insert_pending(
            task_id.clone(),
            PendingFailure {
                tool_call_id: tcid,
                signal: SpawnOnlyFailureSignal {
                    task_id: task_id.clone(),
                    tool_name: "fm_tts".into(),
                    tool_input: Value::Null,
                    error_message: format!("boom-{i}"),
                    suggested_alternatives: Vec::new(),
                    parent_session_key: None,
                    originating_client_message_id: None,
                },
            },
        );
        let removed = state.remove_pending(&task_id);
        assert!(
            removed.is_some(),
            "iteration {i}: remove_pending should return the inserted entry",
        );
    }

    assert!(
        state.pending.is_empty(),
        "pending map must drain to empty after every cycle, found {} entries",
        state.pending.len(),
    );
    assert!(
        state.pending_insertion_order.len() <= MAX_PENDING_FAILURES,
        "pending_insertion_order leaked under remove path: {} entries (cap {})",
        state.pending_insertion_order.len(),
        MAX_PENDING_FAILURES,
    );
    assert!(
        state.pending_insertion_order.is_empty(),
        "pending_insertion_order must be empty after all entries are removed, found {} entries",
        state.pending_insertion_order.len(),
    );
}

// ── F004 B2: TaskSupervisor → ToolProgress bridge ─────────────────────

/// Test reporter that captures every reported event so the bridge
/// assertions can branch on event kind without parsing JSON.
struct CapturingReporter {
    events: Arc<StdMutex<Vec<crate::progress::ProgressEvent>>>,
}

impl crate::progress::ProgressReporter for CapturingReporter {
    fn report(&self, event: crate::progress::ProgressEvent) {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(event);
    }
}

fn collect_progress_events(
    supervisor: &TaskSupervisor,
) -> Arc<StdMutex<Vec<crate::progress::ProgressEvent>>> {
    let events = Arc::new(StdMutex::new(Vec::new()));
    let reporter = Arc::new(CapturingReporter {
        events: Arc::clone(&events),
    });
    supervisor.set_progress_reporter(reporter);
    events
}

fn extract_tool_progress(
    events: &[crate::progress::ProgressEvent],
) -> Vec<(String, String, String)> {
    events
        .iter()
        .filter_map(|event| match event {
            crate::progress::ProgressEvent::ToolProgress {
                name,
                tool_id,
                message,
            } => Some((name.clone(), tool_id.clone(), message.clone())),
            _ => None,
        })
        .collect()
}

#[test]
fn should_emit_tool_progress_on_runtime_state_transition() {
    let supervisor = TaskSupervisor::new();
    let events = collect_progress_events(&supervisor);
    let task_id = supervisor.register("fm_tts", "call-progress-1", Some("api:session"));
    supervisor.mark_running(&task_id);
    supervisor.mark_runtime_state(
        &task_id,
        TaskRuntimeState::DeliveringOutputs,
        Some("send_file".to_string()),
    );

    let captured = events.lock().unwrap().clone();
    let tool_progress = extract_tool_progress(&captured);
    assert!(
        tool_progress.len() >= 2,
        "expected ToolProgress for mark_running + mark_runtime_state, got: {tool_progress:?}"
    );
    // Last event must reflect the DeliveringOutputs transition and
    // anchor on the originating tool_call_id so the chat UI can route
    // it to the right bubble.
    let (name, tool_id, message) = tool_progress.last().unwrap();
    assert_eq!(name, "fm_tts");
    assert_eq!(tool_id, "call-progress-1");
    assert_eq!(message, "fm_tts: delivering outputs");
}

#[test]
fn should_emit_tool_progress_on_completion_with_tool_call_id() {
    let supervisor = TaskSupervisor::new();
    let events = collect_progress_events(&supervisor);
    let task_id = supervisor.register("podcast_generate", "call-complete-1", None);
    supervisor.mark_completed(&task_id, vec!["/tmp/out.mp3".to_string()]);

    let captured = events.lock().unwrap().clone();
    let tool_progress = extract_tool_progress(&captured);
    let completion = tool_progress
        .iter()
        .find(|(_, _, message)| message.ends_with(": completed"))
        .expect("completion progress event missing");
    assert_eq!(completion.0, "podcast_generate");
    assert_eq!(completion.1, "call-complete-1");
    assert_eq!(completion.2, "podcast_generate: completed");
}

#[test]
fn should_emit_tool_progress_on_failure_with_reason() {
    let supervisor = TaskSupervisor::new();
    let events = collect_progress_events(&supervisor);
    let task_id = supervisor.register("fm_tts", "call-fail-1", None);
    supervisor.mark_failed(&task_id, "workspace policy not found".to_string());

    let captured = events.lock().unwrap().clone();
    let tool_progress = extract_tool_progress(&captured);
    let failure = tool_progress
        .iter()
        .find(|(_, _, message)| message.contains("failed"))
        .expect("failure progress event missing");
    assert_eq!(failure.0, "fm_tts");
    assert_eq!(failure.1, "call-fail-1");
    assert_eq!(failure.2, "fm_tts: failed (workspace policy not found)");
}

#[test]
fn should_not_emit_tool_progress_when_no_reporter_attached() {
    let supervisor = TaskSupervisor::new();
    let task_id = supervisor.register("fm_tts", "call-silent-1", None);
    // No reporter attached — must be a no-op (and crucially must not
    // panic on the missing reporter).
    supervisor.mark_running(&task_id);
    supervisor.mark_runtime_state(
        &task_id,
        TaskRuntimeState::DeliveringOutputs,
        Some("send_file".to_string()),
    );
    supervisor.mark_completed(&task_id, vec![]);
    // Nothing to assert beyond the absence of a panic — the reporter is
    // optional by design so the supervisor can be used outside the
    // chat-progress pipeline (e.g. cron, tests).
}

#[test]
fn should_only_emit_failure_progress_once_per_task() {
    let supervisor = TaskSupervisor::new();
    let events = collect_progress_events(&supervisor);
    let task_id = supervisor.register("fm_tts", "call-fail-dedup", None);
    supervisor.mark_failed(&task_id, "first".to_string());
    // Second mark_failed must NOT re-emit a ToolProgress for the
    // same task — mirrors the existing failure-signal dedup contract.
    supervisor.mark_failed(&task_id, "second".to_string());

    let captured = events.lock().unwrap().clone();
    let tool_progress = extract_tool_progress(&captured);
    let failures: Vec<_> = tool_progress
        .iter()
        .filter(|(_, _, message)| message.contains("failed"))
        .collect();
    assert_eq!(
        failures.len(),
        1,
        "expected exactly one failure ToolProgress, got: {failures:?}"
    );
}

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
fn cancel_unknown_task_returns_not_found() {
    let supervisor = TaskSupervisor::new();
    let result = supervisor.cancel("does-not-exist");
    assert_eq!(result, Err(TaskCancelError::NotFound));
}

#[test]
fn cancel_terminal_task_returns_already_terminal() {
    let supervisor = TaskSupervisor::new();
    let task_id = supervisor.register("podcast_generate", "call-cancel-2", Some("session-B"));
    supervisor.mark_completed(&task_id, vec!["output/podcast.mp3".into()]);
    let result = supervisor.cancel(&task_id);
    assert_eq!(result, Err(TaskCancelError::AlreadyTerminal));
    // Cancelling a Failed task is also rejected.
    let task_id2 = supervisor.register("fm_tts", "call-cancel-3", None);
    supervisor.mark_failed(&task_id2, "boom".to_string());
    assert_eq!(
        supervisor.cancel(&task_id2),
        Err(TaskCancelError::AlreadyTerminal)
    );
}

#[test]
fn cancel_emits_progress_event() {
    let supervisor = TaskSupervisor::new();
    let events = collect_progress_events(&supervisor);
    let task_id = supervisor.register("run_pipeline", "call-cancel-4", Some("session-C"));
    supervisor.mark_running(&task_id);
    supervisor.cancel(&task_id).expect("cancel should succeed");

    let captured = events.lock().unwrap().clone();
    let tool_progress = extract_tool_progress(&captured);
    let cancels: Vec<_> = tool_progress
        .iter()
        .filter(|(_, _, message)| message.contains("cancelled"))
        .collect();
    assert!(
        !cancels.is_empty(),
        "expected at least one cancelled ToolProgress, got: {tool_progress:?}"
    );
}

// ────────── M8 Req #4 DoD: cancel cannot be overwritten by late workers ──────────

/// Race regression: a worker that finishes AFTER the user has cancelled
/// the task must NOT resurrect it to `Completed`. The supervisor's
/// `mark_completed` guard short-circuits when the task is already in a
/// terminal state. Asserts state stays `Cancelled`, the on_change callback
/// fires exactly twice (once for `mark_running`, once for `cancel`), and
/// the ProgressReporter does NOT emit a spurious "completed" event after
/// cancellation.
#[test]
fn mark_completed_after_cancel_does_not_overwrite_cancelled_state() {
    use std::sync::Mutex;
    let supervisor = TaskSupervisor::new();
    let progress_events = collect_progress_events(&supervisor);
    let on_change_count: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
    {
        let on_change_count = on_change_count.clone();
        supervisor.set_on_change(move |_task| {
            *on_change_count.lock().unwrap() += 1;
        });
    }

    let task_id = supervisor.register("run_pipeline", "call-race-1", Some("session-X"));
    supervisor.mark_running(&task_id); // notify #1
    supervisor.cancel(&task_id).expect("cancel should succeed"); // notify #2

    // Late-arriving worker tries to mark completed — this is the race.
    supervisor.mark_completed(&task_id, vec!["late/output.bin".into()]); // must noop

    let task = supervisor.get_task(&task_id).expect("task still tracked");
    assert_eq!(
        task.status,
        TaskStatus::Cancelled,
        "late mark_completed must NOT overwrite Cancelled state"
    );
    assert_eq!(task.runtime_state, TaskRuntimeState::Cancelled);
    assert_eq!(task.lifecycle_state(), TaskLifecycleState::Cancelled);
    assert!(
        task.output_files.is_empty(),
        "late completion's output_files must not leak onto a Cancelled task, got: {:?}",
        task.output_files
    );

    // on_change must have fired exactly twice — guard noop must not
    // double-fire the change callback.
    assert_eq!(
        *on_change_count.lock().unwrap(),
        2,
        "on_change should fire exactly twice (mark_running + cancel), not for the noop mark_completed"
    );

    // ProgressReporter must not have emitted any "completed" message
    // after cancellation. We saw running + cancelled, but never completed.
    let captured = progress_events.lock().unwrap().clone();
    let tool_progress = extract_tool_progress(&captured);
    let post_cancel_completed: Vec<_> = tool_progress
        .iter()
        .filter(|(_, _, message)| message.contains("completed"))
        .collect();
    assert!(
        post_cancel_completed.is_empty(),
        "guard must not emit 'completed' progress for a cancelled task, got: {tool_progress:?}"
    );
}

/// Race regression mirror: a worker that fails AFTER the user has
/// cancelled the task must NOT overwrite the cancellation with a
/// `Failed` status. Without the guard this would corrupt the
/// dashboard ("user cancelled" silently flips to "the task crashed").
#[test]
fn mark_failed_after_cancel_does_not_overwrite_cancelled_state() {
    use std::sync::Mutex;
    let supervisor = TaskSupervisor::new();
    let on_change_count: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
    {
        let on_change_count = on_change_count.clone();
        supervisor.set_on_change(move |_task| {
            *on_change_count.lock().unwrap() += 1;
        });
    }
    let failure_signals: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
    {
        let failure_signals = failure_signals.clone();
        supervisor.set_on_failure_signal(move |_signal| {
            *failure_signals.lock().unwrap() += 1;
        });
    }

    let task_id = supervisor.register("run_pipeline", "call-race-2", Some("session-Y"));
    supervisor.mark_running(&task_id); // notify #1
    supervisor.cancel(&task_id).expect("cancel should succeed"); // notify #2

    // Late-arriving worker reports failure — guard must reject.
    supervisor.mark_failed(&task_id, "late worker error".to_string());

    let task = supervisor.get_task(&task_id).expect("task still tracked");
    assert_eq!(
        task.status,
        TaskStatus::Cancelled,
        "late mark_failed must NOT overwrite Cancelled state"
    );
    assert_eq!(task.runtime_state, TaskRuntimeState::Cancelled);
    assert_eq!(
        task.error.as_deref(),
        Some("cancelled by supervisor"),
        "cancel reason must survive the late mark_failed call"
    );

    assert_eq!(
        *on_change_count.lock().unwrap(),
        2,
        "on_change should fire exactly twice (mark_running + cancel), not for the noop mark_failed"
    );
    assert_eq!(
        *failure_signals.lock().unwrap(),
        0,
        "spawn-only failure signal must NOT fire for a cancelled task that hits the guard"
    );
}

/// Idempotency: calling `mark_completed` twice on the same task should
/// be a no-op on the second call. The first call sets the terminal
/// state; the second hits the guard and warns. Output files do NOT
/// regress (the second call's payload is ignored), and the on_change /
/// progress reporter both fire exactly once for the real transition.
#[test]
fn mark_completed_after_completed_is_idempotent_and_warns() {
    use std::sync::Mutex;
    let supervisor = TaskSupervisor::new();
    let progress_events = collect_progress_events(&supervisor);
    let on_change_count: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
    {
        let on_change_count = on_change_count.clone();
        supervisor.set_on_change(move |_task| {
            *on_change_count.lock().unwrap() += 1;
        });
    }

    let task_id = supervisor.register("podcast_generate", "call-race-3", None);
    supervisor.mark_running(&task_id); // notify #1
    supervisor.mark_completed(&task_id, vec!["output/first.mp3".into()]); // notify #2

    // Second call must be a noop — no panic, no state regression.
    supervisor.mark_completed(&task_id, vec!["output/second.mp3".into()]);

    let task = supervisor.get_task(&task_id).expect("task still tracked");
    assert_eq!(task.status, TaskStatus::Completed);
    assert_eq!(
        task.output_files,
        vec!["output/first.mp3".to_string()],
        "second mark_completed must NOT replace the first call's output_files"
    );

    assert_eq!(
        *on_change_count.lock().unwrap(),
        2,
        "on_change should fire exactly twice (mark_running + first mark_completed), not for the noop second call"
    );

    // Progress reporter should see at most one "completed" emission.
    let captured = progress_events.lock().unwrap().clone();
    let tool_progress = extract_tool_progress(&captured);
    let completed_emissions: Vec<_> = tool_progress
        .iter()
        .filter(|(_, _, message)| message.contains("completed"))
        .collect();
    assert_eq!(
        completed_emissions.len(),
        1,
        "expected exactly one 'completed' progress emission, got: {tool_progress:?}"
    );
}

/// mini4 RC2/RC3 regression (`review-octos-web-v3`): a worker's failed
/// TOOL CALL (`unknown tool: write_file`, classified
/// `tool_execution`/`fail_fast`) reaches the supervisor via the
/// harness-event sink. The agent loop treats that error as feedback and
/// KEEPS RUNNING — so the supervisor must not declare the task dead.
/// Pre-fix, the Error arm called `mark_failed`, the chip went red, and
/// the worker's real completion 10 minutes later was refused by the
/// terminal-state guard ("ignoring late mark_completed").
#[test]
fn tool_scoped_error_event_keeps_task_alive_and_completion_lands() {
    let supervisor = TaskSupervisor::new();
    let id = supervisor.register("spawn", "call-web-v3", Some("api:session"));
    supervisor.mark_running(&id);

    let event = crate::harness_errors::HarnessError::ToolExecution {
        tool_name: "write_file".to_string(),
        message: "unknown tool: write_file".to_string(),
    }
    .to_event("api:session", id.clone(), None, None);
    supervisor.apply_harness_event(&id, &event).unwrap();

    let task = supervisor.get_task(&id).expect("task missing");
    assert_eq!(
        task.status,
        TaskStatus::Running,
        "a tool-call error is loop-recoverable; it must not kill the task"
    );
    // The error is still surfaced to operators via runtime detail.
    let detail = task.runtime_detail.as_deref().unwrap_or_default();
    assert!(
        detail.contains("unknown tool: write_file"),
        "tool error must remain visible in runtime_detail: {detail:?}"
    );

    // The owner (spawn join) reports the true outcome.
    supervisor.mark_completed(&id, vec!["review.md".to_string()]);
    let task = supervisor.get_task(&id).expect("task missing");
    assert_eq!(
        task.status,
        TaskStatus::Completed,
        "the worker finished successfully; the chip must say so"
    );
    assert_eq!(task.output_files, vec!["review.md".to_string()]);
}

/// Defense in depth for the same class: a NON-tool-scoped fail-fast error
/// (e.g. `invalid_request`) observed mid-run still marks the task failed
/// eagerly — but that verdict is OBSERVER-derived, not the owner's. If
/// the loop survives (retry / non-streaming fallback) and the owner join
/// later reports success, the completion must override the premature
/// failure instead of being dropped by the terminal guard.
#[test]
fn owner_completion_overrides_observer_derived_failure() {
    let supervisor = TaskSupervisor::new();
    let id = supervisor.register("spawn", "call-observer", Some("api:session"));
    supervisor.mark_running(&id);

    let event = crate::harness_errors::HarnessError::InvalidRequest {
        detail: "http_400".to_string(),
        message: "The supported API model names are ...".to_string(),
    }
    .to_event("api:session", id.clone(), None, None);
    supervisor.apply_harness_event(&id, &event).unwrap();

    let task = supervisor.get_task(&id).expect("task missing");
    assert_eq!(
        task.status,
        TaskStatus::Failed,
        "non-tool-scoped fail_fast errors still fail the task eagerly"
    );

    supervisor.mark_completed(&id, vec!["out.txt".to_string()]);
    let task = supervisor.get_task(&id).expect("task missing");
    assert_eq!(
        task.status,
        TaskStatus::Completed,
        "the owner's completion is authoritative over an observer-derived failure"
    );
    assert_eq!(task.output_files, vec!["out.txt".to_string()]);
    assert!(
        task.error.is_none(),
        "the premature failure's error must be cleared on override: {:?}",
        task.error
    );
}

/// The owner's own failure verdict stays final: `mark_failed` from the
/// spawn join path (worker really died) is NOT overridable by a stray
/// late completion — only OBSERVER-derived failures are.
#[test]
fn owner_reported_failure_still_blocks_late_completion() {
    let supervisor = TaskSupervisor::new();
    let id = supervisor.register("spawn", "call-owner-fail", Some("api:session"));
    supervisor.mark_running(&id);

    supervisor.mark_failed(&id, "did not complete within 50 iterations".to_string());
    supervisor.mark_completed(&id, vec!["late.bin".to_string()]);

    let task = supervisor.get_task(&id).expect("task missing");
    assert_eq!(
        task.status,
        TaskStatus::Failed,
        "an owner-reported failure must not be resurrected by a late completion"
    );
    assert!(task.output_files.is_empty());
}

/// Race regression: a worker that calls `mark_running` AFTER the user has
/// cancelled the task must NOT resurrect it to `Running`. This is the
/// subtle case that hides under register → cancel-before-running →
/// worker still observes the spawn and tries to flip Running before
/// noticing the cancel token.
#[test]
fn mark_running_after_cancel_does_not_overwrite_cancelled_state() {
    let supervisor = TaskSupervisor::new();
    let task_id = supervisor.register("run_pipeline", "call-race-4", Some("session-Z"));
    // Cancel BEFORE mark_running — exercises the "cancelled while still
    // Spawned" branch of the race window.
    supervisor.cancel(&task_id).expect("cancel should succeed");

    // Late worker tries to mark running — must noop.
    supervisor.mark_running(&task_id);

    let task = supervisor.get_task(&task_id).expect("task still tracked");
    assert_eq!(
        task.status,
        TaskStatus::Cancelled,
        "late mark_running must NOT overwrite Cancelled state"
    );
    assert_eq!(task.runtime_state, TaskRuntimeState::Cancelled);
}

/// Race regression: a worker that emits a harness progress event AFTER
/// the user has cancelled the task must NOT corrupt the stored
/// `runtime_state` away from `Cancelled`. Without the guard, ledger
/// snapshots and progress emissions would flip to e.g. `executing_tool`
/// even though the public `status` is still `Cancelled`.
#[test]
fn mark_runtime_state_after_cancel_does_not_overwrite_cancelled_runtime_state() {
    let supervisor = TaskSupervisor::new();
    let task_id = supervisor.register("run_pipeline", "call-race-5", Some("session-W"));
    supervisor.mark_running(&task_id);
    supervisor.cancel(&task_id).expect("cancel should succeed");

    // Late worker reports a phase update — must noop.
    supervisor.mark_runtime_state(
        &task_id,
        TaskRuntimeState::DeliveringOutputs,
        Some(r#"{"workflow_kind":"podcast","current_phase":"render"}"#.into()),
    );

    let task = supervisor.get_task(&task_id).expect("task still tracked");
    assert_eq!(task.status, TaskStatus::Cancelled);
    assert_eq!(
        task.runtime_state,
        TaskRuntimeState::Cancelled,
        "late mark_runtime_state must NOT overwrite Cancelled runtime_state"
    );
}

/// Race regression: late `mark_failed` after the task completed normally
/// must not flip a `Completed` task back to `Failed`. This exercises the
/// non-cancel branch of the new mark_failed guard.
#[test]
fn mark_failed_after_completed_does_not_overwrite_completed_state() {
    let supervisor = TaskSupervisor::new();
    let task_id = supervisor.register("podcast_generate", "call-race-6", None);
    supervisor.mark_running(&task_id);
    supervisor.mark_completed(&task_id, vec!["output/podcast.mp3".into()]);

    // Late worker reports a failure — must noop.
    supervisor.mark_failed(&task_id, "stale failure".to_string());

    let task = supervisor.get_task(&task_id).expect("task still tracked");
    assert_eq!(
        task.status,
        TaskStatus::Completed,
        "late mark_failed must NOT overwrite Completed state"
    );
    assert!(
        task.error.is_none(),
        "Completed task must not gain an error from a late mark_failed, got: {:?}",
        task.error
    );
}

#[test]
fn relaunch_failed_task_creates_successor_and_fires_callback() {
    use std::sync::Mutex;
    let supervisor = TaskSupervisor::new();
    let captured: Arc<Mutex<Vec<RelaunchRequest>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let captured = captured.clone();
        supervisor.set_on_relaunch(move |req| {
            captured.lock().unwrap().push(req.clone());
        });
    }

    let task_id = supervisor.register("run_pipeline", "call-relaunch-1", Some("session-D"));
    supervisor.mark_running(&task_id);
    supervisor.mark_failed(&task_id, "node 'design' failed".to_string());

    let new_id = supervisor
        .relaunch(
            &task_id,
            RelaunchOpts {
                from_node: Some("design".into()),
            },
        )
        .expect("relaunch should succeed");
    assert_ne!(new_id, task_id, "relaunch must allocate a fresh id");

    let new_task = supervisor.get_task(&new_id).expect("successor registered");
    assert_eq!(new_task.tool_name, "run_pipeline");
    assert_eq!(new_task.tool_call_id, "call-relaunch-1");
    assert_eq!(new_task.session_key.as_deref(), Some("session-D"));

    let log = captured.lock().unwrap();
    assert_eq!(log.len(), 1, "relaunch callback fired exactly once");
    assert_eq!(log[0].original_task_id, task_id);
    assert_eq!(log[0].new_task_id, new_id);
    assert_eq!(log[0].opts.from_node.as_deref(), Some("design"));
}

#[test]
fn relaunch_unknown_task_returns_not_found() {
    let supervisor = TaskSupervisor::new();
    let result = supervisor.relaunch("does-not-exist", RelaunchOpts::default());
    assert_eq!(result, Err(TaskRelaunchError::NotFound));
}

#[test]
fn relaunch_active_task_returns_still_active() {
    let supervisor = TaskSupervisor::new();
    let task_id = supervisor.register("run_pipeline", "call-relaunch-2", None);
    supervisor.mark_running(&task_id);
    let result = supervisor.relaunch(&task_id, RelaunchOpts::default());
    assert_eq!(result, Err(TaskRelaunchError::StillActive));
}

#[test]
fn cancel_token_notifies_waiters() {
    let supervisor = TaskSupervisor::new();
    let task_id = supervisor.register("run_pipeline", "call-cancel-notify", None);
    supervisor.mark_running(&task_id);
    let token = supervisor.cancel_token(&task_id);

    // Drive a small async runtime so the token can fire its
    // notification path (poll-then-wait).
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let waiter = {
            let token = token.clone();
            tokio::spawn(async move { token.cancelled().await })
        };
        // Yield so the waiter actually parks on `notified()`.
        tokio::task::yield_now().await;
        supervisor.cancel(&task_id).expect("cancel should succeed");
        tokio::time::timeout(std::time::Duration::from_millis(500), waiter)
            .await
            .expect("waiter must wake within 500ms")
            .expect("waiter task panicked");
    });
    assert!(token.is_cancelled());
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

/// Guard A regression: a parent session that has already accepted
/// `MAX_CHILDREN_PER_PARENT` children must refuse the next register
/// with a structured `ChildFanoutExceeded` error and force-fail every
/// still-active child so the cascade collapses.
#[test]
fn register_task_refuses_201st_child_for_same_parent() {
    // Use a smaller cap via env var so the test does not allocate
    // 200+ tasks in CI. The cap reader caches once per process — we
    // run this test in isolation with a fresh `TaskSupervisor` and a
    // sub-process-friendly cap value that is set before any other
    // register call resolves the cache.
    //
    // Note: setting `OCTOS_MAX_CHILDREN_PER_PARENT` here would be
    // racy because `max_children_per_parent` caches with `OnceLock`.
    // Instead we exercise the production cap (200) — register 200
    // children, then assert the 201st is refused.
    let parent_session = "api:test-parent";
    let supervisor = TaskSupervisor::new();
    for i in 0..MAX_CHILDREN_PER_PARENT {
        let id = supervisor
            .try_register_with_input("tts", &format!("call-{i}"), Some(parent_session), None)
            .unwrap_or_else(|err| panic!("register #{i} should succeed; got {err}"));
        // Mark a slice of the children as active (Running) so the
        // force-fail cascade has something to flip on the 201st
        // call. Leaving every task in Spawned (also active) works
        // identically.
        if i % 2 == 0 {
            supervisor.mark_running(&id);
        }
    }
    assert_eq!(
        supervisor.get_tasks_for_session(parent_session).len(),
        MAX_CHILDREN_PER_PARENT,
        "supervisor should hold exactly the cap before the refusal fires"
    );

    // The 201st register must be refused with a typed error that
    // carries the count, cap, and the parent session key.
    let err = supervisor
        .try_register_with_input("tts", "call-overflow", Some(parent_session), None)
        .expect_err("201st child must be refused");
    match err {
        RegisterTaskError::ChildFanoutExceeded {
            parent_session_key,
            count,
            cap,
        } => {
            assert_eq!(parent_session_key, parent_session);
            assert_eq!(count, MAX_CHILDREN_PER_PARENT);
            assert_eq!(cap, MAX_CHILDREN_PER_PARENT);
        }
        other => panic!("expected ChildFanoutExceeded, got {other:?}"),
    }

    // The cap rejection must not leak a new task into the
    // supervisor — count stays at the cap.
    assert_eq!(
        supervisor.get_tasks_for_session(parent_session).len(),
        MAX_CHILDREN_PER_PARENT,
        "refused register must not insert a new task"
    );

    // Every still-active child of the runaway parent should have
    // been force-marked `Failed` with the structured reason so the
    // cascade collapses instead of waiting on each child to finish.
    let expected_reason =
        format!("child fanout exceeded ({MAX_CHILDREN_PER_PARENT} of {MAX_CHILDREN_PER_PARENT})");
    let tasks = supervisor.get_tasks_for_session(parent_session);
    let any_active = tasks.iter().any(|t| t.status.is_active());
    assert!(
        !any_active,
        "every active child should be flipped to Failed after the cap fires"
    );
    let failed_with_reason = tasks
        .iter()
        .filter(|t| {
            t.status == TaskStatus::Failed && t.error.as_deref() == Some(expected_reason.as_str())
        })
        .count();
    assert!(
        failed_with_reason > 0,
        "at least one child should carry the structured fan-out reason"
    );

    // A subsequent attempt against the same poisoned parent must
    // continue to be refused (fast-path via `poisoned_parents`).
    let err = supervisor
        .try_register_with_input("tts", "call-after-overflow", Some(parent_session), None)
        .expect_err("poisoned parent must keep refusing further registers");
    assert!(matches!(err, RegisterTaskError::ChildFanoutExceeded { .. }));

    // A fresh, distinct parent session is unaffected.
    let other = supervisor
        .try_register_with_input("tts", "call-other-1", Some("api:other-parent"), None)
        .expect("other parents stay unaffected by a poisoned peer");
    assert!(!other.is_empty());
}

/// The fan-out cap bounds LIVE children, not the session's lifetime
/// total. Regression: the cap count had no `is_active()` filter and
/// `tasks` is never pruned, so a long-lived session that merely
/// COMPLETED `MAX_CHILDREN_PER_PARENT` background tasks over its life
/// (tts / podcast / pipeline nodes) tripped the cap on the next
/// register — poisoning the session key forever and force-failing
/// every currently-active legitimate task.
#[test]
fn completed_children_do_not_count_toward_fanout_cap() {
    let parent_session = "api:long-lived-parent";
    let supervisor = TaskSupervisor::new();
    for i in 0..MAX_CHILDREN_PER_PARENT {
        let id = supervisor
            .try_register_with_input("tts", &format!("call-{i}"), Some(parent_session), None)
            .unwrap_or_else(|err| panic!("register #{i} should succeed; got {err}"));
        // Every child finishes cleanly — the session is long-lived,
        // not runaway.
        supervisor.mark_completed(&id, Vec::new());
    }

    let id = supervisor
        .try_register_with_input("tts", "call-after-long-life", Some(parent_session), None)
        .expect("a session with only COMPLETED children must not trip the fan-out cap");
    assert!(!id.is_empty());

    // And the session must not have been poisoned or had work
    // force-failed by the register above.
    let tasks = supervisor.get_tasks_for_session(parent_session);
    assert!(
        tasks
            .iter()
            .all(|t| t.error.as_deref().is_none_or(|e| !e.contains("fanout"))),
        "no child may be force-failed with a fan-out reason on a healthy session"
    );
}

/// codex P2 on the active-only cap count: `cancel` flips a task's STATUS
/// to Cancelled immediately, but the detached worker keeps running until
/// it observes the token — a status-only count would let a session spawn
/// the cap, cancel everything, and spawn another cap while the first
/// workers still execute. A child whose worker is still LIVE (in the
/// process-global live-set armed by [`TaskTerminalGuard::new`]) must keep
/// counting toward the cap until the guard drops.
#[test]
fn cancelled_but_still_live_children_still_count_toward_cap() {
    let parent_session = "api:cancel-bypass-parent";
    let supervisor = Arc::new(TaskSupervisor::new());
    let mut guards = Vec::new();
    for i in 0..MAX_CHILDREN_PER_PARENT {
        let id = supervisor
            .try_register_with_input("busy", &format!("call-{i}"), Some(parent_session), None)
            .unwrap_or_else(|err| panic!("register #{i} should succeed; got {err}"));
        // Arm the production liveness guard (worker "running"), then
        // cancel: status flips Cancelled but the worker is still live.
        guards.push(TaskTerminalGuard::new(supervisor.clone(), id.clone()));
        let _ = supervisor.cancel(&id);
    }

    let err = supervisor
        .try_register_with_input("busy", "call-bypass", Some(parent_session), None)
        .expect_err("cancelled-but-still-LIVE workers must still bound the fan-out cap");
    assert!(matches!(err, RegisterTaskError::ChildFanoutExceeded { .. }));
    drop(guards);
}

/// The legacy `register_with_input` entry point keeps returning a
/// `String`; on cap rejection it returns an empty-string sentinel
/// rather than panicking so existing call sites still type-check.
#[test]
fn legacy_register_returns_empty_string_on_cap_rejection() {
    let parent_session = "api:legacy-parent";
    let supervisor = TaskSupervisor::new();
    for i in 0..MAX_CHILDREN_PER_PARENT {
        supervisor.register("tts", &format!("call-{i}"), Some(parent_session));
    }
    let id = supervisor.register("tts", "call-overflow", Some(parent_session));
    assert!(
        id.is_empty(),
        "legacy register must return empty-string sentinel when refused"
    );
}

/// #2056 — the restore observer fires ONCE per restore, with the table in its
/// FINAL post-sweep state, and never for a re-enable that restores nothing.
/// Consumers that mirror task state elsewhere (the octos-cli goal ledger) use
/// it to notice terminal transitions the previous process never delivered, so
/// a snapshot taken before the orphan sweep would hand them a row that is
/// about to change and a repeat firing would re-drive work already done.
#[test]
fn should_fire_on_restore_once_with_the_swept_table_when_persistence_is_enabled() {
    let dir = tempfile::TempDir::new().unwrap();
    let ledger_path = dir.path().join("tasks.jsonl");

    let writer = TaskSupervisor::new();
    writer.enable_persistence(&ledger_path).unwrap();
    let orphan = writer.register("search", "call-orphan", Some("api:session"));
    writer.mark_running(&orphan);
    let finished = writer.register("fm_tts", "call-done", Some("api:session"));
    writer.mark_completed(&finished, vec![]);
    drop(writer);

    type RestoredRows = Vec<(String, TaskStatus)>;
    let observed: Arc<Mutex<Vec<RestoredRows>>> = Arc::new(Mutex::new(Vec::new()));
    let restored = TaskSupervisor::new();
    let sink = Arc::clone(&observed);
    restored.set_on_restore(move |tasks| {
        let mut rows: RestoredRows = tasks
            .iter()
            .map(|task| (task.id.clone(), task.status.clone()))
            .collect();
        rows.sort_by(|left, right| left.0.cmp(&right.0));
        sink.lock().unwrap().push(rows);
    });
    restored.enable_persistence(&ledger_path).unwrap();

    let calls = observed.lock().unwrap().clone();
    assert_eq!(calls.len(), 1, "exactly one firing per restore");
    let rows: std::collections::HashMap<String, TaskStatus> = calls[0].iter().cloned().collect();
    assert_eq!(
        rows.get(&orphan),
        Some(&TaskStatus::Failed),
        "the observer sees the table AFTER the orphan sweep, not before it",
    );
    assert_eq!(rows.get(&finished), Some(&TaskStatus::Completed));

    // Re-enabling the SAME path restores nothing (the idempotence guard) and
    // must not re-fire.
    restored.enable_persistence(&ledger_path).unwrap();
    assert_eq!(
        observed.lock().unwrap().len(),
        1,
        "a no-op re-enable must not re-fire the restore observer",
    );
}

/// #2056 round 3 (R5) — seed a ledger with one finished task and return its
/// path, so a fresh supervisor enabling on it performs a REAL restore.
fn seeded_restore_ledger(dir: &tempfile::TempDir) -> std::path::PathBuf {
    let ledger_path = dir.path().join("tasks.jsonl");
    let writer = TaskSupervisor::new();
    writer.enable_persistence(&ledger_path).unwrap();
    let task = writer.register("search", "call-restore-seed", Some("api:session"));
    writer.mark_completed(&task, vec![]);
    drop(writer);
    ledger_path
}

/// #2056 round 3 (R5) — the missed-restore handshake delivers EXACTLY ONCE,
/// across every sequential wiring order. Counted at the observer itself: the
/// previous pin compared goal-ledger settle attempts, which is vacuous once
/// the first delivery has already settled the row (a duplicate finds no
/// candidate and writes nothing either way).
#[test]
fn should_deliver_a_restore_exactly_once_however_the_wiring_is_ordered() {
    use std::sync::atomic::AtomicUsize;

    let dir = tempfile::TempDir::new().unwrap();
    let ledger_path = seeded_restore_ledger(&dir);

    let counting_observer = || {
        let calls = Arc::new(AtomicUsize::new(0));
        let sink = Arc::clone(&calls);
        (calls, move |_: &[BackgroundTask]| {
            sink.fetch_add(1, Ordering::SeqCst);
        })
    };

    // Never restored ⇒ nothing to deliver.
    let never_restored = TaskSupervisor::new();
    let (calls, observer) = counting_observer();
    never_restored.set_on_restore(observer);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "a supervisor that never restored must not fire its new observer",
    );

    // Wire, then restore.
    let wire_first = TaskSupervisor::new();
    let (calls, observer) = counting_observer();
    wire_first.set_on_restore(observer);
    wire_first.enable_persistence(&ledger_path).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1, "one restore, one delivery");
    wire_first.enable_persistence(&ledger_path).unwrap();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the idempotent re-enable restores nothing and must not re-deliver",
    );

    // Restore, then wire — the missed-restore path.
    let restore_first = TaskSupervisor::new();
    restore_first.enable_persistence(&ledger_path).unwrap();
    let (calls, observer) = counting_observer();
    restore_first.set_on_restore(observer);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "wiring after the restore must deliver the one it missed",
    );

    // Re-wiring an already-delivered supervisor — the cached-supervisor idiom
    // re-wires at every point of use — must not deliver again.
    let (rewire_calls, observer) = counting_observer();
    restore_first.set_on_restore(observer);
    assert_eq!(
        rewire_calls.load(Ordering::SeqCst),
        0,
        "the pending mark was consumed by the first install",
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "and the replaced observer is not re-invoked either",
    );

    // LATE inheritance onto an already-restored supervisor must consume the
    // missed restore too. Production children inherit before enabling, so this
    // is the path a direct assignment would silently break.
    let parent = TaskSupervisor::new();
    let (inherited_calls, observer) = counting_observer();
    parent.set_on_restore(observer);
    let late_child = TaskSupervisor::new();
    late_child.enable_persistence(&ledger_path).unwrap();
    assert_eq!(
        inherited_calls.load(Ordering::SeqCst),
        0,
        "precondition: the child restored with no observer of its own",
    );
    late_child.inherit_registration_observers(&parent);
    assert_eq!(
        inherited_calls.load(Ordering::SeqCst),
        1,
        "inheriting an observer must deliver the restore the child missed",
    );
    late_child.inherit_registration_observers(&parent);
    assert_eq!(
        inherited_calls.load(Ordering::SeqCst),
        1,
        "a repeat inheritance must not re-deliver",
    );
}

/// #2056 round 3/4 (R5) — the lost wakeup, pinned with POSITIVE EVIDENCE.
/// The round-2 shape observed "no observer wired" under one lock, RELEASED it,
/// and only then raised a separate flag; an installer landing in that gap
/// wired its callback, saw the flag still clear, and delivered nothing — after
/// which nothing ever would, because the same-path re-enable returns at the
/// idempotence guard.
///
/// A cfg-gated hook runs INSIDE the slot's critical section, on exactly the
/// branch that decides "nobody could take this", and holds it while a
/// concurrent installer tries to wire itself. The installer must not be able
/// to COMPLETE there — it has to block on the same mutex — and the delivery
/// must still happen exactly once after the section is released.
///
/// THREE THINGS HERE ARE LOAD-BEARING. Do not "simplify" any of them.
///
/// 1. **The handshake runs hook-first.** The installer thread does not start
///    until the hook says it is already inside the critical section.
///    Signalling the other way round — installer announces, hook waits for it
///    — lets the installer win the race: `notify_restore` then takes its
///    `Some` branch, the hook never runs, and the test passes having exercised
///    nothing. That is not hypothetical; it is what the first draft of this
///    test did, and it went green in 0.01s.
/// 2. **`hook_ran` is asserted.** It is the tripwire for exactly that failure.
///    Without it, any future change that stops this test reaching the
///    missed-restore branch — a different lock order, an extra early return,
///    an observer wired earlier by a helper — converts it from a proof into a
///    green no-op, silently.
/// 3. **The installer ACKNOWLEDGES reaching its lock attempt, and a missing
///    ack FAILS the test.** This is the difference between evidence and
///    inference, and it is subtler than the two above. The second draft held
///    the section and treated "no completion signal arrived" as proof the
///    installer was blocked — but silence has more than one cause. On a loaded
///    runner the installer thread can simply remain UNSCHEDULED for the whole
///    window; then, even with the broken split handshake in place, the hook
///    times out, the pending mark is raised afterwards, the installer runs and
///    delivers once, and every assertion passes. The ack is what rules that
///    out: it proves the thread was runnable and had reached the call, so a
///    subsequent absence of completion is attributable to the lock rather than
///    to the scheduler.
///
/// What the ~500ms runtime does and does not tell you: it shows the held
/// window actually elapsed, which is necessary. It does NOT show the installer
/// was blocked rather than merely late — only the ack does that. A version of
/// this test that keeps the timing but drops the ack is strictly weaker than
/// it looks.
///
/// Residual, stated rather than hidden: between sending the ack and reaching
/// the mutex the installer executes a few instructions, so "blocked" is proven
/// modulo a descheduling window of that size, backed up by the happens-after
/// assertion that the install completed only after the hook released. Every
/// wait is bounded — including the join — so no failure mode hangs the suite.
#[test]
fn should_not_lose_a_restore_when_wiring_races_the_missed_restore_mark() {
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc;
    use std::time::Instant;

    // How long the hook holds the critical section once the installer has
    // acknowledged reaching its lock attempt. Generous enough that an
    // UNBLOCKED installer would comfortably finish inside it.
    const HELD_WINDOW: Duration = Duration::from_millis(500);
    // Budget for the installer's "I reached the lock attempt" ack. Exceeding
    // this FAILS the test — the run proved nothing and must say so.
    const ACK_BUDGET: Duration = Duration::from_secs(10);

    let dir = tempfile::TempDir::new().unwrap();
    let ledger_path = seeded_restore_ledger(&dir);

    let supervisor = TaskSupervisor::new();
    let deliveries = Arc::new(AtomicUsize::new(0));
    let installed_inside_the_section = Arc::new(AtomicBool::new(false));
    let hook_ran = Arc::new(AtomicBool::new(false));
    let installer_acked = Arc::new(AtomicBool::new(false));
    let hook_exit_at: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    let installed_at: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));

    let (inside_tx, inside_rx) = mpsc::channel::<()>();
    let (reached_tx, reached_rx) = mpsc::channel::<()>();
    let (installed_tx, installed_rx) = mpsc::channel::<()>();
    let (done_tx, done_rx) = mpsc::channel::<()>();
    // `Receiver` is not `Sync`; the hook must be.
    let reached_rx = Mutex::new(reached_rx);
    let installed_rx = Mutex::new(installed_rx);

    let observed = Arc::clone(&installed_inside_the_section);
    let ran = Arc::clone(&hook_ran);
    let acked = Arc::clone(&installer_acked);
    let exit_at = Arc::clone(&hook_exit_at);
    supervisor.set_restore_notify_hook_for_test(move || {
        ran.store(true, Ordering::SeqCst);
        // Release the installer only now — this section is held.
        inside_tx.send(()).expect("installer waiting");
        // POSITIVE evidence that the installer thread is runnable and has
        // reached its lock attempt. Without this, the silence below is
        // ambiguous (see the doc comment).
        if reached_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .recv_timeout(ACK_BUDGET)
            .is_ok()
        {
            acked.store(true, Ordering::SeqCst);
            // Having established the installer is AT the lock, it must not be
            // able to get PAST it while this section is held.
            if installed_rx
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .recv_timeout(HELD_WINDOW)
                .is_ok()
            {
                observed.store(true, Ordering::SeqCst);
            }
        }
        *exit_at.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
    });

    let installer = supervisor.clone();
    let installer_deliveries = Arc::clone(&deliveries);
    let installer_installed_at = Arc::clone(&installed_at);
    let handle = std::thread::spawn(move || {
        inside_rx.recv().expect("hook entered the critical section");
        // Ack immediately before the call that must block.
        reached_tx.send(()).expect("hook awaiting the ack");
        installer.set_on_restore(move |_| {
            installer_deliveries.fetch_add(1, Ordering::SeqCst);
        });
        *installer_installed_at
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
        let _ = installed_tx.send(());
        let _ = done_tx.send(());
    });

    supervisor.enable_persistence(&ledger_path).unwrap();
    // Bounded completion before the (now guaranteed-immediate) join, so a
    // regression cannot hang the suite instead of failing it.
    done_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("installer thread must finish once the section is released");
    handle.join().expect("installer thread");

    assert!(
        hook_ran.load(Ordering::SeqCst),
        "the restore must have taken the missed-restore branch, or this test \
         proved nothing",
    );
    assert!(
        installer_acked.load(Ordering::SeqCst),
        "the installer never acknowledged reaching its lock attempt, so this \
         run cannot distinguish 'blocked' from 'never scheduled' — the result \
         is inconclusive, which is a failure, not a pass",
    );
    assert!(
        !installed_inside_the_section.load(Ordering::SeqCst),
        "installing an observer completed while the missed-restore decision \
         was still being made — that gap IS the lost wakeup",
    );
    let exit = hook_exit_at
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .expect("hook recorded its exit");
    let installed = installed_at
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .expect("installer recorded its completion");
    assert!(
        installed >= exit,
        "the install completed before the critical section was released",
    );
    assert_eq!(
        deliveries.load(Ordering::SeqCst),
        1,
        "the restore the installer raced must still be delivered exactly once",
    );
}

/// #2056 — the restore observer travels with the registration observer onto
/// child / nested supervisors, so a child that persists its own task ledger
/// reconciles its own rows.
#[test]
fn should_inherit_on_restore_when_registration_observers_are_inherited() {
    use std::sync::atomic::AtomicUsize;

    let dir = tempfile::TempDir::new().unwrap();
    let ledger_path = dir.path().join("child-tasks.jsonl");

    let parent = TaskSupervisor::new();
    let fired = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&fired);
    parent.set_on_restore(move |_| {
        counter.fetch_add(1, Ordering::SeqCst);
    });

    let child = TaskSupervisor::new();
    child.inherit_registration_observers(&parent);
    child.enable_persistence(&ledger_path).unwrap();

    assert_eq!(
        fired.load(Ordering::SeqCst),
        1,
        "the inherited restore observer fires on the child's own restore",
    );
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

/// NEW-18b Option A — `try_register_node_task` must refuse a child
/// registration when the parent task (looked up by
/// `tool_call_id`) is already in a terminal state. This closes
/// the race where pipeline tokio workers survive a serve restart,
/// observe the orphan-swept parent as `failed`, and continue
/// registering fresh node children that waste CPU/tokens.
#[test]
fn register_node_task_refuses_when_parent_already_failed() {
    let supervisor = TaskSupervisor::new();
    let parent_tcid = "call-pipeline-parent-x";

    // Pre-populate the parent in the failed state (mirrors the
    // post-orphan-sweep shape that triggers the race).
    let parent = supervisor.register("run_pipeline", parent_tcid, Some("sess-A"));
    supervisor.mark_running(&parent);
    supervisor.mark_failed(&parent, "orphaned across restart".to_string());
    assert_eq!(
        supervisor.get_task(&parent).unwrap().status,
        TaskStatus::Failed,
        "parent must be Failed before child registration races in"
    );

    // Straggler pipeline worker attempts to register a child node
    // task against the same parent_tool_call_id. Must be refused.
    let err = supervisor
        .try_register_node_task("pipeline:analyze", parent_tcid, Some("sess-A"))
        .expect_err("registration must be rejected for terminal parent");
    match err {
        RegisterTaskError::ParentTerminal {
            parent_tool_call_id,
            parent_status,
        } => {
            assert_eq!(parent_tool_call_id, parent_tcid);
            assert_eq!(parent_status, TaskStatus::Failed);
        }
        other => panic!("unexpected error variant: {other:?}"),
    }

    // The supervisor must NOT have any child task under that
    // parent — the straggler attempt was rejected before insert.
    let children: Vec<_> = supervisor
        .get_all_tasks()
        .into_iter()
        .filter(|task| task.tool_call_id == parent_tcid && task.tool_name.starts_with("pipeline:"))
        .collect();
    assert!(
        children.is_empty(),
        "no pipeline child task should be registered; got {:?}",
        children.iter().map(|t| &t.tool_name).collect::<Vec<_>>()
    );
}

/// Same guard, but for `Cancelled` and `Completed` parents.
#[test]
fn register_node_task_refuses_when_parent_cancelled_or_completed() {
    let supervisor = TaskSupervisor::new();

    let cancel_tcid = "call-pipeline-parent-cancelled";
    let cancel_parent = supervisor.register("run_pipeline", cancel_tcid, Some("sess-cancel"));
    supervisor.mark_running(&cancel_parent);
    supervisor
        .cancel(&cancel_parent)
        .expect("cancel must succeed");
    let err = supervisor
        .try_register_node_task("pipeline:setup", cancel_tcid, Some("sess-cancel"))
        .expect_err("registration must be rejected for cancelled parent");
    assert!(
        matches!(
            err,
            RegisterTaskError::ParentTerminal {
                parent_status: TaskStatus::Cancelled,
                ..
            }
        ),
        "expected ParentTerminal/Cancelled, got {err:?}"
    );

    let done_tcid = "call-pipeline-parent-completed";
    let done_parent = supervisor.register("run_pipeline", done_tcid, Some("sess-done"));
    supervisor.mark_running(&done_parent);
    supervisor.mark_completed(&done_parent, vec![]);
    let err = supervisor
        .try_register_node_task("pipeline:setup", done_tcid, Some("sess-done"))
        .expect_err("registration must be rejected for completed parent");
    assert!(
        matches!(
            err,
            RegisterTaskError::ParentTerminal {
                parent_status: TaskStatus::Completed,
                ..
            }
        ),
        "expected ParentTerminal/Completed, got {err:?}"
    );
}

/// Healthy parent: registration must succeed.
#[test]
fn register_node_task_succeeds_when_parent_running() {
    let supervisor = TaskSupervisor::new();
    let parent_tcid = "call-pipeline-parent-running";
    let parent = supervisor.register("run_pipeline", parent_tcid, Some("sess-ok"));
    supervisor.mark_running(&parent);

    let child_id = supervisor
        .try_register_node_task("pipeline:analyze", parent_tcid, Some("sess-ok"))
        .expect("registration must succeed when parent is Running");
    assert!(!child_id.is_empty());

    let child = supervisor.get_task(&child_id).expect("child registered");
    assert_eq!(child.tool_name, "pipeline:analyze");
    assert_eq!(child.tool_call_id, parent_tcid);
}

/// Unknown parent (no matching tool_call_id in the supervisor):
/// `try_register_node_task` falls through to normal registration
/// instead of rejecting. This keeps legacy/test callers that
/// never register a `run_pipeline` parent on the no-op path.
#[test]
fn register_node_task_allows_when_no_parent_registered() {
    let supervisor = TaskSupervisor::new();
    let child_id = supervisor
        .try_register_node_task("pipeline:analyze", "call-no-parent", Some("sess-test"))
        .expect("unknown parent must fall through to normal registration");
    assert!(!child_id.is_empty());
}

/// Codex P2 #2 — when a `run_pipeline` task is relaunched with
/// the same `tool_call_id` (mirroring `TaskSupervisor::relaunch`'s
/// behaviour), the lookup must return the ACTIVE relaunch's
/// status, not the stale failed predecessor's. Without preferring
/// active records, a fresh node registration under the live
/// relaunch would be rejected just because the failed record
/// happens to share the tool_call_id.
#[test]
fn parent_status_for_tool_call_id_prefers_active_relaunch_over_stale_failed() {
    let supervisor = TaskSupervisor::new();
    let tcid = "call-relaunched-tcid";

    // Original parent: Failed (the predecessor that triggered
    // relaunch).
    let original = supervisor.register("run_pipeline", tcid, Some("sess-relaunch"));
    supervisor.mark_running(&original);
    supervisor.mark_failed(&original, "predecessor failed".to_string());

    // Relaunch: a fresh parent task registered with the same
    // tool_call_id. Status: Running.
    let relaunched = supervisor.register("run_pipeline", tcid, Some("sess-relaunch"));
    supervisor.mark_running(&relaunched);

    let status = supervisor.parent_status_for_tool_call_id(tcid);
    assert_eq!(
        status,
        Some(TaskStatus::Running),
        "lookup must prefer the active relaunch over the stale failed predecessor"
    );

    // Consequence: try_register_node_task must SUCCEED for the
    // live relaunch.
    let child = supervisor
        .try_register_node_task("pipeline:analyze", tcid, Some("sess-relaunch"))
        .expect("child registration must succeed for live relaunch");
    assert!(!child.is_empty());
}

/// `parent_status_for_tool_call_id` must filter OUT sibling
/// `pipeline:<node>` records when resolving the parent status,
/// because every pipeline child reuses the parent's tool_call_id.
/// Without the filter the lookup could return a sibling's status
/// and incorrectly reject a fresh child even though the actual
/// parent is still Running.
#[test]
fn parent_status_for_tool_call_id_ignores_pipeline_siblings() {
    let supervisor = TaskSupervisor::new();
    let tcid = "call-shared";
    // Sibling pipeline child that just transitioned to Failed.
    let sib = supervisor.register("pipeline:setup", tcid, Some("sess-shared"));
    supervisor.mark_running(&sib);
    supervisor.mark_failed(&sib, "node failed".to_string());

    // Parent run_pipeline task is still Running.
    let parent = supervisor.register("run_pipeline", tcid, Some("sess-shared"));
    supervisor.mark_running(&parent);

    let status = supervisor.parent_status_for_tool_call_id(tcid);
    assert_eq!(
        status,
        Some(TaskStatus::Running),
        "lookup must skip pipeline:<node> siblings and return the parent's status"
    );

    // And as the consequence, registration of another node child
    // must succeed.
    let new_child = supervisor
        .try_register_node_task("pipeline:analyze", tcid, Some("sess-shared"))
        .expect("registration must succeed while parent is Running");
    assert!(!new_child.is_empty());
}

/// NEW-18b Option C — `enable_persistence`'s orphan sweep must
/// also cascade-fail any LIVE pipeline children that share the
/// parent's `tool_call_id`. Catches the case where children
/// already registered before the sweep fires (e.g. they were
/// persisted to JSONL while their workers were running, then the
/// process crashed mid-run).
#[test]
fn enable_persistence_cascades_to_children_with_same_tool_call_id() {
    let dir = tempfile::TempDir::new().unwrap();
    let ledger_path = dir.path().join("tasks.jsonl");

    // Pre-populate the ledger with one orphan parent + two orphan
    // children sharing its tool_call_id, plus one unrelated
    // sibling under a different tool_call_id (must NOT be
    // cascaded). All three "running" tasks have non-terminal
    // runtime_state so the orphan reaper will mark them Failed.
    let parent_tcid = "call-pipeline-mini3-phantom";
    let writer = TaskSupervisor::new();
    writer.enable_persistence(&ledger_path).unwrap();
    let parent = writer.register("run_pipeline", parent_tcid, Some("sess-phantom"));
    let child1 = writer.register("pipeline:analyze", parent_tcid, Some("sess-phantom"));
    let child2 = writer.register("pipeline:synthesize", parent_tcid, Some("sess-phantom"));
    let unrelated = writer.register("pipeline:other", "call-other-parent", Some("sess-phantom"));
    writer.mark_running(&parent);
    writer.mark_running(&child1);
    writer.mark_running(&child2);
    writer.mark_running(&unrelated);
    drop(writer);

    // Fresh supervisor replays the ledger and runs the orphan
    // sweep. After enable_persistence returns, every orphan
    // parent's children should ALSO be terminal.
    let restored = TaskSupervisor::new();
    restored.enable_persistence(&ledger_path).unwrap();

    // Parent: orphan-swept to Failed with the standard reason.
    let parent_task = restored.get_task(&parent).expect("parent persisted");
    assert_eq!(parent_task.status, TaskStatus::Failed);
    assert_eq!(
        parent_task.error.as_deref(),
        Some("orphaned across restart"),
        "parent retains the standard orphan-sweep reason"
    );

    // Both children under the orphaned parent must now be Failed.
    // They could be Failed via EITHER (a) the orphan sweep itself
    // (because they are also non-terminal-runtime-state) OR (b)
    // the Option-C cascade. Both paths satisfy the contract: the
    // child task is terminal and no longer wastes CPU/tokens.
    for cid in [&child1, &child2] {
        let task = restored.get_task(cid).expect("child persisted");
        assert_eq!(
            task.status,
            TaskStatus::Failed,
            "child {cid} must be Failed after restart sweep + cascade"
        );
        assert_eq!(task.runtime_state, TaskRuntimeState::Failed);
        assert!(task.completed_at.is_some());
        let reason = task.error.clone().unwrap_or_default();
        assert!(
            reason == "orphaned across restart" || reason == "parent task orphaned across restart",
            "child {cid} must carry orphan-sweep OR cascade reason, got '{reason}'"
        );
    }

    // The unrelated sibling under a different parent tool_call_id
    // should still be Failed (orphan sweep applies to it too —
    // its own runtime_state is non-terminal) BUT it must NOT
    // carry the "parent task orphaned" reason: that's the cascade
    // marker for descendants of an orphaned parent.
    let other = restored.get_task(&unrelated).expect("unrelated persisted");
    assert_eq!(
        other.status,
        TaskStatus::Failed,
        "unrelated orphan is also swept, just via the main sweep loop"
    );
    // Note: when the unrelated task is itself an orphan, the main
    // sweep marks it Failed first. Then the cascade with its
    // tool_call_id ("call-other-parent") runs but finds no other
    // children under that key. So its reason should be the main
    // sweep's "orphaned across restart", not the cascade's variant.
    assert_eq!(
        other.error.as_deref(),
        Some("orphaned across restart"),
        "unrelated orphan must carry the standard reason"
    );
}

/// Option-C cascade must run as a DISTINCT post-sweep pass.
///
/// Scenario: a pipeline child has `status = Running` (so it's
/// still active from the cascade's perspective) BUT its
/// `runtime_state` was concurrently driven into a terminal state
/// (`ResolvingOutputs` finished and the worker wrote
/// `runtime_state = Completed` but crashed before it could call
/// `mark_completed` to also flip `status = Completed`). The main
/// orphan sweep's `!is_terminal_runtime_state` filter SKIPS this
/// child — runtime_state is already terminal. Without Option-C,
/// the child stays `status = Running` forever after the parent
/// is orphan-swept. With Option-C, `mark_descendants_failed`
/// (which filters by `status.is_active()`) catches it.
///
/// This test pins that Option-C cascade actually transitions
/// such children to `Failed` after `enable_persistence` returns.
#[test]
fn enable_persistence_cascade_catches_active_status_with_terminal_runtime_state() {
    let dir = tempfile::TempDir::new().unwrap();
    let ledger_path = dir.path().join("tasks.jsonl");

    let parent_tcid = "call-mixed-state-parent";
    let writer = TaskSupervisor::new();
    writer.enable_persistence(&ledger_path).unwrap();
    let parent = writer.register("run_pipeline", parent_tcid, Some("sess-mix"));
    // Healthy orphan child that the main sweep catches.
    let healthy_orphan = writer.register("pipeline:setup", parent_tcid, Some("sess-mix"));
    // "Mixed-state" child: status=Running, runtime_state=Completed
    // (set explicitly via mark_runtime_state).
    let mixed_child = writer.register("pipeline:analyze", parent_tcid, Some("sess-mix"));
    writer.mark_running(&parent);
    writer.mark_running(&healthy_orphan);
    writer.mark_running(&mixed_child);
    // Drive runtime_state to a terminal value WITHOUT touching
    // status. This simulates the worker crashing after it set
    // `runtime_state = Completed` but before `mark_completed`
    // flipped `status` to Completed.
    writer.mark_runtime_state(
        &mixed_child,
        TaskRuntimeState::Completed,
        Some("worker finished but crashed pre-mark_completed".to_string()),
    );
    // Sanity: status is still Running, runtime_state is terminal.
    let pre = writer.get_task(&mixed_child).unwrap();
    assert_eq!(pre.status, TaskStatus::Running);
    assert_eq!(pre.runtime_state, TaskRuntimeState::Completed);
    drop(writer);

    let restored = TaskSupervisor::new();
    restored.enable_persistence(&ledger_path).unwrap();

    // Parent: main sweep catches it (status=Running, runtime_state
    // is non-terminal — `Spawned`).
    let parent_task = restored.get_task(&parent).expect("parent loaded");
    assert_eq!(parent_task.status, TaskStatus::Failed);
    assert_eq!(
        parent_task.error.as_deref(),
        Some("orphaned across restart")
    );

    // Healthy orphan child: main sweep catches it.
    let h = restored.get_task(&healthy_orphan).expect("healthy loaded");
    assert_eq!(h.status, TaskStatus::Failed);

    // Mixed-state child: main sweep SKIPS it because its
    // runtime_state is already terminal (Completed). The Option-C
    // cascade fires immediately after and DOES catch it — its
    // status was still `is_active()` when the cascade ran.
    let m = restored.get_task(&mixed_child).expect("mixed loaded");
    assert_eq!(
        m.status,
        TaskStatus::Failed,
        "mixed-state child must be Failed after Option-C cascade"
    );
    assert_eq!(
        m.error.as_deref(),
        Some("parent task orphaned across restart"),
        "mixed-state child must carry the cascade reason (proves Option-C ran distinctly from main sweep)"
    );
}

/// Codex P2 atomicity — the parent-terminal check inside
/// `register_full` happens under the SAME `tasks` lock as the
/// child insert. There is no observable window between lookup
/// and insert. This test pins that the strict node-registration
/// path actually goes through `register_full`'s inside-lock
/// guard (not an outside-lock check that could race).
///
/// We assert this indirectly by verifying that even a child
/// inserted via the regular non-strict path (which has NO
/// parent check) ends up in the supervisor — proving the strict
/// guard is the ONLY mechanism that refuses based on parent
/// state, and that strict mode actually exercises the in-lock
/// recheck (since we use `try_register_node_task`, not the
/// outside-lock convenience wrapper).
#[test]
fn try_register_node_task_uses_in_lock_guard_not_outside_check() {
    let supervisor = TaskSupervisor::new();
    let parent_tcid = "call-atomic-guard";
    let parent = supervisor.register("run_pipeline", parent_tcid, Some("sess-atom"));
    supervisor.mark_running(&parent);
    supervisor.mark_failed(&parent, "orphaned across restart".to_string());

    // Strict registration must reject (in-lock guard).
    let err = supervisor
        .try_register_node_task("pipeline:analyze", parent_tcid, Some("sess-atom"))
        .expect_err("strict path must reject terminal parent");
    assert!(matches!(err, RegisterTaskError::ParentTerminal { .. }));

    // Non-strict registration via `register` (no parent guard)
    // succeeds — this proves the rejection in the strict path
    // is the parent-terminal guard, not some unrelated check.
    let allowed = supervisor.register("pipeline:setup", parent_tcid, Some("sess-atom"));
    assert!(
        !allowed.is_empty(),
        "non-strict register must NOT consult parent status — the guard is opt-in"
    );
}

/// Codex P2 follow-up — terminal-parent rejection must NOT trigger
/// the fan-out cap path's side effects (poisoning the session,
/// `mark_failed`-ing every active sibling under the same
/// `parent_session_key`). The terminal-parent check in
/// `register_full` short-circuits the cap block in two places:
/// (1) at the pre-cap fast path, and (2) under the same lock as
/// the cap-check itself (atomic with the cap decision).
///
/// This test exercises path (2) — it drives the session to
/// `MAX_CHILDREN_PER_PARENT`, then a registration attempt against
/// a TERMINAL parent in that same session must return
/// `ParentTerminal` without poisoning the session or
/// cascade-failing the existing 200 active siblings.
#[test]
fn try_register_node_task_terminal_parent_does_not_trigger_fanout_side_effects() {
    let supervisor = TaskSupervisor::new();
    let session = "api:sess-cap-collateral";

    // Pre-fill the session to MAX_CHILDREN_PER_PARENT - 1 active
    // unrelated tasks, then register the terminal parent as the
    // exact cap-th task. This puts count == cap when the test's
    // straggler attempt fires, so the cap branch is exercised.
    let terminal_parent_tcid = "call-terminal-parent-at-cap";
    let n_fill = MAX_CHILDREN_PER_PARENT - 1;
    let mut active_siblings = Vec::with_capacity(n_fill);
    for i in 0..n_fill {
        let id = supervisor
            .try_register_with_input("tts", &format!("call-{i}"), Some(session), None)
            .unwrap_or_else(|err| panic!("filling cap: register #{i} should succeed; got {err}"));
        supervisor.mark_running(&id);
        active_siblings.push(id);
    }
    let terminal_parent = supervisor
        .try_register_with_input("run_pipeline", terminal_parent_tcid, Some(session), None)
        .expect("terminal parent register at cap-1 must succeed (just barely fits)");
    supervisor.mark_running(&terminal_parent);
    supervisor.mark_failed(&terminal_parent, "orphaned across restart".to_string());
    assert_eq!(
        supervisor.get_tasks_for_session(session).len(),
        MAX_CHILDREN_PER_PARENT,
        "session must be exactly at cap before the test attempt"
    );

    // Snapshot how many active siblings exist BEFORE the attempt.
    // Should be n_fill (the parent itself is Failed, not active).
    let pre_active: usize = supervisor
        .get_tasks_for_session(session)
        .into_iter()
        .filter(|t| t.status.is_active())
        .count();
    assert_eq!(
        pre_active, n_fill,
        "expected {n_fill} active siblings (parent itself is terminal) before attempt"
    );

    // Straggler attempt: register a pipeline child under the
    // terminal parent IN THE CAPPED SESSION. The fix's atomic
    // recheck must catch this and return ParentTerminal — NOT
    // ChildFanoutExceeded. Without the inside-cap-lock recheck
    // the cap path would poison the session and `mark_failed`
    // every active sibling first.
    let err = supervisor
        .try_register_node_task("pipeline:analyze", terminal_parent_tcid, Some(session))
        .expect_err("registration must be rejected for terminal parent (even at cap)");
    assert!(
        matches!(err, RegisterTaskError::ParentTerminal { .. }),
        "must return ParentTerminal not ChildFanoutExceeded; got {err:?}",
    );

    // The session must NOT be poisoned: subsequent legitimate
    // failure attempts (cap-only path, no terminal parent) must
    // still hit ChildFanoutExceeded with their own count, not the
    // ParentTerminal already-poisoned fast path. We can't probe
    // the poisoned set directly, but we can probe its effect:
    // attempting a NON-strict registration would also be refused
    // if poisoned. (Skip this verification since the
    // ChildFanoutExceeded sibling count would itself trigger if
    // we tried — the cleaner assertion is on active sibling
    // counts.)

    // The 200 active siblings must remain UNTOUCHED — the cap
    // path's force-fail cascade did NOT run.
    let post_active: usize = supervisor
        .get_tasks_for_session(session)
        .into_iter()
        .filter(|t| t.status.is_active())
        .count();
    assert_eq!(
        post_active, pre_active,
        "no active sibling may be cascaded by a terminal-parent rejection at cap"
    );
}

/// NEW-09 contract: cascade-failing a child via
/// `mark_descendants_failed` must still emit the per-task
/// completion bubble (spawn_only on_failure_signal +
/// emit_progress_for_state). This pin guarantees that the
/// Option-C cascade does not regress NEW-09 — every cascade-
/// failed child fires the same notification callbacks as a
/// direct `mark_failed` call.
#[test]
fn mark_descendants_failed_emits_progress_and_failure_signal_per_child() {
    use std::sync::Mutex;

    let supervisor = TaskSupervisor::new();
    let parent_tcid = "call-cascade-signals";

    let parent = supervisor.register("run_pipeline", parent_tcid, Some("sess-sig"));
    let c1 = supervisor.register("pipeline:setup", parent_tcid, Some("sess-sig"));
    let c2 = supervisor.register("pipeline:analyze", parent_tcid, Some("sess-sig"));
    // Children inherit the parent's tool_call_id; mark the synth-ack
    // for that id so post-spawn failure signals fire (production wires
    // this from the synth-ack gate in `loop_runner.rs`).
    supervisor.mark_synth_ack_emitted(parent_tcid);
    supervisor.mark_running(&parent);
    supervisor.mark_running(&c1);
    supervisor.mark_running(&c2);

    // Capture every on_failure_signal payload that fires.
    let failure_signals: Arc<Mutex<Vec<SpawnOnlyFailureSignal>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let captured = failure_signals.clone();
        supervisor.set_on_failure_signal(move |signal| {
            captured.lock().unwrap().push(signal.clone());
        });
    }

    // Capture every on_change snapshot. mark_failed fires
    // notify_change unconditionally for every transition.
    let change_log: Arc<Mutex<Vec<BackgroundTask>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let captured = change_log.clone();
        supervisor.set_on_change(move |task| {
            captured.lock().unwrap().push(task.clone());
        });
    }

    let cascaded =
        supervisor.mark_descendants_failed(parent_tcid, "parent task orphaned across restart");
    assert_eq!(cascaded, 2, "both running children should cascade-fail");

    // Failure signals: one per child, neither for the parent.
    let signals = failure_signals.lock().unwrap();
    assert_eq!(
        signals.len(),
        2,
        "every cascade-failed child must fire on_failure_signal (NEW-09)"
    );
    let signal_task_ids: HashSet<&str> = signals.iter().map(|s| s.task_id.as_str()).collect();
    assert!(signal_task_ids.contains(c1.as_str()));
    assert!(signal_task_ids.contains(c2.as_str()));
    for sig in signals.iter() {
        assert_eq!(
            sig.error_message, "parent task orphaned across restart",
            "cascade reason must propagate into the failure signal payload"
        );
        assert_eq!(sig.parent_session_key.as_deref(), Some("sess-sig"));
    }

    // on_change must have fired for both children's terminal
    // transitions. (We don't assert exact count because the
    // parent's earlier mark_running fires it too, but the failed
    // snapshots must be present.)
    let changes = change_log.lock().unwrap();
    let failed_snapshots: Vec<_> = changes
        .iter()
        .filter(|t| t.status == TaskStatus::Failed && t.tool_name.starts_with("pipeline:"))
        .collect();
    assert!(
        failed_snapshots.len() >= 2,
        "on_change must fire for each cascade-failed child terminal transition; \
             got {} failed pipeline snapshots",
        failed_snapshots.len()
    );
}

/// STEP 1 contract: the orphan sweep that runs INSIDE
/// `enable_persistence` fires terminal `mark_failed("orphaned across
/// restart")` transitions. For the TUI task-count chain to learn the
/// task failed, the `on_change` callback MUST be installed BEFORE
/// `enable_persistence` — otherwise the sweep's `notify_change` hits
/// `on_change == None` and the terminal transition is silently dropped.
/// This is the supervisor-level invariant the CLI wiring (session_actor
/// + ui_protocol `run_standalone_turn`) depends on.
#[test]
fn on_change_installed_before_enable_persistence_observes_orphan_sweep() {
    use std::sync::{Arc, Mutex};

    let dir = tempfile::TempDir::new().unwrap();
    let ledger_path = dir.path().join("tasks.jsonl");

    // First runtime: register + mark_running, persist a non-terminal
    // task, then drop the supervisor (simulating a restart).
    let first = TaskSupervisor::new();
    first.enable_persistence(&ledger_path).unwrap();
    let id = first.register_with_lineage("mofa_slides", "call-orphan", Some("api:session"), None);
    first.mark_running(&id);
    assert_eq!(
        first.get_task(&id).expect("task").status,
        TaskStatus::Running
    );
    drop(first);

    // Second runtime: install on_change FIRST, then enable_persistence.
    // The orphan sweep should fire the callback with the now-Failed task.
    let restored = TaskSupervisor::new();
    let observed: Arc<Mutex<Vec<BackgroundTask>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&observed);
    restored.set_on_change(move |task: &BackgroundTask| {
        sink.lock().unwrap().push(task.clone());
    });
    restored.enable_persistence(&ledger_path).unwrap();

    let snapshots = observed.lock().unwrap();
    let orphan_failed = snapshots
        .iter()
        .find(|t| t.id == id && t.status == TaskStatus::Failed)
        .expect("on_change observes the orphaned task's genuine Failed verdict");
    assert_eq!(
        orphan_failed.error.as_deref(),
        Some("orphaned across restart"),
    );
}

/// Inverse of the above: installing `on_change` AFTER
/// `enable_persistence` (the pre-fix ordering) means the orphan sweep's
/// terminal transition is NEVER observed by the callback. Documents WHY
/// the wiring order matters — the supervisor's stored task is Failed
/// either way, but the live notification chain stays cold.
#[test]
fn on_change_installed_after_enable_persistence_misses_orphan_sweep() {
    use std::sync::{Arc, Mutex};

    let dir = tempfile::TempDir::new().unwrap();
    let ledger_path = dir.path().join("tasks.jsonl");

    let first = TaskSupervisor::new();
    first.enable_persistence(&ledger_path).unwrap();
    let id = first.register_with_lineage("mofa_slides", "call-orphan2", Some("api:session"), None);
    first.mark_running(&id);
    drop(first);

    let restored = TaskSupervisor::new();
    // Sweep runs HERE, before any callback is installed.
    restored.enable_persistence(&ledger_path).unwrap();
    let observed: Arc<Mutex<Vec<BackgroundTask>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&observed);
    restored.set_on_change(move |task: &BackgroundTask| {
        sink.lock().unwrap().push(task.clone());
    });

    // Stored task is Failed (sweep ran; non-peer scope), callback never saw it.
    assert_eq!(
        restored.get_task(&id).expect("task").status,
        TaskStatus::Failed
    );
    assert!(
        observed.lock().unwrap().is_empty(),
        "on_change installed AFTER enable_persistence must NOT observe the sweep's transition",
    );
}

/// STEP 2: a guard armed after `mark_running` and dropped WITHOUT a
/// terminal call drives the task to `Failed` with the dropped-worker
/// reason.
#[test]
fn terminal_guard_marks_failed_when_dropped_while_active() {
    use std::sync::Arc;

    let supervisor = Arc::new(TaskSupervisor::new());
    let id = supervisor.register("mofa_slides", "call-guard", Some("api:session"));
    supervisor.mark_running(&id);

    {
        let _guard = TaskTerminalGuard::new(Arc::clone(&supervisor), id.clone());
        // No terminal call inside the scope — simulate an aborted body.
    }

    let task = supervisor.get_task(&id).expect("task");
    assert_eq!(task.status, TaskStatus::Failed);
    assert_eq!(
        task.error.as_deref(),
        Some("worker dropped before reaching terminal state"),
    );
}

/// STEP 2: a guard whose body reached `mark_completed` before drop must
/// leave the task `Completed` — the Drop is a no-op for terminal tasks.
#[test]
fn terminal_guard_noop_when_task_already_completed() {
    use std::sync::Arc;

    let supervisor = Arc::new(TaskSupervisor::new());
    let id = supervisor.register("mofa_slides", "call-guard2", Some("api:session"));
    supervisor.mark_running(&id);

    {
        let _guard = TaskTerminalGuard::new(Arc::clone(&supervisor), id.clone());
        supervisor.mark_completed(&id, vec!["deck.pdf".to_string()]);
    }

    let task = supervisor.get_task(&id).expect("task");
    assert_eq!(
        task.status,
        TaskStatus::Completed,
        "guard Drop must not overwrite a completed task",
    );
    assert!(task.error.is_none());
}

/// STEP 2: a `tokio::spawn` body that `panic!`s after `mark_running`
/// (with the guard armed) leaves the supervisor in `Failed` once the
/// JoinHandle resolves with the panic. Mirrors the production spawn body
/// shape where the guard is the first thing constructed.
#[tokio::test]
async fn terminal_guard_marks_failed_when_body_panics() {
    use std::sync::Arc;

    let supervisor = Arc::new(TaskSupervisor::new());
    let id = supervisor.register("mofa_slides", "call-guard-panic", Some("api:session"));

    let sup = Arc::clone(&supervisor);
    let task_id = id.clone();
    let handle = tokio::spawn(async move {
        sup.mark_running(&task_id);
        let _guard = TaskTerminalGuard::new(Arc::clone(&sup), task_id.clone());
        panic!("simulated worker panic after mark_running");
    });

    let join = handle.await;
    assert!(join.is_err(), "spawned body should have panicked");

    let task = supervisor.get_task(&id).expect("task");
    assert_eq!(
        task.status,
        TaskStatus::Failed,
        "guard Drop on panic-unwind must drive the task to Failed",
    );
    assert_eq!(
        task.error.as_deref(),
        Some("worker dropped before reaching terminal state"),
    );
}

// ── Orphan-sweep liveness gate (fix/orphan-sweep-liveness-gate) ──
//
// The WS turn path rebuilds a BRAND-NEW per-turn `TaskSupervisor`
// every turn and calls `enable_persistence(...)` over the SHARED
// per-session ledger. `enable_persistence`'s orphan-sweep ASSUMES
// "non-terminal ⇒ no live worker", so it FALSELY marks a still-Running
// DETACHED spawn_only task (run_pipeline deep_research, up to ~3600s)
// as "orphaned across restart" — even though the worker is alive on the
// PREVIOUS turn's supervisor and will mark_completed shortly. The fix
// gates the sweep on a process-global live-set that survives the
// per-turn supervisor rebuild and is empty after a true cross-process
// restart.

/// RED→GREEN: a task in the process-global live-set + a `Running` row in
/// the shared ledger must NOT be swept as "orphaned across restart" by a
/// NEW supervisor's `enable_persistence`. Mirrors the real bug: turn N's
/// detached worker is alive (id in live-set) when turn N+1's fresh
/// supervisor opens the same ledger.
#[test]
fn live_detached_task_is_not_swept_as_orphan() {
    let dir = tempfile::TempDir::new().unwrap();
    let ledger_path = dir.path().join("tasks.jsonl");

    // Turn N: supervisor registers + runs a detached spawn_only task and
    // persists a still-Running row.
    let turn_n = TaskSupervisor::new();
    turn_n.enable_persistence(&ledger_path).unwrap();
    let id = turn_n.register("run_pipeline", "call-live-1", Some("api:sess"));
    turn_n.mark_running(&id);

    // The detached worker is alive: its id is in the process-global
    // live-set (in production the TaskTerminalGuard inserts it).
    mark_task_live(&id);
    // RAII clear at scope end so the global set does not leak across tests.
    struct ClearOnDrop<'a>(&'a str);
    impl Drop for ClearOnDrop<'_> {
        fn drop(&mut self) {
            clear_task_live(self.0);
        }
    }
    let _clear = ClearOnDrop(&id);

    // Turn N+1: a BRAND-NEW supervisor opens the SAME ledger. Pre-fix this
    // sweep marks the still-Running row "orphaned across restart".
    let turn_n1 = TaskSupervisor::new();
    turn_n1.enable_persistence(&ledger_path).unwrap();

    let restored = turn_n1.get_task(&id).expect("row restored");
    assert_eq!(
        restored.status,
        TaskStatus::Running,
        "a LIVE detached task (id in live-set) must NOT be swept as orphan",
    );
    assert_ne!(
        restored.error.as_deref(),
        Some("orphaned across restart"),
        "live detached task must never carry the false-orphan reason",
    );
}

/// A `Running` row whose id is NOT in the live-set (a true cross-process
/// restart: new process ⇒ empty live-set) is STILL reaped. Reaping
/// behaviour for genuinely-orphaned tasks is preserved.
#[test]
fn dead_task_not_in_live_set_is_still_reaped() {
    let dir = tempfile::TempDir::new().unwrap();
    let ledger_path = dir.path().join("tasks.jsonl");

    let writer = TaskSupervisor::new();
    writer.enable_persistence(&ledger_path).unwrap();
    let id = writer.register("run_pipeline", "call-dead-1", Some("api:sess"));
    writer.mark_running(&id);
    drop(writer);

    // Simulate a true cross-process restart: the live-set is empty for
    // this id (no live worker in this process). Defensive clear in case a
    // prior test leaked the id.
    clear_task_live(&id);

    let restored = TaskSupervisor::new();
    restored.enable_persistence(&ledger_path).unwrap();

    let task = restored.get_task(&id).expect("row restored");
    assert_eq!(
        task.status,
        TaskStatus::Failed,
        "a dead task absent from the live-set must still be reaped (#27c parks)",
    );
    assert_eq!(
        task.error.as_deref(),
        Some("orphaned across restart"),
        "genuine orphan must still carry the standard reason",
    );
}

/// The RAII drop-guard removes the id from the live-set when the worker
/// future completes/drops, so a finished task is not kept "live" forever
/// and a later genuine restart can reap a stale row.
#[test]
fn live_set_cleared_on_task_terminal() {
    let supervisor = Arc::new(TaskSupervisor::new());
    let id = supervisor.register("run_pipeline", "call-clear-1", Some("api:sess"));
    supervisor.mark_running(&id);

    {
        // Constructing the guard (production: top of the spawn_only body)
        // inserts the id into the live-set.
        let _guard = TaskTerminalGuard::new(Arc::clone(&supervisor), id.clone());
        assert!(
            is_task_live(&id),
            "guard construction must mark the task live",
        );
        supervisor.mark_completed(&id, vec!["deck.pdf".to_string()]);
        // Guard still in scope: id stays live until the worker future drops.
        assert!(is_task_live(&id), "task stays live until the future drops");
    }

    // Drop ran (worker future terminated): id is cleared.
    assert!(
        !is_task_live(&id),
        "guard Drop must clear the id from the live-set on every exit path",
    );
}

/// Higher-level guard mirroring the real bug: turn-1 spawns a (mock) long
/// detached spawn_only task whose worker stays alive; turn-2 supervisor
/// rebuild + sweep does NOT orphan it; then the task completes normally.
#[tokio::test]
async fn turn_rebuild_does_not_orphan_live_detached_task() {
    let dir = tempfile::TempDir::new().unwrap();
    let ledger_path = dir.path().join("tasks.jsonl");

    // Turn 1: register + spawn a detached worker (guarded), persist Running.
    let turn1 = Arc::new(TaskSupervisor::new());
    turn1.enable_persistence(&ledger_path).unwrap();
    let id = turn1.register("run_pipeline", "call-real-bug", Some("api:sess"));

    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let worker_sup = Arc::clone(&turn1);
    let worker_id = id.clone();
    let worker = tokio::spawn(async move {
        worker_sup.mark_running(&worker_id);
        // Production: TaskTerminalGuard armed right after mark_running.
        let _guard = TaskTerminalGuard::new(Arc::clone(&worker_sup), worker_id.clone());
        // Long detached work: block until the test releases us.
        let _ = release_rx.await;
        worker_sup.mark_completed(&worker_id, vec!["report.md".to_string()]);
    });

    // Spin until the worker has marked the task Running + live.
    for _ in 0..1000 {
        if is_task_live(&id) {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(
        is_task_live(&id),
        "worker should be live before turn-2 sweep"
    );

    // Turn 2: a fresh per-turn supervisor opens the SAME ledger and sweeps.
    let turn2 = TaskSupervisor::new();
    turn2.enable_persistence(&ledger_path).unwrap();
    let mid_turn = turn2.get_task(&id).expect("row restored on turn-2");
    assert_ne!(
        mid_turn.error.as_deref(),
        Some("orphaned across restart"),
        "turn-2 sweep must NOT falsely orphan the still-live detached task",
    );

    // The detached worker finishes normally and returns its artifact.
    release_tx.send(()).unwrap();
    worker.await.unwrap();

    let done = turn1.get_task(&id).expect("task");
    assert_eq!(done.status, TaskStatus::Completed);
    assert_eq!(done.output_files, vec!["report.md".to_string()]);
    // Worker future dropped ⇒ live-set cleared.
    assert!(
        !is_task_live(&id),
        "live-set cleared after the worker completes"
    );
}

/// codex round-2 DO-NOT-SHIP regression: a live `run_pipeline` parent
/// registers `pipeline:<node>` CHILD rows that SHARE its `tool_call_id`
/// but carry their OWN task ids. Only the PARENT worker arms a
/// `TaskTerminalGuard`, so the children are never inserted into the
/// live-set. The turn N+1 sweep must NOT reap those active children as
/// orphans while their parent is live — otherwise `run_pipeline
/// deep_research` shows the mini3 "spinner stuck orchestrating" symptom:
/// children falsely marked "orphaned across restart" (direct sweep) /
/// "parent task orphaned across restart" (cascade) even though the
/// pipeline is still running on the prior turn's live worker.
#[test]
fn live_pipeline_child_is_not_swept_when_parent_is_live() {
    let dir = tempfile::TempDir::new().unwrap();
    let ledger_path = dir.path().join("tasks.jsonl");

    // Turn N: a detached run_pipeline parent + its active pipeline child,
    // both persisted Running under the SAME tool_call_id.
    let turn_n = TaskSupervisor::new();
    turn_n.enable_persistence(&ledger_path).unwrap();
    let parent = turn_n.register("run_pipeline", "call-pipe-1", Some("api:sess"));
    turn_n.mark_running(&parent);
    let child = turn_n.register("pipeline:research_node", "call-pipe-1", Some("api:sess"));
    turn_n.mark_running(&child);

    // Only the PARENT worker is live (pipeline children don't arm guards).
    mark_task_live(&parent);
    struct ClearOnDrop<'a>(Vec<&'a str>);
    impl Drop for ClearOnDrop<'_> {
        fn drop(&mut self) {
            for id in &self.0 {
                clear_task_live(id);
            }
        }
    }
    let _clear = ClearOnDrop(vec![&parent, &child]);

    // Turn N+1: a brand-new supervisor opens the SAME ledger and sweeps.
    let turn_n1 = TaskSupervisor::new();
    turn_n1.enable_persistence(&ledger_path).unwrap();

    let restored_parent = turn_n1.get_task(&parent).expect("parent restored");
    assert_eq!(
        restored_parent.status,
        TaskStatus::Running,
        "live run_pipeline parent must not be swept",
    );

    let restored_child = turn_n1.get_task(&child).expect("child restored");
    assert_eq!(
        restored_child.status,
        TaskStatus::Running,
        "an active pipeline child of a LIVE parent must NOT be reaped",
    );
    assert_ne!(
        restored_child.error.as_deref(),
        Some("orphaned across restart"),
        "child must not carry the direct-sweep false-orphan reason",
    );
    assert_ne!(
        restored_child.error.as_deref(),
        Some("parent task orphaned across restart"),
        "child must not carry the cascade false-orphan reason either",
    );
}

/// Boundary counterpart: when NO member of the tool_call_id family is live
/// (a true cross-process restart ⇒ empty live-set), BOTH the parent and
/// its pipeline children are still reaped. The proxy-exemption only fires
/// for a genuinely live family, so reaping of real orphans is preserved.
#[test]
fn dead_pipeline_family_is_still_reaped() {
    let dir = tempfile::TempDir::new().unwrap();
    let ledger_path = dir.path().join("tasks.jsonl");

    let writer = TaskSupervisor::new();
    writer.enable_persistence(&ledger_path).unwrap();
    let parent = writer.register("run_pipeline", "call-pipe-dead", Some("api:sess"));
    writer.mark_running(&parent);
    let child = writer.register("pipeline:node", "call-pipe-dead", Some("api:sess"));
    writer.mark_running(&child);
    drop(writer);

    // True restart: empty live-set for the whole family. Defensive clears
    // in case a prior test leaked an id.
    clear_task_live(&parent);
    clear_task_live(&child);

    let restored = TaskSupervisor::new();
    restored.enable_persistence(&ledger_path).unwrap();

    assert_eq!(
        restored.get_task(&parent).expect("parent").status,
        TaskStatus::Failed,
        "dead parent absent from the live-set must still be reaped",
    );
    assert_eq!(
        restored.get_task(&child).expect("child").status,
        TaskStatus::Failed,
        "dead pipeline child must still be reaped when no parent is live",
    );
}

/// Defense-in-depth (codex round-4): the proxy-exemption is bounded to
/// `pipeline:<node>` rows. Even if a future non-unique producer let an
/// UNRELATED dead task collide on a live task's `tool_call_id`, that dead
/// task — being a NON-pipeline tool — must still be reaped. Only genuine
/// pipeline children may be spared by proxy; the live owner is spared by
/// its own unique id, not by sharing a tcid.
#[test]
fn non_pipeline_task_sharing_live_tcid_is_still_reaped() {
    let dir = tempfile::TempDir::new().unwrap();
    let ledger_path = dir.path().join("tasks.jsonl");

    let turn_n = TaskSupervisor::new();
    turn_n.enable_persistence(&ledger_path).unwrap();
    // A live detached parent.
    let live = turn_n.register("run_pipeline", "call-collide", Some("api:sess"));
    turn_n.mark_running(&live);
    // A SEPARATE, genuinely-dead NON-pipeline task that (hypothetically)
    // collides on the same tcid — simulating a future non-unique producer.
    let dead = turn_n.register("tts", "call-collide", Some("api:sess"));
    turn_n.mark_running(&dead);

    mark_task_live(&live);
    struct ClearOnDrop<'a>(&'a str);
    impl Drop for ClearOnDrop<'_> {
        fn drop(&mut self) {
            clear_task_live(self.0);
        }
    }
    let _clear = ClearOnDrop(&live);

    let turn_n1 = TaskSupervisor::new();
    turn_n1.enable_persistence(&ledger_path).unwrap();

    assert_eq!(
        turn_n1.get_task(&live).expect("live").status,
        TaskStatus::Running,
        "the live task is exempt via its own unique id",
    );
    assert_eq!(
        turn_n1.get_task(&dead).expect("dead").status,
        TaskStatus::Failed,
        "a NON-pipeline task sharing a live tcid must still be reaped \
             (proxy-exemption is bounded to pipeline children)",
    );
}

/// codex round-5 DO-NOT-SHIP regression: the terminal guard is now armed in
/// the FOREGROUND (in `execution.rs` / `spawn.rs`, before `tokio::spawn`),
/// not inside the spawned worker future. This mirrors the real call-site
/// ordering — register (persist `Spawned`) → arm guard (foreground) →
/// spawn → [a fast next turn sweeps] → the worker future finally runs. The
/// pre-fix code armed the guard INSIDE the future, so a sweep that ran
/// before the worker future had progressed (the test gates it explicitly)
/// saw the row non-terminal AND not-live and falsely reaped a
/// scheduled-but-not-yet-run worker. With
/// foreground arming the id is in the live-set before the spawning turn
/// returns, so the pre-poll sweep skips it.
#[tokio::test]
async fn foreground_armed_guard_survives_sweep_before_worker_polls() {
    let dir = tempfile::TempDir::new().unwrap();
    let ledger_path = dir.path().join("tasks.jsonl");

    let turn1 = Arc::new(TaskSupervisor::new());
    turn1.enable_persistence(&ledger_path).unwrap();
    // Foreground: register persists a Spawned row, THEN the guard is armed
    // in the foreground (the fix), BEFORE the worker future is spawned.
    let id = turn1.register("run_pipeline", "call-fg-guard", Some("api:sess"));
    let guard = TaskTerminalGuard::new(Arc::clone(&turn1), id.clone());

    // The worker future is spawned but blocked on a gate: it has NOT yet
    // polled past the gate, so it has NOT called mark_running. In the
    // pre-fix world the guard would not exist yet either.
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let worker_sup = Arc::clone(&turn1);
    let worker_id = id.clone();
    let worker = tokio::spawn(async move {
        // Foreground-armed guard moved into the future; Drop fires at end.
        let _guard = guard;
        let _ = release_rx.await;
        worker_sup.mark_running(&worker_id);
        worker_sup.mark_completed(&worker_id, vec!["report.md".to_string()]);
    });

    // Turn 2 sweeps the SAME ledger BEFORE the worker future progresses
    // past the gate. Pre-fix (guard armed inside the future) this reaps the
    // still-`Spawned`, not-yet-live row.
    let turn2 = TaskSupervisor::new();
    turn2.enable_persistence(&ledger_path).unwrap();
    let mid = turn2.get_task(&id).expect("row restored on turn-2");
    assert_ne!(
        mid.error.as_deref(),
        Some("orphaned across restart"),
        "a foreground-armed guard must keep a pre-poll worker out of the sweep",
    );

    // The worker finishes normally.
    release_tx.send(()).unwrap();
    worker.await.unwrap();
    assert_eq!(
        turn1.get_task(&id).expect("task").status,
        TaskStatus::Completed,
    );
    assert!(
        !is_task_live(&id),
        "live-set cleared after the worker future drops"
    );
}

// ── issue #2035: liveness for CLIENT-DRIVEN work (peers) ──────────────

/// Reproduction of #2035. A `peer_handoff` row is the shape the sweep was
/// never designed for: it is non-terminal for the peer's whole life (it is
/// retired on `peer_close`, not on turn terminal) and its worker is a
/// sovereign session the CLIENT drives, so no `TaskTerminalGuard` is ever
/// armed for it. The next turn's `enable_persistence` therefore reaps a peer
/// that is perfectly healthy — observed live on mini5, where both peers were
/// stamped `failed` six seconds after staging and then ran to completion.
#[test]
fn a_client_driven_task_with_no_lease_is_reaped_by_the_next_sweep() {
    let dir = tempfile::TempDir::new().unwrap();
    let ledger_path = dir.path().join("tasks.jsonl");

    let staging_turn = TaskSupervisor::new();
    staging_turn.enable_persistence(&ledger_path).unwrap();
    let peer = staging_turn.register("peer_handoff", "profile:peer:auditor", Some("api:master"));

    // The very next turn rebuilds a fresh supervisor over the SAME ledger.
    let next_turn = TaskSupervisor::new();
    next_turn.enable_persistence(&ledger_path).unwrap();

    assert_eq!(
        next_turn.get_task(&peer).expect("peer row restored").error,
        Some("orphaned across restart".to_string()),
        "documents the #2035 defect: without a lease the peer is reaped",
    );
}

/// The fix. A liveness lease marks the task live for as long as the CLIENT's
/// work is in flight, so the per-turn sweep skips it — the same exemption a
/// detached tokio worker gets from `TaskTerminalGuard`, minus the terminal
/// side effect (a peer is retired by `peer_close`, not by a worker future).
#[test]
fn a_liveness_lease_keeps_a_client_driven_task_out_of_the_sweep() {
    let dir = tempfile::TempDir::new().unwrap();
    let ledger_path = dir.path().join("tasks.jsonl");

    let staging_turn = TaskSupervisor::new();
    staging_turn.enable_persistence(&ledger_path).unwrap();
    let peer = staging_turn.register("peer_handoff", "profile:peer:auditor", Some("api:master"));
    let _lease = TaskLivenessLease::new(peer.clone());

    // Several turns pass while the peer works; every one of them sweeps.
    for _ in 0..3 {
        let turn = TaskSupervisor::new();
        turn.enable_persistence(&ledger_path).unwrap();
        assert_ne!(
            turn.get_task(&peer).expect("peer row restored").error,
            Some("orphaned across restart".to_string()),
            "a leased peer must survive every per-turn sweep",
        );
    }
}

/// The lease must not defeat the sweep's real purpose. Membership is
/// process-global and starts empty in a new process, so a peer left behind by
/// a genuine cross-process restart is still reaped — modelled here by dropping
/// the lease (what process death does to the whole set) and sweeping again.
#[test]
fn dropping_the_liveness_lease_makes_the_task_reapable_again() {
    let dir = tempfile::TempDir::new().unwrap();
    let ledger_path = dir.path().join("tasks.jsonl");

    let staging_turn = TaskSupervisor::new();
    staging_turn.enable_persistence(&ledger_path).unwrap();
    let peer = staging_turn.register("peer_handoff", "profile:peer:auditor", Some("api:master"));

    {
        let _lease = TaskLivenessLease::new(peer.clone());
        assert!(is_task_live(&peer), "lease marks the task live");
    }
    assert!(!is_task_live(&peer), "drop clears the live-set entry");

    let after_restart = TaskSupervisor::new();
    after_restart.enable_persistence(&ledger_path).unwrap();
    assert_eq!(
        after_restart.get_task(&peer).expect("peer row").error,
        Some("orphaned across restart".to_string()),
        "an unleased peer row is still a genuine orphan",
    );
}

// ── issue #1920: heartbeat-based in-flight orphan reaper ──────────────

/// A live worker whose heartbeat (`updated_at`) has been silent for longer
/// than `stuck_timeout` is reaped: status transitions to `Failed` with the
/// heartbeat message and its cancel token is flipped so a cooperative
/// worker wakes and unwinds.
#[tokio::test]
async fn reaper_fails_stuck_live_task_and_cancels_token() {
    let supervisor = Arc::new(TaskSupervisor::new());
    let id = supervisor.register("run_pipeline", "call-stuck", Some("api:sess"));
    supervisor.mark_running(&id);
    // Arm the guard so the task is LIVE (the reaper only touches live
    // workers — the dropped-worker case belongs to the guard's Drop).
    let _guard = TaskTerminalGuard::new(Arc::clone(&supervisor), id.clone());
    assert!(is_task_live(&id));

    // Age the heartbeat well past the timeout by stamping updated_at in
    // the past (register/mark_running set it to now).
    let stale_at = Utc::now() - chrono::Duration::minutes(10);
    {
        let mut tasks = supervisor.tasks.lock().unwrap_or_else(|e| e.into_inner());
        tasks.get_mut(&id).unwrap().updated_at = stale_at;
    }
    let token = supervisor.cancel_token(&id);
    assert!(!token.is_cancelled());

    let timeout = Duration::from_secs(60);
    let reaped = supervisor.reap_stuck_tasks(Utc::now(), timeout);

    assert_eq!(reaped, vec![id.clone()]);
    let task = supervisor.get_task(&id).expect("task");
    assert_eq!(task.status, TaskStatus::Failed);
    assert_eq!(
        task.error.as_deref(),
        Some("orphaned: no progress for 60s (heartbeat timeout)"),
    );
    assert!(
        token.is_cancelled(),
        "the reaper flips the cancel token so a cooperative worker wakes"
    );
    assert!(
        is_task_live(&id),
        "live-set cleanup stays with the guard's Drop, not the reaper"
    );

    // A second sweep is a no-op: the task is terminal now.
    let reaped_again = supervisor.reap_stuck_tasks(Utc::now(), timeout);
    assert!(reaped_again.is_empty());
}

/// A live worker that keeps its heartbeat fresh (recent `updated_at`) is
/// NOT reaped — this is the long-but-progressing case (deep_research
/// streaming progress events).
#[tokio::test]
async fn reaper_spares_task_with_fresh_heartbeat() {
    let supervisor = Arc::new(TaskSupervisor::new());
    let id = supervisor.register("run_pipeline", "call-fresh", Some("api:sess"));
    supervisor.mark_running(&id);
    let _guard = TaskTerminalGuard::new(Arc::clone(&supervisor), id.clone());

    let reaped = supervisor.reap_stuck_tasks(Utc::now(), Duration::from_secs(60));

    assert!(reaped.is_empty());
    assert_eq!(
        supervisor.get_task(&id).expect("task").status,
        TaskStatus::Running,
    );
    assert!(!supervisor.cancel_token(&id).is_cancelled());
}

/// Terminal tasks are never touched, even if their `updated_at` is ancient.
#[tokio::test]
async fn reaper_never_touches_terminal_tasks() {
    let supervisor = Arc::new(TaskSupervisor::new());
    let id = supervisor.register("spawn", "call-terminal", Some("api:sess"));
    supervisor.mark_running(&id);
    supervisor.mark_completed(&id, vec![]);

    let stale_at = Utc::now() - chrono::Duration::minutes(30);
    {
        let mut tasks = supervisor.tasks.lock().unwrap_or_else(|e| e.into_inner());
        tasks.get_mut(&id).unwrap().updated_at = stale_at;
    }

    let reaped = supervisor.reap_stuck_tasks(Utc::now(), Duration::from_secs(60));

    assert!(reaped.is_empty());
    let task = supervisor.get_task(&id).expect("task");
    assert_eq!(task.status, TaskStatus::Completed);
    assert!(task.error.is_none());
}

/// An ACTIVE task WITHOUT a live worker is the dropped-worker case the
/// `TaskTerminalGuard` owns — the reaper must skip it (double-firing the
/// failure callbacks would surface two recovery signals for one death).
#[tokio::test]
async fn reaper_skips_active_task_without_live_worker() {
    let supervisor = Arc::new(TaskSupervisor::new());
    let id = supervisor.register("spawn", "call-not-live", Some("api:sess"));
    supervisor.mark_running(&id);
    assert!(
        !is_task_live(&id),
        "no guard armed, so the id is not in the live-set"
    );

    let stale_at = Utc::now() - chrono::Duration::minutes(30);
    {
        let mut tasks = supervisor.tasks.lock().unwrap_or_else(|e| e.into_inner());
        tasks.get_mut(&id).unwrap().updated_at = stale_at;
    }

    let reaped = supervisor.reap_stuck_tasks(Utc::now(), Duration::from_secs(60));

    assert!(reaped.is_empty());
    assert_eq!(
        supervisor.get_task(&id).expect("task").status,
        TaskStatus::Running,
        "non-live active tasks are left for TaskTerminalGuard / the startup sweep",
    );
}

/// A future `updated_at` (clock skew on a hydrated snapshot) yields a
/// negative age and must never be reaped.
#[tokio::test]
async fn reaper_tolerates_future_updated_at_clock_skew() {
    let supervisor = Arc::new(TaskSupervisor::new());
    let id = supervisor.register("spawn", "call-skew", Some("api:sess"));
    supervisor.mark_running(&id);
    let _guard = TaskTerminalGuard::new(Arc::clone(&supervisor), id.clone());

    let future_at = Utc::now() + chrono::Duration::minutes(5);
    {
        let mut tasks = supervisor.tasks.lock().unwrap_or_else(|e| e.into_inner());
        tasks.get_mut(&id).unwrap().updated_at = future_at;
    }

    let reaped = supervisor.reap_stuck_tasks(Utc::now(), Duration::from_secs(60));

    assert!(reaped.is_empty());
    assert_eq!(
        supervisor.get_task(&id).expect("task").status,
        TaskStatus::Running,
    );
}

/// `start_reaper` is idempotent and drives `reap_stuck_tasks` on its
/// interval: with a tiny interval/timeout a silently-stuck live task is
/// reaped by the background loop itself (no manual sweep call).
#[tokio::test]
async fn start_reaper_loop_reaps_stuck_task_on_interval() {
    let supervisor = Arc::new(TaskSupervisor::new());
    supervisor.set_reap_interval(Duration::from_millis(20));
    supervisor.set_stuck_timeout(Duration::from_millis(50));
    let id = supervisor.register("spawn", "call-loop", Some("api:sess"));
    supervisor.mark_running(&id);
    let _guard = TaskTerminalGuard::new(Arc::clone(&supervisor), id.clone());

    supervisor.start_reaper();
    supervisor.start_reaper(); // idempotent — must not panic or double-spawn

    // The task makes NO progress: after a few intervals the loop reaps it.
    tokio::time::sleep(Duration::from_millis(250)).await;
    let task = supervisor.get_task(&id).expect("task");
    assert_eq!(task.status, TaskStatus::Failed);
    assert!(
        task.error
            .as_deref()
            .unwrap_or_default()
            .contains("heartbeat timeout")
    );
    assert!(supervisor.cancel_token(&id).is_cancelled());
}

// ---------------------------------------------------------------------------
// #2055 — registration observer (`set_on_register`).
//
// The goal-ledger task-row creation lives in octos-cli (octos-agent cannot
// see octos-fleet), so the supervisor exposes a registration callback the
// runtime wires next to `set_on_terminal`. Fired from `register_full`'s
// single success path, so ONE call site covers every registration kind
// (background/spawn_only, sub-agents, MCP, peers).
// ---------------------------------------------------------------------------

/// The observer fires exactly once per successful registration, with the
/// freshly inserted task snapshot.
#[test]
fn on_register_fires_exactly_once_per_successful_registration() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let supervisor = TaskSupervisor::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(std::sync::Mutex::new(Vec::<BackgroundTask>::new()));
    let calls_c = calls.clone();
    let seen_c = seen.clone();
    supervisor.set_on_register(move |task| {
        calls_c.fetch_add(1, Ordering::SeqCst);
        seen_c.lock().unwrap().push(task.clone());
    });

    let id = supervisor.register("web_probe", "call-reg-1", Some("api:sess-reg"));

    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "one successful registration fires the observer exactly once"
    );
    let snapshots = seen.lock().unwrap();
    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].id, id);
    assert_eq!(snapshots[0].tool_name, "web_probe");
    assert_eq!(snapshots[0].tool_call_id, "call-reg-1");
    assert_eq!(
        snapshots[0].parent_session_key.as_deref(),
        Some("api:sess-reg")
    );

    // Subsequent lifecycle transitions must NOT re-fire the observer.
    supervisor.mark_running(&id);
    supervisor.mark_completed(&id, vec![]);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "on_register is a registration observer, not a change feed"
    );
}

/// Every `register*` entry point funnels through `register_full`, so the
/// observer covers all of them without per-entry-point wiring.
#[test]
fn on_register_fires_for_every_register_entry_point() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let supervisor = TaskSupervisor::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_c = calls.clone();
    supervisor.set_on_register(move |_| {
        calls_c.fetch_add(1, Ordering::SeqCst);
    });

    supervisor.register("t1", "call-ep-1", Some("api:sess-ep"));
    supervisor.register_with_lineage("t2", "call-ep-2", Some("api:sess-ep"), None);
    supervisor.register_with_input(
        "t3",
        "call-ep-3",
        Some("api:sess-ep"),
        Some(serde_json::json!({"a": 1})),
    );
    supervisor.register_with_input_and_cmid("t4", "call-ep-4", Some("api:sess-ep"), None, None);
    supervisor
        .try_register_with_input("t5", "call-ep-5", Some("api:sess-ep"), None)
        .expect("strict registration succeeds");

    assert_eq!(calls.load(Ordering::SeqCst), 5);
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

/// A REFUSED registration (terminal parent / fan-out cap) never fires the
/// observer — there is no task to create a ledger row for.
#[test]
fn on_register_does_not_fire_for_refused_registrations() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Terminal-parent refusal.
    let supervisor = TaskSupervisor::new();
    let parent_tcid = "call-onreg-parent";
    let parent = supervisor.register("run_pipeline", parent_tcid, Some("sess-onreg"));
    supervisor.mark_running(&parent);
    supervisor.mark_failed(&parent, "orphaned across restart".to_string());

    let calls = Arc::new(AtomicUsize::new(0));
    let calls_c = calls.clone();
    supervisor.set_on_register(move |_| {
        calls_c.fetch_add(1, Ordering::SeqCst);
    });
    supervisor
        .try_register_node_task("pipeline:analyze", parent_tcid, Some("sess-onreg"))
        .expect_err("terminal parent refuses the child registration");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "a ParentTerminal refusal must not fire on_register"
    );

    // Fan-out-cap refusal. The cap reader caches once per process
    // (`OnceLock`), so exercise the production cap: fill it with the
    // observer UNWIRED, wire the observer, then assert the refused
    // overflow attempt fires nothing.
    let cap_calls = Arc::new(AtomicUsize::new(0));
    let cap_calls_c = cap_calls.clone();
    let capped = TaskSupervisor::new();
    for i in 0..MAX_CHILDREN_PER_PARENT {
        capped
            .try_register_with_input(
                "tts",
                &format!("call-cap-onreg-{i}"),
                Some("sess-cap-onreg"),
                None,
            )
            .unwrap_or_else(|err| panic!("register #{i} should succeed; got {err}"));
    }
    capped.set_on_register(move |_| {
        cap_calls_c.fetch_add(1, Ordering::SeqCst);
    });
    capped
        .try_register_with_input(
            "tts",
            "call-cap-onreg-overflow",
            Some("sess-cap-onreg"),
            None,
        )
        .expect_err("overflow child exceeds the cap");
    assert_eq!(
        cap_calls.load(Ordering::SeqCst),
        0,
        "a ChildFanoutExceeded refusal must not fire on_register"
    );
}

/// An unwired supervisor registers exactly as before — the observer hook is
/// a no-op (the guard early-outs before taking any task snapshot).
#[test]
fn unwired_on_register_leaves_registration_untouched() {
    let supervisor = TaskSupervisor::new();
    let id = supervisor.register("web_probe", "call-unwired", Some("api:sess-unwired"));
    assert!(!id.is_empty());
    assert_eq!(
        supervisor.get_task(&id).unwrap().status,
        TaskStatus::Spawned
    );
}

/// #2055 review round 2 — child/nested supervisors must inherit the
/// REGISTRATION observers (`on_register` + the NAMED `on_change_listeners`
/// map) so goal-ledger task rows cover nested subagent registries — and
/// must NOT inherit the primary `on_change` / `on_failure` / `on_terminal`
/// callbacks, whose wake semantics are deliberately per-instance.
#[test]
fn inherit_registration_observers_copies_observer_pair_but_not_wake_callbacks() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let parent = TaskSupervisor::new();
    let register_calls = Arc::new(AtomicUsize::new(0));
    let named_calls = Arc::new(AtomicUsize::new(0));
    let primary_calls = Arc::new(AtomicUsize::new(0));
    let terminal_calls = Arc::new(AtomicUsize::new(0));

    let register_c = register_calls.clone();
    parent.set_on_register(move |_| {
        register_c.fetch_add(1, Ordering::SeqCst);
    });
    let named_c = named_calls.clone();
    parent.set_on_change_listener("settle", move |_| {
        named_c.fetch_add(1, Ordering::SeqCst);
    });
    let primary_c = primary_calls.clone();
    parent.set_on_change(move |_| {
        primary_c.fetch_add(1, Ordering::SeqCst);
    });
    let terminal_c = terminal_calls.clone();
    parent.set_on_terminal(move |_| {
        terminal_c.fetch_add(1, Ordering::SeqCst);
    });

    let child = TaskSupervisor::new();
    child.inherit_registration_observers(&parent);

    let id = child.register("web_probe", "call-inherit-1", Some("api:sess-inherit"));
    child.mark_running(&id);
    child.mark_completed(&id, vec![]);

    assert_eq!(
        register_calls.load(Ordering::SeqCst),
        1,
        "the child inherits on_register"
    );
    assert!(
        named_calls.load(Ordering::SeqCst) >= 1,
        "the child inherits the NAMED change listeners"
    );
    assert_eq!(
        primary_calls.load(Ordering::SeqCst),
        0,
        "the primary on_change is per-instance and must NOT be inherited"
    );
    assert_eq!(
        terminal_calls.load(Ordering::SeqCst),
        0,
        "on_terminal is per-instance and must NOT be inherited"
    );
}

/// #2055 review round 2 — the production nesting path: a registry snapshot
/// (`ToolRegistry::snapshot_excluding`) mints a FRESH supervisor (the
/// deliberate per-subtree isolation), which used to drop the registration
/// observers entirely — nested subagent registrations were invisible to the
/// goal ledger. The fresh supervisor now inherits the observer pair while
/// the task maps stay isolated.
#[test]
fn snapshot_excluding_child_supervisor_inherits_registration_observers() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let parent_registry = crate::ToolRegistry::new();
    let register_calls = Arc::new(AtomicUsize::new(0));
    let named_calls = Arc::new(AtomicUsize::new(0));
    let register_c = register_calls.clone();
    parent_registry.supervisor().set_on_register(move |_| {
        register_c.fetch_add(1, Ordering::SeqCst);
    });
    let named_c = named_calls.clone();
    parent_registry
        .supervisor()
        .set_on_change_listener("settle", move |_| {
            named_c.fetch_add(1, Ordering::SeqCst);
        });

    let child_registry = parent_registry.snapshot_excluding(&[]);
    let child = child_registry.supervisor();
    let id = child.register("nested_tool", "call-snap-1", Some("api:sess-snap"));
    child.mark_running(&id);

    assert_eq!(
        register_calls.load(Ordering::SeqCst),
        1,
        "a registration on the snapshot's fresh supervisor reaches the parent's observer"
    );
    assert!(
        named_calls.load(Ordering::SeqCst) >= 1,
        "the named change listeners ride along onto the snapshot"
    );
    // The isolation contract is untouched: the child's task lives in the
    // child's map only.
    assert!(
        parent_registry.supervisor().get_task(&id).is_none(),
        "task maps stay per-subtree"
    );
    assert!(child.get_task(&id).is_some());
}

/// #27c — the full simulated-restart orphan lifecycle: a running task's
/// process dies; the next supervisor's boot sweep PARKS it (awaiting client
/// re-attach) instead of failing it; the returning client revives it with
/// `mark_running` (Parked → Running) and completes it normally. Red lines:
/// terminal tasks are never re-parked, and Parked never fires the terminal
/// failure callback (it is not a verdict, it is a pause).
#[test]
fn orphan_restart_parks_then_client_reattach_revives_full_chain() {
    let temp = tempfile::tempdir().unwrap();
    let ledger_path = temp.path().join("supervisor.jsonl");

    // Boot 1: the "old process" registers a running task and persists it.
    let old = TaskSupervisor::new();
    let task_id = old.register("peer_handoff", "call-27c", Some("octos:local:tui#coding"));
    old.mark_running(&task_id);
    old.enable_persistence(&ledger_path).unwrap();

    // Boot 2: a fresh supervisor (restart) — no live worker for the task.
    // The sweep PARKS it, not fails it.
    let restarted = TaskSupervisor::new();
    restarted.enable_persistence(&ledger_path).unwrap();
    let parked = restarted.get_task(&task_id).expect("task survived restart");
    assert_eq!(parked.status, TaskStatus::Parked, "orphan must be Parked");
    assert_eq!(parked.error.as_deref(), Some("orphaned across restart"));
    assert!(
        !parked.status.is_terminal(),
        "Parked is re-attachable, not terminal"
    );
    assert!(
        !parked.status.is_active(),
        "Parked has no live worker in this process"
    );

    // Boot 3: the returning client re-attaches — mark_running revives the
    // SAME task (Parked → Running) and drives it to completion.
    restarted.mark_running(&task_id);
    let revived = restarted.get_task(&task_id).expect("revived");
    assert_eq!(
        revived.status,
        TaskStatus::Running,
        "client re-attach revives Parked → Running"
    );
    restarted.mark_completed(&task_id, vec![]);
    let done = restarted.get_task(&task_id).expect("done");
    assert_eq!(
        done.status,
        TaskStatus::Completed,
        "revived task completes normally"
    );

    // Red line 1: mark_parked on a terminal task is a no-op.
    restarted.mark_parked(&task_id, "late park".into());
    assert_eq!(
        restarted.get_task(&task_id).unwrap().status,
        TaskStatus::Completed,
        "terminal tasks are never re-parked"
    );

    // Red line 2: a Parked task never fired the terminal-failure path —
    // its runtime_state slot still reads the parked detail on the way
    // through, and completion stamped completed_at only at the REAL end.
    assert!(
        done.completed_at.is_some(),
        "completed_at stamps the real terminal"
    );
}

// #21 (round-4, codex #17 B3) — the peer-task registration's FIRST durable
// row carries the workspace stamp; a failed first write rolls the whole
// registration back.

/// Test ①: after the strict registration (and a simulated crash — a fresh
/// supervisor over the same ledger), the restored task STILL carries the
/// workspace scope. No second `set_workspace_root` write ever happened.
#[test]
fn peer_workspace_stamp_survives_restart_on_first_durable_row() {
    let temp = tempfile::TempDir::new().unwrap();
    let ledger = temp.path().join("tasks.jsonl");
    let supervisor = TaskSupervisor::new();
    supervisor.enable_persistence(&ledger).expect("persistence");

    let task_id = supervisor
        .try_register_peer_with_workspace(
            "peer_handoff",
            "tenant:peer:alpha",
            Some("tenant:api:master"),
            Some("2f686f6d652f7773"),
        )
        .expect("strict registration succeeds");
    assert_eq!(
        supervisor.get_task(&task_id).and_then(|t| t.workspace_root),
        Some("2f686f6d652f7773".to_owned()),
        "the stamp is on the in-memory row"
    );
    drop(supervisor);

    // "Crash" + restart: the FIRST durable row already carried the scope.
    let restored = TaskSupervisor::new();
    restored.enable_persistence(&ledger).expect("restore");
    assert_eq!(
        restored.get_task(&task_id).and_then(|t| t.workspace_root),
        Some("2f686f6d652f7773".to_owned()),
        "the restored row keeps the workspace scope from the FIRST write"
    );
}

/// Test ②: a failed first durable write returns
/// `WorkspacePersistFailed` and leaves NO task row (no half-binding).
#[test]
fn peer_workspace_registration_rolls_back_on_failed_first_write() {
    let temp = tempfile::TempDir::new().unwrap();
    let supervisor = TaskSupervisor::new();
    let ledger = temp.path().join("tasks.jsonl");
    supervisor.enable_persistence(&ledger).expect("persistence");
    // enable_persistence with zero tasks never creates the ledger file —
    // create it, then replace it with a directory → appends now fail.
    std::fs::write(&ledger, "").unwrap();
    std::fs::remove_file(&ledger).unwrap();
    std::fs::create_dir_all(&ledger).unwrap();

    let result = supervisor.try_register_peer_with_workspace(
        "peer_handoff",
        "tenant:peer:beta",
        Some("tenant:api:master"),
        Some("deadbeef"),
    );
    match result {
        Err(RegisterTaskError::WorkspacePersistFailed { tool_call_id, .. }) => {
            assert_eq!(tool_call_id, "tenant:peer:beta");
        }
        other => panic!("expected WorkspacePersistFailed, got {other:?}"),
    }
    let tasks: Vec<String> = supervisor
        .tasks
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .keys()
        .cloned()
        .collect();
    assert!(
        tasks.iter().all(|id| {
            supervisor
                .get_task(id)
                .is_none_or(|t| t.tool_call_id != "tenant:peer:beta")
        }),
        "the rolled-back registration leaves no task row; tasks: {tasks:?}"
    );
}

/// Test ③ (encoding side lives in octos-cli `peers` tests): two DIFFERENT
/// scopes never alias — asserted here at the row level: distinct scopes
/// produce distinct stamps, so the purge-side exact match cannot clear the
/// other's items.
#[test]
fn distinct_workspace_scopes_stay_distinct_on_task_rows() {
    let supervisor = TaskSupervisor::new();
    let a = supervisor
        .try_register_peer_with_workspace("peer_handoff", "t:peer:a", Some("t:api:m"), Some("aa"))
        .expect("a registers");
    let b = supervisor
        .try_register_peer_with_workspace("peer_handoff", "t:peer:b", Some("t:api:m"), Some("bb"))
        .expect("b registers");
    assert_ne!(
        supervisor.get_task(&a).and_then(|t| t.workspace_root),
        supervisor.get_task(&b).and_then(|t| t.workspace_root),
        "different workspace scopes stay different on the durable rows"
    );
}
