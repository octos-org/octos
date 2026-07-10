//! Pipeline execution engine — walks the graph, executes handlers, selects edges.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use eyre::{Result, WrapErr};
use octos_agent::TokenTracker;
use octos_agent::hooks::{HookContext, HookEvent, HookExecutor, HookPayload};
use octos_agent::progress::ProgressEvent;
use octos_agent::tools::TOOL_CTX;
use octos_core::{Message, MessageRole, TokenUsage};
use octos_llm::{ChatConfig, LlmProvider, ProviderRouter, SemaphoreThrottledProvider};
use octos_memory::EpisodeStore;
use serde::Deserialize;
use tracing::{info, warn};

use octos_agent::cost_ledger::{CostAttributionEvent, ReservationHandle};
use octos_agent::validators::ValidatorPhase;
use octos_agent::workspace_contract::run_declared_validators;
use octos_agent::workspace_policy::Validator as WorkspaceValidator;

use crate::checkpoint::{CheckpointStore, PersistedCheckpoint};
use crate::condition;
use crate::context::PipelineContext;
use crate::graph::{
    DeadlineAction, HandlerKind, NodeOutcome, NodeSummary, OutcomeStatus, PipelineEdge,
    PipelineGraph, PipelineNode,
};
use crate::guard::{GuardContext, GuardDecision, PipelineGuard};
use crate::handler::{CodergenHandler, GateHandler, HandlerContext, HandlerRegistry, NoopHandler};
use crate::parser::parse_dot;
use crate::validate;

/// Minimum projected USD per LLM-call node when no model-specific rate
/// is available. Keeps the reservation path live for unknown models so
/// budget-policy breaches surface on every dispatch rather than slipping
/// through a silent `0.0` projection.
const MIN_PER_NODE_PROJECTED_USD: f64 = 0.001;

/// Default pipeline-level projection when the caller leaves
/// [`PipelineContext::pipeline_projected_usd`] unset. One cent keeps
/// the reservation path alive without pre-committing a noticeable
/// budget.
const DEFAULT_PIPELINE_PROJECTED_USD: f64 = 0.01;

/// Default pipeline contract id when [`PipelineContext::contract_id`]
/// is empty. Chosen to match the operator rollup key used elsewhere in
/// the harness for background pipelines.
const DEFAULT_PIPELINE_CONTRACT_ID: &str = "pipeline";

/// Default maximum number of concurrent LLM calls inside a single pipeline run.
pub const DEFAULT_PIPELINE_MAX_CONCURRENT_LLM_CALLS: usize = 4;

/// Cumulative cap on the total number of fan-out workers a single pipeline
/// run may spawn across its lifetime. Each worker counted once at dispatch
/// time, regardless of which branch (`Parallel` or `DynamicParallel`)
/// dispatched it. Beyond this cap the executor fails the pipeline with
/// [`PipelineError::FanoutExceeded`] before the cap-exceeding fan-out
/// dispatches a single worker — partial dispatch leaves the pipeline in a
/// less-recoverable state than an early refusal.
///
/// Motivated by the river/mini4 65,535-child runaway: the per-batch
/// concurrency cap on `dynamic_parallel` nodes only bounds in-flight
/// workers, not lifetime fan-out. A pathological planner that re-fires the
/// same dynamic-parallel node many times can still exhaust the host even
/// with `max_parallel_workers = 8`.
pub const MAX_PIPELINE_FANOUT_TOTAL: usize = 500;

/// Absolute wall-clock ceiling for a single fan-out worker future, used when
/// the worker node declares neither `deadline_secs` nor `timeout_secs`.
///
/// Fan-out workers are `join_all`-awaited, so a single worker future that
/// never resolves wedges the WHOLE fan-out (the node never converges and the
/// pipeline never terminates). The single-node path is already bounded by
/// `dispatch_node`'s `tokio::time::timeout`; this constant gives the fan-out
/// path the same fail-closed guarantee even when no per-node deadline is set.
///
/// Motivated by the deployed `deep_research` wedge: a `search` fan-out child
/// whose `web_search` failed left its worker Agent stuck, re-emitting
/// `ExecutingTool` every 5s while the task itself was already terminal
/// `Failed`. `join_all` blocked forever, the heartbeat reported
/// `search (0/3 nodes …)` for 25+ minutes, and every attached client hung.
///
/// One hour is generous enough that it never preempts a legitimately
/// long-running worker, while still guaranteeing termination.
pub const MAX_FANOUT_WORKER_SECS: u64 = 3600;

/// Effective wall-clock deadline for a single fan-out worker, in priority
/// order: the worker node's `deadline_secs`, then its `timeout_secs`, then the
/// absolute [`MAX_FANOUT_WORKER_SECS`] ceiling. Never `None`: a fan-out worker
/// must ALWAYS be bounded so a hung child cannot wedge `join_all` forever.
fn fanout_worker_deadline(node: &PipelineNode) -> Duration {
    if let Some(secs) = node.deadline_secs {
        // `deadline_secs` is f64; clamp away non-finite / non-positive values
        // so a malformed graph can't produce a zero/NaN deadline that fires
        // instantly or panics in `Duration::from_secs_f64`.
        if secs.is_finite() && secs > 0.0 {
            return Duration::from_secs_f64(secs.min(MAX_FANOUT_WORKER_SECS as f64));
        }
    }
    if let Some(secs) = node.timeout_secs {
        if secs > 0 {
            return Duration::from_secs(secs.min(MAX_FANOUT_WORKER_SECS));
        }
    }
    Duration::from_secs(MAX_FANOUT_WORKER_SECS)
}

/// Structured pipeline-level error variants. Today only the cumulative
/// fan-out cap surfaces this type; the rest of the executor still uses
/// `eyre`-based errors. The enum is `Clone` so the cap-exceeded reason
/// can be embedded into the resulting [`PipelineResult::output`] without
/// re-allocating context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PipelineError {
    /// The cumulative fan-out cap fired. `count` is the number of workers
    /// already dispatched in this pipeline run (i.e. the value of the
    /// counter immediately before the refusal). `cap` is the configured
    /// limit ([`MAX_PIPELINE_FANOUT_TOTAL`]).
    FanoutExceeded { count: usize, cap: usize },
}

impl std::fmt::Display for PipelineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FanoutExceeded { count, cap } => write!(
                f,
                "pipeline fan-out cap exceeded ({count} of {cap}); refusing further workers"
            ),
        }
    }
}

impl std::error::Error for PipelineError {}

/// Returns `true` when the handler kind triggers one or more LLM calls
/// inside the node and therefore participates in cost reservation.
///
/// * `Codergen`: one sub-agent run (many LLM calls inside the loop).
/// * `DynamicParallel`: one planner call + N worker calls; the
///   reservation is sized using the node's declared model so it covers
///   both phases.
///
/// `Shell`, `Gate`, `Noop`, and `Parallel` do not issue LLM calls
/// directly — `Parallel` fan-outs target `Codergen` nodes which each
/// reserve independently when traversal reaches them.
fn handler_kind_reserves(kind: &HandlerKind) -> bool {
    matches!(kind, HandlerKind::Codergen | HandlerKind::DynamicParallel)
}

/// Project a per-node USD cost for reservation purposes.
///
/// Uses the declared model's token pricing with a fixed 2k-in / 2k-out
/// estimate when the model is known. Falls back to
/// [`MIN_PER_NODE_PROJECTED_USD`] for unknown models so the reservation
/// path still fires (and budget breaches still surface).
fn project_node_usd(model: Option<&str>) -> f64 {
    let Some(model) = model else {
        return MIN_PER_NODE_PROJECTED_USD;
    };
    match octos_agent::cost_ledger::project_cost_usd(model, 2_000, 2_000) {
        Some(cost) if cost > 0.0 => cost,
        _ => MIN_PER_NODE_PROJECTED_USD,
    }
}

/// Total count of pipeline deadline expirations, partitioned by action label.
/// Layout: `[abort, skip, retry, escalate]`. Use [`deadline_exceeded_count`] to
/// read a specific action's counter by name.
pub static PIPELINE_DEADLINE_EXCEEDED_TOTAL: [AtomicU64; 4] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

/// Total count of mission checkpoints persisted to a `CheckpointStore`.
pub static PIPELINE_CHECKPOINT_PERSISTED_TOTAL: AtomicU64 = AtomicU64::new(0);

/// Total count of pipeline runs that were resumed from a checkpoint (i.e., a
/// store returned at least one `PersistedCheckpoint` at the start of `run`).
pub static PIPELINE_CHECKPOINT_RESUMED_TOTAL: AtomicU64 = AtomicU64::new(0);

fn deadline_action_index(name: &str) -> usize {
    match name {
        "abort" => 0,
        "skip" => 1,
        "retry" => 2,
        "escalate" => 3,
        _ => 0,
    }
}

/// Read the current `octos_pipeline_deadline_exceeded_total{action=<name>}`
/// counter. Unknown names fall through to the `abort` bucket.
pub fn deadline_exceeded_count(action_name: &str) -> u64 {
    PIPELINE_DEADLINE_EXCEEDED_TOTAL[deadline_action_index(action_name)].load(Ordering::Relaxed)
}

fn record_deadline_exceeded(action: &DeadlineAction) {
    PIPELINE_DEADLINE_EXCEEDED_TOTAL[deadline_action_index(action.name())]
        .fetch_add(1, Ordering::Relaxed);
}

/// Internal result of dispatching a single node. `Completed` carries the
/// produced outcome; `Skipped` signals the deadline fired with
/// `DeadlineAction::Skip` and the outer loop should synthesize a skipped
/// outcome.
enum DispatchOutcome {
    Completed(NodeOutcome),
    Skipped { label: String },
}

fn handler_kind_label(kind: &HandlerKind) -> &'static str {
    match kind {
        HandlerKind::Codergen => "codergen",
        HandlerKind::Shell => "shell",
        HandlerKind::Gate => "gate",
        HandlerKind::Noop => "noop",
        HandlerKind::Parallel => "parallel",
        HandlerKind::DynamicParallel => "dynamic_parallel",
    }
}

/// Skip-set derived from the checkpoint store for a fresh run.
///
/// Returns the set of node IDs that should be skipped because they (or nodes
/// preceding them in completion order) were already recorded. On resume:
/// * if the store yields at least one persisted snapshot, every `node_id`
///   recorded in those snapshots goes into the skip set.
/// * if the topological walk ever reaches one of those nodes, it is treated
///   as completed and its outcome is synthesized as a `Pass` with empty
///   content (downstream nodes still receive that empty input).
fn build_resume_skip_set(store: Option<&Arc<dyn CheckpointStore>>) -> Result<HashSet<String>> {
    let Some(store) = store else {
        return Ok(HashSet::new());
    };
    let list = match store.list() {
        Ok(l) => l,
        Err(e) => {
            warn!(error = %e, "checkpoint store list failed; starting fresh");
            return Ok(HashSet::new());
        }
    };
    if list.is_empty() {
        return Ok(HashSet::new());
    }
    PIPELINE_CHECKPOINT_RESUMED_TOTAL.fetch_add(1, Ordering::Relaxed);
    let skip: HashSet<String> = list.into_iter().map(|c| c.node_id).collect();
    info!(
        skip_count = skip.len(),
        "resuming pipeline from checkpoint store"
    );
    Ok(skip)
}

/// Per-node cost attribution captured during pipeline execution
/// (W1.A4). Recorded for every LLM-call node that opens a
/// [`ReservationHandle`] against the configured `CostAccountant`. The
/// reservation projection is captured at dispatch start; the actual
/// USD spend is computed from the post-dispatch token usage so the UI
/// can render both "reserved" and "actual" sides.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct NodeCost {
    /// Pipeline node id.
    pub node_id: String,
    /// Resolved model key for the node, or `None` when the node ran
    /// with the default provider.
    pub model: Option<String>,
    /// Pre-dispatch USD projection used for the reservation (0.0 when
    /// no accountant was configured).
    pub reserved_usd: f64,
    /// Post-dispatch USD computed from actual token usage. Falls back
    /// to the reserved projection when the model rate is unknown.
    pub actual_usd: f64,
    /// Input tokens consumed by the node.
    pub tokens_in: u32,
    /// Output tokens produced by the node.
    pub tokens_out: u32,
    /// `true` when the per-node `ReservationHandle` was committed to
    /// the ledger. `false` when no accountant was attached or when the
    /// commit was dropped (auto-refunded). Surfaces the "ledger-bound
    /// vs ephemeral" distinction the UI needs to badge cost rows.
    pub committed: bool,
}

/// Result of a complete pipeline execution.
#[derive(Debug, Clone)]
pub struct PipelineResult {
    /// Final output text.
    pub output: String,
    /// Whether the pipeline completed successfully.
    pub success: bool,
    /// Total token usage across all nodes.
    pub token_usage: TokenUsage,
    /// Per-node execution summaries.
    pub node_summaries: Vec<NodeSummary>,
    /// Files written by pipeline nodes (collected from all node outcomes).
    pub files_modified: Vec<std::path::PathBuf>,
    /// M8 parity (W1.A4): per-node cost attribution. One entry per
    /// node that opened a [`ReservationHandle`] against the configured
    /// `CostAccountant`. Empty when no accountant is wired.
    pub node_costs: Vec<NodeCost>,
}

/// Bridge for pipeline status updates to external systems (e.g., messaging channels).
///
/// The pipeline executor updates status words and token counts through this bridge.
/// External consumers (e.g., `StatusIndicator`) read and display them.
#[derive(Clone)]
pub struct PipelineStatusBridge {
    /// Shared status words — pipeline updates these to show node-level progress.
    pub status_words: Arc<std::sync::RwLock<Vec<String>>>,
    /// Shared token tracker — pipeline feeds sub-agent token counts here.
    pub token_tracker: Arc<TokenTracker>,
}

impl PipelineStatusBridge {
    pub fn new(
        status_words: Arc<std::sync::RwLock<Vec<String>>>,
        token_tracker: Arc<TokenTracker>,
    ) -> Self {
        Self {
            status_words,
            token_tracker,
        }
    }

    /// Update the status words pool shown to the user.
    fn set_words(&self, words: Vec<String>) {
        if let Ok(mut w) = self.status_words.write() {
            *w = words;
        }
    }

    /// Add token usage from a sub-agent to the shared tracker.
    fn add_tokens(&self, usage: &TokenUsage) {
        use std::sync::atomic::Ordering;
        self.token_tracker
            .input_tokens
            .fetch_add(usage.input_tokens, Ordering::Relaxed);
        self.token_tracker
            .output_tokens
            .fetch_add(usage.output_tokens, Ordering::Relaxed);
    }
}

/// Configuration for the pipeline executor.
pub struct ExecutorConfig {
    pub default_provider: Arc<dyn LlmProvider>,
    pub provider_router: Option<Arc<ProviderRouter>>,
    pub memory: Arc<EpisodeStore>,
    pub working_dir: PathBuf,
    pub provider_policy: Option<octos_agent::ToolPolicy>,
    pub plugin_dirs: Vec<PathBuf>,
    /// Section B (codex review P1.1): pipeline-level strict-signing policy.
    /// When `true`, the per-node `CodergenHandler` rejects unsigned plugins
    /// at cache build time. Defaults to `false` (legacy permissive path).
    pub plugin_require_signed: bool,
    /// Optional status bridge for live progress updates to messaging channels.
    pub status_bridge: Option<PipelineStatusBridge>,
    /// Shared shutdown signal — set to true to cancel all pipeline workers.
    /// Propagated to each worker agent's shutdown flag.
    pub shutdown: Arc<std::sync::atomic::AtomicBool>,
    /// Maximum number of parallel workers for fan-out stages (default 8).
    /// Prevents unbounded resource consumption under high parallelism.
    pub max_parallel_workers: usize,
    /// Cumulative fan-out worker cap for the entire pipeline run (Guard B).
    /// `None` defaults to [`MAX_PIPELINE_FANOUT_TOTAL`]. Tests set this to
    /// a small value to drive the cap path without waiting on real
    /// LLM-driven planning.
    pub max_pipeline_fanout_total: Option<usize>,
    /// In-process hooks evaluated in registration order before each
    /// dispatchable node. A `Skip` decision records a synthetic `Fail`
    /// outcome for edge routing; an `Abort` decision returns the partial
    /// pipeline result collected so far.
    pub guards: Vec<Arc<dyn PipelineGuard>>,
    /// Maximum number of concurrent LLM calls inside this pipeline run.
    /// `None` defaults to [`DEFAULT_PIPELINE_MAX_CONCURRENT_LLM_CALLS`].
    /// This pipeline-scoped semaphore prevents parallel worker retry storms
    /// without changing global provider/router behavior.
    pub max_concurrent_llm_calls: Option<usize>,
    /// Optional mission checkpoint store. When set, the executor:
    /// * loads the latest `PersistedCheckpoint` at the start of a run and
    ///   skips every node with id `<=` the recorded node in the pipeline's
    ///   declaration order;
    /// * persists one `PersistedCheckpoint` per `MissionCheckpoint`
    ///   declaration after a node completes successfully.
    pub checkpoint_store: Option<Arc<dyn CheckpointStore>>,
    /// Optional hook executor. Fired as `HookEvent::OnSpawnFailure` when a
    /// node's `deadline_action == Escalate` trips.
    pub hook_executor: Option<Arc<HookExecutor>>,
    /// Optional workspace-contract context (coding-blue FA-7). When
    /// populated the executor propagates the parent's compaction
    /// policy onto LLM-call nodes, reserves cost-ledger budget per
    /// node, and runs the declared completion-phase validators at the
    /// pipeline terminal. `None` = legacy behaviour (pre-FA-7),
    /// byte-for-byte identical to the v0 path.
    pub workspace_context: PipelineContext,
    /// M8 parity (W1.A1/A3): snapshot of the parent session's shared
    /// resources (FileStateCache, SubAgentOutputRouter,
    /// AgentSummaryGenerator, TaskSupervisor) picked up via TOOL_CTX
    /// at run_pipeline dispatch. Default = empty, which keeps every
    /// pre-M8 invocation site bitwise identical.
    pub host_context: crate::host_context::PipelineHostContext,
    /// NEW-06 fix: parent-session embedder forwarded onto every per-
    /// node worker [`octos_agent::Agent`] so episodic memory recall
    /// stays on the contamination-safe hybrid scored + filtered path.
    ///
    /// `None` means per-node workers SKIP episodic recall entirely (the
    /// no-embedder branch in `octos_agent::agent::memory`) — BM25-only cwd
    /// recall can't separate cross-task episodes, so injecting it would leak
    /// stale unrelated memory; skipping is the contamination-safe choice.
    pub embedder: Option<Arc<dyn octos_llm::EmbeddingProvider>>,
    /// Phase 2-A — directory used to load `pipeline_models.json` and
    /// `model_catalog.json` for per-node model assignment, plus to
    /// surface profile-level defaults that must not move when
    /// `working_dir` is overridden onto a session-scoped workspace.
    ///
    /// Pipeline runs invoked from a scoped session set this to the
    /// **profile** data dir so catalog reads resolve against the
    /// persistent profile root, even though `working_dir` was swapped
    /// to `scope.workspace()` for per-node worker CWD isolation.
    ///
    /// `None` keeps the pre-Phase-2-A path: catalog reads fall back to
    /// `working_dir` (which, for non-scoped callers, IS the profile
    /// dir). Codex review of #1203 caught this overload — without the
    /// split, scoped runs silently lost strong/fast model defaults +
    /// cost projections.
    pub catalog_dir: Option<PathBuf>,
    /// #1607 (codex-review follow-up): the session sandbox threaded from the
    /// `RunPipelineTool` construction site. `run_terminal_validators` /
    /// `run_node_validators` build a workspace-scoped `ToolRegistry` for the
    /// declared-validator pass; without this they stored `NoSandbox`, so an
    /// untrusted `workspace_policy.toml` `Command` validator could execute
    /// directly on the host from a sandboxed pipeline. Building that registry
    /// via `with_builtins_and_sandbox(&working_dir, create_sandbox(&sandbox))`
    /// confines command validators to the same backend the pipeline's shell/
    /// exec tools use. Default (`SandboxConfig::default()` → `NoSandbox` on a
    /// host without a backend) runs command validators directly — byte-for-byte
    /// identical to the pre-#1607 path.
    pub sandbox: octos_agent::SandboxConfig,
}

/// A single planned sub-task from the LLM planner.
///
/// Accepts multiple field name variants because different LLMs use different
/// names for the same concept (task/query/topic/angle/description).
#[derive(Debug, Clone, Deserialize)]
struct DynamicTask {
    #[serde(
        alias = "query",
        alias = "topic",
        alias = "angle",
        alias = "description",
        alias = "search",
        alias = "instruction"
    )]
    task: String,
    #[serde(default, alias = "name", alias = "title")]
    label: Option<String>,
}

/// Report pipeline progress via the task-local TOOL_CTX reporter (if available).
pub(crate) fn report_progress(message: &str) {
    if let Ok(ctx) = TOOL_CTX.try_with(|c| c.clone()) {
        ctx.reporter.report(ProgressEvent::ToolProgress {
            name: "run_pipeline".to_string(),
            tool_id: ctx.tool_id.clone(),
            message: message.to_string(),
        });
    }
}

/// Maximum bytes kept in a per-node partial-output preview (Gap 4.2). Small on
/// purpose: progress events are frequent, so a preview must not approach the
/// 16 KiB harness-event line cap (`MAX_HARNESS_EVENT_LINE_BYTES`) nor the 1 MiB
/// frame cap. Reuses the Gap-3.4 [`FidelityMode::Truncate`] truncation so a
/// huge node output can't bloat the event.
pub(crate) const NODE_PREVIEW_MAX_CHARS: usize = 2 * 1024;

/// Blocker 1 — hard cap on the free-form `node_id` (and any other free-form
/// string) carried in a node-progress event's `extra` map. A `node_id` is
/// otherwise UNBOUNDED at the call sites (graph node ids, dynamic
/// `<node>_task_<i>` worker ids), so a pathological/long id plus a large
/// preview can serialize past the 16 KiB harness-event line cap — at which
/// point the reader silently DROPS the event (back to opaque, defeating the
/// gap). Bounding the id to a small cap, combined with the per-event
/// serialized-budget check in [`emit_pipeline_node_event`], keeps the
/// assembled line provably under `MAX_HARNESS_EVENT_LINE_BYTES`.
///
/// NOTE: used as `FidelityMode::Truncate { max_chars }`, which truncates at a
/// BYTE offset snapped to a char boundary — so this is effectively a 256-BYTE
/// cap (a generous reasonable cap for any real node/worker id).
pub(crate) const NODE_ID_MAX_CHARS: usize = 256;

/// Linear ETA estimate (seconds remaining) for a pipeline run (Gap 4.2).
///
/// `(elapsed / nodes_done) * nodes_remaining`. Degrades gracefully: returns
/// `None` ("estimating…") when fewer than one node has completed — there is no
/// per-node rate to extrapolate from yet — or when the run is already at/over
/// the total node count.
pub(crate) fn linear_eta_secs(
    elapsed_secs: u64,
    nodes_done: usize,
    nodes_total: usize,
) -> Option<u64> {
    if nodes_done == 0 || nodes_total == 0 || nodes_done >= nodes_total {
        return None;
    }
    let remaining = (nodes_total - nodes_done) as u64;
    // Per-node rate from observed work; integer math keeps it monotone-ish and
    // avoids float drift on the heartbeat. NIT — saturate the multiply so a
    // pathological huge `elapsed_secs` (× many remaining nodes) clamps to
    // `u64::MAX` instead of overflowing/panicking. The increase-while-a-long-
    // node-runs behaviour is inherent to the naive linear formula and left as-is.
    let per_node = elapsed_secs / nodes_done as u64;
    Some(per_node.saturating_mul(remaining))
}

/// Bound a node's output to a short preview using the Gap-3.4 fidelity
/// truncation (UTF-8 safe, appends a `[truncated]` marker). Whitespace-trimmed
/// so a preview chip is compact in the UI.
pub(crate) fn node_output_preview(content: &str) -> String {
    let preview = crate::fidelity::FidelityMode::Truncate {
        max_chars: NODE_PREVIEW_MAX_CHARS,
    }
    .apply(content.trim());
    preview.trim().to_string()
}

/// Blocker 1 — JSON-escaped byte budget for the node-event `message` (the
/// `label (N of M)` string). The `message` is otherwise (a) only RAW-byte
/// bounded to 2 KiB *downstream*, so a >2 KiB raw label is REJECTED by the
/// validator and the whole event silently fails to emit; and (b) NOT counted
/// in the line budget, so a control-byte-heavy 2 KiB label escapes ~6× to
/// ~12 KiB and — added to the free-form node_id + preview budget — pushes the
/// serialized line past the 16 KiB reader cap. Bounding the message by its
/// ESCAPED length to a small cap keeps it both under the 2 KiB raw validator
/// bound (escaped ≥ raw, so escaped ≤ 1 KiB ⇒ raw ≤ 1 KiB) and small in the
/// line, so it never trips the validator and never blows the line budget.
const NODE_MESSAGE_ESCAPED_BUDGET: usize = 1024;

/// Blocker 1 — serialized-budget headroom reserved for the harness-event
/// envelope NOT covered by the bounded free-form fields (the schema, kind,
/// session/task/workflow ids, phase, the integer counters, the JSON scaffold,
/// and per-event escaping slack). The bounded message, node_id, and preview are
/// all measured against the SERIALIZED line below, so this reserve only needs to
/// cover the FIXED scaffold + the bounded fixed-size fields. The fixed fields
/// ARE bounded: `session_id` ≤256 B, `task_id` ≤128 B, `workflow` (pipeline id)
/// ≤128 B and control-free (escapes 1:1 — see [`graph::validate_pipeline_id`]),
/// `phase` ≤64 B, counters/progress ≤32 B; with key names + JSON scaffold this
/// is well under 2 KiB, so 6 KiB is a generous reserve. Even so, the final line
/// is checked against the ACTUAL serialized length, so this is belt-and-braces.
const HARNESS_EVENT_ENVELOPE_RESERVE: usize = 6 * 1024;

// Compile-time guard: the envelope reserve + the (capped) node_id's WORST-CASE
// JSON-escaped length + the message escaped budget must leave room under the
// line cap for a non-trivial preview, so a node event is NEVER droppable for a
// normal node. The runtime path measures the ACTUAL serialized line and shrinks
// the elastic preview to guarantee the bound; this guard only proves headroom
// exists. `FidelityMode::Truncate { max_chars }` caps node_id at
// NODE_ID_MAX_CHARS BYTES (offset snapped to a char boundary) plus a fixed
// `\n... [truncated]` marker (<=32 bytes). An all-control-byte body escapes
// <=6x/byte, so the bounded node_id serializes to <= 6 * (NODE_ID_MAX_CHARS +
// 32) bytes.
const _: () = {
    assert!(
        HARNESS_EVENT_ENVELOPE_RESERVE + NODE_MESSAGE_ESCAPED_BUDGET + 6 * (NODE_ID_MAX_CHARS + 32)
            < octos_agent::harness_events::MAX_HARNESS_EVENT_LINE_BYTES
    );
};

/// Emit a structured per-node progress event into the `octos.harness.event.v1`
/// contract (Gap 4.2). Reads the harness event sink from `TOOL_CTX`; a no-op
/// when no sink is attached (out-of-band callers / unit tests without a sink).
///
/// The canonical `phase`/`message`/`progress` keep working for consumers that
/// ignore the structured fields; the `extra` map carries `node`, `node_index`,
/// `node_total`, and (on completion) `success` + a bounded `preview`. These
/// ride the additive `HarnessProgressEvent.extra` flatten so existing
/// consumers are unaffected.
///
/// Blocker 1 — the assembled event is bounded so its SERIALIZED line stays
/// provably under [`MAX_HARNESS_EVENT_LINE_BYTES`](octos_agent::harness_events::MAX_HARNESS_EVENT_LINE_BYTES);
/// otherwise the reader silently DROPS oversized lines, which would defeat the
/// gap (back to opaque). `node_id` — UNBOUNDED at the call sites and copied
/// verbatim into `extra` (which the Progress validator does NOT inspect) — is
/// capped to [`NODE_ID_MAX_CHARS`], and the `preview` is shrunk by its
/// JSON-ESCAPED length (reusing the Gap-3.4 `json_escaped_len` accounting, since
/// an all-control-byte body escapes up to 6×) so that
/// `escaped(node_id) + escaped(preview) <= MAX_HARNESS_EVENT_LINE_BYTES -
/// HARNESS_EVENT_ENVELOPE_RESERVE`. The reserve covers every other (already
/// bounded) field plus the JSON scaffold, so the full line is under the cap.
///
/// One-shot "emit from the LIVE `TOOL_CTX`" path: resolves the sink + context
/// from the task-local and delegates to [`emit_node_event_to_sink`]. The node
/// execution paths now arm a [`NodeProgressGuard`] (which captures the sink at
/// arm time for cancellation-safety) rather than calling this directly, so this
/// remains as the documented direct-emit API and is exercised by the bounding
/// tests; allow it to be dead outside `cfg(test)`.
#[cfg_attr(not(test), allow(dead_code))]
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_pipeline_node_event(
    pipeline_id: &str,
    phase: &str,
    message: &str,
    node_id: &str,
    node_index: usize,
    node_total: usize,
    success: Option<bool>,
    preview: Option<&str>,
) {
    let Ok(Some(sink)) = TOOL_CTX.try_with(|c| c.harness_event_sink.clone()) else {
        return;
    };
    // The sink context (session/task ids) is needed to assemble the candidate
    // event so we can measure its SERIALIZED line. No context ⇒ nothing to
    // write (same no-op semantics as the registered-emit path).
    let Some(context) = octos_agent::harness_events::lookup_event_sink_context(&sink) else {
        return;
    };
    emit_node_event_to_sink(
        &sink,
        &context,
        pipeline_id,
        phase,
        message,
        node_id,
        node_index,
        node_total,
        success,
        preview,
    );
}

/// Lower-level node-event emit that takes the sink + context EXPLICITLY rather
/// than reading them from the `TOOL_CTX` task-local. This is the path the RAII
/// [`NodeProgressGuard`] uses: it captures the sink/context at ARM time (while
/// `TOOL_CTX` is live) and replays them from `Drop`, where the task-local may be
/// gone (cancellation drops the run future from a different context, and during
/// a panic unwind the task-local frame may already be torn down). All field
/// bounding (Blocker 1) and the workflow truncation (Blocker 3) live here so
/// both the live-context [`emit_pipeline_node_event`] path and the guard-Drop
/// path are bounded identically and the assembled line is PROVABLY emittable.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_node_event_to_sink(
    sink: &str,
    context: &octos_agent::harness_events::HarnessEventSinkContext,
    pipeline_id: &str,
    phase: &str,
    message: &str,
    node_id: &str,
    node_index: usize,
    node_total: usize,
    success: Option<bool>,
    preview: Option<&str>,
) {
    let progress = if node_total > 0 {
        Some((node_index as f64 / node_total as f64).clamp(0.0, 1.0))
    } else {
        None
    };

    // Blocker 3 — bound the workflow (pipeline/graph) id at the emit site. The
    // DOT parser accepts an UNBOUNDED graph id and the harness validator REJECTS
    // a `workflow` over `MAX_WORKFLOW_BYTES`, so a long graph id would make
    // `write_event_to_sink` reject the WHOLE event — and the preview-shrink loop
    // can't fix it (the id is not elastic), so it would silently DROP (back to
    // opaque). Bounding by JSON-ESCAPED length to the validator cap guarantees
    // BOTH the raw bound (escaped ≥ raw ⇒ raw ≤ cap, so the validator passes)
    // AND a bounded line contribution. A parsed graph id is never empty (the
    // parser defaults to "pipeline"), so the non-empty validator rule holds.
    let bounded_workflow =
        bound_str_to_escaped_budget(pipeline_id, octos_agent::harness_events::MAX_WORKFLOW_BYTES);

    // Bound node_id to a small cap (UTF-8 safe; Gap-3.4 truncation appends a
    // marker when it shortens). This is the free-form key the call sites can't
    // bound (graph ids, dynamic `<node>_task_<i>` worker ids).
    let bounded_node_id = crate::fidelity::FidelityMode::Truncate {
        max_chars: NODE_ID_MAX_CHARS,
    }
    .apply(node_id);

    // Blocker 1 — bound the `message` (the free-form `label (N of M)` string)
    // by its JSON-ESCAPED length. The label is UNBOUNDED at the call sites, and
    // the downstream validator REJECTS a >2 KiB *raw* message — so a long label
    // would silently drop the whole event. Bounding by escaped length keeps it
    // both under the raw validator bound (escaped ≥ raw) and small in the line.
    let bounded_message = bound_str_to_escaped_budget(message, NODE_MESSAGE_ESCAPED_BUDGET);

    // The free-form budget the node_id + preview's JSON-escaped lengths must
    // jointly fit. The bounded message + envelope live in the reserve; the
    // assembled line is ALSO measured against the actual serialized length
    // below, so this only seeds an initial preview cap.
    let free_budget = octos_agent::harness_events::MAX_HARNESS_EVENT_LINE_BYTES
        .saturating_sub(HARNESS_EVENT_ENVELOPE_RESERVE)
        .saturating_sub(NODE_MESSAGE_ESCAPED_BUDGET);
    let node_escaped = crate::fidelity::json_escaped_len(&bounded_node_id);
    let mut preview_budget = free_budget.saturating_sub(node_escaped);

    // Assemble the candidate event and shrink the elastic `preview` until the
    // SERIALIZED line is provably under the reader's drop cap. The fixed fields
    // (session/task/workflow ids, phase, counters) and the bounded message are
    // all already small, so at most one shrink iteration is expected; the loop
    // is a hard guarantee against any reserve mis-estimate.
    let build_extra = |preview_budget: usize| {
        let mut extra: HashMap<String, serde_json::Value> = HashMap::new();
        extra.insert(
            "node".to_string(),
            serde_json::Value::String(bounded_node_id.clone()),
        );
        extra.insert(
            "node_index".to_string(),
            serde_json::Value::from(node_index),
        );
        extra.insert(
            "node_total".to_string(),
            serde_json::Value::from(node_total),
        );
        if let Some(s) = success {
            extra.insert("success".to_string(), serde_json::Value::Bool(s));
        }
        if let Some(p) = preview {
            // Shrink the preview by its JSON-ESCAPED length so a control-byte-
            // heavy body (escapes up to 6×) can't push the line over the cap.
            let bounded = bound_str_to_escaped_budget(p, preview_budget);
            extra.insert("preview".to_string(), serde_json::Value::String(bounded));
        }
        extra
    };

    let cap = octos_agent::harness_events::MAX_HARNESS_EVENT_LINE_BYTES;
    let mut event = octos_agent::harness_events::HarnessEvent::progress_with_extra(
        context.session_id.clone(),
        context.task_id.clone(),
        Some(bounded_workflow.clone()),
        phase,
        Some(bounded_message.clone()),
        progress,
        build_extra(preview_budget),
    );
    // Measure the ACTUAL serialized line (what `write_event_to_sink` writes,
    // sans newline) and shrink the preview budget if it's at/over the cap. This
    // makes the bound provable regardless of the reserve estimate.
    while preview_budget > 0 {
        match serde_json::to_string(&event) {
            Ok(line) if line.len() < cap => break,
            Ok(line) => {
                // Shrink by the overshoot (plus a small margin) — never below 0.
                let overshoot = line.len().saturating_sub(cap) + 64;
                preview_budget = preview_budget.saturating_sub(overshoot.max(preview_budget / 2));
                event = octos_agent::harness_events::HarnessEvent::progress_with_extra(
                    context.session_id.clone(),
                    context.task_id.clone(),
                    Some(bounded_workflow.clone()),
                    phase,
                    Some(bounded_message.clone()),
                    progress,
                    build_extra(preview_budget),
                );
            }
            Err(_) => return,
        }
    }
    // `write_event_to_sink` re-validates + writes the single line atomically.
    let _ = octos_agent::harness_events::write_event_to_sink(sink, &event);
}

/// Bound `s` so its JSON-escaped length is `<= budget`, snapping the kept
/// prefix to a UTF-8 char boundary. Reuses the Gap-3.4 escape accounting
/// (`json_escaped_len`): `json_escaped_len` is monotonic non-decreasing in the
/// prefix length, so a binary search on the byte offset converges in O(log n).
/// Returns `s` unchanged when it already fits (no false truncation).
fn bound_str_to_escaped_budget(s: &str, budget: usize) -> String {
    if crate::fidelity::json_escaped_len(s) <= budget {
        return s.to_string();
    }
    let bytes = s.as_bytes();
    let mut lo = 0usize; // largest offset known to FIT
    let mut hi = bytes.len(); // smallest offset known NOT to fit
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        // Snap the probe down to a char boundary so escaped-len is measured on
        // a valid &str slice (and we never split a scalar).
        let mut probe = mid;
        while probe > lo && !s.is_char_boundary(probe) {
            probe -= 1;
        }
        if probe == lo {
            // No boundary between lo and mid; advance hi to guarantee progress.
            hi = mid;
            continue;
        }
        if crate::fidelity::json_escaped_len(&s[..probe]) <= budget {
            lo = probe;
        } else {
            hi = probe;
        }
    }
    s[..lo].to_string()
}

