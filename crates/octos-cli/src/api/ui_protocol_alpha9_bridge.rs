//! M9-α-9 — typed envelope helpers for α-7's once-SSE-only events.
//!
//! Per the M9-α (Sole Transport) ADR (`docs/M9-ALPHA-SOLE-TRANSPORT-ADR.md`)
//! the WebSocket transport is the sole chat transport. SSE has been
//! deleted in α-5/α-6 (atomic with the web bundle). The 5 events α-7
//! surfaced as previously-SSE-only have already been migrated to typed
//! v1 envelopes; this module is the canonical home for the helpers
//! that emit those envelopes (turn/started/completed addenda, file/
//! attached, session/event-bridged).
//!
//! Post-α-5/α-6 the helpers are not yet wired into the new WS lifecycle
//! handlers (that's a separate follow-up). They retain their unit tests
//! and will be reused as-is when the lifecycle handlers re-emit them.
//!
//! **Scope per UPCR-2026-014** (the addendum landing in this PR):
//!
//! 1. `session_result` — final session-completion identity (committed_seq +
//!    message_id + client_message_id) for the closing assistant row.
//!    Carried as `TurnCompletedEvent.session_result` so it rides on the
//!    existing `turn/completed` envelope (not a new method).
//! 2. `file_attached` — per-turn file attachment from a tool's
//!    `files_to_send`. Carried as the new `file/attached` envelope.
//! 3. `tokens_in` / `tokens_out` — final token usage for the turn.
//!    Carried as `TurnCompletedEvent.tokens_in/out`.
//! 4. `topic` on `turn/start` — sub-topic suffix for multi-topic specs.
//!    Carried as `TurnStartedEvent.topic`.
//! 5. `/api/sessions/:id/events/stream` — legacy free-form SSE event
//!    stream. Bridged onto a new `session/event` envelope that wraps the
//!    legacy `type` + payload so WS-only clients keep observing each
//!    frame as it gradually lifts onto a typed v1 envelope.
//!
//! **Coexistence invariants** (same as α-2 / α-3 / α-4):
//! - SSE delivery is unchanged — the helpers in this module ONLY append
//!   to the ledger; the SSE wire path runs through whichever channel
//!   reporter / handler emitted the original frame.
//! - Ledger appends are best-effort. A failure does not affect the SSE
//!   path or the agent loop.
//! - WS clients dedupe by stable identity (turn_id + session_id +
//!   committed_seq) so a client connected to both transports collapses
//!   the duplicate into one logical update.
//!

use std::sync::Arc;

use octos_agent::BackgroundResultPayload;
use octos_core::SessionKey;
use octos_core::ui_protocol::{
    FileAttachedEvent, TurnId, UiNotification, VisualFailedEvent, VisualGeneratingEvent,
    VisualSucceededEvent, VoiceExitEvent,
};

use super::ui_protocol_ledger::UiProtocolLedger;

fn append_alpha9_notification(ledger: &UiProtocolLedger, notification: UiNotification) {
    if let Err(error) = ledger.try_append_notification(notification) {
        tracing::warn!(%error, "failed to persist alpha9 ledger notification");
    }
}

pub(super) fn emit_file_attached(
    ledger: &Arc<UiProtocolLedger>,
    session_id: &SessionKey,
    topic: Option<&str>,
    turn_id: &TurnId,
    path: String,
    tool_call_id: Option<String>,
    mime: Option<String>,
) {
    let notification = UiNotification::FileAttached(FileAttachedEvent {
        session_id: session_id.clone(),
        topic: topic
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(ToOwned::to_owned),
        turn_id: turn_id.clone(),
        path: path.clone(),
        tool_call_id,
        attachment_owner: None,
        mime: mime.clone(),
    });
    // The attachment reaches the client as a native v2 `FileAttached`,
    // projected from this lifecycle notification (see
    // `project_lifecycle_event_to_v2_wire`), which derives size, MIME default, and the
    // owning assistant segment. No separate envelope is emitted here.
    append_alpha9_notification(ledger, notification);
}

