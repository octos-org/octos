//! M9-α-4 — bridge SSE-driven status / progress-gate progress events onto
//! the M9 WebSocket UI Protocol path.
//!
//! Per the M9-α (Sole Transport) ADR (`docs/M9-ALPHA-SOLE-TRANSPORT-ADR.md`)
//! the WebSocket transport is migrating to be the sole chat transport.
//! This module is the α-4 phase: while SSE is still alive (deletion
//! lands in α-5/α-6 atomically with the web bundle), the SSE chat path's
//! status frames and progress-gate (terminal-budget) frames must ALSO
//! be appended to the M9 ledger so any concurrently-connected
//! WebSocket subscriber for the same `SessionKey` sees them through the
//! live broadcast (`UiProtocolLedger::subscribe`).
//!
//! **Survey of status / heartbeat / progress-gate SSE events** (issue
//! #833, audit-lock #845). Source-of-truth for the wire payloads is
//! `crates/octos-cli/src/api/sse.rs::event_to_json` plus the
//! `ProgressEvent` enum in `crates/octos-agent/src/progress.rs`. Events
//! that already have a WS counterpart through α-2 (`tool_progress`),
//! α-3 (turn lifecycle), or `MessageCommitObserver` (`session_result`)
//! are out of scope.
//!
//! | ProgressEvent variant       | SSE wire `type`            | α-4 mirror?  | strategy                                                                |
//! |-----------------------------|----------------------------|--------------|-------------------------------------------------------------------------|
//! | `LlmStatus`                 | `llm_status`               | yes          | `progress/updated` w/ `kind=status`, message + iteration                |
//! | `StreamRetry`               | `stream_retry`             | yes          | `progress/updated` w/ `kind=retry_backoff` (UiRetryBackoff)             |
//! | `MaxIterationsReached`      | `max_iterations_reached`   | yes (gate)   | `progress/updated` w/ `kind=status`, message + extra.limit              |
//! | `TokenBudgetExceeded`       | `token_budget_exceeded`    | yes (gate)   | `progress/updated` w/ `kind=status`, message + extra.used + extra.limit |
//! | `ActivityTimeoutReached`    | `activity_timeout_reached` | yes (gate)   | `progress/updated` w/ `kind=status`, message + extra.elapsed/limit_ms   |
//! | `Thinking` / `Response`     | `thinking` / `response`    | **no**       | High-volume per-iteration noise; web client does not surface them; SSE-only path is fine. |
//! | `StreamChunk` / `StreamDone`| `token` / `stream_end`     | **no**       | Streaming text deltas — γ-3 `message/delta` covers the WS surface; mirroring here would duplicate. |
//! | `CostUpdate`/`TokenUsage`   | `cost_update`              | **no**       | Cost telemetry — covered by `progress/updated` w/ `kind=token_cost_update` from γ-1 envelope work. |
//! | `TaskStarted`/`TaskCompleted`/`TaskInterrupted` | `task_started` / `task_completed` / `task_interrupted` | **no** | Task lifecycle — α-3 covers turn lifecycle; per-task surface lives on `task/updated` and is already published via `TaskQueryStore::watch`. |
//! | `FileModified`              | `file_modified`            | **no**       | File-mutation notice — γ work lifts this to `progress/updated` w/ `kind=file_mutation` separately; the existing SSE path already informs the dashboard. |
//!
//! **Heartbeat is explicitly not on the SSE chat wire**, despite the
//! issue title. Two unrelated heartbeat surfaces exist in the codebase:
//!
//! - `octos_bus::HeartbeatService` is a cross-channel cron poke that
//!   feeds the gateway inbound bus (see
//!   `crates/octos-cli/src/commands/gateway/gateway_runtime.rs`). It
//!   never traverses the SSE chat path and has no per-session WS
//!   subscriber to mirror to.
//! - `octos_agent::Heartbeat` is the realtime-controller liveness
//!   counter (see `crates/octos-agent/src/agent/realtime.rs`) exposed
//!   via the `octos_realtime_heartbeat_*` Prometheus counters in
//!   `metrics.rs`. It is a metrics surface, not a WS-routable event.
//!
//! Neither matches the α-4 contract ("status/heartbeat/progress-gate
//! events emitted on `POST /api/chat?stream=true`"), so this bridge
//! deliberately does NOT mirror them. If a future phase wants those on
//! the WS protocol, that's a separate spec change.
//!
//! **Coexistence invariants** (same as α-2 / α-3):
//! - SSE delivery is unchanged. The base reporter is invoked first.
//! - Ledger appends are best-effort. A failure does not affect the SSE
//!   path or the agent loop.
//! - The web reducer routes `progress/updated` by metadata.kind; clients
//!   on both transports collapse the duplicate into one logical update.
//!
//! When α-5/α-6 land and SSE is deleted, this bridge becomes the
//! straight-through reporter for these events.

