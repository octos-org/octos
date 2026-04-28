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
use octos_llm::{ChatConfig, LlmProvider, ProviderRouter};
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
use crate::handler::{
    CodergenHandler, GateHandler, HandlerContext, HandlerRegistry, NoopHandler, ShellHandler,
};
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

/// Process results from parallel worker execution, producing merged content and summaries.
fn process_worker_results(
    results: Vec<(String, PipelineNode, Duration, Result<NodeOutcome>)>,
    bridge: Option<&PipelineStatusBridge>,
    working_dir: &std::path::Path,
) -> (
    String,
    bool,
    Vec<NodeSummary>,
    TokenUsage,
    Vec<(String, NodeOutcome)>,
) {
    let mut merged_parts = Vec::new();
    let mut any_error = false;
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

                if outcome.status == OutcomeStatus::Error {
                    any_error = true;
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

    (merged_content, any_error, summaries, total_tokens, outcomes)
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
}

impl PipelineExecutor {
    pub fn new(config: ExecutorConfig) -> Self {
        Self { config }
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

    /// Run a pipeline from a DOT string.
    pub async fn run(
        &self,
        dot_content: &str,
        user_input: &str,
        variables: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<PipelineResult> {
        let handlers = self.build_handlers();
        self.run_with_handlers(dot_content, user_input, variables, handlers)
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
        // Parse and validate
        let graph = parse_dot(dot_content).wrap_err("failed to parse pipeline DOT")?;

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

        let diags = validate::validate(&graph);

        for diag in &diags {
            match diag.severity {
                validate::Severity::Error => {
                    tracing::error!(rule = diag.rule, "{}", diag.message);
                }
                validate::Severity::Warning => {
                    warn!(rule = diag.rule, "{}", diag.message);
                }
            }
        }

        if validate::has_errors(&diags) {
            let errors: Vec<_> = diags
                .iter()
                .filter(|d| d.severity == validate::Severity::Error)
                .map(|d| format!("rule {}: {}", d.rule, d.message))
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

        // Execute graph
        let mut result = self
            .execute_graph(&graph, &handlers, &start_node, user_input, variables)
            .await;

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
                        self.commit_pipeline_reservation(handle, &graph.id, &r.token_usage)
                            .await;
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
        usage: &TokenUsage,
    ) {
        let contract_id = self.pipeline_contract_id(graph_id);
        let actual_cost = octos_agent::cost_ledger::project_cost_usd(
            "pipeline-aggregate",
            usage.input_tokens,
            usage.output_tokens,
        )
        .unwrap_or(0.0);
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
            let registry = octos_agent::ToolRegistry::with_builtins(&self.config.working_dir);
            run_declared_validators(
                &registry,
                &self.config.working_dir,
                &policy.validation.validators,
                "pipeline",
                ValidatorPhase::Completion,
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
        let registry = octos_agent::ToolRegistry::with_builtins(&self.config.working_dir);
        run_declared_validators(
            &registry,
            &self.config.working_dir,
            &scoped,
            &format!("pipeline-node-{node_id}"),
            ValidatorPhase::Completion,
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
    /// invoking run_pipeline pill. Returns `None` when no supervisor
    /// is wired (legacy callers).
    fn register_node_task(&self, node_id: &str) -> Option<String> {
        let supervisor = self.config.host_context.task_supervisor.as_ref()?;
        let parent_tool_call_id = self
            .config
            .host_context
            .parent_tool_call_id
            .as_deref()
            .unwrap_or("");
        let session_key = self.config.host_context.parent_session_key.as_deref();
        let tool_name = format!("pipeline:{node_id}");
        let task_id = supervisor.register(&tool_name, parent_tool_call_id, session_key);
        info!(
            node = %node_id,
            task_id = %task_id,
            parent_tool_call_id = %parent_tool_call_id,
            "registered pipeline node child task"
        );
        Some(task_id)
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
        self.build_codergen()
    }

    fn build_codergen(&self) -> CodergenHandler {
        let mut codergen = CodergenHandler::new(
            self.config.default_provider.clone(),
            self.config.memory.clone(),
            self.config.working_dir.clone(),
            self.config.shutdown.clone(),
        )
        .with_provider_policy(self.config.provider_policy.clone())
        .with_plugin_dirs(self.config.plugin_dirs.clone())
        // M8 parity (W1.A1): propagate the host context so per-node
        // Agents inherit the parent session's FileStateCache /
        // SubAgentOutputRouter / AgentSummaryGenerator. Empty context
        // keeps pre-M8 behaviour bitwise identical.
        .with_host_context(self.config.host_context.clone());

        if let Some(ref router) = self.config.provider_router {
            codergen = codergen.with_provider_router(router.clone());
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

    fn build_handlers(&self) -> HandlerRegistry {
        let mut registry = HandlerRegistry::new();

        // coding-blue FA-7: `build_codergen` reads the installed
        // PipelineContext and propagates compaction policy + workspace
        // onto every LLM-call node. When the context is empty (legacy
        // path) the setters are no-ops — behaviour is byte-for-byte
        // identical to pre-FA-7.
        let codergen = self.build_codergen();

        registry.register(HandlerKind::Codergen, Arc::new(codergen));
        registry.register(
            HandlerKind::Shell,
            Arc::new(ShellHandler::new(self.config.working_dir.clone())),
        );
        registry.register(HandlerKind::Gate, Arc::new(GateHandler));
        registry.register(HandlerKind::Noop, Arc::new(NoopHandler));
        // DynamicParallel is handled directly in execute_graph, but needs a registry entry
        registry.register(HandlerKind::DynamicParallel, Arc::new(NoopHandler));

        registry
    }

    async fn execute_graph(
        &self,
        graph: &PipelineGraph,
        handlers: &HandlerRegistry,
        start_node: &str,
        user_input: &str,
        variables: &serde_json::Map<String, serde_json::Value>,
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

        loop {
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
                for target_id in &targets {
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

                    let ctx = HandlerContext {
                        input: fan_input.clone(),
                        completed: completed.clone(),
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

                    let sem = semaphore.clone();
                    futures.push(async move {
                        let _permit = sem.acquire().await.expect("semaphore closed");
                        let start = Instant::now();
                        let result = execute_with_retries_static(
                            &handler,
                            &target_with_prompt,
                            &ctx,
                            max_retries,
                        )
                        .await;
                        let n = par_done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                        let secs = start.elapsed().as_secs();
                        report_progress(&format!(
                            "{par_node_label}: '{par_label}' done ({n}/{total_targets}, {secs}s)"
                        ));
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

                let (merged_content, any_error, worker_summaries, worker_tokens, outcomes) =
                    process_worker_results(
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

                info!(
                    node = %node.id,
                    duration_ms = fan_duration,
                    targets = targets.len(),
                    errors = any_error,
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
                    success: !any_error,
                });
                completed.insert(
                    current_node_id.clone(),
                    NodeOutcome {
                        node_id: node.id.clone(),
                        status: if any_error {
                            OutcomeStatus::Fail
                        } else {
                            OutcomeStatus::Pass
                        },
                        content: merged_content,
                        token_usage: TokenUsage::default(),
                        files_modified: vec![],
                    },
                );

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

                // Default planning prompt
                let planning_prompt = node.prompt.as_deref().unwrap_or(
                    "Generate 4-6 research search angles for this query. \
                     Each angle should cover a different aspect.\n\
                     Respond with ONLY a JSON array of objects with \"task\" and \"label\" fields.",
                );

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
                    planning_prompt,
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

                // Execute all synthetic nodes concurrently
                let total_workers = synthetic_nodes.len();
                let completed_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let mut futures = Vec::new();
                // coding-blue FA-7: same fan-out reservation pattern as
                // the static Parallel branch — reserve per-worker up
                // front, release en bloc when the fan-out completes.
                let mut dp_reservations: Vec<ReservationHandle> = Vec::new();
                for (task_id, mut synth_node) in synthetic_nodes {
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

                    if let Some(handle) = self.reserve_node_budget(&graph.id, &synth_node).await? {
                        dp_reservations.push(handle);
                    }

                    let ctx = HandlerContext {
                        input: dp_input.clone(),
                        completed: completed.clone(),
                        working_dir: self.config.working_dir.clone(),
                    };

                    let handler = codergen_handler.clone();
                    let max_retries = synth_node.max_retries;
                    let worker_label = synth_node.label.clone().unwrap_or_else(|| task_id.clone());
                    let dp_label = dp_label.to_owned();
                    let done_count = completed_count.clone();

                    futures.push(async move {
                        let start = Instant::now();
                        let result =
                            execute_with_retries_static(&handler, &synth_node, &ctx, max_retries)
                                .await;
                        let n = done_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                        let secs = start.elapsed().as_secs();
                        report_progress(&format!(
                            "{dp_label}: '{worker_label}' done ({n}/{total_workers}, {secs}s)"
                        ));
                        (task_id, synth_node, start.elapsed(), result)
                    });
                    // Guard B: count this worker as dispatched (the
                    // future is queued — `join_all` below awaits it).
                    fanout_workers_dispatched = fanout_workers_dispatched.saturating_add(1);
                }

                let results = futures::future::join_all(futures).await;
                drop(dp_reservations);

                let (merged_content, any_error, worker_summaries, worker_tokens, outcomes) =
                    process_worker_results(
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

                info!(
                    node = %node.id,
                    duration_ms = fan_duration,
                    tasks = tasks.len(),
                    errors = any_error,
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
                    success: !any_error,
                });
                completed.insert(
                    current_node_id.clone(),
                    NodeOutcome {
                        node_id: node.id.clone(),
                        status: if any_error {
                            OutcomeStatus::Fail
                        } else {
                            OutcomeStatus::Pass
                        },
                        content: merged_content,
                        token_usage: plan_usage,
                        files_modified: vec![],
                    },
                );

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

            let input_bytes = input_text.len();

            let seq_label = node.label.as_deref().unwrap_or(&node.id);
            report_progress(&format!("{seq_label}: running..."));

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

            let ctx = HandlerContext {
                input: input_text,
                completed: completed.clone(),
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
            let node_task_id = self.register_node_task(&node.id);
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
            if outcome.status == OutcomeStatus::Error {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{NodeOutcome, OutcomeStatus};

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
            status_bridge: None,
            shutdown: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            max_parallel_workers: 8,
            max_pipeline_fanout_total: None,
            checkpoint_store: None,
            hook_executor: None,
            workspace_context: crate::context::PipelineContext::default(),
            host_context: crate::host_context::PipelineHostContext::default(),
        }
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

    async fn create_test_store() -> EpisodeStore {
        let dir = tempfile::tempdir().unwrap();
        let dir = Box::leak(Box::new(dir));
        EpisodeStore::open(dir.path()).await.unwrap()
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
            status_bridge: None,
            shutdown: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            max_parallel_workers: 8,
            max_pipeline_fanout_total: Some(cap),
            checkpoint_store: None,
            hook_executor: None,
            workspace_context: crate::context::PipelineContext::default(),
            host_context: crate::host_context::PipelineHostContext::default(),
        }
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
}
