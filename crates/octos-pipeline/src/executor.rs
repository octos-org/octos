//! Pipeline execution engine — walks the graph, executes handlers, selects edges.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eyre::{Result, WrapErr};
use octos_agent::TokenTracker;
use octos_agent::progress::ProgressEvent;
use octos_agent::tools::TOOL_CTX;
use octos_core::{Message, MessageRole, TokenUsage};
use octos_llm::{ChatConfig, LlmProvider, ProviderRouter};
use octos_memory::EpisodeStore;
use serde::Deserialize;
use tracing::{info, warn};

use crate::condition;
use crate::graph::{
    HandlerKind, NodeOutcome, NodeSummary, OutcomeStatus, PipelineEdge, PipelineGraph, PipelineNode,
};
use crate::handler::{
    CodergenHandler, GateHandler, HandlerContext, HandlerRegistry, NoopHandler, ShellHandler,
};
use crate::parser::parse_dot;
use crate::validate;

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
}

/// A single planned sub-task from the LLM planner.
///
/// Accepts multiple field name variants because different LLMs use different
/// names for the same concept (task/query/topic/angle/description).
#[derive(Debug, Clone, Deserialize)]
struct DynamicTask {
    #[serde(alias = "query", alias = "topic", alias = "angle", alias = "description", alias = "search", alias = "instruction")]
    task: String,
    #[serde(default, alias = "name", alias = "title")]
    label: Option<String>,
}

/// Report pipeline progress via the task-local TOOL_CTX reporter (if available).
fn report_progress(message: &str) {
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
            timestamp: chrono::Utc::now(),
        },
        Message {
        role: MessageRole::User,
        content: prompt,
        media: vec![],
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: None,
        timestamp: chrono::Utc::now(),
    }];

    let config = ChatConfig {
        max_tokens: Some(2000),
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
                    eyre::eyre!("failed to parse planning JSON as array of objects: {e}\nJSON: {preview}")
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
    (merged_content, any_error, summaries, total_tokens, outcomes)
}

/// The main pipeline executor.
pub struct PipelineExecutor {
    config: ExecutorConfig,
}

impl PipelineExecutor {
    pub fn new(config: ExecutorConfig) -> Self {
        Self { config }
    }

    /// Run a pipeline from a DOT string.
    pub async fn run(
        &self,
        dot_content: &str,
        user_input: &str,
        variables: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<PipelineResult> {
        // Parse and validate
        let graph = parse_dot(dot_content).wrap_err("failed to parse pipeline DOT")?;
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

        // Build handler registry
        let handlers = self.build_handlers();

        // Find start node
        let start_node = validate::find_start_node(&graph)
            .ok_or_else(|| eyre::eyre!("no start node found in pipeline"))?;

        // Execute graph
        self.execute_graph(&graph, &handlers, &start_node, user_input, variables)
            .await
    }

    fn build_handlers(&self) -> HandlerRegistry {
        let mut registry = HandlerRegistry::new();

        let mut codergen = CodergenHandler::new(
            self.config.default_provider.clone(),
            self.config.memory.clone(),
            self.config.working_dir.clone(),
        )
        .with_provider_policy(self.config.provider_policy.clone())
        .with_plugin_dirs(self.config.plugin_dirs.clone());

        if let Some(ref router) = self.config.provider_router {
            codergen = codergen.with_provider_router(router.clone());
        }

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
        // Nodes already executed by a parallel fan-out (skip in normal traversal)
        let mut parallel_executed: HashSet<String> = HashSet::new();

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

                let fan_start = Instant::now();

                // Prepare and execute all targets concurrently
                let total_targets = targets.len();
                let par_completed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let mut futures = Vec::new();
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

                    futures.push(async move {
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
                }

                let results = futures::future::join_all(futures).await;

                let (merged_content, any_error, worker_summaries, worker_tokens, outcomes) =
                    process_worker_results(results, self.config.status_bridge.as_ref());

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

                // Resolve worker model
                let worker_model = node.model.clone().or_else(|| graph.default_model.clone());

                let mut synthetic_nodes: Vec<(String, PipelineNode)> = Vec::new();
                for (i, task) in tasks.iter().enumerate() {
                    let task_id = format!("{}_task_{i}", node.id);
                    let prompt = worker_prompt_template.replace("{task}", &task.task);
                    let label = task
                        .label
                        .clone()
                        .unwrap_or_else(|| format!("Task {}", i + 1));

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

                // Get the codergen handler for executing synthetic nodes
                let codergen_handler = handlers.get(&HandlerKind::Codergen).ok_or_else(|| {
                    eyre::eyre!("codergen handler not found for dynamic_parallel workers")
                })?;

                // Execute all synthetic nodes concurrently
                let total_workers = synthetic_nodes.len();
                let completed_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let mut futures = Vec::new();
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
                }

                let results = futures::future::join_all(futures).await;

                let (merged_content, any_error, worker_summaries, worker_tokens, outcomes) =
                    process_worker_results(results, self.config.status_bridge.as_ref());

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

            let ctx = HandlerContext {
                input: input_text,
                completed: completed.clone(),
                working_dir: self.config.working_dir.clone(),
            };

            // Update status words for this sequential node
            if let Some(ref bridge) = self.config.status_bridge {
                let label = node.label.as_deref().unwrap_or(&node.id);
                bridge.set_words(vec![label.to_string()]);
            }

            let seq_label = node.label.as_deref().unwrap_or(&node.id);
            report_progress(&format!("{seq_label}: running..."));

            info!(
                node = %node.id,
                handler = ?node.handler,
                model = ?node_with_prompt.model,
                "executing pipeline node"
            );

            let node_start = Instant::now();

            // Execute with retries
            let outcome = self
                .execute_with_retries(handler, &node_with_prompt, &ctx, node.max_retries)
                .await?;

            let duration_ms = node_start.elapsed().as_millis() as u64;

            report_progress(&format!(
                "{seq_label}: done ({:.0}s)",
                duration_ms as f64 / 1000.0
            ));

            info!(
                node = %node.id,
                status = ?outcome.status,
                duration_ms,
                tokens_in = outcome.token_usage.input_tokens,
                tokens_out = outcome.token_usage.output_tokens,
                "node completed"
            );

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
                return Ok(PipelineResult {
                    output: outcome.content,
                    success: true,
                    token_usage: total_tokens,
                    node_summaries: summaries,
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
                    return Ok(PipelineResult {
                        output: outcome.content,
                        success: outcome.status == OutcomeStatus::Pass,
                        token_usage: total_tokens,
                        node_summaries: summaries,
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
}
