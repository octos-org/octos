//! RunPipelineTool — implements `octos_agent::Tool` to expose pipeline execution.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use octos_agent::cost_ledger::CostAccountant;
use octos_agent::{Tool, ToolPolicy, ToolResult};
use octos_llm::{LlmProvider, ProviderRouter};
use octos_memory::EpisodeStore;
use serde::Deserialize;

use crate::context::PipelineContext;
use crate::discovery::PipelineDiscovery;
use crate::executor::{ExecutorConfig, PipelineExecutor, PipelineStatusBridge};

/// Tool that runs DOT-based pipelines.
pub struct RunPipelineTool {
    default_provider: Arc<dyn LlmProvider>,
    provider_router: Option<Arc<ProviderRouter>>,
    memory: Arc<EpisodeStore>,
    working_dir: PathBuf,
    provider_policy: Option<ToolPolicy>,
    plugin_dirs: Vec<PathBuf>,
    discovery: PipelineDiscovery,
    /// Per-message status bridge (set via `set_status_bridge` before each call).
    status_bridge: std::sync::Mutex<Option<PipelineStatusBridge>>,
    /// Optional cost accountant (coding-blue FA-7). When set, every
    /// pipeline run reserves a pipeline-level budget at dispatch start
    /// and per-node sub-budgets for LLM-call nodes.
    cost_accountant: Option<Arc<CostAccountant>>,
    /// Logical contract id used when the pipeline context
    /// auto-populates from the workspace policy. Defaults to the
    /// graph id + `"pipeline"` fallback when empty.
    contract_id: Option<String>,
}

impl RunPipelineTool {
    pub fn new(
        default_provider: Arc<dyn LlmProvider>,
        memory: Arc<EpisodeStore>,
        working_dir: PathBuf,
        data_dir: PathBuf,
    ) -> Self {
        let discovery = PipelineDiscovery::new(&data_dir, &working_dir);
        Self {
            default_provider,
            provider_router: None,
            memory,
            working_dir,
            provider_policy: None,
            plugin_dirs: Vec::new(),
            discovery,
            status_bridge: std::sync::Mutex::new(None),
            cost_accountant: None,
            contract_id: None,
        }
    }

    /// Attach a [`CostAccountant`] (coding-blue FA-7). When set, pipeline
    /// executions reserve budget against the configured contract id and
    /// commit the cumulative token attribution at pipeline terminal.
    pub fn with_cost_accountant(mut self, accountant: Arc<CostAccountant>) -> Self {
        self.cost_accountant = Some(accountant);
        self
    }

    /// Set the logical contract id for the cost ledger rollups
    /// associated with this tool. Defaults to the pipeline graph id.
    pub fn with_contract_id(mut self, contract_id: impl Into<String>) -> Self {
        self.contract_id = Some(contract_id.into());
        self
    }

    /// Build the [`PipelineContext`] for a single invocation.
    ///
    /// Reads the workspace policy from `self.working_dir` when present
    /// and attaches the tool's LLM provider for LLM-iterative
    /// compaction. When no policy is found the context is empty —
    /// legacy behaviour intact. This is the adoption path for the
    /// slides + site delivery workflows: a workspace with a
    /// `workspace_policy.toml` automatically opts into terminal
    /// validators + per-node compaction on every `run_pipeline` call
    /// without threading new constructor args.
    /// Build the pipeline workspace context, preferring the parent
    /// session's `CostAccountant` from [`PipelineHostContext`] over the
    /// tool's locally configured one. Keeps the pipeline ledger
    /// attribution consistent with the parent session's accountant when
    /// the tool runs inside a session actor (M8 parity W1.A4).
    fn build_workspace_context_with_host(
        &self,
        host: &crate::host_context::PipelineHostContext,
    ) -> PipelineContext {
        let policy = match octos_agent::workspace_policy::read_workspace_policy(&self.working_dir) {
            Ok(policy) => policy,
            Err(error) => {
                tracing::warn!(
                    working_dir = %self.working_dir.display(),
                    error = %error,
                    "run_pipeline: failed to read workspace policy; running legacy path"
                );
                None
            }
        };
        let mut ctx = PipelineContext::new();
        if let Some(policy) = policy {
            ctx = ctx.with_policy(policy);
            ctx = ctx.with_agent_llm_provider(self.default_provider.clone());
        }
        // Prefer the host-context (parent session's) accountant. Falls
        // back to the tool-configured one for non-session callers.
        if let Some(accountant) = host
            .cost_accountant
            .clone()
            .or_else(|| self.cost_accountant.clone())
        {
            ctx = ctx.with_cost_accountant(accountant);
        }
        if let Some(contract_id) = self.contract_id.as_deref() {
            ctx = ctx.with_contract_id(contract_id);
        }
        ctx
    }

