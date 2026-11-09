//! M7.6 — contract-authoring + swarm dispatch dashboard backend.
//!
//! Thin HTTP surface in front of the stable
//! [`octos_swarm::Swarm::dispatch`] primitive. The dashboard's 4-tab UI
//! (Author / Dispatch / Live / Review) consumes this module's REST
//! endpoints + the existing `/api/events`-style SSE stream.
//!
//! # Invariants honoured
//!
//! 1. Dispatch is a thin wrapper — [`dispatch_swarm`] resolves the
//!    shared [`SwarmState`], forwards to `Swarm::dispatch`, records the
//!    dispatch id in the local index, returns `dispatch_id` to the
//!    caller. No orchestration logic lives in this module.
//! 2. Cost roll-up is a live read against the shared
//!    [`PersistentCostLedger`]. Every GET recomputes — no caching.
//! 3. Review decisions are emitted as typed
//!    [`HarnessEventPayload::SwarmReviewDecision`] events — written to
//!    the JSONL sink if configured (durable record), broadcast live to
//!    SSE subscribers, and surface on the Matrix audit channel only
//!    when a Matrix puppet subscriber is attached to the broadcaster.
//! 4. All persisted / event shapes carry `schema_version: u32` pinned
//!    in [`abi_schema`](octos_agent::abi_schema).

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::{Arc, RwLock};

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use octos_agent::harness_events::write_event_to_sink;
use octos_agent::tools::mcp_agent::{
    DispatchOutcome, DispatchRequest, DispatchResponse, McpAgentBackend,
};
use octos_agent::{
    CostLedger, HarnessEvent, HarnessSwarmReviewDecisionEvent, PersistentCostLedger,
    SWARM_REVIEW_DECISION_SCHEMA_VERSION,
};
use octos_swarm::{
    ContractSpec, NoopCostLedger, SubtaskOutcome, SubtaskStatus, Swarm, SwarmBudget, SwarmContext,
    SwarmEventSink, SwarmOutcomeKind, SwarmResult, SwarmTopology,
};
use serde::{Deserialize, Serialize};

use super::AppState;

/// Upper bound on every operator-supplied identifier carried through the
/// dispatch API. Mirrors
/// [`octos_agent::harness_events::MAX_MESSAGE_BYTES`] (2 KiB) so the
/// validator rejects a bad field before the event validator further
/// downstream sees it — making the 400 failure mode uniform with the
/// sink-write validator. Measured in bytes, not chars.
const MAX_DISPATCH_FIELD_BYTES: usize = 2 * 1024;

/// Hard cap on [`SwarmState::dispatches`]. Long-running deployments can
/// accumulate dispatches indefinitely; once this threshold is hit the
/// push path evicts the oldest entry (FIFO ring buffer) and appends the
/// new one. Ring-buffer was chosen over 507-rejection so legitimate
/// high-load production traffic never surfaces a user-visible storage
/// error — the primitive's redb file remains the durable ground truth,
/// this Vec is a best-effort read-model.
pub const MAX_IN_MEMORY_DISPATCHES: usize = 4096;

/// Shared swarm state plumbed into [`AppState`] by `octos serve`. Owns
/// the primitive, the cost ledger, and an in-memory dispatch index used
/// for list / detail endpoints. The state is wired at serve boot so
/// the routes remain thin; tests inject a stub backend + a tempdir-
/// backed ledger.
pub struct SwarmState {
    /// Primitive that owns the `DispatchStore` and the MCP backend.
    /// Wrapped in an `Arc` so handlers can `dispatch` without holding a
    /// handler-exclusive lock.
    pub swarm: Arc<Swarm>,
    /// Live persistent cost ledger. Every `/api/cost/attributions/{id}`
    /// hits this — no caching (invariant 2).
    pub cost_ledger: Arc<PersistentCostLedger>,
    /// In-memory record of each dispatch + its full
    /// [`SwarmResult`]. The primitive's redb file keeps the durable
    /// ground truth — this is the read-model the dashboard renders.
    /// Re-dispatching an existing id overwrites the row. Capped at
    /// [`MAX_IN_MEMORY_DISPATCHES`] — the oldest entry is dropped FIFO
    /// when the cap is exceeded so long-running servers cannot leak
    /// memory through the read-model.
    pub dispatches: RwLock<Vec<DispatchEntry>>,
    /// Default supervisor context stamped on primitive dispatches. Real
    /// deployments override per-request; tests leave the default.
    pub default_context: SwarmContextSpec,
}

/// In-memory detail row — carries both the summary needed by the list
/// endpoint and the full `SwarmResult` for the detail endpoint.
#[derive(Debug, Clone)]
pub struct DispatchEntry {
    pub row: DispatchIndexRow,
    pub result: SwarmResult,
    pub review_reviewer: Option<String>,
    pub review_notes: Option<String>,
}

