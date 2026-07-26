//! Draft client/runtime protocol types for M9.
//!
//! This module intentionally captures only the first protocol slice needed to
//! align client and server work. A first WebSocket server slice now handles
//! session open, turn start, turn interrupt, approval, diff preview, and
//! task-output read requests. The full protocol model also defines harness
//! task-control requests so clients can target a stable AppUI contract while
//! backend support lands behind capabilities.

use crate::{SessionKey, TaskId, ThreadId};
use chrono::{DateTime, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::sync::{OnceLock, RwLock};
use uuid::Uuid;

/// Draft protocol identifier for the first control-plane transport.
pub const UI_PROTOCOL_V1: &str = "octos-ui/v1alpha1";

/// Durable schema version for UI protocol v1 JSON payloads.
pub const UI_PROTOCOL_SCHEMA_VERSION: u32 = 1;

/// Durable schema version for the advertised capability payload.
pub const UI_PROTOCOL_CAPABILITIES_SCHEMA_VERSION: u32 = 2;

/// JSON-RPC version used by UI protocol v1 wire envelopes.
pub const JSON_RPC_VERSION: &str = "2.0";

/// Maximum accepted JSON-RPC text frame size for UI transports.
pub const MAX_TEXT_FRAME_BYTES: usize = 1024 * 1024;

/// Per-turn ownership context for UI/SSE emission.
///
/// Construct this once at ingress, before any live event is emitted, and pass
/// it to emitters instead of re-deriving routing identity from ambient session
/// state. The `thread_id` is required; callers that do not have one must fail
/// before producing a live event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnContext {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    pub thread_id: ThreadId,
}

impl TurnContext {
    pub fn new(session_id: impl Into<String>, topic: Option<String>, thread_id: ThreadId) -> Self {
        Self {
            session_id: session_id.into(),
            topic,
            thread_id,
        }
    }

    pub fn thread_id_str(&self) -> &str {
        self.thread_id.as_str()
    }
}

/// Required server-stamped ownership envelope for web/SSE events.
///
/// This is intentionally generic over payload so individual event families can
/// keep their existing typed payloads while sharing one ownership/routing
/// contract at the emission boundary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventEnvelope<P> {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    pub thread_id: ThreadId,
    pub event_seq: u64,
    pub event_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    pub payload: P,
}

impl<P> EventEnvelope<P> {
    pub fn new(
        ctx: &TurnContext,
        event_seq: u64,
        event_type: impl Into<String>,
        tool_call_id: Option<String>,
        payload: P,
    ) -> Self {
        Self {
            session_id: ctx.session_id.clone(),
            topic: ctx.topic.clone(),
            thread_id: ctx.thread_id.clone(),
            event_seq,
            event_type: event_type.into(),
            tool_call_id,
            payload,
        }
    }
}

/// Feature flag for UPCR-2026-001 typed approval payloads.
pub const UI_PROTOCOL_FEATURE_APPROVAL_TYPED_V1: &str = "approval.typed.v1";

/// Feature flag for UPCR-2026-002 pane snapshot payloads.
pub const UI_PROTOCOL_FEATURE_PANE_SNAPSHOTS_V1: &str = "pane.snapshots.v1";

/// Feature flag for UPCR-2026-003 per-session workspace cwd requests.
pub const UI_PROTOCOL_FEATURE_SESSION_WORKSPACE_CWD_V1: &str = "session.workspace_cwd.v1";

/// Feature flag for UPCR-2026-022 per-session sandbox narrowing requests.
pub const UI_PROTOCOL_FEATURE_SESSION_SANDBOX_V1: &str = "session.sandbox.v1";

/// Feature flag for harness task registry/control commands.
pub const UI_PROTOCOL_FEATURE_HARNESS_TASK_CONTROL_V1: &str = "harness.task_control.v1";

/// Feature flag for UPCR-2026-009 `session/hydrate` authoritative chat-state
/// reload RPC.
pub const UI_PROTOCOL_FEATURE_SESSION_HYDRATE_V1: &str = "state.session_hydrate.v1";

/// Feature flag for UPCR-2026-010 `thread/graph/get` thread partition RPC.
pub const UI_PROTOCOL_FEATURE_THREAD_GRAPH_V1: &str = "state.thread_graph.v1";

/// Feature flag for UPCR-2026-011 `turn/state/get` turn lifecycle RPC.
pub const UI_PROTOCOL_FEATURE_TURN_STATE_GET_V1: &str = "state.turn_state_get.v1";

/// Feature flag for M10 Phase 1 `turn/spawn_complete` envelope event.
///
/// Retained to identify historic `turn/spawn_complete` durable records during
/// migration. New background-result writes use
/// [`PayloadV2::BackgroundChildCompleted`] on a canonical v2 child stream.
pub const UI_PROTOCOL_FEATURE_SPAWN_COMPLETE_V1: &str = "event.spawn_complete.v1";

/// Feature flag for the explicit `file/attached` envelope (UPCR-2026-014
/// M9-α-9).
///
/// Surfaces a dedicated, dedicated-shape notification per artefact path
/// when a `spawn_only` background tool (`mofa_slides`, `podcast_generate`,
/// `fm_tts`, `deep_search`) — or any code path that drains a
/// `BackgroundResultPayload` with non-empty `media` / `envelope_media` —
/// commits to the canonical session ledger. Mirrors per-file media carried by
/// the background child payload (and by historic `turn/spawn_complete` rows), but
/// as an isolated wire signal so SPA reducers (and admin / debug
/// clients) can subscribe to file deliveries without parsing the
/// content-bearing envelopes.
///
/// The fan-out is best-effort and additive: when no client has
/// negotiated this capability the helper still appends the envelope to
/// the ledger (subscribers + replay buffers continue to observe), but
/// the dedicated per-connection wire filter drops the frame so legacy
/// clients see no behaviour change. Clients that advertise this
/// capability receive a `file/attached` per artefact in addition to the
/// canonical background child completion payload.
///
/// Wired by the AppUI WS path's `BackgroundResultSender` closure (see
/// `ui_protocol.rs::install_message_commit_observer` adjacent helpers)
/// after each background result row commits. The notification's
/// `(turn_id, path)` pair gives the SPA an authoritative placement
/// signal even when `turn/spawn_complete`'s richer envelope is delayed
/// or lost to a wire-level filter mismatch — the exact failure mode
/// captured by the slides soak (2026-05-24, 5/8 successful generations
/// produced PPTX bytes but 0/8 surfaced a clickable button on the SPA).
pub const UI_PROTOCOL_FEATURE_FILE_ATTACHED_V1: &str = "event.file_attached.v1";

/// Feature flag for UPCR-2026-014 M9-γ canonical projection envelope.
///
/// Capability-gated — servers advertise it only when they emit the
/// canonical [`Envelope`] shape (see § 14 of the spec). Legacy
/// `message/delta`, `tool/*`, and `turn/completed`
/// notifications continue to flow on connections that do not negotiate
/// this feature, until M9-γ-3 deletes them.
pub const UI_PROTOCOL_FEATURE_PROJECTION_ENVELOPE_V1: &str = "projection.envelope.v1";

/// Feature flag for the Stage 1 canonical projection envelope contract.
///
/// This remains the request token for projecting historical source records
/// into v2 alongside (not a replacement for)
/// [`UI_PROTOCOL_FEATURE_PROJECTION_ENVELOPE_V1`]. Stage 5 writes canonical
/// [`EnvelopeV2`] rows directly, and those rows are delivered regardless of
/// feature negotiation. V2 retains the flattened `projection/envelope`
/// method shape while adding a durable ledger cursor, an explicit turn id,
/// assistant-segment identity, terminal outcomes, and linked
/// background-child completions.
pub const UI_PROTOCOL_FEATURE_PROJECTION_ENVELOPE_V2: &str = "projection.envelope.v2";

/// Feature flag for M12 Phase D-1 auxiliary REST→WS migration.
///
/// Negotiated by clients that route auxiliary panel traffic (sidebar
/// session list, right-rail snapshot/files/tasks, status pill, messages
/// history scroll, workspace contract panel, title rename, session
/// delete, content gallery) onto the existing
/// `/api/ui-protocol/ws` JSON-RPC connection instead of the legacy REST
/// endpoints on `/api/sessions/*`, `/api/status`, and `/api/my/content`.
/// See `docs/adr/m12-phase-d-auxiliary-rest-to-ws.md`.
///
/// REST endpoints stay live for clients that do not negotiate this
/// feature; D-1 is additive only. Phase D-5 retires the REST routes
/// once `octos-web` has migrated (tracked separately).
pub const UI_PROTOCOL_FEATURE_AUXILIARY_REST_TO_WS_V1: &str = "auxiliary.rest_to_ws.v1";

/// Required feature flag for UPCR-2026-021 M15 autonomy inspection/control.
pub const UI_PROTOCOL_FEATURE_CODING_AUTONOMY_V1: &str = "coding.autonomy.v1";

/// Optional M15 feature flag for backend-owned agent lifecycle controls.
pub const UI_PROTOCOL_FEATURE_CODING_AGENT_CONTROL_V1: &str = "coding.agent_control.v1";

/// Optional M15 feature flag for persisted goal runtime controls.
pub const UI_PROTOCOL_FEATURE_CODING_GOAL_RUNTIME_V1: &str = "coding.goal_runtime.v1";

/// Optional M15 feature flag for recurring loop runtime controls.
pub const UI_PROTOCOL_FEATURE_CODING_LOOP_RUNTIME_V1: &str = "coding.loop_runtime.v1";

/// Optional M15 feature flag for backend-owned product review workflows.
pub const UI_PROTOCOL_FEATURE_REVIEW_START_V1: &str = "review.start.v1";

/// Feature flag for backend-owned context generation, checkpoint, and
/// compaction lifecycle inspection.
pub const UI_PROTOCOL_FEATURE_CONTEXT_LIFECYCLE_V1: &str = "context.lifecycle.v1";

/// #965 / UPCR-2026-019 — spec-canonical feature name for the
/// supervised-task inspection surface (`task/list`, `task/updated`,
/// `task/output/read`, `agent/list`, `agent/status/read`, `agent/output/read`,
/// `agent/interrupt`, `agent/close`). The methods themselves stay gated on
/// [`UI_PROTOCOL_FEATURE_CODING_AGENT_CONTROL_V1`] so older clients keep
/// working; this constant is advertised in parallel so the protocol
/// vocabulary matches the M13-A spec strings.
pub const UI_PROTOCOL_FEATURE_HARNESS_TASK_SUPERVISION_INSPECTION_V1: &str =
    "harness.task_supervision_inspection.v1";

/// #965 / UPCR-2026-019 — spec-canonical feature name for the
/// supervised-task artifact surface (`task/artifact/list`, `task/artifact/read`,
/// `agent/artifact/list`, `agent/artifact/read`, `agent/artifact/updated`).
/// The canonical `task/artifact/*` methods are gated on this flag; legacy
/// `agent/artifact/*` aliases remain gated on agent control.
pub const UI_PROTOCOL_FEATURE_HARNESS_TASK_ARTIFACTS_V1: &str = "harness.task_artifacts.v1";

/// Feature flag for UPCR-2026-023 structured `AskUserQuestion` mid-turn
/// user questions. Gates the `user_question/respond` command, the
/// `user_question/requested` notification, and the structured `questions`
/// field on the request event. Advertised through optional
/// `supported_features` in [`UiProtocolCapabilities`]; clients request it
/// through `X-Octos-Ui-Features`. When it is NOT negotiated the agent's
/// `ask_user_question` tool degrades to the `request_user_input`
/// structured-metadata fallback, so the turn never hard-blocks.
pub const UI_PROTOCOL_FEATURE_USER_QUESTION_V1: &str = "user_question.v1";

/// Feature flag for streamed voice-reply audio. When negotiated, the server
/// pushes `voice/audio_chunk` notifications (base64 audio frames) as the
/// cloud TTS synthesizes, so the client can play progressively (MSE) instead
/// of waiting for a complete `file/attached` reply. Not negotiated → the voice
/// turn keeps emitting whole-file `file/attached` audio.
pub const UI_PROTOCOL_FEATURE_VOICE_AUDIO_V1: &str = "event.voice_audio.v1";

/// Feature flag for the model-authored plan/todo checklist. When negotiated,
/// the server pushes `plan/updated` notifications carrying the agent's current
/// ordered checklist (the `update_plan` tool's live state), and replays the
/// latest snapshot on `session/open`. Not negotiated → the plan rides out only
/// on the legacy `tool/completed` `structured_metadata` path.
pub const UI_PROTOCOL_FEATURE_PLAN_TODOS_V1: &str = "plan.todos.v1";

/// Smart-home bridge control (self-hosted/LAN bridge only). Gates
/// `smart_home/status.get`, `smart_home/device.list`,
/// `smart_home/device.command`, `smart_home/camera.stream_start`, and
/// `smart_home/camera.stream_stop`. Device control/state moved server-side
/// so the profile's bridge credentials never reach the browser; camera video
/// itself still streams directly browser-to-bridge (these methods only
/// return the playback URL).
pub const UI_PROTOCOL_FEATURE_SMART_HOME_V1: &str = "smart_home.v1";

/// Server-known feature registry. Used by
/// [`UiProtocolCapabilities::for_negotiated_features`] (UPCR-2026-007) to
/// intersect a client's `X-Octos-Ui-Features` request with the names the
/// server actually honours. The order is the canonical advertisement order
/// surfaced through `supported_features`.
pub const UI_PROTOCOL_KNOWN_FEATURES: &[&str] = &[
    UI_PROTOCOL_FEATURE_APPROVAL_TYPED_V1,
    UI_PROTOCOL_FEATURE_PANE_SNAPSHOTS_V1,
    UI_PROTOCOL_FEATURE_SESSION_WORKSPACE_CWD_V1,
    UI_PROTOCOL_FEATURE_SESSION_SANDBOX_V1,
    UI_PROTOCOL_FEATURE_HARNESS_TASK_CONTROL_V1,
    UI_PROTOCOL_FEATURE_SESSION_HYDRATE_V1,
    UI_PROTOCOL_FEATURE_THREAD_GRAPH_V1,
    UI_PROTOCOL_FEATURE_TURN_STATE_GET_V1,
    UI_PROTOCOL_FEATURE_SPAWN_COMPLETE_V1,
    UI_PROTOCOL_FEATURE_FILE_ATTACHED_V1,
    UI_PROTOCOL_FEATURE_PROJECTION_ENVELOPE_V1,
    UI_PROTOCOL_FEATURE_PROJECTION_ENVELOPE_V2,
    UI_PROTOCOL_FEATURE_AUXILIARY_REST_TO_WS_V1,
    UI_PROTOCOL_FEATURE_CODING_AUTONOMY_V1,
    UI_PROTOCOL_FEATURE_CODING_AGENT_CONTROL_V1,
    UI_PROTOCOL_FEATURE_CODING_GOAL_RUNTIME_V1,
    UI_PROTOCOL_FEATURE_CODING_LOOP_RUNTIME_V1,
    UI_PROTOCOL_FEATURE_REVIEW_START_V1,
    UI_PROTOCOL_FEATURE_CONTEXT_LIFECYCLE_V1,
    UI_PROTOCOL_FEATURE_HARNESS_TASK_SUPERVISION_INSPECTION_V1,
    UI_PROTOCOL_FEATURE_HARNESS_TASK_ARTIFACTS_V1,
    UI_PROTOCOL_FEATURE_USER_QUESTION_V1,
    UI_PROTOCOL_FEATURE_VOICE_AUDIO_V1,
    UI_PROTOCOL_FEATURE_PLAN_TODOS_V1,
    UI_PROTOCOL_FEATURE_SMART_HOME_V1,
];

/// Returns the feature flag that gates `method` per spec § 7 capability
/// negotiation, or `None` if the method is unconditionally available.
///
/// Used by [`UiProtocolCapabilities::for_negotiated_features`] so the
/// negotiated `supported_methods` only advertises capability-gated methods
/// when their gating feature is also in the negotiated `supported_features`
/// set. Without this gate a client that did not request
/// `harness.task_control.v1` would see `task/list` / `task/cancel` /
/// `task/restart_from_node` in the response and then receive
/// `method_not_supported` errors when it tried to call them.
fn method_capability_gate(method: &str) -> Option<&'static str> {
    match method {
        methods::TASK_LIST | methods::TASK_CANCEL | methods::TASK_RESTART_FROM_NODE => {
            Some(UI_PROTOCOL_FEATURE_HARNESS_TASK_CONTROL_V1)
        }
        methods::TASK_ARTIFACT_LIST | methods::TASK_ARTIFACT_READ => {
            Some(UI_PROTOCOL_FEATURE_HARNESS_TASK_ARTIFACTS_V1)
        }
        methods::SESSION_HYDRATE => Some(UI_PROTOCOL_FEATURE_SESSION_HYDRATE_V1),
        methods::THREAD_GRAPH_GET => Some(UI_PROTOCOL_FEATURE_THREAD_GRAPH_V1),
        methods::TURN_STATE_GET => Some(UI_PROTOCOL_FEATURE_TURN_STATE_GET_V1),
        methods::LAUNCH_RESOLVE => Some(UI_PROTOCOL_FEATURE_SESSION_WORKSPACE_CWD_V1),
        methods::SESSION_LIST
        | methods::SESSION_SNAPSHOT
        | methods::SESSION_MESSAGES_PAGE
        | methods::SESSION_STATUS_GET
        | methods::SESSION_FILES_LIST
        | methods::SESSION_TASKS_LIST
        | methods::SESSION_WORKSPACE_GET
        | methods::SESSION_TITLE_SET
        | methods::SESSION_DELETE
        | methods::SYSTEM_STATUS_GET
        | methods::CONTENT_LIST
        | methods::CONTENT_DELETE
        | methods::CONTENT_BULK_DELETE
        | methods::MEMORY_OVERVIEW
        | methods::MEMORY_ENTITY
        | methods::CRON_LIST
        | methods::CRON_TOGGLE => Some(UI_PROTOCOL_FEATURE_AUXILIARY_REST_TO_WS_V1),
        methods::AGENT_LIST
        | methods::AGENT_STATUS_READ
        | methods::AGENT_OUTPUT_READ
        | methods::AGENT_ARTIFACT_LIST
        | methods::AGENT_ARTIFACT_READ
        | methods::AGENT_INTERRUPT
        | methods::AGENT_CLOSE => Some(UI_PROTOCOL_FEATURE_CODING_AGENT_CONTROL_V1),
        methods::SESSION_GOAL_GET | methods::SESSION_GOAL_SET | methods::SESSION_GOAL_CLEAR => {
            Some(UI_PROTOCOL_FEATURE_CODING_GOAL_RUNTIME_V1)
        }
        methods::LOOP_CREATE
        | methods::LOOP_LIST
        | methods::LOOP_DELETE
        | methods::LOOP_PAUSE
        | methods::LOOP_RESUME
        | methods::LOOP_FIRE_NOW => Some(UI_PROTOCOL_FEATURE_CODING_LOOP_RUNTIME_V1),
        methods::REVIEW_START => Some(UI_PROTOCOL_FEATURE_REVIEW_START_V1),
        methods::USER_QUESTION_RESPOND => Some(UI_PROTOCOL_FEATURE_USER_QUESTION_V1),
        methods::SMART_HOME_STATUS_GET
        | methods::SMART_HOME_DEVICE_LIST
        | methods::SMART_HOME_DEVICE_COMMAND
        | methods::SMART_HOME_CAMERA_STREAM_START
        | methods::SMART_HOME_CAMERA_STREAM_STOP => Some(UI_PROTOCOL_FEATURE_SMART_HOME_V1),
        _ => None,
    }
}

pub mod approval_kinds {
    pub const COMMAND: &str = "command";
    pub const DIFF: &str = "diff";
    pub const FILESYSTEM: &str = "filesystem";
    pub const NETWORK: &str = "network";
    pub const SANDBOX_ESCALATION: &str = "sandbox_escalation";
}

pub mod approval_scopes {
    /// Default — re-prompt every time. Aliases: `approve_once`.
    pub const REQUEST: &str = "request";
    /// Auto-resolve within the same `turn_id` only. Aliases: `approve_for_turn`.
    pub const TURN: &str = "turn";
    /// Auto-resolve within the same `session_id` until session/close.
    /// Aliases: `approve_for_session`.
    pub const SESSION: &str = "session";
    /// Auto-resolve every call to the same `tool_name` until session/close.
    /// Aliases: `approve_for_tool`.
    pub const TOOL: &str = "tool";
}

/// Risk literal returned for tools whose manifest does not declare a risk.
///
/// `unspecified` is intentionally distinct from `low`: the server does not
/// silently downgrade unknown tool risk.
pub const RISK_UNSPECIFIED: &str = "unspecified";

/// Normalize a manifest-declared tool risk.
///
/// Blank or missing risk values resolve to [`RISK_UNSPECIFIED`]. The return
/// value is the server-authoritative value surfaced on approval cards.
pub fn manifest_tool_risk(risk: Option<&str>) -> String {
    risk.map(str::trim)
        .filter(|risk| !risk.is_empty())
        .unwrap_or(RISK_UNSPECIFIED)
        .to_owned()
}

/// Register the server-authoritative approval risk for a tool name.
///
/// Plugin loaders call this when trusted manifests are loaded. Re-registering a
/// tool overwrites the prior risk so a reload with a missing/blank risk cannot
/// leave a stale stronger value behind.
pub fn register_tool_approval_risk(tool_name: impl Into<String>, risk: impl Into<String>) {
    let tool_name = tool_name.into();
    let risk = risk.into();
    tool_approval_risk_registry()
        .write()
        .expect("tool approval risk registry poisoned")
        .insert(tool_name, manifest_tool_risk(Some(&risk)));
}

/// Resolve the server-authoritative approval risk for a tool name.
pub fn tool_approval_risk(tool_name: &str) -> String {
    tool_approval_risk_registry()
        .read()
        .expect("tool approval risk registry poisoned")
        .get(tool_name)
        .cloned()
        .unwrap_or_else(|| RISK_UNSPECIFIED.to_owned())
}

fn tool_approval_risk_registry() -> &'static RwLock<HashMap<String, String>> {
    static REGISTRY: OnceLock<RwLock<HashMap<String, String>>> = OnceLock::new();
    REGISTRY.get_or_init(|| RwLock::new(HashMap::new()))
}

#[doc(hidden)]
pub fn clear_tool_approval_risks_for_test() {
    tool_approval_risk_registry()
        .write()
        .expect("tool approval risk registry poisoned")
        .clear();
}

/// JSON-RPC and Octos-application error codes (spec §10 "Error Model").
///
/// Numeric partition:
/// - `-32700`, `-32600..=-32603`: JSON-RPC reserved codes.
/// - `-32000..=-32099`: JSON-RPC server-error band. Pre-existing
///   `METHOD_NOT_SUPPORTED = -32004` lives here; `APPROVAL_NOT_PENDING =
///   -32011` is the spec-explicit slot in this band.
/// - `-32100..=-32199`: Octos application-level taxonomy. All new typed
///   categories from M9-FIX-02 land here so they never collide with
///   transport-layer codes and are easy to grep.
///
/// Additive only — existing codes are not renamed or repurposed.
pub mod rpc_error_codes {
    // JSON-RPC reserved (spec §10 maps `invalid_request` / `internal_error` here).
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL_ERROR: i64 = -32603;

    /// Server-defined slot for a known method this runtime slice doesn't implement.
    pub const METHOD_NOT_SUPPORTED: i64 = -32004;

    /// Spec §10 `APPROVAL_NOT_PENDING`: `respond` against a non-pending approval.
    /// Spec pins this at `-32011`; recorded decision rides in `error.data`.
    pub const APPROVAL_NOT_PENDING: i64 = -32011;

    /// Spec §10 `unknown_session`: `session_id` not known to the runtime.
    pub const UNKNOWN_SESSION: i64 = -32100;
    /// Spec §10 `unknown_turn`: `turn_id` not known for the addressed session.
    pub const UNKNOWN_TURN: i64 = -32101;
    /// Spec §10 `unknown_approval`: `approval_id` not known to the runtime.
    pub const UNKNOWN_APPROVAL_ID: i64 = -32102;
    /// Spec §10 `unknown_preview`: `preview_id` unknown (expired or never issued).
    pub const UNKNOWN_PREVIEW_ID: i64 = -32103;
    /// Spec §10 `unknown_task`: `task_id` not in the runtime task table.
    pub const UNKNOWN_TASK_ID: i64 = -32104;

    /// Spec §10 `approval_cancelled`: `respond` against an administratively cancelled approval.
    pub const APPROVAL_CANCELLED: i64 = -32105;

    /// UPCR-2026-023 `user_question_unknown`: `user_question/respond` against a
    /// `question_id` not pending for the caller's session. Mirrors
    /// [`UNKNOWN_APPROVAL_ID`] for the structured-question surface.
    pub const USER_QUESTION_UNKNOWN: i64 = -32106;
    /// UPCR-2026-023 `user_question_stale`: `user_question/respond` against a
    /// question that was already answered or cancelled. Mirrors
    /// [`APPROVAL_NOT_PENDING`] / [`APPROVAL_CANCELLED`] for the
    /// structured-question surface.
    pub const USER_QUESTION_STALE: i64 = -32107;
    /// UPCR-2026-023 `user_question_invalid`: `user_question/respond` carried
    /// answers that do not match the STORED request (wrong answer count, a
    /// `selected_labels` value not in that question's options, more than one
    /// label on a non-`multi_select` question, or free text where the question
    /// disallows it). The server rejects the call and does NOT resolve the
    /// blocked tool with bad data. Distinct from `user_question_unknown`
    /// (target not found) and `user_question_stale` (target no longer
    /// pending) so the client can tell "fix your answer and retry" from "this
    /// question is gone".
    pub const USER_QUESTION_INVALID: i64 = -32108;

    /// Spec §10 `cursor_out_of_range`: stale or future cursor relative to ledger.
    pub const CURSOR_OUT_OF_RANGE: i64 = -32110;
    /// Spec §10 cursor variant: cursor malformed or wrong-session. Distinct from
    /// `CURSOR_OUT_OF_RANGE` so clients pick "retry with fresh cursor" vs "rehandshake".
    pub const CURSOR_INVALID: i64 = -32111;

    /// Spec §10 `permission_denied`: sandbox / approval-scope / profile policy refusal.
    pub const PERMISSION_DENIED: i64 = -32120;

    /// Spec §10 / §3 capability-negotiation category. New emitters should prefer
    /// this over the legacy `METHOD_NOT_SUPPORTED` (-32004) slot.
    pub const UNSUPPORTED_CAPABILITY: i64 = -32130;

    /// Spec §10 `runtime_unavailable` / `runtime_not_ready`: transient unavailable.
    pub const RUNTIME_NOT_READY: i64 = -32140;

    /// Result-side counterpart to `INVALID_PARAMS`. Spec §10 separates transport
    /// from runtime errors; `MALFORMED_RESULT` flags server-side schema breakage.
    pub const MALFORMED_RESULT: i64 = -32150;

    /// Spec §10 / M9-FIX-04 backpressure signal; carries `retry_after_ms` in `data`.
    pub const RATE_LIMITED: i64 = -32160;

    /// Generic not-found error for non-session-scoped resources (content
    /// catalog rows, profile records, ...) returned by REST 404. Distinct
    /// from `UNKNOWN_SESSION` (-32100) which is reserved for session-scoped
    /// 404s. M12 Phase D-1 introduced this slot when the REST→WS bridge
    /// began surfacing content/profile 404s as typed errors; before, every
    /// REST 404 was force-mapped to `UNKNOWN_SESSION` regardless of
    /// resource kind.
    pub const RESOURCE_NOT_FOUND: i64 = -32170;
}

/// UPCR-2026-021 autonomy runtime error kind registry.
pub mod autonomy_error_kinds {
    pub const AGENT_NOT_FOUND: &str = "agent_not_found";
    pub const AGENT_CONTROL_FORBIDDEN: &str = "agent_control_forbidden";
    pub const AGENT_CONTROL_UNAVAILABLE: &str = "agent_control_unavailable";
    pub const AGENT_ARTIFACT_DENIED: &str = "agent_artifact_denied";
    pub const GOAL_RUNTIME_UNAVAILABLE: &str = "goal_runtime_unavailable";
    pub const GOAL_UNAVAILABLE: &str = "goal_unavailable";
    pub const GOAL_INVALID_STATE: &str = "goal_invalid_state";
    pub const GOAL_RATE_LIMITED: &str = "goal_rate_limited";
    pub const LOOP_RUNTIME_UNAVAILABLE: &str = "loop_runtime_unavailable";
    pub const LOOP_NOT_FOUND: &str = "loop_not_found";
    pub const LOOP_INVALID_INTERVAL: &str = "loop_invalid_interval";
    pub const LOOP_PROMPT_EMPTY: &str = "loop_prompt_empty";
    pub const LOOP_BUSY: &str = "loop_busy";
    pub const LOOP_SLASH_DENIED: &str = "loop_slash_denied";
    pub const LOOP_POLICY_DENIED: &str = "loop_policy_denied";
    pub const AUTONOMY_QUOTA_EXCEEDED: &str = "autonomy_quota_exceeded";
}

/// Logical event-ledger cursor used for resumable UI notification consumption.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiCursor {
    pub stream: String,
    pub seq: u64,
}

/// Stable identity for one client-visible turn.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TurnId(pub Uuid);

impl TurnId {
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for TurnId {
    fn default() -> Self {
        Self::new()
    }
}

/// Stable identity for an approval request.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ApprovalId(pub Uuid);

impl ApprovalId {
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for ApprovalId {
    fn default() -> Self {
        Self::new()
    }
}

/// Stable identity for a structured user-question request (UPCR-2026-023).
///
/// Mirrors [`ApprovalId`]: a `Uuid` newtype minted server-side per
/// `ask_user_question` tool call. The client cannot forge it — a
/// `user_question/respond` is accepted only for a pending `question_id`
/// on the caller's session.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct QuestionId(pub Uuid);

impl QuestionId {
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for QuestionId {
    fn default() -> Self {
        Self::new()
    }
}

/// Stable identity for one diff preview proposal.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PreviewId(pub Uuid);

impl PreviewId {
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for PreviewId {
    fn default() -> Self {
        Self::new()
    }
}

/// Cursor into task output streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputCursor {
    pub offset: u64,
}

/// Generic JSON-RPC request envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcRequest<T> {
    pub jsonrpc: String,
    pub id: String,
    pub method: String,
    pub params: T,
}

impl<T> RpcRequest<T> {
    pub fn new(id: impl Into<String>, method: impl Into<String>, params: T) -> Self {
        Self {
            jsonrpc: JSON_RPC_VERSION.to_owned(),
            id: id.into(),
            method: method.into(),
            params,
        }
    }