    /// Add the global octos-home skills directory as a search path.
    /// This ensures pipelines installed globally (e.g. `~/.octos/skills/`) are
    /// discoverable even when data_dir is per-profile.
    pub fn with_octos_home(mut self, octos_home: PathBuf) -> Self {
        self.discovery.add_search_path(octos_home.join("skills"));
        self
    }

    pub fn with_provider_router(mut self, router: Arc<ProviderRouter>) -> Self {
        self.provider_router = Some(router);
        self
    }

    pub fn with_provider_policy(mut self, policy: Option<ToolPolicy>) -> Self {
        self.provider_policy = policy;
        self
    }

    pub fn with_plugin_dirs(mut self, dirs: Vec<PathBuf>) -> Self {
        self.plugin_dirs = dirs;
        self
    }

    /// Build a model catalog string for the LLM, showing each model's key,
    /// output capacity, context window, and cost.
    /// Resolve pipeline with fallback: try inline DOT first, if it fails to parse,
    /// try as a named pipeline. This handles cases where the LLM produces slightly
    /// malformed DOT — the pre-built pipeline still works as a safety net.
    async fn resolve_with_fallback(&self, pipeline_str: &str) -> Result<String> {
        let trimmed = pipeline_str.trim();
        let is_inline = trimmed.starts_with("digraph ") || trimmed.starts_with("digraph{");

        if is_inline {
            // Sanitize common LLM DOT mistakes before parsing
            let sanitized = sanitize_dot(trimmed);
            let trimmed = sanitized.as_str();

            // Validate inline DOT parses correctly
            match crate::parser::parse_dot(trimmed) {
                Ok(_) => return Ok(pipeline_str.to_string()),
                Err(parse_err) => {
                    // Log the full DOT for debugging parse failures
                    let dot_preview = if trimmed.len() > 500 {
                        let mut end = 500;
                        while !trimmed.is_char_boundary(end) && end > 0 {
                            end -= 1;
                        }
                        format!(
                            "{}...(truncated at {} bytes)",
                            &trimmed[..end],
                            trimmed.len()
                        )
                    } else {
                        trimmed.to_string()
                    };
                    tracing::warn!(
                        dot = %dot_preview,
                        "inline DOT parse failed, trying named fallback: {parse_err}"
                    );
                    // Try to extract a pipeline name hint from the DOT (e.g. "digraph deep_research")
                    if let Some(name) = trimmed
                        .strip_prefix("digraph ")
                        .and_then(|s| s.split_whitespace().next())
                        .map(|s| s.trim_matches('{'))
                    {
                        if !name.is_empty() {
                            if let Ok(dot) = self.discovery.resolve(name).await {
                                tracing::info!(
                                    name,
                                    "fell back to pre-built pipeline after inline DOT parse failure"
                                );
                                return Ok(dot);
                            }
                        }
                    }
                    // No fallback found — return the original parse error
                    tracing::error!(dot = %dot_preview, "no fallback available, returning parse error");
                    return Err(parse_err.wrap_err("inline DOT parse failed with no fallback"));
                }
            }
        }

        // Named pipeline or file path — use normal resolution
        self.discovery.resolve(pipeline_str).await
    }

    /// Set the status bridge for the current message.
    /// Called per-message to connect pipeline progress to the messaging channel's
    /// StatusIndicator (status words + token tracker).
    pub fn set_status_bridge(&self, bridge: PipelineStatusBridge) {
        *self.status_bridge.lock().unwrap_or_else(|e| e.into_inner()) = Some(bridge);
    }
}

#[derive(Deserialize)]
struct Input {
    pipeline: String,
    input: String,
    #[serde(default)]
    variables: serde_json::Map<String, serde_json::Value>,
    /// Pipeline-level timeout in seconds. Default: 1800 (30 min). Max: 1800.
    #[serde(default)]
    timeout_secs: Option<u64>,
}

#[async_trait]
impl Tool for RunPipelineTool {
    fn name(&self) -> &str {
        "run_pipeline"
    }

    fn description(&self) -> &str {
        "Execute a multi-step pipeline defined as an inline DOT graph. Each node runs a \
         specialized agent with its own prompt, model, and output limits. \
         ALWAYS write inline DOT graphs — do NOT use pre-built pipeline names. \
         This lets you pick optimal models per node (cheap for search, high-output for synthesis)."
    }

    fn tags(&self) -> &[&str] {
        &["gateway"]
    }

