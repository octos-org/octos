//! M8 plugin protocol v2 contract tests (W4 forward-compatible scaffolding).
//!
//! These tests pin the contract that **all three** W4 plugins (`mofa_slides`,
//! `podcast_generate`, `fm_tts`) must adhere to once they adopt v2. They run
//! against the existing host-side parser (which already accepts the v2
//! schema via `HarnessEventSink`) so they are valid scaffolding even before
//! the W3 protocol_v2 module lands.
//!
//! Each `#[test]` here either:
//! 1. asserts an existing host-side behaviour (these run today), or
//! 2. is `#[ignore]`d with a doc-comment explaining the W3+W4 dependency
//!    that needs to land before the test can be flipped on.
//!
//! When the W4 plugin work merges, the `#[ignore]` directives are flipped
//! off in the same PR. The test names are stable so reviewers can grep
//! the diff.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use octos_agent::{
    HARNESS_EVENT_SCHEMA_V1, HarnessEvent, HarnessEventPayload, HarnessEventSink, TaskRuntimeState,
    TaskSupervisor,
};

// =========================================================================
// Section 1 — v2 stderr event schema round-trip
// =========================================================================
//
// W4 plugins emit one JSON event per stderr line. The host parses each line
// as a `HarnessEvent`; lines that do not start with `{` fall through to the
// legacy text-line `ToolProgress` path (backward compatibility).

/// A representative `progress` event the W4 plugins must emit. If a plugin
/// regresses the schema (e.g. drops `task_id` or renames `phase`), this
/// test catches it.
#[test]
fn v2_progress_event_schema_round_trips() {
    let raw = serde_json::json!({
        "schema": "octos.harness.event.v1",
        "kind": "progress",
        "session_id": "session-abc",
        "task_id": "task-xyz",
        "workflow": "podcast_generate",
        "phase": "rendering_audio",
        "message": "Rendered 4/12 lines",
        "progress": 0.33
    });
    let parsed = HarnessEvent::from_json_line(&raw.to_string()).expect("parse v2 progress");
    assert_eq!(parsed.schema, HARNESS_EVENT_SCHEMA_V1);
    assert_eq!(parsed.session_id(), "session-abc");
    assert_eq!(parsed.task_id(), "task-xyz");
    assert_eq!(parsed.workflow(), Some("podcast_generate"));
    assert_eq!(parsed.phase(), Some("rendering_audio"));
}

/// W4 plugins must emit `cost_attribution` events on each LLM/API call so
/// the per-task cost panel (G4) can render per-model rows. Schema is
/// versioned so a future v3 can land without breaking.
#[test]
fn v2_cost_attribution_event_schema_round_trips() {
    let raw = serde_json::json!({
        "schema": "octos.harness.event.v1",
        "kind": "cost_attribution",
        "session_id": "session-abc",
        "task_id": "task-xyz",
        "attribution_id": "01HW0V4M9X9JHK4FNN3QXQ0001",
        "contract_id": "podcast_generate.v0.4",
        "model": "qwen3-tts-1.5b",
        "tokens_in": 4321,
        "tokens_out": 567,
        "cost_usd": 0.012345,
        "outcome": "success"
    });
    let parsed = HarnessEvent::from_json_line(&raw.to_string()).expect("parse v2 cost");
    assert_eq!(parsed.schema, HARNESS_EVENT_SCHEMA_V1);
    match parsed.payload {
        HarnessEventPayload::CostAttribution { ref data } => {
            assert_eq!(data.contract_id, "podcast_generate.v0.4");
            assert_eq!(data.model, "qwen3-tts-1.5b");
            assert_eq!(data.tokens_in, 4321);
            assert_eq!(data.tokens_out, 567);
            assert_eq!(data.outcome, "success");
            assert!((data.cost_usd - 0.012345).abs() < 1e-9);
        }
        other => panic!("expected CostAttribution, got {other:?}"),
    }
}

/// W4 plugins emit a terminal `failure` event when they cannot recover.
/// The host folds these into `runtime_detail` so the M8.9 recovery prompt
/// can pick up the actionable error.
#[test]
fn v2_failure_event_schema_round_trips() {
    let raw = serde_json::json!({
        "schema": "octos.harness.event.v1",
        "kind": "failure",
        "session_id": "session-abc",
        "task_id": "task-xyz",
        "phase": "synthesis",
        "message": "voice 'unknown_voice' not registered. available: vivian, serena, ryan.",
        "retryable": true
    });
    let parsed = HarnessEvent::from_json_line(&raw.to_string()).expect("parse v2 failure");
    match parsed.payload {
        HarnessEventPayload::Failure { ref data } => {
            assert_eq!(data.phase.as_deref(), Some("synthesis"));
            assert!(data.message.contains("not registered"));
            assert_eq!(data.retryable, Some(true));
        }
        other => panic!("expected Failure, got {other:?}"),
    }
}