/// Serializable form of [`SwarmContext`] used by the request payload.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct SwarmContextSpec {
    pub session_id: String,
    pub task_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
}

impl SwarmContextSpec {
    fn to_context(&self) -> SwarmContext {
        SwarmContext {
            session_id: self.session_id.clone(),
            task_id: self.task_id.clone(),
            workflow: self.workflow.clone(),
            phase: self.phase.clone(),
        }
    }
}

impl Default for SwarmContextSpec {
    fn default() -> Self {
        Self {
            session_id: "api:swarm-dashboard".into(),
            task_id: "task-swarm".into(),
            workflow: Some("swarm".into()),
            phase: Some("dispatch".into()),
        }
    }
}

/// Lightweight row tracked alongside every dispatched swarm so the
/// list endpoint can render a recent-first table without range-scanning
/// redb. Cost USD is a snapshot at finalize-time for the list view —
/// detail view recomputes live.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DispatchIndexRow {
    pub dispatch_id: String,
    pub contract_id: String,
    pub topology: String,
    pub outcome: String,
    pub total_subtasks: u32,
    pub completed_subtasks: u32,
    pub retry_rounds_used: u32,
    pub created_at: String,
    /// Snapshot from the ledger adapter. Detail endpoint recomputes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_cost_usd: Option<f64>,
    /// `true` once the review gate has written an accept/reject
    /// decision. `None` means no review has been recorded yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_accepted: Option<bool>,
}

/// POST /api/swarm/dispatch request body. Matches the M7.5 primitive
/// signature 1-for-1 so the dashboard's Author tab can schema-validate
/// client-side against the same shape.
#[derive(Debug, Clone, Deserialize)]
pub struct SwarmDispatchRequest {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    /// Stable dispatch id. Re-submitting the same id short-circuits to
    /// the prior finalized record (primitive idempotency invariant).
    pub dispatch_id: String,
    /// Operator-chosen contract id for cost / review roll-ups. Matches
    /// the `contract_id` on [`octos_agent::CostAttributionEvent`].
    pub contract_id: String,
    /// The contract list the primitive dispatches.
    pub contracts: Vec<ContractSpec>,
    /// Topology controlling fan-out.
    pub topology: SwarmTopology,
    /// Per-dispatch budget knobs.
    #[serde(default)]
    pub budget: SwarmBudgetSpec,
    /// Supervisor context. If `None`, falls back to
    /// [`SwarmState::default_context`].
    #[serde(default)]
    pub context: Option<SwarmContextSpec>,
}

fn default_schema_version() -> u32 {
    1
}

/// Serializable mirror of [`SwarmBudget`]. The primitive's struct
/// exposes two optional fields; we mirror them verbatim so the
/// dashboard UI uses the same keys.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct SwarmBudgetSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_contracts: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_retry_rounds: Option<u32>,
}

impl SwarmBudgetSpec {
    fn to_budget(&self) -> SwarmBudget {
        SwarmBudget {
            max_contracts: self.max_contracts,
            max_retry_rounds: self.max_retry_rounds,
        }
    }
}

/// POST /api/swarm/dispatch response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwarmDispatchResponse {
    pub dispatch_id: String,
    pub outcome: String,
    pub total_subtasks: u32,
    pub completed_subtasks: u32,
}

/// GET /api/swarm/dispatches response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwarmDispatchesResponse {
    pub dispatches: Vec<DispatchIndexRow>,
}

/// GET /api/swarm/dispatches/{id} response. Combines the full redb
/// record, per-subtask outcomes, and the live cost rollup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwarmDispatchDetail {
    pub schema_version: u32,
    pub dispatch_id: String,
    pub contract_id: String,
    pub topology: String,
    pub outcome: String,
    pub total_subtasks: u32,
    pub completed_subtasks: u32,
    pub retry_rounds_used: u32,
    pub finalized: bool,
    pub subtasks: Vec<SubtaskView>,
    pub validator_evidence: Vec<ValidatorView>,
    pub cost_attributions: Vec<CostAttributionView>,
    pub total_cost_usd: f64,
    pub review_accepted: Option<bool>,
    pub review_reviewer: Option<String>,
    pub review_notes: Option<String>,
}

/// Per-subtask view exposed to the dashboard. Mirrors
/// [`SubtaskOutcome`] with stable strings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubtaskView {
    pub contract_id: String,
    pub label: Option<String>,
    pub status: String,
    pub attempts: u32,
    pub last_dispatch_outcome: String,
    pub output: String,
    pub error: Option<String>,
}