use std::sync::Arc;

use octos_agent::{ProgressEvent, ProgressReporter};
use octos_core::SessionKey;
use octos_core::ui_protocol::{
    ProgressUpdatedEvent, TurnId, UiNotification, UiProgressMetadata, UiRetryBackoff,
    progress_kinds,
};
use serde_json::json;

use super::ui_protocol_ledger::UiProtocolLedger;

/// Decorator that delegates every event to its `inner` reporter (the SSE
/// channel reporter chain that already has α-2's `tool_progress` mirror
/// installed) AND mirrors status / progress-gate variants of
/// [`ProgressEvent`] onto the M9 ledger as `progress/updated.v1`
/// notifications so connected WebSocket subscribers observe the same
/// status surface SSE consumers see.
///
/// Construction is cheap — `Arc<UiProtocolLedger>` is already
/// process-singleton (see `ui_protocol::event_ledger`), so wrapping
/// costs one pointer copy per turn.
pub(super) struct LedgerStatusGateReporter {
    inner: Arc<dyn ProgressReporter>,
    ledger: Arc<UiProtocolLedger>,
    session_id: SessionKey,
    turn_id: TurnId,
}

impl LedgerStatusGateReporter {
    /// Wrap `inner` so each emitted event is also mirrored onto `ledger`
    /// when applicable. `session_id` is the SSE turn's `SessionKey`;
    /// `turn_id` is the per-request synthetic `TurnId` that lets WS
    /// subscribers correlate this turn's status updates with their pane
    /// state. Reuses the same `TurnId` α-2 / α-3 thread through the
    /// reporter chain so a single WS reducer state machine can fold
    /// every per-turn surface together.
    pub(super) fn new(
        inner: Arc<dyn ProgressReporter>,
        ledger: Arc<UiProtocolLedger>,
        session_id: SessionKey,
        turn_id: TurnId,
    ) -> Self {
        Self {
            inner,
            ledger,
            session_id,
            turn_id,
        }
    }

    /// Build the `progress/updated` payload for the events α-4 cares
    /// about. Returns `None` for events outside the α-4 scope so the
    /// caller knows there is nothing to mirror — the inner reporter
    /// still observes them via the standard delegate path.
    fn map_event(&self, event: &ProgressEvent) -> Option<UiNotification> {
        let metadata = match event {
            ProgressEvent::LlmStatus { message, iteration } => {
                let mut m =
                    UiProgressMetadata::new(progress_kinds::STATUS).with_message(message.clone());
                m.iteration = Some(*iteration);
                m
            }
            ProgressEvent::StreamRetry { iteration } => {
                let mut retry = UiRetryBackoff::new();
                // The legacy SSE frame for stream_retry carries no
                // attempt/backoff/provider — the Rust enum variant only
                // has `iteration`. Surfacing the retry shape (vs a bare
                // `kind=status`) gives the WS client a typed signal it
                // can route to the same reducer that handles backoff
                // notifications from the γ-1 envelope work without
                // synthesising fields the SSE side never had.
                retry.reason = Some("stream retry".into());
                let mut m = UiProgressMetadata::retry_backoff(retry).with_message("stream retry");
                m.iteration = Some(*iteration);
                m
            }
            ProgressEvent::MaxIterationsReached { limit } => {
                let mut m = UiProgressMetadata::new(progress_kinds::STATUS)
                    .with_message("max iterations reached");
                m.extra.insert("limit".into(), json!(*limit));
                m
            }
            ProgressEvent::TokenBudgetExceeded { used, limit } => {
                let mut m = UiProgressMetadata::new(progress_kinds::STATUS)
                    .with_message("token budget exceeded");
                m.extra.insert("used".into(), json!(*used));
                m.extra.insert("limit".into(), json!(*limit));
                m
            }
            ProgressEvent::ActivityTimeoutReached { elapsed, limit } => {
                let mut m = UiProgressMetadata::new(progress_kinds::STATUS)
                    .with_message("activity timeout reached");
                m.extra
                    .insert("elapsed_ms".into(), json!(elapsed.as_millis() as u64));
                m.extra
                    .insert("limit_ms".into(), json!(limit.as_millis() as u64));
                m
            }
            // Out of α-4 scope — see the survey table in the module
            // docstring. Returning None lets the caller skip the ledger
            // mirror without affecting SSE delivery.
            _ => return None,
        };

        Some(UiNotification::ProgressUpdated(ProgressUpdatedEvent::new(
            self.session_id.clone(),
            Some(self.turn_id.clone()),
            metadata,
        )))
    }
}

