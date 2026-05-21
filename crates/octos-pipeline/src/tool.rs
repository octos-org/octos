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
use crate::executor::{ExecutorConfig, PipelineExecutor, PipelineResult, PipelineStatusBridge};
use crate::run_dir::{PipelineRunSummary, RunDir};
use octos_core::TokenUsage;

/// #1020 / M17-B — reason string stamped onto every pipeline run's
/// `summary.json` because pipeline workers do not yet propagate the
/// parent's `ContextManager`. Evidence validators look for this reason
/// to confirm the acceptance bullet is satisfied.
pub const PIPELINE_EXTERNAL_CONTEXT_UNMANAGED_REASON: &str =
    "pipeline workers don't yet propagate ContextManager (M17-B)";

/// Tool that runs DOT-based pipelines.
pub struct RunPipelineTool {
    default_provider: Arc<dyn LlmProvider>,
    provider_router: Option<Arc<ProviderRouter>>,
    memory: Arc<EpisodeStore>,
    working_dir: PathBuf,
    provider_policy: Option<ToolPolicy>,
    plugin_dirs: Vec<PathBuf>,
    /// Section B (codex review P1.1): pipeline-level strict-signing
    /// policy. Defaults to `false` (legacy permissive path). When the
    /// host has opted into `plugins.require_signed`, this is set via
    /// [`Self::with_plugin_require_signed`] so per-node plugin loads
    /// enforce the same gate.
    plugin_require_signed: bool,
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
            plugin_require_signed: false,
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

    /// Section B (codex review P1.1): opt into strict signature
    /// enforcement for pipeline-spawned plugin loads. Inherited from
    /// `plugins.require_signed` on the host config.
    pub fn with_plugin_require_signed(mut self, require_signed: bool) -> Self {
        self.plugin_require_signed = require_signed;
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
        "Run a sanctioned multi-step pipeline by NAME. The only currently \
         sanctioned pipeline is `deep_research` — use it when the user asks \
         for in-depth, multi-source, source-citing research that needs \
         parallel search workers + synthesis. Do NOT compose your own \
         inline DOT graph for ad-hoc tasks (slides, media, code edits, \
         partial regenerations, etc.) — those have purpose-built tools \
         (`mofa_slides`, `podcast_generate`, etc.). If no purpose-built \
         tool exists for what the user asked, surface that as a limitation \
         rather than improvising a custom pipeline."
    }

    fn tags(&self) -> &[&str] {
        &["gateway"]
    }

