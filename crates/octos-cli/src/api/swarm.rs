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
//!    [`HarnessEventPayload::SwarmReviewDecision`] events through the
//!    SSE broadcaster so the UI and any Matrix audit channel see the
//!    same record.
//! 4. All persisted / event shapes carry `schema_version: u32` pinned
//!    in [`abi_schema`](octos_agent::abi_schema).

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::{Arc, RwLock};

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use octos_agent::tools::mcp_agent::{
    DispatchOutcome, DispatchRequest, DispatchResponse, McpAgentBackend,
};
use octos_agent::{
    CostLedger, HarnessEvent, HarnessSwarmReviewDecisionEvent, PersistentCostLedger,
    SWARM_REVIEW_DECISION_SCHEMA_VERSION,
};
use octos_swarm::{
    ContractSpec, NoopCostLedger, SubtaskOutcome, SubtaskStatus, Swarm, SwarmBudget, SwarmContext,
    SwarmOutcomeKind, SwarmResult, SwarmTopology,
};
use serde::{Deserialize, Serialize};

use super::AppState;

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
    /// Re-dispatching an existing id overwrites the row.
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
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("dispatch failed: {e}"),
            )
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
    if req.contract_id.trim().is_empty() {
        return Err("contract_id cannot be empty".into());
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
        if contract.tool_name.trim().is_empty() {
            return Err("contract.tool_name cannot be empty".into());
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
pub async fn submit_review(
    State(state): State<Arc<AppState>>,
    Path(dispatch_id): Path<String>,
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

/// Build a [`SwarmState`] given a backend and data directory. The backend
/// is injected by the caller so the dashboard can swap transports (local
/// stdio, remote HTTPS) without this module growing a backend factory.
pub async fn build_swarm_state(
    backend: Arc<dyn McpAgentBackend>,
    swarm_dir: impl Into<std::path::PathBuf>,
    cost_ledger: Arc<PersistentCostLedger>,
) -> eyre::Result<SwarmState> {
    let swarm_dir = swarm_dir.into();
    let swarm = Swarm::builder(backend, &swarm_dir).build().await?;
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
        }
    }
}

// ── SSE broadcaster helper ──────────────────────────────────────────

impl super::SseBroadcaster {
    /// Send a raw JSON-encoded frame through the broadcast channel. Used
    /// by the review endpoint to forward the typed
    /// [`HarnessEventPayload::SwarmReviewDecision`] event without
    /// pre-wrapping it into a `ProgressEvent`.
    pub fn send_raw(&self, payload: String) -> usize {
        self.tx_send(payload)
    }
}

/// Consume a [`HarnessEvent`] for local unit tests — used to validate
/// the variant round-trips through the dashboard path.
#[cfg(test)]
pub(crate) fn assert_event_is_review_decision(event: &HarnessEvent, dispatch_id: &str) {
    match &event.payload {
        octos_agent::HarnessEventPayload::SwarmReviewDecision { data } => {
            assert_eq!(data.dispatch_id, dispatch_id);
        }
        _ => panic!("expected SwarmReviewDecision variant"),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroUsize;

    #[test]
    fn dispatch_request_rejects_empty_dispatch_id() {
        let req = SwarmDispatchRequest {
            schema_version: 1,
            dispatch_id: String::new(),
            contract_id: "c1".into(),
            contracts: vec![ContractSpec {
                contract_id: "sub".into(),
                tool_name: "run".into(),
                task: serde_json::json!({}),
                label: None,
            }],
            topology: SwarmTopology::Parallel {
                max_concurrency: NonZeroUsize::new(1).unwrap(),
            },
            budget: SwarmBudgetSpec::default(),
            context: None,
        };
        assert!(validate_dispatch_request(&req).is_err());
    }

    #[test]
    fn dispatch_request_rejects_empty_contracts_for_parallel() {
        let req = SwarmDispatchRequest {
            schema_version: 1,
            dispatch_id: "d1".into(),
            contract_id: "c1".into(),
            contracts: vec![],
            topology: SwarmTopology::Parallel {
                max_concurrency: NonZeroUsize::new(1).unwrap(),
            },
            budget: SwarmBudgetSpec::default(),
            context: None,
        };
        assert!(validate_dispatch_request(&req).is_err());
    }

    #[test]
    fn dispatch_request_rejects_retry_budget_over_bound() {
        let req = SwarmDispatchRequest {
            schema_version: 1,
            dispatch_id: "d1".into(),
            contract_id: "c1".into(),
            contracts: vec![ContractSpec {
                contract_id: "sub".into(),
                tool_name: "run".into(),
                task: serde_json::json!({}),
                label: None,
            }],
            topology: SwarmTopology::Sequential,
            budget: SwarmBudgetSpec {
                max_contracts: None,
                max_retry_rounds: Some(9_999),
            },
            context: None,
        };
        assert!(validate_dispatch_request(&req).is_err());
    }

    #[test]
    fn dispatch_request_accepts_valid_input() {
        let req = SwarmDispatchRequest {
            schema_version: 1,
            dispatch_id: "d1".into(),
            contract_id: "c1".into(),
            contracts: vec![ContractSpec {
                contract_id: "sub".into(),
                tool_name: "run".into(),
                task: serde_json::json!({}),
                label: None,
            }],
            topology: SwarmTopology::Parallel {
                max_concurrency: NonZeroUsize::new(1).unwrap(),
            },
            budget: SwarmBudgetSpec::default(),
            context: None,
        };
        assert!(validate_dispatch_request(&req).is_ok());
    }

    #[test]
    fn parallel_topology_helper_builds_non_zero() {
        let topo = parallel_topology(4);
        assert_eq!(topo.max_concurrency(), 4);
    }

    #[test]
    fn parallel_topology_helper_clamps_zero_to_one() {
        let topo = parallel_topology(0);
        assert_eq!(topo.max_concurrency(), 1);
    }
}