    pub fn is_jsonrpc_v2(&self) -> bool {
        self.jsonrpc == JSON_RPC_VERSION
    }
}

/// Generic JSON-RPC success envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcResponse<T> {
    pub jsonrpc: String,
    pub id: String,
    pub result: T,
}

impl<T> RpcResponse<T> {
    pub fn success(id: impl Into<String>, result: T) -> Self {
        Self {
            jsonrpc: JSON_RPC_VERSION.to_owned(),
            id: id.into(),
            result,
        }
    }

    pub fn is_jsonrpc_v2(&self) -> bool {
        self.jsonrpc == JSON_RPC_VERSION
    }
}

/// Generic JSON-RPC notification envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcNotification<T> {
    pub jsonrpc: String,
    pub method: String,
    pub params: T,
}

impl<T> RpcNotification<T> {
    pub fn new(method: impl Into<String>, params: T) -> Self {
        Self {
            jsonrpc: JSON_RPC_VERSION.to_owned(),
            method: method.into(),
            params,
        }
    }

    pub fn is_jsonrpc_v2(&self) -> bool {
        self.jsonrpc == JSON_RPC_VERSION
    }
}

/// JSON-RPC error object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }

    pub fn parse_error(message: impl Into<String>) -> Self {
        Self::new(rpc_error_codes::PARSE_ERROR, message)
    }

    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(rpc_error_codes::INVALID_REQUEST, message)
    }

    pub fn method_not_found(method: impl AsRef<str>) -> Self {
        Self::new(
            rpc_error_codes::METHOD_NOT_FOUND,
            format!("method not found: {}", method.as_ref()),
        )
    }

    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(rpc_error_codes::INVALID_PARAMS, message)
    }

    pub fn internal_error(message: impl Into<String>) -> Self {
        Self::new(rpc_error_codes::INTERNAL_ERROR, message)
    }

    /// Spec §10 `unknown_session`. Echoes the id in `data.session_id` so
    /// clients can reconcile without re-parsing the message string.
    pub fn unknown_session(session_id: impl Into<String>) -> Self {
        let session_id = session_id.into();
        Self::new(
            rpc_error_codes::UNKNOWN_SESSION,
            format!("unknown session: {session_id}"),
        )
        .with_data(serde_json::json!({
            "kind": "unknown_session",
            "session_id": session_id,
        }))
    }

    /// Spec §10 `unknown_turn`.
    pub fn unknown_turn(turn_id: &TurnId) -> Self {
        let turn_id_str = turn_id.0.to_string();
        Self::new(
            rpc_error_codes::UNKNOWN_TURN,
            format!("unknown turn: {turn_id_str}"),
        )
        .with_data(serde_json::json!({
            "kind": "unknown_turn",
            "turn_id": turn_id_str,
        }))
    }

    /// Spec §10 `unknown_approval`.
    pub fn unknown_approval_id(approval_id: &ApprovalId) -> Self {
        let approval_id_str = approval_id.0.to_string();
        Self::new(
            rpc_error_codes::UNKNOWN_APPROVAL_ID,
            format!("unknown approval id: {approval_id_str}"),
        )
        .with_data(serde_json::json!({
            "kind": "unknown_approval",
            "approval_id": approval_id_str,
        }))
    }

    /// Spec §10 `unknown_preview`.
    pub fn unknown_preview_id(preview_id: &PreviewId) -> Self {
        let preview_id_str = preview_id.0.to_string();
        Self::new(
            rpc_error_codes::UNKNOWN_PREVIEW_ID,
            format!("unknown preview id: {preview_id_str}"),
        )
        .with_data(serde_json::json!({
            "kind": "unknown_preview",
            "preview_id": preview_id_str,
        }))
    }

    /// Spec §10 `unknown_task`.
    pub fn unknown_task_id(task_id: &TaskId) -> Self {
        let task_id_str = task_id.to_string();
        Self::new(
            rpc_error_codes::UNKNOWN_TASK_ID,
            format!("unknown task id: {task_id_str}"),
        )
        .with_data(serde_json::json!({
            "kind": "unknown_task",
            "task_id": task_id_str,
        }))
    }

    /// Spec §10 `cursor_out_of_range`. Echoes both the client cursor and
    /// the ledger head in `data` so clients can pick a new resume point.
    pub fn cursor_out_of_range(cursor: &UiCursor, ledger_head: &UiCursor) -> Self {
        Self::new(
            rpc_error_codes::CURSOR_OUT_OF_RANGE,
            format!(
                "cursor out of range: {}@{} (ledger head {}@{})",
                cursor.stream, cursor.seq, ledger_head.stream, ledger_head.seq,
            ),
        )
        .with_data(serde_json::json!({
            "cursor": cursor,
            "ledger_head": ledger_head,
        }))
    }

    /// Spec §10 cursor variant: cursor is malformed or addresses a
    /// different session than the request.
    pub fn cursor_invalid(message: impl Into<String>) -> Self {
        Self::new(rpc_error_codes::CURSOR_INVALID, message)
    }

    /// Spec §10 `permission_denied`.
    pub fn permission_denied(message: impl Into<String>) -> Self {
        Self::new(rpc_error_codes::PERMISSION_DENIED, message)
    }

    /// Spec §10 `APPROVAL_NOT_PENDING` (`-32011`). Carries the recorded
    /// decision in `data.recorded_decision` (snake-case form).
    pub fn approval_not_pending(decision: ApprovalDecision) -> Self {
        let recorded =
            serde_json::to_value(decision).expect("ApprovalDecision serializes to a JSON string");
        Self::new(
            rpc_error_codes::APPROVAL_NOT_PENDING,
            "approval is no longer pending",
        )
        .with_data(serde_json::json!({ "recorded_decision": recorded }))
    }

    /// Read back the recorded decision attached to an
    /// `APPROVAL_NOT_PENDING` (`-32011`) error, if present and well-formed.
    pub fn recorded_decision(&self) -> Option<ApprovalDecision> {
        if self.code != rpc_error_codes::APPROVAL_NOT_PENDING {
            return None;
        }
        let data = self.data.as_ref()?;
        let recorded = data.get("recorded_decision")?.clone();
        serde_json::from_value(recorded).ok()
    }

    /// Spec §10 capability-mismatch error. Carries a typed
    /// `UnsupportedCapabilityReport` in `data` for uniform client handling.
    pub fn unsupported_capability(method: impl Into<String>, reason: impl Into<String>) -> Self {
        let report = UnsupportedCapabilityReport::method(method, reason);
        Self::new(
            rpc_error_codes::UNSUPPORTED_CAPABILITY,
            format!("unsupported capability: {}", report.method),
        )
        .with_data(report.to_error_data())
    }

    /// Spec §10 `runtime_unavailable` / `runtime_not_ready`.
    pub fn runtime_not_ready(message: impl Into<String>) -> Self {
        Self::new(rpc_error_codes::RUNTIME_NOT_READY, message)
    }

    /// Generic REST 404 for non-session-scoped resources (content catalog
    /// rows, profile records, ...). Distinct from
    /// [`Self::unknown_session`] which carries `session_id` in `data` for
    /// session-scoped misses. M12 Phase D-1 added this so the REST→WS
    /// bridge can surface content/profile 404s without misclassifying
    /// them as session misses.
    ///
    /// `resource_type` is a short tag identifying the resource ("content",
    /// "profile", ...); `identifier` is the resource id the client sent.
    /// Both are echoed in `data` so clients can reconcile without parsing
    /// the message string.
    pub fn not_found(resource_type: impl Into<String>, identifier: impl Into<String>) -> Self {
        let resource_type = resource_type.into();
        let identifier = identifier.into();
        Self::new(
            rpc_error_codes::RESOURCE_NOT_FOUND,
            format!("{resource_type} not found: {identifier}"),
        )
        .with_data(serde_json::json!({
            "kind": "not_found",
            "resource_type": resource_type,
            "identifier": identifier,
        }))
    }

    /// Result-side counterpart to `INVALID_PARAMS`. See
    /// [`rpc_error_codes::MALFORMED_RESULT`] for rationale.
    pub fn malformed_result(message: impl Into<String>) -> Self {
        Self::new(rpc_error_codes::MALFORMED_RESULT, message)
    }

    /// Spec §10 / M9-FIX-04 backpressure signal. Optional `retry_after_ms`
    /// hint is attached to `data` when supplied.
    pub fn rate_limited(message: impl Into<String>, retry_after_ms: Option<u64>) -> Self {
        let mut err = Self::new(rpc_error_codes::RATE_LIMITED, message);
        if let Some(retry_after_ms) = retry_after_ms {
            err = err.with_data(serde_json::json!({ "retry_after_ms": retry_after_ms }));
        }
        err
    }
}

/// JSON-RPC error response envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcErrorResponse {
    pub jsonrpc: String,
    pub id: Option<String>,
    pub error: RpcError,
}

impl RpcErrorResponse {
    pub fn new(id: Option<String>, error: RpcError) -> Self {
        Self {
            jsonrpc: JSON_RPC_VERSION.to_owned(),
            id,
            error,
        }
    }

    pub fn for_request<T>(request: &RpcRequest<T>, error: RpcError) -> Self {
        Self::new(Some(request.id.clone()), error)
    }

    pub fn is_jsonrpc_v2(&self) -> bool {
        self.jsonrpc == JSON_RPC_VERSION
    }
}

fn validate_jsonrpc_version(jsonrpc: &str) -> Result<(), RpcError> {
    if jsonrpc == JSON_RPC_VERSION {
        Ok(())
    } else {
        Err(RpcError::invalid_request(format!(
            "unsupported JSON-RPC version: {jsonrpc}"
        )))
    }
}

fn decode_params<T>(method: &str, params: Value) -> Result<T, RpcError>
where
    T: DeserializeOwned,
{
    serde_json::from_value(params)
        .map_err(|err| RpcError::invalid_params(format!("invalid params for {method}: {err}")))
}

fn decode_result<T>(method: &str, result: Value) -> Result<T, RpcError>
where
    T: DeserializeOwned,
{
    // Spec §10: `INVALID_PARAMS` (-32602) is the JSON-RPC code for malformed
    // *params*. A malformed *result* is a server-side schema violation and
    // gets `MALFORMED_RESULT` (-32150) so clients can distinguish the two.
    serde_json::from_value(result)
        .map_err(|err| RpcError::malformed_result(format!("invalid result for {method}: {err}")))
}

pub mod methods {
    pub const CONFIG_CAPABILITIES_LIST: &str = "config/capabilities/list";
    pub const SESSION_STATUS_READ: &str = "session/status/read";
    pub const PROFILE_LOCAL_CREATE: &str = "profile/local/create";
    pub const SESSION_OPEN: &str = "session/open";
    pub const TURN_START: &str = "turn/start";
    pub const TURN_INTERRUPT: &str = "turn/interrupt";
    pub const APPROVAL_RESPOND: &str = "approval/respond";
    pub const APPROVAL_SCOPES_LIST: &str = "approval/scopes/list";
    /// UPCR-2026-023 `user_question/respond` — answer a structured
    /// `user_question/requested` event. Gated by `user_question.v1`.
    pub const USER_QUESTION_RESPOND: &str = "user_question/respond";
    pub const PERMISSION_PROFILE_LIST: &str = "permission/profile/list";
    pub const PERMISSION_PROFILE_SET: &str = "permission/profile/set";
    pub const DIFF_PREVIEW_GET: &str = "diff/preview/get";
    pub const TASK_LIST: &str = "task/list";
    pub const TASK_CANCEL: &str = "task/cancel";
    pub const TASK_RESTART_FROM_NODE: &str = "task/restart_from_node";
    pub const TASK_OUTPUT_READ: &str = "task/output/read";

    /// UPCR-2026-009 `session/hydrate` — authoritative chat-state reload.
    pub const SESSION_HYDRATE: &str = "session/hydrate";
    /// `session/rollback` — drop the last N user turns (conversation-only
    /// rewind), persist an idempotent append-only marker, and return the
    /// trimmed hydrated thread. MUTATING: changes persisted session state.
    pub const SESSION_ROLLBACK: &str = "session/rollback";
    /// `session/fork` — create a NEW session copying the tail of an
    /// existing one (`SessionManager::fork`; parent tracked via
    /// `parent_key`). MUTATING: writes the child session to disk.
    pub const SESSION_FORK: &str = "session/fork";
    /// UPCR-2026-010 `thread/graph/get` — thread partition for the session.
    pub const THREAD_GRAPH_GET: &str = "thread/graph/get";
    /// UPCR-2026-011 `turn/state/get` — turn lifecycle introspection.
    pub const TURN_STATE_GET: &str = "turn/state/get";
    /// `session/btw` — quick aside question answered out-of-band (no tools)
    /// while the session's live turn, if any, keeps running.
    pub const SESSION_BTW: &str = "session/btw";

    /// UPCR-2026-021 M15 agent inspection/control surface.
    pub const AGENT_LIST: &str = "agent/list";
    pub const AGENT_STATUS_READ: &str = "agent/status/read";
    pub const AGENT_OUTPUT_READ: &str = "agent/output/read";
    pub const AGENT_ARTIFACT_LIST: &str = "agent/artifact/list";
    pub const AGENT_ARTIFACT_READ: &str = "agent/artifact/read";
    /// #965 / UPCR-2026-019 — spec-canonical names for the same payloads
    /// served by `agent/artifact/*`. Servers dispatch both into the same
    /// handlers; clients can use either name (the `task/*` form is the
    /// long-term direction per the M13 contract).
    pub const TASK_ARTIFACT_LIST: &str = "task/artifact/list";
    pub const TASK_ARTIFACT_READ: &str = "task/artifact/read";
    pub const AGENT_INTERRUPT: &str = "agent/interrupt";
    pub const AGENT_CLOSE: &str = "agent/close";

    /// UPCR-2026-021 M15 persisted goal runtime surface.
    pub const SESSION_GOAL_GET: &str = "session/goal/get";
    pub const SESSION_GOAL_SET: &str = "session/goal/set";
    pub const SESSION_GOAL_CLEAR: &str = "session/goal/clear";

    /// UPCR-2026-021 M15 recurring loop runtime surface.
    pub const LOOP_CREATE: &str = "loop/create";
    pub const LOOP_LIST: &str = "loop/list";
    pub const LOOP_DELETE: &str = "loop/delete";
    pub const LOOP_PAUSE: &str = "loop/pause";
    pub const LOOP_RESUME: &str = "loop/resume";
    pub const LOOP_FIRE_NOW: &str = "loop/fire_now";

    /// Product-level automated code review workflow.
    ///
    /// This is not a generic child-agent control API. The backend owns the
    /// review template and decides which specialist agents to launch.
    pub const REVIEW_START: &str = "review/start";

    pub const TURN_STARTED: &str = "turn/started";
    pub const TURN_COMPLETED: &str = "turn/completed";
    pub const TURN_ERROR: &str = "turn/error";
    pub const MESSAGE_DELTA: &str = "message/delta";
    pub const MESSAGE_REASONING_DELTA: &str = "message/reasoning_delta";
    pub const TOOL_STARTED: &str = "tool/started";
    pub const TOOL_PROGRESS: &str = "tool/progress";
    pub const TOOL_COMPLETED: &str = "tool/completed";
    pub const APPROVAL_REQUESTED: &str = "approval/requested";
    pub const APPROVAL_AUTO_RESOLVED: &str = "approval/auto_resolved";
    pub const APPROVAL_DECIDED: &str = "approval/decided";
    pub const APPROVAL_CANCELLED: &str = "approval/cancelled";
    /// UPCR-2026-023 `user_question/requested` — structured multiple-choice
    /// question the agent is asking the user mid-turn. While unresolved the
    /// turn stays paused at the blocking-tool boundary (same boundary as
    /// `approval/requested`). Gated by `user_question.v1`.
    pub const USER_QUESTION_REQUESTED: &str = "user_question/requested";
    pub const TASK_UPDATED: &str = "task/updated";
    /// Model-authored plan/todo checklist snapshot (the `update_plan` tool).
    /// Gated by `plan.todos.v1`. Replaces any prior plan wholesale.
    pub const PLAN_UPDATED: &str = "plan/updated";
    pub const TASK_OUTPUT_DELTA: &str = "task/output/delta";
    pub const PROGRESS_UPDATED: &str = "progress/updated";
    pub const WARNING: &str = "warning";
    /// Notifies the client that one or more durable notifications were dropped due
    /// to per-connection backpressure. The client should diverge the cursor and
    /// rehydrate via `session/open` (or REST). Carries the last known durable
    /// cursor so the client can resume cleanly.
    pub const REPLAY_LOSSY: &str = "protocol/replay_lossy";
    /// M10 Phase 1 `turn/spawn_complete` — completion-as-new-envelope event
    /// for `spawn_only` background tool results. Carries the late assistant
    /// `content` + `media` plus the originating user prompt's
    /// `client_message_id` (`response_to_client_message_id`) so the client
    /// can render the result as a NEW assistant bubble under the correct
    /// user prompt — without splice-merging into the existing
    /// spawn-acknowledgement bubble. Gated by
    /// [`UI_PROTOCOL_FEATURE_SPAWN_COMPLETE_V1`].
    pub const TURN_SPAWN_COMPLETE: &str = "turn/spawn_complete";
    /// UPCR-2026-014 (M9-α-9) `file/attached` — per-turn file attachment
    /// event mirroring the SSE `file:` frame from `files_to_send` tool
    /// surfaces.
    pub const FILE_ATTACHED: &str = "file/attached";
    /// #1477 voice rich output — a background visual artifact (illustrated
    /// HTML / image / infographic) began generating for the turn. Lets the
    /// client show a "generating" placeholder WITHOUT scraping an in-band
    /// marker out of the assistant text. Ungated; emitted on the same
    /// ledger-backed live path as `file/attached` (durable append, so a
    /// reconnecting client replays it). The lifecycle is terminated by a typed
    /// `visual/succeeded` or `visual/failed` — NOT by `file/attached` (which is
    /// purely an artifact-delivery signal).
    pub const VISUAL_GENERATING: &str = "visual/generating";
    /// #1477 voice rich output — the background visual task produced its
    /// artifact(s). The structured success counterpart of `visual/generating`:
    /// the client clears the "generating" placeholder off THIS event, keeping
    /// the visual lifecycle decoupled from `file/attached`. Emitted alongside
    /// `file/attached` on the success branch.
    pub const VISUAL_SUCCEEDED: &str = "visual/succeeded";
    /// #1477 voice rich output — the background visual task failed or timed
    /// out, so the client should clear the "generating" placeholder.
    pub const VISUAL_FAILED: &str = "visual/failed";
    /// UPCR-2026-025 voice exit intent — the voice turn detected an end /
    /// goodbye / mute intent (the model appended an in-band `[[EXIT]]` control
    /// marker, which the backend strips from every model-/client-facing surface
    /// and replaces with this typed event). The client uses it to leave the
    /// `/voice` screen and return home AFTER the turn's farewell audio finishes
    /// playing — it must NOT navigate before the reply audio drains. Ungated;
    /// emitted on the same ledger-backed live path as `file/attached`.
    pub const VOICE_EXIT: &str = "voice/exit";
    /// Streamed voice-reply audio chunk (gated by `event.voice_audio.v1`).
    /// One per audio frame from cloud TTS; carries base64 audio plus a
    /// `segment_id`/`seq`/`last` so the client groups and plays chunks in order.
    pub const VOICE_AUDIO_CHUNK: &str = "voice/audio_chunk";
    /// UPCR-2026-014 (M9-γ) `projection/envelope` — canonical projection
    /// envelope notification (spec § 14). γ-1 reserves the method name
    /// in the notification methods list as part of capability negotiation
    /// wire-up; γ-2 (follow-up) will gate emission on the
    /// `projection.envelope.v1` feature and delete the legacy
    /// `message/delta`, `tool/*`, and
    /// `turn/completed` notifications it supersedes.
    pub const PROJECTION_ENVELOPE: &str = "projection/envelope";
    /// UPCR-2026-014 (M9-α-9) `session/event` — wrapper envelope for
    /// legacy `/api/sessions/:id/events/stream` SSE frames bridged onto
    /// the unified v1 surface.
    pub const SESSION_EVENT: &str = "session/event";

    // ---- M12 Phase D-1 auxiliary REST → WS surface ----
    // Each method below replaces a REST endpoint listed in the ADR's
    // inventory table (docs/adr/m12-phase-d-auxiliary-rest-to-ws.md).
    // All thirteen are capability-gated on
    // `UI_PROTOCOL_FEATURE_AUXILIARY_REST_TO_WS_V1`
    // (`content/delete` and `content/bulk_delete` are distinct methods
    // sharing the `content/*` namespace).

    /// Replaces `GET /api/sessions` — sidebar session list.
    pub const SESSION_LIST: &str = "session/list";
    /// Replaces combined `GET /api/sessions/{id}/status` + `/files` +
    /// `/tasks` — single right-rail bootstrap fetch.
    pub const SESSION_SNAPSHOT: &str = "session/snapshot";
    /// Replaces `GET /api/sessions/{id}/messages` — paginated history.
    pub const SESSION_MESSAGES_PAGE: &str = "session/messages_page";
    /// Replaces `GET /api/sessions/{id}/status` — status-pill poller.
    pub const SESSION_STATUS_GET: &str = "session/status.get";
    /// Replaces `GET /api/sessions/{id}/files` — files panel listing.
    pub const SESSION_FILES_LIST: &str = "session/files.list";
    /// Replaces `GET /api/sessions/{id}/tasks` — background tasks panel.
    pub const SESSION_TASKS_LIST: &str = "session/tasks.list";
    /// Replaces `GET /api/sessions/{id}/workspace-contract` — workspace
    /// contract panel.
    pub const SESSION_WORKSPACE_GET: &str = "session/workspace.get";
    /// Replaces `PATCH /api/sessions/{id}/title` — manual title setter.
    pub const SESSION_TITLE_SET: &str = "session/title.set";
    /// Replaces `DELETE /api/sessions/{id}` — session deletion.
    pub const SESSION_DELETE: &str = "session/delete";
    /// Replaces `GET /api/status` — agent/server status (distinct from
    /// `/api/auth/status` which stays REST).
    pub const SYSTEM_STATUS_GET: &str = "system/status.get";
    /// Replaces `GET /api/my/content` — content gallery listing.
    pub const CONTENT_LIST: &str = "content/list";
    /// Replaces `DELETE /api/my/content/{id}` — single-content deletion.
    pub const CONTENT_DELETE: &str = "content/delete";
    /// Replaces `POST /api/my/content/bulk-delete` — bulk-content deletion.
    pub const CONTENT_BULK_DELETE: &str = "content/bulk_delete";
    /// Replaces `GET /api/my/memory` — memory panel overview (long-term
    /// memory, daily notes, entity bank summaries, staging count).
    pub const MEMORY_OVERVIEW: &str = "memory/overview";
    /// Replaces `GET /api/my/memory/entities/{name}` — full entity page.
    pub const MEMORY_ENTITY: &str = "memory/entity";
    /// Replaces `GET /api/my/cron` — cron panel job listing.
    pub const CRON_LIST: &str = "cron/list";
    /// Replaces `PUT /api/my/cron/{job_id}/enabled` — cron job toggle.
    pub const CRON_TOGGLE: &str = "cron/toggle";

    /// Pre-session launch probe. Given the project cwd + optional requested
    /// profile, the server decides whether to resume the folder's
    /// conversation, activate a new one, or surface a cross-profile choice.
    /// Capability-gated on
    /// [`UI_PROTOCOL_FEATURE_SESSION_WORKSPACE_CWD_V1`].
    pub const LAUNCH_RESOLVE: &str = "launch/resolve";

    // ---- Wave4-A: adaptive routing + queue state ----

    /// Wave4-A `router/status` — adaptive routing snapshot notification.
    /// Emitted by the server adjacent to `turn/started` and `turn/completed`
    /// so the client always has a fresh status without polling.
    pub const ROUTER_STATUS: &str = "router/status";
    /// Wave4-A `router/failover` — adaptive router crossed lanes.
    pub const ROUTER_FAILOVER: &str = "router/failover";
    /// Wave4-A `router/set_mode` — runtime mode toggle command. Mode
    /// change is session-scoped, not process-global.
    pub const ROUTER_SET_MODE: &str = "router/set_mode";
    /// Wave4-A `router/get_metrics` — on-demand snapshot of the
    /// `AdaptiveStatus` plus full lane scores / breaker map. Returns
    /// the same payload shape as the `router/status` notification but
    /// as an RPC result.
    pub const ROUTER_GET_METRICS: &str = "router/get_metrics";
    /// Wave4-A `queue/state` — pending-queue snapshot. Client-emitted
    /// today; the constant is defined so type-checked code paths across
    /// the workspace can reference one source of truth.
    pub const QUEUE_STATE: &str = "queue/state";

    /// UPCR-2026-021 M15 agent lifecycle/output notifications.
    pub const AGENT_UPDATED: &str = "agent/updated";
    pub const AGENT_OUTPUT_DELTA: &str = "agent/output/delta";
    pub const AGENT_ARTIFACT_UPDATED: &str = "agent/artifact/updated";
    /// UPCR-2026-021 M15 goal runtime notifications.
    pub const SESSION_GOAL_UPDATED: &str = "session/goal/updated";
    pub const SESSION_GOAL_CLEARED: &str = "session/goal/cleared";
    /// UPCR-2026-021 M15 loop runtime notifications.
    pub const LOOP_UPDATED: &str = "loop/updated";
    pub const LOOP_FIRED: &str = "loop/fired";
    pub const LOOP_COMPLETED: &str = "loop/completed";
    /// M16 `context.lifecycle.v1`: compact-context lifecycle notification.
    pub const CONTEXT_COMPACTION_COMPLETED: &str = "context/compaction_completed";
    pub const CONTEXT_COMPACTION_STARTED: &str = "context/compaction_started";
    /// M16 `context.lifecycle.v1`: prompt normalization report notification.
    pub const CONTEXT_NORMALIZATION_REPORTED: &str = "context/normalization_reported";
    /// Session-level whole-job orchestration status notification.
    pub const SESSION_ORCHESTRATION: &str = "session/orchestration";
    /// #1801 v3 `peer/staged` — agent-initiated peer staging. The model's
    /// `peer_handoff` tool staged a sovereign peer session server-side
    /// (durable brief + optional fenced worktree); sessions are
    /// client-connection-coupled, so this durable notification asks the
    /// user's client to OPEN the staged session (topic `peer-<slug>`) in
    /// the background. `session_id` is the ORIGINATING session; replayed on
    /// reconnect, so clients dedup by existing session.
    pub const PEER_STAGED: &str = "peer/staged";
    /// `peer/closed` — the model's `peer_close` tool tore down a staged peer
    /// session (durable brief + optional fenced worktree evicted). Mirrors
    /// [`PEER_STAGED`]: `session_id` is the ORIGINATING session; durable so
    /// reconnect replay redelivers it, and clients dedup by the closed peer.
    pub const PEER_CLOSED: &str = "peer/closed";

    // ---- Smart-home bridge integration ----
    // Device control/state moved server-side from octos-web's client-only
    // widget so bridge credentials never reach the browser. Camera video
    // stays a direct browser-to-bridge stream; these methods only return
    // the playback URL. All five are capability-gated on
    // `UI_PROTOCOL_FEATURE_SMART_HOME_V1`.

    /// Bridge configuration/reachability status for the current profile.
    pub const SMART_HOME_STATUS_GET: &str = "smart_home/status.get";
    /// Device list + state, proxied from the configured bridge.
    pub const SMART_HOME_DEVICE_LIST: &str = "smart_home/device.list";
    /// Send a device command (on/off, temperature, mode, action, ...).
    pub const SMART_HOME_DEVICE_COMMAND: &str = "smart_home/device.command";
    /// Start a camera stream; returns the bridge's playback URL.
    pub const SMART_HOME_CAMERA_STREAM_START: &str = "smart_home/camera.stream_start";
    /// Stop a camera stream.
    pub const SMART_HOME_CAMERA_STREAM_STOP: &str = "smart_home/camera.stream_stop";
}

/// Reason codes for `approval/cancelled` notifications. The registry is
/// open: clients should treat unknown reasons as an opaque string and may
/// add new entries as future drains land (e.g. `session_closed`).
pub mod approval_cancelled_reasons {
    pub const TURN_INTERRUPTED: &str = "turn_interrupted";
}

/// All command methods defined by the v1alpha1 protocol model.
pub const UI_PROTOCOL_COMMAND_METHODS: &[&str] = &[
    methods::PROFILE_LOCAL_CREATE,
    methods::SESSION_OPEN,
    methods::TURN_START,
    methods::TURN_INTERRUPT,
    methods::APPROVAL_RESPOND,
    methods::APPROVAL_SCOPES_LIST,
    methods::SESSION_BTW,
    methods::USER_QUESTION_RESPOND,
    methods::PERMISSION_PROFILE_LIST,
    methods::PERMISSION_PROFILE_SET,
    methods::DIFF_PREVIEW_GET,
    methods::TASK_LIST,
    methods::TASK_CANCEL,
    methods::TASK_RESTART_FROM_NODE,
    methods::TASK_OUTPUT_READ,
    methods::SESSION_HYDRATE,
    methods::SESSION_ROLLBACK,
    methods::SESSION_FORK,
    methods::THREAD_GRAPH_GET,
    methods::TURN_STATE_GET,
    methods::AGENT_LIST,
    methods::AGENT_STATUS_READ,
    methods::AGENT_OUTPUT_READ,
    methods::AGENT_ARTIFACT_LIST,
    methods::AGENT_ARTIFACT_READ,
    methods::TASK_ARTIFACT_LIST,
    methods::TASK_ARTIFACT_READ,
    methods::AGENT_INTERRUPT,
    methods::AGENT_CLOSE,
    methods::SESSION_GOAL_GET,
    methods::SESSION_GOAL_SET,
    methods::SESSION_GOAL_CLEAR,
    methods::LOOP_CREATE,
    methods::LOOP_LIST,
    methods::LOOP_DELETE,
    methods::LOOP_PAUSE,
    methods::LOOP_RESUME,
    methods::LOOP_FIRE_NOW,
    methods::REVIEW_START,
    methods::SESSION_LIST,
    methods::SESSION_SNAPSHOT,
    methods::SESSION_MESSAGES_PAGE,
    methods::SESSION_STATUS_GET,
    methods::SESSION_FILES_LIST,
    methods::SESSION_TASKS_LIST,
    methods::SESSION_WORKSPACE_GET,
    methods::SESSION_TITLE_SET,
    methods::SESSION_DELETE,
    methods::SYSTEM_STATUS_GET,
    methods::CONTENT_LIST,
    methods::CONTENT_DELETE,
    methods::CONTENT_BULK_DELETE,
    methods::MEMORY_OVERVIEW,
    methods::MEMORY_ENTITY,
    methods::CRON_LIST,
    methods::CRON_TOGGLE,
    methods::ROUTER_SET_MODE,
    methods::ROUTER_GET_METRICS,
    methods::LAUNCH_RESOLVE,
    methods::SMART_HOME_STATUS_GET,
    methods::SMART_HOME_DEVICE_LIST,
    methods::SMART_HOME_DEVICE_COMMAND,
    methods::SMART_HOME_CAMERA_STREAM_START,
    methods::SMART_HOME_CAMERA_STREAM_STOP,
];