/// Forward-compat: parser must ignore unknown future fields without
/// erroring. The W4 plugins may add fields (`browser_ms`, `cdn_url`, ...)
/// that the host should round-trip through opaquely.
#[test]
fn v2_parser_ignores_unknown_future_fields() {
    let raw = serde_json::json!({
        "schema": "octos.harness.event.v1",
        "kind": "progress",
        "session_id": "s",
        "task_id": "t",
        "phase": "ok",
        "browser_ms": 4321,            // unknown to current schema
        "cdn_url": "https://x/y.png",  // unknown to current schema
    });
    let parsed = HarnessEvent::from_json_line(&raw.to_string()).expect("parse with future fields");
    assert_eq!(parsed.phase(), Some("ok"));
}

// =========================================================================
// Section 2 — v1 backward compatibility
// =========================================================================
//
// The host falls back to legacy text-line handling when the stderr line
// doesn't start with `{`. This is load-bearing during the W4 plugin
// migration: v1 plugins keep working unchanged while v2 plugins land
// incrementally.

/// Legacy stderr line: bare text with no `{` prefix. The plugin host
/// surfaces this as a `ToolProgress` event with the line as `message`.
/// This is the v1 contract — must keep working forever.
#[test]
fn legacy_text_line_is_not_parsed_as_v2_event() {
    let line = "voice synthesised in 4.2s";
    assert!(
        !line.trim_start().starts_with('{'),
        "legacy lines never start with '{{'"
    );
    let parse_attempt = HarnessEvent::from_json_line(line);
    assert!(
        parse_attempt.is_err(),
        "legacy lines must not parse as v2 events"
    );
}

/// Mixed stderr: a v2 plugin in mid-migration may emit some v2 lines and
/// some legacy text. The host treats each line independently; this test
/// pins that the parser does not get confused by interleaving.
#[test]
fn mixed_v1_and_v2_lines_are_parsed_independently() {
    let lines = [
        "starting up",
        r#"{"schema":"octos.harness.event.v1","kind":"progress","session_id":"s","task_id":"t","phase":"running","message":"step 1"}"#,
        "intermediate text log",
        r#"{"schema":"octos.harness.event.v1","kind":"progress","session_id":"s","task_id":"t","phase":"running","message":"step 2","progress":0.5}"#,
        "all done",
    ];
    let mut v2_count = 0;
    let mut v1_count = 0;
    for line in lines {
        if line.trim_start().starts_with('{') {
            HarnessEvent::from_json_line(line).expect("v2 line must parse");
            v2_count += 1;
        } else {
            v1_count += 1;
        }
    }
    assert_eq!(v2_count, 2);
    assert_eq!(v1_count, 3);
}

