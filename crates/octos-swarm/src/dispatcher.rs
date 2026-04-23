//! [`Swarm::dispatch`]: the core swarm orchestration primitive.
//!
//! Given a list of [`ContractSpec`], a [`SwarmTopology`], and a
//! [`SwarmBudget`], the dispatcher:
//!
//! 1. Resolves the effective contract list (expanding
//!    [`SwarmTopology::Fanout`] up front).
//! 2. Loads any prior [`DispatchRecord`] from the redb ledger so mid-
//!    dispatch restart is idempotent (invariant 1 + 7).
//! 3. Issues sub-contracts per topology rules:
//!    - [`Parallel`] / [`Fanout`]: bounded-concurrency fan-out via
//!      `tokio::task::JoinSet`.
//!    - [`Sequential`]: one-at-a-time with abort on the first terminal
//!      failure.
//!    - [`Pipeline`]: chain outputs into the next contract at key
//!      `pipeline_input`.
//! 4. Records every dispatch attempt with the [`CostLedger`] (stubbed
//!    until M7.4).
//! 5. Re-dispatches any retryable failures up to
//!    [`MAX_RETRY_ROUNDS`].
//! 6. After all sub-contracts reach terminal state, runs the aggregate
//!    M4.3 validator (if one is configured).
//! 7. Emits the typed
//!    [`HarnessEventPayload::SwarmDispatch`](octos_agent::harness_events::HarnessEventPayload::SwarmDispatch)
//!    event and increments
//!    `octos_swarm_dispatch_total{topology,outcome}`.

use std::path::PathBuf;
use std::sync::Arc;

use eyre::Result;
use metrics::counter;
use octos_agent::harness_events::{HarnessEvent, HarnessSwarmDispatchEvent};
use octos_agent::tools::mcp_agent::{
    DispatchOutcome, DispatchRequest, McpAgentBackend, record_dispatch,
};
use octos_agent::validators::{
    ValidatorInvocation, ValidatorOutcome, ValidatorPhase, ValidatorRunner,
};
use octos_agent::workspace_policy::Validator;
use octos_agent::{HarnessEventPayload, SWARM_DISPATCH_SCHEMA_VERSION};
use tokio::task::JoinSet;
use tracing::{debug, warn};

use crate::ledger::{CostLedger, NoopCostLedger, SwarmCostAttribution};
use crate::persistence::{DispatchRecord, DispatchStore};
use crate::result::{
    AggregateArtifact, SubtaskOutcome, SubtaskStatus, SwarmOutcomeKind, SwarmResult,
};
use crate::topology::{ContractSpec, MAX_CONTRACTS_PER_DISPATCH, SwarmTopology};

/// Maximum number of retry rounds the primitive performs before
/// surfacing a partial result. Bounded per invariant 5 so a flaky
/// sub-agent cannot consume unbounded cost.
pub const MAX_RETRY_ROUNDS: u32 = 3;

/// Budget and knobs passed to [`Swarm::dispatch`]. Kept deliberately
/// small today — M7.4's cost ledger adds per-dispatch cost ceilings
/// once that work lands.
#[derive(Debug, Clone, Default)]
pub struct SwarmBudget {
    /// Optional cap on total sub-contracts issued. Defaults to
    /// [`MAX_CONTRACTS_PER_DISPATCH`].
    pub max_contracts: Option<usize>,
    /// Optional cap on retry rounds. Defaults to [`MAX_RETRY_ROUNDS`].
    pub max_retry_rounds: Option<u32>,
}

impl SwarmBudget {
    pub(crate) fn effective_max_contracts(&self) -> usize {
        self.max_contracts
            .unwrap_or(MAX_CONTRACTS_PER_DISPATCH)
            .min(MAX_CONTRACTS_PER_DISPATCH)
    }

    pub(crate) fn effective_max_retry_rounds(&self) -> u32 {
        self.max_retry_rounds
            .unwrap_or(MAX_RETRY_ROUNDS)
            .min(MAX_RETRY_ROUNDS)
    }
}

/// Aggregate validator configuration. The primitive runs this after
/// all sub-contracts reach terminal state, using the M4.3
/// [`ValidatorRunner`] against the aggregate artifact.
#[derive(Clone)]
pub struct AggregateValidator {
    pub runner: ValidatorRunner,
    pub invocation: ValidatorInvocation,
    pub validators: Vec<Validator>,
}

impl std::fmt::Debug for AggregateValidator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AggregateValidator")
            .field("invocation", &self.invocation)
            .field("validator_count", &self.validators.len())
            .finish()
    }
}