/// Notification methods defined by the v1alpha1 protocol model.
pub const UI_PROTOCOL_NOTIFICATION_METHODS: &[&str] = &[
    methods::SESSION_OPEN,
    methods::TURN_STARTED,
    methods::TURN_COMPLETED,
    methods::TURN_ERROR,
    methods::MESSAGE_DELTA,
    methods::MESSAGE_REASONING_DELTA,
    methods::TOOL_STARTED,
    methods::TOOL_PROGRESS,
    methods::TOOL_COMPLETED,
    methods::APPROVAL_REQUESTED,
    methods::APPROVAL_AUTO_RESOLVED,
    methods::APPROVAL_DECIDED,
    methods::APPROVAL_CANCELLED,
    methods::USER_QUESTION_REQUESTED,
    methods::TASK_UPDATED,
    methods::PLAN_UPDATED,
    methods::TASK_OUTPUT_DELTA,
    methods::PROGRESS_UPDATED,
    methods::WARNING,
    methods::REPLAY_LOSSY,
    methods::TURN_SPAWN_COMPLETE,
    methods::FILE_ATTACHED,
    methods::VISUAL_GENERATING,
    methods::VISUAL_SUCCEEDED,
    methods::VISUAL_FAILED,
    methods::VOICE_EXIT,
    methods::VOICE_AUDIO_CHUNK,
    methods::PROJECTION_ENVELOPE,
    methods::SESSION_EVENT,
    methods::ROUTER_STATUS,
    methods::ROUTER_FAILOVER,
    methods::QUEUE_STATE,
    methods::AGENT_UPDATED,
    methods::AGENT_OUTPUT_DELTA,
    methods::AGENT_ARTIFACT_UPDATED,
    methods::SESSION_GOAL_UPDATED,
    methods::SESSION_GOAL_CLEARED,
    methods::LOOP_UPDATED,
    methods::LOOP_FIRED,
    methods::LOOP_COMPLETED,
    methods::CONTEXT_COMPACTION_COMPLETED,
    methods::CONTEXT_COMPACTION_STARTED,
    methods::CONTEXT_NORMALIZATION_REPORTED,
    methods::PEER_STAGED,
    methods::PEER_CLOSED,
];

/// Request methods currently handled by the first server/runtime slice.
pub const UI_PROTOCOL_FIRST_SERVER_METHODS: &[&str] = &[
    methods::SESSION_OPEN,
    methods::TURN_START,
    methods::TURN_INTERRUPT,
    methods::APPROVAL_RESPOND,
    methods::APPROVAL_SCOPES_LIST,
    methods::SESSION_BTW,
    methods::USER_QUESTION_RESPOND,
    methods::PERMISSION_PROFILE_LIST,
    methods::PERMISSION_PROFILE_SET,
    methods::DIFF_PREVIEW_GET,
    methods::TASK_LIST,
    methods::TASK_CANCEL,
    methods::TASK_RESTART_FROM_NODE,
    methods::TASK_OUTPUT_READ,
    methods::SESSION_HYDRATE,
    methods::SESSION_ROLLBACK,
    methods::SESSION_FORK,
    methods::THREAD_GRAPH_GET,
    methods::TURN_STATE_GET,
    methods::AGENT_LIST,
    methods::AGENT_STATUS_READ,
    methods::AGENT_OUTPUT_READ,
    methods::AGENT_ARTIFACT_LIST,
    methods::AGENT_ARTIFACT_READ,
    methods::TASK_ARTIFACT_LIST,
    methods::TASK_ARTIFACT_READ,
    methods::AGENT_INTERRUPT,
    methods::AGENT_CLOSE,
    methods::SESSION_GOAL_GET,
    methods::SESSION_GOAL_SET,
    methods::SESSION_GOAL_CLEAR,
    methods::LOOP_CREATE,
    methods::LOOP_LIST,
    methods::LOOP_DELETE,
    methods::LOOP_PAUSE,
    methods::LOOP_RESUME,
    methods::LOOP_FIRE_NOW,
    methods::REVIEW_START,
    methods::SESSION_LIST,
    methods::SESSION_SNAPSHOT,
    methods::SESSION_MESSAGES_PAGE,
    methods::SESSION_STATUS_GET,
    methods::SESSION_FILES_LIST,
    methods::SESSION_TASKS_LIST,
    methods::SESSION_WORKSPACE_GET,
    methods::SESSION_TITLE_SET,
    methods::SESSION_DELETE,
    methods::SYSTEM_STATUS_GET,
    methods::CONTENT_LIST,
    methods::CONTENT_DELETE,
    methods::CONTENT_BULK_DELETE,
    methods::MEMORY_OVERVIEW,
    methods::MEMORY_ENTITY,
    methods::CRON_LIST,
    methods::CRON_TOGGLE,
    methods::ROUTER_SET_MODE,
    methods::ROUTER_GET_METRICS,
    methods::LAUNCH_RESOLVE,
    methods::SMART_HOME_STATUS_GET,
    methods::SMART_HOME_DEVICE_LIST,
    methods::SMART_HOME_DEVICE_COMMAND,
    methods::SMART_HOME_CAMERA_STREAM_START,
    methods::SMART_HOME_CAMERA_STREAM_STOP,
];

/// Protocol methods known but not implemented by the first server/runtime slice.
pub const UI_PROTOCOL_FIRST_SERVER_UNSUPPORTED_METHODS: &[&str] = &[];

/// Version metadata clients can use during handshake or compatibility checks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiProtocolVersion {
    pub protocol: String,
    pub schema_version: u32,
    pub jsonrpc: String,
}

impl UiProtocolVersion {
    pub fn current() -> Self {
        Self {
            protocol: UI_PROTOCOL_V1.to_owned(),
            schema_version: UI_PROTOCOL_SCHEMA_VERSION,
            jsonrpc: JSON_RPC_VERSION.to_owned(),
        }
    }

    pub fn is_supported_by_current_runtime(&self) -> bool {
        self.protocol == UI_PROTOCOL_V1
            && self.schema_version <= UI_PROTOCOL_SCHEMA_VERSION
            && self.jsonrpc == JSON_RPC_VERSION
    }
}

/// Capability payload suitable for a client/server handshake.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiProtocolCapabilities {
    pub version: UiProtocolVersion,
    pub capabilities_schema_version: u32,
    pub supported_methods: Vec<String>,
    pub supported_notifications: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supported_features: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unsupported: Vec<UnsupportedCapabilityReport>,
}

impl UiProtocolCapabilities {
    pub fn new(supported_methods: &[&str], supported_notifications: &[&str]) -> Self {
        Self {
            version: UiProtocolVersion::current(),
            capabilities_schema_version: UI_PROTOCOL_CAPABILITIES_SCHEMA_VERSION,
            supported_methods: string_list(supported_methods),
            supported_notifications: string_list(supported_notifications),
            supported_features: Vec::new(),
            unsupported: Vec::new(),
        }
    }

    pub fn full_protocol() -> Self {
        Self::new(
            UI_PROTOCOL_COMMAND_METHODS,
            UI_PROTOCOL_NOTIFICATION_METHODS,
        )
        .with_supported_features([
            UI_PROTOCOL_FEATURE_APPROVAL_TYPED_V1,
            UI_PROTOCOL_FEATURE_PANE_SNAPSHOTS_V1,
            UI_PROTOCOL_FEATURE_SESSION_WORKSPACE_CWD_V1,
            UI_PROTOCOL_FEATURE_SESSION_SANDBOX_V1,
            UI_PROTOCOL_FEATURE_HARNESS_TASK_CONTROL_V1,
            UI_PROTOCOL_FEATURE_HARNESS_TASK_ARTIFACTS_V1,
            UI_PROTOCOL_FEATURE_SESSION_HYDRATE_V1,
            UI_PROTOCOL_FEATURE_THREAD_GRAPH_V1,
            UI_PROTOCOL_FEATURE_TURN_STATE_GET_V1,
            UI_PROTOCOL_FEATURE_SPAWN_COMPLETE_V1,
            UI_PROTOCOL_FEATURE_AUXILIARY_REST_TO_WS_V1,
            UI_PROTOCOL_FEATURE_CODING_AUTONOMY_V1,
            UI_PROTOCOL_FEATURE_CODING_AGENT_CONTROL_V1,
            UI_PROTOCOL_FEATURE_CODING_GOAL_RUNTIME_V1,
            UI_PROTOCOL_FEATURE_CODING_LOOP_RUNTIME_V1,
            UI_PROTOCOL_FEATURE_REVIEW_START_V1,
            UI_PROTOCOL_FEATURE_CONTEXT_LIFECYCLE_V1,
            UI_PROTOCOL_FEATURE_USER_QUESTION_V1,
            UI_PROTOCOL_FEATURE_PLAN_TODOS_V1,
            UI_PROTOCOL_FEATURE_SMART_HOME_V1,
        ])
    }

    pub fn first_server_slice() -> Self {
        let mut capabilities = Self::new(
            UI_PROTOCOL_FIRST_SERVER_METHODS,
            UI_PROTOCOL_NOTIFICATION_METHODS,
        )
        // `first_server_slice` is the no-header compatibility baseline. V2
        // is known to the server (and is advertised after explicit
        // negotiation) but must not appear here: doing so would alter the
        // byte-level `session/open` response for legacy clients that never
        // requested it.
        .with_supported_features(
            UI_PROTOCOL_KNOWN_FEATURES
                .iter()
                .copied()
                .filter(|feature| *feature != UI_PROTOCOL_FEATURE_PROJECTION_ENVELOPE_V2),
        );
        capabilities.unsupported = UI_PROTOCOL_FIRST_SERVER_UNSUPPORTED_METHODS
            .iter()
            .map(|method| {
                UnsupportedCapabilityReport::method(
                    *method,
                    "not implemented by the first server runtime slice",
                )
            })
            .collect();
        capabilities
    }

    /// Build a server-side capabilities payload reflecting the negotiated
    /// feature set per spec § 4 (UPCR-2026-007). `supported_features` is the
    /// intersection of `requested` (typically from `X-Octos-Ui-Features`)
    /// with the server's known feature registry, preserving the order of
    /// the registry. Unknown feature names in `requested` are dropped — the
    /// server does not advertise capabilities it cannot honour.
    ///
    /// Method-level capability gates honour the same intersection: methods
    /// that spec § 7 marks as capability-gated (e.g. `task/list`,
    /// `task/cancel`, `task/restart_from_node` behind
    /// `harness.task_control.v1`) appear in `supported_methods` only when
    /// the gating feature is in the negotiated set. The spec contract is
    /// "servers expose it only when the feature flag is advertised", so
    /// the advertised method set must agree with the advertised feature
    /// set — otherwise a client that did not negotiate `harness.task_control.v1`
    /// would still see the methods in the response and make calls the
    /// server would then reject with `method_not_supported`.
    pub fn for_negotiated_features<I, S>(requested: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let requested: Vec<String> = requested
            .into_iter()
            .map(|feature| feature.as_ref().to_owned())
            .collect();
        let autonomy_base_requested = requested
            .iter()
            .any(|feature| feature == UI_PROTOCOL_FEATURE_CODING_AUTONOMY_V1);
        let supported_features: Vec<String> = UI_PROTOCOL_KNOWN_FEATURES
            .iter()
            .filter(|feature| {
                requested.iter().any(|requested| requested == **feature)
                    && (autonomy_base_requested || !is_autonomy_optional_feature(feature))
            })
            .map(|feature| (*feature).to_owned())
            .collect();
        let supported_methods: Vec<String> = UI_PROTOCOL_FIRST_SERVER_METHODS
            .iter()
            .filter(|method| {
                method_capability_gate(method).is_none_or(|gating_feature| {
                    supported_features
                        .iter()
                        .any(|advertised| advertised == gating_feature)
                })
            })
            .map(|method| (*method).to_owned())
            .collect();
        let mut capabilities = Self {
            version: UiProtocolVersion::current(),
            capabilities_schema_version: UI_PROTOCOL_CAPABILITIES_SCHEMA_VERSION,
            supported_methods,
            supported_notifications: string_list(UI_PROTOCOL_NOTIFICATION_METHODS),
            supported_features,
            unsupported: Vec::new(),
        };
        capabilities.unsupported = UI_PROTOCOL_FIRST_SERVER_UNSUPPORTED_METHODS
            .iter()
            .map(|method| {
                UnsupportedCapabilityReport::method(
                    *method,
                    "not implemented by the first server runtime slice",
                )
            })
            .collect();
        capabilities
    }

    pub fn with_supported_features<I, S>(mut self, features: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.supported_features = features
            .into_iter()
            .map(|feature| feature.as_ref().to_owned())
            .collect();
        self
    }

    pub fn supports_method(&self, method: &str) -> bool {
        self.supported_methods
            .iter()
            .any(|candidate| candidate == method)
    }

    pub fn supports_feature(&self, feature: &str) -> bool {
        self.supported_features
            .iter()
            .any(|candidate| candidate == feature)
    }

    pub fn unsupported_report(&self, method: &str) -> Option<&UnsupportedCapabilityReport> {
        self.unsupported
            .iter()
            .find(|report| report.method == method)
    }
}

fn is_autonomy_optional_feature(feature: &str) -> bool {
    matches!(
        feature,
        UI_PROTOCOL_FEATURE_CODING_AGENT_CONTROL_V1
            | UI_PROTOCOL_FEATURE_CODING_GOAL_RUNTIME_V1
            | UI_PROTOCOL_FEATURE_CODING_LOOP_RUNTIME_V1
    )
}

/// Result of comparing a server's advertised [`UiProtocolCapabilities`] against
/// a caller-supplied required-feature set. This is the **pure** protocol
/// semantics primitive: it has no dependency on any network/transport crate and
/// reasons only over the protocol family string, the schema version, and the
/// advertised `supported_features`.
///
/// `octos-diagnostics` wraps the result of [`compare_protocol`] into a
/// `Check`/report line (the product-facing adapter); both the TUI and the
/// server reuse this same comparator so the skew logic never forks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolCompat {
    /// The server speaks the same protocol family, its schema version is at
    /// least the client's, and every required feature is advertised.
    Compatible,
    /// The protocol family + schema are compatible, but the server does not
    /// advertise one or more features the client requires. Carries the missing
    /// feature names (in the order they were requested).
    MissingFeatures(Vec<String>),
    /// The server is on a different protocol family, or its schema version is
    /// *older* than the client's compiled-in [`UI_PROTOCOL_SCHEMA_VERSION`] —
    /// the two cannot interoperate regardless of features.
    SchemaIncompatible { server: u32, client: u32 },
}

impl ProtocolCompat {
    /// Whether the comparison found no problems at all.
    pub fn is_compatible(&self) -> bool {
        matches!(self, ProtocolCompat::Compatible)
    }
}

/// Pure protocol-compatibility comparator (no new deps; reasons only over the
/// existing protocol consts + the advertised capability payload).
///
/// Decision order (first wins), mirroring the client/server handshake contract:
///
/// 1. **Family mismatch** → [`ProtocolCompat::SchemaIncompatible`] with the
///    server's schema vs the client's compiled-in [`UI_PROTOCOL_SCHEMA_VERSION`]
///    (a different `protocol` string means the wire dialects differ — we report
///    it as schema-incompatible because no feature negotiation can bridge it).
/// 2. **Older server schema** (`server.version.schema_version <
///    UI_PROTOCOL_SCHEMA_VERSION`) → [`ProtocolCompat::SchemaIncompatible`]. A
///    server *newer* than the client is allowed (forward-compatible additive
///    schema), so only an older server is rejected.
/// 3. **Missing required features** → [`ProtocolCompat::MissingFeatures`],
///    preserving the order in `required`.
/// 4. Otherwise → [`ProtocolCompat::Compatible`].
pub fn compare_protocol<I, S>(server: &UiProtocolCapabilities, required: I) -> ProtocolCompat
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    if server.version.protocol != UI_PROTOCOL_V1
        || server.version.schema_version < UI_PROTOCOL_SCHEMA_VERSION
    {
        return ProtocolCompat::SchemaIncompatible {
            server: server.version.schema_version,
            client: UI_PROTOCOL_SCHEMA_VERSION,
        };
    }

    let missing: Vec<String> = required
        .into_iter()
        .filter(|feature| !server.supports_feature(feature.as_ref()))
        .map(|feature| feature.as_ref().to_owned())
        .collect();

    if missing.is_empty() {
        ProtocolCompat::Compatible
    } else {
        ProtocolCompat::MissingFeatures(missing)
    }
}

fn string_list(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

fn default_unsupported_capability_reason() -> String {
    "unsupported by this server".to_owned()
}

/// Typed report for protocol features a runtime slice cannot serve yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnsupportedCapabilityReport {
    pub method: String,
    #[serde(default = "default_unsupported_capability_reason")]
    pub reason: String,
}

impl UnsupportedCapabilityReport {
    pub fn method(method: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            method: method.into(),
            reason: reason.into(),
        }
    }

    pub fn to_error_data(&self) -> Value {
        serde_json::to_value(self).expect("unsupported capability report is JSON-serializable")
    }
}

/// Typed success payload for endpoints that report an unsupported capability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnsupportedCapabilityResult {
    pub unsupported: UnsupportedCapabilityReport,
}

impl UnsupportedCapabilityResult {
    pub fn method(method: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            unsupported: UnsupportedCapabilityReport::method(method, reason),
        }
    }
}

impl RpcError {
    pub fn method_not_supported(method: impl Into<String>) -> Self {
        let report =
            UnsupportedCapabilityReport::method(method, default_unsupported_capability_reason());
        Self::new(
            rpc_error_codes::METHOD_NOT_SUPPORTED,
            format!("method not supported by this server: {}", report.method),
        )
        .with_data(report.to_error_data())
    }
}

/// Typed result variants currently produced by the first server/runtime slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiResultKind {
    ProfileLocalCreate,
    SessionOpen,
    TurnStart,
    TurnInterrupt,
    ApprovalRespond,
    ApprovalScopesList,
    PermissionProfileList,
    PermissionProfileSet,
    DiffPreviewGet,
    TaskList,
    TaskCancel,
    TaskRestartFromNode,
    TaskOutputRead,
    TaskArtifactList,
    TaskArtifactRead,
    SessionHydrate,
    SessionRollback,
    SessionFork,
    ThreadGraphGet,
    TurnStateGet,
    SessionBtw,
    UnsupportedCapability,
}

pub fn first_server_result_kind_for_method(method: &str) -> Option<UiResultKind> {
    match method {
        methods::PROFILE_LOCAL_CREATE => Some(UiResultKind::ProfileLocalCreate),
        methods::SESSION_OPEN => Some(UiResultKind::SessionOpen),
        methods::TURN_START => Some(UiResultKind::TurnStart),
        methods::TURN_INTERRUPT => Some(UiResultKind::TurnInterrupt),
        methods::APPROVAL_RESPOND => Some(UiResultKind::ApprovalRespond),
        methods::APPROVAL_SCOPES_LIST => Some(UiResultKind::ApprovalScopesList),
        methods::PERMISSION_PROFILE_LIST => Some(UiResultKind::PermissionProfileList),
        methods::PERMISSION_PROFILE_SET => Some(UiResultKind::PermissionProfileSet),
        methods::DIFF_PREVIEW_GET => Some(UiResultKind::DiffPreviewGet),
        methods::TASK_LIST => Some(UiResultKind::TaskList),
        methods::TASK_CANCEL => Some(UiResultKind::TaskCancel),
        methods::TASK_RESTART_FROM_NODE => Some(UiResultKind::TaskRestartFromNode),
        methods::TASK_OUTPUT_READ => Some(UiResultKind::TaskOutputRead),
        methods::TASK_ARTIFACT_LIST => Some(UiResultKind::TaskArtifactList),
        methods::TASK_ARTIFACT_READ => Some(UiResultKind::TaskArtifactRead),
        methods::SESSION_HYDRATE => Some(UiResultKind::SessionHydrate),
        methods::SESSION_ROLLBACK => Some(UiResultKind::SessionRollback),
        methods::SESSION_FORK => Some(UiResultKind::SessionFork),
        methods::THREAD_GRAPH_GET => Some(UiResultKind::ThreadGraphGet),
        methods::TURN_STATE_GET => Some(UiResultKind::TurnStateGet),
        methods::SESSION_BTW => Some(UiResultKind::SessionBtw),
        _ => None,
    }
}

/// Minimal input item for a started turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InputItem {
    Text {
        text: String,
    },
    /// Forward-compat fallback for input item kinds not yet known to this
    /// client. The original `kind` tag and any sibling fields are dropped on
    /// purpose so unknown items stay actionable; callers that need the raw
    /// payload should branch on this variant before round-tripping.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionOpenParams {
    pub session_id: SessionKey,
    /// Optional sub-topic suffix to open. When present, the server scopes
    /// replay and live fan-out to the matching topic bucket for this session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<SessionSandboxParams>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<UiCursor>,
}

/// Optional session-scoped sandbox narrowing requested by `session/open`.
///
/// The server validates this object against the profile-derived sandbox before
/// constructing the session runtime. Requests may keep or narrow the inherited
/// policy; they must not widen network, filesystem, or sandbox isolation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSandboxParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_access: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub read_allow_paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnStartParams {
    pub session_id: SessionKey,
    pub turn_id: TurnId,
    pub input: Vec<InputItem>,
    /// UPCR-2026-015 (M9-β-1): pre-uploaded media references the user
    /// attached to this send. Each entry mirrors the `FileRef` shape
    /// already used on `Payload::UserMessage` envelopes (γ-1, PR #848)
    /// — `path` is the durable filesystem handle returned from
    /// `POST /api/upload`; `mime` and `size_bytes` are populated at
    /// upload time. Empty / absent on text-only sends.
    ///
    /// **Wire**: serialised as `media: [...]` (omitted entirely when
    /// empty). The legacy SSE `chatSSE()` path carried the same shape
    /// in its body before α-5/α-6 deleted that transport; this field
    /// restores attachment delivery on the WS path.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub media: Vec<FileRef>,
    /// UPCR-2026-015 (M9-β-1): optional sub-topic suffix that scopes
    /// this send to a per-topic session bucket (`<session>#<topic>`
    /// shape). Mirrors the legacy SSE `topic` query/body field. The
    /// validating scope and looking up history. Empty / absent for
    /// the default-topic case.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    /// UPCR-2026-015 (M9-β-1): when set, the server treats this turn
    /// as a rewrite of an existing queued user message identified by
    /// its `client_message_id` rather than appending a new turn. Used
    /// by the SPA's `/queue` slash-command flow where the user edits a
    /// queued prompt before it dispatches. The legacy SSE path
    /// supported the same field; β-1 restores it on the WS shape.
    /// Absent on regular sends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rewrite_for: Option<String>,
    /// Per-turn reasoning/thinking effort override for thinking-capable models
    /// (DeepSeek V4, OpenAI reasoning models, Grok-4). Set by the TUI `/thinking`
    /// command and attached to every turn for the rest of the session. `None`
    /// (absent) falls back to the gateway/profile default; otherwise the server
    /// overrides the turn's effort. No-op for models without a reasoning style.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffortLevel>,
    /// Explicit "this turn is a live video call" signal — the attached image
    /// (if any) is the user's current camera frame, not an uploaded file. Set
    /// by video-call surfaces (the voice screen with the camera on). The server
    /// folds it into `inbound.metadata["live_video"]`; consumers read it from
    /// `TurnAttachmentContext.live_video` and NEVER infer it from attachment
    /// types. Defaults false; omitted on the wire when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub live_video: bool,
}