impl From<&SubtaskOutcome> for SubtaskView {
    fn from(outcome: &SubtaskOutcome) -> Self {
        Self {
            contract_id: outcome.contract_id.clone(),
            label: outcome.label.clone(),
            status: status_str(outcome.status).to_string(),
            attempts: outcome.attempts,
            last_dispatch_outcome: outcome.last_dispatch_outcome.clone(),
            output: outcome.output.clone(),
            error: outcome.error.clone(),
        }
    }
}

fn status_str(status: SubtaskStatus) -> &'static str {
    match status {
        SubtaskStatus::Completed => "completed",
        SubtaskStatus::RetryableFailed => "retryable_failed",
        SubtaskStatus::TerminalFailed => "terminal_failed",
    }
}

/// Validator evidence per the M4.3 aggregate check. Simplified to just
/// the data the review gate renders.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidatorView {
    pub name: String,
    pub passed: bool,
    pub message: Option<String>,
}

/// Cost attribution row as surfaced to the dashboard. Dropped
/// `supervisor_session` + timestamp fields remain in the persistent
/// ledger — the dashboard only renders what the review gate needs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostAttributionView {
    pub attribution_id: String,
    pub contract_id: String,
    pub model: String,
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub cost_usd: f64,
    pub outcome: String,
    pub timestamp: String,
}

/// GET /api/cost/attributions/{dispatch_id} response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostAttributionsResponse {
    pub dispatch_id: String,
    pub attributions: Vec<CostAttributionView>,
    pub total_cost_usd: f64,
    pub total_tokens_in: u64,
    pub total_tokens_out: u64,
    pub count: u64,
}

/// POST /api/swarm/dispatches/{id}/review request body.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SwarmReviewRequest {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    pub accepted: bool,
    pub reviewer: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

/// POST /api/swarm/dispatches/{id}/review response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwarmReviewResponse {
    pub dispatch_id: String,
    pub accepted: bool,
    pub reviewer: String,
    pub schema_version: u32,
}

// ── Handlers ──────────────────────────────────────────────────────────

/// POST /api/swarm/dispatch — dispatch a swarm and return a stable id.
pub async fn dispatch_swarm(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SwarmDispatchRequest>,
) -> Result<Json<SwarmDispatchResponse>, (StatusCode, String)> {
    let swarm_state = state.swarm_state.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "swarm not configured".into(),
    ))?;

    validate_dispatch_request(&req).map_err(|e| (StatusCode::BAD_REQUEST, e))?;

    let context = req
        .context
        .clone()
        .unwrap_or_else(|| swarm_state.default_context.clone());

    let result = swarm_state
        .swarm
        .dispatch(
            req.dispatch_id.clone(),
            req.contracts.clone(),
            req.topology.clone(),
            req.budget.to_budget(),
            context.to_context(),
        )
        .await
        .map_err(|e| {
            // Log the full eyre chain server-side for operators; the HTTP
            // body is scrubbed so upstream error detail (file paths,
            // credential fragments in nested messages, backend URLs)
            // never leaks to unauthenticated or low-privilege callers.
            tracing::error!(
                dispatch_id = %req.dispatch_id,
                error = ?e,
                "swarm dispatch failed"
            );
            (StatusCode::INTERNAL_SERVER_ERROR, "dispatch failed".into())
        })?;

    let outcome_label = outcome_str(result.outcome).to_string();
    let row = DispatchIndexRow {
        dispatch_id: result.dispatch_id.clone(),
        contract_id: req.contract_id.clone(),
        topology: result.topology.clone(),
        outcome: outcome_label.clone(),
        total_subtasks: result.total_subtasks,
        completed_subtasks: result.completed_subtasks,
        retry_rounds_used: result.retry_rounds_used,
        created_at: chrono::Utc::now().to_rfc3339(),
        total_cost_usd: result.total_cost_usd,
        review_accepted: None,
    };
    let dispatch_id = result.dispatch_id.clone();
    // Record the entry. A repeat call on the same dispatch_id updates
    // the existing row (e.g. if a review was recorded between retries).
    {
        let mut list = swarm_state
            .dispatches
            .write()
            .unwrap_or_else(|e| e.into_inner());
        match list
            .iter_mut()
            .find(|entry| entry.row.dispatch_id == dispatch_id)
        {
            Some(entry) => {
                // Preserve an existing review decision across re-dispatches
                // (the primitive is idempotent — the operator's accept
                // should survive a retry).
                let prior_review = entry.row.review_accepted;
                entry.row = row.clone();
                entry.row.review_accepted = prior_review;
                entry.result = result.clone();
            }
            None => {
                // Ring-buffer: evict the oldest entry (FIFO) when at the
                // cap. The primitive's redb file keeps the durable ground
                // truth; this Vec is a best-effort read-model, so we
                // prefer a silent eviction to rejecting a legitimate
                // dispatch with a user-visible 507.
                if list.len() >= MAX_IN_MEMORY_DISPATCHES {
                    list.remove(0);
                }
                list.push(DispatchEntry {
                    row: row.clone(),
                    result: result.clone(),
                    review_reviewer: None,
                    review_notes: None,
                });
            }
        }
    }

    Ok(Json(SwarmDispatchResponse {
        dispatch_id,
        outcome: outcome_label,
        total_subtasks: result.total_subtasks,
        completed_subtasks: result.completed_subtasks,
    }))
}

