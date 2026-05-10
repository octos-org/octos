//! M9-α-2 — bridge SSE-driven `tool_progress` (and friends) onto the M9
//! WebSocket UI Protocol path.
//!
//! Per the M9-α (Sole Transport) ADR (`docs/M9-ALPHA-SOLE-TRANSPORT-ADR.md`)
//! the WebSocket transport is migrating to be the sole chat transport.
//! This module is the α-2 phase: while SSE is still alive (deletion lands
//! in α-5/α-6 atomically with the web bundle), every `tool_progress`
//! event the agent emits during a `POST /api/chat?stream=true` turn must
//! ALSO be appended to the M9 ledger so any concurrently-connected
//! WebSocket subscriber for the same `SessionKey` sees it through the
//! live broadcast (`UiProtocolLedger::subscribe`).
//!
//! Coexistence invariants:
//! - SSE delivery is unchanged. The base reporter is invoked first.
//! - Ledger appends are best-effort. A failure does not affect the SSE
//!   path or the agent loop.
//! - The web client's `MessageStore.appendToolProgressByCallId` reducer
//!   dedupes by `(tool_call_id, message)`, so a client receiving the
//!   same payload twice (once via SSE, once via WS) collapses it to a
//!   single store entry. That is the explicit dedup contract for the
//!   coexistence period.
//!
//! Out of scope for α-2 (deferred to α-3/α-4):
//! - `tool_started` / `tool_completed` lifecycle envelopes — the spec
//!   uses `tool/progress.v1` only here. α-3 covers tool lifecycle.
//! - Session lifecycle (open/close/title/result).
//! - Heartbeat / progress-gate.
//!
//! When α-5/α-6 land and SSE is deleted, this bridge becomes the
//! straight-through reporter — its inner `Arc<dyn ProgressReporter>`
//! collapses to a no-op and the ledger append is the only path.

use std::sync::Arc;

use octos_agent::{ProgressEvent, ProgressReporter};
use octos_core::SessionKey;
use octos_core::ui_protocol::{ToolProgressEvent, TurnId, UiNotification};

use super::ui_protocol_ledger::UiProtocolLedger;

/// Decorator that delegates every event to its `inner` reporter (the SSE
/// channel reporter, in α-2's coexistence wiring) AND mirrors
/// `ProgressEvent::ToolProgress` onto the M9 ledger as a
/// `tool/progress.v1` notification so connected WebSocket subscribers
/// observe the same event.
///
/// Construction is cheap — `Arc<UiProtocolLedger>` is already
/// process-singleton (see `ui_protocol::event_ledger`), so wrapping costs
/// one pointer copy per turn.
pub(super) struct LedgerToolProgressReporter {
    inner: Arc<dyn ProgressReporter>,
    ledger: Arc<UiProtocolLedger>,
    session_id: SessionKey,
    turn_id: TurnId,
}

impl LedgerToolProgressReporter {
    /// Wrap `inner` so each emitted event is also mirrored onto `ledger`
    /// when applicable. `session_id` is the SSE turn's `SessionKey`;
    /// `turn_id` is the per-request synthetic `TurnId` that lets WS
    /// subscribers correlate this turn's tool calls with their pane state.
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
}