/// Reasoning/thinking effort level carried on the wire (octos-core cannot depend
/// on octos-llm's `ReasoningEffort`, so the serve maps between them). Snake-case
/// on the wire: `"low" | "medium" | "high" | "max"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffortLevel {
    Low,
    Medium,
    High,
    Max,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnInterruptParams {
    pub session_id: SessionKey,
    pub turn_id: TurnId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum ApprovalDecision {
    Approve,
    Deny,
    /// Forward-compat fallback for protocol additions; carries the raw wire
    /// string so callers can introspect or log it without the decoder erroring.
    Unknown(String),
}

impl ApprovalDecision {
    pub fn as_wire_str(&self) -> &str {
        match self {
            Self::Approve => "approve",
            Self::Deny => "deny",
            Self::Unknown(value) => value.as_str(),
        }
    }
}

impl From<String> for ApprovalDecision {
    fn from(value: String) -> Self {
        match value.as_str() {
            "approve" => Self::Approve,
            "deny" => Self::Deny,
            _ => Self::Unknown(value),
        }
    }
}

impl From<ApprovalDecision> for String {
    fn from(value: ApprovalDecision) -> Self {
        value.as_wire_str().to_owned()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovalRespondParams {
    pub session_id: SessionKey,
    pub approval_id: ApprovalId,
    pub decision: ApprovalDecision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_note: Option<String>,
}

impl ApprovalRespondParams {
    pub fn new(
        session_id: SessionKey,
        approval_id: ApprovalId,
        decision: ApprovalDecision,
    ) -> Self {
        Self {
            session_id,
            approval_id,
            decision,
            approval_scope: None,
            client_note: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum ApprovalRespondStatus {
    Accepted,
    /// Forward-compat fallback; preserves any future status string a server
    /// might emit so the decoder tolerates protocol growth.
    Unknown(String),
}

impl ApprovalRespondStatus {
    pub fn as_wire_str(&self) -> &str {
        match self {
            Self::Accepted => "accepted",
            Self::Unknown(value) => value.as_str(),
        }
    }
}

impl From<String> for ApprovalRespondStatus {
    fn from(value: String) -> Self {
        match value.as_str() {
            "accepted" => Self::Accepted,
            _ => Self::Unknown(value),
        }
    }
}

impl From<ApprovalRespondStatus> for String {
    fn from(value: ApprovalRespondStatus) -> Self {
        value.as_wire_str().to_owned()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalRespondResult {
    pub approval_id: ApprovalId,
    pub accepted: bool,
    pub status: ApprovalRespondStatus,
    pub runtime_resumed: bool,
}

impl ApprovalRespondResult {
    pub fn accepted(approval_id: ApprovalId) -> Self {
        Self::accepted_with_runtime_resumed(approval_id, false)
    }

    pub fn accepted_with_runtime_resumed(approval_id: ApprovalId, runtime_resumed: bool) -> Self {
        Self {
            approval_id,
            accepted: true,
            status: ApprovalRespondStatus::Accepted,
            runtime_resumed,
        }
    }
}

/// One per-question answer carried by `user_question/respond` (UPCR-2026-023).
///
/// Forward-compat: serde defaults mean a client that omits `selected_labels`
/// (free-text only) or `free_text` still decodes; unknown sibling fields are
/// ignored. `selected_labels` holds 0..1 entries for a single-select question
/// and 0..N for a `multi_select` question; the labels must match the option
/// labels from the originating request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserQuestionAnswer {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub selected_labels: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub free_text: Option<String>,
}

/// Params for `user_question/respond` — the client's answer to a
/// `user_question/requested` event (UPCR-2026-023). Mirrors
/// [`ApprovalRespondParams`]: correlated by `question_id`, scoped to the
/// caller's `session_id`, with an optional audit/display `client_note` the
/// server must not require.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserQuestionRespondParams {
    pub session_id: SessionKey,
    pub question_id: QuestionId,
    /// One entry per question, in question order.
    pub answers: Vec<UserQuestionAnswer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_note: Option<String>,
}

impl UserQuestionRespondParams {
    pub fn new(
        session_id: SessionKey,
        question_id: QuestionId,
        answers: Vec<UserQuestionAnswer>,
    ) -> Self {
        Self {
            session_id,
            question_id,
            answers,
            client_note: None,
        }
    }
}

/// Ack result for `user_question/respond` (UPCR-2026-023). Mirrors
/// [`ApprovalRespondResult`]: confirms the answer was accepted and whether the
/// waiting runtime turn was resumed by it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserQuestionRespondResult {
    pub question_id: QuestionId,
    pub accepted: bool,
    pub runtime_resumed: bool,
}

impl UserQuestionRespondResult {
    pub fn accepted(question_id: QuestionId) -> Self {
        Self::accepted_with_runtime_resumed(question_id, false)
    }

    pub fn accepted_with_runtime_resumed(question_id: QuestionId, runtime_resumed: bool) -> Self {
        Self {
            question_id,
            accepted: true,
            runtime_resumed,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovalScopesListParams {
    pub session_id: SessionKey,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalScopesListResult {
    pub scopes: Vec<ApprovalScopeEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalScopeEntry {
    pub session_id: SessionKey,
    pub scope: String,
    pub scope_match: String,
    pub decision: ApprovalDecision,
    /// Bound `turn_id` for `turn`-scoped entries; `None` otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<TurnId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermissionProfileMode {
    #[serde(rename = "read_only", alias = "read-only")]
    ReadOnly,
    #[serde(rename = "workspace_write", alias = "workspace-write")]
    WorkspaceWrite,
    #[serde(rename = "danger_full_access", alias = "danger-full-access")]
    DangerFullAccess,
}

impl PermissionProfileMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::ReadOnly => "Read Only",
            Self::WorkspaceWrite => "Workspace Write",
            Self::DangerFullAccess => "Full Access",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionNetworkPolicy {
    Allow,
    Deny,
}

impl PermissionNetworkPolicy {
    pub fn label(self) -> &'static str {
        match self {
            Self::Allow => "network allowed",
            Self::Deny => "network blocked",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionProfileSelection {
    pub mode: PermissionProfileMode,
    pub network: PermissionNetworkPolicy,
}

impl Default for PermissionProfileSelection {
    fn default() -> Self {
        Self {
            mode: PermissionProfileMode::WorkspaceWrite,
            network: PermissionNetworkPolicy::Deny,
        }
    }
}

impl PermissionProfileSelection {
    pub fn normalized(self) -> Self {
        self
    }

    pub fn summary(self) -> String {
        format!("{}, {}", self.mode.label(), self.network.label())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PermissionProfileUpdate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<PermissionProfileMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<PermissionNetworkPolicy>,
    /// Optional approval behavior override. Accepted values are currently
    /// `on-request`/`on_request`/`ask` and `never`. Unknown values are rejected
    /// by the server-side policy resolver.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_policy: Option<String>,
}

impl PermissionProfileUpdate {
    pub fn apply_to(self, previous: PermissionProfileSelection) -> PermissionProfileSelection {
        PermissionProfileSelection {
            mode: self.mode.unwrap_or(previous.mode),
            network: self.network.unwrap_or(previous.network),
        }
        .normalized()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionProfileListParams {
    pub session_id: SessionKey,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionProfileSetParams {
    pub session_id: SessionKey,
    pub update: PermissionProfileUpdate,
    /// Optional runtime-mode override the client asserts for gating
    /// (`"tenant"`, `"cloud"`, `"local"`, `"solo"`). When provided and
    /// stricter than the server's deployment mode, the gate uses the
    /// requested mode. The override can only TIGHTEN gating, never relax.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_mode: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionProfileListResult {
    pub session_id: SessionKey,
    pub current: PermissionProfileSelection,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub profiles: Vec<PermissionProfileSelection>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionProfileSetResult {
    pub session_id: SessionKey,
    pub current: PermissionProfileSelection,
    pub applied: bool,
}

/// Parameters for `profile/local/create` (solo local onboarding).
///
/// Backward compatible: an older client that sends `{name, username, email}`
/// with no `requested_id` still deserializes and works — the server derives
/// the profile id from `username`, exactly as before. A newer client may
/// instead send a meaningful `requested_id` (e.g. `"glm"`, `"deepseek"`) and
/// omit `username`/`email`, because a solo local profile does not require an
/// owner username or email. The server normalizes `requested_id` into a slug
/// and collision-suffixes it (`glm`, `glm-2`, `glm-3`, …) to derive the
/// assigned [`ProfileLocalCreateResult::profile_id`].
///
/// Servers that understand `requested_id` advertise the additive capability
/// feature `profile.local_create.requested_id.v1`; a client can negotiate on
/// that flag before sending the new shape.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileLocalCreateParams {
    /// Meaningful profile id the user typed during onboarding. Normalized
    /// (lowercased, non-`[a-z0-9-]` collapsed to `-`) and uniqueness-suffixed
    /// server-side. Absent / empty / pathological → the server derives the id
    /// from `username` (legacy shape) or generates one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_id: Option<String>,
    /// Display name. Optional; when empty the server falls back to
    /// `requested_id` (then the assigned id) so a profile always has some
    /// display name.
    #[serde(default)]
    pub name: String,
    /// Legacy owner username. Optional now that `requested_id` can name a solo
    /// profile. When present and `requested_id` is absent it still derives the
    /// profile id, preserving the pre-existing behavior.
    #[serde(default)]
    pub username: String,
    /// Legacy owner email. Optional for a solo local profile.
    #[serde(default)]
    pub email: String,
    /// When `Some(true)`, the server records this profile as the machine's
    /// global default — the brain a bare launch resolves to in a folder with no
    /// sticky profile yet (Model A launch flow). Omitted from the wire when
    /// `None` so older servers receive the unchanged shape. Clients only send it
    /// when the server advertises `profile.local_create.default.v1`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub make_default: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileLocalCreateResult {
    pub profile_id: String,
    pub user_id: String,
    pub name: String,
    pub username: String,
    pub email: String,
    pub created: bool,
    pub runtime_mode: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiffPreviewGetParams {
    pub session_id: SessionKey,
    pub preview_id: PreviewId,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskOutputReadParams {
    pub session_id: SessionKey,
    pub task_id: TaskId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<OutputCursor>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskArtifactListParams {
    pub session_id: SessionKey,
    pub task_id: TaskId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskArtifactReadParams {
    pub session_id: SessionKey,
    pub task_id: TaskId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<OutputCursor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskListParams {
    pub session_id: SessionKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskCancelParams {
    pub task_id: TaskId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionKey>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskRestartFromNodeParams {
    pub task_id: TaskId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionKey>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskListResult {
    pub session_id: SessionKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    #[serde(default)]
    pub tasks: Vec<TaskListEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskArtifactRecord {
    pub id: String,
    pub title: String,
    pub kind: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, flatten, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskArtifactListResult {
    pub session_id: SessionKey,
    pub task_id: TaskId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub artifacts: Vec<TaskArtifactRecord>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskArtifactReadResult {
    pub session_id: SessionKey,
    pub task_id: TaskId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    pub artifact: TaskArtifactRecord,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<OutputCursor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<OutputCursor>,
    #[serde(default)]
    pub has_more: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskListEntry {
    pub id: TaskId,
    pub tool_name: String,
    pub tool_call_id: String,
    pub state: TaskRuntimeState,
    pub status: String,
    pub lifecycle_state: String,
    pub runtime_state: String,
    /// #966 / M13-B — origin of this child task. One of `"model"` (the
    /// LLM scheduled it via spawn_agent / spawn / delegate), `"supervisor"`
    /// (a backend supervisor created it, e.g. review/start), or `"user"`
    /// (explicit user-driven schedule, rare). Lets clients tell apart
    /// LLM-owned children from user-initiated tasks without parsing
    /// free-form fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// #966 / M13-B — role label assigned at spawn (e.g.
    /// `"reviewer"`, `"implementer"`, `"test_worker"`, `"explorer"`).
    /// Pairs with the M14-C role templates and lets the UX render
    /// "Reviewer running" instead of "task-xxx running".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// #966 / M13-B — bounded summary capsule for the task (mirrors
    /// `ChildResultSummary.summary` for terminal children). Short text
    /// that clients can render inline without fetching the full
    /// artifact list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// #966 / M13-B — number of artifacts the child has emitted so
    /// far. Lets the UX badge tasks with their artifact count without
    /// resolving `task/artifact/list`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_count: Option<u32>,
    /// #966 / M13-B — runtime policy stamp captured at spawn time
    /// (model, sandbox, approval policy, …). Lets reconnect hydration
    /// surface the same effective state the original task/updated
    /// notifications announced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_policy_stamp: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_key: Option<SessionKey>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_session_key: Option<SessionKey>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_terminal_state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_join_state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_joined_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_failure_action: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_detail: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_phase: Option<String>,
    pub started_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output_files: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_key: Option<SessionKey>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskCancelResult {
    pub task_id: TaskId,
    pub status: TaskRuntimeState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskRestartFromNodeResult {
    pub original_task_id: TaskId,
    pub new_task_id: TaskId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_node: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffPreviewGetStatus {
    Ready,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffPreviewSource {
    PendingStore,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffPreviewGetResult {
    pub status: DiffPreviewGetStatus,
    pub source: DiffPreviewSource,
    pub preview: DiffPreview,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffPreview {
    pub session_id: SessionKey,
    pub preview_id: PreviewId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<DiffPreviewFile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffPreviewFile {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    pub status: DiffPreviewFileStatus,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hunks: Vec<DiffPreviewHunk>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum DiffPreviewFileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    /// Forward-compat fallback for unrecognized file status values.
    Unknown(String),
}

impl DiffPreviewFileStatus {
    pub fn as_wire_str(&self) -> &str {
        match self {
            Self::Added => "added",
            Self::Modified => "modified",
            Self::Deleted => "deleted",
            Self::Renamed => "renamed",
            Self::Unknown(value) => value.as_str(),
        }
    }
}

impl From<String> for DiffPreviewFileStatus {
    fn from(value: String) -> Self {
        match value.as_str() {
            "added" => Self::Added,
            "modified" => Self::Modified,
            "deleted" => Self::Deleted,
            "renamed" => Self::Renamed,
            _ => Self::Unknown(value),
        }
    }
}

impl From<DiffPreviewFileStatus> for String {
    fn from(value: DiffPreviewFileStatus) -> Self {
        value.as_wire_str().to_owned()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffPreviewHunk {
    pub header: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lines: Vec<DiffPreviewLine>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffPreviewLine {
    pub kind: DiffPreviewLineKind,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_line: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffPreviewLineKind {
    Context,
    Added,
    Removed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskOutputReadSource {
    RuntimeProjection,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskOutputReadLimitation {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskOutputReadResult {
    pub session_id: SessionKey,
    pub task_id: TaskId,
    pub source: TaskOutputReadSource,
    pub cursor: OutputCursor,
    pub next_cursor: OutputCursor,
    pub text: String,
    pub bytes_read: u64,
    pub total_bytes: u64,
    pub truncated: bool,
    pub complete: bool,
    pub live_tail_supported: bool,
    /// True when this read came from snapshot projection rather than a live
    /// disk-routed output stream. Clients should treat the cursor returned
    /// alongside `is_snapshot_projection: true` as advisory: a fresh read may
    /// produce a different snapshot, since the underlying data is the latest
    /// task ledger entry rather than a position in a monotonic byte stream.
    /// Governed by accepted `UPCR-2026-006` (audit issue #707, M9 req 7).
    pub is_snapshot_projection: bool,
    pub task_status: String,
    pub runtime_state: String,
    pub lifecycle_state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime_detail: Option<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output_files: Vec<String>,
    pub limitations: Vec<TaskOutputReadLimitation>,
}

// ----- UPCR-2026-009 `session/hydrate` -----

/// Optional include-section tokens recognised by `session/hydrate`'s
/// `include` filter. Unknown tokens are silently dropped per UPCR-2026-009.
pub mod hydrate_sections {
    pub const MESSAGES: &str = "messages";
    pub const THREADS: &str = "threads";
    pub const TURNS: &str = "turns";
    pub const PENDING_APPROVALS: &str = "pending_approvals";
}

/// Defensive ceiling on the size of `SessionHydrateParams.include`. Matches
/// the documented `include_too_large` invalid-params variant in UPCR-2026-009.
pub const SESSION_HYDRATE_INCLUDE_MAX: usize = 32;

/// Params for `session/hydrate` (UPCR-2026-009).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionHydrateParams {
    pub session_id: SessionKey,
    /// Hydrate only items strictly greater than this cursor. Absent = full
    /// hydrate from the beginning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<UiCursor>,
    /// Selection set for response sections. Empty / absent = include all.
    /// Unknown tokens are dropped server-side.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<String>,
}

/// Single hydrated chat row in `SessionHydrateResult.messages`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HydratedMessage {
    pub seq: u64,
    pub role: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<TurnId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_message_id: Option<String>,
    pub persisted_at: DateTime<Utc>,
    /// Reasoning/thinking text captured for this message (#1502), when the
    /// provider emitted it. Surfaced on hydrate so the "· reasoning" block
    /// survives a client restart instead of silently vanishing — the store
    /// has persisted it all along. It is carried with the same hydrate
    /// projection as `message_id` and `source`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    /// Stable per-row identity, derived from `(session_id, seq,
    /// timestamp_nanos)` — identical to the `MessageMeta.message_id` on
    /// `assistant_persisted` envelopes and to the `message_id` on a
    /// `background_child_completed` envelope. Hydrated clients use it to
    /// coalesce durable transcript rows with their canonical v2 projection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    /// Provenance inferred from retained v2 envelopes. A `"background"`
    /// value identifies transcript rows coalesced by a linked
    /// `background_child_completed` envelope; absent means no retained v2
    /// projection establishes special provenance for the row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// File attachments stored with this row in the canonical session
    /// JSONL — surfaced so clients reconstructing history after a
    /// disconnect can render the same attachment represented by the
    /// corresponding canonical v2 projection.
    ///
    /// Backwards-compatible: omitted from the wire when empty so clients
    /// running older protocol versions see the same shape they used to.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub media: Vec<String>,
}

/// Lifecycle state strings for a thread in `ThreadGraphEntry.status` and the
/// hydrate result. Open registry per UPCR-2026-010 / UPCR-2026-011.
pub mod thread_status {
    pub const ACTIVE: &str = "active";
    pub const COMPLETED: &str = "completed";
    pub const ERRORED: &str = "errored";
    pub const INTERRUPTED: &str = "interrupted";
    pub const UNKNOWN: &str = "unknown";
}

/// Wire shape for one thread entry in `thread/graph/get` and `session/hydrate`.
/// Mirrors the in-memory `Session::threads()` projection (UPCR-2026-010).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadGraphEntry {
    pub thread_id: String,
    pub root_seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_client_message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<TurnId>,
    pub message_seqs: Vec<u64>,
    /// Open string registry. Initial values: `active | completed | errored |
    /// interrupted | unknown`. Future values must be registered via UPCR.
    pub status: String,
}

/// Per-turn lifecycle summary bundled into `session/hydrate`. Mirrors
/// `TurnStateGetResult` so a client can assert turn liveness from a single
/// hydrate round-trip without a follow-up RPC.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HydratedTurn {
    pub turn_id: TurnId,
    pub state: TurnLifecycleState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
}

/// Result for `session/hydrate` (UPCR-2026-009). All non-`session_id` /
/// non-`cursor` sections honour the request's `include` filter — sections
/// the client did not request are omitted entirely (NOT serialized as
/// `null`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionHydrateResult {
    pub session_id: SessionKey,
    pub cursor: UiCursor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_state: Option<UiContextState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub messages: Option<Vec<HydratedMessage>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threads: Option<Vec<ThreadGraphEntry>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turns: Option<Vec<HydratedTurn>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_approvals: Option<Vec<ApprovalRequestedEvent>>,
    /// UPCR-2026-023: still-pending structured user-questions for this
    /// session, mirroring [`pending_approvals`](Self::pending_approvals). A
    /// reconnecting client that negotiated `user_question.v1` re-renders these
    /// and can still answer them; omitted (not `null`) when the request did
    /// not ask for the `pending_approvals` section or the connection lacks the
    /// capability.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_questions: Option<Vec<UserQuestionRequestedEvent>>,
    /// Canonical v2 background-child envelopes retained in the ledger replay
    /// window. Populated only when the request asks for `messages` and the
    /// connection negotiated `projection.envelope.v2`; omitted otherwise.
    /// Their `message_id` values match the durable transcript row so a client
    /// can coalesce a background completion without consulting a second
    /// persisted-message wire lane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replayed_envelopes: Option<Vec<EnvelopeV2>>,
    /// Canonical v2 tool envelopes from the hydrate replay window. This keeps
    /// a reload's tool-card reconstruction on the same projection protocol as
    /// live delivery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replayed_tool_envelopes: Option<Vec<EnvelopeV2>>,
}

/// Params for `session/rollback` — conversation-only rewind. Drops the last
/// `num_turns` user turns from the session (persisted + in-memory). `num_turns`
/// must be `>= 1`; the server rejects `0` with `invalid_params`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRollbackParams {
    pub session_id: SessionKey,
    pub num_turns: u32,
}

/// Result for `session/rollback`. `dropped_turns` is the number of user turns
/// actually removed (clamped to the session's turn count), and `thread` is the
/// trimmed session projected via the same shape as `session/hydrate`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionRollbackResult {
    pub dropped_turns: u32,
    pub thread: SessionHydrateResult,
}

/// Params for `session/fork`: branch a new session off `session_id`,
/// copying the last `copy_messages` messages (absent → the FULL
/// history). `new_chat_id` becomes the child's chat id; the channel is
/// derived from the parent key (`channel:chat_id`), matching
/// `SessionManager::fork`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionForkParams {
    pub session_id: SessionKey,
    pub new_chat_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copy_messages: Option<u32>,
}

/// Result for `session/fork`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionForkResult {
    pub new_session_id: SessionKey,
    pub parent_session_id: SessionKey,
    /// Messages actually copied into the child.
    pub copied_messages: u32,
}

// ----- UPCR-2026-010 `thread/graph/get` -----

/// Params for `thread/graph/get` (UPCR-2026-010).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadGraphGetParams {
    pub session_id: SessionKey,
    /// Point-in-time projection cursor. Absent = current head.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<UiCursor>,
}

/// Result for `thread/graph/get` (UPCR-2026-010).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadGraphGetResult {
    pub session_id: SessionKey,
    pub cursor: UiCursor,
    pub threads: Vec<ThreadGraphEntry>,
    /// Persisted message seqs whose `thread_id` could not be resolved to a
    /// known thread (e.g. legacy load, recovery row). Empty in steady state.
    pub orphans: Vec<u64>,
}

// ----- UPCR-2026-011 `turn/state/get` -----

/// Lifecycle state surface for `turn/state/get` (UPCR-2026-011).
///
/// Open registry: future variants must be added via a follow-up UPCR.
/// Wire form is snake_case to match the rest of the v1 enum surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnLifecycleState {
    Active,
    Interrupting,
    Completed,
    Errored,
    Interrupted,
    Unknown,
}

impl TurnLifecycleState {
    /// Wire-form discriminant string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Interrupting => "interrupting",
            Self::Completed => "completed",
            Self::Errored => "errored",
            Self::Interrupted => "interrupted",
            Self::Unknown => "unknown",
        }
    }
}

/// Params for `session/btw` — a quick aside question ("btw, what are you
/// working on?") answered out-of-band while the session's live turn, if any,
/// keeps running. The server answers with ONE restricted LLM call over a
/// snapshot of the session's recent context: no tools, capped output, and the
/// exchange is ephemeral — it is never appended to the session history, so the
/// live turn never sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionBtwParams {
    pub session_id: SessionKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    pub question: String,
}

/// Result for `session/btw`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionBtwResult {
    pub session_id: SessionKey,
    pub answer: String,
    /// Model that produced the answer, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// Params for `turn/state/get` (UPCR-2026-011).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnStateGetParams {
    pub session_id: SessionKey,
    pub turn_id: TurnId,
}

/// Result for `turn/state/get` (UPCR-2026-011).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnStateGetResult {
    pub session_id: SessionKey,
    pub turn_id: TurnId,
    pub state: TurnLifecycleState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_state: Option<UiContextState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    /// Persisted-message seqs owned by this turn. Empty for `unknown` and for
    /// turns that have started but not yet committed a row.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub committed_seqs: Vec<u64>,
}

// ----- M12 Phase D-1 auxiliary REST → WS frames -----
//
// Each pair below mirrors a REST endpoint listed in the ADR's inventory
// table (`docs/adr/m12-phase-d-auxiliary-rest-to-ws.md`). Result payloads
// are typed as opaque [`Value`] containers so the WS dispatchers can
// forward the existing REST handler's JSON body byte-for-byte without
// duplicating the REST DTO shapes (`SessionInfo`, `MessageInfo`,
// `SessionFileInfo`, `BackgroundTaskInfo`, `WorkspaceContractStatus`,
// `StatusResponse`, `ContentEntry`) into the protocol crate. The shapes
// are unchanged from the REST contract — only the transport flips.

/// Params for `session/list`.
///
/// Historically an empty request (`{}`). The optional `cwd` field is an
/// **additive** extension for per-project session storage
/// (`appui.sessions_in_cwd`): when a client supplies it (and has negotiated
/// [`UI_PROTOCOL_FEATURE_SESSION_WORKSPACE_CWD_V1`]), a server with the flag
/// enabled lists the sessions stored under `<cwd>/.octos` instead of the
/// per-profile global store. It is `#[serde(default, skip_serializing_if =
/// "Option::is_none")]` so old clients that send `{}` still deserialize
/// (→ `cwd: None` → legacy global listing) and the wire shape of a
/// no-cwd request is byte-identical to the historical empty object.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionListParams {
    /// Optional project working directory. When present, honored by a
    /// server with `appui.sessions_in_cwd` enabled to scope the listing to
    /// that project's `<cwd>/.octos` session store. Absent → legacy
    /// per-profile/global listing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

/// Result for `session/list`. `sessions` is the JSON array the existing
/// `GET /api/sessions` handler emits (one `SessionInfo` per entry, per
/// `crates/octos-cli/src/api/handlers.rs:508`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionListResult {
    pub sessions: Value,
}

// ----- Smart-home bridge integration -----
//
// Mirrors the REST-facing bridge client in
// `crates/octos-cli/src/api/smart_home_bridge.rs`. Result payloads that
// carry bridge data are typed as opaque [`Value`] containers — same
// rationale as the M12 Phase D-1 frames above: octos-core cannot depend on
// octos-cli (`SmartHomeDevice`, `DeviceListResponse`, `CameraStreamInfo` live
// there), so the bridge's JSON contract stays the single source of truth in
// octos-cli and this crate stays a schema-agnostic envelope layer. Gated on
// [`UI_PROTOCOL_FEATURE_SMART_HOME_V1`].

/// Params for `smart_home/status.get`. Empty request — reports whether this
/// profile has a bridge configured without exposing its URL/token.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SmartHomeStatusGetParams {}

/// Result for `smart_home/status.get`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SmartHomeStatusGetResult {
    pub configured: bool,
}

/// Params for `smart_home/device.list`. Empty request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SmartHomeDeviceListParams {}

/// Result for `smart_home/device.list`. `devices` is the bridge's
/// `DeviceListResponse` JSON body, forwarded byte-for-byte.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SmartHomeDeviceListResult {
    pub devices: Value,
}

/// Params for `smart_home/device.command`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SmartHomeDeviceCommandParams {
    pub device_id: String,
    /// Command payload, forwarded to the bridge as a form-encoded POST body
    /// (see `send_device_command`), e.g. `{"on": true}`.
    pub params: Value,
}

/// Result for `smart_home/device.command`. Empty on success — bridge/request
/// failures surface as a JSON-RPC error response instead.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SmartHomeDeviceCommandResult {}

/// Params for `smart_home/camera.stream_start`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SmartHomeCameraStreamStartParams {
    pub device_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality: Option<u32>,
}

/// Result for `smart_home/camera.stream_start`. `stream` is the bridge's
/// `CameraStreamInfo` JSON body, forwarded byte-for-byte.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SmartHomeCameraStreamStartResult {
    pub stream: Value,
}

/// Params for `smart_home/camera.stream_stop`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SmartHomeCameraStreamStopParams {
    pub device_id: String,
}

/// Result for `smart_home/camera.stream_stop`. Empty on success.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SmartHomeCameraStreamStopResult {}

/// Params for `launch/resolve` — the pre-session launch probe. Given the
/// project `cwd` and the optionally requested profile, the server decides
/// whether to resume the folder's conversation, activate a new one, or surface
/// a cross-profile choice. Gated on
/// [`UI_PROTOCOL_FEATURE_SESSION_WORKSPACE_CWD_V1`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchResolveParams {
    /// Absolute project directory the client launched in.
    pub cwd: String,
    /// The profile the client was launched with (`--profile`), if any. Absent
    /// → the server resolves the folder's sticky profile, else its global
    /// default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
}

/// The action a [`LaunchResolveResult`] tells the client to take.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchDecisionKind {
    /// The resolved profile already has a conversation in the folder — open it.
    Resume,
    /// The folder has no conversation for any known profile — prompt to
    /// activate (create) one for the resolved profile.
    Activate,
    /// The folder holds conversation(s) for other profile(s) but not the
    /// resolved one — offer switch-and-resume or start-fresh.
    CrossProfile,
    /// No profile exists on the machine — send the user to `octos-tui onboard`.
    NoProfile,
}

/// Result for `launch/resolve`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchResolveResult {
    /// What the client should do next.
    pub decision: LaunchDecisionKind,
    /// The canonical (server-finalized) profile id to use — present for every
    /// decision except [`LaunchDecisionKind::NoProfile`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_profile: Option<String>,
    /// The other profiles that already have a conversation in the folder —
    /// present (and non-empty) only for [`LaunchDecisionKind::CrossProfile`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub existing_profiles: Vec<String>,
}

/// Params for `session/snapshot` — combined bootstrap fetch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSnapshotParams {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
}

/// Result for `session/snapshot`. Each field is the JSON body the
/// corresponding REST endpoint returns today (status / files / tasks).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSnapshotResult {
    pub status: Value,
    pub files: Value,
    pub tasks: Value,
}

/// Params for `session/messages_page` — paginated history fetch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMessagesPageParams {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_seq: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
}

/// Default page size when `SessionMessagesPageParams::limit` is omitted.
pub const SESSION_MESSAGES_PAGE_DEFAULT_LIMIT: usize = 100;
/// Server-side clamp on `SessionMessagesPageParams::limit`. Matches the
/// existing REST handler's `.min(500)` clamp at
/// `crates/octos-cli/src/api/handlers.rs:685`.
pub const SESSION_MESSAGES_PAGE_MAX_LIMIT: usize = 500;
/// Server-side clamp on `SessionMessagesPageParams::offset`. Matches the
/// existing REST handler's `.min(10_000)` clamp.
pub const SESSION_MESSAGES_PAGE_MAX_OFFSET: usize = 10_000;

/// Result for `session/messages_page`. `messages` mirrors the REST shape
/// (`Vec<MessageInfo>`). `has_more` / `next_offset` are set by the
/// dispatcher based on `messages.len() == limit`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionMessagesPageResult {
    pub messages: Value,
    pub has_more: bool,
    pub next_offset: usize,
}

/// Params for `session/status.get` — status-pill poller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionStatusGetParams {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
}

/// Result for `session/status.get`. Mirrors the JSON body of
/// `GET /api/sessions/{id}/status` (`{ active, has_deferred_files,
/// has_bg_tasks }`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionStatusGetResult {
    pub status: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_state: Option<UiContextState>,
}

/// Params for `session/files.list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionFilesListParams {
    pub session_id: String,
}

/// Result for `session/files.list`. `files` matches
/// `Vec<SessionFileInfo>` as emitted by `GET /api/sessions/{id}/files`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionFilesListResult {
    pub files: Value,
}

/// Params for `session/tasks.list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTasksListParams {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
}

/// Result for `session/tasks.list`. `tasks` matches the JSON body of
/// `GET /api/sessions/{id}/tasks` (a `BackgroundTaskInfo` array proxied
/// from the gateway; empty in standalone mode).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionTasksListResult {
    pub tasks: Value,
}

/// Params for `session/workspace.get`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionWorkspaceGetParams {
    pub session_id: String,
}

/// Result for `session/workspace.get`. `contracts` matches
/// `Vec<WorkspaceContractStatus>` as emitted by
/// `GET /api/sessions/{id}/workspace-contract`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionWorkspaceGetResult {
    pub contracts: Value,
}

/// Params for `session/title.set`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTitleSetParams {
    pub session_id: String,
    pub title: String,
}

/// Result for `session/title.set`. Echoes the resolved `session_id` and
/// title so the SPA can roundtrip the rename in a single response (the
/// REST `PATCH /api/sessions/{id}/title` returned `204 No Content`; the
/// WS shape lifts the title into the response body for echo-correctness).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTitleSetResult {
    pub session_id: String,
    pub title: String,
}

/// Server-side clamp on `SessionTitleSetParams::title` character count.
/// Matches the existing REST handler's character cap at
/// `crates/octos-cli/src/api/handlers.rs:1162`.
pub const SESSION_TITLE_SET_MAX_CHARS: usize = 200;

/// Params for `session/delete`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionDeleteParams {
    pub session_id: String,
}

/// Result for `session/delete`. Empty (the REST `DELETE` returns
/// `204 No Content`; on WS we send an empty object for consistency with
/// other void RPCs).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionDeleteResult {}

/// Params for `system/status.get`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemStatusGetParams {}

/// Result for `system/status.get`. `status` is the JSON body of the
/// existing `GET /api/status` handler (`StatusResponse` —
/// `crates/octos-cli/src/api/handlers.rs:2592`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SystemStatusGetResult {
    pub status: Value,
}

/// Params for `content/list`. `filters` is a free-form JSON object that
/// mirrors the REST `ContentQuery` shape (category, search, from, to,
/// sort, limit, offset, session_id) — see
/// `crates/octos-cli/src/content_catalog.rs:96` and the dirs/session_id
/// fields consumed by `GET /api/my/content`. Forwarded verbatim to the
/// existing REST handler.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ContentListParams {
    #[serde(default)]
    pub filters: Value,
}

/// Result for `content/list`. Mirrors the JSON body of
/// `GET /api/my/content` (`{ entries: ContentEntry[], total: number }`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContentListResult {
    pub entries: Value,
    pub total: usize,
}

/// Params for `content/delete`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentDeleteParams {
    pub id: String,
}

/// Result for `content/delete`. `deleted` is `true` when the row was
/// removed, `false` when the id was not in the catalog (the REST handler
/// returns the same boolean inside its `ActionResponse.ok`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentDeleteResult {
    pub deleted: bool,
}

/// Params for `content/bulk_delete`. The `ids` vector is capped at
/// [`CONTENT_BULK_DELETE_MAX_IDS`] entries; the WS dispatcher rejects
/// over-cap requests with `INVALID_PARAMS` before any catalog write
/// lock is taken, so a single oversized request cannot block other
/// catalog readers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentBulkDeleteParams {
    pub ids: Vec<String>,
}

/// Server-side cap on `ContentBulkDeleteParams::ids` length. Mirrored
/// in `crates/octos-cli/src/api/ui_protocol.rs` as the dispatcher's
/// early-reject threshold. Chosen so a single bulk-delete cannot
/// monopolize the catalog write-lock for more than a small bounded
/// window even if every id is valid; the 1 MiB WS frame limit is a
/// coarser secondary check.
pub const CONTENT_BULK_DELETE_MAX_IDS: usize = 256;

/// Result for `content/bulk_delete`. `deleted` is the number of catalog
/// rows successfully removed — mirrors the count surfaced by the REST
/// handler's `ActionResponse.message` ("N item(s) deleted.").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentBulkDeleteResult {
    pub deleted: usize,
}

/// Params for `memory/overview`. Empty today; the struct exists so
/// `{}` / `null` params decode uniformly (mirrors
/// [`SystemStatusGetParams`]; the wire `params` MEMBER must still be
/// present — the frame parser rejects requests without one).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryOverviewParams {}

/// Result for `memory/overview`. `overview` is the JSON body of the
/// existing `GET /api/my/memory` handler (`MemoryOverviewResponse` —
/// `crates/octos-cli/src/api/memory_panel.rs`), forwarded whole so the
/// wire shape and the REST shape cannot drift apart — PLUS RPC-layer
/// truncation metadata: the panel serves files up to 2 MiB but an RPC
/// result must fit one ~1 MiB WS text frame, so the dispatcher caps
/// each document field to a per-field byte budget and records the
/// truth beside it (`long_term_truncated` + `long_term_total_bytes`,
/// `today_truncated` + `today_total_bytes`, and per `recent[]` note
/// `content_truncated` + `content_total_bytes`; always present on the
/// WS wire). Capped fields are clean UTF-8 prefixes — no in-band
/// marker is ever spliced into the markdown.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryOverviewResult {
    pub overview: Value,
}

/// Params for `memory/entity`. `name` is the entity page stem — the
/// same value the REST route took as its `{name}` path segment and the
/// same string `memory/overview` returns in each entity summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryEntityParams {
    pub name: String,
}

/// Result for `memory/entity`. Mirrors the JSON body of
/// `GET /api/my/memory/entities/{name}` minus the redundant `ok` flag
/// (RPC success is carried by the envelope), plus RPC-layer truncation
/// metadata (the panel serves files up to 2 MiB; an RPC result must
/// fit one ~1 MiB WS text frame).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryEntityResult {
    pub name: String,
    /// Page markdown. When `content_truncated` is true this is a clean
    /// UTF-8 PREFIX of the page capped at the RPC-layer byte budget —
    /// no in-band marker is spliced into it.
    pub content: String,
    /// True when `content` was capped to fit the WS frame.
    pub content_truncated: bool,
    /// Raw byte length of the FULL page before any RPC-layer cap.
    pub content_total_bytes: usize,
}

/// Params for `cron/list`. Empty today; the struct exists so `{}` /
/// `null` params decode uniformly (mirrors [`SystemStatusGetParams`];
/// the wire `params` MEMBER must still be present).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CronListParams {}

