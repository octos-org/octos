//! Cost / provenance ledger for swarm sub-agent dispatches (M7.4).
//!
//! Every MCP-backed sub-agent dispatch from [`crate::tools::mcp_agent`]
//! that lands with `outcome == "success"` (i.e. a ready contract-gated
//! artifact) records a [`CostAttributionEvent`] in the ledger. Operators
//! get a durable per-contract audit trail tying spend back to the
//! supervisor session, the contract, the model, token volume, and a USD
//! projection via [`octos_llm::pricing`].
//!
//! The ledger is schema-versioned via
//! [`COST_ATTRIBUTION_SCHEMA_VERSION`](crate::abi_schema::COST_ATTRIBUTION_SCHEMA_VERSION).
//! Downstream tooling that reads the typed
//! [`HarnessEventPayload::CostAttribution`](crate::harness_events::HarnessEventPayload::CostAttribution)
//! event or inspects rows directly must honour the version gate so
//! additive fields stay backward compatible.
//!
//! # Storage
//!
//! The redb database follows the M6.5 credential-pool pattern:
//!
//! - `begin_write()` / `commit()` for atomic writes.
//! - `tokio::task::spawn_blocking` wrappers so the async caller never
//!   blocks the runtime on disk I/O.
//! - One record per dispatch, keyed by the event's UUIDv7 ID so
//!   insertions are lock-free and ordering on read reflects dispatch time.
//! - Default on-disk path: `~/.octos/cost_ledger.redb`. Tests use
//!   [`PersistentCostLedger::open`] with a tempdir path.
//!
//! # Budget enforcement
//!
//! [`CostBudgetPolicy`] is OPTIONAL. Absent a policy, the ledger records
//! attributions without enforcement. With a policy configured, callers
//! feed the declared-model rate and a tokens-in estimate into
//! [`CostBudgetPolicy::project`] and reject the dispatch before spawn if
//! the projection breaches any threshold. Budgets can be per-dispatch,
//! per-contract, or global — whichever is most restrictive wins.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use metrics::{counter, histogram};
use redb::{Database, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::abi_schema::COST_ATTRIBUTION_SCHEMA_VERSION;

/// Table for cost attributions: key = attribution_id, value = JSON
const ATTRIBUTIONS_TABLE: TableDefinition<&str, &str> = TableDefinition::new("attributions");
/// Index table for per-contract rollups: key = contract_id, value = list of attribution_ids (JSON).
const CONTRACT_INDEX_TABLE: TableDefinition<&str, &str> = TableDefinition::new("contract_index");
/// Index table for per-session rollups: key = supervisor_session, value = list of attribution_ids (JSON).
const SESSION_INDEX_TABLE: TableDefinition<&str, &str> = TableDefinition::new("session_index");

/// Stable label prefix for the per-process `octos_cost_attribution_total`
/// counter. Labeled with `model` and `outcome` so the operator
/// aggregation in `crates/octos-cli/src/api/metrics.rs` can produce a
/// per-model cost breakdown without re-reading the ledger.
pub const COST_ATTRIBUTION_COUNTER: &str = "octos_cost_attribution_total";

/// Histogram capturing the USD projection of every committed
/// attribution. Bucket widths are chosen by the Prometheus recorder.
pub const COST_USD_HISTOGRAM: &str = "octos_cost_usd";

/// Durable filename inside the data directory. Exposed so callers can
/// stitch the path together if they want to use a custom data dir.
pub const COST_LEDGER_FILE: &str = "cost_ledger.redb";

/// Typed cost / provenance record. Persisted verbatim to redb and also
/// surfaced as the payload of a typed
/// [`HarnessEventPayload::CostAttribution`](crate::harness_events::HarnessEventPayload::CostAttribution)
/// event.
///
/// Fields are additive — new fields MUST be `#[serde(default)]` so old
/// rows keep round-tripping after a schema bump. The `schema_version`
/// field is mandatory to let readers detect incompatible formats
/// upfront and refuse rather than silently dropping data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostAttributionEvent {
    #[serde(default = "default_cost_attribution_schema_version")]
    pub schema_version: u32,
    /// Stable UUIDv7-style ID generated on record. Used as the redb
    /// primary key and propagated to the typed event so downstream
    /// consumers can dedupe replays.
    pub attribution_id: String,
    /// Supervising session that initiated the dispatch. Matches the
    /// `session_id` propagated through
    /// [`crate::harness_events::OCTOS_HARNESS_SESSION_ID_ENV`].
    pub supervisor_session: String,
    /// Opaque contract identifier (typically the workspace contract
    /// artifact path or the workflow slug). Allows per-contract cost
    /// rollups without touching individual rows.
    pub contract_id: String,
    /// Task identifier — mirrors the sub-agent dispatch task id.
    pub task_id: String,
    /// Model key declared by the sub-agent (e.g. `anthropic/claude-haiku`).
    pub model: String,
    /// Prompt / input tokens reported by the sub-agent or estimated by
    /// the dispatcher.
    pub tokens_in: u32,
    /// Completion / output tokens reported by the sub-agent.
    pub tokens_out: u32,
    /// Projected USD cost at record time. Computed via
    /// [`octos_llm::pricing::model_pricing`] when available; falls back
    /// to 0.0 for unknown models so the record still lands.
    pub cost_usd: f64,
    /// Record creation timestamp (RFC3339 UTC).
    pub timestamp: String,
    /// Optional workflow label. Kept as `Option<String>` so older rows
    /// remain readable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    /// Optional workflow phase label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    /// MCP backend label (`"local"` / `"remote"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    /// Dispatch outcome (`"success"`, `"remote_error"`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
}