/// Gap 4.2 / Blocker 1+2 — RAII guard that pairs every `node_started` with a
/// `node_completed` on EVERY exit path (normal completion, an early `?`-return,
/// a panic unwind, or a cancellation that drops the run future mid-node).
///
/// Mirrors the [`ProcessGroupKillGuard`](octos_agent) "limits degrade, never
/// leak" pattern: the guard emits `node_started` at construction (ARM), and its
/// `Drop` emits a terminal `node_completed { success: false }` UNLESS the node
/// completed normally — in which case [`NodeProgressGuard::complete`] emitted
/// the real `node_completed` and DISARMED the guard. The result is exactly one
/// `node_started` + exactly one `node_completed` per node that actually starts;
/// a node whose future is never polled (a never-armed guard) emits nothing.
///
/// Cancellation-safety: the sink path AND the session/task context are CAPTURED
/// at arm time (while `TOOL_CTX` is live). The `Drop` emit replays them via
/// [`emit_node_event_to_sink`] WITHOUT reading the task-local — so it works even
/// when the run future is dropped from a different async context (cancellation)
/// or while the task-local frame is being torn down during a panic unwind. When
/// no sink is attached at arm time (out-of-band callers / unit tests), the guard
/// is inert: it emits nothing and its `Drop` is a no-op.
///
/// Drop is best-effort and PANIC-SAFE: it holds no locks across the emit, takes
/// no `unwrap`, and ignores all IO errors (`emit_node_event_to_sink` already
/// swallows write errors). A panic in `Drop` during an unwind would be a
/// double-panic = abort, so the emit path is kept allocation-light and
/// infallible from the guard's perspective.
struct NodeProgressGuard {
    /// `Some` only when a sink was attached at arm time; `None` ⇒ inert guard.
    sink: Option<String>,
    context: Option<octos_agent::harness_events::HarnessEventSinkContext>,
    pipeline_id: String,
    node_id: String,
    label: String,
    node_index: usize,
    node_total: usize,
    /// `true` until `complete()` runs; a still-armed guard emits the terminal
    /// `node_completed { success: false }` from `Drop`.
    armed: bool,
}

impl NodeProgressGuard {
    /// Arm the guard: capture the sink/context from `TOOL_CTX` (synchronously,
    /// so it survives a later cancellation/unwind) and emit `node_started`.
    fn arm(
        pipeline_id: &str,
        node_id: &str,
        label: &str,
        node_index: usize,
        node_total: usize,
    ) -> Self {
        // Capture the sink + context NOW, while the task-local is live. `None`
        // when no sink is attached ⇒ inert guard (no emit, no-op Drop).
        let sink = TOOL_CTX
            .try_with(|c| c.harness_event_sink.clone())
            .ok()
            .flatten();
        let context = sink
            .as_deref()
            .and_then(octos_agent::harness_events::lookup_event_sink_context);

        let mut guard = Self {
            sink,
            context,
            pipeline_id: pipeline_id.to_string(),
            node_id: node_id.to_string(),
            label: label.to_string(),
            node_index,
            node_total,
            // Stay disarmed until the started emit lands below; a guard with no
            // sink/context stays inert (no emit, no-op Drop).
            armed: false,
        };
        guard.emit("node_started", None, None);
        // Arm AFTER the started emit, and only when there is somewhere to write,
        // so the Drop terminal-event path can fire on every early exit.
        guard.armed = guard.sink.is_some() && guard.context.is_some();
        guard
    }

    /// Emit a node event through the captured (not task-local) sink. No-op when
    /// the guard is inert (no sink/context captured at arm time).
    fn emit(&self, phase: &str, success: Option<bool>, preview: Option<&str>) {
        let (Some(sink), Some(context)) = (self.sink.as_deref(), self.context.as_ref()) else {
            return;
        };
        let suffix = match success {
            Some(true) => " — done",
            Some(false) => " — failed",
            None => "",
        };
        let message = format!(
            "{} ({} of {}){suffix}",
            self.label, self.node_index, self.node_total
        );
        emit_node_event_to_sink(
            sink,
            context,
            &self.pipeline_id,
            phase,
            &message,
            &self.node_id,
            self.node_index,
            self.node_total,
            success,
            preview,
        );
    }

    /// Normal-completion path: emit the real `node_completed` (with the node's
    /// success + bounded preview) and DISARM so `Drop` does not double-emit.
    fn complete(mut self, success: bool, preview: &str) {
        self.emit("node_completed", Some(success), Some(preview));
        self.armed = false;
    }
}

impl Drop for NodeProgressGuard {
    fn drop(&mut self) {
        // Best-effort, panic-safe: if the node completed normally `complete()`
        // already disarmed us. A still-armed guard means an early `?`-return,
        // a panic unwind, or a cancellation drop happened between `node_started`
        // and the normal completion — emit a terminal `node_completed{false}` so
        // the chip flips off "running" instead of dangling forever.
        if !self.armed {
            return;
        }
        self.armed = false;
        self.emit(
            "node_completed",
            Some(false),
            Some("interrupted: node did not complete (error, cancellation, or panic)"),
        );
    }
}

/// Shared status snapshot updated by the pipeline executor and read by the
/// periodic heartbeat task. Lets the chat bubble see a refreshing status
/// chip during long-running phases (`plan_and_search` 13min, `analyze`
/// 9min) where existing milestone-only emits leave a 5+ min gap between
/// visible updates.
#[derive(Clone, Debug)]
pub(crate) struct PipelineStatusSnapshot {
    pub(crate) pipeline_id: String,
    pub(crate) current_node: String,
    pub(crate) nodes_done: usize,
    pub(crate) nodes_total: usize,
    pub(crate) start: Instant,
}

/// RAII guard around the heartbeat `JoinHandle` so the spawned task is
/// aborted on every return path of `run_with_handlers` (Ok, Err, early
/// returns inside the main loop, panics that unwind through).
struct HeartbeatGuard {
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for HeartbeatGuard {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// Spawn the heartbeat. Captures `reporter` + `tool_id` from `TOOL_CTX`
/// synchronously (tokio::spawn would otherwise lose the task-local), then
/// ticks every `interval` and emits a refreshing `ToolProgress` event.
/// Returns `None` when no `TOOL_CTX` is active (out-of-band callers / unit
/// tests) — in that case the heartbeat would be silent anyway.
fn spawn_pipeline_heartbeat(
    status: Arc<std::sync::Mutex<PipelineStatusSnapshot>>,
    interval_secs: u64,
) -> Option<HeartbeatGuard> {
    let ctx = TOOL_CTX.try_with(|c| c.clone()).ok()?;
    let reporter = ctx.reporter.clone();
    let tool_id = ctx.tool_id.clone();
    // Capture the harness event sink synchronously too — `tokio::spawn` loses
    // the task-local, so the heartbeat can't read TOOL_CTX from inside the
    // spawned task. `None` when no sink is attached (heartbeat then emits only
    // the plain ToolProgress chip, as before).
    let harness_sink = ctx.harness_event_sink.clone();
    tracing::info!(
        target: "octos::pipeline::heartbeat",
        tool_id = %tool_id,
        interval_secs,
        "spawn_pipeline_heartbeat: spawned"
    );
    let handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));
        // Skip the immediate first tick — the executor itself emits a
        // `"Pipeline '...' started"` event at T+0, and we don't want a
        // duplicate before it lands.
        interval.tick().await;
        let mut tick_count: u64 = 0;
        loop {
            interval.tick().await;
            tick_count += 1;
            let snap = match status.lock() {
                Ok(g) => g.clone(),
                Err(p) => p.into_inner().clone(),
            };
            let elapsed = snap.start.elapsed().as_secs();
            // Gap 4.2 — naive linear ETA surfaced on every heartbeat. Degrades
            // to "estimating…" until ≥1 node has completed (no per-node rate
            // to extrapolate yet).
            let eta = linear_eta_secs(elapsed, snap.nodes_done, snap.nodes_total);
            let eta_label = match eta {
                Some(secs) => format!("~{secs}s left"),
                None => "estimating…".to_string(),
            };
            let message = if snap.nodes_total > 0 {
                format!(
                    "Pipeline '{}' running: {} ({}/{} nodes, {}s elapsed, {})",
                    snap.pipeline_id,
                    snap.current_node,
                    snap.nodes_done,
                    snap.nodes_total,
                    elapsed,
                    eta_label,
                )
            } else {
                format!(
                    "Pipeline '{}' running: {} ({}s elapsed)",
                    snap.pipeline_id, snap.current_node, elapsed,
                )
            };
            tracing::info!(
                target: "octos::pipeline::heartbeat",
                tick = tick_count,
                elapsed_s = elapsed,
                node = %snap.current_node,
                eta_s = ?eta,
                "heartbeat tick: {message}"
            );
            reporter.report(ProgressEvent::ToolProgress {
                name: "run_pipeline".to_string(),
                tool_id: tool_id.clone(),
                message: message.clone(),
            });
            // Mirror the heartbeat into the structured harness.event.v1
            // contract so consumers that render structured progress see the
            // ETA + N/M as typed fields, not just an opaque chip string. The
            // ToolProgress above stays for plain chat-bubble consumers.
            if let Some(sink) = harness_sink.as_deref() {
                let progress = if snap.nodes_total > 0 {
                    Some((snap.nodes_done as f64 / snap.nodes_total as f64).clamp(0.0, 1.0))
                } else {
                    None
                };
                let mut extra: HashMap<String, serde_json::Value> = HashMap::new();
                extra.insert(
                    "node".to_string(),
                    serde_json::Value::String(snap.current_node.clone()),
                );
                extra.insert(
                    "node_index".to_string(),
                    serde_json::Value::from(snap.nodes_done),
                );
                extra.insert(
                    "node_total".to_string(),
                    serde_json::Value::from(snap.nodes_total),
                );
                extra.insert("elapsed_secs".to_string(), serde_json::Value::from(elapsed));
                if let Some(secs) = eta {
                    extra.insert("eta_secs".to_string(), serde_json::Value::from(secs));
                }
                let _ = octos_agent::harness_events::emit_registered_progress_event_with_extra(
                    sink,
                    Some(snap.pipeline_id.as_str()),
                    "heartbeat",
                    &message,
                    progress,
                    extra,
                );
            }
        }
    });
    Some(HeartbeatGuard { handle })
}

/// Resolve an LLM provider from a model key using an optional router.
fn resolve_provider(
    default: &Arc<dyn LlmProvider>,
    router: Option<&Arc<ProviderRouter>>,
    model_key: Option<&str>,
) -> Result<Arc<dyn LlmProvider>> {
    match (model_key, router) {
        (Some(key), Some(r)) => r.resolve(key),
        (Some(key), None) => {
            warn!(
                model = key,
                "model override but no provider router; using default"
            );
            Ok(default.clone())
        }
        _ => Ok(default.clone()),
    }
}

/// Call LLM to plan dynamic tasks from a prompt + user input.
async fn plan_dynamic_tasks(
    provider: &dyn LlmProvider,
    planning_prompt: &str,
    user_input: &str,
    max_tasks: u32,
) -> Result<(Vec<DynamicTask>, TokenUsage)> {
    let prompt = format!(
        "{planning_prompt}\n\nUser query: {user_input}\n\n\
         IMPORTANT: Respond with ONLY a JSON array of tasks. No explanation, \
         no markdown, no code fences. Example format:\n\
         [{{\"task\": \"search for X\", \"label\": \"Label\"}}, \
         {{\"task\": \"search for Y\", \"label\": \"Label\"}}]\n\
         Generate up to {max_tasks} tasks."
    );

    let messages = vec![
        Message {
            role: MessageRole::System,
            content: "You are a research planner. Output ONLY a JSON array. \
                      No other text."
                .to_string(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        },
        Message {
            role: MessageRole::User,
            content: prompt,
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        },
    ];

    let config = ChatConfig {
        max_tokens: Some(4096),
        ..Default::default()
    };

    let response = provider.chat(&messages, &[], &config).await?;
    let usage = TokenUsage {
        input_tokens: response.usage.input_tokens,
        output_tokens: response.usage.output_tokens,
        ..Default::default()
    };

    // Try content first, then reasoning_content (for reasoning models like kimi-k2.5)
    let content = response.content.unwrap_or_default();
    let text = if content.trim().is_empty() {
        response.reasoning_content.as_deref().unwrap_or("")
    } else {
        &content
    };

    let json_str = extract_json_array(text).ok_or_else(|| {
        let preview: String = text.chars().take(200).collect();
        eyre::eyre!("no JSON array found in planning response: {preview}")
    })?;

    // Try strict parsing first, then fall back to extracting any string values
    let tasks: Vec<DynamicTask> = match serde_json::from_str(json_str) {
        Ok(tasks) => tasks,
        Err(strict_err) => {
            // Fallback: parse as array of generic objects, extract task from
            // the first string field (regardless of field name)
            let preview: String = json_str.chars().take(200).collect();
            tracing::warn!(
                error = %strict_err,
                json_preview = %preview,
                "strict DynamicTask parse failed, trying flexible extraction"
            );
            let arr: Vec<serde_json::Map<String, serde_json::Value>> =
                serde_json::from_str(json_str).map_err(|e| {
                    eyre::eyre!(
                        "failed to parse planning JSON as array of objects: {e}\nJSON: {preview}"
                    )
                })?;
            arr.into_iter()
                .filter_map(|obj| {
                    // Find the first string field as "task", second as "label"
                    let mut strings: Vec<String> = obj
                        .values()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect();
                    if strings.is_empty() {
                        return None;
                    }
                    let task = strings.remove(0);
                    let label = if strings.is_empty() {
                        None
                    } else {
                        Some(strings.remove(0))
                    };
                    Some(DynamicTask { task, label })
                })
                .collect()
        }
    };

    let tasks: Vec<DynamicTask> = tasks.into_iter().take(max_tasks as usize).collect();
    Ok((tasks, usage))
}

/// Generate fallback tasks when the planner fails.
fn fallback_tasks(user_input: &str) -> Vec<DynamicTask> {
    vec![
        DynamicTask {
            task: format!("Search for: {user_input}"),
            label: Some("Primary search".into()),
        },
        DynamicTask {
            task: format!("Search in English for: {user_input}"),
            label: Some("English search".into()),
        },
        DynamicTask {
            task: format!("Search for recent trends and developments: {user_input}"),
            label: Some("Trends".into()),
        },
    ]
}

/// Extract a JSON array from LLM output, handling markdown code fences.
fn extract_json_array(text: &str) -> Option<&str> {
    let text = text.trim();

    // Try direct parse first
    if text.starts_with('[') {
        return Some(text);
    }

    // Look for `[{` specifically — the start of a JSON array of objects.
    // Using bare `[` would greedily match narrative text like "[the angles]".
    if let Some(start) = text.find("[{") {
        if let Some(end) = text.rfind(']') {
            if end > start {
                return Some(&text[start..=end]);
            }
        }
    }

    None
}

fn total_pipeline_tokens(usage: &TokenUsage) -> u32 {
    usage.input_tokens.saturating_add(usage.output_tokens)
}

fn remaining_pipeline_tokens(max_total_tokens: Option<u32>, usage: &TokenUsage) -> Option<u32> {
    let max_total_tokens = max_total_tokens?;
    Some(max_total_tokens.saturating_sub(total_pipeline_tokens(usage)))
}

fn cap_node_output_tokens_for_remaining_budget(
    node: &mut PipelineNode,
    remaining_tokens: u32,
    peer_count: usize,
) {
    if !matches!(node.handler, HandlerKind::Codergen) || remaining_tokens == 0 {
        return;
    }
    let divisor = u32::try_from(peer_count.max(1)).unwrap_or(u32::MAX).max(1);
    let per_node_cap = remaining_tokens.saturating_div(divisor).max(1);
    node.max_output_tokens = Some(
        node.max_output_tokens
            .map_or(per_node_cap, |existing| existing.min(per_node_cap).max(1)),
    );
}

fn collect_completed_files(completed: &HashMap<String, NodeOutcome>) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = completed
        .values()
        .flat_map(|outcome| outcome.files_modified.iter().cloned())
        .collect();
    files.sort();
    files.dedup();
    files
}

/// Process results from parallel worker execution, producing merged content and summaries.
/// Aggregate outcome of a fan-out's worker futures.
struct WorkerResults {
    merged_content: String,
    /// At least one worker returned an `Error` outcome (or its future failed,
    /// e.g. exceeded the per-worker deadline).
    any_error: bool,
    /// At least one worker returned a `Pass` outcome. When this is `false`
    /// EVERY worker failed — the fan-out produced nothing usable, so the
    /// converged node must surface a hard `Error` that terminates the
    /// pipeline (the production `search (0/3 nodes)` total-wedge case),
    /// rather than silently converging on empty content.
    any_pass: bool,
    summaries: Vec<NodeSummary>,
    tokens: TokenUsage,
    outcomes: Vec<(String, NodeOutcome)>,
}

fn process_worker_results(
    results: Vec<(String, PipelineNode, Duration, Result<NodeOutcome>)>,
    bridge: Option<&PipelineStatusBridge>,
    working_dir: &std::path::Path,
) -> WorkerResults {
    let mut merged_parts = Vec::new();
    let mut any_error = false;
    let mut any_pass = false;
    let mut summaries = Vec::new();
    let mut total_tokens = TokenUsage::default();
    let mut outcomes = Vec::new();

    for (task_id, node, elapsed, result) in results {
        let duration_ms = elapsed.as_millis() as u64;
        let label = node.label.as_deref().unwrap_or(&task_id).to_string();

        match result {
            Ok(outcome) => {
                info!(
                    task = %task_id,
                    status = ?outcome.status,
                    duration_ms,
                    "worker completed"
                );

                total_tokens.input_tokens += outcome.token_usage.input_tokens;
                total_tokens.output_tokens += outcome.token_usage.output_tokens;

                if let Some(bridge) = bridge {
                    bridge.add_tokens(&outcome.token_usage);
                }

                summaries.push(NodeSummary {
                    node_id: task_id.clone(),
                    label: label.clone(),
                    model: node.model.clone(),
                    token_usage: outcome.token_usage.clone(),
                    duration_ms,
                    success: outcome.status == OutcomeStatus::Pass,
                });

                match outcome.status {
                    OutcomeStatus::Error => any_error = true,
                    OutcomeStatus::Pass => any_pass = true,
                    // `Fail` / `Skipped` count as neither a hard error nor a
                    // usable pass for the all-failed determination.
                    _ => {}
                }

                merged_parts.push(format!("## {label}\n\n{}", outcome.content));
                outcomes.push((task_id, outcome));
            }
            Err(e) => {
                warn!(task = %task_id, "worker failed: {e}");
                any_error = true;
                let outcome = NodeOutcome {
                    node_id: task_id.clone(),
                    status: OutcomeStatus::Error,
                    content: format!("Error: {e}"),
                    token_usage: TokenUsage::default(),
                    files_modified: vec![],
                };
                summaries.push(NodeSummary {
                    node_id: task_id.clone(),
                    label: label.clone(),
                    model: node.model.clone(),
                    token_usage: TokenUsage::default(),
                    duration_ms,
                    success: false,
                });
                merged_parts.push(format!("## {label}\n\nError: {e}"));
                outcomes.push((task_id, outcome));
            }
        }
    }

    let merged_content = merged_parts.join("\n\n---\n\n");

    // Resolve file references: if workers saved results to disk and output
    // directory paths, read the _search_results.md files and inline their
    // content. This ensures the converge node gets actual data, not just paths.
    let merged_content = resolve_search_result_files(&merged_content, working_dir);

    WorkerResults {
        merged_content,
        any_error,
        any_pass,
        summaries,
        tokens: total_tokens,
        outcomes,
    }
}