/// Result for `cron/list`. Mirrors the JSON body of `GET /api/my/cron`:
/// `jobs` is the rendered job array, `count` its length, and
/// `gateway_running` reports whether a spawned gateway child owns
/// `cron.json` (toggles are refused while it does).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CronListResult {
    pub jobs: Value,
    pub count: usize,
    pub gateway_running: bool,
    /// True when `jobs` was capped (row count or serialized byte budget) so the
    /// result fits a single WS frame. `count` still reports the true total, so a
    /// client can surface "showing N of `count`". Defaults to `false` for
    /// backward compatibility with pre-truncation payloads.
    #[serde(default)]
    pub truncated: bool,
}

/// Params for `cron/toggle`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CronToggleParams {
    pub job_id: String,
    pub enabled: bool,
}

/// Result for `cron/toggle`. `job` is the updated job rendered exactly
/// as a `cron/list` entry. Refusals (spawned gateway owns the store)
/// surface as an RPC error whose `data.detail` is `"gateway_running"`
/// with `data.rest_status = 409`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CronToggleResult {
    pub job: Value,
}

// ----- Wave4-A `router/*` + `queue/state` -----

/// Wave4-A `router/set_mode` params. `mode` is the lowercase string
/// rendering of `octos_llm::AdaptiveMode` — `"off"`, `"hedge"`, or
/// `"lane"`. The string is intentional (a) so the wire stays decoupled
/// from `octos-llm`'s enum variant numeric layout and (b) so client
/// implementations don't have to negotiate over numeric values.
///
/// Mode change is session-scoped — it persists for the lifetime of the
/// `AdaptiveRouter` (process lifetime today), not across restarts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouterSetModeParams {
    pub session_id: SessionKey,
    pub mode: String,
}

/// Wave4-A `router/set_mode` result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouterSetModeResult {
    /// New mode actually committed by the router (echo of `params.mode`
    /// when the call succeeded). Returned so clients can confirm the
    /// transition before swapping their pill state.
    pub mode: String,
}

/// Wave4-A `router/get_metrics` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouterGetMetricsParams {
    pub session_id: SessionKey,
}

/// Wave4-A `router/get_metrics` result. Identical wire shape to
/// [`RouterStatusEvent`] (excluding the redundant `session_id` echo)
/// so a client can use the same code path for both — the notification
/// is a push variant of the same snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RouterGetMetricsResult {
    pub provider_name: String,
    pub mode: String,
    pub qos_ranking: bool,
    pub lane_scores: BTreeMap<String, f64>,
    pub circuit_breakers: BTreeMap<String, String>,
}

// ----- M10 Phase 1 `turn/spawn_complete` -----

/// Notification params for legacy `turn/spawn_complete` records (M10 Phase
/// 1). New writes use [`PayloadV2::BackgroundChildCompleted`]; this shape is
/// retained only so older durable ledger records can be decoded and projected
/// during migration. It carries durable identity plus these distinguishing
/// fields:
///
/// - `task_id` — which `spawn_only` task the completion came from.
/// - `response_to_client_message_id` — the originating user message's
///   `client_message_id`, telling the client which user prompt's thread
///   the new assistant bubble belongs under.
///
/// The full `content` and `media` are required so an old durable record can
/// still be projected atomically to the v2 child stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnSpawnCompleteEvent {
    pub session_id: SessionKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<TurnId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    /// The `spawn_only` task that produced this completion. Always
    /// populated — a `turn/spawn_complete` without a task_id is a server
    /// bug.
    pub task_id: String,
    /// Originating tool call id (the spawn_only tool invocation that
    /// produced this background task). Carrying it on the wire eliminates
    /// the client-side race where a stale `task_id → tool_call_id` map
    /// (built from `task/updated` watchers) would fail to flip the
    /// in-flight chip from spinner to checkmark on completion. Optional
    /// so legacy daemons and synthetic / fallback emission paths that
    /// cannot resolve the originating call still parse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// The originating user message's `client_message_id`, i.e. the
    /// user-prompt anchor under which the new assistant bubble should be
    /// rendered. `None` only for legacy callers that did not propagate
    /// origination through the spawn pipeline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_to_client_message_id: Option<String>,
    pub seq: u64,
    pub message_id: String,
    /// Source of the completion. Always `background` today; reserved as
    /// `String` so future variants (e.g. `recovery_background`) can extend
    /// without a wire-breaking enum change.
    pub source: String,
    pub cursor: UiCursor,
    pub persisted_at: DateTime<Utc>,
    /// REQUIRED. The full assistant text for the completion bubble.
    pub content: String,
    /// File attachments for this completion (e.g. `_report.md`,
    /// `output.mp3`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub media: Vec<String>,
}

// ----- UPCR-2026-014 M9-γ projection envelope -----

/// Token usage carried on `turn_completed` projection envelopes.
///
/// Mirrors [`crate::TokenUsage`] but is wire-stable for the M9-γ
/// projection: all fields default to zero, and the field set is frozen
/// for the v1 envelope. Future fields require a follow-up UPCR.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvelopeTokenUsage {
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub input_tokens: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub reasoning_tokens: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub cache_read_tokens: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub cache_write_tokens: u64,
}

#[inline]
fn is_zero_u64(value: &u64) -> bool {
    *value == 0
}

/// Metadata carried on `assistant_persisted` projection envelopes.
///
/// The projection only needs the durable identity fields here — the
/// committed `seq` already lives on the [`Envelope`] itself, so this
/// struct carries the *row-level* identity (`message_id`) plus the
/// wall-clock commit timestamp clients use for ordering displays.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageMeta {
    /// Server-assigned UUID of the durable row (mirrors
    /// `MessageMeta.message_id`). Stable across replays.
    pub message_id: String,
    /// RFC 3339 wall-clock time the row committed.
    pub persisted_at: DateTime<Utc>,
    /// File attachments persisted with the message — typically a single
    /// `.md` / `.mp3` / `.pptx` artefact emitted by `spawn_only` background
    /// tools (`deep_search`, `mofa_*`, `fm_tts`) or an explicit `send_file`.
    /// Empty for assistant rows that carry only text.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub media: Vec<String>,
}

/// Status carried on `tool_end` projection envelopes.
///
/// Closed snake_case enum. The four variants distinguish the UX states
/// the projection needs to render distinctly:
///
/// - `complete` — tool ran to natural completion, no error.
/// - `error` — tool surfaced a failure (`error` field carries the
///   message).
/// - `skipped` — tool was intentionally not run (e.g. deadline-skip,
///   pre-condition not met). The optional `reason` on the `tool_end`
///   payload explains why.
/// - `aborted` — tool execution was interrupted by an external signal
///   (user `turn/interrupt`, system cancellation). The optional
///   `reason` carries detail.
///
/// Future variants require a follow-up UPCR.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvelopeToolEndStatus {
    Complete,
    Error,
    Skipped,
    Aborted,
}

/// Wire-form file reference carried on `user_message` envelopes (and
/// reused as the canonical attachment shape elsewhere in the protocol).
///
/// Mirrors the `file_attached` payload's identity triple so a client
/// rendering a user upload and a server-attached file uses the same
/// fields. All three are required on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRef {
    pub path: String,
    pub mime: String,
    pub size_bytes: u64,
}

/// Bound for [`Payload::ToolStart::arguments_preview`]: enough for a
/// meaningful `shell(cd … && cargo test …)` argument echo without letting a
/// 1MB tool-arg blob into every persisted envelope + hydrate replay.
pub const ENVELOPE_TOOL_ARGUMENTS_PREVIEW_MAX: usize = 700;

/// Bound for [`Payload::ToolEnd::output_preview`]: a screenful of result
/// excerpt for the tool card, not the full output (which stays in the
/// transcript/tool message).
pub const ENVELOPE_TOOL_OUTPUT_PREVIEW_MAX: usize = 2048;

/// Sealed tagged union of payloads carried by the M9-γ projection
/// envelope. Each variant carries everything the projection needs;
/// the projection function is `(committed_log) → ChatViewModel` and
/// MUST NOT consult any other source of truth.
///
/// Wire form: JSON with `"type"` discriminator and content under `"data"`.
/// Variant names are snake_case to match the spec § 14 / TS shape.
///
/// **Turn shape**: every chat turn begins with exactly one
/// [`Payload::UserMessage`] envelope (server-mirrored from the client's
/// send), followed by zero or more `assistant_delta` / `tool_*` /
/// `file_attached` / `assistant_persisted` envelopes, terminated by
/// exactly one [`Payload::TurnCompleted`]. A refresh-only projection
/// reconstructs the `UserView` for the chat exclusively from
/// `user_message` envelopes — `assistant_delta` and `assistant_persisted`
/// alone are insufficient.
///
/// **Streaming reconciliation rule** (locked by spec § 14.2):
/// `assistant_delta.text` fragments APPEND to the live bubble in
/// strict `seq` order (concatenate). When an `assistant_persisted`
/// arrives for the same thread, its `text` field REPLACES the
/// accumulated streamed text — the persisted form is canonical and
/// avoids double-rendering the final body.
///
/// **Hard barrier**: per the M9-γ ADR and spec § 14.6,
/// [`Payload::TurnCompleted`] is the terminal payload for a `thread_id`.
/// Any envelope arriving on the same `thread_id` AFTER `turn_completed`
/// is DROPPED by the projection and counted in the
/// `octos_projection_post_completion_drop_total` metric. Threads are
/// NOT reused — a new turn must use a NEW `thread_id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Payload {
    /// User-message turn root — server-mirrored from the client's send.
    /// Every chat turn begins with exactly one `user_message` envelope,
    /// and the projection's `UserView` is reconstructed from these
    /// envelopes alone (a refresh-only projection cannot recover user
    /// bubbles from `assistant_delta` / `assistant_persisted`).
    ///
    /// The carrying [`Envelope`] populates `client_message_id` here —
    /// and ONLY here — so the optimistic `<GhostBubble>` overlay can
    /// match its server reflection and unmount.
    UserMessage {
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        files: Vec<FileRef>,
    },
    /// One streamed assistant text fragment. Multiple `assistant_delta`
    /// envelopes for the same `thread_id` accumulate (concatenate by
    /// `seq` order) into the live assistant bubble. An
    /// `assistant_persisted` for the same thread REPLACES the
    /// accumulated text.
    AssistantDelta { text: String },
    /// One streamed assistant reasoning fragment. Clients render this on a
    /// separate reasoning surface from assistant answer text.
    ReasoningDelta { text: String },
    /// Final assistant text persisted to the ledger after streaming
    /// completes. Carries the durable [`MessageMeta`] so the projection
    /// can finalize the bubble's identity and surface attachments. Its
    /// `text` field REPLACES the concatenated streamed deltas for the
    /// same thread (canonical final form; avoids double-rendering).
    AssistantPersisted { text: String, meta: MessageMeta },
    /// Tool invocation begun. The projection opens a tool-call card
    /// keyed on `tool_call_id`.
    ToolStart {
        tool_call_id: String,
        name: String,
        /// Compact JSON of the call arguments, UTF-8-truncated to
        /// [`ENVELOPE_TOOL_ARGUMENTS_PREVIEW_MAX`] — display fidelity for
        /// tool cards (`shell(cd … && cargo test)`), NOT a replayable
        /// argument record. `None` on argument-less calls and on
        /// envelopes persisted before this field existed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        arguments_preview: Option<String>,
    },
    /// Tool emitted a progress message. Idempotent per `(tool_call_id,
    /// seq)`; the projection appends in `seq` order.
    ToolProgress {
        tool_call_id: String,
        message: String,
    },
    /// Tool invocation finished. `error` is set iff `status == "error"`.
    /// `reason` carries optional human-readable detail for `skipped`
    /// (deadline-skip, pre-condition unmet) and `aborted`
    /// (user `turn/interrupt`, system cancellation) outcomes.
    ToolEnd {
        tool_call_id: String,
        status: EnvelopeToolEndStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        /// First lines of the tool result, UTF-8-truncated to
        /// [`ENVELOPE_TOOL_OUTPUT_PREVIEW_MAX`] — the `⎿ …` result excerpt
        /// under the card. `None` for output-less tools and on envelopes
        /// persisted before this field existed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output_preview: Option<String>,
        /// Wall-clock duration of the call, when the emitter tracked it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
    },
    /// File attached to the current thread (e.g. `.md` report from
    /// `deep_search` or `.mp3` from `fm_tts`). The projection adds the
    /// attachment to the most-recent assistant bubble in `thread_id`.
    FileAttached {
        path: String,
        mime: String,
        size_bytes: u64,
    },
    /// Hard barrier — terminal payload for a turn within `thread_id`.
    /// Per the M9-γ ADR, any envelope arriving with the same
    /// `thread_id` AFTER this one is DROPPED by the projection (and
    /// counted in `octos_projection_post_completion_drop_total`).
    /// Threads are not reused — a new turn must use a new `thread_id`.
    TurnCompleted { token_usage: EnvelopeTokenUsage },
}

/// Canonical M9-γ projection envelope.
///
/// Per UPCR-2026-014 and the M9-γ ADR, this is the single shape the
/// web client's deterministic projection consumes. The committed
/// envelope log is `Vec<Envelope>` indexed by `(thread_id, seq)`; the
/// projection is a pure function from that log to `ChatViewModel`.
///
/// Identity collapses to `seq` — the only key the projection cares
/// about. `client_message_id` is populated ONLY on
/// [`Payload::UserMessage`] envelopes so the optimistic
/// `<GhostBubble>` overlay can match its server reflection and unmount;
/// the projection itself NEVER consults it. All other variants leave
/// `client_message_id` at `None` (omitted on the wire).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    /// Multi-turn cluster identity — the chat thread this envelope
    /// projects into. All envelopes for one logical conversation share
    /// a `thread_id`.
    pub thread_id: String,
    /// Server-assigned strict total order WITHIN this `thread_id`.
    /// Strictly monotonic; gaps are an error and trigger
    /// rehydration. Identity for the projection.
    pub seq: u64,
    /// Populated ONLY on [`Payload::UserMessage`] envelopes (the
    /// optimistic `<GhostBubble>` overlay matches its server reflection
    /// here). Absent on every other variant (assistant deltas /
    /// persisted, tool events, file attached, turn_completed). The
    /// projection MUST NOT consult this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_message_id: Option<String>,
    /// Tagged-union payload — see [`Payload`].
    pub payload: Payload,
}

/// Ledger / wire wrapper around [`Envelope`] for the
/// `projection/envelope` notification (UPCR-2026-014 M9-γ).
///
/// The in-memory ledger needs the `SessionKey` to route the event to
/// the right per-session ring and broadcast channel, and it needs the
/// optional `topic` so the topic-scope live filter
/// (`ledger_event_matches_topic_scope`) keeps envelopes flowing to the
/// right subscriber pane. This wrapper carries those routing fields
/// **outside** of the `envelope` body so the durable ledger can persist
/// them and recovery can rebuild the routing context after restart.
///
/// **Wire shape (spec § 14.1, feat(envelope-wire-routing)):** the
/// JSON-RPC `params` field is the bare `Envelope` fields FLATTENED with
/// the routing keys — `{ thread_id, seq, client_message_id?, payload,
/// session_id, topic? }`. `session_id` is the bare base key so a
/// multi-session client can route the envelope to the correct session;
/// `topic` is omitted when `None`. This replaces the original
/// bare-`Envelope`-only wire (no routing keys), which left a
/// multi-session consumer with an unroutable empty `session_id`. The
/// flatten keeps the bare keys at the top level, so a tolerant client
/// that reads `thread_id`/`seq`/`payload` top-level and ignores unknown
/// keys (the octos-web bridge) decodes it unchanged; the decoder also
/// accepts an OLD frame lacking `session_id` (defaults to empty / None).
/// The wire DTO is [`EnvelopeWire`]; serialization happens only at the
/// JSON-RPC boundary in [`UiNotification::into_rpc_notification`] /
/// [`UiNotification::from_method_and_params`].
///
/// **Disk shape (codex #1336 round-2 BLOCKER 4 — UNCHANGED):** the
/// global `Serialize` / `Deserialize` derive on this struct includes ALL
/// fields (envelope + session_id + topic) as a NESTED `{ session_id,
/// topic, envelope }` object so the DURABLE LEDGER round-trips routing
/// state across daemon restart. BLOCKER 4's invariant — disk records
/// must not lose routing, else topic-scoped replay after restart
/// mis-routes — holds: the wire DTO above does not touch this derive,
/// so the disk path is byte-for-byte identical to before this change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvelopeNotification {
    /// Session this envelope belongs to. Used for ledger routing and
    /// broadcast fan-out. Stripped from the wire (spec § 14.1) at the
    /// `into_rpc_notification` boundary; preserved on disk so recovery
    /// can rebuild routing.
    pub session_id: SessionKey,
    /// Optional topic for topic-scoped live forwarders (#1329 P0-A class
    /// fix). Captured at the emit site BEFORE any `base_key()` strip so
    /// the topic-scope filter routes correctly even when `session_id`
    /// is the bare base key. Stripped from the wire at the
    /// `into_rpc_notification` boundary; preserved on disk so recovery
    /// can re-route topic-scoped envelopes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    /// The canonical wire envelope — serializes verbatim per spec § 14.1.
    pub envelope: Envelope,
}

/// Wire DTO for the `projection/envelope` JSON-RPC notification
/// (feat(envelope-wire-routing)).
///
/// This is the shape on the WIRE — distinct from the on-disk derive of
/// [`EnvelopeNotification`]. The bare [`Envelope`] fields are
/// `#[serde(flatten)]`-ed to the top level (`thread_id`, `seq`,
/// `client_message_id?`, `payload`) so an older/tolerant client decodes
/// them unchanged, and the routing keys `session_id` + `topic` sit
/// alongside them. Used ONLY at the JSON-RPC boundary in
/// [`UiNotification::into_rpc_notification`] /
/// [`UiNotification::from_method_and_params`]; the durable ledger never
/// serializes through this type.
///
/// Backward-compatible on decode: `session_id` defaults to the empty
/// [`SessionKey`] and `topic` to `None` when an OLD bare-envelope frame
/// (no routing keys) is received.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct EnvelopeWire {
    /// Bare base session key for client-side routing. Normalized to the
    /// base key at the `into_rpc_notification` boundary (any `#topic`
    /// suffix folded in by `turn/start` is stripped here and surfaced on
    /// `topic` below). Defaults to the empty key for legacy frames that
    /// predate this wire field.
    #[serde(default = "empty_session_key")]
    session_id: SessionKey,
    /// Optional topic for topic-scoped routing. Omitted when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    topic: Option<String>,
    /// Bare envelope fields, flattened to the top level so the wire is
    /// `{ thread_id, seq, client_message_id?, payload, session_id,
    /// topic? }`.
    #[serde(flatten)]
    envelope: Envelope,
}

// ----- projection.envelope.v2 canonical contract -----

/// Closed outcome set for [`PayloadV2::TurnTerminal`].
///
/// V1 represented only successful projection completion. V2 makes every
/// terminal turn outcome explicit, so a refresh/replay projection can settle
/// an errored or interrupted turn without consulting a legacy side channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnTerminalOutcome {
    Completed,
    Errored,
    Interrupted,
    RateLimited,
}

/// Structured error carried by an errored or interrupted v2 terminal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnTerminalError {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// Ownership binding for a v2 `file_attached` payload.
///
/// At least one identity should normally be present. `assistant_segment_id`
/// anchors an attachment to a rendered assistant segment, while
/// `tool_call_id` anchors it to a tool card. Both are retained because a file
/// can be owned by a tool result rendered in an assistant segment.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentOwnerV2 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assistant_segment_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

/// Tagged payload union for [`EnvelopeV2`].
///
/// This deliberately mirrors [`Payload`] as a NEW type rather than extending
/// it: `projection.envelope.v1` remains frozen. The wire stays
/// `{ "type": "…", "data": { … } }`, which is the flattened boundary
/// shape accepted by the Stage-0 octos-web parser.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum PayloadV2 {
    UserMessage {
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        files: Vec<FileRef>,
    },
    /// Every streamed assistant fragment carries the segment it appends to.
    AssistantDelta {
        text: String,
        assistant_segment_id: String,
    },
    ReasoningDelta {
        text: String,
    },
    /// A persisted assistant iteration finalizes the same segment as its
    /// deltas. A later assistant iteration receives a new segment id.
    AssistantPersisted {
        text: String,
        assistant_segment_id: String,
        meta: MessageMeta,
    },
    ToolStart {
        tool_call_id: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        arguments_preview: Option<String>,
    },
    ToolProgress {
        tool_call_id: String,
        message: String,
    },
    ToolEnd {
        tool_call_id: String,
        status: EnvelopeToolEndStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output_preview: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
    },
    /// A file stays explicit about the assistant/tool owner rather than
    /// relying on "most recent bubble" placement heuristics.
    FileAttached {
        path: String,
        mime: String,
        size_bytes: u64,
        attachment_owner: AttachmentOwnerV2,
    },
    /// Canonical terminal for completed, errored, and interrupted turns.
    TurnTerminal {
        outcome: TurnTerminalOutcome,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<TurnTerminalError>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token_usage: Option<EnvelopeTokenUsage>,
    },
    /// Late completion from a spawned/background task. It is emitted on its
    /// own child stream (the carrying [`EnvelopeV2::thread_id`]), linked back
    /// to the already-settled foreground turn by `parent_turn_id`.
    ///
    /// The wire tag retains the Stage-0 parser's compatible
    /// `background/spawn_complete` spelling while the Rust variant makes the
    /// child-stream semantics explicit.
    #[serde(rename = "background/spawn_complete")]
    BackgroundChildCompleted {
        parent_turn_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        response_to_client_message_id: Option<String>,
        task_id: String,
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_call_id: Option<String>,
        message_id: String,
        source: String,
        persisted_at: DateTime<Utc>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        media: Vec<String>,
    },
}

/// Canonical Stage-1 projection envelope.
///
/// This is intentionally independent from [`Envelope`]. All server v2 emits
/// stamp `cursor` with the durable [`UiCursor`] of the source ledger record.
/// The field remains optional on deserialize so the Stage-0 web parser can
/// continue accepting the cursor-absent fixture shape during mixed-version
/// replay; server v2 emission always supplies `Some`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvelopeV2 {
    pub thread_id: String,
    pub seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<UiCursor>,
    /// Explicit turn identity. For a background child stream this is the
    /// child stream identity; its payload carries `parent_turn_id`.
    pub turn_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_message_id: Option<String>,
    pub payload: PayloadV2,
}

/// Ledger/routing wrapper for an [`EnvelopeV2`].
///
/// Like [`EnvelopeNotification`], this stays nested on disk and uses a
/// separate flattened wire DTO only at the RPC boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvelopeV2Notification {
    pub session_id: SessionKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    pub envelope: EnvelopeV2,
}

/// Flattened wire DTO for the v2 contract. The field layout intentionally
/// remains compatible with the Stage-0 parser: routing keys plus the bare
/// envelope fields at the top level, never a nested `envelope` object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct EnvelopeWireV2 {
    #[serde(default = "empty_session_key")]
    session_id: SessionKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    topic: Option<String>,
    #[serde(flatten)]
    envelope: EnvelopeV2,
}

/// Serde default for the wire `session_id`: the empty session key, used
/// when an OLD bare-envelope frame (no routing keys) is decoded.
fn empty_session_key() -> SessionKey {
    SessionKey(String::new())
}

/// Draft command payloads for UI protocol v1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UiCommand {
    ProfileLocalCreate(ProfileLocalCreateParams),
    SessionOpen(SessionOpenParams),
    TurnStart(TurnStartParams),
    TurnInterrupt(TurnInterruptParams),
    ApprovalRespond(ApprovalRespondParams),
    ApprovalScopesList(ApprovalScopesListParams),
    UserQuestionRespond(UserQuestionRespondParams),
    PermissionProfileList(PermissionProfileListParams),
    PermissionProfileSet(PermissionProfileSetParams),
    DiffPreviewGet(DiffPreviewGetParams),
    TaskList(TaskListParams),
    TaskCancel(TaskCancelParams),
    TaskRestartFromNode(TaskRestartFromNodeParams),
    TaskOutputRead(TaskOutputReadParams),
    TaskArtifactList(TaskArtifactListParams),
    TaskArtifactRead(TaskArtifactReadParams),
    SessionHydrate(SessionHydrateParams),
    SessionRollback(SessionRollbackParams),
    SessionFork(SessionForkParams),
    ThreadGraphGet(ThreadGraphGetParams),
    TurnStateGet(TurnStateGetParams),
    SessionBtw(SessionBtwParams),
    // ---- M12 Phase D-1 auxiliary REST → WS frames ----
    SessionList(SessionListParams),
    SessionSnapshot(SessionSnapshotParams),
    SessionMessagesPage(SessionMessagesPageParams),
    SessionStatusGet(SessionStatusGetParams),
    SessionFilesList(SessionFilesListParams),
    SessionTasksList(SessionTasksListParams),
    SessionWorkspaceGet(SessionWorkspaceGetParams),
    SessionTitleSet(SessionTitleSetParams),
    SessionDelete(SessionDeleteParams),
    SystemStatusGet(SystemStatusGetParams),
    ContentList(ContentListParams),
    ContentDelete(ContentDeleteParams),
    ContentBulkDelete(ContentBulkDeleteParams),
    MemoryOverview(MemoryOverviewParams),
    MemoryEntity(MemoryEntityParams),
    CronList(CronListParams),
    CronToggle(CronToggleParams),
    // ---- Wave4-A: adaptive router controls ----
    RouterSetMode(RouterSetModeParams),
    RouterGetMetrics(RouterGetMetricsParams),
    // ---- launch/resolve: pre-session launch probe ----
    LaunchResolve(LaunchResolveParams),
    // ---- Smart-home bridge integration ----
    SmartHomeStatusGet(SmartHomeStatusGetParams),
    SmartHomeDeviceList(SmartHomeDeviceListParams),
    SmartHomeDeviceCommand(SmartHomeDeviceCommandParams),
    SmartHomeCameraStreamStart(SmartHomeCameraStreamStartParams),
    SmartHomeCameraStreamStop(SmartHomeCameraStreamStopParams),
}