impl ProgressReporter for LedgerStatusGateReporter {
    fn report(&self, event: ProgressEvent) {
        // Mirror to the ledger BEFORE delegating to the inner reporter.
        // Same ordering rationale as α-2: a backpressured inner reporter
        // shouldn't delay the WS subscriber's view of progress.
        if let Some(notification) = self.map_event(&event) {
            // `append_notification` performs an in-process broadcast and
            // (when `data_dir` is configured) a write-ahead disk record.
            // Both paths are infallible from the caller's POV — disk
            // errors are logged but do not panic.
            let _ = self.ledger.append_notification(notification);
        }
        self.inner.report(event);
    }

    fn thread_id(&self) -> Option<&str> {
        self.inner.thread_id()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use octos_core::ui_protocol::methods;
    use std::sync::Mutex;
    use std::time::Duration;

    /// Test double that captures every event the inner reporter receives.
    /// Stands in for the SSE chain (channel reporter + α-2 bridge) so we
    /// can assert SSE delivery is preserved during the α-4 coexistence
    /// period.
    #[derive(Default)]
    struct CapturingReporter {
        events: Mutex<Vec<ProgressEvent>>,
    }

    impl ProgressReporter for CapturingReporter {
        fn report(&self, event: ProgressEvent) {
            self.events.lock().unwrap().push(event);
        }
    }

    fn fixture(
        session_id: &str,
    ) -> (
        Arc<CapturingReporter>,
        Arc<UiProtocolLedger>,
        SessionKey,
        TurnId,
        LedgerStatusGateReporter,
    ) {
        let inner = Arc::new(CapturingReporter::default());
        let ledger = Arc::new(UiProtocolLedger::new(64));
        let session_key = SessionKey::new("api", session_id);
        let turn_id = TurnId::new();
        let reporter = LedgerStatusGateReporter::new(
            inner.clone() as Arc<dyn ProgressReporter>,
            ledger.clone(),
            session_key.clone(),
            turn_id.clone(),
        );
        (inner, ledger, session_key, turn_id, reporter)
    }

    /// α-4 acceptance gate (A): `LlmStatus` events emit a
    /// `progress/updated.v1` notification with `kind=status` carrying
    /// the message and iteration, and the inner reporter still sees the
    /// raw event for the SSE wire path.
    #[test]
    fn should_mirror_llm_status_to_ledger_with_status_kind() {
        let (inner, ledger, session_id, turn_id, reporter) = fixture("alpha4-llm-status");
        let mut subscriber = ledger.subscribe(&session_id);

        reporter.report(ProgressEvent::LlmStatus {
            message: "switching providers".into(),
            iteration: 4,
        });

        // SSE side: inner reporter received the raw ProgressEvent.
        let captured = inner.events.lock().unwrap();
        assert_eq!(captured.len(), 1);
        match &captured[0] {
            ProgressEvent::LlmStatus { message, iteration } => {
                assert_eq!(message, "switching providers");
                assert_eq!(*iteration, 4);
            }
            other => panic!("expected LlmStatus, got {other:?}"),
        }

        // WS side: a progress/updated notification with kind=status
        // landed on the ledger broadcast.
        let event = subscriber
            .try_recv()
            .expect("ledger must broadcast progress/updated");
        match &event.event {
            crate::api::ui_protocol_ledger::UiProtocolLedgerEvent::Notification(
                UiNotification::ProgressUpdated(payload),
            ) => {
                assert_eq!(payload.session_id, session_id);
                assert_eq!(payload.turn_id.as_ref(), Some(&turn_id));
                assert_eq!(payload.metadata.kind, progress_kinds::STATUS);
                assert_eq!(
                    payload.metadata.message.as_deref(),
                    Some("switching providers")
                );
                assert_eq!(payload.metadata.iteration, Some(4));
            }
            other => panic!("expected ProgressUpdated notification, got {other:?}"),
        }
        let rpc = event
            .event
            .clone()
            .into_rpc_notification()
            .expect("notification serializes");
        assert_eq!(rpc.method, methods::PROGRESS_UPDATED);
    }

    /// α-4 acceptance gate (B): `StreamRetry` maps to `kind=retry_backoff`
    /// (NOT bare status). The retry sub-payload carries the SSE-side
    /// reason placeholder so a WS client routing by retry shape sees a
    /// typed retry notification, while the inner reporter is unaffected.
    #[test]
    fn should_mirror_stream_retry_with_retry_backoff_kind() {
        let (inner, ledger, session_id, turn_id, reporter) = fixture("alpha4-stream-retry");
        let mut subscriber = ledger.subscribe(&session_id);

        reporter.report(ProgressEvent::StreamRetry { iteration: 3 });

        // Inner reporter still received it.
        assert_eq!(inner.events.lock().unwrap().len(), 1);

        let event = subscriber
            .try_recv()
            .expect("ledger must broadcast progress/updated for stream_retry");
        match &event.event {
            crate::api::ui_protocol_ledger::UiProtocolLedgerEvent::Notification(
                UiNotification::ProgressUpdated(payload),
            ) => {
                assert_eq!(payload.session_id, session_id);
                assert_eq!(payload.turn_id.as_ref(), Some(&turn_id));
                assert_eq!(payload.metadata.kind, progress_kinds::RETRY_BACKOFF);
                assert_eq!(payload.metadata.iteration, Some(3));
                let retry = payload
                    .metadata
                    .retry
                    .as_ref()
                    .expect("retry metadata present");
                assert_eq!(retry.reason.as_deref(), Some("stream retry"));
            }
            other => panic!("expected ProgressUpdated notification, got {other:?}"),
        }
    }

    /// α-4 acceptance gate (C): each progress-gate variant
    /// (`MaxIterationsReached`, `TokenBudgetExceeded`,
    /// `ActivityTimeoutReached`) emits a `progress/updated` envelope with
    /// `kind=status` and the gate-specific extras (limit / used / elapsed)
    /// preserved. This exercises the actual gate frames the soak's
    /// `live-progress-gate.spec.ts` consumes from the SSE side, ensuring
    /// the WS-only ingest path receives the same evidence.
    #[test]
    fn should_mirror_each_progress_gate_with_extras() {
        let (_inner, ledger, session_id, turn_id, reporter) = fixture("alpha4-gates");
        let mut subscriber = ledger.subscribe(&session_id);

        reporter.report(ProgressEvent::MaxIterationsReached { limit: 50 });
        reporter.report(ProgressEvent::TokenBudgetExceeded {
            used: 12_000,
            limit: 10_000,
        });
        reporter.report(ProgressEvent::ActivityTimeoutReached {
            elapsed: Duration::from_secs(45),
            limit: Duration::from_secs(30),
        });

        // 1. MaxIterationsReached
        let max_iter = subscriber.try_recv().expect("max_iterations envelope");
        match &max_iter.event {
            crate::api::ui_protocol_ledger::UiProtocolLedgerEvent::Notification(
                UiNotification::ProgressUpdated(payload),
            ) => {
                assert_eq!(payload.session_id, session_id);
                assert_eq!(payload.turn_id.as_ref(), Some(&turn_id));
                assert_eq!(payload.metadata.kind, progress_kinds::STATUS);
                assert_eq!(
                    payload.metadata.message.as_deref(),
                    Some("max iterations reached")
                );
                assert_eq!(payload.metadata.extra.get("limit"), Some(&json!(50)));
            }
            other => panic!("expected ProgressUpdated, got {other:?}"),
        }

        // 2. TokenBudgetExceeded
        let token_budget = subscriber.try_recv().expect("token_budget envelope");
        match &token_budget.event {
            crate::api::ui_protocol_ledger::UiProtocolLedgerEvent::Notification(
                UiNotification::ProgressUpdated(payload),
            ) => {
                assert_eq!(payload.metadata.kind, progress_kinds::STATUS);
                assert_eq!(
                    payload.metadata.message.as_deref(),
                    Some("token budget exceeded")
                );
                assert_eq!(payload.metadata.extra.get("used"), Some(&json!(12_000)));
                assert_eq!(payload.metadata.extra.get("limit"), Some(&json!(10_000)));
            }
            other => panic!("expected ProgressUpdated, got {other:?}"),
        }

        // 3. ActivityTimeoutReached
        let timeout = subscriber.try_recv().expect("activity_timeout envelope");
        match &timeout.event {
            crate::api::ui_protocol_ledger::UiProtocolLedgerEvent::Notification(
                UiNotification::ProgressUpdated(payload),
            ) => {
                assert_eq!(payload.metadata.kind, progress_kinds::STATUS);
                assert_eq!(
                    payload.metadata.message.as_deref(),
                    Some("activity timeout reached")
                );
                assert_eq!(
                    payload.metadata.extra.get("elapsed_ms"),
                    Some(&json!(45_000))
                );
                assert_eq!(payload.metadata.extra.get("limit_ms"), Some(&json!(30_000)));
            }
            other => panic!("expected ProgressUpdated, got {other:?}"),
        }

        // No more events — the bridge emits exactly one envelope per
        // mirrored event.
        assert!(subscriber.try_recv().is_err());
    }

    /// α-4 acceptance gate (D): the bridge does NOT mirror α-2 territory
    /// (`ToolProgress`, `ToolStarted`, `ToolCompleted`) or α-3 territory
    /// (TurnStarted/Completed). The decorator chain delegates these
    /// events to its `inner` reporter (the α-2 bridge layer or below)
    /// without itself appending — duplicate envelopes on the wire would
    /// violate the per-phase ownership contract documented in the α-2
    /// + α-3 module headers.
    #[test]
    fn should_not_mirror_alpha2_or_alpha3_territory() {
        let (inner, ledger, session_id, _turn_id, reporter) = fixture("alpha4-out-of-scope");
        let mut subscriber = ledger.subscribe(&session_id);

        reporter.report(ProgressEvent::ToolProgress {
            name: "shell".into(),
            tool_id: "call-1".into(),
            message: "running".into(),
        });
        reporter.report(ProgressEvent::ToolStarted {
            name: "shell".into(),
            tool_id: "call-1".into(),
        });
        reporter.report(ProgressEvent::ToolCompleted {
            name: "shell".into(),
            tool_id: "call-1".into(),
            success: true,
            output_preview: "ok".into(),
            duration: Duration::from_millis(10),
        });
        reporter.report(ProgressEvent::Thinking { iteration: 1 });
        reporter.report(ProgressEvent::StreamChunk {
            text: "x".into(),
            iteration: 1,
        });
        reporter.report(ProgressEvent::StreamDone { iteration: 1 });

        // All six events reached the inner reporter.
        assert_eq!(inner.events.lock().unwrap().len(), 6);

        // None of them landed on the ledger via this bridge.
        assert!(
            subscriber.try_recv().is_err(),
            "α-4 bridge must not mirror events outside its survey",
        );
    }

    /// α-4 acceptance gate (E): coexistence — the same status event
    /// reaches BOTH the SSE wire path AND the WS wire path during the
    /// coexistence period. Mirrors α-2's flagship coexistence test but
    /// for a `LlmStatus` frame, which is the canonical α-4 surface a WS
    /// client expects to route to its `progress/updated` reducer when
    /// the SSE web bundle is still live.
    #[test]
    fn should_emit_status_on_both_sse_and_ws_during_coexistence() {
        use crate::api::sse::{ChannelReporter, event_to_json};
        use serde_json::Value;

        let (sse_tx, mut sse_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let sse_reporter: Arc<dyn ProgressReporter> =
            Arc::new(ChannelReporter::new(sse_tx).with_thread_id(Some("cmid-alpha-4".into())));

        let ledger = Arc::new(UiProtocolLedger::new(64));
        let session_id = SessionKey::new("api", "alpha4-coexistence");
        let turn_id = TurnId::new();
        let mut ws_subscriber = ledger.subscribe(&session_id);

        let bridged: Arc<dyn ProgressReporter> = Arc::new(LedgerStatusGateReporter::new(
            sse_reporter,
            ledger.clone(),
            session_id.clone(),
            turn_id.clone(),
        ));

        bridged.report(ProgressEvent::LlmStatus {
            message: "retrying after rate limit".into(),
            iteration: 7,
        });

        // ---- SSE assertion ----
        let sse_raw = sse_rx.try_recv().expect("SSE wire frame must arrive");
        let sse_json: Value = serde_json::from_str(&sse_raw).unwrap();
        assert_eq!(sse_json["type"], "llm_status");
        assert_eq!(sse_json["message"], "retrying after rate limit");
        assert_eq!(sse_json["iteration"], 7);
        assert_eq!(sse_json["thread_id"], "cmid-alpha-4");
        // Sanity: confirm the canonical SSE encoder is what the channel
        // reporter actually used. If `event_to_json` ever changes shape,
        // the bridge mapping must be re-evaluated to stay in sync.
        let canonical = event_to_json(
            &ProgressEvent::LlmStatus {
                message: "retrying after rate limit".into(),
                iteration: 7,
            },
            Some("cmid-alpha-4"),
        );
        assert_eq!(canonical, sse_json);

        // ---- WS assertion ----
        let ws_event = ws_subscriber
            .try_recv()
            .expect("WS broadcast must carry progress/updated envelope");
        match &ws_event.event {
            crate::api::ui_protocol_ledger::UiProtocolLedgerEvent::Notification(
                UiNotification::ProgressUpdated(payload),
            ) => {
                assert_eq!(payload.session_id, session_id);
                assert_eq!(payload.turn_id.as_ref(), Some(&turn_id));
                assert_eq!(payload.metadata.kind, progress_kinds::STATUS);
                assert_eq!(
                    payload.metadata.message.as_deref(),
                    Some("retrying after rate limit")
                );
                assert_eq!(payload.metadata.iteration, Some(7));
            }
            other => panic!("expected ProgressUpdated notification, got {other:?}"),
        }
        let rpc = ws_event
            .event
            .clone()
            .into_rpc_notification()
            .expect("notification serializes");
        assert_eq!(rpc.method, methods::PROGRESS_UPDATED);

        // Coexistence invariant: each transport emits exactly once per event.
        assert!(sse_rx.try_recv().is_err(), "SSE must emit exactly once");
        assert!(
            ws_subscriber.try_recv().is_err(),
            "WS broadcast must emit exactly once"
        );
    }

    /// α-4 acceptance gate (F): when the inner reporter is silent (e.g.
    /// the SSE consumer disconnected mid-turn), the ledger emit still
    /// fires. Mirrors α-2's silent-inner-reporter regression — the
    /// invariant is "WS subscribers should not lose visibility just
    /// because the SSE channel went away".
    #[test]
    fn should_mirror_event_even_when_inner_reporter_panics_into_silence() {
        struct SilentReporter;
        impl ProgressReporter for SilentReporter {
            fn report(&self, _event: ProgressEvent) {}
        }
        let ledger = Arc::new(UiProtocolLedger::new(64));
        let session_id = SessionKey::new("api", "alpha4-silent");
        let turn_id = TurnId::new();
        let mut subscriber = ledger.subscribe(&session_id);

        let reporter = LedgerStatusGateReporter::new(
            Arc::new(SilentReporter) as Arc<dyn ProgressReporter>,
            ledger.clone(),
            session_id.clone(),
            turn_id.clone(),
        );

        reporter.report(ProgressEvent::TokenBudgetExceeded {
            used: 5_000,
            limit: 4_000,
        });

        let event = subscriber
            .try_recv()
            .expect("ledger emit must not depend on inner reporter");
        match &event.event {
            crate::api::ui_protocol_ledger::UiProtocolLedgerEvent::Notification(
                UiNotification::ProgressUpdated(payload),
            ) => {
                assert_eq!(payload.session_id, session_id);
                assert_eq!(payload.turn_id.as_ref(), Some(&turn_id));
                assert_eq!(payload.metadata.kind, progress_kinds::STATUS);
                assert_eq!(payload.metadata.extra.get("used"), Some(&json!(5_000)));
                assert_eq!(payload.metadata.extra.get("limit"), Some(&json!(4_000)));
            }
            other => panic!("expected ProgressUpdated notification, got {other:?}"),
        }
    }

    /// α-4 acceptance gate (G): the bridge's emits route only to the
    /// SessionKey it was given. Without this, a multi-session
    /// `octos serve` instance would cross-deliver status envelopes
    /// between concurrently-active turns. Same isolation invariant α-3
    /// asserts for turn lifecycle.
    #[test]
    fn should_route_status_to_caller_session_only() {
        let ledger = Arc::new(UiProtocolLedger::new(64));
        let session_a = SessionKey::new("api", "alpha4-iso-A");
        let session_b = SessionKey::new("api", "alpha4-iso-B");
        let turn_id = TurnId::new();

        let mut sub_a = ledger.subscribe(&session_a);
        let mut sub_b = ledger.subscribe(&session_b);

        struct SilentReporter;
        impl ProgressReporter for SilentReporter {
            fn report(&self, _event: ProgressEvent) {}
        }
        let reporter = LedgerStatusGateReporter::new(
            Arc::new(SilentReporter) as Arc<dyn ProgressReporter>,
            ledger.clone(),
            session_a.clone(),
            turn_id,
        );

        reporter.report(ProgressEvent::LlmStatus {
            message: "rerouting".into(),
            iteration: 1,
        });
        reporter.report(ProgressEvent::MaxIterationsReached { limit: 10 });

        // Session A receives both envelopes.
        assert!(sub_a.try_recv().is_ok());
        assert!(sub_a.try_recv().is_ok());
        assert!(sub_a.try_recv().is_err());

        // Session B receives nothing.
        assert!(
            sub_b.try_recv().is_err(),
            "α-4 envelopes must NOT cross-deliver to other session subscribers"
        );
    }

    /// α-4 acceptance gate (H): `thread_id` propagates from the inner
    /// reporter (the α-2 bridge wraps a `ChannelReporter` that may
    /// carry a `thread_id`). Without this, downstream consumers that
    /// inspect the reporter's thread id (e.g. the spawn_only completion
    /// path described in `progress.rs::ProgressReporter::thread_id`)
    /// would see `None` once the α-4 layer is added to the chain.
    #[test]
    fn should_pass_through_thread_id_from_inner_reporter() {
        struct ThreadedReporter;
        impl ProgressReporter for ThreadedReporter {
            fn report(&self, _event: ProgressEvent) {}
            fn thread_id(&self) -> Option<&str> {
                Some("cmid-T-abc")
            }
        }
        let ledger = Arc::new(UiProtocolLedger::new(64));
        let reporter = LedgerStatusGateReporter::new(
            Arc::new(ThreadedReporter) as Arc<dyn ProgressReporter>,
            ledger,
            SessionKey::new("api", "alpha4-thread-id"),
            TurnId::new(),
        );

        assert_eq!(reporter.thread_id(), Some("cmid-T-abc"));
    }
}