/// Emit one `file/attached` envelope per delivered artefact when a
/// `spawn_only` background tool's `BackgroundResultPayload` lands on
/// the AppUI WS path.
///
/// Coalesces the payload's persisted-row media (`media`) and
/// projection-only media (`envelope_media`, surfaced on the canonical
/// background-child envelope) into a single
/// deduplicated stream of paths — clients receive ONE `file/attached`
/// per unique artefact regardless of which path source carried it. A
/// path that appears in both sources still emits exactly one envelope.
///
/// Defensive against the production failure mode the slides soak
/// captured (2026-05-24): PPTX artefacts that landed on disk and were
/// verified by the workspace contract never surfaced a clickable
/// button on the SPA because the old background completion carriers
/// both required the SPA's
/// content-bearing-envelope reducers to fire correctly. A dedicated
/// per-file envelope is the redundant signal that keeps the user-
/// visible delivery resilient against placement / sticky-thread bugs
/// in those richer reducers.
///
/// Best-effort: ledger append failures are logged inside the ledger
/// and do not propagate. Callers MUST run this after the persist /
/// canonical background-child projection block so the placement context (turn_id,
/// session_id) is stable. No-op when both source lists are empty.
///
/// **P0-A wire-gap (2026-05-26):** the helper strips any `#<topic>`
/// suffix from the incoming `session_id` before publishing each
/// envelope onto the ledger. The SPA subscribes only to the BASE
/// SessionKey via `session/open`; `handle_turn_start` folds the topic
/// into `params.session_id` but never re-subscribes. Result: every
/// `file_attached` event published on a topic-suffixed broadcast
/// bucket fans out to zero subscribers. Publishing on the base key
/// keeps the live subscribers (and replay) receiving the envelope.
///
/// The SPA dedupes by `tool_call_id` client-side, so the loss of
/// topic info on the wire has no UX cost — see the rationale in
/// 35a68cb9 (the topic-scope-filter exemption companion fix).
pub(super) fn emit_files_attached_from_background(
    ledger: &Arc<UiProtocolLedger>,
    session_id: &SessionKey,
    turn_id: &TurnId,
    media: &[String],
    envelope_media: &[String],
    tool_call_id: Option<String>,
) {
    use std::collections::BTreeSet;
    // #1336 round-2: capture topic BEFORE stripping the suffix below.
    // The strip is required for routing (SPA subscribes on base only),
    // but the event itself must still carry the topic so the
    // topic-scoped classifier accepts it. See [`emit_file_attached`]
    // for the full rationale.
    let topic = session_id.topic().map(ToOwned::to_owned);
    // P0-A wire-gap fix: publish on the base SessionKey (no `#<topic>`
    // suffix). The SPA's `session/open` subscription is keyed on the
    // base form; topic-suffixed broadcasts reach zero live subscribers.
    let base_session = SessionKey(session_id.base_key().to_owned());
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for path in media.iter().chain(envelope_media.iter()) {
        if path.is_empty() {
            continue;
        }
        if !seen.insert(path.clone()) {
            continue;
        }
        let mime = mime_from_path(path);
        emit_file_attached(
            ledger,
            &base_session,
            topic.as_deref(),
            turn_id,
            path.clone(),
            tool_call_id.clone(),
            mime,
        );
    }
}

/// #1477 voice rich output: announce that a background visual artifact began
/// generating, so the client can show a "generating" placeholder driven by this
/// typed event instead of scraping an in-band marker out of the assistant text.
/// Routed on the BASE session key (like [`emit_files_attached_from_background`])
/// while still carrying the topic so topic-scoped subscribers accept it.
pub(super) fn emit_visual_generating_from_background(
    ledger: &Arc<UiProtocolLedger>,
    session_id: &SessionKey,
    turn_id: &TurnId,
    kind: &str,
) {
    let topic = session_id.topic().map(ToOwned::to_owned);
    let base_session = SessionKey(session_id.base_key().to_owned());
    append_alpha9_notification(
        ledger,
        UiNotification::VisualGenerating(VisualGeneratingEvent {
            session_id: base_session,
            topic,
            turn_id: turn_id.clone(),
            kind: kind.to_owned(),
        }),
    );
}

/// #1477 voice rich output: the background visual task produced its artifact(s).
/// The structured success counterpart of
/// [`emit_visual_generating_from_background`] — the client clears the
/// "generating" placeholder off THIS, keeping the visual lifecycle decoupled
/// from `file/attached` (emitted alongside it, NOT in place of it). `files` are
/// the workspace-relative artifact names, same as the accompanying
/// `file/attached`.
pub(super) fn emit_visual_succeeded_from_background(
    ledger: &Arc<UiProtocolLedger>,
    session_id: &SessionKey,
    turn_id: &TurnId,
    kind: &str,
    files: &[String],
) {
    let topic = session_id.topic().map(ToOwned::to_owned);
    let base_session = SessionKey(session_id.base_key().to_owned());
    append_alpha9_notification(
        ledger,
        UiNotification::VisualSucceeded(VisualSucceededEvent {
            session_id: base_session,
            topic,
            turn_id: turn_id.clone(),
            kind: kind.to_owned(),
            files: files.to_vec(),
        }),
    );
}