impl ProgressReporter for LedgerToolProgressReporter {
    fn report(&self, event: ProgressEvent) {
        // Mirror to the ledger BEFORE delegating to the inner reporter.
        // Order matters: if the inner reporter blocks on a backpressured
        // SSE channel (rare; the SSE tx is unbounded today), we still
        // want WS subscribers to see the progress promptly.
        if let ProgressEvent::ToolProgress {
            tool_id, message, ..
        } = &event
        {
            let notification = UiNotification::ToolProgress(ToolProgressEvent {
                session_id: self.session_id.clone(),
                turn_id: self.turn_id.clone(),
                tool_call_id: tool_id.clone(),
                message: Some(message.clone()),
                progress_pct: None,
            });
            // `append_notification` performs an in-process broadcast and
            // (when `data_dir` is configured) a write-ahead disk record.
            // Both paths are infallible from the caller's POV — disk
            // errors are logged but do not panic.
            let _ = self.ledger.append_notification(notification);
        }
        self.inner.report(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use octos_core::ui_protocol::methods;
    use std::sync::Mutex;

    /// Test double that captures every event the inner reporter receives.
    /// Stands in for the SSE `ChannelReporter` so we can assert SSE
    /// delivery is preserved during the α-2 coexistence period.
    #[derive(Default)]
    struct CapturingReporter {
        events: Mutex<Vec<ProgressEvent>>,
    }

    impl ProgressReporter for CapturingReporter {
        fn report(&self, event: ProgressEvent) {
            self.events.lock().unwrap().push(event);
        }
    }

    fn fixture(session_id: &str) -> (Arc<CapturingReporter>, LedgerToolProgressReporter) {
        let inner = Arc::new(CapturingReporter::default());
        let ledger = Arc::new(UiProtocolLedger::new(64));
        let reporter = LedgerToolProgressReporter::new(
            inner.clone() as Arc<dyn ProgressReporter>,
            ledger.clone(),
            SessionKey::new("api", session_id),
            TurnId::new(),
        );
        (inner, reporter)
    }

    #[test]
    fn should_mirror_tool_progress_to_ledger_when_event_is_tool_progress() {
        let inner = Arc::new(CapturingReporter::default());
        let ledger = Arc::new(UiProtocolLedger::new(64));
        let session_id = SessionKey::new("api", "alpha2-fixture");
        let turn_id = TurnId::new();

        let mut subscriber = ledger.subscribe(&session_id);

        let reporter = LedgerToolProgressReporter::new(
            inner.clone() as Arc<dyn ProgressReporter>,
            ledger.clone(),
            session_id.clone(),
            turn_id.clone(),
        );

        reporter.report(ProgressEvent::ToolProgress {
            name: "deep_search".into(),
            tool_id: "call_42".into(),
            message: "phase 2/4".into(),
        });

        // SSE side: the inner reporter still saw the event.
        let captured = inner.events.lock().unwrap();
        assert_eq!(captured.len(), 1, "inner reporter must receive the event");
        match &captured[0] {
            ProgressEvent::ToolProgress {
                name,
                tool_id,
                message,
            } => {
                assert_eq!(name, "deep_search");
                assert_eq!(tool_id, "call_42");
                assert_eq!(message, "phase 2/4");
            }
            other => panic!("expected ToolProgress, got {other:?}"),
        }

        // WS side: the ledger broadcast emitted a tool/progress.v1
        // notification with matching identity.
        let event = subscriber
            .try_recv()
            .expect("ledger must broadcast tool_progress");
        let method = match &event.event {
            crate::api::ui_protocol_ledger::UiProtocolLedgerEvent::Notification(n) => n.method(),
            other => panic!("expected Notification, got {other:?}"),
        };
        assert_eq!(method, methods::TOOL_PROGRESS);
        match &event.event {
            crate::api::ui_protocol_ledger::UiProtocolLedgerEvent::Notification(
                UiNotification::ToolProgress(payload),
            ) => {
                assert_eq!(payload.session_id, session_id);
                assert_eq!(payload.turn_id, turn_id);
                assert_eq!(payload.tool_call_id, "call_42");
                assert_eq!(payload.message.as_deref(), Some("phase 2/4"));
            }
            other => panic!("expected ToolProgress notification, got {other:?}"),
        }
    }

    #[test]
    fn should_pass_through_non_tool_progress_events_without_ledger_emit() {
        let (inner, reporter) = fixture("alpha2-passthrough");

        reporter.report(ProgressEvent::Thinking { iteration: 3 });
        reporter.report(ProgressEvent::ToolStarted {
            name: "shell".into(),
            tool_id: "call_x".into(),
        });

        // Inner reporter receives both events.
        let captured = inner.events.lock().unwrap();
        assert_eq!(captured.len(), 2);
        // No ledger subscriber asserted, but `append_notification` is
        // skipped entirely for non-ToolProgress events — the public
        // contract is "ToolProgress is the only mirror in α-2".
    }

    /// α-2 acceptance gate: the same `ProgressEvent::ToolProgress`
    /// must reach BOTH the SSE wire path AND the WS wire path during
    /// coexistence. This exercises the full reporter chain that
    /// `chat_streaming` builds:
    ///
    ///   ChannelReporter (SSE side)  ←──┐
    ///                                  │
    ///   LedgerToolProgressReporter ────┘ also appends to UiProtocolLedger,
    ///                                    which broadcasts to WS subscribers.
    #[test]
    fn should_emit_tool_progress_on_both_sse_and_ws_during_coexistence() {
        use crate::api::sse::{ChannelReporter, event_to_json};
        use serde_json::Value;

        // SSE side: same channel reporter that handlers.rs::chat_streaming
        // builds. Bind a thread_id matching the cmid path.
        let (sse_tx, mut sse_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let sse_reporter: Arc<dyn ProgressReporter> =
            Arc::new(ChannelReporter::new(sse_tx).with_thread_id(Some("cmid-alpha-2".into())));

        // WS side: subscribe BEFORE emitting so the broadcast catches the
        // event. The ledger is what `event_ledger(state)` returns in
        // production (process-singleton); we instantiate fresh per-test.
        let ledger = Arc::new(UiProtocolLedger::new(64));
        let session_id = SessionKey::new("api", "alpha2-coexistence");
        let turn_id = TurnId::new();
        let mut ws_subscriber = ledger.subscribe(&session_id);

        let bridged: Arc<dyn ProgressReporter> = Arc::new(LedgerToolProgressReporter::new(
            sse_reporter,
            ledger.clone(),
            session_id.clone(),
            turn_id.clone(),
        ));

        // Fire a single ProgressEvent::ToolProgress, identical to what
        // deep_research emits during a long-running spawn_only run.
        bridged.report(ProgressEvent::ToolProgress {
            name: "deep_search".into(),
            tool_id: "call_42".into(),
            message: "phase 2/4".into(),
        });

        // ---- SSE assertion ----
        // The channel reporter encoded a JSON payload matching the
        // legacy SSE wire format consumed by sse-bridge.ts.
        let sse_raw = sse_rx.try_recv().expect("SSE wire frame must arrive");
        let sse_json: Value = serde_json::from_str(&sse_raw).unwrap();
        assert_eq!(sse_json["type"], "tool_progress");
        assert_eq!(sse_json["tool"], "deep_search");
        assert_eq!(sse_json["tool_call_id"], "call_42");
        assert_eq!(sse_json["message"], "phase 2/4");
        assert_eq!(sse_json["thread_id"], "cmid-alpha-2");
        // Sanity: confirm the SSE encoder is the canonical one used by
        // chat_streaming. If `event_to_json` is ever bypassed, this
        // assertion would need updating before the bridge change.
        let canonical = event_to_json(
            &ProgressEvent::ToolProgress {
                name: "deep_search".into(),
                tool_id: "call_42".into(),
                message: "phase 2/4".into(),
            },
            Some("cmid-alpha-2"),
        );
        assert_eq!(canonical, sse_json);

        // ---- WS assertion ----
        // The bridge appended a `tool/progress.v1` notification to the
        // ledger; the broadcast subscribed above has it ready.
        let ws_event = ws_subscriber
            .try_recv()
            .expect("WS broadcast must carry tool_progress envelope");
        match &ws_event.event {
            crate::api::ui_protocol_ledger::UiProtocolLedgerEvent::Notification(
                UiNotification::ToolProgress(payload),
            ) => {
                assert_eq!(payload.session_id, session_id);
                assert_eq!(payload.turn_id, turn_id);
                assert_eq!(payload.tool_call_id, "call_42");
                assert_eq!(payload.message.as_deref(), Some("phase 2/4"));
            }
            other => panic!("expected ToolProgress notification, got {other:?}"),
        }
        // The forwarder serializes the ledger entry into the WS frame
        // via `into_rpc_notification`. Assert the wire method matches
        // the protocol spec (`tool/progress.v1`) so a WS client that
        // routes by method name picks it up.
        let rpc = ws_event
            .event
            .clone()
            .into_rpc_notification()
            .expect("notification serializes");
        assert_eq!(rpc.method, methods::TOOL_PROGRESS);

        // Coexistence invariant: the inner SSE channel must NOT have
        // received a duplicate frame, and the WS broadcast must NOT
        // have replayed.
        assert!(sse_rx.try_recv().is_err(), "SSE must emit exactly once");
        assert!(
            ws_subscriber.try_recv().is_err(),
            "WS broadcast must emit exactly once"
        );
    }

    #[test]
    fn should_mirror_event_even_when_inner_reporter_panics_into_silence() {
        // Inner reporter is a no-op silent reporter; ensure ledger emit
        // still fires and the call returns normally. Mirrors a real-world
        // case where the SSE receiver disconnects mid-turn.
        struct SilentReporter;
        impl ProgressReporter for SilentReporter {
            fn report(&self, _event: ProgressEvent) {}
        }
        let ledger = Arc::new(UiProtocolLedger::new(64));
        let session_id = SessionKey::new("api", "alpha2-silent");
        let turn_id = TurnId::new();
        let mut subscriber = ledger.subscribe(&session_id);

        let reporter = LedgerToolProgressReporter::new(
            Arc::new(SilentReporter) as Arc<dyn ProgressReporter>,
            ledger.clone(),
            session_id,
            turn_id,
        );

        reporter.report(ProgressEvent::ToolProgress {
            name: "run_pipeline".into(),
            tool_id: "call_silent".into(),
            message: "still progressing".into(),
        });

        let event = subscriber
            .try_recv()
            .expect("ledger emit must not depend on inner reporter");
        match &event.event {
            crate::api::ui_protocol_ledger::UiProtocolLedgerEvent::Notification(
                UiNotification::ToolProgress(payload),
            ) => {
                assert_eq!(payload.tool_call_id, "call_silent");
                assert_eq!(payload.message.as_deref(), Some("still progressing"));
            }
            other => panic!("expected ToolProgress notification, got {other:?}"),
        }
    }
}
