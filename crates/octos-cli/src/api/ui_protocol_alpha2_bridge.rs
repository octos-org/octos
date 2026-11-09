//! M9-α-2 — `tool_progress` ledger reporter (post-α-5/α-6).
//!
//! Per the M9-α (Sole Transport) ADR (`docs/M9-ALPHA-SOLE-TRANSPORT-ADR.md`)
//! the WebSocket transport is the sole chat transport. SSE has been
//! deleted in α-5/α-6 (atomic with the web bundle), so the
//! `LedgerToolProgressReporter` defined here no longer has an SSE
//! consumer to mirror — its inner `Arc<dyn ProgressReporter>` is now a
//! pass-through (or no-op) and the ledger append is the only delivery
//! path.
//!
//! The struct is retained as a documented building block for the M9
//! WebSocket lifecycle wiring (the unit tests pin its contract), and the
//! `dead_code` allow keeps the module compiling until a future PR
//! re-installs it as the canonical path inside the new `turn/start` WS
//! handler.

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
/// process-singleton (see `ui_protocol_transport::event_ledger`), so wrapping costs
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
                topic: self.session_id.topic().map(ToOwned::to_owned),
                turn_id: self.turn_id.clone(),
                tool_call_id: tool_id.clone(),
                message: Some(message.clone()),
                progress_pct: None,
            });
            // A durable failure must not fan out a row that restart would lose.
            if let Err(error) = self.ledger.try_append_notification(notification) {
                tracing::warn!(%error, "failed to persist alpha2 tool progress");
            }
        }
        self.inner.report(event);
    }
}