fn default_cost_attribution_schema_version() -> u32 {
    COST_ATTRIBUTION_SCHEMA_VERSION
}

impl CostAttributionEvent {
    /// Construct a fresh attribution record with a generated id and
    /// current UTC timestamp. Callers who already have an id (e.g. when
    /// replaying) should populate the struct directly and call
    /// [`CostLedger::record`] instead.
    pub fn new(
        supervisor_session: impl Into<String>,
        contract_id: impl Into<String>,
        task_id: impl Into<String>,
        model: impl Into<String>,
        tokens_in: u32,
        tokens_out: u32,
        cost_usd: f64,
    ) -> Self {
        Self {
            schema_version: COST_ATTRIBUTION_SCHEMA_VERSION,
            attribution_id: generate_attribution_id(),
            supervisor_session: supervisor_session.into(),
            contract_id: contract_id.into(),
            task_id: task_id.into(),
            model: model.into(),
            tokens_in,
            tokens_out,
            cost_usd,
            timestamp: chrono::Utc::now().to_rfc3339(),
            workflow: None,
            phase: None,
            backend: None,
            outcome: None,
        }
    }

    /// Attach optional workflow metadata without forcing callers into
    /// the full constructor.
    pub fn with_workflow(mut self, workflow: Option<String>, phase: Option<String>) -> Self {
        self.workflow = workflow;
        self.phase = phase;
        self
    }

    /// Attach backend / outcome labels so operators can filter the
    /// ledger by dispatch type without re-keying off the
    /// `SubAgentDispatch` event stream.
    pub fn with_backend_outcome(
        mut self,
        backend: Option<String>,
        outcome: Option<String>,
    ) -> Self {
        self.backend = backend;
        self.outcome = outcome;
        self
    }
}

/// Project a USD cost using the declared model rate and a tokens-in
/// estimate. Returns `Some(0.0)` when the model is known but both token
/// counts are zero (operators can still distinguish that from an
/// unknown model by checking the [`octos_llm::pricing::model_pricing`]
/// return value directly). Returns `None` for unknown models.
pub fn project_cost_usd(model: &str, tokens_in: u32, tokens_out: u32) -> Option<f64> {
    octos_llm::pricing::model_pricing(model).map(|pricing| pricing.cost(tokens_in, tokens_out))
}

/// Generate a UUIDv7-like, time-sortable, globally unique id without
/// pulling the full `uuid` crate as a new dependency. Uses
/// nanosecond-since-epoch + a random 32-bit suffix so two concurrent
/// dispatches in the same nanosecond still disambiguate.
fn generate_attribution_id() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("cost-{nanos:x}-{seq:08x}")
}

/// Abstract cost ledger. Implementations MUST be thread-safe (the
/// harness shares a single instance across sub-agent dispatches) and
/// MUST persist durable writes before returning `Ok(())` so crashes in
/// the supervisor process do not lose attributions.
#[async_trait]
pub trait CostLedger: Send + Sync {
    /// Persist a single attribution row. Returning `Ok(())` promises
    /// the event is durable — callers rely on this for the
    /// "ledger survives process restart" invariant.
    async fn record(&self, event: CostAttributionEvent) -> Result<()>;