    fn input_schema(&self) -> serde_json::Value {
        let adaptive_hints = include_str!("prompts/adaptive_hints.txt");
        let node_attrs = include_str!("prompts/node_attrs.txt");
        let example = include_str!("prompts/example_dot.txt");

        let pipeline_desc = format!(
            "Inline DOT graph. ALWAYS write a custom digraph.\n\n\
             Do NOT specify model= attributes — the system selects optimal models automatically.\n\
             Focus on writing good prompts, choosing tools, and structuring the pipeline.\n\n\
             {node_attrs}\n\n\
             {adaptive_hints}\n\n\
             {example}"
        );

        serde_json::json!({
            "type": "object",
            "properties": {
                "pipeline": {
                    "type": "string",
                    "description": pipeline_desc
                },
                "input": {
                    "type": "string",
                    "description": "The input query or task description for the pipeline"
                },
                "variables": {
                    "type": "object",
                    "description": "Optional key-value pairs for template substitution in node prompts",
                    "additionalProperties": { "type": "string" }
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Timeout in seconds. Estimate based on real execution times: simple 2-node pipeline ~3min → 300s; standard 3-node research pipeline ~8min → 600s; 5-7 topic deep research with crawl+synthesize ~15-20min → 1200s; complex multi-source analysis with many nodes ~25min → 1500s. Max: 1800. Default: 1800"
                }
            },
            "required": ["pipeline", "input"]
        })
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let input: Input =
            serde_json::from_value(args.clone()).wrap_err("invalid run_pipeline input")?;

        let is_inline = input.pipeline.trim().starts_with("digraph ");
        tracing::info!(
            inline = is_inline,
            pipeline_arg = if is_inline {
                "(inline DOT)"
            } else {
                &input.pipeline
            },
            "run_pipeline invoked"
        );

        let dot_content = self.resolve_with_fallback(&input.pipeline).await?;

        let status_bridge = self
            .status_bridge
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();

        // Shutdown signal for cancelling all pipeline workers on timeout/drop.
        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));

        // M8 parity (W1.A1/A3/A4): pull the parent session's shared
        // FileStateCache, SubAgentOutputRouter, AgentSummaryGenerator,
        // TaskSupervisor, and CostAccountant from TOOL_CTX so pipeline
        // workers inherit them via the M8 contract instead of
        // constructing fresh per-run handles. Falls back to whatever
        // self holds when the tool is invoked outside of a session
        // (e.g. unit tests).
        let host_context = octos_agent::tools::TOOL_CTX
            .try_with(crate::host_context::PipelineHostContext::from_tool_context)
            .unwrap_or_default();

        let config = ExecutorConfig {
            default_provider: self.default_provider.clone(),
            provider_router: self.provider_router.clone(),
            memory: self.memory.clone(),
            working_dir: self.working_dir.clone(),
            provider_policy: self.provider_policy.clone(),
            plugin_dirs: self.plugin_dirs.clone(),
            status_bridge,
            shutdown: shutdown.clone(),
            max_parallel_workers: 8,
            max_pipeline_fanout_total: None,
            checkpoint_store: None,
            hook_executor: None,
            // coding-blue FA-7: adopt workspace-contract enforcement.
            // Reads the policy from the working dir on every call so
            // the slides + site delivery workflows (and any other
            // opted-in workflow) get validator + compaction + cost
            // reservation for free. When no policy is present the
            // context is empty and the executor stays on the legacy
            // path.
            workspace_context: self.build_workspace_context_with_host(&host_context),
            host_context,
        };

        // Pipeline-level timeout: default 1800s (30 min), clamped to [60, 1800].
        let timeout_secs = input.timeout_secs.unwrap_or(1800).clamp(60, 1800);

        let executor = PipelineExecutor::new(config);
        let result = tokio::time::timeout(
            Duration::from_secs(timeout_secs),
            executor.run(&dot_content, &input.input, &input.variables),
        )
        .await;

        // Signal shutdown to all workers regardless of how we finished
        shutdown.store(true, std::sync::atomic::Ordering::Release);

        let result = result.map_err(|_| {
            eyre::eyre!(
                "pipeline timed out after {}s (timeout_secs={})",
                timeout_secs,
                timeout_secs
            )
        })??;

        let summary = result
            .node_summaries
            .iter()
            .map(|n| {
                format!(
                    "- {} ({}): {}ms, {}+{} tokens",
                    n.node_id,
                    n.model.as_deref().unwrap_or("default"),
                    n.duration_ms,
                    n.token_usage.input_tokens,
                    n.token_usage.output_tokens,
                )
            })
            .collect::<Vec<_>>()
            .join("\n");

        // Find the report file from this pipeline run's actual files_modified.
        // The session actor auto-delivers .md files via file_modified on ToolResult,
        // so no LLM instruction needed.
        // Ensure absolute path so session actor can find and deliver the file.
        let report_file = result
            .files_modified
            .iter()
            .find(|f| {
                let name = f.file_name().and_then(|n| n.to_str()).unwrap_or("");
                name.ends_with(".md") && !name.starts_with("_search")
            })
            .map(|f| {
                if f.is_absolute() {
                    f.clone()
                } else {
                    std::fs::canonicalize(f).unwrap_or_else(|_| f.clone())
                }
            });
        if let Some(ref path) = report_file {
            tracing::info!(file = %path.display(), "pipeline produced report file");
        }

        // Also set files_to_send so the execution loop auto-delivers
        let files_to_send = report_file.iter().filter(|p| p.exists()).cloned().collect();

        // Surface per-node cost attribution in the structured side-channel so
        // the session actor can pull it back into the SSE `done` event for the
        // W1.G4 cost panel. The data was being silently dropped at the tool
        // boundary before we extended `ToolResult` with `structured_metadata`.
        let structured_metadata = node_costs_metadata(&result.node_costs);

        Ok(ToolResult {
            output: format!(
                "{}\n\n---\nPipeline execution summary:\n{summary}\nTotal: {} input + {} output tokens",
                result.output, result.token_usage.input_tokens, result.token_usage.output_tokens,
            ),
            success: result.success,
            tokens_used: Some(result.token_usage),
            file_modified: report_file,
            files_to_send,
            structured_metadata,
        })
    }
}