/// Supervisor identifiers folded into every typed [`HarnessEvent`] the
/// primitive emits. Forwarded verbatim — the supervisor chooses the
/// granularity (e.g. `session_id = "matrix:room:abc"`,
/// `task_id = "contract-123"`).
#[derive(Debug, Clone)]
pub struct SwarmContext {
    pub session_id: String,
    pub task_id: String,
    pub workflow: Option<String>,
    pub phase: Option<String>,
}

/// Sink trait for consumers that want structured events streamed
/// alongside the returned [`SwarmResult`]. Default
/// [`NoopSwarmEventSink`] discards events.
pub trait SwarmEventSink: Send + Sync {
    fn emit(&self, event: &HarnessEvent);
}

/// Discards every event. Used when no sink is configured.
#[derive(Debug, Default, Clone)]
pub struct NoopSwarmEventSink;

impl SwarmEventSink for NoopSwarmEventSink {
    fn emit(&self, _event: &HarnessEvent) {}
}

/// The swarm orchestration primitive. Construct via
/// [`Swarm::builder`] and inject the shared backend, cost ledger,
/// persistence dir, and optional aggregate validator.
pub struct Swarm {
    backend: Arc<dyn McpAgentBackend>,
    ledger: Arc<dyn CostLedger>,
    store: DispatchStore,
    validator: Option<AggregateValidator>,
    event_sink: Arc<dyn SwarmEventSink>,
}

impl Swarm {
    /// Start building a [`Swarm`]. Required inputs are the MCP backend
    /// and the persistence directory; optional inputs (cost ledger,
    /// aggregate validator, event sink) default to their no-op variants.
    pub fn builder(
        backend: Arc<dyn McpAgentBackend>,
        persistence_dir: impl Into<PathBuf>,
    ) -> SwarmBuilder {
        SwarmBuilder::new(backend, persistence_dir.into())
    }

    /// Dispatch a batch of contracts against the configured backend.
    ///
    /// # Invariants
    /// - Idempotent given same `(contracts, topology, budget)` +
    ///   `dispatch_id`: a second call finds the existing record and
    ///   returns the finalized result without re-dispatching completed
    ///   contracts.
    /// - Fan-out is bounded by [`SwarmTopology::max_concurrency`].
    /// - Sequential aborts on the first terminal failure.
    /// - Pipeline chains `output -> task.pipeline_input` for the next
    ///   contract.
    /// - Retry budget honours [`SwarmBudget::effective_max_retry_rounds`].
    /// - The aggregate validator runs once, after every sub-contract
    ///   reaches terminal state.
    pub async fn dispatch(
        &self,
        dispatch_id: impl Into<String>,
        contracts: Vec<ContractSpec>,
        topology: SwarmTopology,
        budget: SwarmBudget,
        context: SwarmContext,
    ) -> Result<SwarmResult> {
        let dispatch_id = dispatch_id.into();
        let resolved = topology.resolve_contracts(&contracts);

        if resolved.is_empty() {
            eyre::bail!("swarm dispatch requires at least one contract");
        }
        if resolved.len() > budget.effective_max_contracts() {
            eyre::bail!(
                "swarm dispatch exceeds max contracts ({} > {})",
                resolved.len(),
                budget.effective_max_contracts()
            );
        }

        // Load or initialise the durable record. Idempotency invariant:
        // a prior finalized record short-circuits the whole loop.
        let mut record = match self.store.load(&dispatch_id).await? {
            Some(existing) if existing.finalized => {
                debug!(dispatch_id = %dispatch_id, "reusing finalized swarm record");
                return Ok(self.result_from_record(&existing, Vec::new(), None).await);
            }
            Some(existing) => existing,
            None => DispatchRecord::new(
                dispatch_id.clone(),
                context.session_id.clone(),
                context.task_id.clone(),
                topology.clone(),
                resolved
                    .iter()
                    .map(|contract| {
                        SubtaskOutcome::pending(
                            contract.contract_id.clone(),
                            contract.label.clone(),
                        )
                    })
                    .collect(),
            ),
        };

        // Persist the freshly-initialised record before doing any work
        // so a crash between entry and first dispatch leaves a
        // replayable record behind.
        self.store.store(&record).await?;

        let max_rounds = budget.effective_max_retry_rounds();
        let mut round: u32 = record.retry_rounds_used;

        loop {
            let pending_indices: Vec<usize> = record
                .subtasks
                .iter()
                .enumerate()
                .filter_map(|(idx, outcome)| {
                    matches!(outcome.status, SubtaskStatus::RetryableFailed).then_some(idx)
                })
                .collect();

            if pending_indices.is_empty() {
                break;
            }

            debug!(
                dispatch_id = %dispatch_id,
                round,
                pending = pending_indices.len(),
                "dispatching swarm round"
            );

            match topology {
                SwarmTopology::Parallel { .. } | SwarmTopology::Fanout { .. } => {
                    self.run_parallel_round(&mut record, &resolved, &pending_indices, &topology)
                        .await?;
                }
                SwarmTopology::Sequential => {
                    let aborted = self
                        .run_sequential_round(&mut record, &resolved, &pending_indices)
                        .await?;
                    if aborted {
                        self.store.store(&record).await?;
                        break;
                    }
                }
                SwarmTopology::Pipeline => {
                    let aborted = self
                        .run_pipeline_round(&mut record, &resolved, &pending_indices)
                        .await?;
                    if aborted {
                        self.store.store(&record).await?;
                        break;
                    }
                }
            }

            record.retry_rounds_used = round + 1;
            self.store.store(&record).await?;

            round += 1;
            if round >= max_rounds {
                break;
            }
        }

        // Aggregate validator (M4.3) runs AFTER every sub-contract
        // reached a terminal state. We snapshot a preliminary
        // [`SwarmResult`] so the validator can see the aggregate
        // artifact exactly as the supervisor will see it.
        let validator_results = self.run_aggregate_validator(&record).await;

        // Summarize roll-up from the wired ledger adapter.
        let total_cost_usd = self.ledger.summarize(&record.dispatch_id).await;

        let result = SwarmResult::from_parts(
            record.dispatch_id.clone(),
            &topology,
            record.subtasks.clone(),
            validator_results,
            total_cost_usd,
            record.retry_rounds_used,
        );

        // Mark the record finalized so a future restart short-circuits.
        record.finalized = true;
        self.store.store(&record).await?;

        let event = build_event(&result, &context);
        self.event_sink.emit(&event);
        record_swarm_metric(&result.topology, result.outcome);

        Ok(result)
    }

