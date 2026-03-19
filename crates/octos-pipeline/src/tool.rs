//! RunPipelineTool — implements `octos_agent::Tool` to expose pipeline execution.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use octos_agent::{Tool, ToolPolicy, ToolResult};
use octos_llm::{LlmProvider, ProviderRouter};
use octos_memory::EpisodeStore;
use serde::Deserialize;

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
        }
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
    fn build_model_catalog(&self) -> String {
        let router = match &self.provider_router {
            Some(r) => r,
            None => return String::new(),
        };
        let models = router.list_models_with_meta();
        if models.is_empty() {
            return String::new();
        }
        let mut lines = Vec::new();
        for m in &models {
            let max_out_k = m.max_output_tokens / 1000;
            let ctx_k = m.context_window / 1000;
            let mut line = format!(
                "- '{}': {} ({}), {}k output, {}k context",
                m.key, m.model_id, m.provider_name, max_out_k, ctx_k,
            );
            if let Some(ref cost) = m.cost_info {
                line.push_str(&format!(", {cost}"));
            }
            if let Some(ref desc) = m.description {
                line.push_str(&format!(". {desc}"));
            }
            lines.push(line);
        }
        lines.join("\n")
    }

    /// Resolve pipeline with fallback: try inline DOT first, if it fails to parse,
    /// try as a named pipeline. This handles cases where the LLM produces slightly
    /// malformed DOT — the pre-built pipeline still works as a safety net.
    async fn resolve_with_fallback(&self, pipeline_str: &str) -> Result<String> {
        let trimmed = pipeline_str.trim();
        let is_inline = trimmed.starts_with("digraph ") || trimmed.starts_with("digraph{");

        if is_inline {
            // Validate inline DOT parses correctly
            match crate::parser::parse_dot(trimmed) {
                Ok(_) => return Ok(pipeline_str.to_string()),
                Err(parse_err) => {
                    tracing::warn!("inline DOT parse failed, trying named fallback: {parse_err}");
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
        // Build model catalog for the LLM to reference when writing DOT graphs
        let model_catalog = self.build_model_catalog();

        let adaptive_hints = "\
Adapt the pipeline to the query:\n\
- Search angles: 3-4 for simple topics, 5-8 for complex/multi-faceted topics\n\
- Cross-language: ALWAYS include English search angles for broader coverage. \
Also add search angles in languages relevant to the topic's origin \
(e.g. Persian/Arabic for Iran events, Japanese for Japanese tech, German for EU policy, \
Korean for K-pop, Chinese for Chinese education). This finds primary sources others miss.\n\
- Search tools: use deep_search for web research, deep_crawl for specific sites, read_file to read results\n\
- Synthesize: MUST set max_output_tokens high enough for the expected report length (default 4096 truncates long reports)\n\
- Match report language to query language\n\
- For comparative research, add extra search angles per alternative\n\
- For technical topics, include angles for official docs, GitHub repos, and benchmarks\n\
- Model selection: use cheap/fast models for search nodes, pick models with high max_output_tokens for synthesize/report nodes. Avoid using the same model for both search and synthesis.\n\
- Timeouts: synthesize nodes need the highest timeout (900s) since they run last and may retry. Search: 600s, analyze: 300s.\n\
- Analyze nodes should use tools=\"\" (no tools) — they process input text only, no file/search access needed.";

        let node_attrs = "\
Node attributes: handler (codergen|shell|gate|noop|dynamic_parallel|parallel), \
prompt, model, max_output_tokens (default 4096), context_window, tools, timeout_secs, goal_gate, label.\n\
For dynamic_parallel: converge, worker_prompt, planner_model, max_tasks.";

        // Build example DOT using actual model keys when available
        let (search_model, strong_model, synth_model, synth_max_output) =
            if let Some(ref router) = self.provider_router {
                let metas = router.list_models_with_meta();
                if !metas.is_empty() {
                    // Find cheapest/fastest for search, strongest for analysis,
                    // highest max_output for synthesis
                    let mut best_search = &metas[0];
                    let mut best_strong = &metas[0];
                    for m in &metas {
                        // Prefer lower max_output as "cheaper/faster" for search
                        if m.max_output_tokens < best_search.max_output_tokens {
                            best_search = m;
                        }
                        // Prefer higher context window as "stronger"
                        if m.context_window > best_strong.context_window {
                            best_strong = m;
                        }
                    }
                    // For synthesis: pick highest max_output, but exclude the
                    // search model so we don't reuse a cheap/fast model for
                    // long-form generation.
                    let mut best_synth = &metas[0];
                    for m in &metas {
                        if m.key == best_search.key {
                            continue;
                        }
                        if best_synth.key == best_search.key
                            || m.max_output_tokens > best_synth.max_output_tokens
                        {
                            best_synth = m;
                        }
                    }
                    (
                        best_search.key.clone(),
                        best_strong.key.clone(),
                        best_synth.key.clone(),
                        best_synth.max_output_tokens.to_string(),
                    )
                } else {
                    (
                        "cheap".into(),
                        "strong".into(),
                        "strong".into(),
                        "16384".into(),
                    )
                }
            } else {
                (
                    "cheap".into(),
                    "strong".into(),
                    "strong".into(),
                    "16384".into(),
                )
            };

        let example = format!(
            "\
Example:\n\
digraph research {{\n  \
  plan_and_search [handler=\"dynamic_parallel\", converge=\"analyze\", \
prompt=\"Generate 4-6 research angles covering different aspects. Include both Chinese and English angles for cross-language coverage.\", \
worker_prompt=\"You are a research specialist. {{task}}. Call deep_search multiple times IN PARALLEL with different queries to search faster (e.g. one English query + one Chinese query simultaneously). Include ALL URLs, data points, and direct quotes in your response.\", \
model=\"{search_model}\", planner_model=\"{strong_model}\", tools=\"deep_search\", max_tasks=\"8\", timeout_secs=\"600\"]\n  \
  analyze [prompt=\"You will receive research findings from multiple search agents as your input. Cross-reference the findings, resolve contradictions, and organize by subtopic. Preserve ALL data points, URLs, and quotes. Do NOT use any tools — just analyze the input text.\", \
model=\"{strong_model}\", tools=\"\", timeout_secs=\"300\"]\n  \
  synthesize [prompt=\"Write a comprehensive, well-structured report. Include citations with URLs. Save using write_file. Match the query language.\", \
model=\"{synth_model}\", max_output_tokens=\"{synth_max_output}\", tools=\"write_file\", goal_gate=\"true\", timeout_secs=\"900\"]\n  \
  plan_and_search -> analyze\n  \
  analyze -> synthesize\n\
}}"
        );

        let pipeline_desc = if model_catalog.is_empty() {
            format!(
                "Inline DOT graph. ALWAYS write a custom digraph.\n\n\
                 {node_attrs}\n\n\
                 {adaptive_hints}\n\n\
                 {example}"
            )
        } else {
            format!(
                "Inline DOT graph. ALWAYS write a custom digraph — do NOT use pre-built pipeline names.\n\n\
                 Available models (use model=\"key\" in DOT nodes):\n{model_catalog}\n\n\
                 Model strategy: use cheap/fast models for search nodes, \
                 pick the model with highest max output for synthesize/report nodes, \
                 set max_output_tokens to match that model's capacity.\n\n\
                 {node_attrs}\n\n\
                 {adaptive_hints}\n\n\
                 {example}"
            )
        };

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

        let config = ExecutorConfig {
            default_provider: self.default_provider.clone(),
            provider_router: self.provider_router.clone(),
            memory: self.memory.clone(),
            working_dir: self.working_dir.clone(),
            provider_policy: self.provider_policy.clone(),
            plugin_dirs: self.plugin_dirs.clone(),
            status_bridge,
        };

        // Pipeline-level timeout: default 1800s (30 min), clamped to [60, 1800].
        let timeout_secs = input.timeout_secs.unwrap_or(1800).clamp(60, 1800);

        let executor = PipelineExecutor::new(config);
        let result = tokio::time::timeout(
            Duration::from_secs(timeout_secs),
            executor.run(&dot_content, &input.input, &input.variables),
        )
        .await
        .map_err(|_| {
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

        Ok(ToolResult {
            output: format!(
                "{}\n\n---\nPipeline execution summary:\n{summary}\nTotal: {} input + {} output tokens",
                result.output, result.token_usage.input_tokens, result.token_usage.output_tokens,
            ),
            success: result.success,
            tokens_used: Some(result.token_usage),
            ..Default::default()
        })
    }
}