/// Project a non-empty slice of [`NodeCost`] rows into the
/// `ToolResult.structured_metadata` shape the session actor consumes.
///
/// Returns `None` when there are no cost rows so the side-channel stays
/// absent for legacy callers (no accountant / no LLM-call nodes); returns
/// `Some({"node_costs": [...]})` otherwise. Lifted out so tests can assert
/// the projection without standing up a full pipeline run.
fn node_costs_metadata(rows: &[crate::executor::NodeCost]) -> Option<serde_json::Value> {
    if rows.is_empty() {
        None
    } else {
        Some(serde_json::json!({
            "node_costs": rows,
        }))
    }
}

/// Sanitize common LLM DOT mistakes that would cause parse failures.
fn sanitize_dot(dot: &str) -> String {
    let mut result = dot.to_string();

    // Fix: digraph{ → digraph {
    if result.contains("digraph{") {
        result = result.replace("digraph{", "digraph pipeline {");
    }

    // Fix: digraph { (no name) → digraph pipeline {
    // The parser now handles this, but belt-and-suspenders
    if result.starts_with("digraph {") || result.starts_with("digraph  {") {
        result = result.replacen("digraph", "digraph pipeline", 1);
    }

    // Fix: markdown code fences around DOT
    if result.starts_with("```") {
        // Strip ```dot or ```graphviz or ``` prefix/suffix
        let lines: Vec<&str> = result.lines().collect();
        let start = if lines.first().map(|l| l.starts_with("```")).unwrap_or(false) {
            1
        } else {
            0
        };
        let end = if lines.last().map(|l| l.trim() == "```").unwrap_or(false) {
            lines.len() - 1
        } else {
            lines.len()
        };
        result = lines[start..end].join("\n");
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::NodeCost;

    /// Gap 3.1 — when a pipeline run reports per-node cost rows, the tool
    /// surfaces them in `ToolResult.structured_metadata` under the
    /// `"node_costs"` key so the session actor can project them onto the
    /// SSE `done` event for the W1.G4 CostBreakdown panel.
    #[test]
    fn node_costs_metadata_emits_node_costs_array_for_multi_node_pipeline() {
        let rows = vec![
            NodeCost {
                node_id: "draft".into(),
                model: Some("anthropic/claude-haiku".into()),
                reserved_usd: 0.0010,
                actual_usd: 0.0008,
                tokens_in: 320,
                tokens_out: 110,
                committed: true,
            },
            NodeCost {
                node_id: "refine".into(),
                model: Some("anthropic/claude-sonnet".into()),
                reserved_usd: 0.0040,
                actual_usd: 0.0032,
                tokens_in: 540,
                tokens_out: 220,
                committed: true,
            },
        ];

        let meta = node_costs_metadata(&rows).expect("multi-node pipeline must surface metadata");
        let arr = meta
            .get("node_costs")
            .and_then(|v| v.as_array())
            .expect("structured_metadata must carry a `node_costs` array");
        assert_eq!(arr.len(), 2, "one row per pipeline node");
        assert_eq!(
            arr[0].get("node_id").and_then(|v| v.as_str()),
            Some("draft")
        );
        assert_eq!(
            arr[1].get("node_id").and_then(|v| v.as_str()),
            Some("refine")
        );
        assert!(
            arr[0]
                .get("tokens_in")
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
                > 0,
            "tokens_in must be threaded through the projection"
        );
        assert!(
            arr[0]
                .get("actual_usd")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0)
                > 0.0,
            "actual_usd must be threaded through the projection"
        );
    }

    /// When a pipeline runs without an accountant attached, no per-node cost
    /// rows are produced; the side-channel stays absent so legacy callers
    /// observe byte-identical behaviour.
    #[test]
    fn node_costs_metadata_returns_none_for_empty_rows() {
        assert!(node_costs_metadata(&[]).is_none());
    }
}