    async fn run_parallel_round(
        &self,
        record: &mut DispatchRecord,
        contracts: &[ContractSpec],
        pending: &[usize],
        topology: &SwarmTopology,
    ) -> Result<()> {
        let concurrency = topology.max_concurrency().max(1);
        let mut iter = pending.iter().copied();
        let mut active: JoinSet<(usize, SubtaskOutcome)> = JoinSet::new();

        // Prime the join set up to the concurrency limit.
        for _ in 0..concurrency {
            let Some(idx) = iter.next() else {
                break;
            };
            self.spawn_subtask(&mut active, contracts, idx, record.subtasks[idx].attempts);
        }

        while let Some(join) = active.join_next().await {
            let (idx, outcome) = match join {
                Ok(result) => result,
                Err(error) => {
                    warn!(error = %error, "swarm subtask join failed");
                    continue;
                }
            };
            // Forward attribution to the wired ledger adapter.
            self.attribute_cost(record, contracts, idx, &outcome).await;
            record.subtasks[idx] = outcome;

            if let Some(next_idx) = iter.next() {
                self.spawn_subtask(
                    &mut active,
                    contracts,
                    next_idx,
                    record.subtasks[next_idx].attempts,
                );
            }
        }

        Ok(())
    }

    async fn run_sequential_round(
        &self,
        record: &mut DispatchRecord,
        contracts: &[ContractSpec],
        pending: &[usize],
    ) -> Result<bool> {
        for idx in pending {
            let contract = &contracts[*idx];
            let attempts = record.subtasks[*idx].attempts;
            let outcome = dispatch_once(self.backend.as_ref(), contract, attempts).await;
            // Forward attribution to the wired ledger adapter.
            self.attribute_cost(record, contracts, *idx, &outcome).await;
            let is_terminal = outcome.status == SubtaskStatus::TerminalFailed;
            record.subtasks[*idx] = outcome;
            if is_terminal {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn run_pipeline_round(
        &self,
        record: &mut DispatchRecord,
        contracts: &[ContractSpec],
        pending: &[usize],
    ) -> Result<bool> {
        for idx in pending {
            let mut contract = contracts[*idx].clone();
            // Pipeline invariant 4: fold the previous completed
            // subtask's output into this task's `pipeline_input` key.
            let prior_output = if *idx > 0 {
                match record.subtasks[*idx - 1].status {
                    SubtaskStatus::Completed => Some(record.subtasks[*idx - 1].output.clone()),
                    _ => None,
                }
            } else {
                None
            };
            if let Some(prev) = prior_output {
                if let serde_json::Value::Object(ref mut obj) = contract.task {
                    obj.insert("pipeline_input".into(), serde_json::Value::String(prev));
                } else {
                    contract.task = serde_json::json!({
                        "original_task": contract.task,
                        "pipeline_input": prev,
                    });
                }
            }
            let attempts = record.subtasks[*idx].attempts;
            let outcome = dispatch_once(self.backend.as_ref(), &contract, attempts).await;
            // Forward attribution to the wired ledger adapter.
            self.attribute_cost(record, contracts, *idx, &outcome).await;
            let is_terminal = outcome.status == SubtaskStatus::TerminalFailed;
            record.subtasks[*idx] = outcome;
            if is_terminal {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn spawn_subtask(
        &self,
        set: &mut JoinSet<(usize, SubtaskOutcome)>,
        contracts: &[ContractSpec],
        idx: usize,
        attempts: u32,
    ) {
        let backend = Arc::clone(&self.backend);
        let contract = contracts[idx].clone();
        set.spawn(async move {
            let outcome = dispatch_once(backend.as_ref(), &contract, attempts).await;
            (idx, outcome)
        });
    }

    async fn attribute_cost(
        &self,
        record: &DispatchRecord,
        contracts: &[ContractSpec],
        idx: usize,
        outcome: &SubtaskOutcome,
    ) {
        // Forward attribution to the wired ledger adapter — writes through
        // the [`NoopCostLedger`] unless an integration test injects a
        // spy. When M7.4 lands, replace `SwarmCostAttribution` with the
        // shared `CostAttributionEvent` and include token counts.
        self.ledger
            .attribute(&SwarmCostAttribution {
                dispatch_id: record.dispatch_id.clone(),
                contract_id: contracts[idx].contract_id.clone(),
                backend: self.backend.backend_label().to_string(),
                endpoint: self.backend.endpoint_label(),
                outcome: outcome.last_dispatch_outcome.clone(),
                attempt: Some(outcome.attempts),
            })
            .await;
    }

    async fn run_aggregate_validator(&self, record: &DispatchRecord) -> Vec<ValidatorOutcome> {
        let Some(ref validator) = self.validator else {
            return Vec::new();
        };
        // Surface the combined aggregate text to the validator by
        // running against the real validator list. The
        // [`ValidatorRunner`] itself inspects the workspace (M4.3
        // validators are workspace-scoped); the invocation's
        // `repo_label` carries the aggregate identity.
        let filtered: Vec<Validator> = validator
            .validators
            .iter()
            .filter(|v| ValidatorPhase::from(v.phase) == ValidatorPhase::Completion)
            .cloned()
            .collect();

        if filtered.is_empty() {
            debug!(dispatch_id = %record.dispatch_id, "no completion-phase validators configured");
            return Vec::new();
        }

        validator
            .runner
            .run_all(&validator.invocation, &filtered)
            .await
    }

    async fn result_from_record(
        &self,
        record: &DispatchRecord,
        validator_results: Vec<ValidatorOutcome>,
        cost: Option<f64>,
    ) -> SwarmResult {
        SwarmResult::from_parts(
            record.dispatch_id.clone(),
            &record.topology,
            record.subtasks.clone(),
            validator_results,
            cost,
            record.retry_rounds_used,
        )
    }
}

/// Builder for [`Swarm`]. Wires the optional ledger / validator /
/// event sink + async-opens the redb ledger.
pub struct SwarmBuilder {
    backend: Arc<dyn McpAgentBackend>,
    persistence_dir: PathBuf,
    ledger: Arc<dyn CostLedger>,
    validator: Option<AggregateValidator>,
    event_sink: Arc<dyn SwarmEventSink>,
}

impl SwarmBuilder {
    fn new(backend: Arc<dyn McpAgentBackend>, persistence_dir: PathBuf) -> Self {
        Self {
            backend,
            persistence_dir,
            ledger: Arc::new(NoopCostLedger),
            validator: None,
            event_sink: Arc::new(NoopSwarmEventSink),
        }
    }

    /// Override the cost ledger. Without this the primitive uses
    /// [`NoopCostLedger`] and `SwarmResult::total_cost_usd` stays
    /// `None`.
    pub fn with_ledger(mut self, ledger: Arc<dyn CostLedger>) -> Self {
        self.ledger = ledger;
        self
    }

    /// Configure the aggregate M4.3 validator that runs after every
    /// sub-contract reaches terminal state.
    pub fn with_validator(mut self, validator: AggregateValidator) -> Self {
        self.validator = Some(validator);
        self
    }

    /// Route emitted events through `sink`.
    pub fn with_event_sink(mut self, sink: Arc<dyn SwarmEventSink>) -> Self {
        self.event_sink = sink;
        self
    }

    /// Open the redb ledger and return the usable [`Swarm`].
    pub async fn build(self) -> Result<Swarm> {
        let store = DispatchStore::open(&self.persistence_dir).await?;
        Ok(Swarm {
            backend: self.backend,
            ledger: self.ledger,
            store,
            validator: self.validator,
            event_sink: self.event_sink,
        })
    }
}

async fn dispatch_once(
    backend: &dyn McpAgentBackend,
    contract: &ContractSpec,
    prior_attempts: u32,
) -> SubtaskOutcome {
    let request = DispatchRequest {
        tool_name: contract.tool_name.clone(),
        task: contract.task.clone(),
    };
    let response = backend.dispatch(request).await;
    record_dispatch(backend.backend_label(), response.outcome);

    let status = SubtaskStatus::from_dispatch(response.outcome);
    let mut outcome = SubtaskOutcome {
        contract_id: contract.contract_id.clone(),
        label: contract.label.clone(),
        status,
        attempts: prior_attempts.saturating_add(1),
        last_dispatch_outcome: response.outcome.as_str().to_string(),
        output: response.output,
        files_to_send: response.files_to_send,
        error: response.error,
    };
    // If the dispatch returned an error body, preserve it — empty
    // output means the next retry has no stale payload to confuse the
    // pipeline step with.
    if !matches!(response.outcome, DispatchOutcome::Success) && outcome.output.is_empty() {
        if let Some(ref err) = outcome.error {
            outcome.output = err.clone();
        }
    }
    outcome
}

fn build_event(result: &SwarmResult, context: &SwarmContext) -> HarnessEvent {
    let mut message = None;
    if matches!(
        result.outcome,
        SwarmOutcomeKind::Failed | SwarmOutcomeKind::Partial | SwarmOutcomeKind::Aborted
    ) {
        // Surface the first non-completed subtask's error so
        // supervisors have an actionable hint without paging through
        // the full record.
        message = result
            .per_task_outcomes
            .iter()
            .find(|outcome| outcome.status != SubtaskStatus::Completed)
            .and_then(|outcome| outcome.error.clone());
    }

    let event = HarnessSwarmDispatchEvent {
        schema_version: SWARM_DISPATCH_SCHEMA_VERSION,
        session_id: context.session_id.clone(),
        task_id: context.task_id.clone(),
        workflow: context.workflow.clone(),
        phase: context.phase.clone(),
        dispatch_id: result.dispatch_id.clone(),
        topology: result.topology.clone(),
        outcome: result.outcome.as_str().to_string(),
        total_subtasks: Some(result.total_subtasks),
        completed_subtasks: Some(result.completed_subtasks),
        retry_round: Some(result.retry_rounds_used),
        message,
        extra: Default::default(),
    };

    HarnessEvent {
        schema: octos_agent::HARNESS_EVENT_SCHEMA_V1.to_string(),
        payload: HarnessEventPayload::SwarmDispatch { data: event },
    }
}

fn record_swarm_metric(topology: &str, outcome: SwarmOutcomeKind) {
    counter!(
        "octos_swarm_dispatch_total",
        "topology" => topology.to_string(),
        "outcome" => outcome.as_str().to_string()
    )
    .increment(1);
}

/// Emit a rolled-up aggregate artifact view for supervisors that want
/// to stream the combined output without walking the full result.
#[must_use]
pub fn flatten_aggregate(result: &SwarmResult) -> AggregateArtifact {
    result.aggregate_artifact.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_defaults_to_max_rounds() {
        let budget = SwarmBudget::default();
        assert_eq!(budget.effective_max_retry_rounds(), MAX_RETRY_ROUNDS);
        assert_eq!(budget.effective_max_contracts(), MAX_CONTRACTS_PER_DISPATCH);
    }

    #[test]
    fn budget_clamps_to_max_retry_rounds() {
        let budget = SwarmBudget {
            max_contracts: None,
            max_retry_rounds: Some(100),
        };
        assert_eq!(budget.effective_max_retry_rounds(), MAX_RETRY_ROUNDS);
    }

    #[test]
    fn budget_clamps_to_max_contracts() {
        let budget = SwarmBudget {
            max_contracts: Some(10_000),
            max_retry_rounds: None,
        };
        assert_eq!(budget.effective_max_contracts(), MAX_CONTRACTS_PER_DISPATCH);
    }
}