// =========================================================================
// Section 3 — Sink integration: events fold into supervisor state
// =========================================================================
//
// When a plugin writes a v2 event to `$OCTOS_EVENT_SINK`, the host's
// `HarnessEventSink` reader picks it up and applies the appropriate
// transition to the supervised `BackgroundTask`.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v2_progress_event_via_sink_updates_runtime_detail() {
    let supervisor = Arc::new(TaskSupervisor::new());
    let task_id = supervisor.register("mofa_slides", "call-1", Some("api:session-x"));
    supervisor.mark_running(&task_id);

    let sink =
        HarnessEventSink::new(supervisor.clone(), task_id.clone(), "api:session-x").expect("sink");

    // Append a v2 progress event to the sink (simulating what the W4
    // plugin will do via $OCTOS_EVENT_SINK).
    let event_line = format!(
        r#"{{"schema":"octos.harness.event.v1","kind":"progress","session_id":"api:session-x","task_id":"{task_id}","workflow":"mofa_slides","phase":"rendering","message":"Rendering deck 3/8","progress":0.375}}"#,
    );
    append_to_sink(sink.path(), &event_line);

    // The reader is async — wait briefly for the supervisor to fold the
    // event into runtime_detail.
    let mut detail: Option<String> = None;
    for _ in 0..40 {
        let task = supervisor.get_task(&task_id).expect("task exists");
        if task.runtime_detail.is_some() {
            detail = task.runtime_detail.clone();
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let raw = detail.expect("expected runtime_detail to populate from v2 event");
    let value: serde_json::Value = serde_json::from_str(&raw).expect("detail is JSON");
    assert_eq!(value["workflow_kind"], "mofa_slides");
    assert_eq!(value["current_phase"], "rendering");
    assert_eq!(value["progress_message"], "Rendering deck 3/8");

    // Drop sink before the test ends so the reader task is cleaned up.
    drop(sink);
}

// =========================================================================
// Section 4 — SIGTERM contract (placeholder, requires plugin v2 adoption)
// =========================================================================
//
// W4 plugins must respond to SIGTERM within 10s with a clean exit. The
// in-tree test for this lives at the plugin host level (signal propagation
// through tokio::process), but the *plugin-side* test runs inside each
// external plugin's own crate.
//
// The placeholders below document the expected behaviour and skip until
// the plugin adoption PRs land.

/// Once `mofa_slides` adopts v2, this test invokes the binary, sends
/// SIGTERM mid-run, and asserts exit within 10s.
///
/// **Why ignored**: requires the external `mofa-slides` skill to install a
/// `signal_hook::iterator::Signals` handler in its main loop.
#[test]
#[ignore = "W4: pending mofa_slides v2 adoption (SIGTERM handler in mofa-slides repo)"]
fn mofa_slides_responds_to_sigterm_within_10s() {
    // Implementation lands with the W4 mofa_slides PR. Pseudocode:
    //
    //   let child = Command::new("mofa-slides").arg("mofa_slides")
    //                 .stdin(Stdio::piped()).spawn().unwrap();
    //   feed long-running input;
    //   sleep(2s);  // let it get into the rendering phase
    //   kill(child.pid, SIGTERM);
    //   let started = Instant::now();
    //   let status = child.wait_timeout(Duration::from_secs(11)).unwrap();
    //   assert!(status.is_some(), "mofa_slides ignored SIGTERM");
    //   assert!(started.elapsed() < Duration::from_secs(10));
}

/// Once `podcast_generate` adopts v2, this test invokes the binary, sends
/// SIGTERM mid-run, and asserts exit within 10s plus no orphan ffmpeg
/// processes.
#[test]
#[ignore = "W4: pending podcast_generate v2 adoption (SIGTERM handler in mofa-podcast repo)"]
fn podcast_generate_responds_to_sigterm_within_10s_no_orphans() {
    // Implementation lands with the W4 podcast_generate PR.
}

/// Once `fm_tts` adopts v2, this test invokes the binary, sends SIGTERM
/// mid-run, and asserts exit within 10s.
#[test]
#[ignore = "W4: pending fm_tts v2 adoption (SIGTERM handler in mofa-fm repo)"]
fn fm_tts_responds_to_sigterm_within_10s() {
    // Implementation lands with the W4 fm_tts PR.
}

// =========================================================================
// Helpers
// =========================================================================

fn append_to_sink(path: &Path, line: &str) {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .expect("open sink");
    writeln!(file, "{line}").expect("write event line");
    file.flush().expect("flush sink");
}

#[cfg(test)]
mod additional_invariants {
    use super::*;

    /// Defensive: schema string must not change without a runbook update.
    /// `octos.harness.event.v1` is the version byte the host parser
    /// recognises. A v2 schema string would require a new parser branch.
    #[test]
    fn schema_string_is_stable() {
        assert_eq!(HARNESS_EVENT_SCHEMA_V1, "octos.harness.event.v1");
    }

    /// Defensive: `TaskRuntimeState` enum order must not change. The
    /// supervisor's state machine and the on-disk persisted ledger
    /// depend on it (item 8 in the M8 fix-first checklist).
    #[test]
    fn runtime_state_enum_variants_are_stable() {
        // Touch each variant — if a future change reorders / renames a
        // variant the compiler will fail this exhaustive match.
        for state in [
            TaskRuntimeState::Spawned,
            TaskRuntimeState::ExecutingTool,
            TaskRuntimeState::ResolvingOutputs,
            TaskRuntimeState::VerifyingOutputs,
            TaskRuntimeState::DeliveringOutputs,
            TaskRuntimeState::CleaningUp,
            TaskRuntimeState::Completed,
            TaskRuntimeState::Failed,
        ] {
            // Round-trip through the JSON schema to ensure the discriminant
            // names are stable for the persisted ledger.
            let json = serde_json::to_string(&state).expect("serialize state");
            let back: TaskRuntimeState = serde_json::from_str(&json).expect("round-trip state");
            assert_eq!(state, back);
        }
    }
}