    fn input_schema(&self) -> serde_json::Value {
        let pipeline_desc = "Name of the sanctioned pipeline to run. The only currently \
             sanctioned name is `deep_research`. Do NOT pass an inline \
             DOT graph here — inline DOT was the legacy free-form \
             contract; the executor still accepts it for operator \
             debugging but agent-driven runs MUST use the name form. If \
             you find yourself wanting to compose your own DOT, the \
             correct response is to use the purpose-built tool for that \
             domain (`mofa_slides` for slides, `podcast_generate` for \
             podcasts, `voice_synthesize` for TTS, etc.), or tell the \
             user no such tool exists for their request."
            .to_string();

        serde_json::json!({
            "type": "object",
            "properties": {
                "pipeline": {
                    "type": "string",
                    "description": pipeline_desc,
                    "enum": ["deep_research"]
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

    /// Synchronously parse and structurally validate the DOT graph before
    /// the spawn_only intercept dispatches the actual run to the background.
    ///
    /// Without this pre-flight, an LLM-generated invalid DOT (e.g. multiple
    /// dangling roots → `rule 1: ambiguous start`) failed inside the
    /// background task and surfaced only as a user-visible error bubble —
    /// the agent's foreground turn already returned "started in background"
    /// to the LLM, so the model thought it succeeded and never retried.
    /// Catching the bad shape here turns the failure into a tool_result the
    /// LLM can react to in its next iteration.
    ///
    /// Scope is deliberately limited to `parse_dot` + the same `validate::`
    /// lint pass the executor runs — model assignment is skipped because
    /// the topology checks (`ambiguous start`, dangling refs, etc.) are
    /// what the LLM gets wrong; model fields are auto-filled by the
    /// executor and never the failure source.
    async fn pre_flight_validate(&self, args: &serde_json::Value) -> Result<(), String> {
        let input: Input = serde_json::from_value(args.clone())
            .map_err(|e| format!("invalid run_pipeline input: {e}"))?;
        let dot_content = self
            .resolve_with_fallback(&input.pipeline)
            .await
            .map_err(|e| format!("failed to resolve pipeline DOT: {e}"))?;
        let graph = crate::parser::parse_dot(&dot_content)
            .map_err(|e| format!("failed to parse pipeline DOT: {e}"))?;
        let diags = crate::validate::validate(&graph);
        if crate::validate::has_errors(&diags) {
            let errors: Vec<_> = diags
                .iter()
                .filter(|d| d.severity == crate::validate::Severity::Error)
                .map(|d| format!("rule {}: {}", d.rule, d.message))
                .collect();
            return Err(format!(
                "pipeline validation failed:\n{}",
                errors.join("\n")
            ));
        }
        Ok(())
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

        // #1020 / M17-B: capture run start so we can stamp the summary's
        // `start_time` field with the same instant the pipeline launched.
        // RFC3339 keeps the audit-trail JSON human-readable.
        let run_started_at = std::time::SystemTime::now();
        let run_start_rfc3339 = systemtime_to_rfc3339(run_started_at);
        let pipeline_started = std::time::Instant::now();

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
            plugin_require_signed: self.plugin_require_signed,
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

        // #1126 codex P2: compute the run_id + graph_id BEFORE we
        // branch on success vs timeout. The marker write must happen
        // on the timeout path too (the prior shape only emitted on
        // success), otherwise timed-out runs were the one scenario
        // missing audit-trail evidence — exactly the runs validators
        // most need to inspect.
        let graph_id = graph_id_from_dot(&dot_content);
        let run_id = generate_run_id(&graph_id, run_started_at);

        let result = match result {
            Ok(inner) => inner?,
            Err(_) => {
                let duration_ms =
                    u64::try_from(pipeline_started.elapsed().as_millis()).unwrap_or(u64::MAX);
                emit_external_context_unmanaged_timeout_summary(
                    &self.working_dir,
                    &run_id,
                    &graph_id,
                    duration_ms,
                    &run_start_rfc3339,
                    timeout_secs,
                );
                return Err(eyre::eyre!(
                    "pipeline timed out after {}s (timeout_secs={})",
                    timeout_secs,
                    timeout_secs
                ));
            }
        };

        // #1020 / M17-B: stamp the run's `summary.json` with the
        // `external_context_unmanaged` marker so evidence validators can
        // confirm pipeline workers ran without the parent's ContextManager
        // propagated. Failures are logged at WARN and never bubble up:
        // missing audit trail must not regress the user-visible outcome.
        let duration_ms = u64::try_from(pipeline_started.elapsed().as_millis()).unwrap_or(u64::MAX);
        emit_external_context_unmanaged_summary(
            &self.working_dir,
            &run_id,
            &graph_id,
            &result,
            duration_ms,
            &run_start_rfc3339,
        );

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
        let real_report_file = result
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

        // run_pipeline is registered as spawn_only, so the execution-loop
        // background-success branch in `crates/octos-agent/src/agent/execution.rs`
        // requires `files_to_send` to be non-empty (otherwise it marks the task
        // failed with "no output files produced"). Inline DOT pipelines that
        // only return text in `result.output` produce no .md report. Synthesize
        // one so the spawn_only delivery path always has a payload to attach.
        let synthesized_report_file = if real_report_file.is_none() && !result.output.is_empty() {
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let pid = std::process::id();
            let filename = format!("run_pipeline_{timestamp}_{pid}.md");
            let dir = std::env::temp_dir().join("octos_pipeline_synthetic");
            match std::fs::create_dir_all(&dir).and_then(|_| {
                let path = dir.join(&filename);
                std::fs::write(&path, &result.output).map(|_| path)
            }) {
                Ok(path) => {
                    tracing::info!(file = %path.display(), "wrote synthetic pipeline report");
                    Some(path)
                }
                Err(e) => {
                    tracing::warn!(error = %e, "failed to write synthetic pipeline report");
                    None
                }
            }
        } else {
            None
        };

        let report_file = real_report_file.or(synthesized_report_file);
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
            named_outputs: None,
        })
    }
}

/// #1020 / M17-B — Build a [`PipelineRunSummary`] stamped with the
/// `external_context_unmanaged` marker for a completed pipeline run.
///
/// `RunPipelineTool` constructs this for every run because pipeline
/// workers don't yet propagate the parent's `ContextManager` — workers
/// run with per-node prompt context instead. Evidence validators look
/// for `context_mode = "external_context_unmanaged"` plus the reason
/// string to confirm M17-B's acceptance bullet is satisfied.
///
/// `start_time_rfc3339` should be a caller-supplied RFC3339 timestamp
/// (the pipeline-run start) so the summary on disk is comparable across
/// runs and matches the `RunDir` audit trail. We accept it as a string
/// to keep this helper dependency-free of `chrono`.
pub(crate) fn build_pipeline_run_summary(
    graph_id: impl Into<String>,
    result: &PipelineResult,
    duration_ms: u64,
    start_time_rfc3339: impl Into<String>,
) -> PipelineRunSummary {
    PipelineRunSummary {
        graph_id: graph_id.into(),
        success: result.success,
        duration_ms,
        total_tokens: result.token_usage.clone(),
        nodes_executed: result.node_summaries.len(),
        start_time: start_time_rfc3339.into(),
        context_mode: None,
        context_reason: None,
    }
    .with_external_context_unmanaged(PIPELINE_EXTERNAL_CONTEXT_UNMANAGED_REASON)
}

/// #1126 codex P2 follow-up to #1020 / M17-B — write a `summary.json`
/// for the timeout failure path. Without this, runs that hit the
/// pipeline-level timeout had no audit-trail marker at all, even
/// though pipeline workers had been launched and consumed budget.
/// Records `success: false`, a `duration_ms` equal to the elapsed
/// wall-clock at the timeout boundary, zero node summaries, and the
/// same `external_context_unmanaged` marker so validators see a
/// consistent shape for both success and failure paths.
fn emit_external_context_unmanaged_timeout_summary(
    working_dir: &std::path::Path,
    run_id: &str,
    graph_id: &str,
    duration_ms: u64,
    start_time_rfc3339: &str,
    timeout_secs: u64,
) {
    let run_dir = match RunDir::new(working_dir, run_id) {
        Ok(dir) => dir,
        Err(error) => {
            tracing::warn!(
                run_id,
                error = %error,
                "failed to open run dir for M17-B timeout summary; skipping"
            );
            return;
        }
    };
    let reason = format!(
        "{PIPELINE_EXTERNAL_CONTEXT_UNMANAGED_REASON}; pipeline timed out after {timeout_secs}s"
    );
    let summary = PipelineRunSummary {
        graph_id: graph_id.to_string(),
        success: false,
        duration_ms,
        total_tokens: TokenUsage::default(),
        nodes_executed: 0,
        start_time: start_time_rfc3339.to_string(),
        context_mode: None,
        context_reason: None,
    }
    .with_external_context_unmanaged(reason);
    if let Err(error) = run_dir.write_summary(&summary) {
        tracing::warn!(
            run_id,
            error = %error,
            "failed to write M17-B timeout summary; downstream evidence validators may flag this run"
        );
    }
}

/// #1020 / M17-B — write a `summary.json` carrying the
/// `external_context_unmanaged` marker to the run's `.octos/runs/<run_id>/`
/// directory. Failures are logged at WARN and never propagated so the
/// pipeline's user-visible outcome is unchanged when the audit-trail
/// write fails (e.g. read-only filesystem during tests).
fn emit_external_context_unmanaged_summary(
    working_dir: &std::path::Path,
    run_id: &str,
    graph_id: &str,
    result: &PipelineResult,
    duration_ms: u64,
    start_time_rfc3339: &str,
) {
    let run_dir = match RunDir::new(working_dir, run_id) {
        Ok(dir) => dir,
        Err(error) => {
            tracing::warn!(
                run_id,
                error = %error,
                "failed to open run dir for M17-B context-mode summary; skipping"
            );
            return;
        }
    };
    let summary = build_pipeline_run_summary(graph_id, result, duration_ms, start_time_rfc3339);
    if let Err(error) = run_dir.write_summary(&summary) {
        tracing::warn!(
            run_id,
            error = %error,
            "failed to write M17-B context-mode summary; downstream evidence validators may flag this run"
        );
    }
}

/// Extract the graph identifier from the resolved DOT body. Falls back
/// to `"pipeline"` when the header lacks an explicit name (matches the
/// sanitiser's `digraph { ... }` -> `digraph pipeline { ... }` rewrite).
fn graph_id_from_dot(dot_content: &str) -> String {
    let header = dot_content
        .trim_start()
        .strip_prefix("digraph")
        .map(|rest| rest.trim_start())
        .unwrap_or("");
    let candidate: String = header
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect();
    if candidate.is_empty() {
        "pipeline".to_string()
    } else {
        candidate
    }
}

/// Build a filesystem-safe run id of the form
/// `<graph_id>-<unix_secs>-<nanos>-<pid>-<counter>`.
/// Matches the `validate_pipeline_id` constraint (no `/`, `\`, `..`, control
/// chars, <= 128 bytes) and stays unique across simultaneous runs of the
/// same pipeline so two writers do not race on `summary.json`.
///
/// #1126 codex P2: the prior shape `{graph}-{secs}-{pid}` collided when
/// two `run_pipeline` calls for the same graph started in the same
/// second within the same process. Nanosecond resolution + a
/// per-process monotonic counter make collision practically impossible
/// even for back-to-back synchronous fan-out.
fn generate_run_id(graph_id: &str, started_at: std::time::SystemTime) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static RUN_COUNTER: AtomicU64 = AtomicU64::new(0);

    let (secs, nanos) = started_at
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| (d.as_secs(), d.subsec_nanos()))
        .unwrap_or((0, 0));
    let pid = std::process::id();
    let counter = RUN_COUNTER.fetch_add(1, Ordering::Relaxed);
    // Sanitize graph_id defensively — `graph_id_from_dot` already strips
    // unsafe chars but a caller-provided value could be anything.
    let safe_graph: String = graph_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    let candidate = format!("{safe_graph}-{secs}-{nanos:09}-{pid}-{counter}");
    if candidate.is_empty() || candidate.len() > 128 {
        format!("pipeline-{secs}-{nanos:09}-{pid}-{counter}")
    } else {
        candidate
    }
}

/// Format a `SystemTime` as a coarse RFC3339 timestamp without pulling
/// in `chrono`. Falls back to the unix epoch on clock skew.
fn systemtime_to_rfc3339(t: std::time::SystemTime) -> String {
    let secs = t
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    // Inline a minimal date renderer: we only need year/month/day/hour/min/sec
    // for the audit trail. `chrono` is intentionally not pulled in here —
    // keeping octos-pipeline's deps unchanged is a hard rule for #1020.
    let (year, month, day, hour, min, sec) = unix_secs_to_ymdhms(secs);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}Z")
}

