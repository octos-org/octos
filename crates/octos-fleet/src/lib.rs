//! # octos-fleet — the fleet kernel store
//!
//! The durable, transactional core of the fleet kernel (spec
//! `docs/FLEET-KERNEL-V1-SPEC.md`, PR 1): one redb database plus the
//! attempt / lease / generation state machine, the durable plan, a
//! budget settled **inside** each transition's write-transaction, a
//! real claim/ack outbox, and boot recovery reconciliation.
//!
//! This crate is deliberately **self-contained**: its only dependencies
//! are `redb`, `serde`/`serde_json`, `tokio`, `uuid`, `eyre`, and
//! `octos-core` (for [`octos_core::SessionKey`]). It has **zero** LLM /
//! `octos-agent` dependency and is **not** wired into any live path —
//! the closed task-worker, the outbox consumer, and the keeper land in
//! later PRs. Everything here is unit-testable against a tempdir redb.
//!
//! ## The ergonomic layer ([`Fleet`])
//!
//! [`FleetKernelStore`] is the transactional primitive: individual
//! revision-/generation-/lease-fenced CAS ops. [`Fleet`] is the
//! plan-management API **on top** of it — it composes those CAS ops into
//! whole-plan operations ([`Fleet::create`], [`Fleet::view`],
//! [`Fleet::ready_tasks`], [`Fleet::apply_edit`], [`Fleet::record_outcome`],
//! [`Fleet::is_complete`], [`Fleet::summary`]) without changing the store's
//! semantics. This is the surface the future keeper + `goal_get`/`goal_update`
//! tools program against. It adds **no** dependency: still no LLM, no
//! `octos-agent`.
//!
//! ## Invariants
//!
//! - **One transactional store.** Every state transition
//!   ([`FleetKernelStore::launch_child`], [`FleetKernelStore::complete_child`],
//!   …) is a single `begin_write` that reads, checks a CAS predicate,
//!   and writes the next state + budget settlement + outbox append
//!   together — no cross-store window.
//! - **Reads gate too.** All access is serialised through an `io_gate`
//!   whose owned guard moves into `spawn_blocking`, so a cancelled
//!   caller's non-abortable blocking write can never land unordered
//!   against a later read (spec §1 v1.1).
//! - **Schema-versioned.** Every persisted row carries
//!   [`SCHEMA_VERSION`]; a higher-version row loads as `Ok(None)`.

#![deny(unsafe_code)]

mod fleet;
mod records;
mod store;

pub use fleet::{Fleet, FleetSummary, FleetView, PlanEdit, PlanGraphError, TaskSpec, TaskView};
pub use records::{
    AcceptanceCriterion, AcceptanceVerdict, Attempt, AttemptStatus, ChildResultSnapshot,
    ChildStatus, DecisionEntry, DecisionKind, DurablePlan, EvidenceRef, FleetBudget,
    FleetChildRecord, FleetEventKind, FleetRecord, FleetStatus, Lease, OutboxEvent, PlanTask,
    SCHEMA_VERSION, Verifier, WorkerKind,
};
pub use store::{
    AckOutcome, CompleteOutcome, FleetKernelStore, FleetSnapshot, InterruptedAttempt,
    LaunchOutcome, PlanMutateOutcome, ReconcileReport,
};
