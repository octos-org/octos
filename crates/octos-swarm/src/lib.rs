//! Swarm orchestration primitive for octos (harness M7.5).
//!
//! `octos-swarm` formalises the PM + swarm supervisor pattern the
//! octos harness has been running manually: a supervisor writes a
//! contract, fans it out to N sub-agents, aggregates their artifacts,
//! gates the aggregate through an M4.3 validator, rolls up cost via
//! the M7.4 ledger (stubbed here until that work lands), and emits a
//! typed [`HarnessEventPayload::SwarmDispatch`] event the operator
//! dashboard and Matrix channel can render live.
//!
//! # Usage sketch
//!
//! ```ignore
//! use std::num::NonZeroUsize;
//! use std::sync::Arc;
//! use octos_agent::tools::mcp_agent::{build_backend_from_config, McpAgentBackendConfig};
//! use octos_swarm::{
//!     ContractSpec, Swarm, SwarmBudget, SwarmContext, SwarmTopology,
//! };
//!
//! # async fn demo() -> eyre::Result<()> {
//! let cfg = McpAgentBackendConfig::Local {
//!     cmd: "claude".into(),
//!     args: vec!["mcp".into(), "serve".into()],
//!     env: Default::default(),
//!     dispatch_timeout_secs: Some(60),
//! };
//! let backend = build_backend_from_config(&cfg, None)?;
//!
//! let swarm = Swarm::builder(backend, "/tmp/swarm-state").build().await?;
//! let result = swarm
//!     .dispatch(
//!         "dispatch-1",
//!         vec![ContractSpec {
//!             contract_id: "c1".into(),
//!             tool_name: "claude_code/run_task".into(),
//!             task: serde_json::json!({"task": "write a haiku"}),
//!             label: Some("haiku".into()),
//!         }],
//!         SwarmTopology::Parallel {
//!             max_concurrency: NonZeroUsize::new(1).unwrap(),
//!         },
//!         SwarmBudget::default(),
//!         SwarmContext {
//!             session_id: "api:example".into(),
//!             task_id: "task-1".into(),
//!             workflow: Some("poetry".into()),
//!             phase: Some("draft".into()),
//!         },
//!     )
//!     .await?;
//! println!("completed: {}/{}", result.completed_subtasks, result.total_subtasks);
//! # Ok(()) }
//! ```
//!
//! # Invariants
//!
//! 1. [`Swarm::dispatch`] is idempotent given the same
//!    `(dispatch_id, contracts, topology, budget)`: a finalized record
//!    in the redb ledger short-circuits re-dispatch.
//! 2. [`SwarmTopology::Parallel`] fans out up to `max_concurrency`
//!    contracts, aggregation is arrival order.
//! 3. [`SwarmTopology::Sequential`] runs one-at-a-time and aborts on
//!    the first terminal (non-retryable) failure.
//! 4. [`SwarmTopology::Pipeline`] chains output of contract `i` as
//!    `pipeline_input` into contract `i + 1`.
//! 5. Retry budget bounded at [`MAX_RETRY_ROUNDS`] (3).
//! 6. Aggregate M4.3 validator runs AFTER every sub-contract reaches
//!    terminal state.
//! 7. Session-durable: redb-backed, supervisor can reload state and
//!    resume after process restart.
//! 8. Events emitted as
//!    [`HarnessEventPayload::SwarmDispatch`](octos_agent::harness_events::HarnessEventPayload::SwarmDispatch)
//!    with [`SWARM_DISPATCH_SCHEMA_VERSION`](octos_agent::abi_schema::SWARM_DISPATCH_SCHEMA_VERSION)
//!    pinned at 1.
//! 9. Zero new `unsafe` — the workspace-wide `deny(unsafe_code)` lint
//!    is honoured.

#![doc(html_root_url = "https://docs.rs/octos-swarm/0.1.0")]

mod dispatcher;
mod ledger;
mod persistence;
mod result;
mod topology;

pub use dispatcher::{
    AggregateValidator, MAX_RETRY_ROUNDS, NoopSwarmEventSink, Swarm, SwarmBudget, SwarmBuilder,
    SwarmContext, SwarmEventSink, flatten_aggregate,
};
pub use ledger::{CostLedger, NoopCostLedger, SwarmCostAttribution};
pub use persistence::{DISPATCH_RECORD_SCHEMA_VERSION, DispatchRecord, DispatchStore};
pub use result::{AggregateArtifact, SubtaskOutcome, SubtaskStatus, SwarmOutcomeKind, SwarmResult};
pub use topology::{ContractSpec, FanoutPattern, MAX_CONTRACTS_PER_DISPATCH, SwarmTopology};