    /// Return all attributions for a given contract id, sorted by
    /// insertion order (which matches dispatch time for the UUIDv7-like
    /// key format).
    async fn list_for_contract(&self, contract_id: &str) -> Result<Vec<CostAttributionEvent>>;

    /// Return all attributions for a supervisor session.
    async fn list_for_session(&self, session_id: &str) -> Result<Vec<CostAttributionEvent>>;

    /// Aggregate attributions into per-contract rollups. Returned list
    /// is sorted by descending total spend so the operator summary can
    /// surface the top contracts first without extra post-processing.
    async fn aggregate_per_contract(&self) -> Result<Vec<ContractCostRollup>>;
}

/// Aggregated cost totals for a single contract. Used by the operator
/// summary extension to show a per-contract spend breakdown.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContractCostRollup {
    pub contract_id: String,
    pub dispatch_count: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cost_usd: f64,
}

/// Redb-backed [`CostLedger`] mirroring the M6.5 credential-pool
/// storage pattern. All writes go through a single `begin_write()` /
/// `commit()` cycle so the row and both index updates stay atomic.
pub struct PersistentCostLedger {
    db: Arc<Database>,
}

impl PersistentCostLedger {
    /// Default storage path under the user's home directory. Matches
    /// the `~/.octos/` convention used by the auth store and episode
    /// database.
    pub fn home_default_path() -> Option<PathBuf> {
        dirs::home_dir().map(|h| h.join(".octos").join(COST_LEDGER_FILE))
    }

    /// Open or create a ledger at `data_dir`. The redb file is created
    /// inside `data_dir` with the stable [`COST_LEDGER_FILE`] name.
    pub async fn open(data_dir: impl AsRef<Path>) -> Result<Self> {
        let data_dir = data_dir.as_ref().to_path_buf();
        tokio::fs::create_dir_all(&data_dir)
            .await
            .wrap_err("failed to create cost ledger directory")?;

        let db_path = data_dir.join(COST_LEDGER_FILE);
        let db = tokio::task::spawn_blocking(move || {
            let db = Database::create(&db_path).wrap_err("failed to open cost ledger database")?;
            // Initialise tables so empty reads never error out.
            let write_txn = db.begin_write()?;
            {
                let _ = write_txn.open_table(ATTRIBUTIONS_TABLE)?;
                let _ = write_txn.open_table(CONTRACT_INDEX_TABLE)?;
                let _ = write_txn.open_table(SESSION_INDEX_TABLE)?;
            }
            write_txn.commit()?;
            debug!(path = %db_path.display(), "opened cost ledger");
            Ok::<_, eyre::Report>(db)
        })
        .await??;
        Ok(Self { db: Arc::new(db) })
    }

    /// Open the default `~/.octos/cost_ledger.redb` ledger. Fails
    /// cleanly if the home directory cannot be resolved.
    pub async fn open_default() -> Result<Self> {
        let path = Self::home_default_path()
            .ok_or_else(|| eyre::eyre!("could not determine home directory for cost ledger"))?;
        let data_dir = path
            .parent()
            .ok_or_else(|| eyre::eyre!("cost ledger path has no parent directory"))?
            .to_path_buf();
        Self::open(&data_dir).await
    }

    fn append_index(
        txn: &redb::WriteTransaction,
        table: TableDefinition<&'static str, &'static str>,
        key: &str,
        attribution_id: &str,
    ) -> Result<()> {
        let mut table = txn.open_table(table)?;
        let existing: Vec<String> = table
            .get(key)?
            .map(|v| serde_json::from_str::<Vec<String>>(v.value()).unwrap_or_default())
            .unwrap_or_default();
        let mut ids = existing;
        if !ids.iter().any(|id| id == attribution_id) {
            ids.push(attribution_id.to_string());
        }
        let ids_json =
            serde_json::to_string(&ids).wrap_err("failed to serialize cost ledger index entry")?;
        table.insert(key, ids_json.as_str())?;
        Ok(())
    }

    fn load_by_ids(
        txn: &redb::ReadTransaction,
        ids: &[String],
    ) -> Result<Vec<CostAttributionEvent>> {
        let table = txn.open_table(ATTRIBUTIONS_TABLE)?;
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(json) = table.get(id.as_str())? {
                match serde_json::from_str::<CostAttributionEvent>(json.value()) {
                    Ok(event) => out.push(event),
                    Err(error) => {
                        warn!(id = id.as_str(), error = %error, "skipping corrupt ledger row")
                    }
                }
            }
        }
        Ok(out)
    }
}

