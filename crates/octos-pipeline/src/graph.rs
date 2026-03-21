//! Core graph types for pipeline representation.

use std::collections::HashMap;

use octos_core::TokenUsage;
use serde::{Deserialize, Serialize};

/// A parsed, typed pipeline graph ready for execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineGraph {
    /// Graph identifier (from `digraph name { ... }`).
    pub id: String,
    /// Human-readable description (from `label` attribute).
    pub label: Option<String>,
    /// Default model key for nodes that don't specify one.
    pub default_model: Option<String>,
    /// Nodes keyed by their ID.
    pub nodes: HashMap<String, PipelineNode>,
    /// Directed edges.
    pub edges: Vec<PipelineEdge>,
    /// Named subgraphs (clusters).
    #[serde(default)]
    pub subgraphs: Vec<Subgraph>,
}

/// A single node in the pipeline graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineNode {
    /// Node identifier.
    pub id: String,
    /// Handler type.
    pub handler: HandlerKind,
    /// System prompt template. Supports `{input}` and `{variable_name}` substitution.
    pub prompt: Option<String>,
    /// Human-readable label for progress reporting.
    pub label: Option<String>,
    /// Model key for `ProviderRouter::resolve()` (e.g. "cheap", "strong").
    pub model: Option<String>,
    /// Override context window size in tokens.
    pub context_window: Option<u32>,
    /// Override max output tokens per LLM call. Default 4096 is too low for
    /// nodes that write long outputs (e.g. synthesize writing full reports).
    pub max_output_tokens: Option<u32>,
    /// Allowed tool names for this node. Empty = all builtins.
    pub tools: Vec<String>,
    /// If true, a successful outcome means "pipeline goal achieved".
    pub goal_gate: bool,
    /// Retry on error (default 0).
    pub max_retries: u32,
    /// Per-node timeout in seconds.
    pub timeout_secs: Option<u64>,
    /// Hint for edge selection when no condition matches.
    pub suggested_next: Option<String>,
    /// For `Parallel` / `DynamicParallel` nodes: the node to jump to after completion.
    /// All target outputs are merged and fed as input to this convergence node.
    pub converge: Option<String>,
    /// For `DynamicParallel`: prompt template for each worker task.
    /// Supports `{task}` placeholder replaced with each planned task description.
    pub worker_prompt: Option<String>,
    /// For `DynamicParallel`: model key for the planning LLM call (optional).
    pub planner_model: Option<String>,
    /// For `DynamicParallel`: maximum number of dynamic tasks (default 8).
    pub max_tasks: Option<u32>,
}

impl Default for PipelineNode {
    fn default() -> Self {
        Self {
            id: String::new(),
            handler: HandlerKind::Codergen,
            prompt: None,
            label: None,
            model: None,
            context_window: None,
            max_output_tokens: None,
            tools: Vec::new(),
            goal_gate: false,
            max_retries: 0,
            timeout_secs: None,
            suggested_next: None,
            converge: None,
            worker_prompt: None,
            planner_model: None,
            max_tasks: None,
        }
    }
}

/// A directed edge between two nodes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineEdge {
    /// Source node ID.
    pub source: String,
    /// Target node ID.
    pub target: String,
    /// Human-readable label.
    pub label: Option<String>,
    /// Condition expression (e.g. `outcome.status == "pass"`).
    pub condition: Option<String>,
    /// Edge weight for priority (default 1.0, must be positive).
    pub weight: f64,
}

/// Handler type for pipeline nodes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandlerKind {
    /// Run a full agent loop with tools.
    Codergen,
    /// Execute a shell command.
    Shell,
    /// Evaluate a condition (no LLM call).
    Gate,
    /// Pass-through.
    Noop,
    /// Fan-out: run all outgoing targets concurrently, merge results,
    /// then jump to the `converge` node.
    Parallel,
    /// Dynamic fan-out: LLM plans N sub-tasks at runtime, executes them
    /// in parallel, merges results, then jumps to the `converge` node.
    DynamicParallel,
}

impl HandlerKind {
    /// Parse from a string attribute value.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "codergen" => Some(Self::Codergen),
            "shell" => Some(Self::Shell),
            "gate" => Some(Self::Gate),
            "noop" => Some(Self::Noop),
            "parallel" => Some(Self::Parallel),
            "dynamic_parallel" => Some(Self::DynamicParallel),
            _ => None,
        }
    }

    /// Resolve handler from DOT `shape` attribute (Attractor spec mapping).
    pub fn from_shape(shape: &str) -> Option<Self> {
        match shape {
            "Mdiamond" => Some(Self::Noop),       // start node
            "Msquare" => Some(Self::Noop),        // exit node
            "box" => Some(Self::Codergen),        // LLM task (default)
            "hexagon" => Some(Self::Gate),        // human gate / conditional
            "diamond" => Some(Self::Gate),        // conditional routing
            "component" => Some(Self::Parallel),  // parallel fan-out
            "parallelogram" => Some(Self::Shell), // external tool/command
            _ => None,
        }
    }
}

/// The outcome of executing a single pipeline node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeOutcome {
    /// The node ID that produced this outcome.
    pub node_id: String,
    /// Whether the node succeeded.
    pub status: OutcomeStatus,
    /// Text content produced by the node.
    pub content: String,
    /// Token usage for this node.
    pub token_usage: TokenUsage,
}

/// Outcome status for a pipeline node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeStatus {
    Pass,
    Fail,
    Error,
}

/// A named subgraph (cluster) within a pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subgraph {
    /// Subgraph identifier (e.g. "cluster_research").
    pub id: String,
    /// Human-readable label.
    pub label: Option<String>,
    /// Node IDs belonging to this subgraph.
    pub node_ids: Vec<String>,
}

/// Validate that a pipeline identifier (node ID, run ID, graph ID) is safe
/// for use as a filesystem path component. Rejects path separators, `..`,
/// control characters, and excessively long values.
pub fn validate_pipeline_id(id: &str) -> eyre::Result<()> {
    if id.is_empty() {
        eyre::bail!("pipeline identifier must not be empty");
    }
    if id.len() > 128 {
        eyre::bail!(
            "pipeline identifier too long (max 128 chars): {}",
            id.chars().take(32).collect::<String>()
        );
    }
    if id.contains('/') || id.contains('\\') || id.contains('\0') || id.contains("..") {
        eyre::bail!("pipeline identifier contains unsafe characters: {id}");
    }
    if id.chars().any(|c| c.is_control()) {
        eyre::bail!("pipeline identifier contains control characters: {id}");
    }
    Ok(())
}

/// Summary of a single node execution (for reporting).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeSummary {
    pub node_id: String,
    pub label: String,
    pub model: Option<String>,
    pub token_usage: TokenUsage,
    pub duration_ms: u64,
    pub success: bool,
}
