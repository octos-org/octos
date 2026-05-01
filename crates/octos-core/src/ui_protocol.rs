//! Draft client/runtime protocol types for M9.
//!
//! This module intentionally captures only the first protocol slice needed to
//! align client and server work. A first WebSocket server slice now handles
//! session open, turn start, turn interrupt, approval, diff preview, and
//! task-output read requests. The full protocol model also defines harness
//! task-control requests so clients can target a stable AppUI contract while
//! backend support lands behind capabilities.

use crate::{SessionKey, TaskId};
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

/// Feature flag for UPCR-2026-001 typed approval payloads.
pub const UI_PROTOCOL_FEATURE_APPROVAL_TYPED_V1: &str = "approval.typed.v1";

/// Feature flag for UPCR-2026-002 pane snapshot payloads.
pub const UI_PROTOCOL_FEATURE_PANE_SNAPSHOTS_V1: &str = "pane.snapshots.v1";

/// Feature flag for UPCR-2026-003 per-session workspace cwd requests.
pub const UI_PROTOCOL_FEATURE_SESSION_WORKSPACE_CWD_V1: &str = "session.workspace_cwd.v1";

/// Feature flag for harness task registry/control commands.
pub const UI_PROTOCOL_FEATURE_HARNESS_TASK_CONTROL_V1: &str = "harness.task_control.v1";

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
    pub const SESSION_OPEN: &str = "session/open";
    pub const TURN_START: &str = "turn/start";
    pub const TURN_INTERRUPT: &str = "turn/interrupt";
    pub const APPROVAL_RESPOND: &str = "approval/respond";
    pub const APPROVAL_SCOPES_LIST: &str = "approval/scopes/list";
    pub const DIFF_PREVIEW_GET: &str = "diff/preview/get";
    pub const TASK_LIST: &str = "task/list";
    pub const TASK_CANCEL: &str = "task/cancel";
    pub const TASK_RESTART_FROM_NODE: &str = "task/restart_from_node";
    pub const TASK_OUTPUT_READ: &str = "task/output/read";

    pub const TURN_STARTED: &str = "turn/started";
    pub const TURN_COMPLETED: &str = "turn/completed";
    pub const TURN_ERROR: &str = "turn/error";
    pub const MESSAGE_DELTA: &str = "message/delta";
    pub const TOOL_STARTED: &str = "tool/started";
    pub const TOOL_PROGRESS: &str = "tool/progress";
    pub const TOOL_COMPLETED: &str = "tool/completed";
    pub const APPROVAL_REQUESTED: &str = "approval/requested";
    pub const APPROVAL_AUTO_RESOLVED: &str = "approval/auto_resolved";
    pub const APPROVAL_DECIDED: &str = "approval/decided";
    pub const APPROVAL_CANCELLED: &str = "approval/cancelled";
    pub const TASK_UPDATED: &str = "task/updated";
    pub const TASK_OUTPUT_DELTA: &str = "task/output/delta";
    pub const PROGRESS_UPDATED: &str = "progress/updated";
    pub const WARNING: &str = "warning";
    /// Notifies the client that one or more durable notifications were dropped due
    /// to per-connection backpressure. The client should diverge the cursor and
    /// rehydrate via `session/open` (or REST). Carries the last known durable
    /// cursor so the client can resume cleanly.
    pub const REPLAY_LOSSY: &str = "protocol/replay_lossy";
}

/// Reason codes for `approval/cancelled` notifications. The registry is
/// open: clients should treat unknown reasons as an opaque string and may
/// add new entries as future drains land (e.g. `session_closed`).
pub mod approval_cancelled_reasons {
    pub const TURN_INTERRUPTED: &str = "turn_interrupted";
}

/// All command methods defined by the v1alpha1 protocol model.
pub const UI_PROTOCOL_COMMAND_METHODS: &[&str] = &[
    methods::SESSION_OPEN,
    methods::TURN_START,
    methods::TURN_INTERRUPT,
    methods::APPROVAL_RESPOND,
    methods::APPROVAL_SCOPES_LIST,
    methods::DIFF_PREVIEW_GET,
    methods::TASK_LIST,
    methods::TASK_CANCEL,
    methods::TASK_RESTART_FROM_NODE,
    methods::TASK_OUTPUT_READ,
];

/// Notification methods defined by the v1alpha1 protocol model.
pub const UI_PROTOCOL_NOTIFICATION_METHODS: &[&str] = &[
    methods::SESSION_OPEN,
    methods::TURN_STARTED,
    methods::TURN_COMPLETED,
    methods::TURN_ERROR,
    methods::MESSAGE_DELTA,
    methods::TOOL_STARTED,
    methods::TOOL_PROGRESS,
    methods::TOOL_COMPLETED,
    methods::APPROVAL_REQUESTED,
    methods::APPROVAL_AUTO_RESOLVED,
    methods::APPROVAL_DECIDED,
    methods::APPROVAL_CANCELLED,
    methods::TASK_UPDATED,
    methods::TASK_OUTPUT_DELTA,
    methods::PROGRESS_UPDATED,
    methods::WARNING,
    methods::REPLAY_LOSSY,
];

/// Request methods currently handled by the first server/runtime slice.
pub const UI_PROTOCOL_FIRST_SERVER_METHODS: &[&str] = &[
    methods::SESSION_OPEN,
    methods::TURN_START,
    methods::TURN_INTERRUPT,
    methods::APPROVAL_RESPOND,
    methods::APPROVAL_SCOPES_LIST,
    methods::DIFF_PREVIEW_GET,
    methods::TASK_LIST,
    methods::TASK_CANCEL,
    methods::TASK_RESTART_FROM_NODE,
    methods::TASK_OUTPUT_READ,
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
            UI_PROTOCOL_FEATURE_HARNESS_TASK_CONTROL_V1,
        ])
    }

    pub fn first_server_slice() -> Self {
        let mut capabilities = Self::new(
            UI_PROTOCOL_FIRST_SERVER_METHODS,
            UI_PROTOCOL_NOTIFICATION_METHODS,
        )
        .with_supported_features([
            UI_PROTOCOL_FEATURE_APPROVAL_TYPED_V1,
            UI_PROTOCOL_FEATURE_PANE_SNAPSHOTS_V1,
            UI_PROTOCOL_FEATURE_SESSION_WORKSPACE_CWD_V1,
            UI_PROTOCOL_FEATURE_HARNESS_TASK_CONTROL_V1,
        ]);
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
    SessionOpen,
    TurnStart,
    TurnInterrupt,
    ApprovalRespond,
    ApprovalScopesList,
    DiffPreviewGet,
    TaskList,
    TaskCancel,
    TaskRestartFromNode,
    TaskOutputRead,
    UnsupportedCapability,
}