#[async_trait]
impl CostLedger for PersistentCostLedger {
    async fn record(&self, event: CostAttributionEvent) -> Result<()> {
        let db = self.db.clone();
        let id = event.attribution_id.clone();
        let contract = event.contract_id.clone();
        let session = event.supervisor_session.clone();
        let model = event.model.clone();
        let outcome_label = event
            .outcome
            .clone()
            .unwrap_or_else(|| "success".to_string());
        let cost_usd = event.cost_usd;
        let body =
            serde_json::to_string(&event).wrap_err("failed to serialize cost attribution")?;

        tokio::task::spawn_blocking(move || {
            let write_txn = db.begin_write()?;
            {
                let mut table = write_txn.open_table(ATTRIBUTIONS_TABLE)?;
                table.insert(id.as_str(), body.as_str())?;
            }
            Self::append_index(&write_txn, CONTRACT_INDEX_TABLE, &contract, &id)?;
            Self::append_index(&write_txn, SESSION_INDEX_TABLE, &session, &id)?;
            write_txn.commit()?;
            Ok::<_, eyre::Report>(())
        })
        .await??;

        counter!(
            COST_ATTRIBUTION_COUNTER,
            "model" => model,
            "outcome" => outcome_label
        )
        .increment(1);
        histogram!(COST_USD_HISTOGRAM).record(cost_usd);
        Ok(())
    }

    async fn list_for_contract(&self, contract_id: &str) -> Result<Vec<CostAttributionEvent>> {
        let db = self.db.clone();
        let key = contract_id.to_string();
        tokio::task::spawn_blocking(move || {
            let read_txn = db.begin_read()?;
            let index = read_txn.open_table(CONTRACT_INDEX_TABLE)?;
            let ids: Vec<String> = index
                .get(key.as_str())?
                .map(|v| serde_json::from_str(v.value()).unwrap_or_default())
                .unwrap_or_default();
            drop(index);
            Self::load_by_ids(&read_txn, &ids)
        })
        .await?
    }

    async fn list_for_session(&self, session_id: &str) -> Result<Vec<CostAttributionEvent>> {
        let db = self.db.clone();
        let key = session_id.to_string();
        tokio::task::spawn_blocking(move || {
            let read_txn = db.begin_read()?;
            let index = read_txn.open_table(SESSION_INDEX_TABLE)?;
            let ids: Vec<String> = index
                .get(key.as_str())?
                .map(|v| serde_json::from_str(v.value()).unwrap_or_default())
                .unwrap_or_default();
            drop(index);
            Self::load_by_ids(&read_txn, &ids)
        })
        .await?
    }

    async fn aggregate_per_contract(&self) -> Result<Vec<ContractCostRollup>> {
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || {
            let read_txn = db.begin_read()?;
            let index = read_txn.open_table(CONTRACT_INDEX_TABLE)?;
            let attributions = read_txn.open_table(ATTRIBUTIONS_TABLE)?;

            let mut rollups: Vec<ContractCostRollup> = Vec::new();
            for entry in index.iter()? {
                let (key, value) = entry?;
                let contract_id = key.value().to_string();
                let ids: Vec<String> = serde_json::from_str(value.value()).unwrap_or_default();
                let mut rollup = ContractCostRollup {
                    contract_id,
                    dispatch_count: 0,
                    tokens_in: 0,
                    tokens_out: 0,
                    cost_usd: 0.0,
                };
                for id in ids {
                    if let Some(json) = attributions.get(id.as_str())? {
                        if let Ok(event) =
                            serde_json::from_str::<CostAttributionEvent>(json.value())
                        {
                            rollup.dispatch_count += 1;
                            rollup.tokens_in += u64::from(event.tokens_in);
                            rollup.tokens_out += u64::from(event.tokens_out);
                            rollup.cost_usd += event.cost_usd;
                        }
                    }
                }
                rollups.push(rollup);
            }
            rollups.sort_by(|a, b| {
                b.cost_usd
                    .partial_cmp(&a.cost_usd)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.contract_id.cmp(&b.contract_id))
            });
            Ok(rollups)
        })
        .await?
    }
}

