//! Preparation of lifecycle and general durable wire deliveries.

use super::*;

pub(super) fn warn_ledger_failure(context: &'static str, error: &LedgerError) {
    tracing::warn!(
        target: "octos::ui_protocol::ws",
        context,
        error = %error,
        "durable ledger operation failed"
    );
}

/// Shared prep of the sync/async lifecycle delivery twins (#2807): ledger
/// append, v2 projection, capability filter, frame assembly. Returns the
/// wire frame to enqueue plus its [`SendMeta`]. `Ok(None)` =
/// fully handled without touching the wire (the v2 forwarder lane, or a
/// capability-filtered event), so there is nothing to enqueue.
pub(super) fn prepare_lifecycle_send(
    ws: &WsConnection,
    ledger: &UiProtocolLedger,
    notification: UiNotification,
) -> Result<Option<(WsMessage, SendMeta)>, SendError> {
    let features = ws.snapshot_live_features();
    if matches!(
        &notification,
        UiNotification::TurnCompleted(_) | UiNotification::TurnError(_)
    ) && (features.projection_envelope_v2 || ws.has_live_forwarder(notification.session_id()))
    {
        // A canonical v2 persisted row is produced by the commit observer and
        // reaches this connection through the ordered ledger forwarder. If we
        // direct-send the terminal here, it can overtake that forwarder even
        // though the persisted row has the smaller durable cursor. The client
        // then finalizes an empty/partial turn and rejects the late canonical
        // row as post-terminal — the real first-turn soak failure.
        //
        // Put v2 terminals onto the same broadcast lane instead. Appending
        // without the originating-connection suppression tag makes this
        // connection's forwarder deliver it after every earlier ledger row.
        // The v2 stream is cursor-replayable, so a writer failure is recovered
        // by session hydration rather than by letting a lifecycle-priority
        // frame violate the projection's ordering contract.
        //
        // A connection without v2 takes this lane too whenever a live
        // forwarder runs for the session: it receives the same v2
        // projection, and on the direct lane a `serve --stdio` client saw
        // `turn_terminal` seq 48 ahead of the turn's queued `assistant_delta`
        // rows 16..47. With no forwarder (`turn/start` without
        // `session/open`) nothing would deliver an untagged terminal, so it
        // keeps the direct lifecycle path below.
        ledger
            .try_append_notification(notification)
            .map_err(|error| SendError::LifecycleFailure(error.to_string()))?;
        return Ok(None);
    }

    // Tag the broadcast with the originating connection so this
    // connection's own live forwarder skips the duplicate copy.
    let event = ledger
        .try_append_notification_from(notification, ws.connection_id)
        .map_err(|error| SendError::LifecycleFailure(error.to_string()))?;
    let cursor = event.cursor.clone();
    // Codex #1336 round-2 BLOCKER 1: apply the per-connection
    // capability filter at the direct-send boundary. Without this, a
    // connection that negotiated `projection.envelope.v1` would still
    // receive direct-sent legacy `TurnCompleted` lifecycle frames,
    // violating the γ cutover gate's mutual exclusion contract. The
    // ledger append above still happens so the canonical envelope
    // emit (via `ledger.emit_envelope` on the same handler path)
    // delivers via the broadcast forwarder.
    let projected = project_lifecycle_event_to_v2_wire(ledger, &event.event, &event.cursor);
    let event_for_wire = context_event_for_features(projected.unwrap_or(event.event), features);
    let delivery_metric = ui_protocol_delivery_metric(&event_for_wire);
    let method = ledger_event_method(&event_for_wire).to_string();
    if !live_event_passes_capability_filter(&event_for_wire, features) {
        return Ok(None);
    }
    let frame = frame_from_ledger(event_for_wire)
        .ok_or_else(|| SendError::LifecycleFailure(format!("serialize {method}")))?;
    Ok(Some((
        frame,
        SendMeta {
            cursor,
            delivery_metric,
            method,
        },
    )))
}