fn validate_dispatch_request(req: &SwarmDispatchRequest) -> Result<(), String> {
    if req.dispatch_id.trim().is_empty() {
        return Err("dispatch_id cannot be empty".into());
    }
    // Length cap mirrors [`MAX_DISPATCH_FIELD_BYTES`] — a megabyte-scale
    // dispatch_id should be rejected early so we never persist it.
    if req.dispatch_id.len() > MAX_DISPATCH_FIELD_BYTES {
        return Err(format!(
            "dispatch_id length {} exceeds bound {MAX_DISPATCH_FIELD_BYTES}",
            req.dispatch_id.len()
        ));
    }
    if req.contract_id.trim().is_empty() {
        return Err("contract_id cannot be empty".into());
    }
    if req.contract_id.len() > MAX_DISPATCH_FIELD_BYTES {
        return Err(format!(
            "contract_id length {} exceeds bound {MAX_DISPATCH_FIELD_BYTES}",
            req.contract_id.len()
        ));
    }
    // Pre-validate contract count so an oversize payload can be rejected
    // with a typed 400 instead of burning memory serializing it into the
    // primitive only to have the budget cap reject later.
    if req.contracts.len() > octos_swarm::MAX_CONTRACTS_PER_DISPATCH {
        return Err(format!(
            "contracts length {} exceeds bound {}",
            req.contracts.len(),
            octos_swarm::MAX_CONTRACTS_PER_DISPATCH
        ));
    }
    if req.contracts.is_empty() {
        // Fanout topology supplies its own contracts via the pattern,
        // so an empty caller list is only valid when topology is
        // [`SwarmTopology::Fanout`]. Otherwise reject.
        if !matches!(req.topology, SwarmTopology::Fanout { .. }) {
            return Err("contracts list cannot be empty for this topology".into());
        }
    }
    for contract in &req.contracts {
        if contract.contract_id.trim().is_empty() {
            return Err("contract.contract_id cannot be empty".into());
        }
        if contract.contract_id.len() > MAX_DISPATCH_FIELD_BYTES {
            return Err(format!(
                "contract.contract_id length {} exceeds bound {MAX_DISPATCH_FIELD_BYTES}",
                contract.contract_id.len()
            ));
        }
        if contract.tool_name.trim().is_empty() {
            return Err("contract.tool_name cannot be empty".into());
        }
        if contract.tool_name.len() > MAX_DISPATCH_FIELD_BYTES {
            return Err(format!(
                "contract.tool_name length {} exceeds bound {MAX_DISPATCH_FIELD_BYTES}",
                contract.tool_name.len()
            ));
        }
    }
    if let Some(rounds) = req.budget.max_retry_rounds {
        if rounds > octos_swarm::MAX_RETRY_ROUNDS {
            return Err(format!(
                "max_retry_rounds {rounds} exceeds bound {}",
                octos_swarm::MAX_RETRY_ROUNDS
            ));
        }
    }
    if let Some(n) = req.budget.max_contracts {
        if n > octos_swarm::MAX_CONTRACTS_PER_DISPATCH {
            return Err(format!(
                "max_contracts {n} exceeds bound {}",
                octos_swarm::MAX_CONTRACTS_PER_DISPATCH
            ));
        }
    }
    // Belt-and-braces: Parallel topology requires a non-zero concurrency
    // cap. The primitive enforces this via `NonZeroUsize` but we fail
    // fast so the UI surfaces a friendlier 400 error.
    if let SwarmTopology::Parallel { max_concurrency } = &req.topology {
        if max_concurrency.get() == 0 {
            return Err("parallel concurrency must be > 0".into());
        }
    }
    if let SwarmTopology::Fanout {
        max_concurrency,
        pattern,
    } = &req.topology
    {
        if max_concurrency.get() == 0 {
            return Err("fanout concurrency must be > 0".into());
        }
        if pattern.variants.is_empty() {
            return Err("fanout pattern must declare at least one variant".into());
        }
    }
    Ok(())
}