/// Optional budget enforcement policy attached to a [`CostLedger`].
///
/// All fields are `Option` so operators can enable exactly the axes
/// they care about — per-dispatch only, per-contract only, or global
/// only. When multiple axes are populated the most restrictive wins.
///
/// # Example
///
/// ```
/// use octos_agent::cost_ledger::CostBudgetPolicy;
/// let policy = CostBudgetPolicy::default()
///     .with_per_dispatch_usd(0.50)
///     .with_per_contract_usd(5.00);
/// assert!(policy.is_enforced());
/// ```
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default, rename_all = "snake_case")]
pub struct CostBudgetPolicy {
    /// Hard ceiling per individual dispatch. Dispatch rejected before
    /// spawn if the projected cost exceeds this value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub per_dispatch_usd: Option<f64>,
    /// Hard ceiling accumulated across all dispatches bound to a
    /// single contract id. Projected cost + historical spend on the
    /// same contract must stay below this.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub per_contract_usd: Option<f64>,
    /// Hard ceiling accumulated across every contract the ledger has
    /// seen. Useful for tenant-wide spend caps.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub global_usd: Option<f64>,
}

/// Outcome of a budget projection check. Exhaustive so callers handle
/// every rejection reason explicitly.
#[derive(Debug, Clone, PartialEq)]
pub enum BudgetProjection {
    /// Dispatch may proceed. Carries the projected USD for logging.
    Allowed { projected_usd: f64 },
    /// Dispatch rejected — the carried error describes which axis
    /// tripped and by how much.
    Rejected {
        projected_usd: f64,
        reason: BudgetRejectionReason,
    },
}

/// Exhaustive set of reasons a projection may trip. Mirrors the axes of
/// [`CostBudgetPolicy`] so operators can log "which ceiling was hit"
/// without inspecting private state.
#[derive(Debug, Clone, PartialEq)]
pub enum BudgetRejectionReason {
    PerDispatchExceeded { limit_usd: f64, projected_usd: f64 },
    PerContractExceeded { limit_usd: f64, projected_usd: f64 },
    GlobalExceeded { limit_usd: f64, projected_usd: f64 },
}

impl std::fmt::Display for BudgetRejectionReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PerDispatchExceeded {
                limit_usd,
                projected_usd,
            } => write!(
                f,
                "per-dispatch budget ${limit_usd:.4} exceeded by projection ${projected_usd:.4}"
            ),
            Self::PerContractExceeded {
                limit_usd,
                projected_usd,
            } => write!(
                f,
                "per-contract budget ${limit_usd:.4} exceeded by projection ${projected_usd:.4}"
            ),
            Self::GlobalExceeded {
                limit_usd,
                projected_usd,
            } => write!(
                f,
                "global budget ${limit_usd:.4} exceeded by projection ${projected_usd:.4}"
            ),
        }
    }
}

impl CostBudgetPolicy {
    pub fn with_per_dispatch_usd(mut self, ceiling: f64) -> Self {
        self.per_dispatch_usd = Some(ceiling);
        self
    }

    pub fn with_per_contract_usd(mut self, ceiling: f64) -> Self {
        self.per_contract_usd = Some(ceiling);
        self
    }

    pub fn with_global_usd(mut self, ceiling: f64) -> Self {
        self.global_usd = Some(ceiling);
        self
    }

    /// True when at least one axis is populated.
    pub fn is_enforced(&self) -> bool {
        self.per_dispatch_usd.is_some()
            || self.per_contract_usd.is_some()
            || self.global_usd.is_some()
    }

    /// Pure in-memory check. Callers are expected to sum
    /// historical spend for the contract and globally before calling
    /// this so the implementation stays IO-free and unit-testable.
    pub fn project(
        &self,
        projected_usd: f64,
        contract_spend_usd: f64,
        global_spend_usd: f64,
    ) -> BudgetProjection {
        if let Some(limit) = self.per_dispatch_usd {
            if projected_usd > limit {
                return BudgetProjection::Rejected {
                    projected_usd,
                    reason: BudgetRejectionReason::PerDispatchExceeded {
                        limit_usd: limit,
                        projected_usd,
                    },
                };
            }
        }
        if let Some(limit) = self.per_contract_usd {
            let combined = contract_spend_usd + projected_usd;
            if combined > limit {
                return BudgetProjection::Rejected {
                    projected_usd,
                    reason: BudgetRejectionReason::PerContractExceeded {
                        limit_usd: limit,
                        projected_usd: combined,
                    },
                };
            }
        }
        if let Some(limit) = self.global_usd {
            let combined = global_spend_usd + projected_usd;
            if combined > limit {
                return BudgetProjection::Rejected {
                    projected_usd,
                    reason: BudgetRejectionReason::GlobalExceeded {
                        limit_usd: limit,
                        projected_usd: combined,
                    },
                };
            }
        }
        BudgetProjection::Allowed { projected_usd }
    }
}