/// Convert unix seconds (UTC) into (year, month, day, hour, minute, second).
/// Handles dates from 1970-01-01 through 9999-12-31. Returns the epoch on
/// negative values (clock skew).
fn unix_secs_to_ymdhms(secs: i64) -> (i32, u32, u32, u32, u32, u32) {
    if secs < 0 {
        return (1970, 1, 1, 0, 0, 0);
    }
    let total_secs = secs as u64;
    let sec = (total_secs % 60) as u32;
    let total_mins = total_secs / 60;
    let min = (total_mins % 60) as u32;
    let total_hours = total_mins / 60;
    let hour = (total_hours % 24) as u32;
    let mut days = (total_hours / 24) as i64;

    // Compute year/month/day from days-since-epoch (1970-01-01).
    let mut year: i32 = 1970;
    loop {
        let leap = (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0);
        let year_days = if leap { 366 } else { 365 };
        if days < year_days as i64 {
            break;
        }
        days -= year_days as i64;
        year += 1;
    }
    let leap = (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0);
    let month_lens: [u32; 12] = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut month: u32 = 1;
    for &m_len in &month_lens {
        if days < m_len as i64 {
            break;
        }
        days -= m_len as i64;
        month += 1;
    }
    let day = (days as u32) + 1;
    (year, month, day, hour, min, sec)
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

    /// #1020 / M17-B — `build_pipeline_run_summary` MUST stamp the
    /// summary with `context_mode = "external_context_unmanaged"` plus
    /// the canonical M17-B reason string. Evidence validators grep
    /// these fields off `summary.json`, so any drift here silently
    /// breaks the M17-B acceptance bullet for `run_pipeline`.
    #[test]
    fn build_pipeline_run_summary_stamps_external_context_unmanaged_marker() {
        use octos_core::TokenUsage;
        let result = PipelineResult {
            output: "ok".into(),
            success: true,
            token_usage: TokenUsage::default(),
            node_summaries: Vec::new(),
            files_modified: Vec::new(),
            node_costs: Vec::new(),
        };
        let summary =
            build_pipeline_run_summary("test_pipeline", &result, 1234, "2026-05-20T17:00:00Z");
        assert_eq!(summary.graph_id, "test_pipeline");
        assert_eq!(
            summary.context_mode.as_deref(),
            Some("external_context_unmanaged"),
            "every run_pipeline summary must carry the M17-B marker"
        );
        assert_eq!(
            summary.context_reason.as_deref(),
            Some(PIPELINE_EXTERNAL_CONTEXT_UNMANAGED_REASON),
            "the marker reason must match the canonical M17-B constant"
        );
        // The serialized JSON form is what evidence validators actually
        // see on disk — assert the wire shape directly.
        let json = serde_json::to_value(&summary).unwrap();
        assert_eq!(json["context_mode"], "external_context_unmanaged");
        assert!(
            json["context_reason"]
                .as_str()
                .unwrap_or("")
                .contains("M17-B"),
            "context_reason should reference M17-B for grep-ability"
        );
    }

    /// #1020 / M17-B — `emit_external_context_unmanaged_summary` writes
    /// the marker-stamped summary to disk under
    /// `<working_dir>/.octos/runs/<run_id>/summary.json` so the audit
    /// trail satisfies the M17-B evidence requirement at runtime.
    #[test]
    fn emit_external_context_unmanaged_summary_writes_marker_to_disk() {
        use octos_core::TokenUsage;
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let result = PipelineResult {
            output: "ok".into(),
            success: true,
            token_usage: TokenUsage::default(),
            node_summaries: Vec::new(),
            files_modified: Vec::new(),
            node_costs: Vec::new(),
        };
        emit_external_context_unmanaged_summary(
            dir.path(),
            "deep_research-1747800000-12345",
            "deep_research",
            &result,
            5000,
            "2026-05-20T17:00:00Z",
        );
        let summary_path = dir
            .path()
            .join(".octos/runs/deep_research-1747800000-12345/summary.json");
        assert!(
            summary_path.exists(),
            "RunPipelineTool must persist summary.json with the M17-B marker"
        );
        let contents = std::fs::read_to_string(&summary_path).unwrap();
        let json: serde_json::Value = serde_json::from_str(&contents).unwrap();
        assert_eq!(json["context_mode"], "external_context_unmanaged");
        assert_eq!(json["graph_id"], "deep_research");
        assert_eq!(
            json["context_reason"], PIPELINE_EXTERNAL_CONTEXT_UNMANAGED_REASON,
            "summary.json must carry the canonical M17-B reason"
        );
    }

    /// Run-id generator must produce a `validate_pipeline_id`-safe id
    /// even when the graph_id contains unsafe characters (slash, dot,
    /// control bytes). Without this defensive sanitization a maliciously
    /// named pipeline would fail to write `summary.json` and the M17-B
    /// marker would be silently dropped.
    #[test]
    fn generate_run_id_is_pipeline_id_safe() {
        use std::time::{Duration, UNIX_EPOCH};
        let t = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let id = generate_run_id("ev/il..\\name", t);
        assert!(crate::graph::validate_pipeline_id(&id).is_ok());
        // The unix secs anchor + the original name's safe chars should
        // be preserved so operators can correlate the run with its logs.
        assert!(id.contains("1700000000"));
    }

    /// `graph_id_from_dot` extracts the digraph name when present and
    /// falls back to `"pipeline"` for anonymous graphs. The fallback
    /// path is what inline-DOT LLM calls use, so missing it would mean
    /// inline runs write `summary.json` under an empty-string run id —
    /// which `validate_pipeline_id` rejects.
    #[test]
    fn graph_id_from_dot_uses_pipeline_fallback_for_anonymous_graphs() {
        assert_eq!(
            graph_id_from_dot("digraph deep_research { a -> b }"),
            "deep_research"
        );
        assert_eq!(graph_id_from_dot("digraph { a -> b }"), "pipeline");
        assert_eq!(graph_id_from_dot("  digraph  research_42 {"), "research_42");
    }

    /// #1126 codex P2 acceptance: two run_pipeline calls for the same
    /// graph that start within the same second in the same process
    /// must produce DISTINCT run ids so their `summary.json` files do
    /// NOT race / overwrite. Before this fix the id was
    /// `{graph}-{secs}-{pid}`, which collided. After: nanos + counter
    /// make collision practically impossible.
    #[test]
    fn generate_run_id_distinguishes_concurrent_runs_in_same_second() {
        let t = std::time::SystemTime::now();
        let id1 = generate_run_id("deep_research", t);
        let id2 = generate_run_id("deep_research", t);
        assert_ne!(
            id1, id2,
            "two run ids minted in the same second for the same graph must differ"
        );
    }

    /// #1126 codex P2 acceptance: when a pipeline run times out, a
    /// `summary.json` with the `external_context_unmanaged` marker
    /// must still be written so evidence validators can confirm the
    /// run launched workers. The reason string must include the
    /// timeout duration.
    #[test]
    fn emit_external_context_unmanaged_timeout_summary_writes_marker_to_disk() {
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        emit_external_context_unmanaged_timeout_summary(
            dir.path(),
            "deep_research-1747800000-000000001-12345-0",
            "deep_research",
            1_800_000,
            "2026-05-20T17:00:00Z",
            1800,
        );
        let summary_path = dir
            .path()
            .join(".octos/runs/deep_research-1747800000-000000001-12345-0/summary.json");
        assert!(
            summary_path.exists(),
            "timeout path must persist summary.json with M17-B marker"
        );
        let contents = std::fs::read_to_string(&summary_path).unwrap();
        let json: serde_json::Value = serde_json::from_str(&contents).unwrap();
        assert_eq!(json["success"], false, "timeout summary records failure");
        assert_eq!(json["context_mode"], "external_context_unmanaged");
        assert!(
            json["context_reason"]
                .as_str()
                .unwrap_or("")
                .contains("timed out"),
            "context_reason must explicitly mention the timeout for audit",
        );
        assert!(
            json["context_reason"]
                .as_str()
                .unwrap_or("")
                .contains("1800"),
            "context_reason must include the timeout in seconds",
        );
    }
}