/// #1477 voice rich output: the background visual task failed / timed out, so
/// the client should clear the "generating" placeholder.
pub(super) fn emit_visual_failed_from_background(
    ledger: &Arc<UiProtocolLedger>,
    session_id: &SessionKey,
    turn_id: &TurnId,
    reason: Option<String>,
) {
    let topic = session_id.topic().map(ToOwned::to_owned);
    let base_session = SessionKey(session_id.base_key().to_owned());
    append_alpha9_notification(
        ledger,
        UiNotification::VisualFailed(VisualFailedEvent {
            session_id: base_session,
            topic,
            turn_id: turn_id.clone(),
            reason,
        }),
    );
}

/// UPCR-2026-025 voice exit intent: announce that the voice turn detected an
/// end / goodbye / mute intent, so the client leaves the `/voice` screen and
/// returns home. The marker `[[EXIT]]` was already stripped from every
/// model-/client-facing surface; this typed event is the sole signal. Routed on
/// the BASE session key (like the visual lifecycle) while still carrying the
/// topic so topic-scoped subscribers accept it.
pub(super) fn emit_voice_exit_from_background(
    ledger: &Arc<UiProtocolLedger>,
    session_id: &SessionKey,
    turn_id: &TurnId,
) {
    let topic = session_id.topic().map(ToOwned::to_owned);
    let base_session = SessionKey(session_id.base_key().to_owned());
    append_alpha9_notification(
        ledger,
        UiNotification::VoiceExit(VoiceExitEvent {
            session_id: base_session,
            topic,
            turn_id: turn_id.clone(),
        }),
    );
}

/// Lightweight extension-based MIME sniffer used by
/// [`emit_files_attached_from_background`].
///
/// `file/attached` clients render attachments from the `mime` hint
/// when present and fall back to extension parsing otherwise. We
/// populate it with the artefact families the spawn_only producers
/// actually emit — `.pptx`, `.html`, `.md`, `.mp3`, `.mp4`, `.png`,
/// `.jpg`. Anything else returns `None` and clients fall back to
/// extension-based rendering. We deliberately do NOT crack open the
/// file (no I/O on the dispatch hot path); the wire shape is best-
/// effort so extension drift is recovered by the client.
fn mime_from_path(path: &str) -> Option<String> {
    let lower = path.to_lowercase();
    let suffix = lower.rsplit('.').next()?;
    let mime = match suffix {
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "html" | "htm" => "text/html",
        "md" | "markdown" => "text/markdown",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "mp4" => "video/mp4",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        "txt" => "text/plain",
        "json" => "application/json",
        "zip" => "application/zip",
        _ => return None,
    };
    Some(mime.to_owned())
}

/// Resolve the effective envelope-media list for a spawn_only background
/// completion payload.
///
/// The `BackgroundResultPayload` shape carries TWO media lists by design
/// (see `BackgroundResultPayload::envelope_media` doc):
///
/// - `media` lands on the durable completion row. Populated by the contract
///   `Satisfied` path with `output_files`; left empty by the
///   `NotConfigured` `send_file` fallback because each delivered file
///   already has its own per-file durable companion row.
/// - `envelope_media` surfaces on the canonical background-child
///   envelope. Populated by the
///   `NotConfigured` `send_file` fallback with `sent_files`; left
///   empty by the `Satisfied` path because `media` already carries the
///   list.
///
/// OUP coalesces this effective list onto BOTH its persisted completion row
/// and canonical envelope, so strict identity reconciliation sees the same
/// media after reload. Empty per-file companion rows remain durable; the
/// producer's split fields remain unchanged for other channel consumers.
///
/// The two media lists let the producer preserve durable transcript data
/// while placing artifacts on the canonical projection. The cost is that EVERY consumer that needs
/// to render attachments has to coalesce the two: pre-helper, the
/// `BackgroundResultSender` closure inlined an `if envelope_media
/// is_empty { media } else { envelope_media }` fallback, and a future
/// caller that forgot to mirror that fallback would silently drop
/// half of the live spawn-only completion shapes.
///
/// This helper centralises the fallback so the canonical child-envelope
/// builder, the `emit_files_attached_from_background` caller,
/// and future consumers all see the same effective list. Returning a
/// fresh `Vec<String>` (rather than a borrowed slice) keeps callers
/// free to retain ownership when both sources outlive the call site.
///
/// The slides soak round-13 (2026-05-25) confirmed the production
/// shape: `mofa_slides` enters the `Satisfied` branch with
/// `media: [deck.pptx]` and `envelope_media: []`. Without the
/// fallback, background-child and `file/attached` consumers both
/// see an empty list and the SPA never renders a download button.
pub(super) fn effective_envelope_media(payload: &BackgroundResultPayload) -> Vec<String> {
    if payload.envelope_media.is_empty() {
        payload.media.clone()
    } else {
        payload.envelope_media.clone()
    }
}