pub fn first_server_result_kind_for_method(method: &str) -> Option<UiResultKind> {
    match method {
        methods::SESSION_OPEN => Some(UiResultKind::SessionOpen),
        methods::TURN_START => Some(UiResultKind::TurnStart),
        methods::TURN_INTERRUPT => Some(UiResultKind::TurnInterrupt),
        methods::APPROVAL_RESPOND => Some(UiResultKind::ApprovalRespond),
        methods::APPROVAL_SCOPES_LIST => Some(UiResultKind::ApprovalScopesList),
        methods::DIFF_PREVIEW_GET => Some(UiResultKind::DiffPreviewGet),
        methods::TASK_LIST => Some(UiResultKind::TaskList),
        methods::TASK_CANCEL => Some(UiResultKind::TaskCancel),
        methods::TASK_RESTART_FROM_NODE => Some(UiResultKind::TaskRestartFromNode),
        methods::TASK_OUTPUT_READ => Some(UiResultKind::TaskOutputRead),
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<UiCursor>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnStartParams {
    pub session_id: SessionKey,
    pub turn_id: TurnId,
    pub input: Vec<InputItem>,
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
pub struct TaskListEntry {
    pub id: TaskId,
    pub tool_name: String,
    pub tool_call_id: String,
    pub state: TaskRuntimeState,
    pub status: String,
    pub lifecycle_state: String,
    pub runtime_state: String,
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

/// Draft command payloads for UI protocol v1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UiCommand {
    SessionOpen(SessionOpenParams),
    TurnStart(TurnStartParams),
    TurnInterrupt(TurnInterruptParams),
    ApprovalRespond(ApprovalRespondParams),
    ApprovalScopesList(ApprovalScopesListParams),
    DiffPreviewGet(DiffPreviewGetParams),
    TaskList(TaskListParams),
    TaskCancel(TaskCancelParams),
    TaskRestartFromNode(TaskRestartFromNodeParams),
    TaskOutputRead(TaskOutputReadParams),
}

impl UiCommand {
    pub fn method(&self) -> &'static str {
        match self {
            Self::SessionOpen(_) => methods::SESSION_OPEN,
            Self::TurnStart(_) => methods::TURN_START,
            Self::TurnInterrupt(_) => methods::TURN_INTERRUPT,
            Self::ApprovalRespond(_) => methods::APPROVAL_RESPOND,
            Self::ApprovalScopesList(_) => methods::APPROVAL_SCOPES_LIST,
            Self::DiffPreviewGet(_) => methods::DIFF_PREVIEW_GET,
            Self::TaskList(_) => methods::TASK_LIST,
            Self::TaskCancel(_) => methods::TASK_CANCEL,
            Self::TaskRestartFromNode(_) => methods::TASK_RESTART_FROM_NODE,
            Self::TaskOutputRead(_) => methods::TASK_OUTPUT_READ,
        }
    }

    pub fn into_rpc_request(
        self,
        id: impl Into<String>,
    ) -> Result<RpcRequest<Value>, serde_json::Error> {
        let method = self.method();
        let params = match self {
            Self::SessionOpen(params) => serde_json::to_value(params),
            Self::TurnStart(params) => serde_json::to_value(params),
            Self::TurnInterrupt(params) => serde_json::to_value(params),
            Self::ApprovalRespond(params) => serde_json::to_value(params),
            Self::ApprovalScopesList(params) => serde_json::to_value(params),
            Self::DiffPreviewGet(params) => serde_json::to_value(params),
            Self::TaskList(params) => serde_json::to_value(params),
            Self::TaskCancel(params) => serde_json::to_value(params),
            Self::TaskRestartFromNode(params) => serde_json::to_value(params),
            Self::TaskOutputRead(params) => serde_json::to_value(params),
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
            methods::SESSION_OPEN => Ok(Self::SessionOpen(decode_params(method, params)?)),
            methods::TURN_START => Ok(Self::TurnStart(decode_params(method, params)?)),
            methods::TURN_INTERRUPT => Ok(Self::TurnInterrupt(decode_params(method, params)?)),
            methods::APPROVAL_RESPOND => Ok(Self::ApprovalRespond(decode_params(method, params)?)),
            methods::APPROVAL_SCOPES_LIST => {
                Ok(Self::ApprovalScopesList(decode_params(method, params)?))
            }
            methods::DIFF_PREVIEW_GET => Ok(Self::DiffPreviewGet(decode_params(method, params)?)),
            methods::TASK_LIST => Ok(Self::TaskList(decode_params(method, params)?)),
            methods::TASK_CANCEL => Ok(Self::TaskCancel(decode_params(method, params)?)),
            methods::TASK_RESTART_FROM_NODE => {
                Ok(Self::TaskRestartFromNode(decode_params(method, params)?))
            }
            methods::TASK_OUTPUT_READ => Ok(Self::TaskOutputRead(decode_params(method, params)?)),
            _ => Err(RpcError::method_not_found(method)),
        }
    }
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<UiCursor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub panes: Option<UiPaneSnapshot>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnInterruptResult {
    pub interrupted: bool,
}

impl TurnInterruptResult {
    pub fn new(interrupted: bool) -> Self {
        Self { interrupted }
    }
}

/// Typed RPC success results keyed by the originating request method.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(clippy::large_enum_variant)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
pub enum UiRpcResult {
    SessionOpen(SessionOpenResult),
    TurnStart(TurnStartResult),
    TurnInterrupt(TurnInterruptResult),
    ApprovalRespond(ApprovalRespondResult),
    ApprovalScopesList(ApprovalScopesListResult),
    DiffPreviewGet(DiffPreviewGetResult),
    TaskList(TaskListResult),
    TaskCancel(TaskCancelResult),
    TaskRestartFromNode(TaskRestartFromNodeResult),
    TaskOutputRead(TaskOutputReadResult),
    UnsupportedCapability(UnsupportedCapabilityResult),
}

impl UiRpcResult {
    pub fn kind(&self) -> UiResultKind {
        match self {
            Self::SessionOpen(_) => UiResultKind::SessionOpen,
            Self::TurnStart(_) => UiResultKind::TurnStart,
            Self::TurnInterrupt(_) => UiResultKind::TurnInterrupt,
            Self::ApprovalRespond(_) => UiResultKind::ApprovalRespond,
            Self::ApprovalScopesList(_) => UiResultKind::ApprovalScopesList,
            Self::DiffPreviewGet(_) => UiResultKind::DiffPreviewGet,
            Self::TaskList(_) => UiResultKind::TaskList,
            Self::TaskCancel(_) => UiResultKind::TaskCancel,
            Self::TaskRestartFromNode(_) => UiResultKind::TaskRestartFromNode,
            Self::TaskOutputRead(_) => UiResultKind::TaskOutputRead,
            Self::UnsupportedCapability(_) => UiResultKind::UnsupportedCapability,
        }
    }

    pub fn method(&self) -> Option<&str> {
        match self {
            Self::SessionOpen(_) => Some(methods::SESSION_OPEN),
            Self::TurnStart(_) => Some(methods::TURN_START),
            Self::TurnInterrupt(_) => Some(methods::TURN_INTERRUPT),
            Self::ApprovalRespond(_) => Some(methods::APPROVAL_RESPOND),
            Self::ApprovalScopesList(_) => Some(methods::APPROVAL_SCOPES_LIST),
            Self::DiffPreviewGet(_) => Some(methods::DIFF_PREVIEW_GET),
            Self::TaskList(_) => Some(methods::TASK_LIST),
            Self::TaskCancel(_) => Some(methods::TASK_CANCEL),
            Self::TaskRestartFromNode(_) => Some(methods::TASK_RESTART_FROM_NODE),
            Self::TaskOutputRead(_) => Some(methods::TASK_OUTPUT_READ),
            Self::UnsupportedCapability(result) => Some(result.unsupported.method.as_str()),
        }
    }

    pub fn into_result_value(self) -> Result<Value, serde_json::Error> {
        match self {
            Self::SessionOpen(result) => serde_json::to_value(result),
            Self::TurnStart(result) => serde_json::to_value(result),
            Self::TurnInterrupt(result) => serde_json::to_value(result),
            Self::ApprovalRespond(result) => serde_json::to_value(result),
            Self::ApprovalScopesList(result) => serde_json::to_value(result),
            Self::DiffPreviewGet(result) => serde_json::to_value(result),
            Self::TaskList(result) => serde_json::to_value(result),
            Self::TaskCancel(result) => serde_json::to_value(result),
            Self::TaskRestartFromNode(result) => serde_json::to_value(result),
            Self::TaskOutputRead(result) => serde_json::to_value(result),
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
            methods::SESSION_OPEN => Ok(Self::SessionOpen(decode_result(method, result)?)),
            methods::TURN_START => Ok(Self::TurnStart(decode_result(method, result)?)),
            methods::TURN_INTERRUPT => Ok(Self::TurnInterrupt(decode_result(method, result)?)),
            methods::APPROVAL_RESPOND => Ok(Self::ApprovalRespond(decode_result(method, result)?)),
            methods::APPROVAL_SCOPES_LIST => {
                Ok(Self::ApprovalScopesList(decode_result(method, result)?))
            }
            methods::DIFF_PREVIEW_GET => Ok(Self::DiffPreviewGet(decode_result(method, result)?)),
            methods::TASK_LIST => Ok(Self::TaskList(decode_result(method, result)?)),
            methods::TASK_CANCEL => Ok(Self::TaskCancel(decode_result(method, result)?)),
            methods::TASK_RESTART_FROM_NODE => {
                Ok(Self::TaskRestartFromNode(decode_result(method, result)?))
            }
            methods::TASK_OUTPUT_READ => Ok(Self::TaskOutputRead(decode_result(method, result)?)),
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnStartedEvent {
    pub session_id: SessionKey,
    pub turn_id: TurnId,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageDeltaEvent {
    pub session_id: SessionKey,
    pub turn_id: TurnId,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolStartedEvent {
    pub session_id: SessionKey,
    pub turn_id: TurnId,
    pub tool_call_id: String,
    pub tool_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolProgressEvent {
    pub session_id: SessionKey,
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
            approval_id,
            turn_id,
            reason: approval_cancelled_reasons::TURN_INTERRUPTED.to_owned(),
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
    pub task_id: TaskId,
    pub title: String,
    pub state: TaskRuntimeState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime_detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskOutputDeltaEvent {
    pub session_id: SessionKey,
    pub task_id: TaskId,
    pub cursor: OutputCursor,
    pub text: String,
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
    pub turn_id: TurnId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<UiCursor>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnErrorEvent {
    pub session_id: SessionKey,
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

/// Draft notification payloads for UI protocol v1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(clippy::large_enum_variant)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UiNotification {
    SessionOpened(SessionOpened),
    TurnStarted(TurnStartedEvent),
    MessageDelta(MessageDeltaEvent),
    ToolStarted(ToolStartedEvent),
    ToolProgress(ToolProgressEvent),
    ToolCompleted(ToolCompletedEvent),
    ApprovalRequested(ApprovalRequestedEvent),
    ApprovalAutoResolved(ApprovalAutoResolvedEvent),
    ApprovalDecided(ApprovalDecidedEvent),
    ApprovalCancelled(ApprovalCancelledEvent),
    TaskUpdated(TaskUpdatedEvent),
    TaskOutputDelta(TaskOutputDeltaEvent),
    ProgressUpdated(ProgressUpdatedEvent),
    Warning(WarningEvent),
    TurnCompleted(TurnCompletedEvent),
    TurnError(TurnErrorEvent),
    ReplayLossy(ReplayLossyEvent),
}

impl UiNotification {
    pub fn method(&self) -> &'static str {
        match self {
            Self::SessionOpened(_) => methods::SESSION_OPEN,
            Self::TurnStarted(_) => methods::TURN_STARTED,
            Self::MessageDelta(_) => methods::MESSAGE_DELTA,
            Self::ToolStarted(_) => methods::TOOL_STARTED,
            Self::ToolProgress(_) => methods::TOOL_PROGRESS,
            Self::ToolCompleted(_) => methods::TOOL_COMPLETED,
            Self::ApprovalRequested(_) => methods::APPROVAL_REQUESTED,
            Self::ApprovalAutoResolved(_) => methods::APPROVAL_AUTO_RESOLVED,
            Self::ApprovalDecided(_) => methods::APPROVAL_DECIDED,
            Self::ApprovalCancelled(_) => methods::APPROVAL_CANCELLED,
            Self::TaskUpdated(_) => methods::TASK_UPDATED,
            Self::TaskOutputDelta(_) => methods::TASK_OUTPUT_DELTA,
            Self::ProgressUpdated(_) => methods::PROGRESS_UPDATED,
            Self::Warning(_) => methods::WARNING,
            Self::TurnCompleted(_) => methods::TURN_COMPLETED,
            Self::TurnError(_) => methods::TURN_ERROR,
            Self::ReplayLossy(_) => methods::REPLAY_LOSSY,
        }
    }

    pub fn into_rpc_notification(self) -> Result<RpcNotification<Value>, serde_json::Error> {
        let method = self.method();
        let params = match self {
            Self::SessionOpened(params) => serde_json::to_value(params),
            Self::TurnStarted(params) => serde_json::to_value(params),
            Self::MessageDelta(params) => serde_json::to_value(params),
            Self::ToolStarted(params) => serde_json::to_value(params),
            Self::ToolProgress(params) => serde_json::to_value(params),
            Self::ToolCompleted(params) => serde_json::to_value(params),
            Self::ApprovalRequested(params) => serde_json::to_value(params),
            Self::ApprovalAutoResolved(params) => serde_json::to_value(params),
            Self::ApprovalDecided(params) => serde_json::to_value(params),
            Self::ApprovalCancelled(params) => serde_json::to_value(params),
            Self::TaskUpdated(params) => serde_json::to_value(params),
            Self::TaskOutputDelta(params) => serde_json::to_value(params),
            Self::ProgressUpdated(params) => serde_json::to_value(params),
            Self::Warning(params) => serde_json::to_value(params),
            Self::TurnCompleted(params) => serde_json::to_value(params),
            Self::TurnError(params) => serde_json::to_value(params),
            Self::ReplayLossy(params) => serde_json::to_value(params),
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
            methods::TASK_UPDATED => Ok(Self::TaskUpdated(decode_params(method, params)?)),
            methods::TASK_OUTPUT_DELTA => Ok(Self::TaskOutputDelta(decode_params(method, params)?)),
            methods::PROGRESS_UPDATED => Ok(Self::ProgressUpdated(decode_params(method, params)?)),
            methods::WARNING => Ok(Self::Warning(decode_params(method, params)?)),
            methods::TURN_COMPLETED => Ok(Self::TurnCompleted(decode_params(method, params)?)),
            methods::TURN_ERROR => Ok(Self::TurnError(decode_params(method, params)?)),
            methods::REPLAY_LOSSY => Ok(Self::ReplayLossy(decode_params(method, params)?)),
            _ => Err(RpcError::method_not_found(method)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ui_command_method_matches_expected_transport_name() {
        let cmd = UiCommand::TurnInterrupt(TurnInterruptParams {
            session_id: SessionKey("local:demo".into()),
            turn_id: TurnId::new(),
        });

        assert_eq!(cmd.method(), methods::TURN_INTERRUPT);
    }

    #[test]
    fn protocol_version_and_first_server_capabilities_round_trip() {
        let capabilities = UiProtocolCapabilities::first_server_slice();

        assert!(capabilities.version.is_supported_by_current_runtime());
        assert_eq!(
            capabilities.capabilities_schema_version,
            UI_PROTOCOL_CAPABILITIES_SCHEMA_VERSION
        );
        assert!(capabilities.supports_method(methods::SESSION_OPEN));
        assert!(capabilities.supports_method(methods::TURN_START));
        assert!(capabilities.supports_method(methods::TURN_INTERRUPT));
        assert!(capabilities.supports_method(methods::APPROVAL_RESPOND));
        assert!(capabilities.supports_method(methods::DIFF_PREVIEW_GET));
        assert!(capabilities.supports_method(methods::TASK_OUTPUT_READ));
        assert!(capabilities.supports_feature(UI_PROTOCOL_FEATURE_APPROVAL_TYPED_V1));
        assert!(capabilities.supports_feature(UI_PROTOCOL_FEATURE_PANE_SNAPSHOTS_V1));
        assert!(capabilities.supports_feature(UI_PROTOCOL_FEATURE_SESSION_WORKSPACE_CWD_V1));
        assert!(capabilities.supports_feature(UI_PROTOCOL_FEATURE_HARNESS_TASK_CONTROL_V1));
        assert!(capabilities.supports_method(methods::TASK_LIST));
        assert!(capabilities.supports_method(methods::TASK_CANCEL));
        assert!(capabilities.supports_method(methods::TASK_RESTART_FROM_NODE));
        assert!(capabilities.unsupported.is_empty());

        let json = serde_json::to_string(&capabilities).expect("serialize capabilities");
        let decoded: UiProtocolCapabilities =
            serde_json::from_str(&json).expect("deserialize capabilities");

        assert_eq!(decoded, capabilities);
        assert!(
            decoded
                .supported_notifications
                .contains(&methods::SESSION_OPEN.to_owned())
        );
    }

    #[test]
    fn capabilities_accept_absent_supported_features() {
        let legacy = json!({
            "version": UiProtocolVersion::current(),
            "capabilities_schema_version": 1,
            "supported_methods": [methods::SESSION_OPEN],
            "supported_notifications": [methods::SESSION_OPEN]
        });

        let decoded: UiProtocolCapabilities =
            serde_json::from_value(legacy).expect("legacy capabilities decode");

        assert!(decoded.supported_features.is_empty());
        assert!(!decoded.supports_feature(UI_PROTOCOL_FEATURE_APPROVAL_TYPED_V1));
        assert!(!decoded.supports_feature(UI_PROTOCOL_FEATURE_PANE_SNAPSHOTS_V1));
        assert!(!decoded.supports_feature(UI_PROTOCOL_FEATURE_SESSION_WORKSPACE_CWD_V1));
        assert!(!decoded.supports_feature(UI_PROTOCOL_FEATURE_HARNESS_TASK_CONTROL_V1));
    }

    #[test]
    fn full_protocol_capabilities_advertise_harness_task_control() {
        let capabilities = UiProtocolCapabilities::full_protocol();

        assert!(capabilities.supports_method(methods::TASK_LIST));
        assert!(capabilities.supports_method(methods::TASK_CANCEL));
        assert!(capabilities.supports_method(methods::TASK_RESTART_FROM_NODE));
        assert!(capabilities.supports_method(methods::TASK_OUTPUT_READ));
        assert!(capabilities.supports_feature(UI_PROTOCOL_FEATURE_HARNESS_TASK_CONTROL_V1));
        assert!(capabilities.unsupported.is_empty());
    }

    #[test]
    fn session_open_params_cwd_is_additive_and_round_trips() {
        let params = SessionOpenParams {
            session_id: SessionKey("local:demo".into()),
            profile_id: Some("coding".into()),
            cwd: Some("/repo".into()),
            after: None,
        };

        let wire = serde_json::to_value(&params).expect("serialize session/open params");
        assert_eq!(wire["cwd"], json!("/repo"));

        let decoded: SessionOpenParams =
            serde_json::from_value(wire).expect("deserialize session/open params");
        assert_eq!(decoded, params);

        let legacy = json!({
            "session_id": "local:demo",
            "profile_id": "coding"
        });
        let decoded_legacy: SessionOpenParams =
            serde_json::from_value(legacy).expect("legacy session/open params");
        assert!(decoded_legacy.cwd.is_none());
    }

    #[test]
    fn session_opened_pane_snapshot_round_trips() {
        let session_id = SessionKey("local:demo".into());
        let opened = SessionOpened {
            session_id: session_id.clone(),
            active_profile_id: Some("coding".into()),
            workspace_root: Some("/repo".into()),
            cursor: None,
            panes: Some(UiPaneSnapshot {
                session_id: session_id.clone(),
                generated_at: None,
                workspace: Some(UiWorkspacePaneSnapshot {
                    root: "/repo".into(),
                    readable_roots: vec!["/repo".into()],
                    writable_roots: vec!["/repo".into()],
                    contract: vec!["feature pane.snapshots.v1".into()],
                    entries: vec![UiWorkspacePaneEntry {
                        path: "src/lib.rs".into(),
                        label: "lib.rs".into(),
                        depth: 1,
                        kind: "file".into(),
                        detail: Some("12 KB".into()),
                    }],
                    limitations: Vec::new(),
                }),
                artifacts: Some(UiArtifactPaneSnapshot {
                    items: vec![UiArtifactPaneItem {
                        title: "lib.rs".into(),
                        kind: "file".into(),
                        path: Some("src/lib.rs".into()),
                        uri: None,
                        source: Some("workspace".into()),
                        status: "12 KB".into(),
                        source_task_id: None,
                        preview_id: None,
                        size_bytes: Some(12_288),
                        updated_at: None,
                    }],
                    limitations: Vec::new(),
                }),
                git: Some(UiGitPaneSnapshot {
                    repo_root: Some("/repo".into()),
                    branch: Some("coding-green".into()),
                    head: Some("abc1234".into()),
                    clean: false,
                    status: vec![UiGitStatusItem {
                        code: "M".into(),
                        path: "src/lib.rs".into(),
                        detail: "modified".into(),
                    }],
                    history: vec![UiGitHistoryItem {
                        commit: "abc1234".into(),
                        summary: "pane snapshots".into(),
                    }],
                    limitations: Vec::new(),
                }),
                limitations: Vec::new(),
            }),
        };

        let wire = serde_json::to_value(&opened).expect("serialize session/open panes");
        assert_eq!(wire["workspace_root"], json!("/repo"));
        assert_eq!(wire["panes"]["workspace"]["root"], json!("/repo"));
        assert_eq!(
            wire["panes"]["artifacts"]["items"][0]["title"],
            json!("lib.rs")
        );
        assert_eq!(wire["panes"]["git"]["branch"], json!("coding-green"));

        let decoded: SessionOpened =
            serde_json::from_value(wire).expect("deserialize session/open panes");
        assert_eq!(decoded, opened);
    }

    #[test]
    fn ui_protocol_v1_wire_contract_is_golden() {
        assert_eq!(UI_PROTOCOL_V1, "octos-ui/v1alpha1");
        assert_eq!(UI_PROTOCOL_SCHEMA_VERSION, 1);
        assert_eq!(UI_PROTOCOL_CAPABILITIES_SCHEMA_VERSION, 2);
        assert_eq!(JSON_RPC_VERSION, "2.0");
        assert_eq!(UI_PROTOCOL_FEATURE_APPROVAL_TYPED_V1, "approval.typed.v1");
        assert_eq!(UI_PROTOCOL_FEATURE_PANE_SNAPSHOTS_V1, "pane.snapshots.v1");
        assert_eq!(
            UI_PROTOCOL_FEATURE_SESSION_WORKSPACE_CWD_V1,
            "session.workspace_cwd.v1"
        );
        assert_eq!(
            UI_PROTOCOL_FEATURE_HARNESS_TASK_CONTROL_V1,
            "harness.task_control.v1"
        );

        assert_eq!(
            UI_PROTOCOL_COMMAND_METHODS,
            &[
                "session/open",
                "turn/start",
                "turn/interrupt",
                "approval/respond",
                "approval/scopes/list",
                "diff/preview/get",
                "task/list",
                "task/cancel",
                "task/restart_from_node",
                "task/output/read",
            ]
        );
        assert_eq!(
            UI_PROTOCOL_NOTIFICATION_METHODS,
            &[
                "session/open",
                "turn/started",
                "turn/completed",
                "turn/error",
                "message/delta",
                "tool/started",
                "tool/progress",
                "tool/completed",
                "approval/requested",
                "approval/auto_resolved",
                "approval/decided",
                "approval/cancelled",
                "task/updated",
                "task/output/delta",
                "progress/updated",
                "warning",
                "protocol/replay_lossy",
            ]
        );
        assert_eq!(
            UI_PROTOCOL_FIRST_SERVER_METHODS,
            &[
                "session/open",
                "turn/start",
                "turn/interrupt",
                "approval/respond",
                "approval/scopes/list",
                "diff/preview/get",
                "task/list",
                "task/cancel",
                "task/restart_from_node",
                "task/output/read",
            ]
        );
        assert!(UI_PROTOCOL_FIRST_SERVER_UNSUPPORTED_METHODS.is_empty());
    }

    #[test]
    fn ui_protocol_v1_representative_wire_payloads_are_golden() {
        let turn_id = TurnId(Uuid::from_u128(1));
        let approval_id = ApprovalId(Uuid::from_u128(2));
        let preview_id = PreviewId(Uuid::from_u128(3));
        let task_id = TaskId(Uuid::from_u128(4));

        assert_eq!(
            serde_json::to_value(UiProtocolCapabilities::first_server_slice())
                .expect("capabilities json"),
            json!({
                "version": {
                    "protocol": "octos-ui/v1alpha1",
                    "schema_version": 1,
                    "jsonrpc": "2.0"
                },
                "capabilities_schema_version": 2,
                "supported_methods": [
                    "session/open",
                    "turn/start",
                    "turn/interrupt",
                    "approval/respond",
                    "approval/scopes/list",
                    "diff/preview/get",
                    "task/list",
                    "task/cancel",
                    "task/restart_from_node",
                    "task/output/read"
                ],
                "supported_notifications": [
                    "session/open",
                    "turn/started",
                    "turn/completed",
                    "turn/error",
                    "message/delta",
                    "tool/started",
                    "tool/progress",
                    "tool/completed",
                    "approval/requested",
                    "approval/auto_resolved",
                    "approval/decided",
                    "approval/cancelled",
                    "task/updated",
                    "task/output/delta",
                    "progress/updated",
                    "warning",
                    "protocol/replay_lossy"
                ],
                "supported_features": [
                    "approval.typed.v1",
                    "pane.snapshots.v1",
                    "session.workspace_cwd.v1",
                    "harness.task_control.v1"
                ]
            })
        );

        let turn_start = UiCommand::TurnStart(TurnStartParams {
            session_id: SessionKey("local:demo".into()),
            turn_id: turn_id.clone(),
            input: vec![InputItem::Text {
                text: "hello".into(),
            }],
        })
        .into_rpc_request("req-turn-start")
        .expect("serialize turn/start");
        assert_eq!(
            serde_json::to_value(turn_start).expect("turn/start json"),
            json!({
                "jsonrpc": "2.0",
                "id": "req-turn-start",
                "method": "turn/start",
                "params": {
                    "session_id": "local:demo",
                    "turn_id": turn_id,
                    "input": [
                        {
                            "kind": "text",
                            "text": "hello"
                        }
                    ]
                }
            })
        );

        let approval_response = UiCommand::ApprovalRespond(ApprovalRespondParams::new(
            SessionKey("local:demo".into()),
            approval_id.clone(),
            ApprovalDecision::Approve,
        ))
        .into_rpc_request("req-approval")
        .expect("serialize approval/respond");
        assert_eq!(
            serde_json::to_value(approval_response).expect("approval/respond json"),
            json!({
                "jsonrpc": "2.0",
                "id": "req-approval",
                "method": "approval/respond",
                "params": {
                    "session_id": "local:demo",
                    "approval_id": approval_id,
                    "decision": "approve"
                }
            })
        );

        let diff_result = UiRpcResult::DiffPreviewGet(DiffPreviewGetResult {
            status: DiffPreviewGetStatus::Ready,
            source: DiffPreviewSource::PendingStore,
            preview: DiffPreview {
                session_id: SessionKey("local:demo".into()),
                preview_id: preview_id.clone(),
                title: Some("preview".into()),
                files: vec![DiffPreviewFile {
                    path: "src/lib.rs".into(),
                    old_path: None,
                    status: DiffPreviewFileStatus::Modified,
                    hunks: vec![DiffPreviewHunk {
                        header: "@@ -1 +1 @@".into(),
                        lines: vec![
                            DiffPreviewLine {
                                kind: DiffPreviewLineKind::Context,
                                content: "fn demo() {".into(),
                                old_line: Some(1),
                                new_line: Some(1),
                            },
                            DiffPreviewLine {
                                kind: DiffPreviewLineKind::Added,
                                content: "    println!(\"hello\");".into(),
                                old_line: None,
                                new_line: Some(2),
                            },
                        ],
                    }],
                }],
            },
        })
        .into_rpc_response("req-diff")
        .expect("serialize diff result");
        assert_eq!(
            serde_json::to_value(diff_result).expect("diff result json"),
            json!({
                "jsonrpc": "2.0",
                "id": "req-diff",
                "result": {
                    "status": "ready",
                    "source": "pending_store",
                    "preview": {
                        "session_id": "local:demo",
                        "preview_id": preview_id,
                        "title": "preview",
                        "files": [
                            {
                                "path": "src/lib.rs",
                                "status": "modified",
                                "hunks": [
                                    {
                                        "header": "@@ -1 +1 @@",
                                        "lines": [
                                            {
                                                "kind": "context",
                                                "content": "fn demo() {",
                                                "old_line": 1,
                                                "new_line": 1
                                            },
                                            {
                                                "kind": "added",
                                                "content": "    println!(\"hello\");",
                                                "new_line": 2
                                            }
                                        ]
                                    }
                                ]
                            }
                        ]
                    }
                }
            })
        );

        let task_output = UiRpcResult::TaskOutputRead(TaskOutputReadResult {
            session_id: SessionKey("local:demo".into()),
            task_id: task_id.clone(),
            source: TaskOutputReadSource::RuntimeProjection,
            cursor: OutputCursor { offset: 0 },
            next_cursor: OutputCursor { offset: 6 },
            text: "output".into(),
            bytes_read: 6,
            total_bytes: 6,
            truncated: false,
            complete: true,
            live_tail_supported: false,
            is_snapshot_projection: true,
            task_status: "completed".into(),
            runtime_state: "completed".into(),
            lifecycle_state: "completed".into(),
            runtime_detail: None,
            output_files: vec![],
            limitations: vec![TaskOutputReadLimitation {
                code: "snapshot_projection".into(),
                message: "served from task snapshot".into(),
            }],
        })
        .into_rpc_response("req-task")
        .expect("serialize task output result");
        assert_eq!(
            serde_json::to_value(task_output).expect("task output json"),
            json!({
                "jsonrpc": "2.0",
                "id": "req-task",
                "result": {
                    "session_id": "local:demo",
                    "task_id": task_id,
                    "source": "runtime_projection",
                    "cursor": { "offset": 0 },
                    "next_cursor": { "offset": 6 },
                    "text": "output",
                    "bytes_read": 6,
                    "total_bytes": 6,
                    "truncated": false,
                    "complete": true,
                    "live_tail_supported": false,
                    "is_snapshot_projection": true,
                    "task_status": "completed",
                    "runtime_state": "completed",
                    "lifecycle_state": "completed",
                    "limitations": [
                        {
                            "code": "snapshot_projection",
                            "message": "served from task snapshot"
                        }
                    ]
                }
            })
        );

        // M9 review fix MEDIUM #4 (UPCR-2026-004): pin the literal wire form
        // for `task/updated` carrying the new `cancelled` lifecycle state so a
        // future rename of the variant or a serializer regression that drops
        // the snake_case shape is caught by the representative-payload golden
        // gate, not just by the variant-level round-trip tests at the bottom
        // of this module.
        let task_cancelled = UiNotification::TaskUpdated(TaskUpdatedEvent {
            session_id: SessionKey("local:demo".into()),
            task_id: task_id.clone(),
            title: "spawn_only_runner".into(),
            state: TaskRuntimeState::Cancelled,
            runtime_detail: Some("user cancelled".into()),
        })
        .into_rpc_notification()
        .expect("serialize task/updated cancelled");
        assert_eq!(
            serde_json::to_value(task_cancelled).expect("task/updated cancelled json"),
            json!({
                "jsonrpc": "2.0",
                "method": "task/updated",
                "params": {
                    "session_id": "local:demo",
                    "task_id": task_id,
                    "title": "spawn_only_runner",
                    "state": "cancelled",
                    "runtime_detail": "user cancelled"
                }
            })
        );

        let warning = UiNotification::Warning(WarningEvent {
            session_id: SessionKey("local:demo".into()),
            turn_id: Some(turn_id),
            code: "mock_warning".into(),
            message: "mock payload".into(),
        })
        .into_rpc_notification()
        .expect("serialize warning");
        assert_eq!(
            serde_json::to_value(warning).expect("warning json"),
            json!({
                "jsonrpc": "2.0",
                "method": "warning",
                "params": {
                    "session_id": "local:demo",
                    "turn_id": TurnId(Uuid::from_u128(1)),
                    "code": "mock_warning",
                    "message": "mock payload"
                }
            })
        );
    }

    #[test]
    fn generic_and_typed_approval_payloads_round_trip() {
        let session_id = SessionKey("local:demo".into());
        let turn_id = TurnId(Uuid::from_u128(1));
        let approval_id = ApprovalId(Uuid::from_u128(2));

        let generic = ApprovalRequestedEvent::generic(
            session_id.clone(),
            approval_id.clone(),
            turn_id.clone(),
            "shell",
            "Approval requested",
            "Run cargo test?",
        );
        let generic_json = serde_json::to_value(&generic).expect("generic approval json");
        assert!(generic_json.get("approval_kind").is_none());
        assert!(generic_json.get("typed_details").is_none());
        assert_eq!(
            serde_json::from_value::<ApprovalRequestedEvent>(generic_json)
                .expect("generic approval decodes"),
            generic
        );

        let command = ApprovalTypedDetails::command(
            ApprovalCommandDetails {
                argv: vec!["cargo".into(), "test".into()],
                command_line: Some("cargo test".into()),
                cwd: Some("/Users/yuechen/home/octos".into()),
                env_keys: vec!["RUST_LOG".into()],
                tool_call_id: Some("tool-1".into()),
            },
            Some(ApprovalSandboxDetails {
                mode: Some("workspace_write".into()),
                filesystem_access: Some("workspace_write".into()),
                network_access: Some(false),
                writable_roots: vec!["/Users/yuechen/home/octos".into()],
            }),
        );
        assert_typed_approval_round_trips(
            ApprovalRequestedEvent {
                approval_kind: Some(approval_kinds::COMMAND.into()),
                risk: Some("medium".into()),
                typed_details: Some(command),
                render_hints: Some(ApprovalRenderHints {
                    default_decision: Some("deny".into()),
                    primary_label: Some("Approve".into()),
                    secondary_label: Some("Deny".into()),
                    danger: Some(false),
                    monospace_fields: vec![
                        "typed_details.command.command_line".into(),
                        "typed_details.command.cwd".into(),
                    ],
                }),
                ..generic.clone()
            },
            approval_kinds::COMMAND,
        );

        assert_typed_approval_round_trips(
            ApprovalRequestedEvent {
                approval_kind: Some(approval_kinds::DIFF.into()),
                typed_details: Some(ApprovalTypedDetails {
                    kind: approval_kinds::DIFF.into(),
                    command: None,
                    sandbox: None,
                    diff: Some(ApprovalDiffDetails {
                        preview_id: PreviewId(Uuid::from_u128(3)),
                        operation: Some("apply".into()),
                        file_count: Some(2),
                        additions: Some(14),
                        deletions: Some(5),
                        summary: Some("Update approval reducer tests".into()),
                    }),
                    filesystem: None,
                    network: None,
                    sandbox_escalation: None,
                }),
                ..generic.clone()
            },
            approval_kinds::DIFF,
        );

        assert_typed_approval_round_trips(
            ApprovalRequestedEvent {
                approval_kind: Some(approval_kinds::FILESYSTEM.into()),
                typed_details: Some(ApprovalTypedDetails {
                    kind: approval_kinds::FILESYSTEM.into(),
                    command: None,
                    sandbox: None,
                    diff: None,
                    filesystem: Some(ApprovalFilesystemDetails {
                        operation: "write".into(),
                        paths: vec!["docs/example.md".into()],
                        outside_workspace: false,
                        writable_roots: vec!["/Users/yuechen/home/octos".into()],
                    }),
                    network: None,
                    sandbox_escalation: None,
                }),
                ..generic.clone()
            },
            approval_kinds::FILESYSTEM,
        );

        assert_typed_approval_round_trips(
            ApprovalRequestedEvent {
                approval_kind: Some(approval_kinds::NETWORK.into()),
                typed_details: Some(ApprovalTypedDetails {
                    kind: approval_kinds::NETWORK.into(),
                    command: None,
                    sandbox: None,
                    diff: None,
                    filesystem: None,
                    network: Some(ApprovalNetworkDetails {
                        operation: "connect".into(),
                        hosts: vec!["api.openai.com".into()],
                        ports: vec![443],
                        urls: vec!["https://api.openai.com/v1/responses".into()],
                    }),
                    sandbox_escalation: None,
                }),
                ..generic.clone()
            },
            approval_kinds::NETWORK,
        );

        assert_typed_approval_round_trips(
            ApprovalRequestedEvent {
                approval_kind: Some(approval_kinds::SANDBOX_ESCALATION.into()),
                typed_details: Some(ApprovalTypedDetails {
                    kind: approval_kinds::SANDBOX_ESCALATION.into(),
                    command: None,
                    sandbox: None,
                    diff: None,
                    filesystem: None,
                    network: None,
                    sandbox_escalation: Some(ApprovalSandboxEscalationDetails {
                        from: Some(ApprovalSandboxEscalationEndpoint {
                            mode: Some("workspace_write".into()),
                            network_access: Some(false),
                        }),
                        to: Some(ApprovalSandboxEscalationEndpoint {
                            mode: Some("danger_full_access".into()),
                            network_access: Some(true),
                        }),
                        requested_permissions: vec![
                            "filesystem_unrestricted".into(),
                            "network_access".into(),
                        ],
                        justification: Some("Run integration tests".into()),
                        suggested_prefix_rule: vec!["cargo".into(), "test".into()],
                    }),
                }),
                ..generic
            },
            approval_kinds::SANDBOX_ESCALATION,
        );
    }

    fn assert_typed_approval_round_trips(event: ApprovalRequestedEvent, expected_kind: &str) {
        let value = serde_json::to_value(&event).expect("typed approval json");
        assert_eq!(value["approval_kind"], json!(expected_kind));
        assert_eq!(value["typed_details"]["kind"], json!(expected_kind));
        assert_eq!(
            serde_json::from_value::<ApprovalRequestedEvent>(value)
                .expect("typed approval decodes"),
            event
        );
    }

    #[test]
    fn unknown_typed_approval_kind_decodes_for_generic_fallback() {
        let value = json!({
            "session_id": "local:demo",
            "approval_id": ApprovalId(Uuid::from_u128(2)),
            "turn_id": TurnId(Uuid::from_u128(1)),
            "tool_name": "future",
            "title": "Future approval",
            "body": "Fallback body remains actionable",
            "approval_kind": "future_kind",
            "typed_details": {
                "kind": "future_kind"
            }
        });

        let decoded: ApprovalRequestedEvent =
            serde_json::from_value(value).expect("unknown typed approval decodes");

        assert_eq!(decoded.approval_kind.as_deref(), Some("future_kind"));
        assert_eq!(
            decoded
                .typed_details
                .as_ref()
                .map(|details| details.kind.as_str()),
            Some("future_kind")
        );
        assert_eq!(decoded.title, "Future approval");
        assert_eq!(decoded.body, "Fallback body remains actionable");
    }

    #[test]
    fn approval_respond_accepts_legacy_and_typed_metadata() {
        let legacy = json!({
            "session_id": "local:demo",
            "approval_id": ApprovalId(Uuid::from_u128(2)),
            "decision": "approve"
        });
        let legacy: ApprovalRespondParams =
            serde_json::from_value(legacy).expect("legacy approval/respond decodes");
        assert_eq!(legacy.approval_scope, None);
        assert_eq!(legacy.client_note, None);

        let typed = json!({
            "session_id": "local:demo",
            "approval_id": ApprovalId(Uuid::from_u128(2)),
            "decision": "deny",
            "approval_scope": "request",
            "client_note": "Denied for this invocation"
        });
        let typed: ApprovalRespondParams =
            serde_json::from_value(typed).expect("typed approval/respond decodes");
        assert_eq!(
            typed.approval_scope.as_deref(),
            Some(approval_scopes::REQUEST)
        );
        assert_eq!(
            typed.client_note.as_deref(),
            Some("Denied for this invocation")
        );
    }

    #[test]
    fn ui_command_builds_and_parses_json_rpc_request() {
        let command = UiCommand::TurnStart(TurnStartParams {
            session_id: SessionKey("local:demo".into()),
            turn_id: TurnId(Uuid::from_u128(1)),
            input: vec![InputItem::Text {
                text: "hello".into(),
            }],
        });

        let request = command
            .clone()
            .into_rpc_request("req-1")
            .expect("serialize command params");

        assert_eq!(request.jsonrpc, JSON_RPC_VERSION);
        assert_eq!(request.id, "req-1");
        assert_eq!(request.method, methods::TURN_START);

        let wire = serde_json::to_value(&request).expect("serialize request");
        assert_eq!(wire["jsonrpc"], json!(JSON_RPC_VERSION));
        assert_eq!(wire["params"]["session_id"], json!("local:demo"));
        assert_eq!(wire["params"]["input"][0]["kind"], json!("text"));
        assert!(wire["params"].get("kind").is_none());

        let decoded_request: RpcRequest<Value> =
            serde_json::from_value(wire).expect("deserialize request");
        let decoded = UiCommand::from_rpc_request(decoded_request).expect("parse request params");

        assert_eq!(decoded, command);
    }

    #[test]
    fn task_control_commands_build_and_parse_json_rpc_requests() {
        let session_id = SessionKey("local:demo".into());
        let task_id = TaskId(Uuid::from_u128(42));

        let list = UiCommand::TaskList(TaskListParams {
            session_id: session_id.clone(),
            topic: Some("default".into()),
        });
        assert_eq!(list.method(), methods::TASK_LIST);
        let list_wire = list
            .clone()
            .into_rpc_request("task-list")
            .expect("serialize task/list");
        assert_eq!(list_wire.method, methods::TASK_LIST);
        assert_eq!(list_wire.params["session_id"], json!("local:demo"));
        assert_eq!(
            UiCommand::from_rpc_request(list_wire).expect("decode task/list"),
            list
        );

        let cancel = UiCommand::TaskCancel(TaskCancelParams {
            task_id: task_id.clone(),
            session_id: Some(session_id.clone()),
            profile_id: Some("coding".into()),
        });
        assert_eq!(cancel.method(), methods::TASK_CANCEL);
        let cancel_wire = cancel
            .clone()
            .into_rpc_request("task-cancel")
            .expect("serialize task/cancel");
        assert_eq!(cancel_wire.params["task_id"], json!(task_id));
        assert_eq!(cancel_wire.params["profile_id"], json!("coding"));
        assert_eq!(
            UiCommand::from_rpc_request(cancel_wire).expect("decode task/cancel"),
            cancel
        );

        let restart = UiCommand::TaskRestartFromNode(TaskRestartFromNodeParams {
            task_id: TaskId(Uuid::from_u128(43)),
            node_id: Some("node-7".into()),
            session_id: Some(session_id),
            profile_id: None,
        });
        assert_eq!(restart.method(), methods::TASK_RESTART_FROM_NODE);
        let restart_wire = restart
            .clone()
            .into_rpc_request("task-restart")
            .expect("serialize task/restart_from_node");
        assert_eq!(restart_wire.params["node_id"], json!("node-7"));
        assert_eq!(
            UiCommand::from_rpc_request(restart_wire).expect("decode task/restart_from_node"),
            restart
        );
    }

    #[test]
    fn typed_rpc_results_map_from_methods_and_round_trip() {
        let opened = SessionOpened {
            session_id: SessionKey("local:demo".into()),
            active_profile_id: Some("coding".into()),
            workspace_root: None,
            cursor: Some(UiCursor {
                stream: "events".into(),
                seq: 42,
            }),
            panes: None,
        };

        let session_result = UiRpcResult::SessionOpen(SessionOpenResult::new(opened));
        assert_eq!(session_result.kind(), UiResultKind::SessionOpen);
        assert_eq!(session_result.method(), Some(methods::SESSION_OPEN));

        let response = session_result
            .clone()
            .into_rpc_response("open-1")
            .expect("serialize session/open result");
        assert_eq!(response.id, "open-1");
        assert_eq!(response.result["opened"]["session_id"], json!("local:demo"));

        let decoded = UiRpcResult::from_method_and_result(methods::SESSION_OPEN, response.result)
            .expect("decode session/open result");
        assert_eq!(decoded, session_result);

        let turn_start = UiRpcResult::TurnStart(TurnStartResult::accepted());
        let value = turn_start
            .clone()
            .into_result_value()
            .expect("serialize turn/start result");
        assert_eq!(value, json!({ "accepted": true }));
        assert_eq!(
            UiRpcResult::from_method_and_result(methods::TURN_START, value)
                .expect("decode turn/start result"),
            turn_start
        );

        let turn_interrupt = UiRpcResult::TurnInterrupt(TurnInterruptResult::new(false));
        let value = turn_interrupt
            .clone()
            .into_result_value()
            .expect("serialize turn/interrupt result");
        assert_eq!(value, json!({ "interrupted": false }));
        assert_eq!(
            UiRpcResult::from_method_and_result(methods::TURN_INTERRUPT, value)
                .expect("decode turn/interrupt result"),
            turn_interrupt
        );

        let approval_id = ApprovalId::new();
        let approval =
            UiRpcResult::ApprovalRespond(ApprovalRespondResult::accepted(approval_id.clone()));
        assert_eq!(approval.kind(), UiResultKind::ApprovalRespond);
        assert_eq!(approval.method(), Some(methods::APPROVAL_RESPOND));
        let value = approval
            .clone()
            .into_result_value()
            .expect("serialize approval/respond result");
        assert_eq!(value["approval_id"], json!(approval_id));
        assert_eq!(value["status"], json!("accepted"));
        assert_eq!(
            UiRpcResult::from_method_and_result(methods::APPROVAL_RESPOND, value)
                .expect("decode approval/respond result"),
            approval
        );

        let scopes_result = UiRpcResult::ApprovalScopesList(ApprovalScopesListResult {
            scopes: vec![ApprovalScopeEntry {
                session_id: SessionKey("local:demo".into()),
                scope: approval_scopes::SESSION.into(),
                scope_match: "shell".into(),
                decision: ApprovalDecision::Approve,
                turn_id: None,
            }],
        });
        assert_eq!(scopes_result.kind(), UiResultKind::ApprovalScopesList);
        assert_eq!(scopes_result.method(), Some(methods::APPROVAL_SCOPES_LIST));
        let value = scopes_result
            .clone()
            .into_result_value()
            .expect("serialize approval/scopes/list result");
        assert_eq!(value["scopes"][0]["scope"], json!(approval_scopes::SESSION));
        assert_eq!(
            UiRpcResult::from_method_and_result(methods::APPROVAL_SCOPES_LIST, value)
                .expect("decode approval/scopes/list result"),
            scopes_result
        );

        assert_eq!(
            first_server_result_kind_for_method(methods::DIFF_PREVIEW_GET),
            Some(UiResultKind::DiffPreviewGet)
        );
        assert_eq!(
            first_server_result_kind_for_method(methods::TASK_LIST),
            Some(UiResultKind::TaskList)
        );
        assert_eq!(
            first_server_result_kind_for_method(methods::TASK_CANCEL),
            Some(UiResultKind::TaskCancel)
        );
        assert_eq!(
            first_server_result_kind_for_method(methods::TASK_RESTART_FROM_NODE),
            Some(UiResultKind::TaskRestartFromNode)
        );

        let preview_id = PreviewId::new();
        let diff_result = UiRpcResult::DiffPreviewGet(DiffPreviewGetResult {
            status: DiffPreviewGetStatus::Ready,
            source: DiffPreviewSource::PendingStore,
            preview: DiffPreview {
                session_id: SessionKey("local:demo".into()),
                preview_id: preview_id.clone(),
                title: Some("preview".into()),
                files: vec![DiffPreviewFile {
                    path: "src/lib.rs".into(),
                    old_path: None,
                    status: DiffPreviewFileStatus::Modified,
                    hunks: vec![DiffPreviewHunk {
                        header: "@@ -1 +1 @@".into(),
                        lines: vec![DiffPreviewLine {
                            kind: DiffPreviewLineKind::Added,
                            content: "let value = 1;".into(),
                            old_line: None,
                            new_line: Some(1),
                        }],
                    }],
                }],
            },
        });
        let value = diff_result
            .clone()
            .into_result_value()
            .expect("serialize diff/preview/get result");
        assert_eq!(value["status"], json!("ready"));
        assert_eq!(value["preview"]["preview_id"], json!(preview_id));
        assert_eq!(
            UiRpcResult::from_method_and_result(methods::DIFF_PREVIEW_GET, value)
                .expect("decode diff/preview/get result"),
            diff_result
        );

        let started_at = DateTime::parse_from_rfc3339("2026-04-30T12:00:00Z")
            .expect("parse started_at")
            .with_timezone(&Utc);
        let updated_at = DateTime::parse_from_rfc3339("2026-04-30T12:01:00Z")
            .expect("parse updated_at")
            .with_timezone(&Utc);
        let list_task_id = TaskId(Uuid::from_u128(44));
        let task_list = UiRpcResult::TaskList(TaskListResult {
            session_id: SessionKey("local:demo".into()),
            topic: Some("default".into()),
            tasks: vec![TaskListEntry {
                id: list_task_id.clone(),
                tool_name: "spawn_only_runner".into(),
                tool_call_id: "call-1".into(),
                state: TaskRuntimeState::Running,
                status: "running".into(),
                lifecycle_state: "running".into(),
                runtime_state: "executing_tool".into(),
                parent_session_key: Some(SessionKey("local:demo".into())),
                child_session_key: Some(SessionKey("local:demo#child-1".into())),
                child_terminal_state: None,
                child_join_state: None,
                child_joined_at: None,
                child_failure_action: None,
                runtime_detail: Some(json!({ "current_phase": "coding" })),
                workflow_kind: Some("coding".into()),
                current_phase: Some("coding".into()),
                started_at,
                updated_at,
                completed_at: None,
                output_files: vec!["octos-file://task-output".into()],
                error: None,
                session_key: Some(SessionKey("local:demo".into())),
            }],
        });
        assert_eq!(task_list.kind(), UiResultKind::TaskList);
        assert_eq!(task_list.method(), Some(methods::TASK_LIST));
        let value = task_list
            .clone()
            .into_result_value()
            .expect("serialize task/list result");
        assert_eq!(value["tasks"][0]["id"], json!(list_task_id));
        assert_eq!(value["tasks"][0]["state"], json!("running"));
        assert_eq!(
            UiRpcResult::from_method_and_result(methods::TASK_LIST, value)
                .expect("decode task/list result"),
            task_list
        );

        let cancel_result = UiRpcResult::TaskCancel(TaskCancelResult {
            task_id: TaskId(Uuid::from_u128(45)),
            status: TaskRuntimeState::Cancelled,
        });
        assert_eq!(cancel_result.kind(), UiResultKind::TaskCancel);
        assert_eq!(cancel_result.method(), Some(methods::TASK_CANCEL));
        let value = cancel_result
            .clone()
            .into_result_value()
            .expect("serialize task/cancel result");
        assert_eq!(value["status"], json!("cancelled"));
        assert_eq!(
            UiRpcResult::from_method_and_result(methods::TASK_CANCEL, value)
                .expect("decode task/cancel result"),
            cancel_result
        );

        let restart_result = UiRpcResult::TaskRestartFromNode(TaskRestartFromNodeResult {
            original_task_id: TaskId(Uuid::from_u128(46)),
            new_task_id: TaskId(Uuid::from_u128(47)),
            from_node: Some("node-7".into()),
        });
        assert_eq!(restart_result.kind(), UiResultKind::TaskRestartFromNode);
        assert_eq!(
            restart_result.method(),
            Some(methods::TASK_RESTART_FROM_NODE)
        );
        let value = restart_result
            .clone()
            .into_result_value()
            .expect("serialize task/restart_from_node result");
        assert_eq!(value["from_node"], json!("node-7"));
        assert_eq!(
            UiRpcResult::from_method_and_result(methods::TASK_RESTART_FROM_NODE, value)
                .expect("decode task/restart_from_node result"),
            restart_result
        );

        let task_result = UiRpcResult::TaskOutputRead(TaskOutputReadResult {
            session_id: SessionKey("local:demo".into()),
            task_id: TaskId::new(),
            source: TaskOutputReadSource::RuntimeProjection,
            cursor: OutputCursor { offset: 0 },
            next_cursor: OutputCursor { offset: 4 },
            text: "done".into(),
            bytes_read: 4,
            total_bytes: 4,
            truncated: false,
            complete: true,
            live_tail_supported: false,
            is_snapshot_projection: true,
            task_status: "failed".into(),
            runtime_state: "delivering_outputs".into(),
            lifecycle_state: "completed".into(),
            runtime_detail: Some(json!({ "phase": "collecting_output" })),
            output_files: vec!["octos-file://output".into()],
            limitations: vec![TaskOutputReadLimitation {
                code: "live_tail_unavailable".into(),
                message: "task/output/delta is not emitted".into(),
            }],
        });
        let value = task_result
            .clone()
            .into_result_value()
            .expect("serialize task/output/read result");
        assert_eq!(value["source"], json!("runtime_projection"));
        assert_eq!(value["next_cursor"]["offset"], json!(4));
        // Audit issue #707 / accepted UPCR-2026-006: clients must be able to
        // distinguish a snapshot projection read from a (future) live-tail
        // read on the wire, not just by inferring it from `live_tail_supported
        // == false` or the `runtime_projection` source label.
        assert_eq!(value["is_snapshot_projection"], json!(true));
        assert_eq!(
            UiRpcResult::from_method_and_result(methods::TASK_OUTPUT_READ, value)
                .expect("decode task/output/read result"),
            task_result
        );
    }

    #[test]
    fn ui_command_parser_reports_invalid_method_and_params() {
        let unknown = RpcRequest::new("req-1", "turn/unknown", json!({}));
        let err = UiCommand::from_rpc_request(unknown).expect_err("reject unknown method");
        assert_eq!(err.code, rpc_error_codes::METHOD_NOT_FOUND);

        let malformed = RpcRequest::new(
            "req-2",
            methods::TURN_INTERRUPT,
            json!({ "session_id": "local:demo" }),
        );
        let err = UiCommand::from_rpc_request(malformed).expect_err("reject malformed params");
        assert_eq!(err.code, rpc_error_codes::INVALID_PARAMS);
        assert!(err.message.contains(methods::TURN_INTERRUPT));
    }

    #[test]
    fn unsupported_capability_report_is_typed_error_data() {
        let legacy_data = json!({ "method": methods::TASK_OUTPUT_READ });
        let legacy: UnsupportedCapabilityReport =
            serde_json::from_value(legacy_data).expect("deserialize legacy unsupported data");
        assert_eq!(legacy.method, methods::TASK_OUTPUT_READ);
        assert_eq!(legacy.reason, "unsupported by this server");

        let error = RpcError::method_not_supported(methods::DIFF_PREVIEW_GET);
        assert_eq!(error.code, rpc_error_codes::METHOD_NOT_SUPPORTED);
        let data = error.data.expect("unsupported error should carry data");
        let report: UnsupportedCapabilityReport =
            serde_json::from_value(data).expect("deserialize typed unsupported data");
        assert_eq!(report.method, methods::DIFF_PREVIEW_GET);

        let result =
            UnsupportedCapabilityResult::method(methods::APPROVAL_RESPOND, "approval is pending");
        let value = UiRpcResult::UnsupportedCapability(result.clone())
            .into_result_value()
            .expect("serialize unsupported result");
        assert_eq!(
            value["unsupported"]["method"],
            json!(methods::APPROVAL_RESPOND)
        );
        let decoded: UnsupportedCapabilityResult =
            serde_json::from_value(value).expect("deserialize unsupported result");
        assert_eq!(decoded, result);
    }

    #[test]
    fn rich_progress_metadata_round_trips_with_extra_fields() {
        let value = json!({
            "kind": "token_cost_update",
            "message": "usage updated",
            "token_cost": {
                "input_tokens": 12,
                "output_tokens": 7,
                "session_cost": 0.0025,
                "currency": "USD"
            },
            "provider": "openai"
        });

        let metadata: UiProgressMetadata =
            serde_json::from_value(value).expect("deserialize rich progress metadata");

        assert_eq!(metadata.kind, progress_kinds::TOKEN_COST_UPDATE);
        assert_eq!(metadata.message.as_deref(), Some("usage updated"));
        assert_eq!(
            metadata
                .token_cost
                .as_ref()
                .and_then(|cost| cost.input_tokens),
            Some(12)
        );
        assert_eq!(
            metadata.extra.get("provider"),
            Some(&Value::String("openai".into()))
        );

        let encoded = serde_json::to_value(&metadata).expect("serialize rich progress metadata");
        assert_eq!(encoded["provider"], json!("openai"));
        assert_eq!(encoded["token_cost"]["session_cost"], json!(0.0025));
    }

    #[test]
    fn rich_progress_event_uses_standalone_notification_method() {
        let metadata = UiProgressMetadata::file_mutation(UiFileMutationNotice::new(
            "src/main.rs",
            file_mutation_operations::WRITE,
        ));
        let event = UiProgressEvent::new(
            SessionKey("local:demo".into()),
            Some(TurnId(Uuid::from_u128(3))),
            metadata,
        );

        let notification = event
            .clone()
            .into_rpc_notification()
            .expect("serialize progress notification");

        assert_eq!(notification.method, methods::PROGRESS_UPDATED);
        assert_eq!(
            notification.params["metadata"]["kind"],
            json!("file_mutation")
        );
        assert_eq!(
            notification.params["metadata"]["file_mutation"]["operation"],
            json!("write")
        );

        let decoded = UiProgressEvent::from_rpc_notification(notification)
            .expect("decode progress notification");
        assert_eq!(decoded, event);
    }

    #[test]
    fn rpc_success_and_error_responses_use_json_rpc_v2() {
        let success = RpcResponse::success("req-1", json!({ "ok": true }));
        assert_eq!(success.jsonrpc, JSON_RPC_VERSION);
        assert!(success.is_jsonrpc_v2());

        let error = RpcErrorResponse::new(None, RpcError::parse_error("invalid json"));
        let wire = serde_json::to_value(&error).expect("serialize error response");

        assert_eq!(
            wire,
            json!({
                "jsonrpc": JSON_RPC_VERSION,
                "id": null,
                "error": {
                    "code": rpc_error_codes::PARSE_ERROR,
                    "message": "invalid json"
                }
            })
        );
    }

    #[test]
    fn ui_notification_builds_and_parses_json_rpc_notification() {
        let event = UiNotification::MessageDelta(MessageDeltaEvent {
            session_id: SessionKey("local:demo".into()),
            turn_id: TurnId(Uuid::from_u128(2)),
            text: "partial".into(),
        });

        let notification = event
            .clone()
            .into_rpc_notification()
            .expect("serialize notification params");

        assert_eq!(notification.jsonrpc, JSON_RPC_VERSION);
        assert_eq!(notification.method, methods::MESSAGE_DELTA);

        let wire = serde_json::to_value(&notification).expect("serialize notification");
        assert_eq!(wire["params"]["text"], json!("partial"));
        assert!(wire["params"].get("kind").is_none());

        let decoded_notification: RpcNotification<Value> =
            serde_json::from_value(wire).expect("deserialize notification");
        let decoded = UiNotification::from_rpc_notification(decoded_notification)
            .expect("parse notification params");

        assert_eq!(decoded, event);
    }

    #[test]
    fn resumable_notifications_carry_event_ledger_cursors() {
        let session_id = SessionKey("local:demo".into());
        let opened_cursor = UiCursor {
            stream: session_id.0.clone(),
            seq: 7,
        };
        let opened = UiNotification::SessionOpened(SessionOpened {
            session_id: session_id.clone(),
            active_profile_id: None,
            workspace_root: None,
            cursor: Some(opened_cursor.clone()),
            panes: None,
        });

        let opened_wire = opened
            .clone()
            .into_rpc_notification()
            .expect("serialize session/open notification");
        assert_eq!(opened_wire.params["cursor"]["stream"], json!(session_id.0));
        assert_eq!(opened_wire.params["cursor"]["seq"], json!(7));
        assert_eq!(
            UiNotification::from_rpc_notification(opened_wire)
                .expect("decode session/open notification"),
            opened
        );

        let completed_cursor = UiCursor {
            stream: session_id.0.clone(),
            seq: 8,
        };
        let completed = UiNotification::TurnCompleted(TurnCompletedEvent {
            session_id,
            turn_id: TurnId(Uuid::from_u128(9)),
            cursor: Some(completed_cursor),
        });
        let completed_wire = completed
            .clone()
            .into_rpc_notification()
            .expect("serialize turn/completed notification");
        assert_eq!(completed_wire.method, methods::TURN_COMPLETED);
        assert_eq!(completed_wire.params["cursor"]["seq"], json!(8));
        assert_eq!(
            UiNotification::from_rpc_notification(completed_wire)
                .expect("decode turn/completed notification"),
            completed
        );
    }

    #[test]
    fn notification_round_trips_through_json() {
        let event = UiNotification::Warning(WarningEvent {
            session_id: SessionKey("local:demo".into()),
            turn_id: None,
            code: "mock_warning".into(),
            message: "mock payload".into(),
        });

        let json = serde_json::to_string(&event).expect("serialize event");
        let decoded: UiNotification = serde_json::from_str(&json).expect("deserialize event");

        assert_eq!(decoded, event);
    }

    #[test]
    fn progress_updated_round_trip_minimal() {
        let event = UiNotification::ProgressUpdated(ProgressUpdatedEvent::new(
            SessionKey("local:demo".into()),
            None,
            UiProgressMetadata::new(progress_kinds::STATUS),
        ));

        let notification = event
            .clone()
            .into_rpc_notification()
            .expect("serialize progress/updated notification");
        assert_eq!(notification.method, methods::PROGRESS_UPDATED);

        let wire = serde_json::to_value(&notification).expect("serialize wire");
        assert_eq!(
            wire,
            json!({
                "jsonrpc": "2.0",
                "method": "progress/updated",
                "params": {
                    "session_id": "local:demo",
                    "metadata": { "kind": "status" }
                }
            })
        );

        let decoded_notification: RpcNotification<Value> =
            serde_json::from_value(wire).expect("deserialize wire");
        let decoded = UiNotification::from_rpc_notification(decoded_notification)
            .expect("decode progress/updated notification");
        assert_eq!(decoded, event);
    }

    // ----- M9-FIX-08 approval/cancelled wire registration -----

    #[test]
    fn approval_cancelled_notification_registers_method_and_round_trips() {
        let event = UiNotification::ApprovalCancelled(ApprovalCancelledEvent::turn_interrupted(
            SessionKey("local:demo".into()),
            ApprovalId::new(),
            TurnId::new(),
        ));
        assert_eq!(event.method(), methods::APPROVAL_CANCELLED);
        assert!(UI_PROTOCOL_NOTIFICATION_METHODS.contains(&methods::APPROVAL_CANCELLED));

        let rpc = event
            .clone()
            .into_rpc_notification()
            .expect("serialize approval/cancelled");
        let decoded =
            UiNotification::from_rpc_notification(rpc).expect("deserialize approval/cancelled");
        assert_eq!(decoded, event);
    }

    #[test]
    fn progress_updated_round_trip_with_typed_fields() {
        let mut token_cost = UiTokenCostUpdate::new();
        token_cost.input_tokens = Some(120);
        token_cost.output_tokens = Some(45);
        token_cost.session_cost = Some(0.0035);
        token_cost.currency = Some("USD".into());

        let mut retry = UiRetryBackoff::new();
        retry.attempt = Some(2);
        retry.max_attempts = Some(5);
        retry.backoff_ms = Some(250);
        retry.reason = Some("rate_limited".into());

        let mut metadata = UiProgressMetadata::token_cost(token_cost);
        metadata.iteration = Some(2);
        metadata.retry = Some(retry);

        let turn_id = TurnId(Uuid::from_u128(7));
        let event = UiNotification::ProgressUpdated(ProgressUpdatedEvent::new(
            SessionKey("local:demo".into()),
            Some(turn_id.clone()),
            metadata,
        ));

        let wire = serde_json::to_value(
            event
                .clone()
                .into_rpc_notification()
                .expect("serialize progress/updated"),
        )
        .expect("serialize wire");
        assert_eq!(
            wire,
            json!({
                "jsonrpc": "2.0",
                "method": "progress/updated",
                "params": {
                    "session_id": "local:demo",
                    "turn_id": turn_id,
                    "metadata": {
                        "kind": "token_cost_update",
                        "iteration": 2,
                        "retry": {
                            "attempt": 2,
                            "max_attempts": 5,
                            "backoff_ms": 250,
                            "reason": "rate_limited"
                        },
                        "token_cost": {
                            "input_tokens": 120,
                            "output_tokens": 45,
                            "session_cost": 0.0035,
                            "currency": "USD"
                        }
                    }
                }
            })
        );

        let decoded_notification: RpcNotification<Value> =
            serde_json::from_value(wire).expect("deserialize wire");
        let decoded = UiNotification::from_rpc_notification(decoded_notification)
            .expect("decode progress/updated");
        assert_eq!(decoded, event);
    }

    #[test]
    fn approval_decision_unknown_falls_through() {
        let decoded: ApprovalDecision =
            serde_json::from_value(json!("future_decision_kind")).expect("decode unknown decision");
        assert_eq!(
            decoded,
            ApprovalDecision::Unknown("future_decision_kind".into())
        );

        let re_encoded = serde_json::to_value(&decoded).expect("encode unknown decision");
        assert_eq!(re_encoded, json!("future_decision_kind"));

        // Known wire values still hit the typed variants.
        let approve: ApprovalDecision =
            serde_json::from_value(json!("approve")).expect("decode approve");
        assert_eq!(approve, ApprovalDecision::Approve);
        assert_eq!(
            serde_json::to_value(&ApprovalDecision::Deny).expect("encode deny"),
            json!("deny")
        );
    }

    // ----- Spec §10 typed error taxonomy round-trips (M9-FIX-02) -----

    /// Helper: serialize an `RpcError` and decode it back, asserting that
    /// `code` survives the trip and `data` is preserved (or absent).
    fn round_trip_rpc_error(err: &RpcError) -> RpcError {
        let value = serde_json::to_value(err).expect("serialize RpcError");
        serde_json::from_value(value).expect("deserialize RpcError")
    }

    #[test]
    fn approval_not_pending_carries_recorded_decision() {
        let approve = RpcError::approval_not_pending(ApprovalDecision::Approve);
        let json = serde_json::to_value(&approve).expect("serialize approval_not_pending");
        assert_eq!(json["code"], json!(-32011));
        assert_eq!(json["data"]["recorded_decision"], json!("approve"));
        assert_eq!(
            round_trip_rpc_error(&approve).recorded_decision(),
            Some(ApprovalDecision::Approve),
        );

        let deny = RpcError::approval_not_pending(ApprovalDecision::Deny);
        assert_eq!(
            round_trip_rpc_error(&deny).recorded_decision(),
            Some(ApprovalDecision::Deny),
        );

        // Wrong code must not pretend to carry a recorded decision.
        let mislabeled = RpcError::new(rpc_error_codes::INTERNAL_ERROR, "x")
            .with_data(json!({ "recorded_decision": "approve" }));
        assert_eq!(mislabeled.recorded_decision(), None);
    }

    #[test]
    fn cursor_out_of_range_round_trip() {
        let cursor = UiCursor {
            stream: "local:demo".into(),
            seq: 7,
        };
        let head = UiCursor {
            stream: "local:demo".into(),
            seq: 12,
        };
        let err = RpcError::cursor_out_of_range(&cursor, &head);
        assert_eq!(err.code, rpc_error_codes::CURSOR_OUT_OF_RANGE);
        let data = round_trip_rpc_error(&err).data.expect("carries data");
        assert_eq!(data["cursor"]["seq"], json!(7));
        assert_eq!(data["ledger_head"]["seq"], json!(12));
        assert_eq!(data["cursor"]["stream"], json!("local:demo"));
    }

    #[test]
    fn decode_malformed_result_returns_malformed_result_not_invalid_params() {
        // Bad inbound result must surface MALFORMED_RESULT, never INVALID_PARAMS.
        let bad = json!({ "definitely_not": "a session_open result" });
        let err = UiRpcResult::from_method_and_result(methods::SESSION_OPEN, bad)
            .expect_err("malformed result should fail to decode");
        assert_eq!(err.code, rpc_error_codes::MALFORMED_RESULT);
        assert_ne!(err.code, rpc_error_codes::INVALID_PARAMS);
        assert!(err.message.contains(methods::SESSION_OPEN));
    }

    #[test]
    fn unsupported_capability_result_round_trips() {
        // `from_method_and_result` must reconstruct UnsupportedCapability
        // even though the originating method is `approval/respond`.
        let result = UiRpcResult::UnsupportedCapability(UnsupportedCapabilityResult::method(
            methods::APPROVAL_RESPOND,
            "approval is pending",
        ));
        let value = result
            .clone()
            .into_result_value()
            .expect("serialize unsupported result");
        let decoded = UiRpcResult::from_method_and_result(methods::APPROVAL_RESPOND, value)
            .expect("decode unsupported result");
        assert_eq!(decoded, result);
        assert_eq!(decoded.kind(), UiResultKind::UnsupportedCapability);

        // Regular ApprovalRespond payload must still route to its typed variant.
        let regular =
            UiRpcResult::ApprovalRespond(ApprovalRespondResult::accepted(ApprovalId::new()))
                .into_result_value()
                .expect("serialize approval respond");
        let decoded_regular =
            UiRpcResult::from_method_and_result(methods::APPROVAL_RESPOND, regular)
                .expect("decode approval respond");
        assert_eq!(decoded_regular.kind(), UiResultKind::ApprovalRespond);
    }

    #[test]
    fn unknown_id_constructors_round_trip_with_typed_data() {
        // One round-trip per `unknown_*` constant.
        let turn = TurnId(Uuid::from_u128(42));
        let approval = ApprovalId(Uuid::from_u128(7));
        let preview = PreviewId(Uuid::from_u128(11));
        let task = TaskId(Uuid::from_u128(99));
        let cases: [(RpcError, i64, &str, &str, Value); 5] = [
            (
                RpcError::unknown_session("local:demo"),
                -32100,
                "unknown_session",
                "session_id",
                json!("local:demo"),
            ),
            (
                RpcError::unknown_turn(&turn),
                -32101,
                "unknown_turn",
                "turn_id",
                json!(turn.0.to_string()),
            ),
            (
                RpcError::unknown_approval_id(&approval),
                -32102,
                "unknown_approval",
                "approval_id",
                json!(approval.0.to_string()),
            ),
            (
                RpcError::unknown_preview_id(&preview),
                -32103,
                "unknown_preview",
                "preview_id",
                json!(preview.0.to_string()),
            ),
            (
                RpcError::unknown_task_id(&task),
                -32104,
                "unknown_task",
                "task_id",
                json!(task.to_string()),
            ),
        ];
        for (err, code, kind, key, value) in cases {
            assert_eq!(err.code, code);
            let decoded = round_trip_rpc_error(&err);
            assert_eq!(decoded.code, code);
            let data = decoded.data.unwrap();
            assert_eq!(data["kind"], json!(kind));
            assert_eq!(data[key], value);
        }
    }

    #[test]
    fn application_error_constructors_round_trip() {
        // One round-trip per remaining application-level constant.
        let cursor_invalid = RpcError::cursor_invalid("malformed cursor");
        assert_eq!(cursor_invalid.code, rpc_error_codes::CURSOR_INVALID);
        assert_eq!(cursor_invalid.code, -32111);
        assert_eq!(round_trip_rpc_error(&cursor_invalid), cursor_invalid);

        let permission = RpcError::permission_denied("sandbox: outside workspace");
        assert_eq!(permission.code, rpc_error_codes::PERMISSION_DENIED);
        assert_eq!(permission.code, -32120);
        assert_eq!(
            round_trip_rpc_error(&permission).message,
            permission.message
        );

        let unsupported =
            RpcError::unsupported_capability(methods::DIFF_PREVIEW_GET, "flag disabled");
        assert_eq!(unsupported.code, rpc_error_codes::UNSUPPORTED_CAPABILITY);
        assert_eq!(unsupported.code, -32130);
        let unsupported_decoded = round_trip_rpc_error(&unsupported);
        let report: UnsupportedCapabilityReport =
            serde_json::from_value(unsupported_decoded.data.unwrap())
                .expect("typed report decodes");
        assert_eq!(report.method, methods::DIFF_PREVIEW_GET);
        assert_eq!(report.reason, "flag disabled");

        let not_ready = RpcError::runtime_not_ready("initializing");
        assert_eq!(not_ready.code, rpc_error_codes::RUNTIME_NOT_READY);
        assert_eq!(not_ready.code, -32140);
        assert_eq!(round_trip_rpc_error(&not_ready).message, "initializing");

        let malformed = RpcError::malformed_result("invalid result for foo");
        assert_eq!(malformed.code, rpc_error_codes::MALFORMED_RESULT);
        assert_eq!(malformed.code, -32150);
        assert_eq!(round_trip_rpc_error(&malformed), malformed);

        let plain = RpcError::rate_limited("too many turns", None);
        assert_eq!(plain.code, rpc_error_codes::RATE_LIMITED);
        assert_eq!(plain.code, -32160);
        assert!(round_trip_rpc_error(&plain).data.is_none());

        let hinted = RpcError::rate_limited("too many turns", Some(2_500));
        assert_eq!(
            round_trip_rpc_error(&hinted).data.unwrap()["retry_after_ms"],
            json!(2_500)
        );
    }

    #[test]
    fn closed_string_enums_capture_unknown_wire_values() {
        // ApprovalRespondStatus
        let status: ApprovalRespondStatus =
            serde_json::from_value(json!("queued_for_review")).expect("decode status");
        assert_eq!(
            status,
            ApprovalRespondStatus::Unknown("queued_for_review".into())
        );
        assert_eq!(
            serde_json::to_value(&status).expect("encode status"),
            json!("queued_for_review")
        );
        assert_eq!(
            serde_json::to_value(&ApprovalRespondStatus::Accepted).expect("encode accepted"),
            json!("accepted")
        );

        // DiffPreviewFileStatus
        let file_status: DiffPreviewFileStatus =
            serde_json::from_value(json!("type_changed")).expect("decode file status");
        assert_eq!(
            file_status,
            DiffPreviewFileStatus::Unknown("type_changed".into())
        );
        assert_eq!(
            serde_json::to_value(&file_status).expect("encode file status"),
            json!("type_changed")
        );
        assert_eq!(
            serde_json::to_value(&DiffPreviewFileStatus::Renamed).expect("encode renamed"),
            json!("renamed")
        );
    }

    #[test]
    fn input_item_unknown_kind_falls_through() {
        // Tagged input items with future kinds decode to the Unknown unit
        // variant rather than erroring. Known kinds still decode normally.
        let unknown: InputItem = serde_json::from_value(json!({
            "kind": "voice",
            "audio_url": "https://example.test/clip.wav"
        }))
        .expect("decode unknown input item kind");
        assert_eq!(unknown, InputItem::Unknown);

        let known: InputItem = serde_json::from_value(json!({
            "kind": "text",
            "text": "hello"
        }))
        .expect("decode text input item");
        assert_eq!(
            known,
            InputItem::Text {
                text: "hello".into()
            }
        );
    }

    #[test]
    fn rpc_error_codes_partition_is_disjoint() {
        // Application-layer codes must live in -32100..=-32199; the
        // spec-pinned APPROVAL_NOT_PENDING is the documented exception.
        for code in [
            rpc_error_codes::UNKNOWN_SESSION,
            rpc_error_codes::UNKNOWN_TURN,
            rpc_error_codes::UNKNOWN_APPROVAL_ID,
            rpc_error_codes::UNKNOWN_PREVIEW_ID,
            rpc_error_codes::UNKNOWN_TASK_ID,
            rpc_error_codes::APPROVAL_CANCELLED,
            rpc_error_codes::CURSOR_OUT_OF_RANGE,
            rpc_error_codes::CURSOR_INVALID,
            rpc_error_codes::PERMISSION_DENIED,
            rpc_error_codes::UNSUPPORTED_CAPABILITY,
            rpc_error_codes::RUNTIME_NOT_READY,
            rpc_error_codes::MALFORMED_RESULT,
            rpc_error_codes::RATE_LIMITED,
        ] {
            assert!(
                (-32199..=-32100).contains(&code),
                "{code} outside -32100..=-32199",
            );
        }
        assert_eq!(rpc_error_codes::APPROVAL_NOT_PENDING, -32011);
        assert_eq!(rpc_error_codes::APPROVAL_CANCELLED, -32105);
    }

    #[test]
    fn approval_decided_notification_round_trips_through_wire() {
        let session_id = SessionKey("local:demo".into());
        let approval_id = ApprovalId(Uuid::from_u128(0xa11));
        let turn_id = TurnId(Uuid::from_u128(0xb22));
        let decided_at: DateTime<Utc> = DateTime::parse_from_rfc3339("2026-04-28T12:00:00Z")
            .expect("rfc3339 timestamp")
            .with_timezone(&Utc);
        let event = UiNotification::ApprovalDecided(ApprovalDecidedEvent {
            session_id: session_id.clone(),
            approval_id: approval_id.clone(),
            turn_id: turn_id.clone(),
            decision: ApprovalDecision::Approve,
            scope: Some(approval_scopes::SESSION.into()),
            decided_at,
            decided_by: "user:abc".into(),
            auto_resolved: true,
            policy_id: Some("policy-1".into()),
            client_note: Some("looks good".into()),
        });

        let wire = event
            .clone()
            .into_rpc_notification()
            .expect("serialize approval/decided");
        assert_eq!(wire.method, methods::APPROVAL_DECIDED);
        assert_eq!(
            wire.params["approval_id"],
            serde_json::to_value(&approval_id).unwrap()
        );
        assert_eq!(wire.params["decision"], json!("approve"));
        assert_eq!(wire.params["auto_resolved"], json!(true));
        assert_eq!(wire.params["policy_id"], json!("policy-1"));

        let decoded = UiNotification::from_rpc_notification(wire).expect("decode approval/decided");
        assert_eq!(decoded, event);

        let body = serde_json::to_string(&event).expect("serialize event");
        let again: UiNotification = serde_json::from_str(&body).expect("deserialize event");
        assert_eq!(again, event);
    }

    #[test]
    fn first_server_capabilities_advertise_approval_cancelled() {
        let capabilities = UiProtocolCapabilities::first_server_slice();
        assert!(
            capabilities
                .supported_notifications
                .iter()
                .any(|method| method == methods::APPROVAL_CANCELLED),
            "approval/cancelled must be advertised so clients can render it",
        );
    }

    // ----- M9 review fix MEDIUM #4 (UPCR-2026-004): Cancelled task state -----

    #[test]
    fn task_runtime_state_cancelled_round_trips_as_snake_case_cancelled() {
        // Wire form must be exactly `"cancelled"` so the agent's
        // `TaskLifecycleState::Cancelled` (also `snake_case`-serialized as
        // `"cancelled"`) flows through the protocol mapper without falling
        // back to `Running`. UPCR-2026-004 promises `"cancelled"` (the British
        // spelling) as the wire literal.
        let value = serde_json::to_value(TaskRuntimeState::Cancelled).expect("serialize Cancelled");
        assert_eq!(value, json!("cancelled"));
        let parsed: TaskRuntimeState = serde_json::from_value(value).expect("round-trip Cancelled");
        assert_eq!(parsed, TaskRuntimeState::Cancelled);
    }

    #[test]
    fn task_updated_event_round_trips_with_cancelled_state() {
        let event = UiNotification::TaskUpdated(TaskUpdatedEvent {
            session_id: SessionKey("local:demo".into()),
            task_id: TaskId(Uuid::from_u128(7)),
            title: "spawn_only_runner".into(),
            state: TaskRuntimeState::Cancelled,
            runtime_detail: Some("user cancelled".into()),
        });
        let rpc = event
            .clone()
            .into_rpc_notification()
            .expect("serialize task/updated cancelled");
        let decoded =
            UiNotification::from_rpc_notification(rpc).expect("deserialize task/updated cancelled");
        assert_eq!(decoded, event);
    }
}