impl UiCommand {
    pub fn method(&self) -> &'static str {
        match self {
            Self::ProfileLocalCreate(_) => methods::PROFILE_LOCAL_CREATE,
            Self::SessionOpen(_) => methods::SESSION_OPEN,
            Self::TurnStart(_) => methods::TURN_START,
            Self::TurnInterrupt(_) => methods::TURN_INTERRUPT,
            Self::ApprovalRespond(_) => methods::APPROVAL_RESPOND,
            Self::ApprovalScopesList(_) => methods::APPROVAL_SCOPES_LIST,
            Self::UserQuestionRespond(_) => methods::USER_QUESTION_RESPOND,
            Self::PermissionProfileList(_) => methods::PERMISSION_PROFILE_LIST,
            Self::PermissionProfileSet(_) => methods::PERMISSION_PROFILE_SET,
            Self::DiffPreviewGet(_) => methods::DIFF_PREVIEW_GET,
            Self::TaskList(_) => methods::TASK_LIST,
            Self::TaskCancel(_) => methods::TASK_CANCEL,
            Self::TaskRestartFromNode(_) => methods::TASK_RESTART_FROM_NODE,
            Self::TaskOutputRead(_) => methods::TASK_OUTPUT_READ,
            Self::TaskArtifactList(_) => methods::TASK_ARTIFACT_LIST,
            Self::TaskArtifactRead(_) => methods::TASK_ARTIFACT_READ,
            Self::SessionHydrate(_) => methods::SESSION_HYDRATE,
            Self::SessionRollback(_) => methods::SESSION_ROLLBACK,
            Self::SessionFork(_) => methods::SESSION_FORK,
            Self::ThreadGraphGet(_) => methods::THREAD_GRAPH_GET,
            Self::TurnStateGet(_) => methods::TURN_STATE_GET,
            Self::SessionBtw(_) => methods::SESSION_BTW,
            Self::SessionList(_) => methods::SESSION_LIST,
            Self::SessionSnapshot(_) => methods::SESSION_SNAPSHOT,
            Self::SessionMessagesPage(_) => methods::SESSION_MESSAGES_PAGE,
            Self::SessionStatusGet(_) => methods::SESSION_STATUS_GET,
            Self::SessionFilesList(_) => methods::SESSION_FILES_LIST,
            Self::SessionTasksList(_) => methods::SESSION_TASKS_LIST,
            Self::SessionWorkspaceGet(_) => methods::SESSION_WORKSPACE_GET,
            Self::SessionTitleSet(_) => methods::SESSION_TITLE_SET,
            Self::SessionDelete(_) => methods::SESSION_DELETE,
            Self::SystemStatusGet(_) => methods::SYSTEM_STATUS_GET,
            Self::ContentList(_) => methods::CONTENT_LIST,
            Self::ContentDelete(_) => methods::CONTENT_DELETE,
            Self::ContentBulkDelete(_) => methods::CONTENT_BULK_DELETE,
            Self::MemoryOverview(_) => methods::MEMORY_OVERVIEW,
            Self::MemoryEntity(_) => methods::MEMORY_ENTITY,
            Self::CronList(_) => methods::CRON_LIST,
            Self::CronToggle(_) => methods::CRON_TOGGLE,
            Self::RouterSetMode(_) => methods::ROUTER_SET_MODE,
            Self::RouterGetMetrics(_) => methods::ROUTER_GET_METRICS,
            Self::LaunchResolve(_) => methods::LAUNCH_RESOLVE,
            Self::SmartHomeStatusGet(_) => methods::SMART_HOME_STATUS_GET,
            Self::SmartHomeDeviceList(_) => methods::SMART_HOME_DEVICE_LIST,
            Self::SmartHomeDeviceCommand(_) => methods::SMART_HOME_DEVICE_COMMAND,
            Self::SmartHomeCameraStreamStart(_) => methods::SMART_HOME_CAMERA_STREAM_START,
            Self::SmartHomeCameraStreamStop(_) => methods::SMART_HOME_CAMERA_STREAM_STOP,
        }
    }

    pub fn into_rpc_request(
        self,
        id: impl Into<String>,
    ) -> Result<RpcRequest<Value>, serde_json::Error> {
        let method = self.method();
        let params = match self {
            Self::ProfileLocalCreate(params) => serde_json::to_value(params),
            Self::SessionOpen(params) => serde_json::to_value(params),
            Self::TurnStart(params) => serde_json::to_value(params),
            Self::TurnInterrupt(params) => serde_json::to_value(params),
            Self::ApprovalRespond(params) => serde_json::to_value(params),
            Self::ApprovalScopesList(params) => serde_json::to_value(params),
            Self::UserQuestionRespond(params) => serde_json::to_value(params),
            Self::PermissionProfileList(params) => serde_json::to_value(params),
            Self::PermissionProfileSet(params) => serde_json::to_value(params),
            Self::DiffPreviewGet(params) => serde_json::to_value(params),
            Self::TaskList(params) => serde_json::to_value(params),
            Self::TaskCancel(params) => serde_json::to_value(params),
            Self::TaskRestartFromNode(params) => serde_json::to_value(params),
            Self::TaskOutputRead(params) => serde_json::to_value(params),
            Self::TaskArtifactList(params) => serde_json::to_value(params),
            Self::TaskArtifactRead(params) => serde_json::to_value(params),
            Self::SessionHydrate(params) => serde_json::to_value(params),
            Self::SessionRollback(params) => serde_json::to_value(params),
            Self::SessionFork(params) => serde_json::to_value(params),
            Self::ThreadGraphGet(params) => serde_json::to_value(params),
            Self::TurnStateGet(params) => serde_json::to_value(params),
            Self::SessionBtw(params) => serde_json::to_value(params),
            Self::SessionList(params) => serde_json::to_value(params),
            Self::SessionSnapshot(params) => serde_json::to_value(params),
            Self::SessionMessagesPage(params) => serde_json::to_value(params),
            Self::SessionStatusGet(params) => serde_json::to_value(params),
            Self::SessionFilesList(params) => serde_json::to_value(params),
            Self::SessionTasksList(params) => serde_json::to_value(params),
            Self::SessionWorkspaceGet(params) => serde_json::to_value(params),
            Self::SessionTitleSet(params) => serde_json::to_value(params),
            Self::SessionDelete(params) => serde_json::to_value(params),
            Self::SystemStatusGet(params) => serde_json::to_value(params),
            Self::ContentList(params) => serde_json::to_value(params),
            Self::ContentDelete(params) => serde_json::to_value(params),
            Self::ContentBulkDelete(params) => serde_json::to_value(params),
            Self::MemoryOverview(params) => serde_json::to_value(params),
            Self::MemoryEntity(params) => serde_json::to_value(params),
            Self::CronList(params) => serde_json::to_value(params),
            Self::CronToggle(params) => serde_json::to_value(params),
            Self::RouterSetMode(params) => serde_json::to_value(params),
            Self::RouterGetMetrics(params) => serde_json::to_value(params),
            Self::LaunchResolve(params) => serde_json::to_value(params),
            Self::SmartHomeStatusGet(params) => serde_json::to_value(params),
            Self::SmartHomeDeviceList(params) => serde_json::to_value(params),
            Self::SmartHomeDeviceCommand(params) => serde_json::to_value(params),
            Self::SmartHomeCameraStreamStart(params) => serde_json::to_value(params),
            Self::SmartHomeCameraStreamStop(params) => serde_json::to_value(params),
        }?;

        Ok(RpcRequest::new(id, method, params))
    }

    pub fn from_rpc_request(request: RpcRequest<Value>) -> Result<Self, RpcError> {
        let RpcRequest {
            jsonrpc,
            method,
            params,
            ..
        } = request;

        validate_jsonrpc_version(&jsonrpc)?;
        Self::from_method_and_params(&method, params)
    }

    pub fn from_method_and_params(method: &str, params: Value) -> Result<Self, RpcError> {
        match method {
            methods::PROFILE_LOCAL_CREATE => {
                Ok(Self::ProfileLocalCreate(decode_params(method, params)?))
            }
            methods::SESSION_OPEN => Ok(Self::SessionOpen(decode_params(method, params)?)),
            methods::TURN_START => Ok(Self::TurnStart(decode_params(method, params)?)),
            methods::TURN_INTERRUPT => Ok(Self::TurnInterrupt(decode_params(method, params)?)),
            methods::APPROVAL_RESPOND => Ok(Self::ApprovalRespond(decode_params(method, params)?)),
            methods::APPROVAL_SCOPES_LIST => {
                Ok(Self::ApprovalScopesList(decode_params(method, params)?))
            }
            methods::USER_QUESTION_RESPOND => {
                Ok(Self::UserQuestionRespond(decode_params(method, params)?))
            }
            methods::PERMISSION_PROFILE_LIST => {
                Ok(Self::PermissionProfileList(decode_params(method, params)?))
            }
            methods::PERMISSION_PROFILE_SET => {
                Ok(Self::PermissionProfileSet(decode_params(method, params)?))
            }
            methods::DIFF_PREVIEW_GET => Ok(Self::DiffPreviewGet(decode_params(method, params)?)),
            methods::TASK_LIST => Ok(Self::TaskList(decode_params(method, params)?)),
            methods::TASK_CANCEL => Ok(Self::TaskCancel(decode_params(method, params)?)),
            methods::TASK_RESTART_FROM_NODE => {
                Ok(Self::TaskRestartFromNode(decode_params(method, params)?))
            }
            methods::TASK_OUTPUT_READ => Ok(Self::TaskOutputRead(decode_params(method, params)?)),
            methods::TASK_ARTIFACT_LIST => {
                Ok(Self::TaskArtifactList(decode_params(method, params)?))
            }
            methods::TASK_ARTIFACT_READ => {
                Ok(Self::TaskArtifactRead(decode_params(method, params)?))
            }
            methods::SESSION_HYDRATE => Ok(Self::SessionHydrate(decode_params(method, params)?)),
            methods::SESSION_ROLLBACK => Ok(Self::SessionRollback(decode_params(method, params)?)),
            methods::SESSION_FORK => Ok(Self::SessionFork(decode_params(method, params)?)),
            methods::THREAD_GRAPH_GET => Ok(Self::ThreadGraphGet(decode_params(method, params)?)),
            methods::TURN_STATE_GET => Ok(Self::TurnStateGet(decode_params(method, params)?)),
            methods::SESSION_BTW => Ok(Self::SessionBtw(decode_params(method, params)?)),
            methods::SESSION_LIST => Ok(Self::SessionList(decode_optional_params(method, params)?)),
            methods::LAUNCH_RESOLVE => Ok(Self::LaunchResolve(decode_params(method, params)?)),
            methods::SESSION_SNAPSHOT => Ok(Self::SessionSnapshot(decode_params(method, params)?)),
            methods::SESSION_MESSAGES_PAGE => {
                Ok(Self::SessionMessagesPage(decode_params(method, params)?))
            }
            methods::SESSION_STATUS_GET => {
                Ok(Self::SessionStatusGet(decode_params(method, params)?))
            }
            methods::SESSION_FILES_LIST => {
                Ok(Self::SessionFilesList(decode_params(method, params)?))
            }
            methods::SESSION_TASKS_LIST => {
                Ok(Self::SessionTasksList(decode_params(method, params)?))
            }
            methods::SESSION_WORKSPACE_GET => {
                Ok(Self::SessionWorkspaceGet(decode_params(method, params)?))
            }
            methods::SESSION_TITLE_SET => Ok(Self::SessionTitleSet(decode_params(method, params)?)),
            methods::SESSION_DELETE => Ok(Self::SessionDelete(decode_params(method, params)?)),
            methods::SYSTEM_STATUS_GET => Ok(Self::SystemStatusGet(decode_optional_params(
                method, params,
            )?)),
            methods::CONTENT_LIST => Ok(Self::ContentList(decode_optional_params(method, params)?)),
            methods::CONTENT_DELETE => Ok(Self::ContentDelete(decode_params(method, params)?)),
            methods::CONTENT_BULK_DELETE => {
                Ok(Self::ContentBulkDelete(decode_params(method, params)?))
            }
            methods::MEMORY_OVERVIEW => Ok(Self::MemoryOverview(decode_optional_params(
                method, params,
            )?)),
            methods::MEMORY_ENTITY => Ok(Self::MemoryEntity(decode_params(method, params)?)),
            methods::CRON_LIST => Ok(Self::CronList(decode_optional_params(method, params)?)),
            methods::CRON_TOGGLE => Ok(Self::CronToggle(decode_params(method, params)?)),
            methods::ROUTER_SET_MODE => Ok(Self::RouterSetMode(decode_params(method, params)?)),
            methods::ROUTER_GET_METRICS => {
                Ok(Self::RouterGetMetrics(decode_params(method, params)?))
            }
            methods::SMART_HOME_STATUS_GET => Ok(Self::SmartHomeStatusGet(decode_optional_params(
                method, params,
            )?)),
            methods::SMART_HOME_DEVICE_LIST => Ok(Self::SmartHomeDeviceList(
                decode_optional_params(method, params)?,
            )),
            methods::SMART_HOME_DEVICE_COMMAND => {
                Ok(Self::SmartHomeDeviceCommand(decode_params(method, params)?))
            }
            methods::SMART_HOME_CAMERA_STREAM_START => Ok(Self::SmartHomeCameraStreamStart(
                decode_params(method, params)?,
            )),
            methods::SMART_HOME_CAMERA_STREAM_STOP => Ok(Self::SmartHomeCameraStreamStop(
                decode_params(method, params)?,
            )),
            _ => Err(RpcError::method_not_found(method)),
        }
    }
}

/// Decode params that may be omitted entirely on the wire (i.e. the
/// param object is `{}` or absent). Used for empty-request methods like
/// `session/list` and `system/status.get` where the params struct has no
/// required fields.
fn decode_optional_params<T: DeserializeOwned + Default>(
    method: &str,
    params: Value,
) -> Result<T, RpcError> {
    if params.is_null() {
        return Ok(T::default());
    }
    decode_params(method, params)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UiPaneSnapshot {
    pub session_id: SessionKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generated_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<UiWorkspacePaneSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifacts: Option<UiArtifactPaneSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<UiGitPaneSnapshot>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub limitations: Vec<UiPaneSnapshotLimitation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiPaneSnapshotLimitation {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiWorkspacePaneSnapshot {
    pub root: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub readable_roots: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub writable_roots: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contract: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entries: Vec<UiWorkspacePaneEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub limitations: Vec<UiPaneSnapshotLimitation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiWorkspacePaneEntry {
    pub path: String,
    pub label: String,
    pub depth: usize,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiArtifactPaneSnapshot {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<UiArtifactPaneItem>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub limitations: Vec<UiPaneSnapshotLimitation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiArtifactPaneItem {
    pub title: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_task_id: Option<TaskId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview_id: Option<PreviewId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiGitPaneSnapshot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    pub clean: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub status: Vec<UiGitStatusItem>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<UiGitHistoryItem>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub limitations: Vec<UiPaneSnapshotLimitation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiGitStatusItem {
    pub code: String,
    pub path: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiGitHistoryItem {
    pub commit: String,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionOpened {
    pub session_id: SessionKey,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_profile_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_state: Option<UiContextState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<UiCursor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub panes: Option<UiPaneSnapshot>,
    /// Server-supported features negotiated for this session per spec § 4
    /// capability negotiation (UPCR-2026-007). Carries the protocol version,
    /// schema version, supported method/notification sets, and the negotiated
    /// `supported_features` intersection of the client's
    /// `X-Octos-Ui-Features` request with the server's known feature
    /// registry. Clients without the header receive the server's
    /// `first_server_slice` default so they can still discover the surface
    /// in-band.
    ///
    /// Older clients that don't expect the field continue to ignore it per
    /// spec § 4 ("clients should treat unknown fields as ignorable"). Older
    /// serialized payloads (e.g. ledger replays from before the field
    /// existed) decode successfully because `UiProtocolCapabilities` itself
    /// fills missing optional members with `serde(default)`; the field uses
    /// `serde(default)` for forward compatibility.
    #[serde(default = "UiProtocolCapabilities::first_server_slice")]
    pub capabilities: UiProtocolCapabilities,
    /// Server-persisted per-session reasoning/thinking effort, surfaced on
    /// session open/reconnect so a restarting TUI can restore its local
    /// `/thinking` state and mark its menu without re-deriving it. `None`
    /// (omitted on the wire) means no effort has ever been persisted for this
    /// session — the client should treat that as "default" (no override).
    ///
    /// This is the authoritative value across a full serve/TUI restart: in
    /// `--stdio` mode the serve is a child of the TUI, so a TUI restart
    /// respawns the serve and only this disk-backed value survives. The server
    /// persists it whenever a `turn/start` carries `reasoning_effort` and reads
    /// it back here. Additive + backward-compatible: older clients ignore the
    /// field, and older serialized payloads decode it as `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffortLevel>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionOpenResult {
    pub opened: SessionOpened,
}

impl SessionOpenResult {
    pub fn new(opened: SessionOpened) -> Self {
        Self { opened }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnStartResult {
    pub accepted: bool,
}

impl TurnStartResult {
    pub fn accepted() -> Self {
        Self { accepted: true }
    }
}

/// Typed result for `turn/interrupt`.
///
/// `interrupted` is the canonical boolean response. The three optional
/// fields (`reason`, `terminal_state`, `ack_timeout`) carry diagnostic
/// information the server has historically emitted via raw `json!` and are
/// codified here under accepted `UPCR-2026-008`.
///
/// String registry for `reason` (when `interrupted` is `false`):
/// - `turn_id_mismatch` — the turn_id sent does not match the active turn for
///   the session.
/// - `<terminal_state>` — set by `terminal_state` instead; `reason` stays
///   `None` for the already-terminal case.
///
/// Future `reason` values must be added via a follow-up UPCR.
///
/// `terminal_state` is set when interrupt was sent against a turn that had
/// already reached a terminal state. Values come from the server's terminal
/// state enum and currently include `completed`, `errored`, and
/// `interrupted`.
///
/// `ack_timeout` is set to `Some(true)` when the server captured the
/// interrupt but could not confirm the wire-side terminal event was received
/// within the ack window. It is omitted otherwise.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnInterruptResult {
    pub interrupted: bool,
    /// Diagnostic reason when `interrupted` is `false` and the server has a
    /// non-terminal explanation (e.g., `turn_id_mismatch`). String registry;
    /// future values must be registered via UPCR.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Set when interrupt was sent against a turn that had already reached a
    /// terminal state. Carries the terminal state name (`completed`,
    /// `errored`, `interrupted`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_state: Option<String>,
    /// `Some(true)` when the interrupt was captured but the wire-side ack of
    /// the terminal event timed out. Indicates the server did finish the
    /// interrupt but could not confirm client receipt within the ack window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ack_timeout: Option<bool>,
}

impl TurnInterruptResult {
    /// Bare constructor preserved for back-compat: callers passing only the
    /// boolean see no behavioural change.
    pub fn new(interrupted: bool) -> Self {
        Self {
            interrupted,
            reason: None,
            terminal_state: None,
            ack_timeout: None,
        }
    }

    /// Successful interrupt of a running turn — the typical happy-path
    /// response.
    pub fn interrupted_ok() -> Self {
        Self::new(true)
    }

    /// Interrupt declined with a diagnostic reason (e.g.,
    /// `turn_id_mismatch`).
    pub fn declined(reason: impl Into<String>) -> Self {
        Self {
            interrupted: false,
            reason: Some(reason.into()),
            terminal_state: None,
            ack_timeout: None,
        }
    }

    /// Interrupt against a turn that was already terminal. `interrupted` is
    /// `true` only when the prior terminal was itself `interrupted`.
    pub fn already_terminal(terminal_state: impl Into<String>, interrupted: bool) -> Self {
        Self {
            interrupted,
            reason: None,
            terminal_state: Some(terminal_state.into()),
            ack_timeout: None,
        }
    }

    /// Interrupt was captured but the wire-side ack timed out. Server still
    /// emitted the terminal event; client just couldn't be confirmed.
    pub fn ack_timed_out() -> Self {
        Self {
            interrupted: true,
            reason: None,
            terminal_state: None,
            ack_timeout: Some(true),
        }
    }
}

/// Typed RPC success results keyed by the originating request method.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(clippy::large_enum_variant)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
pub enum UiRpcResult {
    ProfileLocalCreate(ProfileLocalCreateResult),
    SessionOpen(SessionOpenResult),
    TurnStart(TurnStartResult),
    TurnInterrupt(TurnInterruptResult),
    ApprovalRespond(ApprovalRespondResult),
    ApprovalScopesList(ApprovalScopesListResult),
    PermissionProfileList(PermissionProfileListResult),
    PermissionProfileSet(PermissionProfileSetResult),
    DiffPreviewGet(DiffPreviewGetResult),
    TaskList(TaskListResult),
    TaskCancel(TaskCancelResult),
    TaskRestartFromNode(TaskRestartFromNodeResult),
    TaskOutputRead(TaskOutputReadResult),
    TaskArtifactList(TaskArtifactListResult),
    TaskArtifactRead(TaskArtifactReadResult),
    SessionHydrate(SessionHydrateResult),
    SessionRollback(SessionRollbackResult),
    SessionFork(SessionForkResult),
    ThreadGraphGet(ThreadGraphGetResult),
    TurnStateGet(TurnStateGetResult),
    SessionBtw(SessionBtwResult),
    UnsupportedCapability(UnsupportedCapabilityResult),
}

impl UiRpcResult {
    pub fn kind(&self) -> UiResultKind {
        match self {
            Self::ProfileLocalCreate(_) => UiResultKind::ProfileLocalCreate,
            Self::SessionOpen(_) => UiResultKind::SessionOpen,
            Self::TurnStart(_) => UiResultKind::TurnStart,
            Self::TurnInterrupt(_) => UiResultKind::TurnInterrupt,
            Self::ApprovalRespond(_) => UiResultKind::ApprovalRespond,
            Self::ApprovalScopesList(_) => UiResultKind::ApprovalScopesList,
            Self::PermissionProfileList(_) => UiResultKind::PermissionProfileList,
            Self::PermissionProfileSet(_) => UiResultKind::PermissionProfileSet,
            Self::DiffPreviewGet(_) => UiResultKind::DiffPreviewGet,
            Self::TaskList(_) => UiResultKind::TaskList,
            Self::TaskCancel(_) => UiResultKind::TaskCancel,
            Self::TaskRestartFromNode(_) => UiResultKind::TaskRestartFromNode,
            Self::TaskOutputRead(_) => UiResultKind::TaskOutputRead,
            Self::TaskArtifactList(_) => UiResultKind::TaskArtifactList,
            Self::TaskArtifactRead(_) => UiResultKind::TaskArtifactRead,
            Self::SessionHydrate(_) => UiResultKind::SessionHydrate,
            Self::SessionRollback(_) => UiResultKind::SessionRollback,
            Self::SessionFork(_) => UiResultKind::SessionFork,
            Self::ThreadGraphGet(_) => UiResultKind::ThreadGraphGet,
            Self::TurnStateGet(_) => UiResultKind::TurnStateGet,
            Self::SessionBtw(_) => UiResultKind::SessionBtw,
            Self::UnsupportedCapability(_) => UiResultKind::UnsupportedCapability,
        }
    }

    pub fn method(&self) -> Option<&str> {
        match self {
            Self::ProfileLocalCreate(_) => Some(methods::PROFILE_LOCAL_CREATE),
            Self::SessionOpen(_) => Some(methods::SESSION_OPEN),
            Self::TurnStart(_) => Some(methods::TURN_START),
            Self::TurnInterrupt(_) => Some(methods::TURN_INTERRUPT),
            Self::ApprovalRespond(_) => Some(methods::APPROVAL_RESPOND),
            Self::ApprovalScopesList(_) => Some(methods::APPROVAL_SCOPES_LIST),
            Self::PermissionProfileList(_) => Some(methods::PERMISSION_PROFILE_LIST),
            Self::PermissionProfileSet(_) => Some(methods::PERMISSION_PROFILE_SET),
            Self::DiffPreviewGet(_) => Some(methods::DIFF_PREVIEW_GET),
            Self::TaskList(_) => Some(methods::TASK_LIST),
            Self::TaskCancel(_) => Some(methods::TASK_CANCEL),
            Self::TaskRestartFromNode(_) => Some(methods::TASK_RESTART_FROM_NODE),
            Self::TaskOutputRead(_) => Some(methods::TASK_OUTPUT_READ),
            Self::TaskArtifactList(_) => Some(methods::TASK_ARTIFACT_LIST),
            Self::TaskArtifactRead(_) => Some(methods::TASK_ARTIFACT_READ),
            Self::SessionHydrate(_) => Some(methods::SESSION_HYDRATE),
            Self::SessionRollback(_) => Some(methods::SESSION_ROLLBACK),
            Self::SessionFork(_) => Some(methods::SESSION_FORK),
            Self::ThreadGraphGet(_) => Some(methods::THREAD_GRAPH_GET),
            Self::TurnStateGet(_) => Some(methods::TURN_STATE_GET),
            Self::SessionBtw(_) => Some(methods::SESSION_BTW),
            Self::UnsupportedCapability(result) => Some(result.unsupported.method.as_str()),
        }
    }

    pub fn into_result_value(self) -> Result<Value, serde_json::Error> {
        match self {
            Self::ProfileLocalCreate(result) => serde_json::to_value(result),
            Self::SessionOpen(result) => serde_json::to_value(result),
            Self::TurnStart(result) => serde_json::to_value(result),
            Self::TurnInterrupt(result) => serde_json::to_value(result),
            Self::ApprovalRespond(result) => serde_json::to_value(result),
            Self::ApprovalScopesList(result) => serde_json::to_value(result),
            Self::PermissionProfileList(result) => serde_json::to_value(result),
            Self::PermissionProfileSet(result) => serde_json::to_value(result),
            Self::DiffPreviewGet(result) => serde_json::to_value(result),
            Self::TaskList(result) => serde_json::to_value(result),
            Self::TaskCancel(result) => serde_json::to_value(result),
            Self::TaskRestartFromNode(result) => serde_json::to_value(result),
            Self::TaskOutputRead(result) => serde_json::to_value(result),
            Self::TaskArtifactList(result) => serde_json::to_value(result),
            Self::TaskArtifactRead(result) => serde_json::to_value(result),
            Self::SessionHydrate(result) => serde_json::to_value(result),
            Self::SessionRollback(result) => serde_json::to_value(result),
            Self::SessionFork(result) => serde_json::to_value(result),
            Self::ThreadGraphGet(result) => serde_json::to_value(result),
            Self::TurnStateGet(result) => serde_json::to_value(result),
            Self::SessionBtw(result) => serde_json::to_value(result),
            Self::UnsupportedCapability(result) => serde_json::to_value(result),
        }
    }

    pub fn into_rpc_response(
        self,
        id: impl Into<String>,
    ) -> Result<RpcResponse<Value>, serde_json::Error> {
        let result = self.into_result_value()?;
        Ok(RpcResponse::success(id, result))
    }

    pub fn from_method_and_result(method: &str, result: Value) -> Result<Self, RpcError> {
        // A server may answer any command method with an
        // `UnsupportedCapabilityResult` payload (per spec §3 capability
        // negotiation). The wire shape — an object with a single
        // `"unsupported"` key — is unambiguous, so peek at it before
        // committing to the method-specific decode path.
        if is_unsupported_capability_result(&result) {
            let parsed: UnsupportedCapabilityResult = decode_result(method, result)?;
            return Ok(Self::UnsupportedCapability(parsed));
        }
        match method {
            methods::PROFILE_LOCAL_CREATE => {
                Ok(Self::ProfileLocalCreate(decode_result(method, result)?))
            }
            methods::SESSION_OPEN => Ok(Self::SessionOpen(decode_result(method, result)?)),
            methods::TURN_START => Ok(Self::TurnStart(decode_result(method, result)?)),
            methods::TURN_INTERRUPT => Ok(Self::TurnInterrupt(decode_result(method, result)?)),
            methods::APPROVAL_RESPOND => Ok(Self::ApprovalRespond(decode_result(method, result)?)),
            methods::APPROVAL_SCOPES_LIST => {
                Ok(Self::ApprovalScopesList(decode_result(method, result)?))
            }
            methods::PERMISSION_PROFILE_LIST => {
                Ok(Self::PermissionProfileList(decode_result(method, result)?))
            }
            methods::PERMISSION_PROFILE_SET => {
                Ok(Self::PermissionProfileSet(decode_result(method, result)?))
            }
            methods::DIFF_PREVIEW_GET => Ok(Self::DiffPreviewGet(decode_result(method, result)?)),
            methods::TASK_LIST => Ok(Self::TaskList(decode_result(method, result)?)),
            methods::TASK_CANCEL => Ok(Self::TaskCancel(decode_result(method, result)?)),
            methods::TASK_RESTART_FROM_NODE => {
                Ok(Self::TaskRestartFromNode(decode_result(method, result)?))
            }
            methods::TASK_OUTPUT_READ => Ok(Self::TaskOutputRead(decode_result(method, result)?)),
            methods::TASK_ARTIFACT_LIST => {
                Ok(Self::TaskArtifactList(decode_result(method, result)?))
            }
            methods::TASK_ARTIFACT_READ => {
                Ok(Self::TaskArtifactRead(decode_result(method, result)?))
            }
            methods::SESSION_HYDRATE => Ok(Self::SessionHydrate(decode_result(method, result)?)),
            methods::SESSION_ROLLBACK => Ok(Self::SessionRollback(decode_result(method, result)?)),
            methods::SESSION_FORK => Ok(Self::SessionFork(decode_result(method, result)?)),
            methods::THREAD_GRAPH_GET => Ok(Self::ThreadGraphGet(decode_result(method, result)?)),
            methods::TURN_STATE_GET => Ok(Self::TurnStateGet(decode_result(method, result)?)),
            methods::SESSION_BTW => Ok(Self::SessionBtw(decode_result(method, result)?)),
            _ => Err(RpcError::method_not_found(method)),
        }
    }
}

/// Heuristic gate for `UiRpcResult::UnsupportedCapability` decoding: returns
/// `true` only when the result looks like `{"unsupported": {...}}`, which is
/// the unique shape of [`UnsupportedCapabilityResult`].
fn is_unsupported_capability_result(value: &Value) -> bool {
    let Some(obj) = value.as_object() else {
        return false;
    };
    if obj.len() != 1 {
        return false;
    }
    obj.get("unsupported")
        .map(|v| v.is_object())
        .unwrap_or(false)
}

pub mod progress_kinds {
    pub const STATUS: &str = "status";
    pub const THINKING: &str = "thinking";
    pub const RESPONSE: &str = "response";
    pub const STREAM_END: &str = "stream_end";
    pub const RETRY_BACKOFF: &str = "retry_backoff";
    pub const FILE_MUTATION: &str = "file_mutation";
    pub const TOKEN_COST_UPDATE: &str = "token_cost_update";
    pub const TOOL_PROGRESS: &str = "tool_progress";
    pub const TOOL_COMPLETED: &str = "tool_completed";
    /// Creative status-word rotation matching the gateway's
    /// `StatusIndicator` (`✦ Pondering...`, `✦ Brewing...`, etc.) for
    /// the web ThinkingIndicator surface. The server emits
    /// `progress/updated{kind:"status_word", label:"<word>"}` every
    /// ~8s during an in-flight agent turn; the SPA bridge
    /// (`ui-protocol-event-router.ts`) lifts these onto a
    /// `crew:status_word` DOM event the `ThinkingIndicator` listens
    /// for. CJK-aware: the rotator picks Chinese words for Chinese
    /// input, English for English.
    pub const STATUS_WORD: &str = "status_word";
    pub const UNKNOWN: &str = "unknown";
}

pub mod file_mutation_operations {
    pub const CREATE: &str = "create";
    pub const MODIFY: &str = "modify";
    pub const WRITE: &str = "write";
    pub const DELETE: &str = "delete";
}

fn is_metadata_extra_empty(extra: &BTreeMap<String, Value>) -> bool {
    extra.is_empty()
}

fn default_file_mutation_operation() -> String {
    file_mutation_operations::MODIFY.to_owned()
}

/// Retry/backoff status for transient model, stream, or tool recovery.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UiRetryBackoff {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_attempts: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backoff_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_provider: Option<String>,
}

impl UiRetryBackoff {
    pub fn new() -> Self {
        Self {
            attempt: None,
            max_attempts: None,
            backoff_ms: None,
            reason: None,
            provider: None,
            next_provider: None,
        }
    }
}

impl Default for UiRetryBackoff {
    fn default() -> Self {
        Self::new()
    }
}

/// File mutation notice for tools that write, modify, create, or delete files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiFileMutationNotice {
    pub path: String,
    #[serde(default = "default_file_mutation_operation")]
    pub operation: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview_id: Option<PreviewId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_written: Option<u64>,
}

impl UiFileMutationNotice {
    pub fn new(path: impl Into<String>, operation: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            operation: operation.into(),
            preview_id: None,
            tool_call_id: None,
            bytes_written: None,
        }
    }
}

/// Token and cost counters reported during a turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UiTokenCostUpdate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_cost: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_cost: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,
    /// Model identifier that produced this cost update. Populated by the
    /// agent emit layer from `LlmProvider::model_id()` so chat clients can
    /// render `model · tokens_in / tokens_out · duration` footers without
    /// scraping the legacy `metadata.label` field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Model context window in tokens, when the provider exposes it. Lets
    /// clients render an honest context-fill gauge against the real window
    /// instead of a hardcoded default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
}

impl UiTokenCostUpdate {
    pub fn new() -> Self {
        Self {
            input_tokens: None,
            output_tokens: None,
            reasoning_tokens: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            total_tokens: None,
            response_cost: None,
            session_cost: None,
            currency: None,
            model: None,
            context_window: None,
        }
    }
}

impl Default for UiTokenCostUpdate {
    fn default() -> Self {
        Self::new()
    }
}

/// Generic metadata for progress updates that do not fit existing
/// first-wave notification variants.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UiProgressMetadata {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iteration: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress_pct: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<UiRetryBackoff>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_mutation: Option<UiFileMutationNotice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_cost: Option<UiTokenCostUpdate>,
    #[serde(default, flatten, skip_serializing_if = "is_metadata_extra_empty")]
    pub extra: BTreeMap<String, Value>,
}

impl UiProgressMetadata {
    pub fn new(kind: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            label: None,
            message: None,
            detail: None,
            iteration: None,
            progress_pct: None,
            retry: None,
            file_mutation: None,
            token_cost: None,
            extra: BTreeMap::new(),
        }
    }

    pub fn retry_backoff(retry: UiRetryBackoff) -> Self {
        let mut metadata = Self::new(progress_kinds::RETRY_BACKOFF);
        metadata.retry = Some(retry);
        metadata
    }

    pub fn file_mutation(notice: UiFileMutationNotice) -> Self {
        let mut metadata = Self::new(progress_kinds::FILE_MUTATION);
        metadata.file_mutation = Some(notice);
        metadata
    }

    pub fn token_cost(update: UiTokenCostUpdate) -> Self {
        let mut metadata = Self::new(progress_kinds::TOKEN_COST_UPDATE);
        metadata.token_cost = Some(update);
        metadata
    }

    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }

    pub fn with_iteration(mut self, iteration: u32) -> Self {
        self.iteration = Some(iteration);
        self
    }
}

/// Standalone rich progress notification payload.
///
/// Also exposed as the inner type for `UiNotification::ProgressUpdated` so
/// typed clients can decode `progress/updated` notifications uniformly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UiProgressEvent {
    pub session_id: SessionKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<TurnId>,
    pub metadata: UiProgressMetadata,
}

/// Spec-aligned alias for `UiProgressEvent`. The protocol spec refers to the
/// `progress/updated` payload as a `ProgressUpdatedEvent`; this alias keeps that
/// naming available to callers without duplicating the struct definition.
pub type ProgressUpdatedEvent = UiProgressEvent;

impl UiProgressEvent {
    pub fn new(
        session_id: SessionKey,
        turn_id: Option<TurnId>,
        metadata: UiProgressMetadata,
    ) -> Self {
        Self {
            session_id,
            turn_id,
            metadata,
        }
    }

    pub fn method(&self) -> &'static str {
        methods::PROGRESS_UPDATED
    }

    pub fn into_rpc_notification(self) -> Result<RpcNotification<Value>, serde_json::Error> {
        Ok(RpcNotification::new(
            methods::PROGRESS_UPDATED,
            serde_json::to_value(self)?,
        ))
    }

    pub fn from_rpc_notification(notification: RpcNotification<Value>) -> Result<Self, RpcError> {
        let RpcNotification {
            jsonrpc,
            method,
            params,
        } = notification;

        validate_jsonrpc_version(&jsonrpc)?;
        if method == methods::PROGRESS_UPDATED {
            decode_params(&method, params)
        } else {
            Err(RpcError::method_not_found(method))
        }
    }
}

/// Session-level "whole job" orchestration status (`session/orchestration`
/// notification). Lets a client render a single job-status indicator that stays
/// active across the gap between a sub-agent's "task completed" and the master's
/// re-entry turn — a gap the client cannot infer on its own because the
/// master-continuation queue is server-side.
///
/// `active` is true when the session has any of: an in-flight turn, a running
/// sub-agent, or a queued/in-flight master continuation. `phase` is a coarse
/// human label ("working" / "orchestrating" / "re-entering"); `running_agents`
/// is the count of non-terminal sub-agents. When `active` is false the client
/// hides the indicator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionOrchestrationEvent {
    pub session_id: SessionKey,
    pub active: bool,
    #[serde(default)]
    pub running_agents: u32,
    #[serde(default)]
    pub pending_continuations: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnStartedEvent {
    pub session_id: SessionKey,
    pub turn_id: TurnId,
    pub timestamp: DateTime<Utc>,
    /// UPCR-2026-014 (M9-α-9): optional sub-topic suffix that scopes the
    /// turn within a session (mirrors the `<session>#<topic>` shape
    /// carried on REST/SSE chat). Multi-topic specs need this to filter
    /// `turn/started` envelopes by topic when observing the unified WS
    /// surface — the addendum is purely additive (absent on legacy
    /// turn-start envelopes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageDeltaEvent {
    pub session_id: SessionKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    pub turn_id: TurnId,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReasoningDeltaEvent {
    pub session_id: SessionKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    pub turn_id: TurnId,
    pub text: String,
}

/// #1477 voice rich output: a background visual artifact began generating for
/// the turn. The client renders a "generating" placeholder keyed off this
/// typed event instead of scraping an in-band `[[VISUAL:...]]` marker out of the
/// assistant text (which the backend now keeps out of the wire/persisted
/// surfaces entirely). The lifecycle terminates on `visual/succeeded` or
/// `visual/failed`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VisualGeneratingEvent {
    pub session_id: SessionKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    pub turn_id: TurnId,
    /// `html` | `illustrated` | `image` | `infographic`.
    pub kind: String,
}

/// #1477 voice rich output: the background visual task produced its artifact(s).
/// The structured success counterpart of [`VisualGeneratingEvent`] — the client
/// clears the "generating" placeholder off this, NOT off `file/attached` (which
/// stays a pure artifact-delivery signal). Emitted alongside `file/attached` on
/// the success branch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VisualSucceededEvent {
    pub session_id: SessionKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    pub turn_id: TurnId,
    /// `html` | `illustrated` | `image` | `infographic`.
    pub kind: String,
    /// Workspace-relative filenames of the delivered artifact(s) — the same
    /// paths carried on the accompanying `file/attached` event(s).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
}