fn outcome_str(outcome: SwarmOutcomeKind) -> &'static str {
    match outcome {
        SwarmOutcomeKind::Success => "success",
        SwarmOutcomeKind::Partial => "partial",
        SwarmOutcomeKind::Failed => "failed",
        SwarmOutcomeKind::Aborted => "aborted",
    }
}

/// GET /api/swarm/dispatches — list recent dispatches with rollup.
pub async fn list_dispatches(
    State(state): State<Arc<AppState>>,
) -> Result<Json<SwarmDispatchesResponse>, (StatusCode, String)> {
    let swarm_state = state.swarm_state.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "swarm not configured".into(),
    ))?;

    let entries = swarm_state
        .dispatches
        .read()
        .unwrap_or_else(|e| e.into_inner());
    let mut dispatches: Vec<DispatchIndexRow> = entries.iter().map(|e| e.row.clone()).collect();
    // Reverse so latest is first.
    dispatches.reverse();
    Ok(Json(SwarmDispatchesResponse { dispatches }))
}

/// GET /api/swarm/dispatches/{id} — full dispatch detail.
pub async fn dispatch_detail(
    State(state): State<Arc<AppState>>,
    Path(dispatch_id): Path<String>,
) -> Result<Json<SwarmDispatchDetail>, (StatusCode, String)> {
    let swarm_state = state.swarm_state.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "swarm not configured".into(),
    ))?;

    let entry = {
        let entries = swarm_state
            .dispatches
            .read()
            .unwrap_or_else(|e| e.into_inner());
        entries
            .iter()
            .find(|e| e.row.dispatch_id == dispatch_id)
            .cloned()
    }
    .ok_or((
        StatusCode::NOT_FOUND,
        format!("unknown dispatch: {dispatch_id}"),
    ))?;

    // Live cost ledger read — invariant 2.
    let attributions = swarm_state
        .cost_ledger
        .list_for_contract(&dispatch_id)
        .await
        .unwrap_or_default();

    let total_cost_usd = attributions.iter().map(|a| a.cost_usd).sum::<f64>();

    let subtasks = entry
        .result
        .per_task_outcomes
        .iter()
        .map(SubtaskView::from)
        .collect();

    let validator_evidence = entry
        .result
        .validator_results
        .iter()
        .map(|v| ValidatorView {
            name: v.validator_id.clone(),
            passed: v.required_gate_passed(),
            message: Some(v.reason.clone()),
        })
        .collect();

    Ok(Json(SwarmDispatchDetail {
        schema_version: octos_swarm::DISPATCH_RECORD_SCHEMA_VERSION,
        dispatch_id: entry.row.dispatch_id.clone(),
        contract_id: entry.row.contract_id.clone(),
        topology: entry.row.topology.clone(),
        outcome: entry.row.outcome.clone(),
        total_subtasks: entry.result.total_subtasks,
        completed_subtasks: entry.result.completed_subtasks,
        retry_rounds_used: entry.result.retry_rounds_used,
        finalized: true,
        subtasks,
        validator_evidence,
        cost_attributions: attributions
            .iter()
            .map(|e| CostAttributionView {
                attribution_id: e.attribution_id.clone(),
                contract_id: e.contract_id.clone(),
                model: e.model.clone(),
                tokens_in: e.tokens_in,
                tokens_out: e.tokens_out,
                cost_usd: e.cost_usd,
                outcome: e.outcome.clone().unwrap_or_default(),
                timestamp: e.timestamp.clone(),
            })
            .collect(),
        total_cost_usd,
        review_accepted: entry.row.review_accepted,
        review_reviewer: entry.review_reviewer.clone(),
        review_notes: entry.review_notes.clone(),
    }))
}

/// GET /api/cost/attributions/{dispatch_id} — live ledger read.
pub async fn cost_attributions(
    State(state): State<Arc<AppState>>,
    Path(dispatch_id): Path<String>,
) -> Result<Json<CostAttributionsResponse>, (StatusCode, String)> {
    let swarm_state = state.swarm_state.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "swarm not configured".into(),
    ))?;

    // Invariant 2: every call hits the live ledger — never a cache.
    let attributions = swarm_state
        .cost_ledger
        .list_for_contract(&dispatch_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("ledger: {e}")))?;

    let total_cost_usd = attributions.iter().map(|a| a.cost_usd).sum::<f64>();
    let total_tokens_in = attributions.iter().map(|a| u64::from(a.tokens_in)).sum();
    let total_tokens_out = attributions.iter().map(|a| u64::from(a.tokens_out)).sum();
    let count = attributions.len() as u64;

    let view = attributions
        .iter()
        .map(|e| CostAttributionView {
            attribution_id: e.attribution_id.clone(),
            contract_id: e.contract_id.clone(),
            model: e.model.clone(),
            tokens_in: e.tokens_in,
            tokens_out: e.tokens_out,
            cost_usd: e.cost_usd,
            outcome: e.outcome.clone().unwrap_or_default(),
            timestamp: e.timestamp.clone(),
        })
        .collect();

    Ok(Json(CostAttributionsResponse {
        dispatch_id,
        attributions: view,
        total_cost_usd,
        total_tokens_in,
        total_tokens_out,
        count,
    }))
}

