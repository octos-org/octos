//! DOT-based pipeline orchestration engine for octos.
//!
//! Parse DOT graphs with typed attributes, walk the graph with async handlers,
//! and execute multi-step agent workflows with per-node model selection.

pub mod artifact;
pub mod checkpoint;
pub mod condition;
pub mod discovery;
pub mod events;
pub mod executor;
pub mod fidelity;
pub mod graph;
pub mod handler;
pub mod human_gate;
pub mod manager;
pub mod parser;
pub mod run_dir;
pub mod server;
pub mod stylesheet;
pub mod thread;
pub mod tool;
pub mod validate;

pub use artifact::ArtifactStore;
pub use checkpoint::{Checkpoint, CheckpointStore};
pub use events::{
    CollectingEventHandler, PipelineEvent, PipelineEventHandler, TracingEventHandler,
};
pub use executor::{PipelineExecutor, PipelineResult, PipelineStatusBridge};
pub use fidelity::FidelityMode;
pub use graph::{
    HandlerKind, NodeOutcome, OutcomeStatus, PipelineEdge, PipelineGraph, PipelineNode, Subgraph,
    validate_pipeline_id,
};
pub use handler::{
    CodergenHandler, GateHandler, Handler, HandlerRegistry, NoopHandler, ShellHandler,
};
pub use human_gate::{HumanInputProvider, HumanInputType, HumanRequest, HumanResponse};
pub use manager::{
    ChildExecutor, ChildResult, ChildSpec, ManagerOutcome, PipelineManager, SupervisionStrategy,
};
pub use parser::parse_dot;
pub use run_dir::{NodeStatus, PipelineRunSummary, RunDir};
pub use server::{
    CancelRequest, PipelineServer, RunStatus, RunStatusResponse, SubmitRequest, SubmitResponse,
};
pub use stylesheet::{ModelStylesheet, ResolvedStyle, StyleRule};
pub use thread::{Thread, ThreadRegistry};
pub use tool::RunPipelineTool;
pub use validate::{LintDiagnostic, Severity, validate};