/// #1477 voice rich output: the background visual task failed or timed out, so
/// the client should clear the "generating" placeholder.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VisualFailedEvent {
    pub session_id: SessionKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    pub turn_id: TurnId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// UPCR-2026-025 voice exit intent: the voice turn detected an end / goodbye /
/// mute intent. The model appended an in-band `[[EXIT]]` control marker; the
/// backend strips it from every model-/client-facing surface (so it never
/// reaches TTS, the `message/delta` wire, or the persisted session) and emits
/// this typed event instead. The client leaves the `/voice` screen and returns
/// home — but only AFTER the turn's farewell audio finishes playing, so the
/// goodbye is heard before navigation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VoiceExitEvent {
    pub session_id: SessionKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    pub turn_id: TurnId,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolStartedEvent {
    pub session_id: SessionKey,
    /// Topic routing key — populated from the originating
    /// `SessionKey.topic()` BEFORE any `base_key()` strip. Carried on
    /// the wire so a topic-scoped subscriber routes the event correctly
    /// even when the emit-side `session_id` was reconstructed from
    /// `base_key()`. Closes the P0-A class routing drop (#1329); see
    /// `UiNotification::topic()`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    pub turn_id: TurnId,
    pub tool_call_id: String,
    pub tool_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolProgressEvent {
    pub session_id: SessionKey,
    /// Topic routing key (see [`ToolStartedEvent::topic`]; #1329).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    pub turn_id: TurnId,
    pub tool_call_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress_pct: Option<f32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCompletedEvent {
    pub session_id: SessionKey,
    /// Topic routing key (see [`ToolStartedEvent::topic`]; #1329).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    pub turn_id: TurnId,
    pub tool_call_id: String,
    pub tool_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub success: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_preview: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalCommandDetails {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub argv: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_line: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env_keys: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalSandboxDetails {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filesystem_access: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_access: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub writable_roots: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalDiffDetails {
    pub preview_id: PreviewId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub additions: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deletions: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalFilesystemDetails {
    pub operation: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    pub outside_workspace: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub writable_roots: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalNetworkDetails {
    pub operation: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hosts: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<u16>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub urls: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalSandboxEscalationEndpoint {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_access: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalSandboxEscalationDetails {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<ApprovalSandboxEscalationEndpoint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<ApprovalSandboxEscalationEndpoint>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requested_permissions: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub justification: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suggested_prefix_rule: Vec<String>,
}

/// UPCR-2026-001 typed approval payload. `kind` is intentionally a string
/// registry so unknown future values can fall back to generic approval text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalTypedDetails {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<ApprovalCommandDetails>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<ApprovalSandboxDetails>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<ApprovalDiffDetails>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filesystem: Option<ApprovalFilesystemDetails>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<ApprovalNetworkDetails>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_escalation: Option<ApprovalSandboxEscalationDetails>,
}

impl ApprovalTypedDetails {
    pub fn command(
        command: ApprovalCommandDetails,
        sandbox: Option<ApprovalSandboxDetails>,
    ) -> Self {
        Self {
            kind: approval_kinds::COMMAND.to_owned(),
            command: Some(command),
            sandbox,
            diff: None,
            filesystem: None,
            network: None,
            sandbox_escalation: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalRenderHints {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_decision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secondary_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub danger: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub monospace_fields: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalRequestedEvent {
    pub session_id: SessionKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    pub approval_id: ApprovalId,
    pub turn_id: TurnId,
    pub tool_name: String,
    pub title: String,
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub risk: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub typed_details: Option<ApprovalTypedDetails>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub render_hints: Option<ApprovalRenderHints>,
}

impl ApprovalRequestedEvent {
    pub fn generic(
        session_id: SessionKey,
        approval_id: ApprovalId,
        turn_id: TurnId,
        tool_name: impl Into<String>,
        title: impl Into<String>,
        body: impl Into<String>,
    ) -> Self {
        Self {
            session_id,
            topic: None,
            approval_id,
            turn_id,
            tool_name: tool_name.into(),
            title: title.into(),
            body: body.into(),
            approval_kind: None,
            risk: None,
            typed_details: None,
            render_hints: None,
        }
    }
}

/// Notification emitted when an incoming approval request was auto-resolved by
/// a previously recorded scope policy entry, instead of surfacing a fresh
/// `approval/requested` to the client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalAutoResolvedEvent {
    pub session_id: SessionKey,
    /// Topic routing key (see [`ToolStartedEvent::topic`]; #1329).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    pub approval_id: ApprovalId,
    pub turn_id: TurnId,
    pub tool_name: String,
    pub scope: String,
    pub scope_match: String,
    pub decision: ApprovalDecision,
}

/// Durable record of an approval decision (manual or auto-resolved).
///
/// Replayed on reconnect so a client that connected after the decision
/// renders the approval card as Decided rather than as still pending.
///
/// Carries identifiers and decision metadata only; payload bodies (command
/// strings, diffs) are intentionally omitted for compliance / PII reasons.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalDecidedEvent {
    pub session_id: SessionKey,
    /// Topic routing key (see [`ToolStartedEvent::topic`]; #1329).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    pub approval_id: ApprovalId,
    pub turn_id: TurnId,
    pub decision: ApprovalDecision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    pub decided_at: DateTime<Utc>,
    pub decided_by: String,
    #[serde(default)]
    pub auto_resolved: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_note: Option<String>,
}

impl ApprovalDecidedEvent {
    /// Construct a manual-decision event with the supplied identifiers.
    pub fn manual(
        session_id: SessionKey,
        approval_id: ApprovalId,
        turn_id: TurnId,
        decision: ApprovalDecision,
        decided_by: impl Into<String>,
    ) -> Self {
        Self {
            session_id,
            topic: None,
            approval_id,
            turn_id,
            decision,
            scope: None,
            decided_at: Utc::now(),
            decided_by: decided_by.into(),
            auto_resolved: false,
            policy_id: None,
            client_note: None,
        }
    }
}

/// Durable notification announcing that a previously pending approval was
/// cancelled by the server before any client could respond. Reason values
/// follow [`approval_cancelled_reasons`] (open registry).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalCancelledEvent {
    pub session_id: SessionKey,
    /// Topic routing key (see [`ToolStartedEvent::topic`]; #1329).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    pub approval_id: ApprovalId,
    pub turn_id: TurnId,
    pub reason: String,
}

impl ApprovalCancelledEvent {
    pub fn turn_interrupted(
        session_id: SessionKey,
        approval_id: ApprovalId,
        turn_id: TurnId,
    ) -> Self {
        Self {
            session_id,
            topic: None,
            approval_id,
            turn_id,
            reason: approval_cancelled_reasons::TURN_INTERRUPTED.to_owned(),
        }
    }
}

/// One selectable option on a [`UserQuestion`] (UPCR-2026-023).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserQuestionOption {
    pub label: String,
    pub description: String,
}

/// One structured multiple-choice question carried by a
/// [`UserQuestionRequestedEvent`] (UPCR-2026-023). 2–4 `options`, an optional
/// `multi_select`, and a server-forced `allow_free_text` ("Other" escape
/// hatch). `header` is a short label (≤ 12 chars).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserQuestion {
    pub header: String,
    pub question: String,
    pub options: Vec<UserQuestionOption>,
    #[serde(default)]
    pub multi_select: bool,
    /// Server forces this `true` so a free-text "Other" is always offered.
    #[serde(default)]
    pub allow_free_text: bool,
}

/// Notification emitted when the agent's `ask_user_question` tool asks the user
/// a structured multiple-choice question mid-turn (UPCR-2026-023). Mirrors
/// [`ApprovalRequestedEvent`]: while unresolved the turn stays paused at the
/// blocking-tool boundary, and the mandatory generic `title`/`body` keep a
/// client that does not understand the structured `questions` field
/// actionable. The structured `questions` field is gated by `user_question.v1`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserQuestionRequestedEvent {
    pub session_id: SessionKey,
    /// Topic routing key (see [`ToolStartedEvent::topic`]; #1329).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    pub question_id: QuestionId,
    pub turn_id: TurnId,
    /// Mandatory generic fallback text.
    pub title: String,
    /// Mandatory generic fallback text.
    pub body: String,
    /// 1–4 structured questions. A client that does not understand this field
    /// falls back to rendering `title`/`body` and answering via free text.
    pub questions: Vec<UserQuestion>,
}

impl UserQuestionRequestedEvent {
    pub fn new(
        session_id: SessionKey,
        question_id: QuestionId,
        turn_id: TurnId,
        title: impl Into<String>,
        body: impl Into<String>,
        questions: Vec<UserQuestion>,
    ) -> Self {
        Self {
            session_id,
            topic: None,
            question_id,
            turn_id,
            title: title.into(),
            body: body.into(),
            questions,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskRuntimeState {
    Pending,
    Running,
    Completed,
    Failed,
    /// M9 review fix (MEDIUM #4) — governed by accepted UPCR-2026-004:
    /// background tasks cancelled mid-flight (e.g. via the
    /// `POST /api/tasks/{id}/cancel` endpoint) emit lifecycle state
    /// `cancelled` from the agent's `TaskLifecycleState`. Without this
    /// variant the AppUi mapper fell back to `Running` and rendered
    /// cancelled tasks as still running. Wire form: `"cancelled"`.
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskUpdatedEvent {
    pub session_id: SessionKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    pub task_id: TaskId,
    /// Originating tool call id. Carrying it on the wire alongside
    /// `task_id` lets the client flip the in-flight chip from spinner
    /// to checkmark without a race against a `task/updated` -> watcher
    /// -> TaskStore lookup chain (the chain that previously stayed cold
    /// because the client bridge rejected `task/updated` envelopes that
    /// lacked `turn_id`). Optional so legacy daemons and synthetic /
    /// fallback emission paths that cannot resolve the originating call
    /// still parse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    pub title: String,
    pub state: TaskRuntimeState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime_detail: Option<String>,
    // ── #1123 / M13-B projection fields ─────────────────────────────
    // #1113 wired these fields onto the `BackgroundTask` snapshot and
    // `task/list` projection, but the live `task/updated` notification
    // dropped them — clients subscribed to events saw a stale shape
    // while clients calling `task/list` saw the new shape. Mirroring
    // them here closes the gap. All five use
    // `#[serde(default, skip_serializing_if = "Option::is_none")]` so
    // legacy daemons and synthetic emitters that cannot resolve the
    // values still round-trip.
    /// Origin of the underlying task: `"model"`, `"supervisor"`, or
    /// `"user"`. Mirrors `BackgroundTask::source`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Role label assigned at spawn (`"reviewer"`, `"implementer"`,
    /// `"test_worker"`, `"explorer"`). Mirrors `BackgroundTask::role`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Bounded summary capsule mirroring `ChildResultSummary.summary`
    /// for terminal children. Mirrors `BackgroundTask::summary`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Number of artifacts emitted so far so UX can badge tasks without
    /// resolving `task/artifact/list`. Mirrors
    /// `BackgroundTask::artifact_count`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_count: Option<u32>,
    /// Effective runtime policy stamp captured at spawn. Stored as raw
    /// JSON so the wire shape does not depend on the AppUI
    /// `UiAutonomyRuntimePolicyStamp` schema. Mirrors
    /// `BackgroundTask::runtime_policy_stamp`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_policy_stamp: Option<Value>,
    /// C1 step 4: the turn that originated this task. Lets the client
    /// reconcile its per-turn "N running" task count when a sub-agent
    /// fails/recovers/errors/is-orphaned — without it the count stayed
    /// stuck and the chip stuck "Orchestrating". Optional so legacy daemons
    /// and synthetic / fallback emission paths that cannot resolve the
    /// originating turn still parse; omitted from the wire when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<TurnId>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskOutputDeltaEvent {
    pub session_id: SessionKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    pub task_id: TaskId,
    pub cursor: OutputCursor,
    pub text: String,
}

/// Status of one model-authored plan item. Wire form is snake_case
/// (`"pending"`, `"in_progress"`, `"completed"`) so clients map it to a glyph
/// without matching free-form strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanItemStatus {
    Pending,
    InProgress,
    Completed,
}

/// One entry in the agent's live checklist (`plan/updated`). `id` is stable
/// across updates so a client can re-render in place without losing selection
/// or scroll position when the plan mutates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiPlanItem {
    pub id: String,
    pub title: String,
    pub status: PlanItemStatus,
    /// Optional priority/label tag (e.g. `"P3"`), rendered as a chip.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
}

/// Snapshot of the agent's plan for a session. The `update_plan` tool sends the
/// full ordered list on every call, so a `plan/updated` REPLACES any prior
/// plan wholesale rather than diffing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiPlanRecord {
    pub items: Vec<UiPlanItem>,
    /// Overall activity label for the header line (e.g. `"Building memory
    /// panel…"`). `None` → the client derives one from the in-progress item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub updated_at_ms: i64,
}

/// `plan/updated` notification payload. Template: [`TaskUpdatedEvent`]. Gated by
/// `plan.todos.v1`; replayed as an ephemeral snapshot on `session/open`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanUpdatedEvent {
    pub session_id: SessionKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    /// The turn that authored this plan, when known. Lets the client scope the
    /// panel to the active turn and drop it on turn completion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<TurnId>,
    pub plan: UiPlanRecord,
}

/// Runtime policy details attached to M15 agent records. The policy stamp is
/// backend-owned and intentionally open so future autonomy policy fields round
/// trip without forcing clients back to raw JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UiAutonomyRuntimePolicyStamp {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_policy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_policy_id: Option<String>,
    #[serde(default, flatten, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Artifact metadata shared by `agent/artifact/list`,
/// `agent/artifact/updated`, and the nested artifact list on agent snapshots.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UiAgentArtifact {
    pub id: String,
    pub title: String,
    pub kind: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, flatten, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// M15 agent lifecycle snapshot. This mirrors the existing raw fixture and
/// orchestrator payload while keeping optional display aliases (`title`,
/// `summary`, `output_tail`) compatible with the production AppUI projection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UiAgentRecord {
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_agent_id: Option<String>,
    pub session_id: SessionKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    pub path: String,
    pub role: String,
    pub nickname: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub backend_kind: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_task: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub profile_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_policy_stamp: Option<UiAutonomyRuntimePolicyStamp>,
    #[serde(default)]
    pub artifact_count: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<UiAgentArtifact>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentUpdatedEvent {
    pub session_id: SessionKey,
    pub agent: UiAgentRecord,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentOutputDeltaEvent {
    pub session_id: SessionKey,
    pub agent_id: String,
    pub cursor: OutputCursor,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentArtifactUpdatedEvent {
    pub session_id: SessionKey,
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<UiAgentArtifact>,
}

/// M15 persisted goal snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UiGoalRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    pub goal_id: String,
    pub objective: String,
    pub status: String,
    pub token_budget: u64,
    pub tokens_used: u64,
    pub time_used_seconds: u64,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionGoalUpdatedEvent {
    pub session_id: SessionKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    pub goal: UiGoalRecord,
    pub transition_actor: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionGoalClearedEvent {
    pub session_id: SessionKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    #[serde(default)]
    pub cleared: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<UiGoalRecord>,
    pub transition_actor: String,
}

/// M15 recurring loop snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UiLoopRecord {
    pub loop_id: String,
    pub session_id: SessionKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    pub prompt: String,
    pub mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_seconds: Option<u64>,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_run_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_at_ms: Option<i64>,
    pub expires_at_ms: i64,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoopUpdatedEvent {
    pub session_id: SessionKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loop_id: Option<String>,
    #[serde(rename = "loop")]
    pub loop_state: UiLoopRecord,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ok: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UiLoopFire {
    #[serde(default)]
    pub queued: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duplicate: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dedupe_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, flatten, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoopFiredEvent {
    pub session_id: SessionKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    pub loop_id: String,
    #[serde(default, rename = "loop", skip_serializing_if = "Option::is_none")]
    pub loop_state: Option<UiLoopRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fire: Option<UiLoopFire>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ok: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoopCompletedEvent {
    pub session_id: SessionKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    pub loop_id: String,
    #[serde(default, rename = "loop", skip_serializing_if = "Option::is_none")]
    pub loop_state: Option<UiLoopRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// M16 active model-visible context state exposed through AppUI lifecycle
/// notifications. This intentionally carries hashes and counts, not raw
/// transcript content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiContextState {
    pub session_id: SessionKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    pub generation: u64,
    pub transcript_hash: String,
    pub item_count: usize,
    pub token_estimate: usize,
    pub recovery_state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_checkpoint_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_compaction_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiContextCompactionRecord {
    pub compaction_id: String,
    pub checkpoint_id: String,
    pub status: String,
    pub policy_id: String,
    pub trigger: String,
    pub input_generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_generation: Option<u64>,
    pub input_transcript_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacement_transcript_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_transcript_hash: Option<String>,
    pub input_item_count: usize,
    pub retained_count: usize,
    pub dropped_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_item_id: Option<String>,
    pub token_estimate_before: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_estimate_after: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextCompactionCompletedEvent {
    pub session_id: SessionKey,
    pub context_state: UiContextState,
    pub compaction: UiContextCompactionRecord,
}

/// UPCR-2026-026: emitted immediately BEFORE a context compaction pass so
/// clients can show an in-progress state (spinner/bar). Always followed by
/// `context/compaction_completed` for the same generation — today's serve
/// compaction is synchronous, so both may arrive in one delivery batch;
/// clients must tolerate a zero-duration window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextCompactionStartedEvent {
    pub session_id: SessionKey,
    /// Pre-compaction context state (token_estimate = the "before" size).
    pub context_state: UiContextState,
    /// Trigger label, mirrors the eventual completed record's trigger.
    pub trigger: String,
    /// The token threshold that tripped this compaction (context-window
    /// derived) — lets clients render an honest fullness percentage.
    pub threshold_tokens: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiContextNormalizationReport {
    pub generation: u64,
    pub input_transcript_hash: String,
    pub output_prompt_hash: String,
    pub model_capability_id: String,
    pub prompt_message_count: usize,
    pub token_estimate: usize,
    pub repaired_count: usize,
    pub dropped_count: usize,
    pub synthetic_count: usize,
    pub truncated_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextNormalizationReportedEvent {
    pub session_id: SessionKey,
    pub context_state: UiContextState,
    pub normalization: UiContextNormalizationReport,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WarningEvent {
    pub session_id: SessionKey,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<TurnId>,
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnCompletedEvent {
    pub session_id: SessionKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    pub turn_id: TurnId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<UiCursor>,
    /// UPCR-2026-014 (M9-α-9): aggregated input-token count for the
    /// completed turn (LLM-side prompt usage, summed across iterations).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_in: Option<u32>,
    /// UPCR-2026-014 (M9-α-9): aggregated output-token count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_out: Option<u32>,
    /// UPCR-2026-014 (M9-α-9): durable per-row identity for the final
    /// assistant message that closed the turn. Mirrors the SSE
    /// `session_result` frame's role.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_result: Option<TurnSessionResult>,
}

/// UPCR-2026-014 (M9-α-9) `turn/completed.session_result` payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnSessionResult {
    /// Authoritative committed seq for the final assistant row.
    pub committed_seq: u64,
    /// Stable per-row id (`session:seq:timestamp_ns`).
    pub message_id: String,
    /// Originating user prompt's `client_message_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_message_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnErrorEvent {
    pub session_id: SessionKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    pub turn_id: TurnId,
    pub code: String,
    pub message: String,
}

/// Wire signal that one or more durable notifications were dropped due to
/// per-connection backpressure. Clients should diverge from their cursor and
/// rehydrate via REST snapshot or `session/open` replay.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplayLossyEvent {
    pub session_id: SessionKey,
    pub dropped_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_durable_cursor: Option<UiCursor>,
}

/// UPCR-2026-014 (M9-α-9): per-turn `file_attached` envelope, mirrors
/// the SSE-only `file:` frame the agent loop emits when a tool's
/// `files_to_send` declares an artifact for the active turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileAttachedEvent {
    pub session_id: SessionKey,
    /// Topic routing key (see [`ToolStartedEvent::topic`]; #1329).
    /// Closes the P0-A regression that motivated the prior
    /// `ledger_event_matches_topic_scope` exemption — now that the
    /// field exists, the classifier consults the explicit topic and
    /// the exemption is no longer needed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    pub turn_id: TurnId,
    /// Filesystem path or URL the tool produced.
    pub path: String,
    /// Originating tool call (if any).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Optional MIME-type hint surfaced by the tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime: Option<String>,
}

/// A streamed voice-reply audio chunk (`voice/audio_chunk`). Emitted per
/// audio frame as cloud TTS synthesizes, gated by `event.voice_audio.v1`.
/// Chunks sharing a `segment_id` form one playable utterance (one reply
/// sentence); `seq` orders them and `last` marks the segment's final chunk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VoiceAudioChunkEvent {
    pub session_id: SessionKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    pub turn_id: TurnId,
    /// Groups chunks into one playable utterance (per reply sentence).
    pub segment_id: String,
    /// Chunk order within the segment (0-based).
    pub seq: u32,
    /// MIME type of the audio bytes, e.g. "audio/mpeg".
    pub mime: String,
    /// Base64-encoded raw audio bytes for this chunk.
    pub audio_b64: String,
    /// True on the final chunk of the segment.
    pub last: bool,
}

/// UPCR-2026-014 (M9-α-9): wrapper for legacy
/// `/api/sessions/:id/events/stream` SSE frames bridged onto the WS
/// surface. `kind` is the legacy SSE `type` field; `payload` is the
/// full frame body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionEventBridgedEvent {
    pub session_id: SessionKey,
    pub kind: String,
    pub payload: Value,
    /// Echo of any `topic` field carried on the legacy frame.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
}

/// Wave4-A — adaptive router status snapshot pushed alongside `turn/started`
/// and `turn/completed`. Mirrors `octos_llm::AdaptiveStatus` plus the
/// information needed by clients to render the routing pill / lane debug
/// view.
///
/// `lane_scores` carries one entry per active lane keyed by
/// `"<provider_name>/<model_id>"` (the same key used in
/// `model_catalog.json`). `circuit_breakers` carries the same keys mapped
/// to a string-rendered breaker state (`"closed"`, `"open"`, `"half_open"`)
/// so the wire shape stays stable when the underlying enum gains variants.
///
/// `BTreeMap` is intentional — deterministic wire order keeps the
/// web-client diff path stable across re-renders.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RouterStatusEvent {
    pub session_id: SessionKey,
    /// Currently selected provider, in `"<provider_name>/<model_id>"` form.
    pub provider_name: String,
    /// Active adaptive mode (`off` | `hedge` | `lane`).
    pub mode: String,
    /// QoS quality-ranking toggle (orthogonal to mode).
    pub qos_ranking: bool,
    /// Per-lane scores, sorted by lane key for deterministic wire output.
    pub lane_scores: BTreeMap<String, f64>,
    /// Per-lane circuit-breaker state — `"closed"`, `"open"`, or
    /// `"half_open"`. Lanes absent from this map have no breaker
    /// observed yet (cold start).
    pub circuit_breakers: BTreeMap<String, String>,
}

/// Wave4-A — emitted when the adaptive router fails over from one lane
/// to another. `from_provider` / `to_provider` use the same
/// `"<provider_name>/<model_id>"` key shape as
/// [`RouterStatusEvent::lane_scores`]. `reason` is free-text from the
/// router (e.g. "circuit_breaker_open", "score_drop"). `elapsed_ms` is
/// the wall time from initial provider attempt to failover decision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RouterFailoverEvent {
    pub session_id: SessionKey,
    pub from_provider: String,
    pub to_provider: String,
    pub reason: String,
    pub elapsed_ms: u64,
}

/// Wave4-A — current send-queue depth observed by the client/server
/// FIFO. `head_client_message_id` identifies the in-flight turn whose
/// completion will release the next queued frame. `None` when the queue
/// is empty (after the in-flight turn lands).
///
/// **Server emission status:** the queue itself is client-side today
/// (`octos-web/src/runtime/ui-protocol-send.ts` per-session FIFO). The
/// server never emits this variant — the web bridge manufactures it
/// locally using the existing DOM event pattern so other clients can
/// observe queue state uniformly. The variant is defined here so the
/// type shape is identical across client implementations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueStateEvent {
    pub session_id: SessionKey,
    pub pending_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_client_message_id: Option<String>,
}

/// `peer/staged` (#1801 v3) — agent-initiated peer staging. Emitted by the
/// serve/WS turn path when the model's `peer_handoff` tool staged a peer
/// through the host callback: the durable brief (and optional fenced
/// worktree) already exist on disk, and the user's client is asked to open
/// the staged session in the background. Sessions are
/// client-connection-coupled — the MODEL stages, the CLIENT opens.
///
/// Durable (ledger-appended): reconnect replay redelivers the event, so a
/// client dedups by an already-open session for `topic`. `topic` here is the
/// staged PEER's session topic (`peer-<slug>`), a payload field — routing
/// still keys off `session_id`, the ORIGINATING session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeerStagedEvent {
    /// The ORIGINATING session (the conversation whose turn staged the peer).
    pub session_id: SessionKey,
    /// Topic of the staged peer session the client opens (`peer-<slug>`).
    pub topic: String,
    /// Directory slug reserved under the profile's `peers/` root.
    pub slug: String,
    /// The full task contract handed to the peer (also durable on disk at
    /// `brief_path` — carried inline so the client can render a preview
    /// without a filesystem read).
    pub brief: String,
    /// Absolute path of the durable brief (`peers/<slug>/brief.md`).
    pub brief_path: String,
    /// Working directory for the peer session (worktree checkout when
    /// fenced, else the originating session's workspace root).
    pub cwd: String,
    /// Fence branch (`peer/<slug>`) when a worktree was created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_branch: Option<String>,
    /// Profile the peer session runs under.
    pub profile_id: String,
}

/// `peer/closed` — the model's `peer_close` tool tore down a staged peer
/// session: the durable brief (and optional fenced worktree) were evicted
/// server-side, so the user's client should close the peer pane it opened
/// for `topic`. Mirrors [`PeerStagedEvent`]: routing keys off `session_id`
/// (the ORIGINATING session), and `topic` (`peer-<slug>`) is the closed
/// peer's session topic carried as a payload field.
///
/// Durable (ledger-appended): reconnect replay redelivers the event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeerClosedEvent {
    /// The ORIGINATING session (the conversation whose turn closed the peer).
    pub session_id: SessionKey,
    /// Topic of the closed peer session the client tears down (`peer-<slug>`).
    pub topic: String,
    /// Directory slug that was reserved under the profile's `peers/` root.
    pub slug: String,
    /// Profile the peer session ran under.
    pub profile_id: String,
}

/// Draft notification payloads for UI protocol v1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(clippy::large_enum_variant)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UiNotification {
    SessionOpened(SessionOpened),
    TurnStarted(TurnStartedEvent),
    MessageDelta(MessageDeltaEvent),
    ReasoningDelta(ReasoningDeltaEvent),
    /// #1477 voice rich output: a background visual artifact started generating.
    VisualGenerating(VisualGeneratingEvent),
    /// #1477 voice rich output: a background visual artifact was produced.
    VisualSucceeded(VisualSucceededEvent),
    /// #1477 voice rich output: a background visual task failed / timed out.
    VisualFailed(VisualFailedEvent),
    /// UPCR-2026-025 voice exit intent: the voice turn detected an end /
    /// goodbye / mute intent; the client returns home after the farewell audio.
    VoiceExit(VoiceExitEvent),
    ToolStarted(ToolStartedEvent),
    ToolProgress(ToolProgressEvent),
    ToolCompleted(ToolCompletedEvent),
    ApprovalRequested(ApprovalRequestedEvent),
    ApprovalAutoResolved(ApprovalAutoResolvedEvent),
    ApprovalDecided(ApprovalDecidedEvent),
    ApprovalCancelled(ApprovalCancelledEvent),
    /// UPCR-2026-023: structured mid-turn user question. Mirrors
    /// `ApprovalRequested`; pauses the turn at the blocking-tool boundary.
    UserQuestionRequested(UserQuestionRequestedEvent),
    TaskUpdated(TaskUpdatedEvent),
    /// Model-authored plan/todo checklist snapshot (gated by `plan.todos.v1`).
    PlanUpdated(PlanUpdatedEvent),
    TaskOutputDelta(TaskOutputDeltaEvent),
    ProgressUpdated(ProgressUpdatedEvent),
    Warning(WarningEvent),
    TurnCompleted(TurnCompletedEvent),
    TurnError(TurnErrorEvent),
    ReplayLossy(ReplayLossyEvent),
    /// Legacy completion event for durable records written before the v2-only
    /// background-child projection migration. New writes use `EnvelopeV2`.
    TurnSpawnComplete(TurnSpawnCompleteEvent),
    /// UPCR-2026-014 (M9-α-9): per-turn file attachment event.
    FileAttached(FileAttachedEvent),
    /// Streamed voice-reply audio chunk (gated by `event.voice_audio.v1`).
    VoiceAudioChunk(VoiceAudioChunkEvent),
    /// UPCR-2026-014 (M9-α-9): wrapper for legacy
    /// `/api/sessions/:id/events/stream` SSE frames bridged onto the
    /// unified v1 ledger.
    SessionEventBridged(SessionEventBridgedEvent),
    /// Wave4-A: adaptive routing snapshot emitted on `turn/started` and
    /// `turn/completed` so clients can render the routing pill / lane
    /// debug view without polling.
    RouterStatus(RouterStatusEvent),
    /// Wave4-A: adaptive router crossed a lane (failover). The status
    /// emitted at the next turn boundary will reflect the new lane, but
    /// clients that want to surface the transition itself (toast, status
    /// pill flash) subscribe to this notification.
    RouterFailover(RouterFailoverEvent),
    /// Wave4-A: queue-state snapshot. Client-manufactured today — server
    /// never emits this. See [`QueueStateEvent`] docs.
    QueueState(QueueStateEvent),
    /// UPCR-2026-021 M15: backend-owned agent lifecycle snapshot.
    AgentUpdated(AgentUpdatedEvent),
    /// UPCR-2026-021 M15: best-effort agent output tail delta.
    AgentOutputDelta(AgentOutputDeltaEvent),
    /// UPCR-2026-021 M15: agent artifact metadata changed.
    AgentArtifactUpdated(AgentArtifactUpdatedEvent),
    /// UPCR-2026-021 M15: persisted session goal changed.
    SessionGoalUpdated(SessionGoalUpdatedEvent),
    /// UPCR-2026-021 M15: persisted session goal cleared.
    SessionGoalCleared(SessionGoalClearedEvent),
    /// UPCR-2026-021 M15: recurring loop metadata changed.
    LoopUpdated(LoopUpdatedEvent),
    /// UPCR-2026-021 M15: loop fired and queued/attempted a continuation.
    LoopFired(LoopFiredEvent),
    /// UPCR-2026-021 M15: loop iteration reached a terminal result.
    LoopCompleted(LoopCompletedEvent),
    /// M16: compact-context lifecycle event.
    ContextCompactionCompleted(ContextCompactionCompletedEvent),
    ContextCompactionStarted(ContextCompactionStartedEvent),
    /// M16: prompt normalization lifecycle event.
    ContextNormalizationReported(ContextNormalizationReportedEvent),
    /// Session-level whole-job orchestration status. Emitted when the session's
    /// orchestration state changes (turn active / sub-agents running / master
    /// continuation pending), so a client can render a job indicator that stays
    /// live across the sub-agent-complete → master-re-entry gap.
    SessionOrchestration(SessionOrchestrationEvent),
    /// #1801 v3: the model's `peer_handoff` tool staged a sovereign peer
    /// session (durable brief + optional fenced worktree); the client opens
    /// the staged session in the background. See [`PeerStagedEvent`].
    PeerStaged(PeerStagedEvent),
    /// The model's `peer_close` tool tore down a staged peer session; the
    /// client closes the peer pane it opened. See [`PeerClosedEvent`].
    PeerClosed(PeerClosedEvent),
    /// UPCR-2026-014 (M9-γ) canonical projection envelope (`projection/envelope`).
    /// Spec § 14. Capability-gated on `projection.envelope.v1`; the
    /// per-connection live filter keeps legacy and envelope deliveries
    /// mutually exclusive (legacy clients never see this variant,
    /// negotiated clients see ONLY this variant for the events it
    /// supersedes — `message/delta`, `tool/*`,
    /// `turn/completed`, `file/attached`).
    Envelope(EnvelopeNotification),
    /// Stage-1 canonical projection envelope. Uses the same flattened
    /// `projection/envelope` method as v1. New canonical rows are delivered
    /// unconditionally; `projection.envelope.v2` still requests v2
    /// projection of historical source records.
    EnvelopeV2(EnvelopeV2Notification),
}

fn set_topic_if_absent(slot: &mut Option<String>, topic: &str) {
    if slot
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .is_none()
    {
        *slot = Some(topic.to_owned());
    }
}

impl UiNotification {
    pub fn method(&self) -> &'static str {
        match self {
            Self::SessionOpened(_) => methods::SESSION_OPEN,
            Self::TurnStarted(_) => methods::TURN_STARTED,
            Self::MessageDelta(_) => methods::MESSAGE_DELTA,
            Self::ReasoningDelta(_) => methods::MESSAGE_REASONING_DELTA,
            Self::VisualGenerating(_) => methods::VISUAL_GENERATING,
            Self::VisualSucceeded(_) => methods::VISUAL_SUCCEEDED,
            Self::VisualFailed(_) => methods::VISUAL_FAILED,
            Self::VoiceExit(_) => methods::VOICE_EXIT,
            Self::ToolStarted(_) => methods::TOOL_STARTED,
            Self::ToolProgress(_) => methods::TOOL_PROGRESS,
            Self::ToolCompleted(_) => methods::TOOL_COMPLETED,
            Self::ApprovalRequested(_) => methods::APPROVAL_REQUESTED,
            Self::ApprovalAutoResolved(_) => methods::APPROVAL_AUTO_RESOLVED,
            Self::ApprovalDecided(_) => methods::APPROVAL_DECIDED,
            Self::ApprovalCancelled(_) => methods::APPROVAL_CANCELLED,
            Self::UserQuestionRequested(_) => methods::USER_QUESTION_REQUESTED,
            Self::TaskUpdated(_) => methods::TASK_UPDATED,
            Self::PlanUpdated(_) => methods::PLAN_UPDATED,
            Self::TaskOutputDelta(_) => methods::TASK_OUTPUT_DELTA,
            Self::ProgressUpdated(_) => methods::PROGRESS_UPDATED,
            Self::Warning(_) => methods::WARNING,
            Self::TurnCompleted(_) => methods::TURN_COMPLETED,
            Self::TurnError(_) => methods::TURN_ERROR,
            Self::ReplayLossy(_) => methods::REPLAY_LOSSY,
            Self::TurnSpawnComplete(_) => methods::TURN_SPAWN_COMPLETE,
            Self::FileAttached(_) => methods::FILE_ATTACHED,
            Self::VoiceAudioChunk(_) => methods::VOICE_AUDIO_CHUNK,
            Self::SessionEventBridged(_) => methods::SESSION_EVENT,
            Self::RouterStatus(_) => methods::ROUTER_STATUS,
            Self::RouterFailover(_) => methods::ROUTER_FAILOVER,
            Self::QueueState(_) => methods::QUEUE_STATE,
            Self::AgentUpdated(_) => methods::AGENT_UPDATED,
            Self::AgentOutputDelta(_) => methods::AGENT_OUTPUT_DELTA,
            Self::AgentArtifactUpdated(_) => methods::AGENT_ARTIFACT_UPDATED,
            Self::SessionGoalUpdated(_) => methods::SESSION_GOAL_UPDATED,
            Self::SessionGoalCleared(_) => methods::SESSION_GOAL_CLEARED,
            Self::LoopUpdated(_) => methods::LOOP_UPDATED,
            Self::LoopFired(_) => methods::LOOP_FIRED,
            Self::LoopCompleted(_) => methods::LOOP_COMPLETED,
            Self::ContextCompactionCompleted(_) => methods::CONTEXT_COMPACTION_COMPLETED,
            Self::ContextCompactionStarted(_) => methods::CONTEXT_COMPACTION_STARTED,
            Self::ContextNormalizationReported(_) => methods::CONTEXT_NORMALIZATION_REPORTED,
            Self::SessionOrchestration(_) => methods::SESSION_ORCHESTRATION,
            Self::PeerStaged(_) => methods::PEER_STAGED,
            Self::PeerClosed(_) => methods::PEER_CLOSED,
            Self::Envelope(_) => methods::PROJECTION_ENVELOPE,
            Self::EnvelopeV2(_) => methods::PROJECTION_ENVELOPE,
        }
    }

    pub fn session_id(&self) -> &SessionKey {
        match self {
            Self::SessionOpened(event) => &event.session_id,
            Self::TurnStarted(event) => &event.session_id,
            Self::MessageDelta(event) => &event.session_id,
            Self::ReasoningDelta(event) => &event.session_id,
            Self::VisualGenerating(event) => &event.session_id,
            Self::VisualSucceeded(event) => &event.session_id,
            Self::VisualFailed(event) => &event.session_id,
            Self::VoiceExit(event) => &event.session_id,
            Self::ToolStarted(event) => &event.session_id,
            Self::ToolProgress(event) => &event.session_id,
            Self::ToolCompleted(event) => &event.session_id,
            Self::ApprovalRequested(event) => &event.session_id,
            Self::ApprovalAutoResolved(event) => &event.session_id,
            Self::ApprovalDecided(event) => &event.session_id,
            Self::ApprovalCancelled(event) => &event.session_id,
            Self::UserQuestionRequested(event) => &event.session_id,
            Self::TaskUpdated(event) => &event.session_id,
            Self::PlanUpdated(event) => &event.session_id,
            Self::TaskOutputDelta(event) => &event.session_id,
            Self::ProgressUpdated(event) => &event.session_id,
            Self::Warning(event) => &event.session_id,
            Self::TurnCompleted(event) => &event.session_id,
            Self::TurnError(event) => &event.session_id,
            Self::ReplayLossy(event) => &event.session_id,
            Self::TurnSpawnComplete(event) => &event.session_id,
            Self::FileAttached(event) => &event.session_id,
            Self::VoiceAudioChunk(event) => &event.session_id,
            Self::SessionEventBridged(event) => &event.session_id,
            Self::RouterStatus(event) => &event.session_id,
            Self::RouterFailover(event) => &event.session_id,
            Self::QueueState(event) => &event.session_id,
            Self::AgentUpdated(event) => &event.session_id,
            Self::AgentOutputDelta(event) => &event.session_id,
            Self::AgentArtifactUpdated(event) => &event.session_id,
            Self::SessionGoalUpdated(event) => &event.session_id,
            Self::SessionGoalCleared(event) => &event.session_id,
            Self::LoopUpdated(event) => &event.session_id,
            Self::LoopFired(event) => &event.session_id,
            Self::LoopCompleted(event) => &event.session_id,
            Self::ContextCompactionCompleted(event) => &event.session_id,
            Self::ContextCompactionStarted(event) => &event.session_id,
            Self::ContextNormalizationReported(event) => &event.session_id,
            Self::SessionOrchestration(event) => &event.session_id,
            Self::PeerStaged(event) => &event.session_id,
            Self::PeerClosed(event) => &event.session_id,
            Self::Envelope(event) => &event.session_id,
            Self::EnvelopeV2(event) => &event.session_id,
        }
    }

    pub fn topic(&self) -> Option<&str> {
        match self {
            Self::TurnStarted(event) => event.topic.as_deref().or_else(|| event.session_id.topic()),
            Self::MessageDelta(event) => {
                event.topic.as_deref().or_else(|| event.session_id.topic())
            }
            Self::ReasoningDelta(event) => {
                event.topic.as_deref().or_else(|| event.session_id.topic())
            }
            Self::VisualGenerating(event) => {
                event.topic.as_deref().or_else(|| event.session_id.topic())
            }
            Self::VisualSucceeded(event) => {
                event.topic.as_deref().or_else(|| event.session_id.topic())
            }
            Self::VoiceExit(event) => event.topic.as_deref().or_else(|| event.session_id.topic()),
            Self::VisualFailed(event) => {
                event.topic.as_deref().or_else(|| event.session_id.topic())
            }
            Self::ToolStarted(event) => event.topic.as_deref().or_else(|| event.session_id.topic()),
            Self::ToolProgress(event) => {
                event.topic.as_deref().or_else(|| event.session_id.topic())
            }
            Self::ToolCompleted(event) => {
                event.topic.as_deref().or_else(|| event.session_id.topic())
            }
            Self::ApprovalRequested(event) => {
                event.topic.as_deref().or_else(|| event.session_id.topic())
            }
            Self::ApprovalAutoResolved(event) => {
                event.topic.as_deref().or_else(|| event.session_id.topic())
            }
            Self::ApprovalDecided(event) => {
                event.topic.as_deref().or_else(|| event.session_id.topic())
            }
            Self::ApprovalCancelled(event) => {
                event.topic.as_deref().or_else(|| event.session_id.topic())
            }
            Self::UserQuestionRequested(event) => {
                event.topic.as_deref().or_else(|| event.session_id.topic())
            }
            Self::TaskUpdated(event) => event.topic.as_deref().or_else(|| event.session_id.topic()),
            Self::PlanUpdated(event) => event.topic.as_deref().or_else(|| event.session_id.topic()),
            Self::TaskOutputDelta(event) => {
                event.topic.as_deref().or_else(|| event.session_id.topic())
            }
            Self::TurnCompleted(event) => {
                event.topic.as_deref().or_else(|| event.session_id.topic())
            }
            Self::TurnError(event) => event.topic.as_deref().or_else(|| event.session_id.topic()),
            Self::TurnSpawnComplete(event) => {
                event.topic.as_deref().or_else(|| event.session_id.topic())
            }
            Self::FileAttached(event) => {
                event.topic.as_deref().or_else(|| event.session_id.topic())
            }
            Self::VoiceAudioChunk(event) => {
                event.topic.as_deref().or_else(|| event.session_id.topic())
            }
            Self::SessionEventBridged(event) => {
                event.topic.as_deref().or_else(|| event.session_id.topic())
            }
            Self::Envelope(event) => event.topic.as_deref().or_else(|| event.session_id.topic()),
            Self::EnvelopeV2(event) => event.topic.as_deref().or_else(|| event.session_id.topic()),
            _ => self.session_id().topic(),
        }
    }

    pub fn stamp_topic_from_session(&mut self) {
        let Some(topic) = self.session_id().topic().map(ToOwned::to_owned) else {
            return;
        };
        match self {
            Self::TurnStarted(event) => set_topic_if_absent(&mut event.topic, &topic),
            Self::MessageDelta(event) => set_topic_if_absent(&mut event.topic, &topic),
            Self::ReasoningDelta(event) => set_topic_if_absent(&mut event.topic, &topic),
            Self::VisualGenerating(event) => set_topic_if_absent(&mut event.topic, &topic),
            Self::VisualSucceeded(event) => set_topic_if_absent(&mut event.topic, &topic),
            Self::VisualFailed(event) => set_topic_if_absent(&mut event.topic, &topic),
            Self::VoiceExit(event) => set_topic_if_absent(&mut event.topic, &topic),
            Self::ToolStarted(event) => set_topic_if_absent(&mut event.topic, &topic),
            Self::ToolProgress(event) => set_topic_if_absent(&mut event.topic, &topic),
            Self::ToolCompleted(event) => set_topic_if_absent(&mut event.topic, &topic),
            Self::ApprovalRequested(event) => set_topic_if_absent(&mut event.topic, &topic),
            Self::ApprovalAutoResolved(event) => set_topic_if_absent(&mut event.topic, &topic),
            Self::ApprovalDecided(event) => set_topic_if_absent(&mut event.topic, &topic),
            Self::ApprovalCancelled(event) => set_topic_if_absent(&mut event.topic, &topic),
            Self::UserQuestionRequested(event) => set_topic_if_absent(&mut event.topic, &topic),
            Self::TaskUpdated(event) => set_topic_if_absent(&mut event.topic, &topic),
            Self::PlanUpdated(event) => set_topic_if_absent(&mut event.topic, &topic),
            Self::TaskOutputDelta(event) => set_topic_if_absent(&mut event.topic, &topic),
            Self::TurnCompleted(event) => set_topic_if_absent(&mut event.topic, &topic),
            Self::TurnError(event) => set_topic_if_absent(&mut event.topic, &topic),
            Self::TurnSpawnComplete(event) => set_topic_if_absent(&mut event.topic, &topic),
            Self::FileAttached(event) => set_topic_if_absent(&mut event.topic, &topic),
            Self::VoiceAudioChunk(event) => set_topic_if_absent(&mut event.topic, &topic),
            Self::SessionEventBridged(event) => set_topic_if_absent(&mut event.topic, &topic),
            Self::Envelope(event) => set_topic_if_absent(&mut event.topic, &topic),
            Self::EnvelopeV2(event) => set_topic_if_absent(&mut event.topic, &topic),
            _ => {}
        }
    }

    pub fn into_rpc_notification(mut self) -> Result<RpcNotification<Value>, serde_json::Error> {
        self.stamp_topic_from_session();
        let method = self.method();
        let params = match self {
            Self::SessionOpened(params) => serde_json::to_value(params),
            Self::TurnStarted(params) => serde_json::to_value(params),
            Self::MessageDelta(params) => serde_json::to_value(params),
            Self::ReasoningDelta(params) => serde_json::to_value(params),
            Self::VisualGenerating(params) => serde_json::to_value(params),
            Self::VisualSucceeded(params) => serde_json::to_value(params),
            Self::VisualFailed(params) => serde_json::to_value(params),
            Self::VoiceExit(params) => serde_json::to_value(params),
            Self::ToolStarted(params) => serde_json::to_value(params),
            Self::ToolProgress(params) => serde_json::to_value(params),
            Self::ToolCompleted(params) => serde_json::to_value(params),
            Self::ApprovalRequested(params) => serde_json::to_value(params),
            Self::ApprovalAutoResolved(params) => serde_json::to_value(params),
            Self::ApprovalDecided(params) => serde_json::to_value(params),
            Self::ApprovalCancelled(params) => serde_json::to_value(params),
            Self::UserQuestionRequested(params) => serde_json::to_value(params),
            Self::TaskUpdated(params) => serde_json::to_value(params),
            Self::PlanUpdated(params) => serde_json::to_value(params),
            Self::TaskOutputDelta(params) => serde_json::to_value(params),
            Self::ProgressUpdated(params) => serde_json::to_value(params),
            Self::Warning(params) => serde_json::to_value(params),
            Self::TurnCompleted(params) => serde_json::to_value(params),
            Self::TurnError(params) => serde_json::to_value(params),
            Self::ReplayLossy(params) => serde_json::to_value(params),
            Self::TurnSpawnComplete(params) => serde_json::to_value(params),
            Self::FileAttached(params) => serde_json::to_value(params),
            Self::VoiceAudioChunk(params) => serde_json::to_value(params),
            Self::SessionEventBridged(params) => serde_json::to_value(params),
            Self::RouterStatus(params) => serde_json::to_value(params),
            Self::RouterFailover(params) => serde_json::to_value(params),
            Self::QueueState(params) => serde_json::to_value(params),
            Self::AgentUpdated(params) => serde_json::to_value(params),
            Self::AgentOutputDelta(params) => serde_json::to_value(params),
            Self::AgentArtifactUpdated(params) => serde_json::to_value(params),
            Self::SessionGoalUpdated(params) => serde_json::to_value(params),
            Self::SessionGoalCleared(params) => serde_json::to_value(params),
            Self::LoopUpdated(params) => serde_json::to_value(params),
            Self::LoopFired(params) => serde_json::to_value(params),
            Self::LoopCompleted(params) => serde_json::to_value(params),
            Self::ContextCompactionCompleted(params) => serde_json::to_value(params),
            Self::ContextCompactionStarted(params) => serde_json::to_value(params),
            Self::ContextNormalizationReported(params) => serde_json::to_value(params),
            Self::SessionOrchestration(params) => serde_json::to_value(params),
            // #1801 v3: `topic` on the payload is the staged PEER's topic
            // (`peer-<slug>`), NOT this notification's routing topic — the
            // `stamp_topic_from_session` catch-all above leaves it alone,
            // and routing keys off `session_id` (the originating session).
            Self::PeerStaged(params) => serde_json::to_value(params),
            Self::PeerClosed(params) => serde_json::to_value(params),
            // UPCR-2026-014 (M9-γ) + feat(envelope-wire-routing): the wire
            // shape per spec § 14.1 is the bare `Envelope` fields FLATTENED
            // with the routing keys `session_id` (the bare base key) +
            // optional `topic`, i.e. `{ thread_id, seq, client_message_id?,
            // payload, session_id, topic? }`. A multi-session client routes
            // on `session_id`; a topic-scoped pane routes on `topic`.
            //
            // The flatten keeps the bare Envelope keys at the TOP level so
            // an older/tolerant client (e.g. the octos-web bridge) that
            // reads `thread_id`/`seq`/`payload` top-level and ignores
            // unknown keys decodes it unchanged. The matching decoder in
            // `from_method_and_params` accepts an OLD frame lacking
            // `session_id` (defaults to empty / `None`).
            //
            // Codex #1336 round-2 BLOCKER 4 required that the DURABLE
            // LEDGER preserve routing on disk: that invariant is unchanged
            // — the disk path uses the derive-based `Serialize` on
            // `EnvelopeNotification` (nested `{ session_id, topic,
            // envelope }`), which this wire DTO does NOT touch. Only the
            // WIRE is un-stripped here.
            //
            // codex BLOCKER (feat(envelope-wire-routing)): on a TOPIC
            // turn, `turn/start` folds the topic into `session_id` as
            // `"base#topic"` and that composite key is carried forward
            // into the emitted `EnvelopeNotification.session_id`. A client
            // only knows the bare base key, so a `"base#topic"` wire key
            // misroutes the message and defeats orphan-chip self-heal —
            // and it contradicts the spec text above (wire = bare base key
            // + separate topic). Normalize the wire `session_id` to the
            // base key here (wire boundary ONLY; the disk derive keeps
            // `"base#topic"`), and keep the topic — recovering it from the
            // suffix when the explicit `topic` field is empty so it is
            // never lost.
            Self::Envelope(params) => serde_json::to_value(&EnvelopeWire {
                session_id: SessionKey(params.session_id.base_key().to_owned()),
                topic: params
                    .topic
                    .clone()
                    .or_else(|| params.session_id.topic().map(str::to_owned)),
                envelope: params.envelope,
            }),
            // Stage 1 v2 deliberately reuses the same flattened method
            // boundary as v1. The capability selects the contract; `turn_id`
            // makes the two wire DTOs unambiguous on decode.
            Self::EnvelopeV2(params) => serde_json::to_value(&EnvelopeWireV2 {
                session_id: SessionKey(params.session_id.base_key().to_owned()),
                topic: params
                    .topic
                    .clone()
                    .or_else(|| params.session_id.topic().map(str::to_owned)),
                envelope: params.envelope,
            }),
        }?;

        Ok(RpcNotification::new(method, params))
    }

    pub fn from_rpc_notification(notification: RpcNotification<Value>) -> Result<Self, RpcError> {
        let RpcNotification {
            jsonrpc,
            method,
            params,
        } = notification;

        validate_jsonrpc_version(&jsonrpc)?;
        Self::from_method_and_params(&method, params)
    }

    pub fn from_method_and_params(method: &str, params: Value) -> Result<Self, RpcError> {
        match method {
            methods::SESSION_OPEN => Ok(Self::SessionOpened(decode_params(method, params)?)),
            methods::TURN_STARTED => Ok(Self::TurnStarted(decode_params(method, params)?)),
            methods::MESSAGE_DELTA => Ok(Self::MessageDelta(decode_params(method, params)?)),
            methods::MESSAGE_REASONING_DELTA => {
                Ok(Self::ReasoningDelta(decode_params(method, params)?))
            }
            methods::VISUAL_GENERATING => {
                Ok(Self::VisualGenerating(decode_params(method, params)?))
            }
            methods::VISUAL_SUCCEEDED => Ok(Self::VisualSucceeded(decode_params(method, params)?)),
            methods::VISUAL_FAILED => Ok(Self::VisualFailed(decode_params(method, params)?)),
            methods::VOICE_EXIT => Ok(Self::VoiceExit(decode_params(method, params)?)),
            methods::TOOL_STARTED => Ok(Self::ToolStarted(decode_params(method, params)?)),
            methods::TOOL_PROGRESS => Ok(Self::ToolProgress(decode_params(method, params)?)),
            methods::TOOL_COMPLETED => Ok(Self::ToolCompleted(decode_params(method, params)?)),
            methods::APPROVAL_REQUESTED => {
                Ok(Self::ApprovalRequested(decode_params(method, params)?))
            }
            methods::APPROVAL_AUTO_RESOLVED => {
                Ok(Self::ApprovalAutoResolved(decode_params(method, params)?))
            }
            methods::APPROVAL_DECIDED => Ok(Self::ApprovalDecided(decode_params(method, params)?)),
            methods::APPROVAL_CANCELLED => {
                Ok(Self::ApprovalCancelled(decode_params(method, params)?))
            }
            methods::USER_QUESTION_REQUESTED => {
                Ok(Self::UserQuestionRequested(decode_params(method, params)?))
            }
            methods::TASK_UPDATED => Ok(Self::TaskUpdated(decode_params(method, params)?)),
            methods::PLAN_UPDATED => Ok(Self::PlanUpdated(decode_params(method, params)?)),
            methods::TASK_OUTPUT_DELTA => Ok(Self::TaskOutputDelta(decode_params(method, params)?)),
            methods::PROGRESS_UPDATED => Ok(Self::ProgressUpdated(decode_params(method, params)?)),
            methods::WARNING => Ok(Self::Warning(decode_params(method, params)?)),
            methods::TURN_COMPLETED => Ok(Self::TurnCompleted(decode_params(method, params)?)),
            methods::TURN_ERROR => Ok(Self::TurnError(decode_params(method, params)?)),
            methods::REPLAY_LOSSY => Ok(Self::ReplayLossy(decode_params(method, params)?)),
            methods::TURN_SPAWN_COMPLETE => {
                Ok(Self::TurnSpawnComplete(decode_params(method, params)?))
            }
            methods::FILE_ATTACHED => Ok(Self::FileAttached(decode_params(method, params)?)),
            methods::VOICE_AUDIO_CHUNK => Ok(Self::VoiceAudioChunk(decode_params(method, params)?)),
            methods::SESSION_EVENT => Ok(Self::SessionEventBridged(decode_params(method, params)?)),
            methods::ROUTER_STATUS => Ok(Self::RouterStatus(decode_params(method, params)?)),
            methods::ROUTER_FAILOVER => Ok(Self::RouterFailover(decode_params(method, params)?)),
            methods::QUEUE_STATE => Ok(Self::QueueState(decode_params(method, params)?)),
            methods::AGENT_UPDATED => Ok(Self::AgentUpdated(decode_params(method, params)?)),
            methods::AGENT_OUTPUT_DELTA => {
                Ok(Self::AgentOutputDelta(decode_params(method, params)?))
            }
            methods::AGENT_ARTIFACT_UPDATED => {
                Ok(Self::AgentArtifactUpdated(decode_params(method, params)?))
            }
            methods::SESSION_GOAL_UPDATED => {
                Ok(Self::SessionGoalUpdated(decode_params(method, params)?))
            }
            methods::SESSION_GOAL_CLEARED => {
                Ok(Self::SessionGoalCleared(decode_params(method, params)?))
            }
            methods::LOOP_UPDATED => Ok(Self::LoopUpdated(decode_params(method, params)?)),
            methods::LOOP_FIRED => Ok(Self::LoopFired(decode_params(method, params)?)),
            methods::LOOP_COMPLETED => Ok(Self::LoopCompleted(decode_params(method, params)?)),
            methods::CONTEXT_COMPACTION_STARTED => Ok(Self::ContextCompactionStarted(
                decode_params(method, params)?,
            )),
            methods::CONTEXT_COMPACTION_COMPLETED => Ok(Self::ContextCompactionCompleted(
                decode_params(method, params)?,
            )),
            methods::CONTEXT_NORMALIZATION_REPORTED => Ok(Self::ContextNormalizationReported(
                decode_params(method, params)?,
            )),
            methods::SESSION_ORCHESTRATION => {
                Ok(Self::SessionOrchestration(decode_params(method, params)?))
            }
            methods::PEER_STAGED => Ok(Self::PeerStaged(decode_params(method, params)?)),
            methods::PEER_CLOSED => Ok(Self::PeerClosed(decode_params(method, params)?)),
            // UPCR-2026-014 (M9-γ) + feat(envelope-wire-routing): decode
            // the FLATTENED wire frame — bare Envelope keys plus the
            // routing keys `session_id` + `topic`. Backward-compatible:
            // an OLD bare-envelope frame that omits `session_id` /
            // `topic` decodes with `session_id` defaulting to the empty
            // `SessionKey` and `topic` to `None` (the `#[serde(default)]`
            // on `EnvelopeWire`), so a legacy producer never errors here.
            // A multi-session consumer routes on the recovered
            // `session_id`; for a legacy empty key it falls back to its
            // ambient connection context.
            methods::PROJECTION_ENVELOPE => {
                if params.get("turn_id").is_some() {
                    let wire: EnvelopeWireV2 = decode_params(method, params)?;
                    Ok(Self::EnvelopeV2(EnvelopeV2Notification {
                        session_id: wire.session_id,
                        topic: wire.topic,
                        envelope: wire.envelope,
                    }))
                } else {
                    let wire: EnvelopeWire = decode_params(method, params)?;
                    Ok(Self::Envelope(EnvelopeNotification {
                        session_id: wire.session_id,
                        topic: wire.topic,
                        envelope: wire.envelope,
                    }))
                }
            }
            _ => Err(RpcError::method_not_found(method)),
        }
    }
}

#[cfg(test)]
#[path = "ui_protocol_tests.rs"]
mod tests;