/// Convenience: pair a ledger with an optional policy so call-sites
/// can hold a single `Arc<CostAccountant>` without juggling two
/// dependencies. [`SpawnTool::with_cost_accountant`](crate::tools::spawn::SpawnTool::with_cost_accountant)
/// stores one of these.
pub struct CostAccountant {
    ledger: Arc<dyn CostLedger>,
    policy: Option<CostBudgetPolicy>,
}

impl CostAccountant {
    pub fn new(ledger: Arc<dyn CostLedger>, policy: Option<CostBudgetPolicy>) -> Self {
        Self { ledger, policy }
    }

    pub fn ledger(&self) -> &Arc<dyn CostLedger> {
        &self.ledger
    }

    pub fn policy(&self) -> Option<&CostBudgetPolicy> {
        self.policy.as_ref()
    }

    /// Look up historical spend for a contract and evaluate the
    /// policy. Returns [`BudgetProjection::Allowed`] when no policy is
    /// attached — callers can treat an absent policy as "no cap" and
    /// skip the check altogether.
    pub async fn project_dispatch(
        &self,
        contract_id: &str,
        projected_usd: f64,
    ) -> Result<BudgetProjection> {
        let Some(policy) = self.policy.as_ref() else {
            return Ok(BudgetProjection::Allowed { projected_usd });
        };
        let contract_spend = if policy.per_contract_usd.is_some() {
            sum_cost(&self.ledger.list_for_contract(contract_id).await?)
        } else {
            0.0
        };
        let global_spend = if policy.global_usd.is_some() {
            sum_cost_rollups(&self.ledger.aggregate_per_contract().await?)
        } else {
            0.0
        };
        Ok(policy.project(projected_usd, contract_spend, global_spend))
    }
}

fn sum_cost(events: &[CostAttributionEvent]) -> f64 {
    events.iter().map(|event| event.cost_usd).sum()
}

fn sum_cost_rollups(rollups: &[ContractCostRollup]) -> f64 {
    rollups.iter().map(|r| r.cost_usd).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_cost_uses_model_pricing_catalog() {
        // claude-haiku pricing is registered in octos-llm::pricing.
        let cost = project_cost_usd("claude-haiku", 1_000_000, 1_000_000).unwrap();
        assert!(cost > 0.0);
    }

    #[test]
    fn project_cost_returns_none_for_unknown_model() {
        assert!(project_cost_usd("completely-unknown-model-xyz", 100, 100).is_none());
    }

    #[test]
    fn budget_policy_rejects_per_dispatch_over_cap() {
        let policy = CostBudgetPolicy::default().with_per_dispatch_usd(0.10);
        let projection = policy.project(0.50, 0.0, 0.0);
        match projection {
            BudgetProjection::Rejected { reason, .. } => {
                matches!(reason, BudgetRejectionReason::PerDispatchExceeded { .. });
            }
            other => panic!("expected rejection, got {other:?}"),
        }
    }

    #[test]
    fn budget_policy_allows_when_no_axis_configured() {
        let policy = CostBudgetPolicy::default();
        match policy.project(10_000.0, 0.0, 0.0) {
            BudgetProjection::Allowed { projected_usd } => {
                assert!((projected_usd - 10_000.0).abs() < f64::EPSILON);
            }
            other => panic!("expected allowed, got {other:?}"),
        }
    }

    #[test]
    fn budget_policy_rejects_when_per_contract_exceeded() {
        let policy = CostBudgetPolicy::default().with_per_contract_usd(1.00);
        let projection = policy.project(0.50, 0.80, 0.0);
        match projection {
            BudgetProjection::Rejected { reason, .. } => match reason {
                BudgetRejectionReason::PerContractExceeded { limit_usd, .. } => {
                    assert!((limit_usd - 1.00).abs() < f64::EPSILON);
                }
                other => panic!("wrong reason: {other:?}"),
            },
            other => panic!("expected rejection, got {other:?}"),
        }
    }

    #[test]
    fn cost_attribution_new_populates_timestamp_and_id() {
        let event = CostAttributionEvent::new(
            "session-1",
            "contract-A",
            "task-xyz",
            "claude-haiku",
            100,
            50,
            0.0001,
        );
        assert!(event.attribution_id.starts_with("cost-"));
        assert!(event.timestamp.contains('T'));
        assert_eq!(event.schema_version, COST_ATTRIBUTION_SCHEMA_VERSION);
    }
}