/// Scan merged worker output for research directory paths and inline
/// the `_search_results.md` file contents. Workers may output paths like
/// "Results saved to: ./research/topic-slug/" — we find those directories
/// and read their summary files so downstream nodes get actual content.
fn resolve_search_result_files(content: &str, working_dir: &std::path::Path) -> String {
    use std::path::Path;

    let mut result = content.to_string();
    let mut appended = Vec::new();

    // Find research directories referenced in the content
    for line in content.lines() {
        // Look for paths to research directories
        let path_candidates: Vec<&str> = line
            .split_whitespace()
            .filter(|w| w.contains("/research/") || w.contains("_search_results"))
            .collect();

        for candidate in path_candidates {
            let clean = candidate.trim_matches(|c: char| {
                !c.is_alphanumeric() && c != '/' && c != '_' && c != '-' && c != '.'
            });
            let path = Path::new(clean);

            // Try reading _search_results.md from the directory
            let search_results_path = if path.is_dir() {
                path.join("_search_results.md")
            } else if path
                .file_name()
                .map(|f| f == "_search_results.md")
                .unwrap_or(false)
            {
                path.to_path_buf()
            } else {
                continue;
            };

            if search_results_path.exists() {
                match std::fs::read_to_string(&search_results_path) {
                    Ok(file_content) if !file_content.is_empty() => {
                        let preview = if file_content.len() > 50000 {
                            let mut end = 50000;
                            while !file_content.is_char_boundary(end) && end > 0 {
                                end -= 1;
                            }
                            format!("{}...(truncated)", &file_content[..end])
                        } else {
                            file_content
                        };
                        if !appended.iter().any(|p: &String| {
                            p == &search_results_path.to_string_lossy().to_string()
                        }) {
                            appended.push(search_results_path.to_string_lossy().to_string());
                            result.push_str(&format!(
                                "\n\n--- Search results from {} ---\n{}",
                                search_results_path.display(),
                                preview
                            ));
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    // Also scan the working directory for recent research directories
    if appended.is_empty() {
        // Fallback: if no paths found in content, look for research dirs in working_dir
        if let Ok(entries) = std::fs::read_dir(working_dir.join("research")) {
            let mut dirs: Vec<_> = entries
                .filter_map(|e| e.ok())
                .filter(|e| e.path().is_dir())
                .collect();
            // Sort by modified time, newest first
            dirs.sort_by(|a, b| {
                b.metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
                    .cmp(
                        &a.metadata()
                            .and_then(|m| m.modified())
                            .unwrap_or(std::time::SystemTime::UNIX_EPOCH),
                    )
            });
            // Read up to 8 most recent _search_results.md
            for dir in dirs.iter().take(8) {
                let sr = dir.path().join("_search_results.md");
                if sr.exists() {
                    if let Ok(file_content) = std::fs::read_to_string(&sr) {
                        if !file_content.is_empty() && file_content.len() > 100 {
                            let preview = if file_content.len() > 50000 {
                                // Find a valid char boundary near 50000 bytes
                                let mut end = 50000;
                                while !file_content.is_char_boundary(end) && end > 0 {
                                    end -= 1;
                                }
                                format!("{}...(truncated)", &file_content[..end])
                            } else {
                                file_content
                            };
                            result.push_str(&format!(
                                "\n\n--- Search results from {} ---\n{}",
                                sr.display(),
                                preview
                            ));
                        }
                    }
                }
            }
        }
    }

    result
}

/// The main pipeline executor.
pub struct PipelineExecutor {
    config: ExecutorConfig,
    /// Explicit DAG-scheduler override. `Some(true)`/`Some(false)` force the
    /// scheduler on/off and take PRECEDENCE over the `OCTOS_PIPELINE_DAG` env;
    /// `None` defers to the env. Default `None` → byte-identical production
    /// until an operator opts in. The builder sets it so a test can force the
    /// legacy path even when an opted-in env (`OCTOS_PIPELINE_DAG=1`) is set.
    dag_override: Option<bool>,
}

impl PipelineExecutor {
    pub fn new(config: ExecutorConfig) -> Self {
        Self {
            config,
            dag_override: None,
        }
    }

    /// Builder: force the DAG scheduler on or off, overriding the env. Used by
    /// tests (env vars race across parallel tests) and by callers that must pin
    /// a path deterministically.
    pub fn with_dag_scheduler(mut self, on: bool) -> Self {
        self.dag_override = Some(on);
        self
    }

    /// Builder: attach a workspace-contract context (coding-blue FA-7).
    ///
    /// Replaces the executor's current [`PipelineContext`] with the
    /// caller-supplied one. When the context's `is_empty()` is `true`
    /// the executor stays on the legacy path (validators, compaction,
    /// and cost reservation are all inert); otherwise every LLM-call
    /// node inherits the parent's compaction policy, the pipeline-level
    /// reservation runs at dispatch start, and the declared terminal
    /// validators fire after the final edge is selected.
    ///
    /// Example:
    /// ```ignore
    /// let ctx = PipelineContext::new()
    ///     .with_policy(workspace_policy)
    ///     .with_agent_llm_provider(llm.clone())
    ///     .with_cost_accountant(accountant.clone())
    ///     .with_contract_id("slides-delivery")
    ///     .with_projected_usd(0.25);
    /// let exec = PipelineExecutor::new(config).with_workspace_context(ctx);
    /// ```
    pub fn with_workspace_context(mut self, context: PipelineContext) -> Self {
        self.config.workspace_context = context;
        self
    }

    /// Access the currently installed workspace context. Returns an
    /// empty context when the caller never opted in.
    pub fn workspace_context(&self) -> &PipelineContext {
        &self.config.workspace_context
    }

    fn max_concurrent_llm_calls(&self) -> usize {
        self.config
            .max_concurrent_llm_calls
            .unwrap_or(DEFAULT_PIPELINE_MAX_CONCURRENT_LLM_CALLS)
            .max(1)
    }

    fn pipeline_llm_semaphore(&self) -> Arc<tokio::sync::Semaphore> {
        Arc::new(tokio::sync::Semaphore::new(self.max_concurrent_llm_calls()))
    }

    #[doc(hidden)]
    pub fn max_concurrent_llm_calls_for_test(&self) -> usize {
        self.max_concurrent_llm_calls()
    }

    /// Run a pipeline from a DOT string.
    pub async fn run(
        &self,
        dot_content: &str,
        user_input: &str,
        variables: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<PipelineResult> {
        let llm_semaphore = self.pipeline_llm_semaphore();
        let handlers = self.build_handlers(Some(llm_semaphore.clone()));
        let graph = parse_dot(dot_content).wrap_err("failed to parse pipeline DOT")?;
        self.run_graph_with_handlers_throttled(
            graph,
            user_input,
            variables,
            handlers,
            llm_semaphore,
        )
        .await
    }

    /// Run an already-parsed [`PipelineGraph`] using the executor's default
    /// handler registry — the graph-accepting analog of [`Self::run`]. Lets an
    /// IR-composed graph execute with the full production tool set without
    /// round-tripping through DOT text.
    pub async fn run_graph(
        &self,
        graph: PipelineGraph,
        user_input: &str,
        variables: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<PipelineResult> {
        let llm_semaphore = self.pipeline_llm_semaphore();
        let handlers = self.build_handlers(Some(llm_semaphore.clone()));
        self.run_graph_with_handlers_throttled(
            graph,
            user_input,
            variables,
            handlers,
            llm_semaphore,
        )
        .await
    }

    /// Compile an L2 typed-IR program (see [`crate::compose`]) under `profile`
    /// and run it. The IR is lowered straight to a [`PipelineGraph`] and never
    /// round-trips through DOT text. Compose-time failures (unknown kind,
    /// dangling edge, cycle, profile violation) are surfaced as an error
    /// carrying the structured repair feedback.
    pub async fn run_ir(
        &self,
        ir_json: &str,
        profile: &crate::profile::ValidationProfile,
        user_input: &str,
        variables: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<PipelineResult> {
        let graph = crate::compose::compose(ir_json, profile, variables)
            .map_err(|e| eyre::eyre!("IR compose failed: {e}"))?;
        let llm_semaphore = self.pipeline_llm_semaphore();
        let handlers = self.build_handlers(Some(llm_semaphore.clone()));
        self.run_graph_with_handlers_throttled(
            graph,
            user_input,
            variables,
            handlers,
            llm_semaphore,
        )
        .await
    }

    /// Run a pipeline from a DOT string using a caller-supplied handler
    /// registry. Useful for tests that want to install a custom
    /// `Handler` against a given `HandlerKind` without touching the
    /// executor's default wiring.
    pub async fn run_with_handlers(
        &self,
        dot_content: &str,
        user_input: &str,
        variables: &serde_json::Map<String, serde_json::Value>,
        handlers: HandlerRegistry,
    ) -> Result<PipelineResult> {
        let graph = parse_dot(dot_content).wrap_err("failed to parse pipeline DOT")?;
        self.run_graph_with_handlers(graph, user_input, variables, handlers)
            .await
    }

    /// Run a pipeline from an already-parsed [`PipelineGraph`] using a
    /// caller-supplied handler registry. This graph-accepting entry lets
    /// L2-composed pipelines (see [`crate::compose`]) execute WITHOUT
    /// round-tripping through DOT text — [`Self::run_with_handlers`] is now just
    /// `parse_dot(...)` followed by this. The per-node capability lock
    /// (`CodergenHandler`'s tool deny-list) runs regardless of how the graph was
    /// built, so a compiled-IR graph inherits identical gating to a DOT graph.
    pub async fn run_graph_with_handlers(
        &self,
        graph: PipelineGraph,
        user_input: &str,
        variables: &serde_json::Map<String, serde_json::Value>,
        handlers: HandlerRegistry,
    ) -> Result<PipelineResult> {
        // Caller supplied their own handlers (whose codergen may or may not be
        // throttled). Mint a fresh pipeline-scoped semaphore for the planner /
        // fan-out dispatch on the legacy `execute_graph` path so this entry
        // still bounds concurrent LLM calls.
        let llm_semaphore = self.pipeline_llm_semaphore();
        self.run_graph_with_handlers_throttled(
            graph,
            user_input,
            variables,
            handlers,
            llm_semaphore,
        )
        .await
    }

    /// Body of [`Self::run_graph_with_handlers`] with an explicit
    /// pipeline-scoped LLM concurrency semaphore. The semaphore bounds the
    /// planner / fan-out LLM dispatch on the legacy `execute_graph` path so a
    /// parallel worker retry storm can't exceed `max_concurrent_llm_calls`
    /// without changing global provider/router behavior. The DAG path
    /// (`execute_graph_dag`) never runs the planner/fan-out dispatch, and its
    /// codergen handler is already throttled via `build_codergen`. The public
    /// [`Self::run`] / [`Self::run_graph`] / [`Self::run_ir`] entries thread
    /// the SAME semaphore into both `build_handlers` and here so codergen and
    /// planner LLM calls share one limit.
    async fn run_graph_with_handlers_throttled(
        &self,
        mut graph: PipelineGraph,
        user_input: &str,
        variables: &serde_json::Map<String, serde_json::Value>,
        handlers: HandlerRegistry,
        llm_semaphore: Arc<tokio::sync::Semaphore>,
    ) -> Result<PipelineResult> {
        // Replace the historical pipeline-guard plugin's
        // before_tool_call hook with an in-process pass that fills
        // `node.model` / `node.planner_model` for any node the LLM
        // left unset, using the profile's `model_catalog.json` /
        // `pipeline_models.json`.
        //
        // The plugin form has been observed to silently degrade when
        // its manifest fails to parse on daemon bootstrap (load order
        // race); since this assignment is correctness-critical for
        // strong-vs-fast cost/quality routing across nodes, moving it
        // in-process makes the behavior deterministic. See
        // `book/src/skill-development.md`'s "Before You Start: Skill
        // vs. Workspace Contract" rubric and the pipeline-guard case
        // study for the full rationale.
        //
        // Phase 2-A — catalog reads MUST resolve against the profile
        // data dir, NOT the per-session workspace `working_dir` was
        // overridden onto. `catalog_dir` is the explicit split: it
        // defaults to `working_dir` for backward compat (legacy
        // callers where the two are the same), and scoped callers
        // (e.g. `RunPipelineTool::execute`) set it to the profile dir
        // so model assignment + cost projection don't silently degrade.
        // Caught in codex review of PR #1203.
        let catalog_dir = self
            .config
            .catalog_dir
            .as_deref()
            .unwrap_or(&self.config.working_dir);
        crate::model_assignment::assign_from_catalog_dir(&mut graph, catalog_dir);

        // ── Pipeline start: log graph structure ──
        let node_summary: Vec<String> = graph
            .nodes
            .values()
            .map(|n| {
                let model = n.model.as_deref().unwrap_or("default");
                let tools = n.tools.join(",");
                format!(
                    "  {} [model={}, handler={:?}, tools={}]",
                    n.id, model, n.handler, tools
                )
            })
            .collect();
        let edge_summary: Vec<String> = graph
            .edges
            .iter()
            .map(|e| format!("  {} -> {}", e.source, e.target))
            .collect();
        info!(
            nodes = graph.nodes.len(),
            edges = graph.edges.len(),
            "pipeline start\n{}\n{}",
            node_summary.join("\n"),
            edge_summary.join("\n")
        );

        let validation_context = self.validation_context(&graph, catalog_dir, variables);
        let diags = validate::diagnostics_with_context(&graph, &validation_context);

        for diag in &diags {
            match diag.severity {
                validate::Severity::Error => {
                    tracing::error!(
                        rule = diag.rule_id.code(),
                        rule_number = diag.rule,
                        location = ?diag.location,
                        fix_hint = ?diag.fix_hint,
                        "{}",
                        diag.message
                    );
                }
                validate::Severity::Warning => {
                    warn!(
                        rule = diag.rule_id.code(),
                        rule_number = diag.rule,
                        location = ?diag.location,
                        fix_hint = ?diag.fix_hint,
                        "{}",
                        diag.message
                    );
                }
            }
        }

        if validate::has_errors(&diags) {
            let errors: Vec<_> = diags
                .iter()
                .filter(|d| d.severity == validate::Severity::Error)
                .map(|d| {
                    format!(
                        "{} (rule {}, {:?}): {}",
                        d.rule_id.code(),
                        d.rule,
                        d.location,
                        d.message
                    )
                })
                .collect();
            eyre::bail!("pipeline validation failed:\n{}", errors.join("\n"));
        }

        // Find start node
        let start_node = validate::find_start_node(&graph)
            .ok_or_else(|| eyre::eyre!("no start node found in pipeline"))?;

        info!(start_node = %start_node, "pipeline executing");

        let pipeline_start = Instant::now();

        // coding-blue FA-7: reserve pipeline-level cost ledger budget
        // up front when a CostAccountant was threaded in. The handle is
        // held for the duration of execution — on success we commit
        // with the cumulative token attribution, on failure (bail!) the
        // handle is dropped and auto-refunds.
        let pipeline_reservation = self
            .reserve_pipeline_budget(&graph.id)
            .await
            .wrap_err("pipeline cost reservation failed")?;

        // Execute graph. The ready-set DAG scheduler replaces the single-path
        // walk ONLY for graphs it fully supports (no Parallel/DynamicParallel
        // fan-out, no `converge` — those need the legacy runtime orchestration)
        // and ONLY when opted in. Setup (model fill, pipeline reservation) and
        // teardown (terminal validators, reservation commit) are shared.
        // Explicit builder override wins over the env; otherwise read the env.
        // Checkpoint-backed runs stay on the legacy walk: the DAG path does not
        // build the resume skip-set or persist `node.checkpoints`, so a
        // checkpointed pipeline must not silently lose resume/persist parity.
        //
        // The pipeline-scoped LLM throttle is threaded into the legacy
        // `execute_graph` (which owns the planner / fan-out dispatch). The DAG
        // path has no planner/fan-out dispatch and reaches the LLM only through
        // its already-throttled codergen handler, so it needs no semaphore arg.
        let dag_selected = self.dag_override.unwrap_or_else(dag_scheduler_enabled);
        let use_dag = dag_selected
            && graph_is_dag_schedulable(&graph)
            && self.config.checkpoint_store.is_none();
        let mut result = if use_dag {
            info!(pipeline = %graph.id, "executing on ready-set DAG scheduler");
            self.execute_graph_dag(&graph, &handlers, &start_node, user_input, variables)
                .await
        } else {
            self.execute_graph(
                &graph,
                &handlers,
                &start_node,
                user_input,
                variables,
                llm_semaphore,
            )
            .await
        };

        // coding-blue FA-7: pipeline-terminal validators. The gate
        // runs only on a successful pipeline (failure results already
        // carry their own reason). On validator failure we rewrite the
        // PipelineResult with `success = false` and a reason-tagged
        // output so the caller sees a structured terminal error, then
        // drop the reservation (auto-refund) without committing.
        let mut validators_failed_reason: Option<String> = None;
        if let Ok(ref r) = result {
            if r.success {
                if let Err(reason) = self.run_terminal_validators(&graph.id).await {
                    warn!(
                        pipeline = %graph.id,
                        reason = %reason,
                        "pipeline-terminal validator rejected result"
                    );
                    validators_failed_reason = Some(reason);
                }
            }
        }
        if let Some(reason) = validators_failed_reason {
            if let Ok(ref mut r) = result {
                r.success = false;
                r.output = format!(
                    "Pipeline validator rejected completion: {reason}\n\n{}",
                    r.output
                );
            }
        }

        // ── Pipeline end: log summary ──
        let total_ms = pipeline_start.elapsed().as_millis() as u64;
        match &result {
            Ok(r) => {
                // Commit the pipeline-level reservation with the real
                // cumulative token attribution only when the pipeline
                // succeeded (including the terminal validator gate).
                // On a terminal validator rejection the reservation
                // is dropped unchanged at scope exit — ReservationHandle
                // Drop auto-refunds, preserving the ledger invariant.
                if r.success {
                    if let Some(handle) = pipeline_reservation.as_ref() {
                        self.commit_pipeline_reservation(handle, &graph.id, r).await;
                    }
                }
                let node_results: Vec<String> = r
                    .node_summaries
                    .iter()
                    .map(|n| {
                        format!(
                            "  {} ({}): {} {}ms {}+{} tokens",
                            n.node_id,
                            n.model.as_deref().unwrap_or("default"),
                            if n.success { "Pass" } else { "FAIL" },
                            n.duration_ms,
                            n.token_usage.input_tokens,
                            n.token_usage.output_tokens,
                        )
                    })
                    .collect();
                info!(
                    duration_ms = total_ms,
                    nodes = r.node_summaries.len(),
                    "pipeline complete\n{}",
                    node_results.join("\n")
                );
            }
            Err(e) => {
                // Drop pipeline reservation — ReservationHandle::Drop
                // auto-refunds when the handle is dropped uncommitted,
                // so we don't need to do anything beyond exiting scope.
                drop(pipeline_reservation);
                tracing::error!(
                    duration_ms = total_ms,
                    error = %e,
                    "pipeline failed"
                );
            }
        }

        result
    }

    /// Reserve the pipeline-level projection against the configured
    /// `CostAccountant`. Returns:
    /// * `Ok(None)` when no accountant is configured (legacy path).
    /// * `Ok(Some(handle))` on a successful reservation.
    /// * `Err` when the accountant exists but the reservation is
    ///   rejected by the budget policy — the pipeline aborts before
    ///   running any node, so per-node spend never starts.
    async fn reserve_pipeline_budget(&self, graph_id: &str) -> Result<Option<ReservationHandle>> {
        let Some(accountant) = self.config.workspace_context.cost_accountant.as_ref() else {
            return Ok(None);
        };
        let contract_id = self.pipeline_contract_id(graph_id);
        let projected_usd = self.pipeline_projected_usd();
        let handle = accountant
            .reserve(&contract_id, projected_usd)
            .await
            .map_err(|breach| eyre::eyre!("cost budget breach: {breach}"))?;
        info!(
            contract_id = %contract_id,
            projected_usd,
            "pipeline cost reservation opened"
        );
        Ok(Some(handle))
    }

    /// Commit the pipeline-level reservation with the cumulative token
    /// attribution. Errors are logged (not propagated) because the
    /// reservation auto-refunds on drop — double-counting a ledger row
    /// would be worse than a missed attribution.
    async fn commit_pipeline_reservation(
        &self,
        handle: &ReservationHandle,
        graph_id: &str,
        result: &PipelineResult,
    ) {
        let contract_id = self.pipeline_contract_id(graph_id);
        let usage = &result.token_usage;
        let node_cost_total: f64 = result
            .node_costs
            .iter()
            .map(|row| row.actual_usd)
            .filter(|cost| cost.is_finite() && *cost > 0.0)
            .sum();
        let actual_cost = if node_cost_total > 0.0 {
            node_cost_total
        } else {
            octos_agent::cost_ledger::project_cost_usd(
                "pipeline-aggregate",
                usage.input_tokens,
                usage.output_tokens,
            )
            .unwrap_or(0.0)
        };
        let event = CostAttributionEvent::new(
            contract_id.clone(),
            contract_id.clone(),
            format!("pipeline-{graph_id}"),
            "pipeline-aggregate",
            usage.input_tokens,
            usage.output_tokens,
            actual_cost,
        );
        if let Err(error) = handle.commit(event).await {
            tracing::warn!(
                contract_id = %contract_id,
                error = %error,
                "pipeline cost reservation commit failed; handle auto-refunds"
            );
        } else {
            info!(
                contract_id = %contract_id,
                tokens_in = usage.input_tokens,
                tokens_out = usage.output_tokens,
                cost_usd = actual_cost,
                "pipeline cost reservation committed"
            );
        }
    }

    /// Resolve the contract id used for cost-ledger rollups. Falls back
    /// to the pipeline graph id when the caller left the field empty
    /// so the ledger still attributes spend to a stable key.
    fn pipeline_contract_id(&self, graph_id: &str) -> String {
        let explicit = self.config.workspace_context.contract_id.trim();
        if !explicit.is_empty() {
            return explicit.to_string();
        }
        if !graph_id.is_empty() {
            return graph_id.to_string();
        }
        DEFAULT_PIPELINE_CONTRACT_ID.to_string()
    }

    /// Resolve the pipeline-level projected USD used for the opening
    /// reservation. Falls back to
    /// [`DEFAULT_PIPELINE_PROJECTED_USD`] when the caller leaves the
    /// field unset so the reservation path still surfaces breaches.
    fn pipeline_projected_usd(&self) -> f64 {
        let declared = self.config.workspace_context.pipeline_projected_usd;
        if declared > 0.0 {
            declared
        } else {
            DEFAULT_PIPELINE_PROJECTED_USD
        }
    }

    /// Run the declared completion-phase validators for the pipeline
    /// terminal gate. Returns `Ok(())` when either no workspace policy
    /// is installed OR every required validator passes. A required
    /// failure maps to `Err(reason)`; callers demote the pipeline
    /// result to `success=false` and refund the reservation.
    ///
    /// The `workspace_root` defaults to the executor's `working_dir`
    /// when the policy doesn't specify one — this mirrors the
    /// spawn/delegate/swarm pattern established by FA-2 (commits
    /// 40c307f6, fd7ed734, a7e041c6, f27eeb90).
    async fn run_terminal_validators(&self, _graph_id: &str) -> Result<(), String> {
        let ws_ctx = &self.config.workspace_context;
        let Some(policy) = ws_ctx.policy.as_ref() else {
            return Ok(());
        };
        if policy.validation.validators.is_empty() && policy.validation.on_completion.is_empty() {
            return Ok(());
        }

        // `on_completion` holds the legacy action-string checks
        // (e.g. `file_exists:output/deck.pptx`). Typed validators live
        // in `validation.validators`. Both need to pass at terminal.
        let legacy_failures = self.evaluate_on_completion_actions(&policy.validation.on_completion);
        if let Some(reason) = legacy_failures {
            return Err(reason);
        }

        if !policy.validation.validators.is_empty() {
            // Build a workspace-scoped ToolRegistry for the validator
            // runner — it only needs the workspace root for file
            // existence + the registered tools for tool_call
            // validators. Matches the spawn-agent-mcp pattern.
            //
            // #1607 (codex-review follow-up): carry the session sandbox so
            // `build_validator_runner` confines `ValidatorSpec::Command`
            // validators to it. `with_builtins` would store `NoSandbox`,
            // letting a workspace-declared command validator escape to the
            // host from a sandboxed pipeline.
            let registry = octos_agent::ToolRegistry::with_builtins_and_sandbox(
                &self.config.working_dir,
                octos_agent::create_sandbox(&self.config.sandbox),
            );
            run_declared_validators(
                &registry,
                &self.config.working_dir,
                &policy.validation.validators,
                "pipeline",
                ValidatorPhase::Completion,
                None,
            )
            .await?;
        }

        Ok(())
    }

    /// Evaluate legacy `on_completion: ["file_exists:..."]` action
    /// strings against the working directory. Returns `Some(reason)`
    /// when any required check fails.
    fn evaluate_on_completion_actions(&self, actions: &[String]) -> Option<String> {
        let mut failures = Vec::new();
        for action in actions {
            if let Some(spec) = action.strip_prefix("file_exists:") {
                // Support both concrete paths and globs via the
                // glob::glob API.
                let abs_pattern = if std::path::Path::new(spec).is_absolute() {
                    spec.to_string()
                } else {
                    self.config
                        .working_dir
                        .join(spec)
                        .to_string_lossy()
                        .to_string()
                };
                let any_match = match glob::glob(&abs_pattern) {
                    Ok(entries) => entries.filter_map(Result::ok).any(|p| p.exists()),
                    Err(_) => false,
                };
                if !any_match {
                    failures.push(action.clone());
                }
            } else {
                // Unknown action form — accept for forward-compat but
                // log a warning so operators notice legacy strings we
                // didn't port.
                warn!(
                    action = %action,
                    "on_completion action form not recognized by pipeline executor"
                );
            }
        }
        if failures.is_empty() {
            None
        } else {
            Some(format!(
                "pipeline completion validator failed: {}",
                failures.join(", ")
            ))
        }
    }

    /// Run per-node validators declared in
    /// [`PipelineContext::validators_by_node`] for `node_id`. Returns
    /// `Ok(())` when no override is installed for that node OR every
    /// required validator passes.
    async fn run_node_validators(&self, node_id: &str) -> Result<(), String> {
        let ws_ctx = &self.config.workspace_context;
        let Some(validators) = ws_ctx.validators_by_node.get(node_id) else {
            return Ok(());
        };
        if validators.is_empty() {
            return Ok(());
        }
        // Per-node validators target the completion phase — the node
        // has finished producing its artifact before we evaluate. A
        // separate turn-end phase isn't meaningful inside pipeline
        // execution.
        let scoped: Vec<WorkspaceValidator> = validators.to_vec();
        // #1607 (codex-review follow-up): carry the session sandbox so
        // per-node command validators run confined (see
        // `run_terminal_validators`).
        let registry = octos_agent::ToolRegistry::with_builtins_and_sandbox(
            &self.config.working_dir,
            octos_agent::create_sandbox(&self.config.sandbox),
        );
        run_declared_validators(
            &registry,
            &self.config.working_dir,
            &scoped,
            &format!("pipeline-node-{node_id}"),
            ValidatorPhase::Completion,
            None,
        )
        .await
        .map(|_| ())
    }

    /// M8 parity (W1.A3): register a child task in the parent
    /// session's [`TaskSupervisor`] so the admin dashboard sees the
    /// pipeline's substructure. The registration carries the node id
    /// as the synthetic tool name (`pipeline:<node_id>`) and the
    /// `parent_tool_call_id` from the host context as the
    /// `tool_call_id` so the UI can stitch the node tree under the
    /// invoking run_pipeline pill.
    ///
    /// NEW-18b — uses the supervisor's strict
    /// [`octos_agent::task_supervisor::TaskSupervisor::try_register_node_task`]
    /// entry point so a child registration against an already-terminal
    /// parent (e.g. orphan-swept on restart) is refused. Returns:
    /// * `Ok(None)` when no supervisor is wired (legacy callers).
    /// * `Ok(Some(task_id))` on a successful registration.
    /// * `Err(reason)` when the parent is terminal OR the cap fires.
    ///   The caller short-circuits the local node future so dropped
    ///   workers don't continue burning CPU/tokens against a parent
    ///   that no longer has a live failure-recovery path.
    fn register_node_task(&self, node_id: &str) -> Result<Option<String>, String> {
        let Some(supervisor) = self.config.host_context.task_supervisor.as_ref() else {
            return Ok(None);
        };
        let parent_tool_call_id = self
            .config
            .host_context
            .parent_tool_call_id
            .as_deref()
            .unwrap_or("");
        let session_key = self.config.host_context.parent_session_key.as_deref();
        let tool_name = format!("pipeline:{node_id}");
        match supervisor.try_register_node_task(&tool_name, parent_tool_call_id, session_key) {
            Ok(task_id) => {
                info!(
                    node = %node_id,
                    task_id = %task_id,
                    parent_tool_call_id = %parent_tool_call_id,
                    "registered pipeline node child task"
                );
                Ok(Some(task_id))
            }
            Err(err) => {
                warn!(
                    node = %node_id,
                    parent_tool_call_id = %parent_tool_call_id,
                    error = %err,
                    "pipeline node child registration refused; aborting node future"
                );
                Err(err.to_string())
            }
        }
    }

    /// Reserve sub-budget for a single LLM-call node. Returns:
    /// * `Ok(None)` when no accountant is configured OR the handler
    ///   kind does not participate in reservation.
    /// * `Ok(Some(handle))` on a successful per-node reservation. The
    ///   handle is held for the duration of the node's dispatch; on
    ///   failure we drop it (auto-refund), on success we also drop it
    ///   since the pipeline-level handle records the cumulative spend.
    /// * `Err` when the accountant exists but the reservation is
    ///   rejected — the caller should treat this as a terminal error.
    async fn reserve_node_budget(
        &self,
        graph_id: &str,
        node: &PipelineNode,
    ) -> Result<Option<ReservationHandle>> {
        let Some(accountant) = self.config.workspace_context.cost_accountant.as_ref() else {
            return Ok(None);
        };
        if !handler_kind_reserves(&node.handler) {
            return Ok(None);
        }
        let contract_id = self.pipeline_contract_id(graph_id);
        let projected_usd = project_node_usd(node.model.as_deref());
        let handle = accountant
            .reserve(&contract_id, projected_usd)
            .await
            .map_err(|breach| {
                eyre::eyre!("cost budget breach reserving node '{}': {breach}", node.id)
            })?;
        info!(
            contract_id = %contract_id,
            node = %node.id,
            projected_usd,
            "per-node cost reservation opened"
        );
        Ok(Some(handle))
    }

    /// Build a fresh [`CodergenHandler`] with the installed
    /// [`PipelineContext`] applied. Used by acceptance tests to
    /// confirm the per-handler wiring (compaction policy + workspace).
    #[doc(hidden)]
    pub fn build_codergen_for_test(&self) -> CodergenHandler {
        self.build_codergen(Some(self.pipeline_llm_semaphore()))
    }

    fn build_codergen(
        &self,
        llm_semaphore: Option<Arc<tokio::sync::Semaphore>>,
    ) -> CodergenHandler {
        let mut codergen = CodergenHandler::new(
            self.config.default_provider.clone(),
            self.config.memory.clone(),
            self.config.working_dir.clone(),
            self.config.shutdown.clone(),
        )
        .with_provider_policy(self.config.provider_policy.clone())
        .with_plugin_dirs(self.config.plugin_dirs.clone())
        .with_plugin_require_signed(self.config.plugin_require_signed)
        // M8 parity (W1.A1): propagate the host context so per-node
        // Agents inherit the parent session's FileStateCache /
        // SubAgentOutputRouter / AgentSummaryGenerator. Empty context
        // keeps pre-M8 behaviour bitwise identical.
        .with_host_context(self.config.host_context.clone());

        // NEW-06 fix: thread the parent embedder through to the
        // handler so every per-node worker Agent runs the
        // contamination-safe hybrid memory recall path. When unset
        // (legacy callers without an embedder configured) workers stay
        // on the cwd-only fallback — identical to pre-fix behaviour.
        if let Some(ref embedder) = self.config.embedder {
            codergen = codergen.with_embedder(embedder.clone());
        }

        if let Some(ref router) = self.config.provider_router {
            codergen = codergen.with_provider_router(router.clone());
        }
        if let Some(semaphore) = llm_semaphore {
            codergen = codergen.with_llm_semaphore(semaphore);
        }

        let ws_ctx = &self.config.workspace_context;
        if let Some(policy) = ws_ctx.policy.as_ref() {
            codergen = codergen.with_compaction_policy(policy.compaction.clone());
            codergen = codergen.with_compaction_workspace(Some(policy.clone()));
        }
        if let Some(provider) = ws_ctx.agent_llm_provider.as_ref() {
            codergen = codergen.with_compaction_llm_provider(Some(provider.clone()));
        }

        codergen
    }

    fn build_handlers(
        &self,
        llm_semaphore: Option<Arc<tokio::sync::Semaphore>>,
    ) -> HandlerRegistry {
        let mut registry = HandlerRegistry::new();

        // coding-blue FA-7: `build_codergen` reads the installed
        // PipelineContext and propagates compaction policy + workspace
        // onto every LLM-call node. When the context is empty (legacy
        // path) the setters are no-ops — behaviour is byte-for-byte
        // identical to pre-FA-7.
        let codergen = self.build_codergen(llm_semaphore);

        registry.register(HandlerKind::Codergen, Arc::new(codergen));
        // The `shell` handler is intentionally NOT registered: shell is
        // arbitrary code execution and is banned in pipelines (rule_23_no_shell
        // rejects any such graph before execution). Leaving it unregistered is
        // defense-in-depth — a `handler=shell` node has no handler to dispatch.
        registry.register(HandlerKind::Gate, Arc::new(GateHandler));
        registry.register(HandlerKind::Noop, Arc::new(NoopHandler));
        // DynamicParallel is handled directly in execute_graph, but needs a registry entry
        registry.register(HandlerKind::DynamicParallel, Arc::new(NoopHandler));

        registry
    }

    fn validation_context(
        &self,
        graph: &PipelineGraph,
        catalog_dir: &std::path::Path,
        variables: &serde_json::Map<String, serde_json::Value>,
    ) -> validate::ValidationContext {
        // codex pre-merge P2: include plugin tool names (when `plugin_dirs` is
        // set AND the graph references a non-built-in tool) so a graph
        // allow-listing a legitimate plugin tool isn't rejected by Rule 19.
        // Shared with `RunPipelineTool::pre_flight_validate` via
        // `known_tool_names_with_plugins` so the two validation paths can't
        // drift. Load failures are non-fatal (fall back to built-ins).
        let tool_names = validate::known_tool_names_with_plugins(
            &self.config.working_dir,
            &self.config.plugin_dirs,
            self.config.plugin_require_signed,
            &validate::referenced_tool_entries(graph),
        );
        validate::ValidationContext::default()
            .with_runtime_variables(variables.keys().cloned())
            .with_known_models(crate::model_assignment::known_model_keys_from_catalog_dir(
                catalog_dir,
            ))
            .with_known_tools(tool_names)
    }

    fn evaluate_before_node_guards(
        &self,
        graph: &PipelineGraph,
        node: &PipelineNode,
        total_tokens: &TokenUsage,
        pipeline_start: Instant,
        completed_count: usize,
        visit_counts: &HashMap<String, usize>,
    ) -> Result<GuardDecision> {
        if self.config.guards.is_empty() {
            return Ok(GuardDecision::Allow);
        }

        let ctx = GuardContext {
            graph,
            node,
            cumulative_tokens: total_pipeline_tokens(total_tokens),
            elapsed: pipeline_start.elapsed(),
            completed_count,
            visit_counts,
        };

        for guard in &self.config.guards {
            match guard.before_node(&ctx)? {
                GuardDecision::Allow => {}
                decision => return Ok(decision),
            }
        }

        Ok(GuardDecision::Allow)
    }

    fn guard_aborted_result(
        &self,
        node_id: &str,
        reason: impl std::fmt::Display,
        total_tokens: TokenUsage,
        summaries: Vec<NodeSummary>,
        completed: &HashMap<String, NodeOutcome>,
        node_costs: Vec<NodeCost>,
    ) -> PipelineResult {
        PipelineResult {
            output: format!("Pipeline aborted by guard before node '{node_id}': {reason}"),
            success: false,
            token_usage: total_tokens,
            node_summaries: summaries,
            files_modified: collect_completed_files(completed),
            node_costs,
        }
    }

    async fn execute_graph(
        &self,
        graph: &PipelineGraph,
        handlers: &HandlerRegistry,
        start_node: &str,
        user_input: &str,
        variables: &serde_json::Map<String, serde_json::Value>,
        llm_semaphore: Arc<tokio::sync::Semaphore>,
    ) -> Result<PipelineResult> {
        let pipeline_start = Instant::now();
        let mut current_node_id = start_node.to_string();
        let mut completed: HashMap<String, NodeOutcome> = HashMap::new();
        let mut summaries = Vec::new();
        let mut total_tokens = TokenUsage::default();
        // M8 parity (W1.A4): per-node cost attribution accumulated as
        // each LLM-call node finishes. Surfaced in `PipelineResult`.
        let mut node_costs: Vec<NodeCost> = Vec::new();
        // M8 parity (W1.A3): per-node task supervisor registrations.
        // Threaded so we can mark each node Completed/Failed at end.
        let mut node_task_ids: HashMap<String, String> = HashMap::new();
        // Nodes already executed by a parallel fan-out (skip in normal traversal)
        let mut parallel_executed: HashSet<String> = HashSet::new();
        // Per-node visit counts surfaced to before-node guards. The current
        // node is counted immediately before guard evaluation.
        let mut visit_counts: HashMap<String, usize> = HashMap::new();
        // Guard B: cumulative fan-out worker counter. Incremented exactly
        // once per dispatched worker across both `Parallel` and
        // `DynamicParallel` branches. Once the counter equals
        // [`MAX_PIPELINE_FANOUT_TOTAL`] the executor refuses any further
        // fan-out and fails the pipeline with `PipelineError::FanoutExceeded`.
        let mut fanout_workers_dispatched: usize = 0;
        // Nodes to skip because they (and everything before them) are
        // recorded in a persisted checkpoint. Synthesized outcomes for these
        // nodes propagate through the graph so downstream handlers still run.
        let resume_skip: HashSet<String> =
            build_resume_skip_set(self.config.checkpoint_store.as_ref())?;

        info!(
            pipeline = %graph.id,
            start = %current_node_id,
            nodes = graph.nodes.len(),
            "starting pipeline execution"
        );

        report_progress(&format!(
            "Pipeline '{}' started ({} nodes)",
            graph.id,
            graph.nodes.len()
        ));

        // Periodic heartbeat (issue #964 follow-up): a fresh `ToolProgress`
        // event every 5s with the current node + nodes-done counter +
        // elapsed seconds. Existing milestone-only emits leave 5+ min gaps
        // (analyze can run 9 min between events) — without the heartbeat
        // the chat bubble appears frozen for entire pipeline phases.
        let heartbeat_status = Arc::new(std::sync::Mutex::new(PipelineStatusSnapshot {
            pipeline_id: graph.id.clone(),
            current_node: current_node_id.clone(),
            nodes_done: 0,
            nodes_total: graph.nodes.len(),
            start: Instant::now(),
        }));
        let _heartbeat = spawn_pipeline_heartbeat(heartbeat_status.clone(), 5);

        loop {
            // Refresh the heartbeat snapshot at every iteration so the
            // periodic chip reflects the node currently executing. The
            // counter increments after each handler completes (see the
            // `parallel_executed` short-circuit + the post-handler block
            // further down where `completed.insert(...)` runs).
            if let Ok(mut g) = heartbeat_status.lock() {
                g.current_node = current_node_id.clone();
                g.nodes_done = completed.len();
            }

            let node = graph
                .nodes
                .get(&current_node_id)
                .ok_or_else(|| eyre::eyre!("node '{}' not found", current_node_id))?;

            // Skip nodes already executed by a parallel fan-out
            if parallel_executed.contains(&current_node_id) {
                // This node's output is already in `completed`; select next edge normally
                let outcome = completed.get(&current_node_id).unwrap().clone();
                match self.select_next_edge(graph, &current_node_id, &outcome)? {
                    Some(next) => {
                        current_node_id = next;
                        continue;
                    }
                    None => {
                        return Ok(PipelineResult {
                            output: outcome.content,
                            success: outcome.status == OutcomeStatus::Pass,
                            token_usage: total_tokens,
                            node_summaries: summaries,
                            files_modified: vec![],
                            node_costs: node_costs.clone(),
                        });
                    }
                }
            }

            // Skip nodes marked completed by a persisted checkpoint. We
            // synthesize a `Pass` outcome with empty content so downstream
            // edge selection and input construction still work, but no
            // handler runs.
            if resume_skip.contains(&current_node_id) {
                info!(
                    node = %current_node_id,
                    "skipping node (resume from checkpoint)"
                );
                let synth = NodeOutcome {
                    node_id: current_node_id.clone(),
                    status: OutcomeStatus::Pass,
                    content: String::new(),
                    token_usage: TokenUsage::default(),
                    files_modified: vec![],
                };
                summaries.push(NodeSummary {
                    node_id: current_node_id.clone(),
                    label: node.label.as_deref().unwrap_or(&node.id).to_string(),
                    model: node.model.clone(),
                    token_usage: TokenUsage::default(),
                    duration_ms: 0,
                    success: true,
                });
                completed.insert(current_node_id.clone(), synth.clone());
                match self.select_next_edge(graph, &current_node_id, &synth)? {
                    Some(next) => {
                        current_node_id = next;
                        continue;
                    }
                    None => {
                        return Ok(PipelineResult {
                            output: synth.content,
                            success: true,
                            token_usage: total_tokens,
                            node_summaries: summaries,
                            files_modified: vec![],
                            node_costs: node_costs.clone(),
                        });
                    }
                }
            }

            let visit_count = visit_counts.entry(current_node_id.clone()).or_insert(0);
            *visit_count = visit_count.saturating_add(1);

            match self.evaluate_before_node_guards(
                graph,
                node,
                &total_tokens,
                pipeline_start,
                completed.len(),
                &visit_counts,
            ) {
                Ok(GuardDecision::Allow) => {}
                Ok(GuardDecision::Skip(reason)) => {
                    info!(
                        node = %node.id,
                        reason = %reason,
                        "node skipped by pipeline guard"
                    );
                    let skipped = NodeOutcome {
                        node_id: node.id.clone(),
                        status: OutcomeStatus::Fail,
                        content: format!("Node '{}' skipped by pipeline guard: {reason}", node.id),
                        token_usage: TokenUsage::default(),
                        files_modified: vec![],
                    };
                    summaries.push(NodeSummary {
                        node_id: node.id.clone(),
                        label: node.label.as_deref().unwrap_or(&node.id).to_string(),
                        model: node.model.clone(),
                        token_usage: TokenUsage::default(),
                        duration_ms: 0,
                        success: false,
                    });
                    completed.insert(current_node_id.clone(), skipped.clone());
                    match self.select_next_edge(graph, &current_node_id, &skipped)? {
                        Some(next_id) => {
                            current_node_id = next_id;
                            continue;
                        }
                        None => {
                            return Ok(PipelineResult {
                                output: skipped.content,
                                success: false,
                                token_usage: total_tokens,
                                node_summaries: summaries,
                                files_modified: collect_completed_files(&completed),
                                node_costs: node_costs.clone(),
                            });
                        }
                    }
                }
                Ok(GuardDecision::Abort(reason)) => {
                    warn!(
                        node = %node.id,
                        reason = %reason,
                        "pipeline guard aborted execution"
                    );
                    return Ok(self.guard_aborted_result(
                        &node.id,
                        reason,
                        total_tokens,
                        summaries,
                        &completed,
                        node_costs.clone(),
                    ));
                }
                Err(error) => {
                    warn!(
                        node = %node.id,
                        error = %error,
                        "pipeline guard failed before node"
                    );
                    return Ok(self.guard_aborted_result(
                        &node.id,
                        format!("guard error: {error}"),
                        total_tokens,
                        summaries,
                        &completed,
                        node_costs.clone(),
                    ));
                }
            }

            // --- Parallel fan-out ---
            if node.handler == HandlerKind::Parallel {
                let converge_id = node.converge.as_ref().ok_or_else(|| {
                    eyre::eyre!("parallel node '{}' missing converge attribute", node.id)
                })?;

                let targets: Vec<String> = graph
                    .edges
                    .iter()
                    .filter(|e| e.source == current_node_id)
                    .map(|e| e.target.clone())
                    .collect();

                // Update status words to show parallel targets
                if let Some(ref bridge) = self.config.status_bridge {
                    let words: Vec<String> = targets
                        .iter()
                        .filter_map(|t| graph.nodes.get(t))
                        .map(|n| n.label.as_deref().unwrap_or(&n.id).to_string())
                        .collect();
                    bridge.set_words(words);
                }

                // Build the input text for parallel targets (same as normal)
                let predecessors: Vec<&str> = graph
                    .edges
                    .iter()
                    .filter(|e| e.target == current_node_id)
                    .map(|e| e.source.as_str())
                    .collect();
                let fan_input = if predecessors.is_empty() {
                    user_input.to_string()
                } else {
                    predecessors
                        .iter()
                        .filter_map(|p| completed.get(*p))
                        .map(|o| o.content.as_str())
                        .collect::<Vec<_>>()
                        .join("\n\n---\n\n")
                };

                info!(
                    node = %node.id,
                    targets = ?targets,
                    converge = %converge_id,
                    "parallel fan-out: spawning {} concurrent targets",
                    targets.len()
                );

                // Guard B: refuse the fan-out if dispatching every
                // target would push the pipeline past the cumulative
                // cap. Failing before any dispatch keeps recovery clean
                // (no half-spawned batch).
                let fanout_cap = self
                    .config
                    .max_pipeline_fanout_total
                    .unwrap_or(MAX_PIPELINE_FANOUT_TOTAL);
                if fanout_workers_dispatched.saturating_add(targets.len()) > fanout_cap {
                    let err = PipelineError::FanoutExceeded {
                        count: fanout_workers_dispatched,
                        cap: fanout_cap,
                    };
                    warn!(
                        node = %node.id,
                        count = fanout_workers_dispatched,
                        cap = fanout_cap,
                        targets = targets.len(),
                        "pipeline fan-out cap exceeded; refusing parallel dispatch"
                    );
                    return Err(eyre::eyre!(err));
                }

                let llm_target_count = targets
                    .iter()
                    .filter_map(|target_id| graph.nodes.get(target_id))
                    .filter(|target| matches!(target.handler, HandlerKind::Codergen))
                    .count()
                    .max(1);
                let parallel_remaining_tokens =
                    remaining_pipeline_tokens(graph.max_total_tokens, &total_tokens);
                if matches!(parallel_remaining_tokens, Some(0)) {
                    return Ok(PipelineResult {
                        output: format!(
                            "Pipeline token budget exhausted before parallel node '{}': spent {} tokens",
                            node.id,
                            total_pipeline_tokens(&total_tokens)
                        ),
                        success: false,
                        token_usage: total_tokens,
                        node_summaries: summaries,
                        files_modified: vec![],
                        node_costs: node_costs.clone(),
                    });
                }

                let fan_start = Instant::now();

                // Prepare and execute all targets concurrently, capped by semaphore
                let total_targets = targets.len();
                let par_completed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let semaphore = Arc::new(tokio::sync::Semaphore::new(
                    self.config.max_parallel_workers,
                ));
                let mut futures = Vec::new();
                // coding-blue FA-7: collect per-target reservations so
                // they drop together when the fan-out finishes. A
                // rejected reservation aborts the whole fan-out before
                // any worker dispatches, which keeps the concurrent
                // branches from racing past the budget.
                let mut fanout_reservations: Vec<ReservationHandle> = Vec::new();
                for (target_idx, target_id) in targets.iter().enumerate() {
                    let target_node = graph
                        .nodes
                        .get(target_id)
                        .ok_or_else(|| eyre::eyre!("parallel target '{}' not found", target_id))?;

                    let handler = handlers
                        .get(&target_node.handler)
                        .ok_or_else(|| eyre::eyre!("no handler for {:?}", target_node.handler))?;

                    // Apply template substitution and model defaults to target node
                    let mut target_with_prompt = target_node.clone();
                    if let Some(ref prompt) = target_with_prompt.prompt {
                        let mut resolved = prompt.replace("{input}", "");
                        for (k, v) in variables.iter() {
                            let placeholder = format!("{{{k}}}");
                            let value = v.as_str().unwrap_or("");
                            resolved = resolved.replace(&placeholder, value);
                        }
                        target_with_prompt.prompt = Some(resolved.trim_end().to_string());
                    }
                    if target_with_prompt.model.is_none() {
                        target_with_prompt.model = graph.default_model.clone();
                    }
                    if let Some(remaining_tokens) = parallel_remaining_tokens {
                        cap_node_output_tokens_for_remaining_budget(
                            &mut target_with_prompt,
                            remaining_tokens,
                            llm_target_count,
                        );
                    }

                    // Reserve budget for each LLM-call branch before
                    // dispatching. If any branch's reservation fails,
                    // bail — but first drop the handles collected so
                    // far so they auto-refund.
                    if let Some(handle) = self
                        .reserve_node_budget(&graph.id, &target_with_prompt)
                        .await?
                    {
                        fanout_reservations.push(handle);
                    }

                    // Parallel children inherit the fan-out node's
                    // predecessor outcomes (same source the fan_input
                    // string was concatenated from). Keeps GateHandler's
                    // `predecessor_outcomes` view consistent with `input`.
                    let par_predecessor_outcomes: Vec<NodeOutcome> = predecessors
                        .iter()
                        .filter_map(|p| completed.get(*p).cloned())
                        .collect();

                    let ctx = HandlerContext {
                        input: fan_input.clone(),
                        completed: completed.clone(),
                        predecessor_outcomes: par_predecessor_outcomes,
                        working_dir: self.config.working_dir.clone(),
                    };

                    let handler = handler.clone();
                    let max_retries = target_with_prompt.max_retries;
                    let tid = target_id.clone();
                    let par_label = target_with_prompt
                        .label
                        .clone()
                        .unwrap_or_else(|| tid.clone());
                    let par_done = par_completed.clone();
                    let par_node_label = node.label.as_deref().unwrap_or(&node.id).to_string();

                    // Gap 4.2 / Blocker 2 — N/M is the sub-node's 1-based
                    // position within the fan-out. The RAII NodeProgressGuard is
                    // armed INSIDE the future (after the permit is acquired) so a
                    // sub-node whose future is never polled (a LATER target's
                    // prep — lookup/handler/budget reservation — aborts the whole
                    // fan-out via `?` before `join_all`) NEVER emits
                    // `node_started`. Once armed, the guard's Drop pairs the
                    // started with a `node_completed{false}` even if the worker
                    // PANICS (the future unwinds; `join_all` surfaces the panic).
                    let par_node_index = target_idx + 1;

                    let sem = semaphore.clone();
                    let pipeline_id = graph.id.clone();
                    let guard_label = par_label.clone();
                    let guard_tid = tid.clone();
                    // Bound EVERY fan-out worker so a child whose future never
                    // resolves cannot wedge `join_all` (and with it the whole
                    // pipeline) forever, while honoring the worker's configured
                    // `deadline_action` (skip/retry/escalate) on a deadline
                    // expiry — exactly like the single-node `dispatch_node`
                    // path. `run_fanout_worker` keeps every timed attempt
                    // bounded by `MAX_FANOUT_WORKER_SECS`, so the deployed
                    // `deep_research` wedge cannot return.
                    let hook_executor = self.config.hook_executor.clone();
                    futures.push(async move {
                        let _permit = sem.acquire().await.expect("semaphore closed");
                        // Arm the guard HERE — only once the future is actually
                        // polled, guaranteeing a started/completed pair.
                        let guard = NodeProgressGuard::arm(
                            &pipeline_id,
                            &guard_tid,
                            &guard_label,
                            par_node_index,
                            total_targets,
                        );
                        let start = Instant::now();
                        let result = run_fanout_worker(
                            &handler,
                            &target_with_prompt,
                            &ctx,
                            max_retries,
                            hook_executor.as_ref(),
                        )
                        .await;
                        let n = par_done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                        let secs = start.elapsed().as_secs();
                        report_progress(&format!(
                            "{par_node_label}: '{par_label}' done ({n}/{total_targets}, {secs}s)"
                        ));
                        // Gap 4.2 / Blocker 2 — normal-completion path: emit the
                        // real `node_completed` (success + bounded preview) and
                        // disarm. The futures are `join_all`-polled on THIS task
                        // (not `tokio::spawn`ed), so the guard's captured sink is
                        // the same one; concurrent emits go through the shared
                        // append-only sink (one atomic whole line each), so
                        // interleaving can't corrupt a line.
                        let (success, preview) = match &result {
                            Ok(o) => (
                                o.status == OutcomeStatus::Pass,
                                node_output_preview(&o.content),
                            ),
                            Err(e) => (false, node_output_preview(&format!("Error: {e}"))),
                        };
                        guard.complete(success, &preview);
                        (tid, target_with_prompt, start.elapsed(), result)
                    });
                    // Guard B: count the worker as dispatched (the
                    // future is queued — `join_all` below awaits its
                    // completion) so subsequent fan-outs see the
                    // updated tally before they ask for headroom.
                    fanout_workers_dispatched = fanout_workers_dispatched.saturating_add(1);
                }

                let results = futures::future::join_all(futures).await;

                // Drop all per-branch reservations — the pipeline-level
                // handle commits with the cumulative attribution, so
                // per-branch handles only gated the dispatch-time
                // budget projection.
                drop(fanout_reservations);

                let WorkerResults {
                    merged_content,
                    any_error,
                    any_pass,
                    summaries: worker_summaries,
                    tokens: worker_tokens,
                    outcomes,
                } = process_worker_results(
                    results,
                    self.config.status_bridge.as_ref(),
                    &self.config.working_dir,
                );

                total_tokens.input_tokens += worker_tokens.input_tokens;
                total_tokens.output_tokens += worker_tokens.output_tokens;
                summaries.extend(worker_summaries);
                for (id, outcome) in outcomes {
                    parallel_executed.insert(id.clone());
                    completed.insert(id, outcome);
                }

                let fan_duration = fan_start.elapsed().as_millis() as u64;

                // When EVERY worker hard-ERRORED (≥1 error, no pass), the
                // fan-out produced nothing usable — surface a hard `Error` so
                // the pipeline terminates instead of silently converging on
                // empty content. This is the `search (0/3 nodes)` total-wedge
                // case from production. A partial failure (≥1 pass) stays
                // `Fail` so the fault-tolerant "synthesize from what worked"
                // path is kept. An all-`Skipped` fan-out (deadline_action=skip
                // on every branch) is NOT a hard failure — it carries no error,
                // so it stays `Fail` and convergence proceeds rather than
                // aborting; the configured `skip` action must continue.
                let fan_status = if any_error && !any_pass {
                    OutcomeStatus::Error
                } else if any_pass && !any_error {
                    OutcomeStatus::Pass
                } else {
                    OutcomeStatus::Fail
                };

                info!(
                    node = %node.id,
                    duration_ms = fan_duration,
                    targets = targets.len(),
                    errors = any_error,
                    any_pass,
                    status = ?fan_status,
                    "parallel fan-out complete, converging to '{}'",
                    converge_id
                );

                // Record the parallel node itself as a pass-through summary
                summaries.push(NodeSummary {
                    node_id: node.id.clone(),
                    label: node.label.as_deref().unwrap_or(&node.id).to_string(),
                    model: None,
                    token_usage: TokenUsage::default(),
                    duration_ms: fan_duration,
                    success: fan_status == OutcomeStatus::Pass,
                });
                completed.insert(
                    current_node_id.clone(),
                    NodeOutcome {
                        node_id: node.id.clone(),
                        status: fan_status,
                        content: merged_content,
                        token_usage: TokenUsage::default(),
                        files_modified: vec![],
                    },
                );

                // All workers failed → terminate the pipeline now rather than
                // feeding empty merged content into the converge node. Mirrors
                // the single-node `OutcomeStatus::Error` stop path below —
                // including its `continue_on_error` escape hatch: when the
                // fan-out node opts in, the pipeline intentionally tolerates a
                // fully-failed fan-out and lets the normal convergence/error
                // routing handle the empty merged content instead of aborting.
                if fan_status == OutcomeStatus::Error && !node.continue_on_error {
                    warn!(
                        node = %node.id,
                        "parallel fan-out: every worker failed, stopping pipeline"
                    );
                    return Ok(PipelineResult {
                        output: format!(
                            "Pipeline failed at fan-out node '{}': all {} workers failed",
                            node.id,
                            targets.len()
                        ),
                        success: false,
                        token_usage: total_tokens,
                        node_summaries: summaries,
                        files_modified: vec![],
                        node_costs: node_costs.clone(),
                    });
                }

                // Update status words to show convergence node
                if let Some(ref bridge) = self.config.status_bridge {
                    if let Some(conv_node) = graph.nodes.get(converge_id) {
                        let label = conv_node.label.as_deref().unwrap_or(converge_id);
                        bridge.set_words(vec![label.to_string()]);
                    }
                }

                // Jump to convergence node — feed merged output as its input
                // We stash the merged content so the convergence node can pick it up
                // from the parallel node's completed entry.
                current_node_id = converge_id.clone();
                continue;
            }

            // --- Dynamic parallel fan-out ---
            if node.handler == HandlerKind::DynamicParallel {
                let converge_id = node.converge.as_ref().ok_or_else(|| {
                    eyre::eyre!(
                        "dynamic_parallel node '{}' missing converge attribute",
                        node.id
                    )
                })?;

                // Build the input text (same as normal nodes)
                let predecessors: Vec<&str> = graph
                    .edges
                    .iter()
                    .filter(|e| e.target == current_node_id)
                    .map(|e| e.source.as_str())
                    .collect();
                let dp_input = if predecessors.is_empty() {
                    user_input.to_string()
                } else {
                    predecessors
                        .iter()
                        .filter_map(|p| completed.get(*p))
                        .map(|o| o.content.as_str())
                        .collect::<Vec<_>>()
                        .join("\n\n---\n\n")
                };

                // Update status for planning phase
                if let Some(ref bridge) = self.config.status_bridge {
                    let label = node.label.as_deref().unwrap_or(&node.id);
                    bridge.set_words(vec![format!("{label} (planning)")]);
                }

                let max_tasks = node.max_tasks.unwrap_or(8);

                // Resolve planner LLM provider
                let planner_provider = resolve_provider(
                    &self.config.default_provider,
                    self.config.provider_router.as_ref(),
                    node.planner_model
                        .as_deref()
                        .or(node.model.as_deref())
                        .or(graph.default_model.as_deref()),
                )?;
                let planner_provider: Arc<dyn LlmProvider> = Arc::new(
                    SemaphoreThrottledProvider::new(planner_provider, llm_semaphore.clone()),
                );

                // Default planning prompt, WITH runtime-variable substitution:
                // the dynamic_parallel planner reads `node.prompt` directly
                // (before the sequential node_with_prompt path), so it must get
                // the same `{var}` substitution sequential nodes + worker
                // prompts get — else an authored `plan_prompt` like
                // `Plan for {topic}` reaches the planner as a literal placeholder.
                let mut planning_prompt = node
                    .prompt
                    .as_deref()
                    .unwrap_or(
                        "Generate 4-6 research search angles for this query. \
                         Each angle should cover a different aspect.\n\
                         Respond with ONLY a JSON array of objects with \"task\" and \"label\" fields.",
                    )
                    .to_string();
                for (k, v) in variables {
                    planning_prompt =
                        planning_prompt.replace(&format!("{{{k}}}"), v.as_str().unwrap_or(""));
                }

                let dp_label = node.label.as_deref().unwrap_or(&node.id);
                report_progress(&format!("{dp_label}: planning sub-tasks..."));

                info!(
                    node = %node.id,
                    planner_model = %planner_provider.model_id(),
                    max_tasks,
                    "dynamic_parallel: planning sub-tasks"
                );

                let fan_start = Instant::now();

                // Plan tasks via LLM (with fallback)
                let (tasks, plan_usage) = match plan_dynamic_tasks(
                    planner_provider.as_ref(),
                    &planning_prompt,
                    &dp_input,
                    max_tasks,
                )
                .await
                {
                    Ok((tasks, usage)) if tasks.len() >= 2 => {
                        info!(
                            task_count = tasks.len(),
                            "dynamic planning produced {} tasks",
                            tasks.len()
                        );
                        (tasks, usage)
                    }
                    Ok((tasks, usage)) => {
                        warn!(
                            task_count = tasks.len(),
                            "planner returned too few tasks, using fallback"
                        );
                        (fallback_tasks(&dp_input), usage)
                    }
                    Err(e) => {
                        warn!(error = %e, "dynamic planner failed, using fallback tasks");
                        (fallback_tasks(&dp_input), TokenUsage::default())
                    }
                };

                total_tokens.input_tokens += plan_usage.input_tokens;
                total_tokens.output_tokens += plan_usage.output_tokens;
                if let Some(ref bridge) = self.config.status_bridge {
                    bridge.add_tokens(&plan_usage);
                }
                let dynamic_remaining_tokens =
                    remaining_pipeline_tokens(graph.max_total_tokens, &total_tokens);
                if matches!(dynamic_remaining_tokens, Some(0)) {
                    return Ok(PipelineResult {
                        output: format!(
                            "Pipeline token budget exhausted after dynamic_parallel planner '{}': spent {} tokens",
                            node.id,
                            total_pipeline_tokens(&total_tokens)
                        ),
                        success: false,
                        token_usage: total_tokens,
                        node_summaries: summaries,
                        files_modified: vec![],
                        node_costs: node_costs.clone(),
                    });
                }

                // Build synthetic PipelineNodes for each dynamic task
                let worker_prompt_template = node.worker_prompt.as_deref().unwrap_or(
                    "You are a research specialist.\n\n{task}\n\nUse the available tools to find relevant information. Include ALL URLs and source references.",
                );

                // Resolve worker model pool. If model contains commas,
                // it's a pool of models for round-robin distribution across workers.
                let model_str = node
                    .model
                    .as_deref()
                    .or(graph.default_model.as_deref())
                    .unwrap_or("");
                let model_pool: Vec<&str> = if model_str.contains(',') {
                    model_str
                        .split(',')
                        .map(|s| s.trim())
                        .filter(|s| !s.is_empty())
                        .collect()
                } else {
                    vec![model_str]
                };
                if model_pool.len() > 1 {
                    info!(
                        node = %node.id,
                        pool_size = model_pool.len(),
                        models = model_str,
                        "worker model pool: distributing {} workers across {} models",
                        tasks.len(),
                        model_pool.len(),
                    );
                }

                let mut synthetic_nodes: Vec<(String, PipelineNode)> = Vec::new();
                for (i, task) in tasks.iter().enumerate() {
                    let task_id = format!("{}_task_{i}", node.id);
                    let prompt = worker_prompt_template.replace("{task}", &task.task);
                    let label = task
                        .label
                        .clone()
                        .unwrap_or_else(|| format!("Task {}", i + 1));

                    // Round-robin model from pool
                    let worker_model = Some(model_pool[i % model_pool.len()].to_string());

                    synthetic_nodes.push((
                        task_id.clone(),
                        PipelineNode {
                            id: task_id,
                            handler: HandlerKind::Codergen,
                            prompt: Some(prompt),
                            label: Some(label),
                            model: worker_model.clone(),
                            tools: node.tools.clone(),
                            timeout_secs: node.timeout_secs,
                            max_retries: node.max_retries,
                            // Inherit the source node's deadline settings so
                            // `run_fanout_worker` honors deadline_action/
                            // deadline_secs on dynamic_parallel workers too
                            // (not just the static fan-out path). Without this
                            // a dynamic worker defaults to the 1h ceiling/Abort
                            // and ignores a configured skip/retry. (codex #1427)
                            deadline_secs: node.deadline_secs,
                            deadline_action: node.deadline_action,
                            ..Default::default()
                        },
                    ));
                }

                // Update status words to show parallel worker labels
                if let Some(ref bridge) = self.config.status_bridge {
                    let words: Vec<String> = synthetic_nodes
                        .iter()
                        .map(|(_, n)| n.label.as_deref().unwrap_or(&n.id).to_string())
                        .collect();
                    bridge.set_words(words);
                }

                let worker_labels: Vec<String> = synthetic_nodes
                    .iter()
                    .map(|(_, n)| n.label.as_deref().unwrap_or(&n.id).to_string())
                    .collect();
                report_progress(&format!(
                    "{dp_label}: {} workers running ({})",
                    synthetic_nodes.len(),
                    worker_labels.join(", ")
                ));

                info!(
                    node = %node.id,
                    tasks = synthetic_nodes.len(),
                    converge = %converge_id,
                    "dynamic_parallel: spawning {} concurrent workers",
                    synthetic_nodes.len()
                );

                // Guard B: refuse before dispatching any synthetic
                // worker if the pipeline-lifetime fan-out cap would be
                // exceeded. Mirrors the static Parallel gate so the
                // 65,535-child river runaway cannot survive even a
                // re-firing dynamic_parallel node.
                let fanout_cap = self
                    .config
                    .max_pipeline_fanout_total
                    .unwrap_or(MAX_PIPELINE_FANOUT_TOTAL);
                if fanout_workers_dispatched.saturating_add(synthetic_nodes.len()) > fanout_cap {
                    let err = PipelineError::FanoutExceeded {
                        count: fanout_workers_dispatched,
                        cap: fanout_cap,
                    };
                    warn!(
                        node = %node.id,
                        count = fanout_workers_dispatched,
                        cap = fanout_cap,
                        targets = synthetic_nodes.len(),
                        "pipeline fan-out cap exceeded; refusing dynamic_parallel dispatch"
                    );
                    return Err(eyre::eyre!(err));
                }

                // Get the codergen handler for executing synthetic nodes
                let codergen_handler = handlers.get(&HandlerKind::Codergen).ok_or_else(|| {
                    eyre::eyre!("codergen handler not found for dynamic_parallel workers")
                })?;

                // Execute all synthetic nodes concurrently, capped by the
                // same `max_parallel_workers` semaphore the static `Parallel`
                // branch uses. The planner may yield more tasks than the
                // limit; without this gate every synthetic worker would
                // dispatch at once (the planner's `llm_semaphore` bounds
                // concurrent LLM calls, not in-flight workers).
                let total_workers = synthetic_nodes.len();
                let completed_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let semaphore = Arc::new(tokio::sync::Semaphore::new(
                    self.config.max_parallel_workers,
                ));
                let mut futures = Vec::new();
                // coding-blue FA-7: same fan-out reservation pattern as
                // the static Parallel branch — reserve per-worker up
                // front, release en bloc when the fan-out completes.
                let mut dp_reservations: Vec<ReservationHandle> = Vec::new();
                for (worker_idx, (task_id, mut synth_node)) in
                    synthetic_nodes.into_iter().enumerate()
                {
                    // Apply variable substitution to synthetic prompt
                    if let Some(prompt) = synth_node.prompt.take() {
                        let mut resolved = prompt.replace("{input}", "");
                        for (k, v) in variables.iter() {
                            let placeholder = format!("{{{k}}}");
                            let value = v.as_str().unwrap_or("");
                            resolved = resolved.replace(&placeholder, value);
                        }
                        synth_node.prompt = Some(resolved.trim_end().to_string());
                    }
                    if let Some(remaining_tokens) = dynamic_remaining_tokens {
                        cap_node_output_tokens_for_remaining_budget(
                            &mut synth_node,
                            remaining_tokens,
                            total_workers,
                        );
                    }

                    if let Some(handle) = self.reserve_node_budget(&graph.id, &synth_node).await? {
                        dp_reservations.push(handle);
                    }

                    // Same logic as the static-fan-out site: dynamic
                    // workers inherit the dynamic_parallel node's
                    // predecessors so GateHandler sees the same
                    // upstream outcomes that built `dp_input`.
                    let dp_predecessor_outcomes: Vec<NodeOutcome> = predecessors
                        .iter()
                        .filter_map(|p| completed.get(*p).cloned())
                        .collect();

                    let ctx = HandlerContext {
                        input: dp_input.clone(),
                        completed: completed.clone(),
                        predecessor_outcomes: dp_predecessor_outcomes,
                        working_dir: self.config.working_dir.clone(),
                    };

                    let handler = codergen_handler.clone();
                    let max_retries = synth_node.max_retries;
                    let worker_label = synth_node.label.clone().unwrap_or_else(|| task_id.clone());
                    let dp_label = dp_label.to_owned();
                    let done_count = completed_count.clone();

                    // Gap 4.2 / Blocker 2 — `deep_research` IS dynamic_parallel.
                    // N/M is the worker's 1-based position within the
                    // dynamically-expanded total. The RAII NodeProgressGuard is
                    // armed INSIDE the future so a worker whose future is never
                    // polled (a LATER worker's budget reservation aborts the
                    // whole fan-out via `?` before `join_all`) NEVER emits
                    // `node_started`. Once armed, the guard's Drop pairs the
                    // started with a `node_completed{false}` even if the worker
                    // PANICS (the future unwinds; `join_all` surfaces the panic).
                    let dp_node_index = worker_idx + 1;

                    let sem = semaphore.clone();
                    let pipeline_id = graph.id.clone();
                    let guard_label = worker_label.clone();
                    let guard_tid = task_id.clone();
                    // Bound EVERY dynamic_parallel worker (this is the
                    // `deep_research` fan-out path that wedged in production) so
                    // a child whose future never resolves cannot block
                    // `join_all` forever, while honoring the worker's configured
                    // `deadline_action`. `run_fanout_worker` keeps every timed
                    // attempt bounded by `MAX_FANOUT_WORKER_SECS`.
                    let hook_executor = self.config.hook_executor.clone();
                    futures.push(async move {
                        let _permit = sem.acquire().await.expect("semaphore closed");
                        // Arm the guard HERE — only once the future is actually
                        // polled AND holds a worker permit, guaranteeing a
                        // started/completed pair for a worker that actually ran.
                        let guard = NodeProgressGuard::arm(
                            &pipeline_id,
                            &guard_tid,
                            &guard_label,
                            dp_node_index,
                            total_workers,
                        );
                        let start = Instant::now();
                        let result = run_fanout_worker(
                            &handler,
                            &synth_node,
                            &ctx,
                            max_retries,
                            hook_executor.as_ref(),
                        )
                        .await;
                        let n = done_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                        let secs = start.elapsed().as_secs();
                        report_progress(&format!(
                            "{dp_label}: '{worker_label}' done ({n}/{total_workers}, {secs}s)"
                        ));
                        // Gap 4.2 / Blocker 2 — normal-completion path: emit the
                        // real `node_completed` (success + bounded preview) and
                        // disarm. Polled on THIS task via `join_all` (not
                        // `tokio::spawn`), so the guard's captured sink is the
                        // same one; the shared append-only sink serializes whole
                        // lines so concurrent emits can't interleave-corrupt.
                        let (success, preview) = match &result {
                            Ok(o) => (
                                o.status == OutcomeStatus::Pass,
                                node_output_preview(&o.content),
                            ),
                            Err(e) => (false, node_output_preview(&format!("Error: {e}"))),
                        };
                        guard.complete(success, &preview);
                        (task_id, synth_node, start.elapsed(), result)
                    });
                    // Guard B: count this worker as dispatched (the
                    // future is queued — `join_all` below awaits it).
                    fanout_workers_dispatched = fanout_workers_dispatched.saturating_add(1);
                }

                let results = futures::future::join_all(futures).await;
                drop(dp_reservations);

                let WorkerResults {
                    merged_content,
                    any_error,
                    any_pass,
                    summaries: worker_summaries,
                    tokens: worker_tokens,
                    outcomes,
                } = process_worker_results(
                    results,
                    self.config.status_bridge.as_ref(),
                    &self.config.working_dir,
                );

                total_tokens.input_tokens += worker_tokens.input_tokens;
                total_tokens.output_tokens += worker_tokens.output_tokens;
                summaries.extend(worker_summaries);
                for (id, outcome) in outcomes {
                    completed.insert(id, outcome);
                }

                let fan_duration = fan_start.elapsed().as_millis() as u64;

                report_progress(&format!(
                    "{dp_label}: done ({} workers, {:.0}s)",
                    tasks.len(),
                    fan_duration as f64 / 1000.0
                ));

                // Same all-failed rule as the static `parallel` branch: when
                // EVERY worker hard-ERRORED (≥1 error, no pass — the production
                // `search (0/3 nodes)` total-wedge case) the dynamic fan-out
                // produced nothing usable, so surface a hard `Error` that
                // terminates the pipeline rather than converging on empty
                // content. A partial failure stays `Fail` (fault-tolerant
                // synthesize-from-rest). An all-`Skipped` fan-out carries no
                // error, so it stays `Fail` and convergence proceeds — the
                // configured `deadline_action=skip` must continue.
                let fan_status = if any_error && !any_pass {
                    OutcomeStatus::Error
                } else if any_pass && !any_error {
                    OutcomeStatus::Pass
                } else {
                    OutcomeStatus::Fail
                };

                info!(
                    node = %node.id,
                    duration_ms = fan_duration,
                    tasks = tasks.len(),
                    errors = any_error,
                    any_pass,
                    status = ?fan_status,
                    "dynamic_parallel complete, converging to '{}'",
                    converge_id
                );

                // Record the dynamic_parallel node itself
                summaries.push(NodeSummary {
                    node_id: node.id.clone(),
                    label: node.label.as_deref().unwrap_or(&node.id).to_string(),
                    model: None,
                    token_usage: plan_usage.clone(),
                    duration_ms: fan_duration,
                    success: fan_status == OutcomeStatus::Pass,
                });
                completed.insert(
                    current_node_id.clone(),
                    NodeOutcome {
                        node_id: node.id.clone(),
                        status: fan_status,
                        content: merged_content,
                        token_usage: plan_usage,
                        files_modified: vec![],
                    },
                );

                // All workers failed → terminate the pipeline instead of
                // feeding empty merged content into the converge node. Honors
                // the same `continue_on_error` escape hatch as the static
                // `parallel` branch: an opted-in node tolerates a fully-failed
                // fan-out and lets normal convergence/error routing proceed.
                if fan_status == OutcomeStatus::Error && !node.continue_on_error {
                    warn!(
                        node = %node.id,
                        "dynamic_parallel: every worker failed, stopping pipeline"
                    );
                    return Ok(PipelineResult {
                        output: format!(
                            "Pipeline failed at dynamic_parallel node '{}': all {} workers failed",
                            node.id,
                            tasks.len()
                        ),
                        success: false,
                        token_usage: total_tokens,
                        node_summaries: summaries,
                        files_modified: vec![],
                        node_costs: node_costs.clone(),
                    });
                }

                // Update status words to show convergence node
                if let Some(ref bridge) = self.config.status_bridge {
                    if let Some(conv_node) = graph.nodes.get(converge_id) {
                        let label = conv_node.label.as_deref().unwrap_or(converge_id);
                        bridge.set_words(vec![label.to_string()]);
                    }
                }

                // Jump to convergence node
                current_node_id = converge_id.clone();
                continue;
            }

            // --- Normal sequential execution ---

            let handler = handlers
                .get(&node.handler)
                .ok_or_else(|| eyre::eyre!("no handler for {:?}", node.handler))?;

            // Build input for this node: predecessor outputs or user_input
            let predecessors: Vec<&str> = graph
                .edges
                .iter()
                .filter(|e| e.target == current_node_id)
                .map(|e| e.source.as_str())
                .collect();

            let input_text = if predecessors.is_empty() {
                user_input.to_string()
            } else {
                predecessors
                    .iter()
                    .filter_map(|p| completed.get(*p))
                    .map(|o| o.content.as_str())
                    .collect::<Vec<_>>()
                    .join("\n\n---\n\n")
            };

            // Template substitution in prompt — only substitute variables,
            // NOT {input}. The input is passed separately as the task instruction
            // so the system prompt defines the role, not a one-shot instruction.
            let mut node_with_prompt = node.clone();
            if let Some(ref prompt) = node_with_prompt.prompt {
                let mut resolved = prompt.replace("{input}", "");
                for (k, v) in variables {
                    let placeholder = format!("{{{k}}}");
                    let value = v.as_str().unwrap_or("");
                    resolved = resolved.replace(&placeholder, value);
                }
                // Trim trailing whitespace left by removing {input}
                let resolved = resolved.trim_end().to_string();
                node_with_prompt.prompt = Some(resolved);
            }

            // Resolve model from graph default if node doesn't specify one
            if node_with_prompt.model.is_none() {
                node_with_prompt.model = graph.default_model.clone();
            }

            if let Some(remaining_tokens) =
                remaining_pipeline_tokens(graph.max_total_tokens, &total_tokens)
            {
                if remaining_tokens == 0 {
                    return Ok(PipelineResult {
                        output: format!(
                            "Pipeline token budget exhausted before node '{}': spent {} tokens",
                            node.id,
                            total_pipeline_tokens(&total_tokens)
                        ),
                        success: false,
                        token_usage: total_tokens,
                        node_summaries: summaries,
                        files_modified: vec![],
                        node_costs: node_costs.clone(),
                    });
                }
                cap_node_output_tokens_for_remaining_budget(
                    &mut node_with_prompt,
                    remaining_tokens,
                    1,
                );
            }

            let input_bytes = input_text.len();

            let seq_label = node.label.as_deref().unwrap_or(&node.id).to_string();
            report_progress(&format!("{seq_label}: running..."));

            // Gap 4.2 / Blocker 1 — RAII NodeProgressGuard pairs the
            // `node_started` emit with a `node_completed` on EVERY exit path of
            // this loop body: the normal-completion `guard.complete(...)` below,
            // plus the guard's Drop for any early `?`-return (reserve_node_budget,
            // dispatch, select_next_edge), early `return` (skipped/goal/error/
            // budget/no-edge), a panic in the handler, or a cancellation that
            // drops this run future mid-node. The guard captures the sink at arm
            // time so its Drop emit survives an unwound TOOL_CTX task-local. The
            // 1-based index is `completed.len() + 1` (nodes finished so far + this).
            let node_total = graph.nodes.len();
            let node_index = completed.len() + 1;
            let node_progress_guard =
                NodeProgressGuard::arm(&graph.id, &node.id, &seq_label, node_index, node_total);

            info!(
                node = %node.id,
                handler = ?node.handler,
                model = ?node_with_prompt.model,
                input_bytes,
                tools = ?node.tools,
                "executing pipeline node"
            );

            // Update status words for this sequential node
            if let Some(ref bridge) = self.config.status_bridge {
                bridge.set_words(vec![seq_label.to_string()]);
            }

            // Direct-predecessor outcomes in graph edge order — preserves
            // each predecessor's `OutcomeStatus` (Pass/Fail/Error) for
            // GateHandler. `Vec` ordering matches edge iteration so the
            // single-predecessor branching case is fully deterministic
            // (codex round-5 + round-6).
            let predecessor_outcomes: Vec<NodeOutcome> = predecessors
                .iter()
                .filter_map(|p| completed.get(*p).cloned())
                .collect();

            let ctx = HandlerContext {
                input: input_text,
                completed: completed.clone(),
                predecessor_outcomes,
                working_dir: self.config.working_dir.clone(),
            };

            let node_start = Instant::now();

            // M8 parity (W1.A3): register a child task in the parent
            // session's TaskSupervisor so the admin dashboard sees the
            // pipeline's substructure under the run_pipeline parent
            // tool_call_id. The supervisor's progress reporter (set by
            // the session actor) bridges every state transition onto
            // the SSE stream so the chat UI's NodeCard can render the
            // node tree live.
            //
            // NEW-18b — refuse to register when the parent task is
            // already terminal (e.g. orphan-swept on serve restart).
            // Bail out of the executor loop instead of letting the
            // straggler worker burn CPU/tokens producing output that
            // will never be reaped by the dead parent.
            let node_task_id = match self.register_node_task(&node.id) {
                Ok(opt) => opt,
                Err(reason) => {
                    warn!(
                        node = %node.id,
                        reason = %reason,
                        "pipeline executor aborting: parent task is terminal — registration refused"
                    );
                    return Ok(PipelineResult {
                        output: format!("Pipeline aborted before node '{}': {reason}", node.id),
                        success: false,
                        token_usage: total_tokens,
                        node_summaries: summaries,
                        files_modified: vec![],
                        node_costs: node_costs.clone(),
                    });
                }
            };
            if let Some(ref id) = node_task_id {
                node_task_ids.insert(node.id.clone(), id.clone());
                if let Some(ref supervisor) = self.config.host_context.task_supervisor {
                    supervisor.mark_running(id);
                }
            }

            // coding-blue FA-7: reserve per-node budget before dispatch
            // on LLM-call nodes. A rejected reservation aborts the
            // pipeline before the sub-agent is built; on dispatch
            // failure the handle drops (Drop auto-refunds). Conditional
            // branches that never reach this line never reserve, which
            // is the design invariant for "unreached branches don't
            // count against the pipeline budget".
            let node_reservation = self
                .reserve_node_budget(&graph.id, &node_with_prompt)
                .await?;
            let node_reserved_usd = node_reservation
                .as_ref()
                .map(|h| h.reserved_amount_usd())
                .unwrap_or(0.0);

            // Execute with retries — and enforce the node's deadline when set.
            let dispatch = self
                .dispatch_node(handler, &node_with_prompt, &ctx, node.max_retries)
                .await;

            let mut outcome = match dispatch? {
                DispatchOutcome::Completed(outcome) => outcome,
                DispatchOutcome::Skipped { label } => {
                    let duration_ms = node_start.elapsed().as_millis() as u64;
                    info!(
                        node = %node.id,
                        duration_ms,
                        "node skipped due to deadline_action=skip"
                    );
                    summaries.push(NodeSummary {
                        node_id: node.id.clone(),
                        label: label.clone(),
                        model: node_with_prompt.model.clone(),
                        token_usage: TokenUsage::default(),
                        duration_ms,
                        success: false,
                    });
                    let skipped = NodeOutcome {
                        node_id: node.id.clone(),
                        status: OutcomeStatus::Skipped,
                        content: format!("Node '{}' skipped (deadline exceeded)", node.id),
                        token_usage: TokenUsage::default(),
                        files_modified: vec![],
                    };
                    completed.insert(current_node_id.clone(), skipped.clone());
                    match self.select_next_edge(graph, &current_node_id, &skipped)? {
                        Some(next_id) => {
                            current_node_id = next_id;
                            continue;
                        }
                        None => {
                            let mut all_files: Vec<std::path::PathBuf> = Vec::new();
                            for o in completed.values() {
                                all_files.extend(o.files_modified.iter().cloned());
                            }
                            all_files.sort();
                            all_files.dedup();
                            return Ok(PipelineResult {
                                output: skipped.content,
                                success: false,
                                token_usage: total_tokens,
                                node_summaries: summaries,
                                files_modified: all_files,
                                node_costs: node_costs.clone(),
                            });
                        }
                    }
                }
            };

            // M8 parity (W1.A2): on a retryable first-attempt failure,
            // engage the M8.9 recovery loop to re-attempt ONCE with a
            // synthesised recovery prompt. Mirrors the spawn_only
            // recovery flow already wired in session_actor. Skipped /
            // Pass outcomes short-circuit; the second failure is
            // terminal.
            let recovery_input = serde_json::json!({
                "node": node.id,
                "input": ctx.input,
            });
            let recovery_decision =
                crate::recovery::classify_outcome(&node_with_prompt, &outcome, &recovery_input);
            if let crate::recovery::RecoveryDecision::Retryable(signal) = recovery_decision {
                if let Some(handler) = handlers.get(&node.handler) {
                    match crate::recovery::recover_node(
                        handler,
                        &node_with_prompt,
                        &ctx,
                        &signal,
                        &self.config.shutdown,
                    )
                    .await
                    {
                        Ok(r) if r.retried => {
                            tracing::info!(
                                node = %node.id,
                                first_status = "fail/error",
                                retry_status = ?r.outcome.status,
                                "M8.9 pipeline recovery completed retry"
                            );
                            outcome = r.outcome;
                        }
                        Ok(_) => {
                            // Recovery skipped (shutdown raised); keep
                            // the original failure outcome.
                        }
                        Err(error) => {
                            tracing::warn!(
                                node = %node.id,
                                error = %error,
                                "M8.9 pipeline recovery dispatch errored"
                            );
                        }
                    }
                }
            }

            let duration_ms = node_start.elapsed().as_millis() as u64;

            report_progress(&format!(
                "{seq_label}: done ({:.0}s)",
                duration_ms as f64 / 1000.0
            ));

            info!(
                node = %node.id,
                model = ?node_with_prompt.model,
                status = ?outcome.status,
                duration_ms,
                tokens_in = outcome.token_usage.input_tokens,
                tokens_out = outcome.token_usage.output_tokens,
                output_chars = outcome.content.len(),
                "node completed"
            );

            // coding-blue FA-7: per-node validators. When the pipeline
            // context has a `validators_by_node` override for this
            // node, run it now against the working directory. A
            // required-validator failure demotes the node outcome to
            // `Error`, which both records a fail summary and triggers
            // the existing Error-handling branch below (pipeline stops,
            // returns success=false).
            if outcome.status == OutcomeStatus::Pass {
                if let Err(reason) = self.run_node_validators(&node.id).await {
                    warn!(
                        node = %node.id,
                        reason = %reason,
                        "per-node validator rejected outcome"
                    );
                    outcome.status = OutcomeStatus::Error;
                    outcome.content =
                        format!("Pipeline node validator rejected '{}': {reason}", node.id);
                }
            }

            // M8 parity (W1.A4): drop the per-node reservation handle
            // (auto-refund) and capture a NodeCost row from the actual
            // post-dispatch token usage. The pipeline-level handle
            // already records the cumulative attribution at the run's
            // terminal so per-node ledger writes would double-count;
            // the NodeCost row stays in-memory for the UI panel and
            // the SSE done payload. `committed = true` indicates an
            // accountant was bound; the actual ledger commit lives at
            // pipeline scope.
            let node_cost_committed = node_reservation.is_some();
            drop(node_reservation);

            let actual_usd = octos_agent::cost_ledger::project_cost_usd(
                node_with_prompt.model.as_deref().unwrap_or("pipeline-node"),
                outcome.token_usage.input_tokens,
                outcome.token_usage.output_tokens,
            )
            .unwrap_or(node_reserved_usd);
            node_costs.push(NodeCost {
                node_id: node.id.clone(),
                model: node_with_prompt.model.clone(),
                reserved_usd: node_reserved_usd,
                actual_usd,
                tokens_in: outcome.token_usage.input_tokens,
                tokens_out: outcome.token_usage.output_tokens,
                committed: node_cost_committed,
            });

            // M8 parity (W1.A3): mark the registered child task
            // terminal so the supervisor's progress reporter pushes a
            // final state transition onto the SSE stream.
            if let Some(task_id) = node_task_ids.get(&node.id).cloned() {
                if let Some(ref supervisor) = self.config.host_context.task_supervisor {
                    match outcome.status {
                        OutcomeStatus::Pass => {
                            let files: Vec<String> = outcome
                                .files_modified
                                .iter()
                                .map(|p| p.display().to_string())
                                .collect();
                            supervisor.mark_completed(&task_id, files);
                        }
                        OutcomeStatus::Fail | OutcomeStatus::Error => {
                            supervisor.mark_failed(&task_id, format!("node {} failed", node.id));
                        }
                        OutcomeStatus::Skipped => {
                            // Treat as completed-with-no-output so the
                            // supervisor doesn't keep the task as
                            // running for the rest of the pipeline.
                            supervisor.mark_completed(&task_id, Vec::new());
                        }
                    }
                }
            }

            // Record tokens and feed to status bridge
            total_tokens.input_tokens += outcome.token_usage.input_tokens;
            total_tokens.output_tokens += outcome.token_usage.output_tokens;
            if let Some(ref bridge) = self.config.status_bridge {
                bridge.add_tokens(&outcome.token_usage);
            }

            summaries.push(NodeSummary {
                node_id: node.id.clone(),
                label: node.label.as_deref().unwrap_or(&node.id).to_string(),
                model: node_with_prompt.model.clone(),
                token_usage: outcome.token_usage.clone(),
                duration_ms,
                success: outcome.status == OutcomeStatus::Pass,
            });

            completed.insert(current_node_id.clone(), outcome.clone());

            // Gap 4.2 / Blocker 1 — normal-completion path: emit the real
            // `node_completed` (node name, success, BOUNDED partial-output
            // preview via Gap-3.4 truncation) AND disarm the guard so its Drop
            // does not double-emit a terminal `node_completed{false}`. Every
            // early-exit path above this line instead lets the guard's Drop fire.
            let node_success = outcome.status == OutcomeStatus::Pass;
            let preview = node_output_preview(&outcome.content);
            node_progress_guard.complete(node_success, &preview);

            // Persist mission checkpoints declared on this node (if any) and
            // the store is configured. Best-effort — a failed persist logs a
            // warning but does not abort the run.
            if outcome.status == OutcomeStatus::Pass && !node.checkpoints.is_empty() {
                if let Some(store) = self.config.checkpoint_store.as_ref() {
                    for decl in &node.checkpoints {
                        let seq = PIPELINE_CHECKPOINT_PERSISTED_TOTAL.load(Ordering::Relaxed);
                        let record =
                            PersistedCheckpoint::from_declaration(&graph.id, &node.id, decl, seq);
                        if let Err(e) = store.persist(&record) {
                            warn!(
                                node = %node.id,
                                checkpoint = %decl.name,
                                error = %e,
                                "failed to persist mission checkpoint"
                            );
                        } else {
                            PIPELINE_CHECKPOINT_PERSISTED_TOTAL.fetch_add(1, Ordering::Relaxed);
                            info!(
                                node = %node.id,
                                checkpoint = %decl.name,
                                "persisted mission checkpoint"
                            );
                        }
                    }
                }
            }

            // Check goal gate
            if node.goal_gate && outcome.status == OutcomeStatus::Pass {
                report_progress(&format!(
                    "Pipeline '{}' complete ({:.0}s)",
                    graph.id,
                    pipeline_start.elapsed().as_secs_f64()
                ));
                info!(
                    pipeline = %graph.id,
                    goal_node = %node.id,
                    "goal gate passed — pipeline complete"
                );
                // Collect all files written by any node in this pipeline
                let mut all_files: Vec<std::path::PathBuf> = outcome.files_modified.clone();
                for o in completed.values() {
                    all_files.extend(o.files_modified.iter().cloned());
                }
                all_files.sort();
                all_files.dedup();
                return Ok(PipelineResult {
                    output: outcome.content,
                    success: true,
                    token_usage: total_tokens,
                    node_summaries: summaries,
                    files_modified: all_files,
                    node_costs: node_costs.clone(),
                });
            }

            // Handle errors
            if outcome.status == OutcomeStatus::Error && !node.continue_on_error {
                warn!(
                    node = %node.id,
                    "node returned error, stopping pipeline"
                );
                return Ok(PipelineResult {
                    output: format!("Pipeline failed at node '{}': {}", node.id, outcome.content),
                    success: false,
                    token_usage: total_tokens,
                    node_summaries: summaries,
                    files_modified: vec![],
                    node_costs: node_costs.clone(),
                });
            }
            if outcome.status == OutcomeStatus::Error && node.continue_on_error {
                warn!(
                    node = %node.id,
                    "node returned error, continuing because continue_on_error=true"
                );
            }

            if let Some(max_total_tokens) = graph.max_total_tokens {
                let spent = total_tokens
                    .input_tokens
                    .saturating_add(total_tokens.output_tokens);
                if spent >= max_total_tokens {
                    warn!(
                        pipeline = %graph.id,
                        spent,
                        max_total_tokens,
                        "pipeline token budget exhausted"
                    );
                    return Ok(PipelineResult {
                        output: format!(
                            "Pipeline token budget exhausted after node '{}': spent {spent}/{max_total_tokens} tokens",
                            node.id
                        ),
                        success: false,
                        token_usage: total_tokens,
                        node_summaries: summaries,
                        files_modified: vec![],
                        node_costs: node_costs.clone(),
                    });
                }
            }

            // Select next edge
            match self.select_next_edge(graph, &current_node_id, &outcome)? {
                Some(next_id) => {
                    info!(
                        from = %current_node_id,
                        to = %next_id,
                        "edge selected"
                    );
                    current_node_id = next_id;
                }
                None => {
                    // No outgoing edges — pipeline terminates
                    info!(
                        pipeline = %graph.id,
                        final_node = %current_node_id,
                        elapsed_ms = pipeline_start.elapsed().as_millis() as u64,
                        "pipeline complete (no outgoing edges)"
                    );
                    let mut all_files: Vec<std::path::PathBuf> = outcome.files_modified.clone();
                    for o in completed.values() {
                        all_files.extend(o.files_modified.iter().cloned());
                    }
                    all_files.sort();
                    all_files.dedup();
                    return Ok(PipelineResult {
                        output: outcome.content,
                        success: outcome.status == OutcomeStatus::Pass,
                        token_usage: total_tokens,
                        node_summaries: summaries,
                        files_modified: all_files,
                        node_costs: node_costs.clone(),
                    });
                }
            }
        }
    }

    async fn execute_with_retries(
        &self,
        handler: &Arc<dyn crate::handler::Handler>,
        node: &crate::graph::PipelineNode,
        ctx: &HandlerContext,
        max_retries: u32,
    ) -> Result<NodeOutcome> {
        for attempt in 0..=max_retries {
            let outcome = handler.execute(node, ctx).await?;

            if outcome.status != OutcomeStatus::Error || attempt >= max_retries {
                return Ok(outcome);
            }

            warn!(
                node = %node.id,
                attempt = attempt + 1,
                max_retries,
                "retrying node after error"
            );
            tokio::time::sleep(Duration::from_millis(1000 * 2u64.pow(attempt))).await;
        }
        unreachable!()
    }

    /// Execute a node, honoring both the generic `max_retries` and — when
    /// `deadline_secs` is set — the `deadline_action` for timeouts.
    async fn dispatch_node(
        &self,
        handler: &Arc<dyn crate::handler::Handler>,
        node: &PipelineNode,
        ctx: &HandlerContext,
        max_retries: u32,
    ) -> Result<DispatchOutcome> {
        let Some(deadline_secs) = node.deadline_secs else {
            let outcome = self
                .execute_with_retries(handler, node, ctx, max_retries)
                .await?;
            return Ok(DispatchOutcome::Completed(outcome));
        };

        let deadline = Duration::from_secs_f64(deadline_secs);
        let action = node.deadline_action.unwrap_or(DeadlineAction::Abort);
        let label = node.label.as_deref().unwrap_or(&node.id).to_string();

        // For Retry, we loop over attempts. For all others, a single timed run.
        let max_attempts = match action {
            DeadlineAction::Retry { max_attempts } => max_attempts.max(1),
            _ => 1,
        };

        let mut last_err: Option<eyre::Report> = None;
        for attempt in 0..max_attempts {
            let fut = self.execute_with_retries(handler, node, ctx, max_retries);
            match tokio::time::timeout(deadline, fut).await {
                Ok(Ok(outcome)) => return Ok(DispatchOutcome::Completed(outcome)),
                Ok(Err(e)) => {
                    last_err = Some(e);
                    if attempt + 1 >= max_attempts {
                        break;
                    }
                }
                Err(_timeout) => {
                    record_deadline_exceeded(&action);
                    warn!(
                        node = %node.id,
                        deadline_secs,
                        attempt = attempt + 1,
                        action = action.name(),
                        "node deadline exceeded"
                    );
                    match action {
                        DeadlineAction::Abort => {
                            eyre::bail!(
                                "node '{}' exceeded deadline of {}s (action=abort)",
                                node.id,
                                deadline_secs
                            );
                        }
                        DeadlineAction::Skip => {
                            return Ok(DispatchOutcome::Skipped { label });
                        }
                        DeadlineAction::Retry { .. } => {
                            if attempt + 1 >= max_attempts {
                                eyre::bail!(
                                    "node '{}' exceeded deadline on all {} retry attempt(s)",
                                    node.id,
                                    max_attempts
                                );
                            }
                            // else: fall through and try again
                        }
                        DeadlineAction::Escalate => {
                            if let Some(hook) = self.config.hook_executor.as_ref() {
                                let payload = HookPayload::on_spawn_failure(
                                    node.id.clone(),
                                    label.clone(),
                                    String::new(),
                                    String::new(),
                                    Some("pipeline"),
                                    Some(handler_kind_label(&node.handler)),
                                    format!(
                                        "deadline_exceeded: node '{}' deadline={}s",
                                        node.id, deadline_secs
                                    ),
                                    Vec::new(),
                                    "deadline_exceeded",
                                    None::<&HookContext>,
                                );
                                let _ = hook.run(HookEvent::OnSpawnFailure, &payload).await;
                            }
                            eyre::bail!(
                                "node '{}' exceeded deadline of {}s (action=escalate)",
                                node.id,
                                deadline_secs
                            );
                        }
                    }
                }
            }
        }
        Err(last_err.unwrap_or_else(|| eyre::eyre!("node '{}' failed all retry attempts", node.id)))
    }

    /// 5-step edge selection algorithm.
    /// Ready-set DAG scheduler — the traversal that replaces the single-path
    /// walk for schedulable graphs (gated; see `run_graph_with_handlers`).
    ///
    /// A node runs once ALL its forward predecessors are settled (completed or
    /// pruned) AND at least one incoming edge fired — so a diamond
    /// `A→B, A→C, B→D, C→D` runs BOTH branches and `D` joins both, the bug the
    /// single-path walk silently half-executed. Conditional edges fire on
    /// `condition` match; unconditional edges fire only on success
    /// (fail-closed). Nodes whose every forward predecessor settled with no
    /// fired in-edge are pruned (recursively). A fired back-edge
    /// (`label=="back_edge"`) re-runs its forward-reachable region, bounded by
    /// a per-node run guard so retry loops terminate.
    async fn execute_graph_dag(
        &self,
        graph: &PipelineGraph,
        handlers: &HandlerRegistry,
        start_node: &str,
        user_input: &str,
        variables: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<PipelineResult> {
        /// Max times any single node may execute — terminates retry loops.
        const MAX_NODE_RUNS: u32 = 10;

        let rc = DagRunCtx {
            graph,
            handlers,
            user_input,
            variables,
        };

        // Startup parity with the legacy walk: the initial "started" progress
        // event + the 5s heartbeat the pipeline UI relies on (long nodes would
        // otherwise look frozen).
        report_progress(&format!(
            "Pipeline '{}' started ({} nodes)",
            graph.id,
            graph.nodes.len()
        ));
        let heartbeat_status = Arc::new(std::sync::Mutex::new(PipelineStatusSnapshot {
            pipeline_id: graph.id.clone(),
            current_node: start_node.to_string(),
            nodes_done: 0,
            nodes_total: graph.nodes.len(),
            start: Instant::now(),
        }));
        let _heartbeat = spawn_pipeline_heartbeat(heartbeat_status.clone(), 5);

        let mut completed: HashMap<String, NodeOutcome> = HashMap::new();
        let mut pruned: HashMap<String, PruneReason> = HashMap::new();
        let mut summaries: Vec<NodeSummary> = Vec::new();
        let mut node_costs: Vec<NodeCost> = Vec::new();
        let mut total_tokens = TokenUsage::default();
        // Back-edge critique to feed a node on its next (retry) run, keyed by
        // the back-edge target. Captured when the back-edge fires, consumed when
        // the target re-runs.
        let mut feedback: HashMap<String, NodeOutcome> = HashMap::new();
        // Set when any node is pruned because a REQUIRED input failed: such a
        // run did not complete a required path, so it must report failure even
        // if some unrelated terminal node passed.
        let mut any_failure_prune = false;
        // Per-back-edge-TARGET retry counter, bounding loops. Keyed by target and
        // incremented on every retry — unlike counting the target's executions it
        // advances even when the target itself never runs (e.g. it is pruned each
        // round), so a back-edge that fires before its target executes cannot
        // spin forever.
        let mut retry_count: HashMap<String, u32> = HashMap::new();

        loop {
            // ---- Forward pass: run ready nodes, prune dead ones, until quiescent.
            loop {
                let settled = |id: &str| completed.contains_key(id) || pruned.contains_key(id);

                // A node is runnable when every NON-PRUNED forward predecessor's
                // edge has fired (a join thus has all its required inputs — a
                // failed predecessor whose edge is fail-closed makes the join
                // prunable, not partial) and ≥1 fired (or it's a root). Pick the
                // lexicographically smallest ready id so concurrent branches
                // execute in a deterministic order (HashMap key iteration is
                // randomized — matters under budgets / goal-gate / side effects).
                let runnable = graph
                    .nodes
                    .keys()
                    .filter(|id| {
                        !settled(id)
                            && matches!(
                                dag_node_readiness(graph, id, &completed, &pruned),
                                Ok(NodeReadiness::Runnable)
                            )
                    })
                    .min()
                    .cloned();

                if let Some(node_id) = runnable {
                    // Refresh the heartbeat snapshot so the periodic chip shows
                    // the node currently executing.
                    if let Ok(mut s) = heartbeat_status.lock() {
                        s.current_node = node_id.clone();
                        s.nodes_done = completed.len();
                    }

                    // Pre-dispatch budget gate (parity with the legacy walk):
                    // refuse to start a node once the pipeline token budget is
                    // exhausted; otherwise cap this node's output to what remains.
                    let remaining_tokens =
                        remaining_pipeline_tokens(graph.max_total_tokens, &total_tokens);
                    if remaining_tokens == Some(0) {
                        let msg = format!(
                            "Pipeline token budget exhausted before node '{}': spent {} tokens",
                            node_id,
                            total_pipeline_tokens(&total_tokens)
                        );
                        return Ok(dag_build_result(
                            Some(false),
                            Some(msg),
                            &completed,
                            summaries,
                            total_tokens,
                            node_costs,
                            graph,
                            false,
                        ));
                    }

                    // Keep graph-edge order (fired_forward_into walks
                    // graph.edges) — the documented fan-in order downstream
                    // handlers/synthesis depend on; do NOT re-sort.
                    let fired = fired_forward_into(graph, &node_id, &completed)?;
                    let node_feedback = feedback.get(&node_id).cloned();
                    let step = match self
                        .dag_execute_node(
                            &rc,
                            &node_id,
                            &fired,
                            &completed,
                            remaining_tokens,
                            node_feedback.as_ref(),
                        )
                        .await?
                    {
                        DagStep::Ran(o) => *o,
                        DagStep::Aborted(reason) => {
                            return Ok(dag_build_result(
                                Some(false),
                                Some(format!(
                                    "Pipeline aborted before node '{node_id}': {reason}"
                                )),
                                &completed,
                                summaries,
                                total_tokens,
                                node_costs,
                                graph,
                                false,
                            ));
                        }
                    };
                    feedback.remove(&node_id); // critique consumed by this run

                    total_tokens.input_tokens += step.outcome.token_usage.input_tokens;
                    total_tokens.output_tokens += step.outcome.token_usage.output_tokens;
                    if let Some(ref bridge) = self.config.status_bridge {
                        bridge.add_tokens(&step.outcome.token_usage);
                    }
                    summaries.push(step.summary);
                    node_costs.push(step.cost);

                    let status = step.outcome.status;
                    let node_meta = graph.nodes.get(&node_id);
                    let continue_on_error = node_meta.map(|n| n.continue_on_error).unwrap_or(false);
                    let is_goal_gate = node_meta.map(|n| n.goal_gate).unwrap_or(false);
                    completed.insert(node_id.clone(), step.outcome);

                    // Hard error stops the pipeline (mirrors the single-path walk).
                    if status == OutcomeStatus::Error && !continue_on_error {
                        let msg = format!(
                            "Pipeline failed at node '{}': {}",
                            node_id,
                            completed
                                .get(&node_id)
                                .map(|o| o.content.as_str())
                                .unwrap_or("")
                        );
                        return Ok(dag_build_result(
                            Some(false),
                            Some(msg),
                            &completed,
                            summaries,
                            total_tokens,
                            node_costs,
                            graph,
                            false,
                        ));
                    }

                    // Goal gate: a passing goal node ends the pipeline
                    // immediately and successfully (mirrors the single-path walk).
                    if is_goal_gate && status == OutcomeStatus::Pass {
                        let content = completed
                            .get(&node_id)
                            .map(|o| o.content.clone())
                            .unwrap_or_default();
                        info!(pipeline = %graph.id, goal_node = %node_id, "DAG: goal gate passed — pipeline complete");
                        return Ok(dag_build_result(
                            Some(true),
                            Some(content),
                            &completed,
                            summaries,
                            total_tokens,
                            node_costs,
                            graph,
                            false,
                        ));
                    }

                    // Token-budget ceiling (post-node, mirrors the legacy walk).
                    if let Some(max_total) = graph.max_total_tokens {
                        let spent = total_tokens
                            .input_tokens
                            .saturating_add(total_tokens.output_tokens);
                        if spent >= max_total {
                            let msg = format!(
                                "Pipeline token budget exhausted after node '{node_id}': spent {spent}/{max_total} tokens"
                            );
                            return Ok(dag_build_result(
                                Some(false),
                                Some(msg),
                                &completed,
                                summaries,
                                total_tokens,
                                node_costs,
                                graph,
                                false,
                            ));
                        }
                    }

                    // Eager back-edge: if THIS node's outcome fires a back-edge,
                    // retry its region NOW — before any forward consumer of the
                    // SAME (e.g. failed) outcome is scheduled (a `report` node on
                    // the same fail condition would otherwise run on a transient
                    // failure that's about to be retried). Bounded by the guard.
                    for e in graph
                        .edges
                        .iter()
                        .filter(|e| e.source == node_id && edge_is_back(graph, e))
                    {
                        let fires = completed
                            .get(&node_id)
                            .map(|src| dag_back_edge_fires(e, src).unwrap_or(false))
                            .unwrap_or(false);
                        if fires && retry_count.get(&e.target).copied().unwrap_or(0) < MAX_NODE_RUNS
                        {
                            *retry_count.entry(e.target.clone()).or_insert(0) += 1;
                            if let Some(src_outcome) = completed.get(&node_id).cloned() {
                                feedback.insert(e.target.clone(), src_outcome);
                            }
                            let region = forward_reachable(graph, &e.target);
                            info!(node = %e.target, region = region.len(), "DAG: eager back-edge retry");
                            for r in &region {
                                completed.remove(r);
                                pruned.remove(r);
                            }
                            break;
                        }
                    }
                    continue;
                }

                // No runnable node — prune one that is settled but can never get
                // all its required inputs (a not-taken conditional branch, or a
                // join missing a failed/fail-closed predecessor). Smallest id
                // first; carry the prune REASON so failure-prunes propagate.
                let prunable = graph
                    .nodes
                    .keys()
                    .filter(|id| !settled(id))
                    .filter_map(
                        |id| match dag_node_readiness(graph, id, &completed, &pruned) {
                            Ok(NodeReadiness::Prune(reason)) => Some((id.clone(), reason)),
                            _ => None,
                        },
                    )
                    .min_by(|a, b| a.0.cmp(&b.0));

                match prunable {
                    Some((p, reason)) => {
                        if reason == PruneReason::Failure {
                            any_failure_prune = true;
                        }
                        info!(node = %p, failure = (reason == PruneReason::Failure), "DAG: pruning node");
                        pruned.insert(p, reason);
                    }
                    None => break, // forward pass quiescent
                }
            }

            // ---- Back-edge retries: a fired back-edge re-runs its region,
            // carrying the source's critique forward as feedback to the target.
            let fired_back: Vec<(String, NodeOutcome)> = graph
                .edges
                .iter()
                .filter(|e| edge_is_back(graph, e))
                .filter_map(|e| {
                    let src = completed.get(&e.source)?;
                    if dag_back_edge_fires(e, src).unwrap_or(false) {
                        Some((e.target.clone(), src.clone()))
                    } else {
                        None
                    }
                })
                .collect();

            let mut retried = false;
            for (target, src_outcome) in fired_back {
                let rc = retry_count.get(&target).copied().unwrap_or(0);
                if rc < MAX_NODE_RUNS {
                    *retry_count.entry(target.clone()).or_insert(0) += 1;
                    // Hand the target the critique that triggered the retry so
                    // the re-run is feedback-driven, not a blind repeat.
                    feedback.insert(target.clone(), src_outcome);
                    let region = forward_reachable(graph, &target);
                    info!(node = %target, region = region.len(), "DAG: back-edge retry");
                    for r in &region {
                        completed.remove(r);
                        pruned.remove(r);
                    }
                    retried = true;
                    break;
                }
                warn!(
                    node = %target,
                    max = MAX_NODE_RUNS,
                    "DAG: loop guard hit; not retrying"
                );
            }
            if !retried {
                break;
            }
        }

        // Normal quiescence: derive success from the terminal nodes (those with
        // no fired outgoing forward edge) rather than assuming success — a
        // terminal that settled Fail/Skipped (or a retry loop whose guard was
        // hit while still failing) must report failure, like the legacy walk.
        Ok(dag_build_result(
            None,
            None,
            &completed,
            summaries,
            total_tokens,
            node_costs,
            graph,
            any_failure_prune,
        ))
    }

    /// Execute one node on the DAG path. Mirrors the per-node body of
    /// `execute_graph` (input join, template/model resolution, task
    /// registration, per-node budget reservation, dispatch-with-retries+
    /// deadline, per-node validators, cost row, supervisor terminal) but is
    /// driven by the scheduler's fired-predecessor set rather than a single
    /// walk pointer. Fan-out/converge/recovery-loop are NOT handled here —
    /// such graphs are routed to the legacy walk by `graph_is_dag_schedulable`.
    async fn dag_execute_node(
        &self,
        rc: &DagRunCtx<'_>,
        node_id: &str,
        fired_preds: &[String],
        completed: &HashMap<String, NodeOutcome>,
        remaining_tokens: Option<u32>,
        feedback: Option<&NodeOutcome>,
    ) -> Result<DagStep> {
        let graph = rc.graph;
        let handlers = rc.handlers;
        let user_input = rc.user_input;
        let variables = rc.variables;
        let node = graph
            .nodes
            .get(node_id)
            .ok_or_else(|| eyre::eyre!("DAG: unknown node {node_id}"))?;
        let handler = handlers
            .get(&node.handler)
            .ok_or_else(|| eyre::eyre!("no handler for {:?}", node.handler))?;

        // Input = fired forward-predecessor outputs, PLUS any back-edge
        // feedback that triggered a retry of this node (the evaluator/check
        // critique). Without the feedback a repair loop would re-run on the
        // same original input and never converge (legacy includes the back-edge
        // source as a predecessor; the DAG forward graph excludes it, so it is
        // threaded explicitly here).
        let mut input_parts: Vec<&str> = fired_preds
            .iter()
            .filter_map(|p| completed.get(p))
            .map(|o| o.content.as_str())
            .collect();
        if let Some(fb) = feedback {
            input_parts.push(fb.content.as_str());
        }
        let input_text = if input_parts.is_empty() {
            user_input.to_string()
        } else {
            input_parts.join("\n\n---\n\n")
        };

        let mut node_with_prompt = node.clone();
        if let Some(ref prompt) = node_with_prompt.prompt {
            let mut resolved = prompt.replace("{input}", "");
            for (k, v) in variables {
                let placeholder = format!("{{{k}}}");
                resolved = resolved.replace(&placeholder, v.as_str().unwrap_or(""));
            }
            node_with_prompt.prompt = Some(resolved.trim_end().to_string());
        }
        if node_with_prompt.model.is_none() {
            node_with_prompt.model = graph.default_model.clone();
        }

        // Cap this node's output to the remaining pipeline budget before
        // dispatch (parity with the legacy walk) so a Codergen node can't
        // request its full limit and overshoot `max_total_tokens`.
        if let Some(rem) = remaining_tokens {
            cap_node_output_tokens_for_remaining_budget(&mut node_with_prompt, rem, 1);
        }

        let mut predecessor_outcomes: Vec<NodeOutcome> = fired_preds
            .iter()
            .filter_map(|p| completed.get(p).cloned())
            .collect();
        if let Some(fb) = feedback {
            predecessor_outcomes.push(fb.clone());
        }
        let ctx = HandlerContext {
            input: input_text,
            completed: completed.clone(),
            predecessor_outcomes,
            working_dir: self.config.working_dir.clone(),
        };

        let seq_label = node.label.as_deref().unwrap_or(&node.id).to_string();
        report_progress(&format!("{seq_label}: running..."));
        let node_total = graph.nodes.len();
        let node_index = completed.len() + 1;
        let guard = NodeProgressGuard::arm(&graph.id, &node.id, &seq_label, node_index, node_total);

        if let Some(ref bridge) = self.config.status_bridge {
            bridge.set_words(vec![seq_label.clone()]);
        }

        // Registration refusal (terminal parent / fanout cap) → structured
        // failure result, NOT an execution error (parity with the legacy walk).
        let node_task_id = match self.register_node_task(&node.id) {
            Ok(opt) => opt,
            Err(reason) => return Ok(DagStep::Aborted(reason)),
        };
        if let Some(ref id) = node_task_id {
            if let Some(ref supervisor) = self.config.host_context.task_supervisor {
                supervisor.mark_running(id);
            }
        }

        let node_reservation = self
            .reserve_node_budget(&graph.id, &node_with_prompt)
            .await?;
        let node_reserved_usd = node_reservation
            .as_ref()
            .map(|h| h.reserved_amount_usd())
            .unwrap_or(0.0);

        let node_start = Instant::now();
        let dispatch = self
            .dispatch_node(handler, &node_with_prompt, &ctx, node.max_retries)
            .await;
        let mut outcome = match dispatch? {
            DispatchOutcome::Completed(o) => o,
            DispatchOutcome::Skipped { .. } => NodeOutcome {
                node_id: node.id.clone(),
                status: OutcomeStatus::Skipped,
                content: format!("Node '{}' skipped (deadline exceeded)", node.id),
                token_usage: TokenUsage::default(),
                files_modified: vec![],
            },
        };

        // M8.9 recovery (parity with the walk): one re-attempt on a retryable
        // first failure before the outcome is allowed to prune/abort. Skipped/
        // Pass outcomes short-circuit inside `classify_outcome`.
        let recovery_input = serde_json::json!({ "node": node.id, "input": ctx.input });
        if let crate::recovery::RecoveryDecision::Retryable(signal) =
            crate::recovery::classify_outcome(&node_with_prompt, &outcome, &recovery_input)
        {
            match crate::recovery::recover_node(
                handler,
                &node_with_prompt,
                &ctx,
                &signal,
                &self.config.shutdown,
            )
            .await
            {
                Ok(r) if r.retried => outcome = r.outcome,
                Ok(_) => {}
                Err(error) => {
                    warn!(node = %node.id, error = %error, "DAG: M8.9 recovery dispatch errored");
                }
            }
        }

        // Per-node terminal validators — only on a passing outcome (parity with
        // the walk): a node that already returned Fail/Error/Skipped must route
        // through its failure edge, not be demoted to Error by a validator.
        if outcome.status == OutcomeStatus::Pass {
            if let Err(reason) = self.run_node_validators(&node.id).await {
                warn!(node = %node.id, reason = %reason, "DAG: per-node validator rejected outcome");
                outcome.status = OutcomeStatus::Error;
                outcome.content =
                    format!("Pipeline node validator rejected '{}': {reason}", node.id);
            }
        }

        // Measure AFTER recovery + validators (parity) so a recovery re-attempt
        // is included in the reported node duration.
        let duration_ms = node_start.elapsed().as_millis() as u64;

        let node_cost_committed = node_reservation.is_some();
        drop(node_reservation);
        let actual_usd = octos_agent::cost_ledger::project_cost_usd(
            node_with_prompt.model.as_deref().unwrap_or("pipeline-node"),
            outcome.token_usage.input_tokens,
            outcome.token_usage.output_tokens,
        )
        .unwrap_or(node_reserved_usd);
        let cost = NodeCost {
            node_id: node.id.clone(),
            model: node_with_prompt.model.clone(),
            reserved_usd: node_reserved_usd,
            actual_usd,
            tokens_in: outcome.token_usage.input_tokens,
            tokens_out: outcome.token_usage.output_tokens,
            committed: node_cost_committed,
        };

        if let Some(task_id) = node_task_id {
            if let Some(ref supervisor) = self.config.host_context.task_supervisor {
                match outcome.status {
                    OutcomeStatus::Pass => {
                        let files = outcome
                            .files_modified
                            .iter()
                            .map(|p| p.display().to_string())
                            .collect();
                        supervisor.mark_completed(&task_id, files);
                    }
                    OutcomeStatus::Fail | OutcomeStatus::Error => {
                        supervisor.mark_failed(&task_id, format!("node {} failed", node.id));
                    }
                    OutcomeStatus::Skipped => supervisor.mark_completed(&task_id, Vec::new()),
                }
            }
        }

        let success = outcome.status == OutcomeStatus::Pass;
        let summary = NodeSummary {
            node_id: node.id.clone(),
            label: seq_label,
            model: node_with_prompt.model.clone(),
            token_usage: outcome.token_usage.clone(),
            duration_ms,
            success,
        };
        guard.complete(success, &node_output_preview(&outcome.content));

        Ok(DagStep::Ran(Box::new(DagNodeOutput {
            outcome,
            summary,
            cost,
        })))
    }

    fn select_next_edge(
        &self,
        graph: &PipelineGraph,
        current: &str,
        outcome: &NodeOutcome,
    ) -> Result<Option<String>> {
        let outgoing: Vec<&PipelineEdge> =
            graph.edges.iter().filter(|e| e.source == current).collect();

        if outgoing.is_empty() {
            return Ok(None);
        }

        // Step 1: Evaluate conditions
        let mut condition_matches: Vec<&PipelineEdge> = Vec::new();
        for edge in &outgoing {
            if let Some(ref cond_str) = edge.condition {
                let expr = condition::parse_condition(cond_str)?;
                if condition::evaluate(&expr, outcome) {
                    condition_matches.push(edge);
                }
            }
        }

        // Step 2: If any condition matches, pick highest-weight match
        if !condition_matches.is_empty() {
            return Ok(Some(pick_by_weight(&condition_matches)));
        }

        // Step 3: Check suggested_next from node attribute
        if let Some(ref next) = graph.nodes[current].suggested_next {
            if outgoing.iter().any(|e| e.target == *next) {
                return Ok(Some(next.clone()));
            }
        }

        // Step 4: Check edge labels matching outcome content
        for edge in &outgoing {
            if let Some(ref label) = edge.label {
                if outcome.content.contains(label.as_str()) {
                    return Ok(Some(edge.target.clone()));
                }
            }
        }

        // Step 5: Highest-weight unconditional edge
        let unconditional: Vec<&PipelineEdge> = outgoing
            .iter()
            .filter(|e| e.condition.is_none())
            .copied()
            .collect();

        if !unconditional.is_empty() {
            return Ok(Some(pick_by_weight(&unconditional)));
        }

        // Fallback: first outgoing edge by target name
        let fallback = outgoing.iter().min_by_key(|e| &e.target).unwrap();
        Ok(Some(fallback.target.clone()))
    }
}

/// Retry helper usable from parallel futures (no `&self` borrow).
async fn execute_with_retries_static(
    handler: &Arc<dyn crate::handler::Handler>,
    node: &crate::graph::PipelineNode,
    ctx: &HandlerContext,
    max_retries: u32,
) -> Result<NodeOutcome> {
    for attempt in 0..=max_retries {
        let outcome = handler.execute(node, ctx).await?;
        if outcome.status != OutcomeStatus::Error || attempt >= max_retries {
            return Ok(outcome);
        }
        warn!(
            node = %node.id,
            attempt = attempt + 1,
            max_retries,
            "retrying node after error"
        );
        tokio::time::sleep(Duration::from_millis(1000 * 2u64.pow(attempt))).await;
    }
    unreachable!()
}

/// Run a single fan-out worker, honoring its `deadline_action` on a deadline
/// expiry while ALWAYS bounding each timed attempt so a genuinely-hung worker
/// cannot wedge `join_all`.
///
/// This is the fan-out analogue of [`PipelineExecutor::dispatch_node`]: it
/// routes a fan-out worker's deadline expiry through the SAME
/// `deadline_action` machinery the single-node path uses, so a parallel /
/// dynamic_parallel target that declares `deadline_secs` with
/// `deadline_action = skip|retry|escalate` gets the configured behavior
/// instead of an unconditional `Err`.
///
/// Bounding guarantee (the production wedge must NOT return): every timed
/// attempt is wrapped in [`fanout_worker_deadline`], which is itself clamped
/// to the absolute [`MAX_FANOUT_WORKER_SECS`] ceiling. So even with no
/// per-node `deadline_secs`, a worker whose future never resolves is cut off
/// at the ceiling. `Skip` stops after the first expiry (it never re-runs the
/// hung future); `Retry` re-attempts at most `max_attempts` times, each
/// attempt independently bounded — so the worker always terminates.
///
/// Returns:
/// * `Ok(NodeOutcome)` with `OutcomeStatus::Skipped` when the deadline fired
///   and `deadline_action == Skip` — `process_worker_results` treats a
///   `Skipped` outcome as neither a usable pass nor a hard error, so the
///   normal convergence routing handles it.
/// * `Err` when the deadline fired and the action is `Abort` (default),
///   `Escalate`, or `Retry` with all attempts exhausted; or when the
///   underlying handler itself errors.
async fn run_fanout_worker(
    handler: &Arc<dyn crate::handler::Handler>,
    node: &crate::graph::PipelineNode,
    ctx: &HandlerContext,
    max_retries: u32,
    hook_executor: Option<&Arc<HookExecutor>>,
) -> Result<NodeOutcome> {
    // Effective per-attempt deadline, already clamped to MAX_FANOUT_WORKER_SECS
    // so even an action that re-runs the worker (Retry) stays bounded and the
    // hung-worker wedge cannot return.
    let deadline = fanout_worker_deadline(node);
    let action = node.deadline_action.unwrap_or(DeadlineAction::Abort);
    let label = node.label.as_deref().unwrap_or(&node.id).to_string();

    // For Retry, loop over attempts; every other action runs once.
    let max_attempts = match action {
        DeadlineAction::Retry { max_attempts } => max_attempts.max(1),
        _ => 1,
    };

    let mut last_err: Option<eyre::Report> = None;
    for attempt in 0..max_attempts {
        let fut = execute_with_retries_static(handler, node, ctx, max_retries);
        match tokio::time::timeout(deadline, fut).await {
            Ok(Ok(outcome)) => return Ok(outcome),
            Ok(Err(e)) => {
                // The handler itself errored (not a deadline expiry). Mirror the
                // pre-existing behavior: surface the error to the worker result.
                return Err(e);
            }
            Err(_timeout) => {
                record_deadline_exceeded(&action);
                warn!(
                    node = %node.id,
                    deadline_secs = deadline.as_secs(),
                    attempt = attempt + 1,
                    action = action.name(),
                    "fan-out worker exceeded deadline"
                );
                match action {
                    DeadlineAction::Abort => {
                        return Err(eyre::eyre!(
                            "fan-out worker '{}' exceeded deadline of {}s (action=abort)",
                            node.id,
                            deadline.as_secs()
                        ));
                    }
                    DeadlineAction::Skip => {
                        return Ok(NodeOutcome {
                            node_id: node.id.clone(),
                            status: OutcomeStatus::Skipped,
                            content: format!("Node '{}' skipped (deadline exceeded)", node.id),
                            token_usage: TokenUsage::default(),
                            files_modified: vec![],
                        });
                    }
                    DeadlineAction::Retry { .. } => {
                        last_err = Some(eyre::eyre!(
                            "fan-out worker '{}' exceeded deadline of {}s",
                            node.id,
                            deadline.as_secs()
                        ));
                        if attempt + 1 >= max_attempts {
                            return Err(eyre::eyre!(
                                "fan-out worker '{}' exceeded deadline on all {} retry attempt(s)",
                                node.id,
                                max_attempts
                            ));
                        }
                        // else: fall through and try again (still bounded).
                    }
                    DeadlineAction::Escalate => {
                        if let Some(hook) = hook_executor {
                            let payload = HookPayload::on_spawn_failure(
                                node.id.clone(),
                                label.clone(),
                                String::new(),
                                String::new(),
                                Some("pipeline"),
                                Some(handler_kind_label(&node.handler)),
                                format!(
                                    "deadline_exceeded: fan-out worker '{}' deadline={}s",
                                    node.id,
                                    deadline.as_secs()
                                ),
                                Vec::new(),
                                "deadline_exceeded",
                                None::<&HookContext>,
                            );
                            let _ = hook.run(HookEvent::OnSpawnFailure, &payload).await;
                        }
                        return Err(eyre::eyre!(
                            "fan-out worker '{}' exceeded deadline of {}s (action=escalate)",
                            node.id,
                            deadline.as_secs()
                        ));
                    }
                }
            }
        }
    }
    Err(last_err
        .unwrap_or_else(|| eyre::eyre!("fan-out worker '{}' failed all retry attempts", node.id)))
}

/// Pick the edge with the highest weight, tie-break by lexicographic target.
fn pick_by_weight(edges: &[&PipelineEdge]) -> String {
    let max_weight = edges
        .iter()
        .map(|e| e.weight)
        .fold(f64::NEG_INFINITY, f64::max);

    let ties: Vec<&&PipelineEdge> = edges
        .iter()
        .filter(|e| (e.weight - max_weight).abs() < f64::EPSILON)
        .collect();

    ties.iter()
        .min_by_key(|e| &e.target)
        .unwrap()
        .target
        .clone()
}

// ---------------------------------------------------------------------------
// DAG scheduler helpers (ready-set traversal). Free functions so both the
// readiness closures and `execute_graph_dag` share one definition.
// ---------------------------------------------------------------------------

/// Per-node result the DAG scheduler accumulates from `dag_execute_node`.
struct DagNodeOutput {
    outcome: NodeOutcome,
    summary: NodeSummary,
    cost: NodeCost,
}

/// Outcome of a single `dag_execute_node` call: either the node ran, or its
/// task registration was refused (terminal parent / fanout cap) — which the
/// scheduler converts into a structured failure `PipelineResult`, like the
/// legacy walk, rather than an execution error.
enum DagStep {
    Ran(Box<DagNodeOutput>),
    Aborted(String),
}

/// Why a node was pruned from a DAG run. The distinction is load-bearing for
/// fail-closed joins: an `Optional` prune (a conditional branch not taken) is a
/// legitimately-absent input a downstream join may proceed without, but a
/// `Failure` prune (a required input failed upstream) must PROPAGATE — a join
/// consuming a failure-pruned predecessor is itself failure-pruned, never run
/// with partial input.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PruneReason {
    Optional,
    Failure,
}

/// Readiness verdict for a node in the DAG forward pass.
enum NodeReadiness {
    /// Not all forward predecessors are settled yet.
    NotReady,
    /// Every required input is available — run it.
    Runnable,
    /// Settled but cannot obtain all required inputs — prune with this reason.
    Prune(PruneReason),
}

/// Run-invariant context threaded to every `dag_execute_node` call (keeps the
/// per-node signature small — the graph/handlers/input/vars never change
/// across a single DAG run).
struct DagRunCtx<'a> {
    graph: &'a PipelineGraph,
    handlers: &'a HandlerRegistry,
    user_input: &'a str,
    variables: &'a serde_json::Map<String, serde_json::Value>,
}

/// Production opt-in for the DAG scheduler. Tests use the
/// `with_dag_scheduler` builder instead (env is process-global / racy).
fn dag_scheduler_enabled() -> bool {
    matches!(
        std::env::var("OCTOS_PIPELINE_DAG").ok().as_deref(),
        Some("1") | Some("true") | Some("TRUE")
    )
}

/// A graph is DAG-schedulable when it uses none of the routing features the
/// ready-set scheduler does not implement (and which the single-path
/// `select_next_edge` walk does): runtime fan-out
/// (`Parallel`/`DynamicParallel`/`converge`), `node.suggested_next` hints, and
/// content-matching forward edge `label`s. The DAG path understands only
/// `condition` predicates + source status for edge firing, so a graph that
/// routes by label or suggested_next must stay on the legacy walk to avoid
/// firing every labeled branch. Marked back-edge labels are exempt (they encode
/// loops, not routing). Everything else (linear, diamonds, conditional
/// branches, bounded retry loops) the scheduler handles.
fn graph_is_dag_schedulable(graph: &PipelineGraph) -> bool {
    let nodes_ok = graph.nodes.values().all(|n| {
        !matches!(
            n.handler,
            HandlerKind::Parallel | HandlerKind::DynamicParallel
        ) && n.converge.is_none()
            && n.suggested_next.is_none()
    });
    // A non-back-edge forward edge carrying a `label` (content-matching) or a
    // non-default `weight` (the legacy selector uses weight to pick exactly one
    // edge; DAG firing would fan out to all) is routing the DAG firing logic
    // does not implement → keep such graphs on the legacy walk.
    let edges_ok = graph.edges.iter().all(|e| {
        edge_is_back(graph, e) || (e.label.is_none() && (e.weight - 1.0).abs() < f64::EPSILON)
    });
    // A back-edge must (a) carry a condition — a conditionless back-edge can't
    // fire meaningfully (always → guard loop; never → dead retry) — and (b)
    // target either the validated START node or a node that ALSO has a forward
    // predecessor. A back-edge-only NON-start target would be a spurious initial
    // root (the legacy walk reaches it only via the back-edge); route such loops
    // to the legacy walk. A retry edge back to the start IS valid — start is the
    // legitimate root and re-runs under the loop guard.
    let start = validate::find_start_node(graph);
    let back_edges_ok = graph.edges.iter().all(|e| {
        !edge_is_back(graph, e)
            || (e.condition.is_some()
                && (start.as_deref() == Some(e.target.as_str())
                    || !forward_preds(graph, &e.target).is_empty()))
    });
    nodes_ok && edges_ok && back_edges_ok
}

/// A back-edge is an edge that (a) carries a `retry`/`back_edge`/`guard_back`
/// marker on its label or condition — the same predicate
/// `validate::edge_allows_back_edge` uses to PERMIT a cycle — AND (b) actually
/// closes a cycle (its target can reach its source through the graph). Both
/// conditions matter: the marker alone is insufficient (an acyclic forward edge
/// whose condition merely contains "retry" must NOT be stripped from the
/// forward graph, or its target would become a spurious root), and only marked
/// edges may legally close a cycle (validation rejects unmarked ones).
fn edge_is_back(graph: &PipelineGraph, edge: &PipelineEdge) -> bool {
    let marked = edge
        .label
        .as_deref()
        .is_some_and(validate::has_back_edge_marker)
        || edge
            .condition
            .as_deref()
            .is_some_and(validate::has_back_edge_marker);
    marked && graph_reaches(graph, &edge.target, &edge.source)
}

/// Can `to` be reached from `from` over the graph's edges? Used to decide
/// whether a marked edge is topologically backward (target reaches source).
/// Bounded by a visited-set, so cycles in the graph don't loop it.
fn graph_reaches(graph: &PipelineGraph, from: &str, to: &str) -> bool {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut stack: Vec<&str> = vec![from];
    while let Some(n) = stack.pop() {
        if n == to {
            return true;
        }
        if !seen.insert(n) {
            continue;
        }
        for e in graph.edges.iter().filter(|e| e.source == n) {
            stack.push(e.target.as_str());
        }
    }
    false
}

/// Readiness of a node for the DAG forward pass — distinguishing optional from
/// failure prunes so fail-closed semantics propagate across multiple hops.
///
/// A forward predecessor `p` of `id` is satisfied when its edge fired (input
/// provided) or it was `Optional`-pruned (a not-taken branch). It is a MISSING
/// REQUIRED input when: it was `Failure`-pruned (propagated upstream failure),
/// or it completed with a Fail/Error and the (unconditional) edge to `id` is
/// fail-closed. Any missing required input ⇒ `Prune(Failure)`. Otherwise the
/// node runs if ≥1 input fired, or is `Prune(Optional)` if every predecessor was
/// an optional not-taken branch.
fn dag_node_readiness(
    graph: &PipelineGraph,
    id: &str,
    completed: &HashMap<String, NodeOutcome>,
    pruned: &HashMap<String, PruneReason>,
) -> Result<NodeReadiness> {
    let fpreds = forward_preds(graph, id);
    if !fpreds
        .iter()
        .all(|p| completed.contains_key(p) || pruned.contains_key(p))
    {
        return Ok(NodeReadiness::NotReady);
    }
    if fpreds.is_empty() {
        // Root. `graph_is_dag_schedulable` guarantees every non-start node has a
        // forward predecessor (a back-edge-only target — a spurious root — keeps
        // its graph on the legacy walk), so the only no-forward-pred node here
        // is the validated start.
        return Ok(NodeReadiness::Runnable);
    }
    let fired = fired_forward_into(graph, id, completed)?;
    let mut missing_required = false;
    for p in &fpreds {
        if fired.contains(p) {
            continue; // input provided
        }
        match pruned.get(p) {
            Some(PruneReason::Failure) => missing_required = true, // propagate
            Some(PruneReason::Optional) => {}                      // not-taken branch
            None => {
                // p completed but its edge to `id` did not fire. Strict
                // fail-closed: a failed source on an UNCONDITIONAL (required)
                // edge is a missing required input — a join never runs on
                // partial input. A conditional mismatch or a router suppression
                // is an optional not-taken branch. (A recovery that should
                // rejoin a join is expressed with a conditional success edge
                // `p -> join [condition=pass]`, which is an optional miss on
                // failure, not a hard dependency.)
                let edge_unconditional = graph.edges.iter().any(|e| {
                    e.source.as_str() == p.as_str()
                        && e.target.as_str() == id
                        && !edge_is_back(graph, e)
                        && e.condition.is_none()
                });
                let coe = graph
                    .nodes
                    .get(p)
                    .map(|n| n.continue_on_error)
                    .unwrap_or(false);
                let p_failed = completed
                    .get(p)
                    .map(|o| matches!(o.status, OutcomeStatus::Fail | OutcomeStatus::Error))
                    .unwrap_or(false)
                    && !coe;
                if edge_unconditional && p_failed {
                    missing_required = true;
                }
            }
        }
    }
    if missing_required {
        Ok(NodeReadiness::Prune(PruneReason::Failure))
    } else if fired.is_empty() {
        Ok(NodeReadiness::Prune(PruneReason::Optional))
    } else {
        Ok(NodeReadiness::Runnable)
    }
}

/// Forward (non-back-edge) predecessor sources of `node_id`.
fn forward_preds(graph: &PipelineGraph, node_id: &str) -> Vec<String> {
    graph
        .edges
        .iter()
        .filter(|e| e.target == node_id && !edge_is_back(graph, e))
        .map(|e| e.source.clone())
        .collect()
}

/// Forward predecessor sources whose edge into `node_id` *fired*, given the
/// current `completed` outcomes. The DAG join feeds a node exactly these.
fn fired_forward_into(
    graph: &PipelineGraph,
    node_id: &str,
    completed: &HashMap<String, NodeOutcome>,
) -> Result<Vec<String>> {
    let mut fired = Vec::new();
    for e in graph
        .edges
        .iter()
        .filter(|e| e.target == node_id && !edge_is_back(graph, e))
    {
        if let Some(src) = completed.get(&e.source) {
            let continue_on_error = graph
                .nodes
                .get(&e.source)
                .map(|n| n.continue_on_error)
                .unwrap_or(false);
            if dag_forward_edge_fires(graph, e, src, continue_on_error)? {
                fired.push(e.source.clone());
            }
        }
    }
    Ok(fired)
}

/// Does a forward edge fire under the normal rules? Conditional edges fire on
/// `condition` match; unconditional edges fire only on a successful source
/// (fail-closed — an unconditional edge out of a `Fail` source does NOT fire),
/// and not when a conditional sibling matched (conditions take precedence).
fn dag_edge_fires_normally(
    graph: &PipelineGraph,
    edge: &PipelineEdge,
    src_outcome: &NodeOutcome,
    src_continue_on_error: bool,
) -> Result<bool> {
    if let Some(cond) = &edge.condition {
        let expr = condition::parse_condition(cond)?;
        return Ok(condition::evaluate(&expr, src_outcome));
    }
    let src_success = matches!(
        src_outcome.status,
        OutcomeStatus::Pass | OutcomeStatus::Skipped
    ) || (src_outcome.status == OutcomeStatus::Error && src_continue_on_error);
    if !src_success {
        return Ok(false);
    }
    let conditional_sibling_matched = graph
        .edges
        .iter()
        .filter(|e| e.source == edge.source && !edge_is_back(graph, e))
        .filter_map(|e| e.condition.as_deref())
        .any(|cond| {
            condition::parse_condition(cond)
                .map(|expr| condition::evaluate(&expr, src_outcome))
                .unwrap_or(false)
        });
    Ok(!conditional_sibling_matched)
}

/// Does a forward edge fire? Normal firing (see [`dag_edge_fires_normally`]),
/// plus the legacy `select_next_edge` Step-6 fallback: an ALL-CONDITIONAL
/// router with NO matching condition routes to its lowest-target edge rather
/// than dead-ending. Restricted to all-conditional sources because legacy only
/// reaches that fallback when no unconditional edge exists, so an unconditional
/// edge out of a failed source stays fail-closed.
fn dag_forward_edge_fires(
    graph: &PipelineGraph,
    edge: &PipelineEdge,
    src_outcome: &NodeOutcome,
    src_continue_on_error: bool,
) -> Result<bool> {
    if dag_edge_fires_normally(graph, edge, src_outcome, src_continue_on_error)? {
        return Ok(true);
    }
    let outgoing: Vec<&PipelineEdge> = graph
        .edges
        .iter()
        .filter(|e| e.source == edge.source && !edge_is_back(graph, e))
        .collect();
    // Fallback applies only to all-conditional routers (no unconditional edge).
    if outgoing.is_empty() || !outgoing.iter().all(|e| e.condition.is_some()) {
        return Ok(false);
    }
    let mut any_matched = false;
    for e in &outgoing {
        if let Some(cond) = e.condition.as_deref() {
            let expr = condition::parse_condition(cond)?;
            if condition::evaluate(&expr, src_outcome) {
                any_matched = true;
                break;
            }
        }
    }
    if any_matched {
        return Ok(false);
    }
    // Nothing matched → route to the lowest-target outgoing edge.
    let min_target = outgoing.iter().map(|e| e.target.as_str()).min();
    Ok(min_target == Some(edge.target.as_str()))
}

/// A back-edge fires only when its condition matches (an unconditional
/// back-edge is inert — a meaningful retry carries e.g.
/// `condition="outcome.status == \"fail\""`). The per-node run guard bounds it.
fn dag_back_edge_fires(edge: &PipelineEdge, src_outcome: &NodeOutcome) -> Result<bool> {
    match &edge.condition {
        Some(cond) => {
            let expr = condition::parse_condition(cond)?;
            Ok(condition::evaluate(&expr, src_outcome))
        }
        None => Ok(false),
    }
}

/// Nodes reachable from `start` over forward edges (the region a fired
/// back-edge must re-run so its consumers see the retried output).
fn forward_reachable(graph: &PipelineGraph, start: &str) -> HashSet<String> {
    let mut seen = HashSet::new();
    let mut stack = vec![start.to_string()];
    while let Some(n) = stack.pop() {
        if !seen.insert(n.clone()) {
            continue;
        }
        for e in graph
            .edges
            .iter()
            .filter(|e| e.source == n && !edge_is_back(graph, e))
        {
            stack.push(e.target.clone());
        }
    }
    seen
}

/// Terminal nodes of a finished DAG run: completed nodes from which NO outgoing
/// forward edge fired (nothing downstream ran from them). These are the
/// effective sinks of the *executed* subgraph — the analogue of the node where
/// the single-path walk stops — so both the result output and the success
/// verdict derive from them, not from raw structural sinks (which a dead/pruned
/// branch could distort). Sorted for determinism.
fn dag_terminal_nodes(
    graph: &PipelineGraph,
    completed: &HashMap<String, NodeOutcome>,
) -> Vec<String> {
    let mut terms: Vec<String> = completed
        .keys()
        .filter(|n| {
            let coe = graph
                .nodes
                .get(*n)
                .map(|x| x.continue_on_error)
                .unwrap_or(false);
            let Some(outcome) = completed.get(*n) else {
                return false;
            };
            !graph
                .edges
                .iter()
                .filter(|e| &e.source == *n && !edge_is_back(graph, e))
                .any(|e| dag_forward_edge_fires(graph, e, outcome, coe).unwrap_or(false))
        })
        .cloned()
        .collect();
    terms.sort();
    terms
}

/// Assemble the terminal [`PipelineResult`] from accumulated DAG state.
///
/// * `success`: `Some(s)` for explicit early-exit paths (hard error, budget,
///   goal gate); `None` to DERIVE it from the terminal nodes — the run
///   succeeds iff there is ≥1 terminal and every terminal settled `Pass`.
/// * `override_output`: explicit output for early-exit paths; `None` joins the
///   terminal nodes' content.
#[allow(clippy::too_many_arguments)] // cohesive result-builder; splitting hurts clarity
fn dag_build_result(
    success: Option<bool>,
    override_output: Option<String>,
    completed: &HashMap<String, NodeOutcome>,
    summaries: Vec<NodeSummary>,
    total_tokens: TokenUsage,
    node_costs: Vec<NodeCost>,
    graph: &PipelineGraph,
    had_failure_prune: bool,
) -> PipelineResult {
    let mut all_files: Vec<std::path::PathBuf> = Vec::new();
    for o in completed.values() {
        all_files.extend(o.files_modified.iter().cloned());
    }
    all_files.sort();
    all_files.dedup();

    let terminals = dag_terminal_nodes(graph, completed);

    // `None` => normal quiescent termination (success derived from terminals).
    let derived = success.is_none();
    let success = success.unwrap_or_else(|| {
        !terminals.is_empty()
            && terminals
                .iter()
                .filter_map(|n| completed.get(n))
                .all(|o| o.status == OutcomeStatus::Pass)
    });
    // A run that failure-pruned a required join did not complete a required
    // path, so it reports failure even if an unrelated terminal passed.
    let success = success && !had_failure_prune;

    let output = override_output.unwrap_or_else(|| {
        terminals
            .iter()
            .filter_map(|n| completed.get(n))
            .map(|o| o.content.as_str())
            .collect::<Vec<_>>()
            .join("\n\n---\n\n")
    });

    // A normally-terminated run preserves files written by completed nodes even
    // when the terminal outcome was Fail/guard-hit (parity with the legacy
    // no-outgoing terminal); only explicit early-exit failures (hard error /
    // budget / aborted registration) drop them.
    let files_modified = if success || derived {
        all_files
    } else {
        vec![]
    };

    PipelineResult {
        output,
        success,
        token_usage: total_tokens,
        node_summaries: summaries,
        files_modified,
        node_costs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{NodeOutcome, OutcomeStatus, PipelineNode};
    use crate::guard::{TimeoutGuard, TokenBudgetGuard};
    use crate::handler::Handler;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    #[test]
    fn fanout_worker_deadline_priority_and_clamp() {
        // No deadline/timeout → absolute ceiling (never `None`, so a worker can
        // never hang forever).
        let bare = PipelineNode {
            id: "w".into(),
            ..Default::default()
        };
        assert_eq!(
            fanout_worker_deadline(&bare),
            Duration::from_secs(MAX_FANOUT_WORKER_SECS)
        );

        // `timeout_secs` is used when no `deadline_secs`.
        let timed = PipelineNode {
            id: "w".into(),
            timeout_secs: Some(42),
            ..Default::default()
        };
        assert_eq!(fanout_worker_deadline(&timed), Duration::from_secs(42));

        // `deadline_secs` WINS over `timeout_secs`.
        let both = PipelineNode {
            id: "w".into(),
            deadline_secs: Some(7.5),
            timeout_secs: Some(42),
            ..Default::default()
        };
        assert_eq!(fanout_worker_deadline(&both), Duration::from_secs_f64(7.5));

        // Non-finite / non-positive deadline_secs falls through to timeout_secs.
        let bad_deadline = PipelineNode {
            id: "w".into(),
            deadline_secs: Some(0.0),
            timeout_secs: Some(9),
            ..Default::default()
        };
        assert_eq!(
            fanout_worker_deadline(&bad_deadline),
            Duration::from_secs(9)
        );

        // A pathological over-cap value is clamped to the absolute ceiling.
        let over = PipelineNode {
            id: "w".into(),
            timeout_secs: Some(MAX_FANOUT_WORKER_SECS * 100),
            ..Default::default()
        };
        assert_eq!(
            fanout_worker_deadline(&over),
            Duration::from_secs(MAX_FANOUT_WORKER_SECS)
        );
    }

    #[test]
    fn dag_schedulable_excludes_fanout_and_converge() {
        // Plain static graphs schedule on the DAG path.
        let linear = crate::parser::parse_dot(
            "digraph d { a [handler=codergen, tools=read_file]; \
             b [handler=codergen, tools=read_file]; a -> b }",
        )
        .unwrap();
        assert!(graph_is_dag_schedulable(&linear));

        // Runtime fan-out (converge + dynamic_parallel) needs the legacy walk.
        let fanout = crate::parser::parse_dot(
            "digraph p { s [handler=dynamic_parallel, prompt=\"plan\", converge=\"m\"]; \
             m [handler=codergen, tools=read_file]; s -> m }",
        )
        .unwrap();
        assert!(!graph_is_dag_schedulable(&fanout));

        // A normal retry loop (back-edge target `work` has a forward pred
        // `start`) is schedulable.
        let retry = crate::parser::parse_dot(
            "digraph r { start [handler=codergen, tools=read_file]; \
             work [handler=codergen, tools=read_file]; \
             check [handler=codergen, tools=read_file]; \
             start -> work; work -> check; \
             check -> work [label=\"back_edge\", condition=\"outcome.status == \\\"fail\\\"\"] }",
        )
        .unwrap();
        assert!(graph_is_dag_schedulable(&retry));

        // A back-edge-only target (`work` reachable solely via the back-edge,
        // no forward predecessor) is a spurious root → legacy.
        let spurious = crate::parser::parse_dot(
            "digraph b { start [handler=codergen, tools=read_file]; \
             check [handler=codergen, tools=read_file]; \
             work [handler=codergen, tools=read_file]; \
             start -> check; work -> check; \
             check -> work [label=\"back_edge\", condition=\"outcome.status == \\\"fail\\\"\"] }",
        )
        .unwrap();
        assert!(!graph_is_dag_schedulable(&spurious));

        // A retry edge back to the START node is valid (start is the root).
        let retry_to_start = crate::parser::parse_dot(
            "digraph s { start [handler=codergen, tools=read_file]; \
             check [handler=codergen, tools=read_file]; \
             start -> check; \
             check -> start [label=\"back_edge\", condition=\"outcome.status == \\\"fail\\\"\"] }",
        )
        .unwrap();
        assert!(graph_is_dag_schedulable(&retry_to_start));
    }

    #[test]
    fn dag_forward_edge_fail_closed_on_unconditional_fail() {
        let pass = NodeOutcome {
            node_id: "x".into(),
            status: OutcomeStatus::Pass,
            content: String::new(),
            token_usage: TokenUsage::default(),
            files_modified: vec![],
        };
        let fail = NodeOutcome {
            status: OutcomeStatus::Fail,
            ..pass.clone()
        };
        let graph = crate::parser::parse_dot(
            "digraph g { x [handler=codergen, tools=read_file]; \
             y [handler=codergen, tools=read_file]; x -> y }",
        )
        .unwrap();
        let edge = &graph.edges[0];
        // Unconditional edge fires on Pass, NOT on Fail (fail-closed).
        assert!(dag_forward_edge_fires(&graph, edge, &pass, false).unwrap());
        assert!(!dag_forward_edge_fires(&graph, edge, &fail, false).unwrap());
    }

    #[test]
    fn test_edge_selection_condition_match() {
        let graph = crate::parser::parse_dot(
            r#"
            digraph test {
                a [prompt="test"]
                b [prompt="test"]
                c [prompt="test"]
                a -> b [condition="outcome.status == \"pass\""]
                a -> c [condition="outcome.status == \"fail\""]
            }
            "#,
        )
        .unwrap();

        let executor = PipelineExecutor::new(make_test_config());
        let outcome = NodeOutcome {
            node_id: "a".into(),
            status: OutcomeStatus::Pass,
            content: String::new(),
            token_usage: TokenUsage::default(),
            files_modified: vec![],
        };

        let next = executor.select_next_edge(&graph, "a", &outcome).unwrap();
        assert_eq!(next, Some("b".into()));
    }

    #[test]
    fn test_edge_selection_weight_tiebreak() {
        let graph = crate::parser::parse_dot(
            r#"
            digraph test {
                a -> b [weight="2.0"]
                a -> c [weight="1.0"]
            }
            "#,
        )
        .unwrap();

        let executor = PipelineExecutor::new(make_test_config());
        let outcome = NodeOutcome {
            node_id: "a".into(),
            status: OutcomeStatus::Pass,
            content: String::new(),
            token_usage: TokenUsage::default(),
            files_modified: vec![],
        };

        let next = executor.select_next_edge(&graph, "a", &outcome).unwrap();
        assert_eq!(next, Some("b".into()));
    }

    fn make_test_config() -> ExecutorConfig {
        // Minimal config for edge selection tests (doesn't actually run agents)
        ExecutorConfig {
            default_provider: Arc::new(MockProvider),
            provider_router: None,
            memory: Arc::new(
                tokio::runtime::Runtime::new()
                    .unwrap()
                    .block_on(create_test_store()),
            ),
            working_dir: PathBuf::from("/tmp"),
            provider_policy: None,
            plugin_dirs: vec![],
            plugin_require_signed: false,
            status_bridge: None,
            shutdown: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            max_parallel_workers: 8,
            max_pipeline_fanout_total: None,
            guards: Vec::new(),
            max_concurrent_llm_calls: None,
            checkpoint_store: None,
            hook_executor: None,
            workspace_context: crate::context::PipelineContext::default(),
            host_context: crate::host_context::PipelineHostContext::default(),
            embedder: None,
            catalog_dir: None,
            sandbox: octos_agent::SandboxConfig::default(),
        }
    }

    /// #1607 (codex-review follow-up): `run_terminal_validators` /
    /// `run_node_validators` build their workspace-scoped validator registry
    /// with `with_builtins_and_sandbox(&self.config.working_dir,
    /// create_sandbox(&self.config.sandbox))`. Lock in that the sandbox
    /// threaded onto `ExecutorConfig` reaches that registry (i.e. NOT the
    /// pre-fix hardcoded `with_builtins` / `NoSandbox`). Docker mode is chosen
    /// because `create_sandbox` returns a `DockerSandbox` unconditionally
    /// (no docker binary required), so the assertion is host-independent.
    #[test]
    fn pipeline_threads_configured_sandbox_into_validator_registry() {
        let mut config = make_test_config();
        config.sandbox = octos_agent::SandboxConfig {
            mode: octos_agent::SandboxMode::Docker,
            ..octos_agent::SandboxConfig::default()
        };
        // Reconstruct exactly what the two validator blocks build.
        let registry = octos_agent::ToolRegistry::with_builtins_and_sandbox(
            &config.working_dir,
            octos_agent::create_sandbox(&config.sandbox),
        );
        let sandbox = registry.sandbox();
        assert!(
            sandbox.is_docker(),
            "pipeline validator registry must inherit the ExecutorConfig \
             sandbox (Docker here), not the pre-#1607 hardcoded NoSandbox"
        );
        assert!(
            !sandbox.is_noop(),
            "a real backend threaded onto ExecutorConfig must not be a no-op"
        );
    }

    /// #1607: an explicit `SandboxMode::None` on `ExecutorConfig` resolves to a
    /// no-op backend, so command validators run the argv directly (pre-#1607
    /// behaviour on a host without a configured backend). Note: the STRUCT
    /// default is `SandboxMode::Auto`, which resolves to a REAL backend on
    /// macOS/Linux — so this test pins `None` explicitly to stay
    /// host-independent (mirrors `spawn_none_sandbox_registry_is_noop`).
    #[test]
    fn pipeline_none_sandbox_registry_is_noop() {
        let mut config = make_test_config();
        config.sandbox = octos_agent::SandboxConfig {
            mode: octos_agent::SandboxMode::None,
            ..octos_agent::SandboxConfig::default()
        };
        let registry = octos_agent::ToolRegistry::with_builtins_and_sandbox(
            &config.working_dir,
            octos_agent::create_sandbox(&config.sandbox),
        );
        assert!(
            registry.sandbox().is_noop(),
            "SandboxMode::None must resolve to a no-op backend so command \
             validators run directly (host-independent)"
        );
    }

    struct MockProvider;

    #[async_trait::async_trait]
    impl LlmProvider for MockProvider {
        async fn chat(
            &self,
            _messages: &[octos_core::Message],
            _tools: &[octos_llm::ToolSpec],
            _config: &octos_llm::ChatConfig,
        ) -> Result<octos_llm::ChatResponse> {
            Ok(octos_llm::ChatResponse {
                content: Some("done".into()),
                reasoning_content: None,
                tool_calls: vec![],
                stop_reason: octos_llm::StopReason::EndTurn,
                usage: octos_llm::TokenUsage {
                    input_tokens: 0,
                    output_tokens: 0,
                    ..Default::default()
                },
                provider_index: None,
            })
        }

        fn model_id(&self) -> &str {
            "mock"
        }

        fn provider_name(&self) -> &str {
            "mock"
        }
    }

    struct CountingProvider {
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl LlmProvider for CountingProvider {
        async fn chat(
            &self,
            _messages: &[octos_core::Message],
            _tools: &[octos_llm::ToolSpec],
            _config: &octos_llm::ChatConfig,
        ) -> Result<octos_llm::ChatResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(octos_llm::ChatResponse {
                content: Some("done".into()),
                reasoning_content: None,
                tool_calls: vec![],
                stop_reason: octos_llm::StopReason::EndTurn,
                usage: octos_llm::TokenUsage {
                    input_tokens: 0,
                    output_tokens: 0,
                    ..Default::default()
                },
                provider_index: None,
            })
        }

        fn model_id(&self) -> &str {
            "mock"
        }

        fn provider_name(&self) -> &str {
            "mock"
        }
    }

    #[test]
    fn validation_rejects_malformed_pipeline_before_llm_dispatch() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut config = make_test_config();
        config.default_provider = Arc::new(CountingProvider {
            calls: calls.clone(),
        });
        let executor = PipelineExecutor::new(config);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let err = runtime
            .block_on(executor.run(
                r#"
                digraph test {
                    start [prompt="Use {missing_runtime_binding}"]
                }
                "#,
                "input",
                &serde_json::Map::new(),
            ))
            .expect_err("unbound template variable must reject the pipeline");
        assert!(
            err.to_string().contains("T-Agent"),
            "unexpected validation error: {err}"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "validation must fail before any LLM call"
        );
    }

    async fn create_test_store() -> EpisodeStore {
        let dir = tempfile::tempdir().unwrap();
        let dir = Box::leak(Box::new(dir));
        EpisodeStore::open(dir.path()).await.unwrap()
    }

    #[test]
    fn defaults_pipeline_llm_throttle_to_four_permits() {
        let executor = PipelineExecutor::new(make_test_config());
        assert_eq!(
            executor.max_concurrent_llm_calls_for_test(),
            DEFAULT_PIPELINE_MAX_CONCURRENT_LLM_CALLS
        );
        assert_eq!(
            executor
                .build_codergen_for_test()
                .llm_available_permits_for_test(),
            Some(DEFAULT_PIPELINE_MAX_CONCURRENT_LLM_CALLS)
        );
    }

    #[test]
    fn honors_configured_pipeline_llm_throttle_and_clamps_zero() {
        let mut config = make_test_config();
        config.max_concurrent_llm_calls = Some(2);
        let executor = PipelineExecutor::new(config);
        assert_eq!(executor.max_concurrent_llm_calls_for_test(), 2);
        assert_eq!(
            executor
                .build_codergen_for_test()
                .llm_available_permits_for_test(),
            Some(2)
        );

        let mut config = make_test_config();
        config.max_concurrent_llm_calls = Some(0);
        let executor = PipelineExecutor::new(config);
        assert_eq!(executor.max_concurrent_llm_calls_for_test(), 1);
        assert_eq!(
            executor
                .build_codergen_for_test()
                .llm_available_permits_for_test(),
            Some(1)
        );
    }

    #[test]
    fn caps_codergen_output_tokens_by_remaining_pipeline_budget() {
        let mut node = PipelineNode {
            handler: HandlerKind::Codergen,
            ..Default::default()
        };
        cap_node_output_tokens_for_remaining_budget(&mut node, 900, 3);
        assert_eq!(node.max_output_tokens, Some(300));

        node.max_output_tokens = Some(100);
        cap_node_output_tokens_for_remaining_budget(&mut node, 900, 3);
        assert_eq!(node.max_output_tokens, Some(100));
    }

    #[test]
    fn leaves_non_llm_nodes_uncapped_by_pipeline_budget() {
        let mut node = PipelineNode {
            handler: HandlerKind::Shell,
            max_output_tokens: Some(500),
            ..Default::default()
        };
        cap_node_output_tokens_for_remaining_budget(&mut node, 900, 3);
        assert_eq!(node.max_output_tokens, Some(500));
    }

    // --- extract_json_array tests ---

    #[test]
    fn test_extract_json_array_direct() {
        let input = r#"[{"task": "a", "label": "A"}]"#;
        assert_eq!(extract_json_array(input), Some(input));
    }

    #[test]
    fn test_extract_json_array_with_code_fence() {
        let input = "```json\n[{\"task\": \"a\"}]\n```";
        assert_eq!(extract_json_array(input), Some("[{\"task\": \"a\"}]"));
    }

    #[test]
    fn test_extract_json_array_with_narrative() {
        let input =
            "Here are [the angles] I recommend:\n[{\"task\": \"search\", \"label\": \"L\"}]";
        let result = extract_json_array(input).unwrap();
        assert!(result.starts_with("[{"));
        assert!(result.ends_with(']'));
    }

    #[test]
    fn test_extract_json_array_no_array() {
        assert_eq!(extract_json_array("no json here"), None);
    }

    #[test]
    fn test_extract_json_array_bare_brackets_no_object() {
        // Bare brackets without `{` should not match
        assert_eq!(extract_json_array("see [this] for details"), None);
    }

    #[test]
    fn test_extract_json_array_whitespace() {
        let input = "  \n  [{\"task\": \"x\"}]  \n  ";
        assert_eq!(extract_json_array(input), Some("[{\"task\": \"x\"}]"));
    }

    // --- DynamicTask deserialization tests ---

    #[test]
    fn test_dynamic_task_full() {
        let json = r#"{"task": "search for X", "label": "Primary"}"#;
        let t: DynamicTask = serde_json::from_str(json).unwrap();
        assert_eq!(t.task, "search for X");
        assert_eq!(t.label.as_deref(), Some("Primary"));
    }

    #[test]
    fn test_dynamic_task_no_label() {
        let json = r#"{"task": "search for Y"}"#;
        let t: DynamicTask = serde_json::from_str(json).unwrap();
        assert_eq!(t.task, "search for Y");
        assert!(t.label.is_none());
    }

    #[test]
    fn test_dynamic_task_extra_fields_ignored() {
        let json = r#"{"task": "search", "label": "L", "extra": 42}"#;
        let t: DynamicTask = serde_json::from_str(json).unwrap();
        assert_eq!(t.task, "search");
    }

    #[test]
    fn test_dynamic_task_array() {
        let json = r#"[{"task": "a", "label": "A"}, {"task": "b"}]"#;
        let tasks: Vec<DynamicTask> = serde_json::from_str(json).unwrap();
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].task, "a");
        assert_eq!(tasks[1].label, None);
    }

    // --- fallback_tasks tests ---

    #[test]
    fn test_fallback_tasks_count() {
        let tasks = fallback_tasks("test query");
        assert_eq!(tasks.len(), 3);
        assert!(tasks.iter().all(|t| t.label.is_some()));
        assert!(tasks[0].task.contains("test query"));
    }

    /// Build a fresh ExecutorConfig identical to `make_test_config` but
    /// with a per-test cumulative fan-out cap so Guard B fires on a
    /// small synthetic graph instead of waiting for 500 dispatches.
    async fn make_capped_config(cap: usize) -> ExecutorConfig {
        ExecutorConfig {
            default_provider: Arc::new(MockProvider),
            provider_router: None,
            memory: Arc::new(create_test_store().await),
            working_dir: PathBuf::from("/tmp"),
            provider_policy: None,
            plugin_dirs: vec![],
            plugin_require_signed: false,
            status_bridge: None,
            shutdown: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            max_parallel_workers: 8,
            max_pipeline_fanout_total: Some(cap),
            guards: Vec::new(),
            max_concurrent_llm_calls: None,
            checkpoint_store: None,
            hook_executor: None,
            workspace_context: crate::context::PipelineContext::default(),
            host_context: crate::host_context::PipelineHostContext::default(),
            embedder: None,
            catalog_dir: None,
            sandbox: octos_agent::SandboxConfig::default(),
        }
    }

    struct TokenHandler {
        calls: Arc<AtomicUsize>,
        input_tokens: u32,
        output_tokens: u32,
    }

    #[async_trait::async_trait]
    impl Handler for TokenHandler {
        async fn execute(&self, node: &PipelineNode, _ctx: &HandlerContext) -> Result<NodeOutcome> {
            self.calls.fetch_add(1, AtomicOrdering::Relaxed);
            Ok(NodeOutcome {
                node_id: node.id.clone(),
                status: OutcomeStatus::Pass,
                content: format!("{} complete", node.id),
                token_usage: TokenUsage {
                    input_tokens: self.input_tokens,
                    output_tokens: self.output_tokens,
                    ..Default::default()
                },
                files_modified: vec![],
            })
        }
    }

    struct NodeRecordingGuard {
        seen: Arc<Mutex<Vec<String>>>,
    }

    impl PipelineGuard for NodeRecordingGuard {
        fn before_node(&self, ctx: &GuardContext<'_>) -> Result<GuardDecision> {
            self.seen.lock().unwrap().push(ctx.node.id.clone());
            Ok(GuardDecision::Allow)
        }
    }

    struct SelectiveGuard {
        target: &'static str,
        decision: GuardDecision,
    }

    impl PipelineGuard for SelectiveGuard {
        fn before_node(&self, ctx: &GuardContext<'_>) -> Result<GuardDecision> {
            if ctx.node.id == self.target {
                Ok(self.decision.clone())
            } else {
                Ok(GuardDecision::Allow)
            }
        }
    }

    struct OrderedGuard {
        name: &'static str,
        seen: Arc<Mutex<Vec<String>>>,
        decision: GuardDecision,
    }

    impl PipelineGuard for OrderedGuard {
        fn before_node(&self, ctx: &GuardContext<'_>) -> Result<GuardDecision> {
            self.seen.lock().unwrap().push(format!(
                "{}:{}:{}:{}:{}",
                self.name,
                ctx.node.id,
                ctx.cumulative_tokens,
                ctx.completed_count,
                ctx.visit_counts
                    .get(&ctx.node.id)
                    .copied()
                    .unwrap_or_default()
            ));
            Ok(self.decision.clone())
        }
    }

    struct ErrorGuard;

    impl PipelineGuard for ErrorGuard {
        fn before_node(&self, _ctx: &GuardContext<'_>) -> Result<GuardDecision> {
            Err(eyre::eyre!("guard storage unavailable"))
        }
    }

    #[tokio::test]
    async fn token_budget_guard_aborts_before_next_node_with_partial_result() {
        let mut config = make_capped_config(10).await;
        config.guards = vec![Arc::new(TokenBudgetGuard::new(7)) as Arc<dyn PipelineGuard>];
        let executor = PipelineExecutor::new(config);

        let calls = Arc::new(AtomicUsize::new(0));
        let mut handlers = HandlerRegistry::new();
        handlers.register(
            HandlerKind::Noop,
            Arc::new(TokenHandler {
                calls: calls.clone(),
                input_tokens: 4,
                output_tokens: 3,
            }),
        );

        let dot = r#"
            digraph t {
                a [handler="noop"]
                b [handler="noop"]
                a -> b
            }
        "#;

        let result = executor
            .run_with_handlers(dot, "seed", &serde_json::Map::new(), handlers)
            .await
            .expect("pipeline should return a partial result");

        assert!(!result.success);
        assert!(
            result
                .output
                .contains("token budget exhausted before node 'b'"),
            "unexpected output: {}",
            result.output
        );
        assert_eq!(calls.load(AtomicOrdering::Relaxed), 1);
        assert_eq!(result.node_summaries.len(), 1);
        assert_eq!(result.node_summaries[0].node_id, "a");
        assert_eq!(result.token_usage.input_tokens, 4);
        assert_eq!(result.token_usage.output_tokens, 3);
    }

    #[tokio::test]
    async fn timeout_guard_aborts_before_dispatch() {
        let mut config = make_capped_config(10).await;
        config.guards = vec![Arc::new(TimeoutGuard::new(Duration::ZERO)) as Arc<dyn PipelineGuard>];
        let executor = PipelineExecutor::new(config);

        let result = executor
            .run(
                r#"digraph t { a [handler="noop"] }"#,
                "seed",
                &serde_json::Map::new(),
            )
            .await
            .expect("timeout guard should return a partial result");

        assert!(!result.success);
        assert!(result.node_summaries.is_empty());
        assert!(
            result.output.contains("pipeline timeout before node 'a'"),
            "unexpected output: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn guard_skip_records_fail_outcome_for_edge_routing() {
        let mut config = make_capped_config(10).await;
        config.guards = vec![Arc::new(SelectiveGuard {
            target: "gate",
            decision: GuardDecision::Skip("closed by policy".into()),
        }) as Arc<dyn PipelineGuard>];
        let executor = PipelineExecutor::new(config);

        let dot = r#"
            digraph t {
                gate [handler="noop"]
                fallback [handler="noop"]
                bad [handler="noop"]
                gate -> fallback [condition="outcome.status == \"fail\""]
                gate -> bad [condition="outcome.status == \"pass\""]
            }
        "#;

        let result = executor
            .run(dot, "seed", &serde_json::Map::new())
            .await
            .expect("guard skip should route to fallback");

        assert!(result.success, "fallback noop should recover the pipeline");
        assert_eq!(result.node_summaries[0].node_id, "gate");
        assert!(!result.node_summaries[0].success);
        assert!(
            result
                .node_summaries
                .iter()
                .any(|s| s.node_id == "fallback")
        );
        assert!(!result.node_summaries.iter().any(|s| s.node_id == "bad"));
        assert!(
            result
                .output
                .contains("Node 'gate' skipped by pipeline guard: closed by policy"),
            "unexpected output: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn guards_run_in_registration_order_and_short_circuit() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut config = make_capped_config(10).await;
        config.guards = vec![
            Arc::new(OrderedGuard {
                name: "first",
                seen: seen.clone(),
                decision: GuardDecision::Allow,
            }) as Arc<dyn PipelineGuard>,
            Arc::new(OrderedGuard {
                name: "second",
                seen: seen.clone(),
                decision: GuardDecision::Abort("stop here".into()),
            }) as Arc<dyn PipelineGuard>,
            Arc::new(OrderedGuard {
                name: "third",
                seen: seen.clone(),
                decision: GuardDecision::Allow,
            }) as Arc<dyn PipelineGuard>,
        ];
        let executor = PipelineExecutor::new(config);

        let result = executor
            .run(
                r#"digraph t { a [handler="noop"] }"#,
                "seed",
                &serde_json::Map::new(),
            )
            .await
            .expect("guard abort should return partial result");

        assert!(!result.success);
        assert_eq!(
            *seen.lock().unwrap(),
            vec!["first:a:0:0:1".to_string(), "second:a:0:0:1".to_string()]
        );
    }

    #[tokio::test]
    async fn guard_errors_abort_instead_of_allowing_dispatch() {
        let mut config = make_capped_config(10).await;
        config.guards = vec![Arc::new(ErrorGuard) as Arc<dyn PipelineGuard>];
        let executor = PipelineExecutor::new(config);

        let calls = Arc::new(AtomicUsize::new(0));
        let mut handlers = HandlerRegistry::new();
        handlers.register(
            HandlerKind::Noop,
            Arc::new(TokenHandler {
                calls: calls.clone(),
                input_tokens: 0,
                output_tokens: 0,
            }),
        );

        let result = executor
            .run_with_handlers(
                r#"digraph t { a [handler="noop"] }"#,
                "seed",
                &serde_json::Map::new(),
                handlers,
            )
            .await
            .expect("guard error should return partial result");

        assert!(!result.success);
        assert_eq!(calls.load(AtomicOrdering::Relaxed), 0);
        assert!(
            result
                .output
                .contains("guard error: guard storage unavailable"),
            "unexpected output: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn guards_run_once_for_static_parallel_before_worker_spawn() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut config = make_capped_config(10).await;
        config.guards =
            vec![Arc::new(NodeRecordingGuard { seen: seen.clone() }) as Arc<dyn PipelineGuard>];
        let executor = PipelineExecutor::new(config);

        let dot = r#"
            digraph t {
                fan [handler="parallel", converge="merge"]
                a [handler="noop"]
                b [handler="noop"]
                merge [handler="noop"]
                fan -> a
                fan -> b
                a -> merge
                b -> merge
            }
        "#;

        let result = executor
            .run(dot, "seed", &serde_json::Map::new())
            .await
            .expect("parallel pipeline should complete");
        assert!(result.success);

        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.iter().filter(|node| node.as_str() == "fan").count(), 1);
        assert!(!seen.iter().any(|node| node == "a"));
        assert!(!seen.iter().any(|node| node == "b"));
        assert!(seen.iter().any(|node| node == "merge"));
    }

    #[tokio::test]
    async fn guards_run_once_for_dynamic_parallel_before_worker_spawn() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut config = make_capped_config(10).await;
        config.guards =
            vec![Arc::new(NodeRecordingGuard { seen: seen.clone() }) as Arc<dyn PipelineGuard>];
        let executor = PipelineExecutor::new(config);

        let mut handlers = HandlerRegistry::new();
        handlers.register(HandlerKind::Codergen, Arc::new(NoopHandler));
        handlers.register(HandlerKind::DynamicParallel, Arc::new(NoopHandler));
        handlers.register(HandlerKind::Noop, Arc::new(NoopHandler));

        let dot = r#"
            digraph t {
                plan [handler="dynamic_parallel", converge="merge", prompt="plan"]
                merge [handler="noop"]
                plan -> merge
            }
        "#;

        let result = executor
            .run_with_handlers(dot, "seed", &serde_json::Map::new(), handlers)
            .await
            .expect("dynamic parallel pipeline should complete");
        assert!(result.success);

        let seen = seen.lock().unwrap().clone();
        assert_eq!(
            seen.iter().filter(|node| node.as_str() == "plan").count(),
            1
        );
        assert!(!seen.iter().any(|node| node.starts_with("plan_task_")));
        assert!(seen.iter().any(|node| node == "merge"));
    }

    /// Guard B regression: a `dynamic_parallel` node whose worker count
    /// exceeds the cumulative fan-out cap must fail the pipeline with
    /// `PipelineError::FanoutExceeded` before any worker dispatches.
    /// The test forces the planner to fall back to the 3-task fallback
    /// (the `MockProvider` returns plain "done" which fails JSON
    /// extraction) and sets the cap to 2 so the fan-out trips.
    #[tokio::test]
    async fn dynamic_parallel_fails_after_cumulative_cap() {
        let config = make_capped_config(2).await;
        let executor = PipelineExecutor::new(config);

        // Minimal dynamic_parallel graph. The planner is the
        // MockProvider, which returns content "done" — that fails JSON
        // extraction and routes through the 3-task fallback. With
        // cap=2 the fan-out gate refuses before any worker dispatches.
        let dot = r#"
            digraph t {
                plan [handler="dynamic_parallel", converge="merge", prompt="plan"]
                merge [handler="noop"]
                plan -> merge
            }
        "#;

        let result = executor
            .run(dot, "drive a runaway plan", &serde_json::Map::new())
            .await;

        let Err(error) = result else {
            panic!("expected pipeline to fail at the fan-out cap; got {result:?}");
        };
        // The structured `PipelineError::FanoutExceeded` is wrapped in
        // an `eyre::Report` — downcast to assert the typed reason.
        let typed = error
            .downcast_ref::<PipelineError>()
            .expect("expected PipelineError variant in failure chain");
        match typed {
            PipelineError::FanoutExceeded { count, cap } => {
                assert_eq!(*cap, 2, "cap should match the per-test override");
                assert_eq!(*count, 0, "no workers should dispatch before the cap fires");
            }
        }
    }

    /// Planner provider for the dynamic fan-out concurrency test: returns a
    /// JSON array of exactly 6 tasks so the `dynamic_parallel` node plans
    /// MORE workers than `max_parallel_workers`.
    struct SixTaskPlanner;

    #[async_trait::async_trait]
    impl LlmProvider for SixTaskPlanner {
        async fn chat(
            &self,
            _messages: &[octos_core::Message],
            _tools: &[octos_llm::ToolSpec],
            _config: &octos_llm::ChatConfig,
        ) -> Result<octos_llm::ChatResponse> {
            Ok(octos_llm::ChatResponse {
                content: Some(
                    r#"[{"task":"t1","label":"T1"},{"task":"t2","label":"T2"},
                        {"task":"t3","label":"T3"},{"task":"t4","label":"T4"},
                        {"task":"t5","label":"T5"},{"task":"t6","label":"T6"}]"#
                        .to_string(),
                ),
                reasoning_content: None,
                tool_calls: vec![],
                stop_reason: octos_llm::StopReason::EndTurn,
                usage: octos_llm::TokenUsage::default(),
                provider_index: None,
            })
        }

        fn model_id(&self) -> &str {
            "mock-planner"
        }

        fn provider_name(&self) -> &str {
            "mock"
        }
    }

    /// Codergen stand-in that records the peak number of concurrently
    /// executing fan-out workers: increment an in-flight counter on entry,
    /// fold it into the running maximum, hold the slot across a real await,
    /// decrement on exit.
    struct ConcurrencyProbeHandler {
        in_flight: Arc<AtomicUsize>,
        max_in_flight: Arc<AtomicUsize>,
        executed: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl Handler for ConcurrencyProbeHandler {
        async fn execute(&self, node: &PipelineNode, _ctx: &HandlerContext) -> Result<NodeOutcome> {
            let now = self.in_flight.fetch_add(1, AtomicOrdering::SeqCst) + 1;
            self.max_in_flight.fetch_max(now, AtomicOrdering::SeqCst);
            // Hold the slot across an await point. `join_all` polls every
            // worker future once within microseconds, so 50ms guarantees all
            // concurrently-dispatched workers pile up before the first exits
            // — no racing needed for a deterministic peak.
            tokio::time::sleep(Duration::from_millis(50)).await;
            self.in_flight.fetch_sub(1, AtomicOrdering::SeqCst);
            self.executed.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(NodeOutcome {
                node_id: node.id.clone(),
                status: OutcomeStatus::Pass,
                content: format!("{} done", node.id),
                token_usage: TokenUsage::default(),
                files_modified: vec![],
            })
        }
    }

    /// The dynamic fan-out must honor `max_parallel_workers` exactly like
    /// the static `Parallel` branch. A planner that yields 6 tasks with
    /// `max_parallel_workers = 2` may never have more than 2 workers
    /// in flight at once.
    #[tokio::test]
    async fn should_cap_dynamic_parallel_worker_concurrency_when_planner_exceeds_limit() {
        let mut config = make_capped_config(100).await;
        config.default_provider = Arc::new(SixTaskPlanner);
        config.max_parallel_workers = 2;
        let executor = PipelineExecutor::new(config);

        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_in_flight = Arc::new(AtomicUsize::new(0));
        let executed = Arc::new(AtomicUsize::new(0));

        let mut handlers = HandlerRegistry::new();
        handlers.register(
            HandlerKind::Codergen,
            Arc::new(ConcurrencyProbeHandler {
                in_flight: in_flight.clone(),
                max_in_flight: max_in_flight.clone(),
                executed: executed.clone(),
            }),
        );
        handlers.register(HandlerKind::DynamicParallel, Arc::new(NoopHandler));
        handlers.register(HandlerKind::Noop, Arc::new(NoopHandler));

        let dot = r#"
            digraph t {
                plan [handler="dynamic_parallel", converge="merge", prompt="plan"]
                merge [handler="noop"]
                plan -> merge
            }
        "#;

        let result = executor
            .run_with_handlers(dot, "seed", &serde_json::Map::new(), handlers)
            .await
            .expect("dynamic parallel pipeline should complete");
        assert!(result.success);

        // The planner JSON must have driven the fan-out (6 workers), not the
        // 3-task fallback — otherwise the concurrency assertion below tests
        // a weaker shape than intended.
        assert_eq!(
            executed.load(AtomicOrdering::SeqCst),
            6,
            "expected all 6 planned workers to run"
        );
        let peak = max_in_flight.load(AtomicOrdering::SeqCst);
        assert!(
            peak <= 2,
            "dynamic_parallel fan-out must gate workers to max_parallel_workers=2, \
             but {peak} ran concurrently"
        );
    }

    /// Guard B sanity check: when the fan-out is below the cap the
    /// pipeline executes normally. Static `Parallel` graph with two
    /// noop targets and cap=4 — well within budget.
    #[tokio::test]
    async fn parallel_under_cap_runs_to_completion() {
        let config = make_capped_config(4).await;
        let executor = PipelineExecutor::new(config);

        let dot = r#"
            digraph t {
                fan [handler="parallel", converge="merge"]
                a [handler="noop"]
                b [handler="noop"]
                merge [handler="noop"]
                fan -> a
                fan -> b
                a -> merge
                b -> merge
            }
        "#;

        let result = executor
            .run(dot, "happy path", &serde_json::Map::new())
            .await;
        assert!(
            result.is_ok(),
            "fan-out below cap should complete: {result:?}"
        );
    }

    // ── L2 typed-IR execution (S1-3) ───────────────────────────────────

    /// A composed typed-IR program executes through `run_graph_with_handlers`
    /// without ever round-tripping through DOT text.
    #[tokio::test]
    async fn run_ir_executes_composed_graph_without_dot_roundtrip() {
        let executor = PipelineExecutor::new(make_capped_config(4).await);
        let ir = r#"{"id":"p","nodes":[{"id":"g","kind":{"type":"gate"}}]}"#;
        let result = executor
            .run_ir(
                ir,
                &crate::profile::ValidationProfile::l2_default(),
                "hi",
                &serde_json::Map::new(),
            )
            .await;
        assert!(result.is_ok(), "composed gate graph should run: {result:?}");
    }

    /// Compose-time failures surface as an error before any execution begins.
    #[tokio::test]
    async fn run_ir_surfaces_compose_errors_before_execution() {
        let executor = PipelineExecutor::new(make_capped_config(4).await);
        let bad = r#"{"id":"p","nodes":[{"id":"n","kind":{"type":"shell"}}]}"#;
        let err = executor
            .run_ir(
                bad,
                &crate::profile::ValidationProfile::l2_default(),
                "x",
                &serde_json::Map::new(),
            )
            .await
            .expect_err("unknown palette kind must fail at compose");
        assert!(err.to_string().contains("compose"), "got: {err}");
    }

    /// Real END-TO-END: compile an L2 typed-IR "deep research" workflow and
    /// EXECUTE it against a live DeepSeek model (not MockProvider). Env-gated +
    /// `#[ignore]` so normal CI skips it. Run with:
    ///   DEEPSEEK_API_KEY=... cargo test -p octos-pipeline \
    ///     run_ir_e2e_deepseek_real -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "needs DEEPSEEK_API_KEY + network"]
    async fn run_ir_e2e_deepseek_real() {
        let Ok(key) = std::env::var("DEEPSEEK_API_KEY") else {
            eprintln!("SKIP: DEEPSEEK_API_KEY not set");
            return;
        };
        let provider: Arc<dyn LlmProvider> = Arc::new(
            octos_llm::openai::OpenAIProvider::new(key, "deepseek-chat")
                .with_base_url("https://api.deepseek.com/v1"),
        );
        let mut config = make_capped_config(4).await;
        config.default_provider = provider;
        let executor = PipelineExecutor::new(config);

        // A small but real "deep research" IR: research -> synthesize report.
        let ir = r#"{
            "id": "deep_research_e2e",
            "nodes": [
                {"id":"research","kind":{"type":"research","prompt":"Briefly research the current state of Rust async runtimes (Tokio, smol, io_uring runtimes). List 4-5 concrete factual points. Keep it under 120 words."}},
                {"id":"report","kind":{"type":"synthesize","prompt":"Using the prior research findings, write a tight ~150-word summary report with a title, in markdown."}}
            ],
            "edges": [ {"source":"research","target":"report"} ]
        }"#;

        let result = executor
            .run_ir(
                ir,
                &crate::profile::ValidationProfile::l2_default(),
                "Rust async runtimes, 2026",
                &serde_json::Map::new(),
            )
            .await;

        match &result {
            Ok(r) => {
                eprintln!(
                    "=== e2e success={} nodes_run={} ===",
                    r.success,
                    r.node_summaries.len()
                );
                eprintln!("=== FINAL OUTPUT ===\n{}\n=== END ===", r.output);
            }
            Err(e) => eprintln!("=== e2e ERROR ===\n{e:?}"),
        }
        let r = result.expect("composed IR pipeline should execute end-to-end");
        assert!(r.success, "pipeline should succeed");
        assert!(!r.output.trim().is_empty(), "should produce report output");
    }

    // ── Heartbeat (#964 follow-up) ─────────────────────────────────────
    //
    // Verifies that `spawn_pipeline_heartbeat` ticks at the configured
    // interval, reads the shared `PipelineStatusSnapshot` each tick, and
    // emits `ProgressEvent::ToolProgress` events through the captured
    // reporter. The guard's `Drop` aborts the task so it doesn't outlive
    // the surrounding `run_with_handlers` call.

    /// Capturing reporter — collects every emitted `ProgressEvent` into a
    /// `Vec` so the test can assert on the messages.
    #[derive(Default, Clone)]
    struct CapturingReporter {
        events: Arc<std::sync::Mutex<Vec<octos_agent::progress::ProgressEvent>>>,
    }

    impl octos_agent::progress::ProgressReporter for CapturingReporter {
        fn report(&self, event: octos_agent::progress::ProgressEvent) {
            if let Ok(mut g) = self.events.lock() {
                g.push(event);
            }
        }
    }

    #[tokio::test]
    async fn heartbeat_emits_periodic_progress_with_current_node() {
        let reporter = CapturingReporter::default();
        let captured = reporter.events.clone();

        let ctx = octos_agent::tools::ToolContext {
            tool_id: "tc-heartbeat".to_string(),
            reporter: Arc::new(reporter),
            ..octos_agent::tools::ToolContext::zero()
        };

        let status = Arc::new(std::sync::Mutex::new(PipelineStatusSnapshot {
            pipeline_id: "research".to_string(),
            current_node: "plan_and_search".to_string(),
            nodes_done: 0,
            nodes_total: 3,
            start: Instant::now(),
        }));

        // Run the heartbeat inside TOOL_CTX.scope so the spawn helper can
        // capture reporter + tool_id synchronously. The 1s interval keeps
        // the test fast while still proving the periodic shape.
        let status_for_advance = status.clone();
        TOOL_CTX
            .scope(ctx, async move {
                let _guard = spawn_pipeline_heartbeat(status_for_advance.clone(), 1)
                    .expect("heartbeat should spawn when TOOL_CTX is set");
                // Wait long enough for ≥2 ticks: first tick is consumed
                // by `interval.tick().await` (the skip-immediate guard),
                // the next two fire at +1s and +2s. Sleep 2.4s real time.
                tokio::time::sleep(Duration::from_millis(2_400)).await;

                // Update the snapshot mid-flight so the next tick
                // reflects the new node — guards against a stale snapshot
                // baked at spawn time.
                if let Ok(mut g) = status_for_advance.lock() {
                    g.current_node = "analyze".to_string();
                    g.nodes_done = 1;
                }
                tokio::time::sleep(Duration::from_millis(1_100)).await;
                // Guard drops here — heartbeat task aborts.
            })
            .await;

        let events = captured.lock().unwrap();
        // Expect ≥2 ticks (sleep 2.4s skips first immediate tick, then
        // fires at +1s and +2s) plus possibly +3.5s for the post-update
        // tick. Lower bound: 2.
        assert!(
            events.len() >= 2,
            "expected ≥2 heartbeat events in 3.5s; got {}: {:?}",
            events.len(),
            events,
        );

        let messages: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                octos_agent::progress::ProgressEvent::ToolProgress { message, .. } => {
                    Some(message.clone())
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            messages.len(),
            events.len(),
            "heartbeat must emit ToolProgress events only — got: {:?}",
            events,
        );

        let combined = messages.join("\n");
        assert!(
            combined.contains("research"),
            "heartbeat must include the pipeline id; got: {combined}",
        );
        assert!(
            combined.contains("plan_and_search") || combined.contains("analyze"),
            "heartbeat must surface the current_node from the snapshot; got: {combined}",
        );
        // Each tick should also include an elapsed-seconds suffix so
        // every message is unique — protects against SPA dedup-by-message
        // that would otherwise collapse identical chips.
        assert!(
            combined.contains("s elapsed"),
            "heartbeat message must contain '<N>s elapsed'; got: {combined}",
        );
    }

    #[tokio::test]
    async fn heartbeat_guard_drop_stops_emission() {
        let reporter = CapturingReporter::default();
        let captured = reporter.events.clone();

        let ctx = octos_agent::tools::ToolContext {
            tool_id: "tc-heartbeat-stop".to_string(),
            reporter: Arc::new(reporter),
            ..octos_agent::tools::ToolContext::zero()
        };

        let status = Arc::new(std::sync::Mutex::new(PipelineStatusSnapshot {
            pipeline_id: "p".to_string(),
            current_node: "n".to_string(),
            nodes_done: 0,
            nodes_total: 1,
            start: Instant::now(),
        }));

        TOOL_CTX
            .scope(ctx, async move {
                {
                    let _guard = spawn_pipeline_heartbeat(status.clone(), 1).unwrap();
                    tokio::time::sleep(Duration::from_millis(1_200)).await;
                    // _guard drops here when block exits.
                }
                let count_at_drop = captured.lock().unwrap().len();
                // Sleep past 2 more theoretical tick intervals.
                tokio::time::sleep(Duration::from_millis(2_500)).await;
                let count_after_drop = captured.lock().unwrap().len();
                assert_eq!(
                    count_at_drop, count_after_drop,
                    "no new heartbeat events should fire after the guard drops; got {count_at_drop} -> {count_after_drop}",
                );
            })
            .await;
    }

    // ── Gap 4.2: structured per-node progress + ETA + previews ─────────

    /// Linear ETA: `(elapsed / done) * remaining`, with graceful degradation.
    #[test]
    fn linear_eta_degrades_then_extrapolates() {
        // 0 nodes done → no rate yet → "estimating…" (None).
        assert_eq!(linear_eta_secs(30, 0, 3), None);
        // total 0 (degenerate) → None.
        assert_eq!(linear_eta_secs(30, 0, 0), None);
        // 1 of 3 done in 30s → 30s/node × 2 remaining = 60s.
        assert_eq!(linear_eta_secs(30, 1, 3), Some(60));
        // 2 of 4 done in 40s → 20s/node × 2 remaining = 40s.
        assert_eq!(linear_eta_secs(40, 2, 4), Some(40));
        // last node done / over-count → None (nothing left to estimate).
        assert_eq!(linear_eta_secs(90, 3, 3), None);
        assert_eq!(linear_eta_secs(90, 4, 3), None);

        // Monotone-ish sanity: as more nodes complete at a steady rate, the
        // ETA decreases (or holds), never increases.
        let mut prev = u64::MAX;
        for done in 1..5usize {
            // steady 10s/node.
            let elapsed = (done as u64) * 10;
            if let Some(eta) = linear_eta_secs(elapsed, done, 5) {
                assert!(
                    eta <= prev,
                    "ETA must not grow as nodes complete at a steady rate: {eta} > {prev}"
                );
                prev = eta;
            }
        }
    }

    /// A huge node output must yield a small, bounded preview (Gap-3.4 reuse).
    #[test]
    fn node_output_preview_is_bounded() {
        let huge = "x".repeat(500_000);
        let preview = node_output_preview(&huge);
        assert!(
            preview.len() <= NODE_PREVIEW_MAX_CHARS + 64,
            "preview must be bounded near the cap; got {} bytes",
            preview.len()
        );
        assert!(
            preview.contains("[truncated]"),
            "a truncated preview must carry the Gap-3.4 marker; got: {}",
            &preview[..preview.len().min(80)]
        );
        // A small output passes through unbounded (no false truncation).
        let small = node_output_preview("short answer");
        assert_eq!(small, "short answer");
    }

    /// NodeStarted + NodeCompleted each emit a structured `octos.harness
    /// .event.v1` Progress event with node name + N/M (+ preview + success on
    /// completed). RED before the executor wired the harness sink: only an
    /// opaque heartbeat existed.
    #[tokio::test]
    async fn node_started_and_completed_emit_structured_harness_events() {
        use octos_agent::harness_events::{
            HarnessEvent, HarnessEventSinkContext, attach_event_sink_context,
            detach_event_sink_context,
        };

        // A real on-disk sink + registered context so the emit helper resolves
        // session/task ids and writes a v1 event line.
        let sink_file = tempfile::NamedTempFile::new().expect("sink file");
        let sink_uri = sink_file.path().display().to_string();
        attach_event_sink_context(
            sink_uri.clone(),
            HarnessEventSinkContext {
                session_id: "api:session".to_string(),
                task_id: "tc-pipeline-gap42".to_string(),
            },
        );

        let ctx = octos_agent::tools::ToolContext {
            tool_id: "tc-pipeline-gap42".to_string(),
            harness_event_sink: Some(sink_uri.clone()),
            ..octos_agent::tools::ToolContext::zero()
        };

        let sink_for_assert = sink_uri.clone();
        TOOL_CTX
            .scope(ctx, async move {
                // node 2 of 3 starts.
                emit_pipeline_node_event(
                    "research",
                    "node_started",
                    "analyze (2 of 3)",
                    "analyze",
                    2,
                    3,
                    None,
                    None,
                );
                // node 2 of 3 completes with a bounded preview.
                let preview = node_output_preview(&"y".repeat(100_000));
                emit_pipeline_node_event(
                    "research",
                    "node_completed",
                    "analyze (2 of 3) — done",
                    "analyze",
                    2,
                    3,
                    Some(true),
                    Some(&preview),
                );
            })
            .await;

        detach_event_sink_context(&sink_uri);

        let lines = std::fs::read_to_string(&sink_for_assert).expect("read sink");
        let events: Vec<HarnessEvent> = lines
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| HarnessEvent::from_json_line(l).expect("valid harness event"))
            .collect();
        assert_eq!(events.len(), 2, "expected NodeStarted + NodeCompleted");

        // Both are Progress events on the v1 contract carrying the structured
        // node fields the consumers render (via runtime_detail_value).
        let started = events[0].runtime_detail_value(None, None);
        assert_eq!(started["kind"], "progress");
        assert_eq!(started["workflow_kind"], "research");
        assert_eq!(started["phase"], "node_started");
        assert_eq!(started["node"], "analyze");
        assert_eq!(started["node_index"], 2);
        assert_eq!(started["node_total"], 3);
        assert!(
            started["progress_message"]
                .as_str()
                .unwrap()
                .contains("2 of 3")
        );

        let completed = events[1].runtime_detail_value(None, None);
        assert_eq!(completed["phase"], "node_completed");
        assert_eq!(completed["node"], "analyze");
        assert_eq!(completed["success"], true);
        let preview = completed["preview"].as_str().expect("preview field");
        assert!(
            preview.len() <= NODE_PREVIEW_MAX_CHARS + 64,
            "completed preview must be bounded; got {} bytes",
            preview.len()
        );
        // The whole event line must stay well under the harness line cap.
        let line_len = serde_json::to_string(&events[1]).unwrap().len();
        assert!(
            line_len < octos_agent::harness_events::MAX_HARNESS_EVENT_LINE_BYTES,
            "structured progress event must stay under the line cap; got {line_len}"
        );
    }

    /// The heartbeat carries the linear ETA (and "estimating…" when 0 done).
    #[tokio::test]
    async fn heartbeat_carries_eta_label() {
        let reporter = CapturingReporter::default();
        let captured = reporter.events.clone();

        let ctx = octos_agent::tools::ToolContext {
            tool_id: "tc-heartbeat-eta".to_string(),
            reporter: Arc::new(reporter),
            ..octos_agent::tools::ToolContext::zero()
        };

        // Start with 0 done so the first ticks read "estimating…", then flip
        // to 1-of-3 so a later tick extrapolates an ETA.
        let status = Arc::new(std::sync::Mutex::new(PipelineStatusSnapshot {
            pipeline_id: "research".to_string(),
            current_node: "plan".to_string(),
            nodes_done: 0,
            nodes_total: 3,
            start: Instant::now(),
        }));

        let status_for_advance = status.clone();
        TOOL_CTX
            .scope(ctx, async move {
                let _guard = spawn_pipeline_heartbeat(status_for_advance.clone(), 1)
                    .expect("heartbeat should spawn");
                tokio::time::sleep(Duration::from_millis(1_200)).await;
                if let Ok(mut g) = status_for_advance.lock() {
                    g.nodes_done = 1;
                    g.current_node = "analyze".to_string();
                }
                tokio::time::sleep(Duration::from_millis(2_200)).await;
            })
            .await;

        let messages: Vec<String> = captured
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                octos_agent::progress::ProgressEvent::ToolProgress { message, .. } => {
                    Some(message.clone())
                }
                _ => None,
            })
            .collect();
        let combined = messages.join("\n");
        assert!(
            combined.contains("estimating…"),
            "heartbeat must say 'estimating…' before any node completes; got: {combined}"
        );
        assert!(
            combined.contains("s left"),
            "heartbeat must surface an ETA once ≥1 node completes; got: {combined}"
        );
    }

    /// Phase 2-A integration — the `working_dir` set on
    /// [`ExecutorConfig`] must flow all the way down through
    /// [`PipelineExecutor::build_codergen`] onto the per-node
    /// [`CodergenHandler`]'s `working_dir`. This is the wire that
    /// `RunPipelineTool::execute` rides when it swaps the tool's
    /// pinned working dir for `scope.workspace()`. If this regresses,
    /// the mini5 NEW-06 fix silently goes dead even though the
    /// resolver still computes the right CWD.
    ///
    /// `make_test_config` opens its own runtime so it can't be called
    /// from inside `#[tokio::test]`; we mirror the `make_capped_config`
    /// pattern (async test + async config builder) so we share the
    /// outer runtime.
    #[tokio::test]
    async fn build_codergen_propagates_executor_working_dir_to_handler() {
        let custom_wd = tempfile::tempdir().expect("temp dir");
        let mut config = ExecutorConfig {
            default_provider: Arc::new(MockProvider),
            provider_router: None,
            memory: Arc::new(create_test_store().await),
            working_dir: PathBuf::from("/tmp"),
            provider_policy: None,
            plugin_dirs: vec![],
            plugin_require_signed: false,
            status_bridge: None,
            shutdown: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            max_parallel_workers: 8,
            max_pipeline_fanout_total: None,
            guards: Vec::new(),
            max_concurrent_llm_calls: None,
            checkpoint_store: None,
            hook_executor: None,
            workspace_context: crate::context::PipelineContext::default(),
            host_context: crate::host_context::PipelineHostContext::default(),
            embedder: None,
            catalog_dir: None,
            sandbox: octos_agent::SandboxConfig::default(),
        };
        config.working_dir = custom_wd.path().to_path_buf();
        let executor = PipelineExecutor::new(config);
        let codergen = executor.build_codergen_for_test();
        assert_eq!(
            codergen.working_dir_for_test(),
            custom_wd.path(),
            "CodergenHandler must inherit ExecutorConfig.working_dir so the \
             Phase 2-A scope override actually reaches per-node worker CWDs"
        );
    }

    /// Phase 2-A codex review (#1203) — when the pipeline runs inside a
    /// session, the worker CWD (`working_dir`) and the catalog/profile
    /// root MUST be separable. The executor's model assignment pass
    /// reads `pipeline_models.json` / `model_catalog.json` from the
    /// profile data dir, not the per-session workspace. Without the
    /// split, scoped runs would silently lose strong/fast model
    /// defaults and cost projections would fall back to the minimum
    /// estimate. Pin the split: with `catalog_dir` populated, catalog
    /// reads resolve against it even though `working_dir` was swapped.
    #[tokio::test]
    async fn catalog_dir_overrides_working_dir_for_model_assignment() {
        let profile_root = tempfile::tempdir().expect("profile root");
        let session_workspace = tempfile::tempdir().expect("session workspace");

        // Write a minimal catalog only under the profile root. If the
        // assignment pass reads from working_dir (the session
        // workspace) it will find nothing and silently no-op; if it
        // reads from catalog_dir (the profile root) it will load the
        // file.
        let pipeline_models = profile_root.path().join("pipeline_models.json");
        std::fs::write(&pipeline_models, b"{\"strong\":[],\"fast\":[]}").unwrap();

        let config = ExecutorConfig {
            default_provider: Arc::new(MockProvider),
            provider_router: None,
            memory: Arc::new(create_test_store().await),
            // worker CWD = per-session workspace (what Phase 2-A
            // overrides onto when a scope is present).
            working_dir: session_workspace.path().to_path_buf(),
            provider_policy: None,
            plugin_dirs: vec![],
            plugin_require_signed: false,
            status_bridge: None,
            shutdown: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            max_parallel_workers: 8,
            max_pipeline_fanout_total: None,
            guards: Vec::new(),
            max_concurrent_llm_calls: None,
            checkpoint_store: None,
            hook_executor: None,
            workspace_context: crate::context::PipelineContext::default(),
            host_context: crate::host_context::PipelineHostContext::default(),
            embedder: None,
            // catalog reads must hit the PROFILE root, not the worker CWD.
            catalog_dir: Some(profile_root.path().to_path_buf()),
            sandbox: octos_agent::SandboxConfig::default(),
        };

        // Pin the helper that the executor uses for catalog lookup:
        // unwrap_or-fallback must yield the catalog_dir when set.
        let executor = PipelineExecutor::new(config);
        let catalog_dir = executor
            .config
            .catalog_dir
            .as_deref()
            .unwrap_or(&executor.config.working_dir);
        assert_eq!(
            catalog_dir,
            profile_root.path(),
            "catalog_dir must be preferred over working_dir for catalog reads — \
             scoped runs lose model defaults without this split (codex #1203 P2)"
        );
        assert_ne!(
            catalog_dir, executor.config.working_dir,
            "the test setup must actually exercise the split path \
             (catalog_dir != working_dir)"
        );
    }

    /// Backward-compat — when `catalog_dir` is `None` (legacy callers
    /// that didn't opt into the split), catalog reads still resolve
    /// against `working_dir`. This is exactly the pre-Phase-2-A path.
    #[tokio::test]
    async fn catalog_dir_falls_back_to_working_dir_when_unset() {
        let only_dir = tempfile::tempdir().expect("temp dir");
        let mut config = ExecutorConfig {
            default_provider: Arc::new(MockProvider),
            provider_router: None,
            memory: Arc::new(create_test_store().await),
            working_dir: PathBuf::from("/tmp"),
            provider_policy: None,
            plugin_dirs: vec![],
            plugin_require_signed: false,
            status_bridge: None,
            shutdown: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            max_parallel_workers: 8,
            max_pipeline_fanout_total: None,
            guards: Vec::new(),
            max_concurrent_llm_calls: None,
            checkpoint_store: None,
            hook_executor: None,
            workspace_context: crate::context::PipelineContext::default(),
            host_context: crate::host_context::PipelineHostContext::default(),
            embedder: None,
            catalog_dir: None,
            sandbox: octos_agent::SandboxConfig::default(),
        };
        config.working_dir = only_dir.path().to_path_buf();
        let executor = PipelineExecutor::new(config);
        let catalog_dir = executor
            .config
            .catalog_dir
            .as_deref()
            .unwrap_or(&executor.config.working_dir);
        assert_eq!(
            catalog_dir,
            only_dir.path(),
            "without catalog_dir the executor must fall back to working_dir \
             (legacy callers, pre-Phase-2-A behaviour)"
        );
    }

    // ── Gap 4.2 / Blocker 1: node-progress event line MUST stay under the
    // 16 KiB harness-event line cap or the reader silently DROPS it ─────────

    /// Blocker 1 (RED on 3d5353d5) — a node event with a pathological 4 KiB
    /// `node_id` PLUS a 2 KiB all-control-byte preview (which JSON-escapes ~6x
    /// to ~12 KiB) serializes to >16 KiB and would be DROPPED by the reader's
    /// `MAX_HARNESS_EVENT_LINE_BYTES` gate — defeating the gap (back to opaque).
    /// After the fix the assembled event line is provably under the cap.
    #[tokio::test]
    async fn pathological_node_event_stays_under_line_cap() {
        use octos_agent::harness_events::{
            HarnessEvent, HarnessEventSinkContext, MAX_HARNESS_EVENT_LINE_BYTES,
            attach_event_sink_context, detach_event_sink_context,
        };

        let sink_file = tempfile::NamedTempFile::new().expect("sink file");
        let sink_uri = sink_file.path().display().to_string();
        attach_event_sink_context(
            sink_uri.clone(),
            HarnessEventSinkContext {
                session_id: "api:session".to_string(),
                task_id: "tc-pipeline-blocker1".to_string(),
            },
        );

        let ctx = octos_agent::tools::ToolContext {
            tool_id: "tc-pipeline-blocker1".to_string(),
            harness_event_sink: Some(sink_uri.clone()),
            ..octos_agent::tools::ToolContext::zero()
        };

        // A 4 KiB node_id (free-form, unbounded at the call site) and a 2 KiB
        // body that is ALL NUL bytes — each escapes to ` ` (6 bytes) so a
        // naive 2 KiB preview balloons to ~12 KiB serialized. node_id + preview
        // + a long message together blow past the 16 KiB line cap.
        let long_node_id = "n".repeat(4 * 1024);
        let control_body = "\u{0}".repeat(2 * 1024);
        let preview = node_output_preview(&control_body);
        // A max-allowed message (the validator already caps `message` at 2 KiB):
        // the OVER-CAP comes from the unbounded `node_id` + control-byte preview
        // in `extra`, which the Progress validator never inspects.
        let long_message = format!("{} (2 of 3)", "M".repeat(2000));

        let sink_for_assert = sink_uri.clone();
        TOOL_CTX
            .scope(ctx, async move {
                emit_pipeline_node_event(
                    "research",
                    "node_completed",
                    &long_message,
                    &long_node_id,
                    2,
                    3,
                    Some(true),
                    Some(&preview),
                );
            })
            .await;

        detach_event_sink_context(&sink_uri);

        let lines = std::fs::read_to_string(&sink_for_assert).expect("read sink");
        let event_lines: Vec<&str> = lines.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(
            event_lines.len(),
            1,
            "expected exactly one node_completed event line"
        );
        let line = event_lines[0];
        assert!(
            line.len() < MAX_HARNESS_EVENT_LINE_BYTES,
            "node event line must stay under the {MAX_HARNESS_EVENT_LINE_BYTES}-byte cap \
             (else the reader DROPS it); got {} bytes",
            line.len()
        );
        // And it must still be a valid, readable event (not dropped/garbled).
        let event =
            HarnessEvent::from_json_line(line).expect("event must round-trip (not dropped)");
        let detail = event.runtime_detail_value(None, None);
        assert_eq!(detail["phase"], "node_completed");
        // node_id is bounded but still present (truncation marker is fine).
        let node = detail["node"].as_str().expect("node field present");
        assert!(
            node.len() <= NODE_ID_MAX_CHARS + 16,
            "node_id must be bounded to its cap; got {} bytes",
            node.len()
        );
    }

    /// Blocker 1 (RED on 27c26433) — the node-event `message` (the
    /// `label (N of M)` string) was NOT in the line budget and was only
    /// raw-byte-bounded to 2 KiB *downstream*. Two failure modes:
    ///   1. a control-byte-heavy `message` just under 2 KiB raw escapes ~6× to
    ///      ~12 KiB, which — added to the ~10 KiB free-form budget for
    ///      node_id + preview — pushes the serialized line PAST 16 KiB and the
    ///      reader DROPS it; and
    ///   2. a `message` *over* 2 KiB raw (a long node label) is REJECTED by the
    ///      validator (`message exceeded 2048 bytes`) → the event never emits.
    /// After the fix the message is bounded by its escaped length (so it never
    /// trips the validator) AND counted in the line budget, so the serialized
    /// line is provably under the cap and the event always emits.
    #[tokio::test]
    async fn pathological_node_label_message_stays_under_line_cap() {
        use octos_agent::harness_events::{
            HarnessEvent, HarnessEventSinkContext, MAX_HARNESS_EVENT_LINE_BYTES,
            attach_event_sink_context, detach_event_sink_context,
        };

        let sink_file = tempfile::NamedTempFile::new().expect("sink file");
        let sink_uri = sink_file.path().display().to_string();
        attach_event_sink_context(
            sink_uri.clone(),
            HarnessEventSinkContext {
                session_id: "api:session".to_string(),
                task_id: "tc-pipeline-blocker1-label".to_string(),
            },
        );

        let ctx = octos_agent::tools::ToolContext {
            tool_id: "tc-pipeline-blocker1-label".to_string(),
            harness_event_sink: Some(sink_uri.clone()),
            ..octos_agent::tools::ToolContext::zero()
        };

        // A pathological node LABEL → message: 8 KiB of NUL bytes (each escapes
        // to ` ` = 6 bytes, so raw 8 KiB → ~48 KiB escaped) plus the
        // `(N of M)` suffix the call sites append. This both (a) exceeds the
        // 2 KiB raw `message` validator bound (→ rejected, no emit) AND (b)
        // would balloon the serialized line far past 16 KiB. Combined with a
        // 4 KiB free-form node_id and a 2 KiB all-control-byte preview, an
        // unbounded message guarantees an over-cap (or rejected) line.
        let control_label = "\u{0}".repeat(8 * 1024);
        let long_message = format!("{control_label} (2 of 3)");
        let long_node_id = "n".repeat(4 * 1024);
        let control_body = "\u{0}".repeat(2 * 1024);
        let preview = node_output_preview(&control_body);

        let sink_for_assert = sink_uri.clone();
        TOOL_CTX
            .scope(ctx, async move {
                emit_pipeline_node_event(
                    "research",
                    "node_completed",
                    &long_message,
                    &long_node_id,
                    2,
                    3,
                    Some(true),
                    Some(&preview),
                );
            })
            .await;

        detach_event_sink_context(&sink_uri);

        let lines = std::fs::read_to_string(&sink_for_assert).expect("read sink");
        let event_lines: Vec<&str> = lines.lines().filter(|l| !l.trim().is_empty()).collect();
        // The event must EMIT (a long/control-byte label must not silently drop
        // the whole event by tripping the 2 KiB raw-message validator bound).
        assert_eq!(
            event_lines.len(),
            1,
            "a pathological node LABEL must still emit exactly one node_completed \
             event (not be rejected by the message validator); got {event_lines:?}"
        );
        let line = event_lines[0];
        assert!(
            line.len() < MAX_HARNESS_EVENT_LINE_BYTES,
            "node event line (incl. message) must stay under the \
             {MAX_HARNESS_EVENT_LINE_BYTES}-byte cap (else the reader DROPS it); \
             got {} bytes",
            line.len()
        );
        let event =
            HarnessEvent::from_json_line(line).expect("event must round-trip (not dropped)");
        let detail = event.runtime_detail_value(None, None);
        assert_eq!(detail["phase"], "node_completed");
    }

    // ── Gap 4.2 / Blocker 2: Parallel + DynamicParallel sub-nodes MUST emit
    // structured per-node events (deep_research IS dynamic_parallel) ─────────

    /// Drain all `node_started`/`node_completed` events written to `sink_path`
    /// and return `(node_label, phase, success)` tuples for assertion.
    fn drain_node_events(sink_path: &str) -> Vec<(String, String, Option<bool>)> {
        use octos_agent::harness_events::HarnessEvent;
        let lines = std::fs::read_to_string(sink_path).unwrap_or_default();
        lines
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| HarnessEvent::from_json_line(l).ok())
            .map(|e| e.runtime_detail_value(None, None))
            .filter(|d| d["phase"] == "node_started" || d["phase"] == "node_completed")
            .map(|d| {
                (
                    d["node"].as_str().unwrap_or_default().to_string(),
                    d["phase"].as_str().unwrap_or_default().to_string(),
                    d["success"].as_bool(),
                )
            })
            .collect()
    }

    /// Blocker 2 (RED on 3d5353d5) — a static `parallel` fan-out must emit a
    /// structured `node_started` + `node_completed` for EACH sub-node. Before
    /// the fix the parallel branch `continue`s before the sequential emit
    /// sites, so a parallel pipeline emitted NO per-node structured progress.
    #[tokio::test]
    async fn parallel_subnodes_emit_structured_events() {
        use octos_agent::harness_events::{
            HarnessEventSinkContext, attach_event_sink_context, detach_event_sink_context,
        };

        let sink_file = tempfile::NamedTempFile::new().expect("sink file");
        let sink_uri = sink_file.path().display().to_string();
        attach_event_sink_context(
            sink_uri.clone(),
            HarnessEventSinkContext {
                session_id: "api:session".to_string(),
                task_id: "tc-pipeline-parallel".to_string(),
            },
        );

        let ctx = octos_agent::tools::ToolContext {
            tool_id: "tc-pipeline-parallel".to_string(),
            harness_event_sink: Some(sink_uri.clone()),
            ..octos_agent::tools::ToolContext::zero()
        };

        let dot = r#"
            digraph t {
                fan [handler="parallel", converge="merge"]
                a [handler="noop"]
                b [handler="noop"]
                merge [handler="noop"]
                fan -> a
                fan -> b
                a -> merge
                b -> merge
            }
        "#;

        let sink_for_run = sink_uri.clone();
        let result = TOOL_CTX
            .scope(ctx, async move {
                let config = make_capped_config(8).await;
                let executor = PipelineExecutor::new(config);
                executor
                    .run(dot, "parallel happy path", &serde_json::Map::new())
                    .await
            })
            .await;
        assert!(
            result.is_ok(),
            "parallel pipeline should complete: {result:?}"
        );

        detach_event_sink_context(&sink_uri);

        let events = drain_node_events(&sink_for_run);
        for sub in ["a", "b"] {
            assert!(
                events
                    .iter()
                    .any(|(n, p, _)| n == sub && p == "node_started"),
                "parallel sub-node '{sub}' must emit node_started; got {events:?}"
            );
            assert!(
                events
                    .iter()
                    .any(|(n, p, s)| n == sub && p == "node_completed" && *s == Some(true)),
                "parallel sub-node '{sub}' must emit node_completed(success); got {events:?}"
            );
        }
    }

    /// Blocker 2 (RED on 3d5353d5) — a `dynamic_parallel` node (the shape
    /// `deep_research` uses) must emit structured per-sub-node events for each
    /// dynamically-expanded worker. The `MockProvider` planner returns plain
    /// "done" → JSON extraction fails → the 3-task fallback expands, so we
    /// expect node_started + node_completed for each fallback worker task.
    #[tokio::test]
    async fn dynamic_parallel_subnodes_emit_structured_events() {
        use octos_agent::harness_events::{
            HarnessEventSinkContext, attach_event_sink_context, detach_event_sink_context,
        };

        let sink_file = tempfile::NamedTempFile::new().expect("sink file");
        let sink_uri = sink_file.path().display().to_string();
        attach_event_sink_context(
            sink_uri.clone(),
            HarnessEventSinkContext {
                session_id: "api:session".to_string(),
                task_id: "tc-pipeline-dynparallel".to_string(),
            },
        );

        let ctx = octos_agent::tools::ToolContext {
            tool_id: "tc-pipeline-dynparallel".to_string(),
            harness_event_sink: Some(sink_uri.clone()),
            ..octos_agent::tools::ToolContext::zero()
        };

        let dot = r#"
            digraph t {
                plan [handler="dynamic_parallel", converge="merge", prompt="plan"]
                merge [handler="noop"]
                plan -> merge
            }
        "#;

        let sink_for_run = sink_uri.clone();
        let result = TOOL_CTX
            .scope(ctx, async move {
                // Generous cap so the 3-task fallback fan-out runs to completion.
                let config = make_capped_config(64).await;
                let executor = PipelineExecutor::new(config);
                executor
                    .run(dot, "dynamic happy path", &serde_json::Map::new())
                    .await
            })
            .await;
        assert!(
            result.is_ok(),
            "dynamic_parallel pipeline should complete: {result:?}"
        );

        detach_event_sink_context(&sink_uri);

        let events = drain_node_events(&sink_for_run);
        let started = events
            .iter()
            .filter(|(_, p, _)| p == "node_started")
            .count();
        let completed = events
            .iter()
            .filter(|(_, p, _)| p == "node_completed")
            .count();
        assert!(
            started >= 2,
            "dynamic_parallel must emit a node_started per worker (>=2 fallback tasks); got {started} ({events:?})"
        );
        assert_eq!(
            started, completed,
            "every dynamic worker that starts must also emit node_completed; \
             started={started} completed={completed} ({events:?})"
        );
    }

    /// Blocker 2 (RED on 27c26433) — when a LATER sub-node's fan-out PREP
    /// fails (here: a per-contract budget reservation that admits the 1st
    /// codergen target but REJECTS the 2nd), the run loop early-returns via `?`
    /// BEFORE `join_all`, so any future already pushed is never polled. On
    /// 27c26433 `node_started` was emitted in the prep loop (outside the
    /// future), so the 1st target's `node_started` was emitted with no matching
    /// `node_completed` → a chip stuck "running" forever. After the fix the
    /// `node_started` emit lives INSIDE each future, so a future that never runs
    /// emits NOTHING and `node_started` count == `node_completed` count.
    #[tokio::test]
    async fn parallel_prep_failure_leaves_no_dangling_node_started() {
        use octos_agent::cost_ledger::{CostAccountant, CostBudgetPolicy, PersistentCostLedger};
        use octos_agent::harness_events::{
            HarnessEventSinkContext, attach_event_sink_context, detach_event_sink_context,
        };

        let sink_file = tempfile::NamedTempFile::new().expect("sink file");
        let sink_uri = sink_file.path().display().to_string();
        attach_event_sink_context(
            sink_uri.clone(),
            HarnessEventSinkContext {
                session_id: "api:session".to_string(),
                task_id: "tc-pipeline-prepfail".to_string(),
            },
        );

        let ctx = octos_agent::tools::ToolContext {
            tool_id: "tc-pipeline-prepfail".to_string(),
            harness_event_sink: Some(sink_uri.clone()),
            ..octos_agent::tools::ToolContext::zero()
        };

        // Per-contract ceiling sized against the reservation SEQUENCE:
        // pipeline-level (0.001 below) + 1st codergen target (0.001) = 0.002 is
        // ADMITTED, but adding the 2nd target (0.003) trips the 0.0025 ceiling.
        // The 2nd reservation `?`-propagates out of the fan-out prep loop before
        // `join_all`, abandoning the 1st target's already-pushed future.
        let ledger_dir = tempfile::tempdir().expect("ledger dir");
        let ledger = PersistentCostLedger::open(ledger_dir.path())
            .await
            .expect("open cost ledger");
        let policy = CostBudgetPolicy::default().with_per_contract_usd(0.0025);
        let accountant = Arc::new(CostAccountant::new(Arc::new(ledger), Some(policy)));

        // Two codergen fan-out targets (codergen reserves; noop does not).
        let dot = r#"
            digraph t {
                fan [handler="parallel", converge="merge"]
                a [handler="codergen", prompt="a"]
                b [handler="codergen", prompt="b"]
                merge [handler="noop"]
                fan -> a
                fan -> b
                a -> merge
                b -> merge
            }
        "#;

        let sink_for_run = sink_uri.clone();
        let result = TOOL_CTX
            .scope(ctx, async move {
                let mut config = make_capped_config(8).await;
                config.workspace_context = crate::context::PipelineContext::new()
                    .with_cost_accountant(accountant)
                    // Small pipeline-level projection so the per-NODE fan-out
                    // reservations (not the pipeline reserve) are what trips the
                    // ceiling on the 2nd target.
                    .with_projected_usd(0.001);
                let executor = PipelineExecutor::new(config);
                executor
                    .run(dot, "parallel prep failure", &serde_json::Map::new())
                    .await
            })
            .await;

        detach_event_sink_context(&sink_uri);

        // The pipeline is EXPECTED to fail (budget breach on the 2nd target).
        assert!(
            result.is_err(),
            "expected the fan-out to fail on the 2nd reservation; got {result:?}"
        );

        let events = drain_node_events(&sink_for_run);
        let started = events
            .iter()
            .filter(|(_, p, _)| p == "node_started")
            .count();
        let completed = events
            .iter()
            .filter(|(_, p, _)| p == "node_completed")
            .count();
        assert_eq!(
            started, completed,
            "every emitted node_started must have a matching node_completed even \
             when fan-out prep aborts early (no stuck-running chip); \
             started={started} completed={completed} ({events:?})"
        );
    }

    /// Blocker 2 (RED on 27c26433) — same dangling-`node_started` guard for
    /// `dynamic_parallel` (the shape `deep_research` uses). A LATER worker's
    /// budget reservation is rejected, aborting the fan-out prep loop before
    /// `join_all`; no worker may be left with a `node_started` and no matching
    /// `node_completed`.
    #[tokio::test]
    async fn dynamic_parallel_prep_failure_leaves_no_dangling_node_started() {
        use octos_agent::cost_ledger::{CostAccountant, CostBudgetPolicy, PersistentCostLedger};
        use octos_agent::harness_events::{
            HarnessEventSinkContext, attach_event_sink_context, detach_event_sink_context,
        };

        let sink_file = tempfile::NamedTempFile::new().expect("sink file");
        let sink_uri = sink_file.path().display().to_string();
        attach_event_sink_context(
            sink_uri.clone(),
            HarnessEventSinkContext {
                session_id: "api:session".to_string(),
                task_id: "tc-pipeline-dp-prepfail".to_string(),
            },
        );

        let ctx = octos_agent::tools::ToolContext {
            tool_id: "tc-pipeline-dp-prepfail".to_string(),
            harness_event_sink: Some(sink_uri.clone()),
            ..octos_agent::tools::ToolContext::zero()
        };

        // The 3-task fallback expands (MockProvider returns "done" → JSON
        // extraction fails). Reservation sequence: pipeline (0.001) + worker1
        // (0.001) = 0.002 ADMITTED; adding worker2 (0.003) trips the 0.0025
        // ceiling, aborting the prep loop before `join_all`.
        let ledger_dir = tempfile::tempdir().expect("ledger dir");
        let ledger = PersistentCostLedger::open(ledger_dir.path())
            .await
            .expect("open cost ledger");
        let policy = CostBudgetPolicy::default().with_per_contract_usd(0.0025);
        let accountant = Arc::new(CostAccountant::new(Arc::new(ledger), Some(policy)));

        let dot = r#"
            digraph t {
                plan [handler="dynamic_parallel", converge="merge", prompt="plan"]
                merge [handler="noop"]
                plan -> merge
            }
        "#;

        let sink_for_run = sink_uri.clone();
        let result = TOOL_CTX
            .scope(ctx, async move {
                let mut config = make_capped_config(64).await;
                config.workspace_context = crate::context::PipelineContext::new()
                    .with_cost_accountant(accountant)
                    .with_projected_usd(0.001);
                let executor = PipelineExecutor::new(config);
                executor
                    .run(dot, "dynamic prep failure", &serde_json::Map::new())
                    .await
            })
            .await;

        detach_event_sink_context(&sink_uri);

        assert!(
            result.is_err(),
            "expected the dynamic fan-out to fail on a later reservation; got {result:?}"
        );

        let events = drain_node_events(&sink_for_run);
        let started = events
            .iter()
            .filter(|(_, p, _)| p == "node_started")
            .count();
        let completed = events
            .iter()
            .filter(|(_, p, _)| p == "node_completed")
            .count();
        assert_eq!(
            started, completed,
            "dynamic_parallel: every emitted node_started must have a matching \
             node_completed even when prep aborts early; \
             started={started} completed={completed} ({events:?})"
        );
    }

    /// NIT (RED on 3d5353d5 if multiplication were unguarded) — the linear ETA
    /// must SATURATE instead of overflowing when `per_node * remaining` would
    /// exceed `u64::MAX`. A pathological huge `elapsed` with many nodes
    /// remaining must not panic / wrap.
    #[test]
    fn linear_eta_saturates_on_huge_elapsed() {
        // per_node = u64::MAX / 1 = u64::MAX; remaining = large → would overflow
        // a plain `*`. Must clamp to u64::MAX, not wrap.
        let eta = linear_eta_secs(u64::MAX, 1, 1_000_000);
        assert_eq!(eta, Some(u64::MAX), "ETA must saturate, not overflow");
    }

    // ── Gap 4.2 / Blocker 3: an unbounded graph/workflow id must not silently
    // DROP the whole node event (workflow > 128 B fails the validator) ───────

    /// Blocker 3 (RED on cab744a4) — `emit_pipeline_node_event` copies the
    /// graph id verbatim into the event `workflow`, but the DOT parser accepts
    /// an UNBOUNDED graph id and the harness validator REJECTS `workflow >128 B`.
    /// A pathological >128-byte graph id therefore makes `write_event_to_sink`
    /// reject the event — and the preview-shrink loop can't fix it (the id is
    /// not elastic) — so the event silently DROPS (back to opaque). After the
    /// fix the workflow id is truncated at the emit site to the validator limit,
    /// so the line is provably emittable with preview shrunk all the way to 0.
    #[tokio::test]
    async fn oversized_graph_id_node_event_still_emits() {
        use octos_agent::harness_events::{
            HarnessEvent, HarnessEventSinkContext, MAX_HARNESS_EVENT_LINE_BYTES,
            attach_event_sink_context, detach_event_sink_context,
        };

        let sink_file = tempfile::NamedTempFile::new().expect("sink file");
        let sink_uri = sink_file.path().display().to_string();
        attach_event_sink_context(
            sink_uri.clone(),
            HarnessEventSinkContext {
                session_id: "api:session".to_string(),
                task_id: "tc-pipeline-blocker3".to_string(),
            },
        );

        let ctx = octos_agent::tools::ToolContext {
            tool_id: "tc-pipeline-blocker3".to_string(),
            harness_event_sink: Some(sink_uri.clone()),
            ..octos_agent::tools::ToolContext::zero()
        };

        // A 512-byte graph id (well over the 128-byte MAX_WORKFLOW_BYTES) plus a
        // big preview. The id is NOT elastic, so without an emit-site bound the
        // validator rejects the event and nothing is written.
        let huge_graph_id = "g".repeat(512);
        let preview = node_output_preview(&"z".repeat(50_000));

        let sink_for_assert = sink_uri.clone();
        let huge_for_assert = huge_graph_id.clone();
        TOOL_CTX
            .scope(ctx, async move {
                emit_pipeline_node_event(
                    &huge_graph_id,
                    "node_started",
                    "analyze (1 of 2)",
                    "analyze",
                    1,
                    2,
                    None,
                    Some(&preview),
                );
            })
            .await;

        detach_event_sink_context(&sink_uri);

        let lines = std::fs::read_to_string(&sink_for_assert).expect("read sink");
        let event_lines: Vec<&str> = lines.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(
            event_lines.len(),
            1,
            "an oversized graph id must NOT drop the event; expected exactly one \
             node event line, got {event_lines:?}"
        );
        let line = event_lines[0];
        assert!(
            line.len() < MAX_HARNESS_EVENT_LINE_BYTES,
            "node event line must stay under the {MAX_HARNESS_EVENT_LINE_BYTES}-byte cap; \
             got {} bytes",
            line.len()
        );
        let event =
            HarnessEvent::from_json_line(line).expect("event must round-trip (not dropped)");
        let detail = event.runtime_detail_value(None, None);
        assert_eq!(detail["phase"], "node_started");
        // The workflow id was truncated to the validator bound — prefix preserved.
        let workflow = detail["workflow_kind"].as_str().expect("workflow present");
        assert!(
            workflow.len() <= 128,
            "workflow id must be truncated to the validator bound; got {} bytes",
            workflow.len()
        );
        assert!(
            huge_for_assert.starts_with(workflow) || workflow.starts_with("gg"),
            "truncated workflow must be a prefix of the original id; got {workflow:?}"
        );
    }

    // ── Gap 4.2 / Blocker 1+2: NodeProgressGuard — every node_started gets a
    // matching node_completed on EVERY exit path (error, panic, cancellation) ─

    /// A test handler whose `execute` returns `Err` on the first call — drives
    /// the SEQUENTIAL dispatch `?`-early-return path between the `node_started`
    /// and the (skipped) `node_completed` emit.
    struct ErroringHandler;
    #[async_trait::async_trait]
    impl crate::handler::Handler for ErroringHandler {
        async fn execute(&self, node: &PipelineNode, _ctx: &HandlerContext) -> Result<NodeOutcome> {
            eyre::bail!("handler '{}' hard-errored on purpose", node.id)
        }
    }

    /// A test handler whose `execute` PANICS — exercises the guard's Drop on
    /// unwind (a panic between node_started and node_completed must still flip
    /// the chip off "running").
    struct PanickingHandler;
    #[async_trait::async_trait]
    impl crate::handler::Handler for PanickingHandler {
        async fn execute(
            &self,
            _node: &PipelineNode,
            _ctx: &HandlerContext,
        ) -> Result<NodeOutcome> {
            panic!("handler panicked on purpose");
        }
    }

    /// A test handler that NEVER returns (parks forever) so the run future can
    /// be polled once into the node, then dropped (cancellation) mid-node.
    struct HangingHandler;
    #[async_trait::async_trait]
    impl crate::handler::Handler for HangingHandler {
        async fn execute(
            &self,
            _node: &PipelineNode,
            _ctx: &HandlerContext,
        ) -> Result<NodeOutcome> {
            std::future::pending::<()>().await;
            unreachable!()
        }
    }

    fn install_handler(
        kind: HandlerKind,
        handler: Arc<dyn crate::handler::Handler>,
    ) -> HandlerRegistry {
        let mut registry = HandlerRegistry::new();
        registry.register(kind, handler);
        // DynamicParallel needs a (noop) registry entry even when unused.
        registry.register(HandlerKind::DynamicParallel, Arc::new(NoopHandler));
        registry.register(HandlerKind::Noop, Arc::new(NoopHandler));
        registry
    }

    /// Blocker 1 (RED on cab744a4) — a SEQUENTIAL node whose dispatch errors
    /// (`?`-returns out of the loop between the node_started emit and the
    /// node_completed emit) must STILL get a matching `node_completed{success:
    /// false}` via the RAII guard's Drop — otherwise the chip is stuck "running".
    #[tokio::test]
    async fn sequential_dispatch_error_emits_node_completed_via_guard() {
        use octos_agent::harness_events::{
            HarnessEventSinkContext, attach_event_sink_context, detach_event_sink_context,
        };

        let sink_file = tempfile::NamedTempFile::new().expect("sink file");
        let sink_uri = sink_file.path().display().to_string();
        attach_event_sink_context(
            sink_uri.clone(),
            HarnessEventSinkContext {
                session_id: "api:session".to_string(),
                task_id: "tc-seq-error".to_string(),
            },
        );
        let ctx = octos_agent::tools::ToolContext {
            tool_id: "tc-seq-error".to_string(),
            harness_event_sink: Some(sink_uri.clone()),
            ..octos_agent::tools::ToolContext::zero()
        };

        // Single codergen node whose handler hard-errors → dispatch `?`-returns.
        let dot = r#"
            digraph t {
                solo [handler="codergen", prompt="go"]
            }
        "#;

        let sink_for_run = sink_uri.clone();
        let result = TOOL_CTX
            .scope(ctx, async move {
                let config = make_capped_config(8).await;
                let executor = PipelineExecutor::new(config);
                let handlers = install_handler(HandlerKind::Codergen, Arc::new(ErroringHandler));
                executor
                    .run_with_handlers(dot, "seq error", &serde_json::Map::new(), handlers)
                    .await
            })
            .await;

        detach_event_sink_context(&sink_uri);
        assert!(
            result.is_err(),
            "erroring dispatch must surface as Err: {result:?}"
        );

        let events = drain_node_events(&sink_for_run);
        let started = events
            .iter()
            .filter(|(_, p, _)| p == "node_started")
            .count();
        let completed = events
            .iter()
            .filter(|(_, p, _)| p == "node_completed")
            .count();
        assert_eq!(
            (started, completed),
            (1, 1),
            "a sequential node that errors must emit exactly one node_started AND \
             one node_completed (no dangling); got {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|(n, p, s)| n == "solo" && p == "node_completed" && *s == Some(false)),
            "the guard-Drop completion must mark the interrupted node failed; got {events:?}"
        );
    }

    /// Blocker 1 (RED on cab744a4) — a node whose handler PANICS must still emit
    /// `node_completed` via the guard's Drop during unwind. We catch the panic
    /// at the run boundary so the test observes the emitted events.
    #[tokio::test]
    async fn sequential_panic_emits_node_completed_via_guard_drop() {
        use octos_agent::harness_events::{
            HarnessEventSinkContext, attach_event_sink_context, detach_event_sink_context,
        };

        let sink_file = tempfile::NamedTempFile::new().expect("sink file");
        let sink_uri = sink_file.path().display().to_string();
        attach_event_sink_context(
            sink_uri.clone(),
            HarnessEventSinkContext {
                session_id: "api:session".to_string(),
                task_id: "tc-seq-panic".to_string(),
            },
        );
        let ctx = octos_agent::tools::ToolContext {
            tool_id: "tc-seq-panic".to_string(),
            harness_event_sink: Some(sink_uri.clone()),
            ..octos_agent::tools::ToolContext::zero()
        };

        let dot = r#"
            digraph t {
                solo [handler="codergen", prompt="go"]
            }
        "#;

        let sink_for_run = sink_uri.clone();
        // Run on a separate tokio task so the panic is contained and joined,
        // letting the guard's Drop (synchronous emit) run during unwind.
        let join = tokio::spawn(async move {
            TOOL_CTX
                .scope(ctx, async move {
                    let config = make_capped_config(8).await;
                    let executor = PipelineExecutor::new(config);
                    let handlers =
                        install_handler(HandlerKind::Codergen, Arc::new(PanickingHandler));
                    executor
                        .run_with_handlers(dot, "seq panic", &serde_json::Map::new(), handlers)
                        .await
                })
                .await
        })
        .await;

        detach_event_sink_context(&sink_uri);
        assert!(
            join.is_err(),
            "the handler panic must propagate as a join error"
        );

        let events = drain_node_events(&sink_for_run);
        let started = events
            .iter()
            .filter(|(_, p, _)| p == "node_started")
            .count();
        let completed = events
            .iter()
            .filter(|(_, p, _)| p == "node_completed")
            .count();
        assert_eq!(
            (started, completed),
            (1, 1),
            "a panicking node must still emit node_completed via guard Drop; got {events:?}"
        );
    }

    /// Blocker 1 (RED on cab744a4) — a CANCELLED run (the run future is dropped
    /// mid-node) must flip every started node off "running" via the guard's
    /// Drop. The guard captures the sink at arm time, so Drop works even though
    /// the TOOL_CTX task-local is gone when the future is dropped.
    #[tokio::test]
    async fn cancelled_run_emits_node_completed_via_guard_drop() {
        use octos_agent::harness_events::{
            HarnessEventSinkContext, attach_event_sink_context, detach_event_sink_context,
        };

        let sink_file = tempfile::NamedTempFile::new().expect("sink file");
        let sink_uri = sink_file.path().display().to_string();
        attach_event_sink_context(
            sink_uri.clone(),
            HarnessEventSinkContext {
                session_id: "api:session".to_string(),
                task_id: "tc-cancel".to_string(),
            },
        );
        let ctx = octos_agent::tools::ToolContext {
            tool_id: "tc-cancel".to_string(),
            harness_event_sink: Some(sink_uri.clone()),
            ..octos_agent::tools::ToolContext::zero()
        };

        let dot = r#"
            digraph t {
                solo [handler="codergen", prompt="go"]
            }
        "#;

        let sink_for_run = sink_uri.clone();
        TOOL_CTX
            .scope(ctx, async move {
                let config = make_capped_config(8).await;
                let executor = PipelineExecutor::new(config);
                let handlers = install_handler(HandlerKind::Codergen, Arc::new(HangingHandler));
                let vars = serde_json::Map::new();
                let run = executor.run_with_handlers(dot, "cancel", &vars, handlers);
                // Drive the run far enough to enter the node (emit node_started +
                // park on the hanging handler), then DROP it (cancellation).
                let timed = tokio::time::timeout(Duration::from_millis(150), run).await;
                assert!(timed.is_err(), "the hanging handler must not complete");
                // `timed` (and the inner run future) is dropped here → guard Drop.
            })
            .await;

        detach_event_sink_context(&sink_uri);

        let events = drain_node_events(&sink_for_run);
        let started = events
            .iter()
            .filter(|(_, p, _)| p == "node_started")
            .count();
        let completed = events
            .iter()
            .filter(|(_, p, _)| p == "node_completed")
            .count();
        assert_eq!(
            started, completed,
            "a cancelled run must complete every started node via guard Drop; \
             started={started} completed={completed} ({events:?})"
        );
        assert!(
            started >= 1,
            "the run must have entered the node; got {events:?}"
        );
    }

    /// Guard happy-path regression — a SEQUENTIAL node that completes normally
    /// must emit EXACTLY one `node_started` and EXACTLY one `node_completed`
    /// (success): `complete()` disarms the guard so its Drop does NOT fire a
    /// second terminal event. Locks the "exactly one started + one completed per
    /// node that runs; no double-emit" invariant for the sequential path.
    #[tokio::test]
    async fn sequential_happy_path_emits_exactly_one_pair() {
        use octos_agent::harness_events::{
            HarnessEventSinkContext, attach_event_sink_context, detach_event_sink_context,
        };

        let sink_file = tempfile::NamedTempFile::new().expect("sink file");
        let sink_uri = sink_file.path().display().to_string();
        attach_event_sink_context(
            sink_uri.clone(),
            HarnessEventSinkContext {
                session_id: "api:session".to_string(),
                task_id: "tc-seq-happy".to_string(),
            },
        );
        let ctx = octos_agent::tools::ToolContext {
            tool_id: "tc-seq-happy".to_string(),
            harness_event_sink: Some(sink_uri.clone()),
            ..octos_agent::tools::ToolContext::zero()
        };

        // Single noop node — completes normally; the guard must disarm.
        let dot = r#"
            digraph t {
                solo [handler="noop"]
            }
        "#;

        let sink_for_run = sink_uri.clone();
        let result = TOOL_CTX
            .scope(ctx, async move {
                let config = make_capped_config(8).await;
                let executor = PipelineExecutor::new(config);
                executor
                    .run(dot, "seq happy", &serde_json::Map::new())
                    .await
            })
            .await;

        detach_event_sink_context(&sink_uri);
        assert!(
            result.is_ok(),
            "happy-path sequential run must succeed: {result:?}"
        );

        let events = drain_node_events(&sink_for_run);
        let started: Vec<_> = events
            .iter()
            .filter(|(_, p, _)| p == "node_started")
            .collect();
        let completed: Vec<_> = events
            .iter()
            .filter(|(_, p, _)| p == "node_completed")
            .collect();
        assert_eq!(
            (started.len(), completed.len()),
            (1, 1),
            "a sequential node that completes normally must emit EXACTLY one \
             started + one completed (guard disarmed, no double-emit); got {events:?}"
        );
        assert_eq!(
            completed[0].2,
            Some(true),
            "the normal completion must report success=true, not the guard's \
             interrupted fallback; got {events:?}"
        );
    }

    /// Blocker 2 (RED on cab744a4) — a PANIC inside a Parallel sub-node future
    /// must still emit `node_completed` for that sub-node via the guard's Drop
    /// (the future unwinds; `join_all` surfaces the panic). Without the guard,
    /// the panicking worker's node_started dangles.
    #[tokio::test]
    async fn parallel_subnode_panic_emits_node_completed_via_guard_drop() {
        use octos_agent::harness_events::{
            HarnessEventSinkContext, attach_event_sink_context, detach_event_sink_context,
        };

        let sink_file = tempfile::NamedTempFile::new().expect("sink file");
        let sink_uri = sink_file.path().display().to_string();
        attach_event_sink_context(
            sink_uri.clone(),
            HarnessEventSinkContext {
                session_id: "api:session".to_string(),
                task_id: "tc-par-panic".to_string(),
            },
        );
        let ctx = octos_agent::tools::ToolContext {
            tool_id: "tc-par-panic".to_string(),
            harness_event_sink: Some(sink_uri.clone()),
            ..octos_agent::tools::ToolContext::zero()
        };

        // Parallel fan-out where each sub-node is a codergen target whose
        // handler panics. `join_all` polls the worker futures on THIS task.
        let dot = r#"
            digraph t {
                fan [handler="parallel", converge="merge"]
                a [handler="codergen", prompt="a"]
                merge [handler="noop"]
                fan -> a
                a -> merge
            }
        "#;

        let sink_for_run = sink_uri.clone();
        let join = tokio::spawn(async move {
            TOOL_CTX
                .scope(ctx, async move {
                    let config = make_capped_config(8).await;
                    let executor = PipelineExecutor::new(config);
                    let handlers =
                        install_handler(HandlerKind::Codergen, Arc::new(PanickingHandler));
                    executor
                        .run_with_handlers(dot, "par panic", &serde_json::Map::new(), handlers)
                        .await
                })
                .await
        })
        .await;

        detach_event_sink_context(&sink_uri);
        // The worker panic propagates through join_all → the run task panics.
        assert!(join.is_err(), "a panicking parallel worker must propagate");

        let events = drain_node_events(&sink_for_run);
        let started = events
            .iter()
            .filter(|(_, p, _)| p == "node_started")
            .count();
        let completed = events
            .iter()
            .filter(|(_, p, _)| p == "node_completed")
            .count();
        assert_eq!(
            started, completed,
            "every parallel sub-node that starts must emit node_completed even on \
             panic; started={started} completed={completed} ({events:?})"
        );
        assert!(
            started >= 1,
            "the parallel worker must have started; got {events:?}"
        );
    }
}