/// POST /api/swarm/dispatches/{id}/review — write a typed review event.
///
/// Delivery guarantees:
/// - **Durable**: written to the JSONL harness-event sink when
///   [`AppState::harness_event_sink_path`] is configured. Without a sink,
///   the decision is live-only.
/// - **Live**: broadcast to SSE subscribers on the existing `/api/events`
///   stream.
/// - **Matrix audit**: materialises only when a Matrix puppet subscriber
///   is attached to the broadcaster — nothing in this handler pushes to
///   Matrix directly.
///
/// The body's `reviewer` field is cross-checked against the authenticated
/// caller. Only bootstrap / rotated admin tokens — i.e. callers who
/// authenticate as [`AuthIdentity::Admin`](super::router::AuthIdentity::Admin)
/// — may submit a `reviewer` string different from their own id. Every
/// caller identified as [`AuthIdentity::User`](super::router::AuthIdentity::User)
/// (regardless of the user's [`UserRole`](crate::user_store::UserRole) —
/// including `UserRole::Admin` rows) MUST submit under their own id.
/// Admin-token callers can set any `reviewer` string — the field then
/// doubles as an impersonation record for audit (e.g. an on-call admin
/// filing a decision on behalf of a PM).
pub async fn submit_review(
    State(state): State<Arc<AppState>>,
    Path(dispatch_id): Path<String>,
    identity: Option<axum::Extension<super::router::AuthIdentity>>,
    Json(req): Json<SwarmReviewRequest>,
) -> Result<Json<SwarmReviewResponse>, (StatusCode, String)> {
    let swarm_state = state.swarm_state.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "swarm not configured".into(),
    ))?;

    if req.reviewer.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "reviewer cannot be empty".into()));
    }
    if req.schema_version > SWARM_REVIEW_DECISION_SCHEMA_VERSION {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "unsupported review schema_version {} (max: {})",
                req.schema_version, SWARM_REVIEW_DECISION_SCHEMA_VERSION
            ),
        ));
    }

    // Cross-check `reviewer` against the authenticated caller.
    // - [`AuthIdentity::Admin`] (bootstrap / rotated admin tokens): any
    //   `reviewer` string is allowed (audit-by-impersonation).
    // - [`AuthIdentity::User`]: the body's `reviewer` MUST match the
    //   user's id exactly. This applies to every user row regardless of
    //   [`UserRole`] — a `UserRole::Admin` user still cannot submit
    //   under another user's id; only the bootstrap-token variant has
    //   that bypass.
    // - No identity (middleware not active, e.g. unauthenticated serve
    //   mode): skip the check — there is nothing to compare against.
    if let Some(axum::Extension(super::router::AuthIdentity::User { id, .. })) = identity.as_ref() {
        if &req.reviewer != id {
            return Err((StatusCode::FORBIDDEN, "reviewer_identity_mismatch".into()));
        }
    }

    // 404 when the dispatch is unknown — the dashboard should only
    // present reviewable dispatches sourced from the list endpoint.
    let (topology_label, session_id, task_id) = {
        let entries = swarm_state
            .dispatches
            .read()
            .unwrap_or_else(|e| e.into_inner());
        let entry = entries
            .iter()
            .find(|e| e.row.dispatch_id == dispatch_id)
            .ok_or((
                StatusCode::NOT_FOUND,
                format!("unknown dispatch: {dispatch_id}"),
            ))?;
        (
            entry.row.topology.clone(),
            swarm_state.default_context.session_id.clone(),
            swarm_state.default_context.task_id.clone(),
        )
    };

    // Record the decision on the matching entry for the list view.
    {
        let mut entries = swarm_state
            .dispatches
            .write()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = entries
            .iter_mut()
            .find(|e| e.row.dispatch_id == dispatch_id)
        {
            entry.row.review_accepted = Some(req.accepted);
            entry.review_reviewer = Some(req.reviewer.clone());
            entry.review_notes = req.notes.clone();
        }
    }

    // Emit the typed event through the SSE broadcaster so the dashboard
    // and any Matrix audit channel receive the decision on the existing
    // /api/events stream (invariant 3 + 4).
    let mut extra = HashMap::new();
    extra.insert("topology".into(), serde_json::Value::String(topology_label));
    let event = HarnessEvent::swarm_review_decision(HarnessSwarmReviewDecisionEvent {
        schema_version: SWARM_REVIEW_DECISION_SCHEMA_VERSION,
        session_id,
        task_id,
        workflow: Some("swarm".into()),
        phase: Some("review".into()),
        dispatch_id: dispatch_id.clone(),
        accepted: req.accepted,
        reviewer: req.reviewer.clone(),
        notes: req.notes.clone(),
        extra,
    });
    // Validate the event before broadcasting so bad inputs surface as
    // 400 instead of a malformed SSE frame.
    event.validate().map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            format!("review event invalid: {e}"),
        )
    })?;

    // Persist to the JSONL harness-event sink FIRST (if configured) so
    // the decision survives a crash before the live SSE frame goes out.
    // Broadcast-only would lose the decision when no subscriber is
    // connected; a Matrix audit puppet is only one possible subscriber.
    if let Some(sink_path) = state.harness_event_sink_path.as_ref() {
        if let Err(err) = write_event_to_sink(sink_path, &event) {
            tracing::warn!(
                dispatch_id = %dispatch_id,
                error = %err,
                "failed to persist review decision to harness sink"
            );
        }
    }

    let body = serde_json::to_string(&event.runtime_detail_value(None, None))
        .unwrap_or_else(|_| "{}".into());
    let _ = state.broadcaster.send_raw(body);

    Ok(Json(SwarmReviewResponse {
        dispatch_id,
        accepted: req.accepted,
        reviewer: req.reviewer,
        schema_version: SWARM_REVIEW_DECISION_SCHEMA_VERSION,
    }))
}