/// Shared prep of the sync/async durable delivery twins (#2807): goal guard,
/// evidence recording, forwarder-lane routing, ledger append, v2 projection,
/// capability filter, frame assembly. Returns the wire frame to enqueue plus
/// its [`SendMeta`]. `Ok(None)` = fully handled without touching the wire,
/// so there is nothing to enqueue.
pub(super) fn prepare_durable_send(
    ws: &WsConnection,
    ledger: &UiProtocolLedger,
    notification: UiNotification,
) -> Result<Option<(WsMessage, SendMeta)>, SendError> {
    // #1959 — drop a goal chip event that a newer clear/update already
    // superseded for this session (see the guard's doc comment).
    if !goal_event_passes_generation_guard(&notification) {
        return Ok(None);
    }
    // M15-F5 (#44): mirror production supervised-task lifecycle updates into
    // the `task-ledger.jsonl` evidence ledger. NO-OP unless the live tmux soak
    // set `OCTOSCODE_M15_UX_OUTPUT_DIR`, so this is free in normal production.
    record_task_evidence(&notification);
    // A lifecycle source that `project_lifecycle_event_to_v2_wire` turns
    // into a v2 row takes the ordered forwarder lane while a live forwarder
    // runs for its session, like the terminal in `send_notification_lifecycle`:
    // its projected per-thread `seq` follows every earlier ledger row, so a
    // direct send could overtake rows still queued on that forwarder. Without
    // one, the direct path below is the only live delivery.
    if matches!(
        &notification,
        UiNotification::TurnCompleted(_)
            | UiNotification::TurnError(_)
            | UiNotification::FileAttached(_)
            | UiNotification::TurnSpawnComplete(_)
    ) && ws.has_live_forwarder(notification.session_id())
    {
        ledger
            .try_append_notification(notification)
            .map_err(|error| SendError::LifecycleFailure(error.to_string()))?;
        return Ok(None);
    }
    let event = ledger
        .try_append_notification_from(notification, ws.connection_id)
        .map_err(|error| SendError::LifecycleFailure(error.to_string()))?;
    let cursor = event.cursor.clone();
    // Peer close/purge may run on the host connection while the question
    // belongs to an external client. Persist it, but never direct-send it
    // to the host (the normal forwarder/replay paths apply this same gate).
    if matches!(
        &event.event,
        UiProtocolLedgerEvent::Notification(UiNotification::UserQuestionCancelled(_))
    ) && !ledgered_event_visible_to_connection(&event, ws.connection_id)
    {
        return Ok(None);
    }
    // Codex #1336 round-2 BLOCKER 1: apply the per-connection
    // capability filter at the direct-send boundary. A connection
    // that negotiated `projection.envelope.v1` must not receive
    // direct-sent legacy notifications (for example,
    // `ToolStarted` / `ToolProgress` / `ToolCompleted`,
    // `FileAttached`, etc.) — the canonical envelopes emitted by
    // `ledger.emit_envelope` reach the same connection via the live
    // forwarder. Ledger append still occurs above so OTHER
    // connections (without the feature) receive the legacy shape via
    // their own forwarders.
    let features = ws.snapshot_live_features();
    let projected = project_lifecycle_event_to_v2_wire(ledger, &event.event, &event.cursor);
    let event_for_wire = context_event_for_features(projected.unwrap_or(event.event), features);
    let delivery_metric = ui_protocol_delivery_metric(&event_for_wire);
    let method = ledger_event_method(&event_for_wire).to_string();
    if !live_event_passes_capability_filter(&event_for_wire, features) {
        return Ok(None);
    }
    let frame = match frame_from_ledger(event_for_wire) {
        Some(frame) => frame,
        None => {
            return Err(SendError::BackpressureDrop);
        }
    };
    Ok(Some((
        frame,
        SendMeta {
            cursor,
            delivery_metric,
            method,
        },
    )))
}

pub(super) fn emit_session_compact_notifications(
    ws: &WsConnection,
    ledger: &UiProtocolLedger,
    features: ConnectionUiFeatures,
    notifications: Vec<UiNotification>,
) -> Result<(), RpcError> {
    for notification in notifications {
        if features.context_lifecycle_available() {
            send_notification_durable(ws, ledger, notification).map_err(|error| {
                RpcError::internal_error(format!("session/compact delivery failed: {error:?}"))
            })?;
        } else {
            ledger.try_append_notification_from(notification, ws.connection_id)?;
        }
    }
    Ok(())
}