// ── Helpers for wiring from `serve` ─────────────────────────────────

/// [`SwarmEventSink`] that forwards every emitted [`HarnessEvent`] to
/// the shared SSE broadcaster and — when configured — appends the same
/// event to the JSONL harness sink.
///
/// F-009: previously `build_swarm_state` relied on the primitive's
/// default [`NoopSwarmEventSink`], so every `HarnessSwarmDispatchEvent`
/// was discarded and `/api/events/harness?kinds=SwarmDispatch` never
/// received a single frame. The M7.8 live gate expected these events.
pub struct BroadcasterSwarmEventSink {
    pub broadcaster: Arc<super::EventBroadcaster>,
    pub sink_path: Option<String>,
}

impl BroadcasterSwarmEventSink {
    pub fn new(broadcaster: Arc<super::EventBroadcaster>, sink_path: Option<String>) -> Self {
        Self {
            broadcaster,
            sink_path,
        }
    }
}

impl SwarmEventSink for BroadcasterSwarmEventSink {
    fn emit(&self, event: &HarnessEvent) {
        // Persist FIRST so a crash between the write and the broadcast
        // can't lose the dispatch record. The live broadcast is the
        // fire-and-forget half — subscribers that miss the frame can
        // replay from the sink file later.
        if let Some(sink_path) = self.sink_path.as_ref() {
            if let Err(err) = write_event_to_sink(sink_path, event) {
                tracing::warn!(
                    error = %err,
                    "failed to persist swarm dispatch event to harness sink"
                );
            }
        }
        let body = serde_json::to_string(&event.runtime_detail_value(None, None))
            .unwrap_or_else(|_| "{}".into());
        let _ = self.broadcaster.send_raw(body);
    }
}

/// Build a [`SwarmState`] given a backend and data directory. The backend
/// is injected by the caller so the dashboard can swap transports (local
/// stdio, remote HTTPS) without this module growing a backend factory.
///
/// The wired event sink forwards every `HarnessSwarmDispatchEvent`
/// through [`BroadcasterSwarmEventSink`] so the dashboard's Live tab +
/// the M7.8 live gate both see frames on `/api/events/harness`.
///
/// M7 req 7: callers MAY pass a [`octos_swarm::DispatchPolicy`] to wire
/// the swarm-level approval / tool-policy / sandbox / env-allowlist
/// gates. Pass `None` for the legacy unenforced behaviour (existing
/// integration tests pre-fix) — production deployments should pass a
/// real policy.
pub async fn build_swarm_state(
    backend: Arc<dyn McpAgentBackend>,
    swarm_dir: impl Into<std::path::PathBuf>,
    cost_ledger: Arc<PersistentCostLedger>,
    broadcaster: Arc<super::EventBroadcaster>,
    sink_path: Option<String>,
    dispatch_policy: Option<octos_swarm::DispatchPolicy>,
) -> eyre::Result<SwarmState> {
    let swarm_dir = swarm_dir.into();
    let sink: Arc<dyn SwarmEventSink> =
        Arc::new(BroadcasterSwarmEventSink::new(broadcaster, sink_path));
    let mut builder = Swarm::builder(backend, &swarm_dir).with_event_sink(sink);
    if let Some(policy) = dispatch_policy {
        builder = builder.with_dispatch_policy(policy);
    }
    let swarm = builder.build().await?;
    Ok(SwarmState {
        swarm: Arc::new(swarm),
        cost_ledger,
        dispatches: RwLock::new(Vec::new()),
        default_context: SwarmContextSpec::default(),
    })
}

/// Build a [`SwarmState`] backed by a [`NoopCostLedger`] and a stub
/// MCP backend. Used by integration tests so they do not need a real
/// MCP subprocess or credential setup.
pub async fn build_test_swarm_state(
    swarm_dir: impl Into<std::path::PathBuf>,
    cost_ledger: Arc<PersistentCostLedger>,
) -> eyre::Result<SwarmState> {
    let backend: Arc<dyn McpAgentBackend> = Arc::new(TestStubBackend::default());
    let swarm_dir = swarm_dir.into();
    let swarm = Swarm::builder(backend, &swarm_dir)
        // Wire a NoopCostLedger so the primitive's summarize never
        // contradicts the live PersistentCostLedger read.
        .with_ledger(Arc::new(NoopCostLedger))
        .build()
        .await?;
    Ok(SwarmState {
        swarm: Arc::new(swarm),
        cost_ledger,
        dispatches: RwLock::new(Vec::new()),
        default_context: SwarmContextSpec::default(),
    })
}

/// Test variant that wires the [`BroadcasterSwarmEventSink`] so integration
/// tests can assert swarm dispatch events hit the broadcaster.
pub async fn build_test_swarm_state_with_broadcaster(
    swarm_dir: impl Into<std::path::PathBuf>,
    cost_ledger: Arc<PersistentCostLedger>,
    broadcaster: Arc<super::EventBroadcaster>,
    sink_path: Option<String>,
) -> eyre::Result<SwarmState> {
    let backend: Arc<dyn McpAgentBackend> = Arc::new(TestStubBackend::default());
    let swarm_dir = swarm_dir.into();
    let sink: Arc<dyn SwarmEventSink> =
        Arc::new(BroadcasterSwarmEventSink::new(broadcaster, sink_path));
    let swarm = Swarm::builder(backend, &swarm_dir)
        .with_ledger(Arc::new(NoopCostLedger))
        .with_event_sink(sink)
        .build()
        .await?;
    Ok(SwarmState {
        swarm: Arc::new(swarm),
        cost_ledger,
        dispatches: RwLock::new(Vec::new()),
        default_context: SwarmContextSpec::default(),
    })
}

/// Test stub backend that succeeds every dispatch with a short output.
/// Used by [`build_test_swarm_state`] so the primitive can exercise
/// end-to-end without a real MCP subprocess.
#[derive(Debug, Default, Clone)]
pub struct TestStubBackend {
    pub output: String,
}

#[async_trait::async_trait]
impl McpAgentBackend for TestStubBackend {
    fn backend_label(&self) -> &'static str {
        "test_stub"
    }

    fn endpoint_label(&self) -> String {
        "test-stub".into()
    }

    async fn dispatch(&self, _request: DispatchRequest) -> DispatchResponse {
        DispatchResponse {
            outcome: DispatchOutcome::Success,
            output: if self.output.is_empty() {
                "ok".into()
            } else {
                self.output.clone()
            },
            files_to_send: Vec::new(),
            error: None,
            context_contract: None,
        }
    }
}

// ── SSE broadcaster helper ──────────────────────────────────────────

impl super::EventBroadcaster {
    /// Send a raw JSON-encoded frame through the broadcast channel. Used
    /// by the review endpoint to forward the typed
    /// [`HarnessEventPayload::SwarmReviewDecision`] event without
    /// pre-wrapping it into a `ProgressEvent`.
    pub fn send_raw(&self, payload: String) -> usize {
        self.tx_send(payload)
    }
}

// Re-export a subset of primitive types so integration tests in the CLI
// crate don't need to import `octos_swarm` directly.
pub use octos_swarm::ContractSpec as SwarmContractSpec;
pub use octos_swarm::MAX_RETRY_ROUNDS as SWARM_MAX_RETRY_ROUNDS;
pub use octos_swarm::SubtaskStatus as SwarmSubtaskStatus;

/// Expose the underlying primitive's topology ctor shorthand so the
/// tests don't need to re-import `NonZeroUsize`.
pub fn parallel_topology(concurrency: usize) -> SwarmTopology {
    SwarmTopology::Parallel {
        max_concurrency: NonZeroUsize::new(concurrency.max(1)).unwrap(),
    }
}
