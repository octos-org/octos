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

        Ok(ToolResult {
            output: format!(
                "{}\n\n---\nPipeline execution summary:\n{summary}\nTotal: {} input + {} output tokens",
                result.output, result.token_usage.input_tokens, result.token_usage.output_tokens,
            ),
            success: result.success,
            tokens_used: Some(result.token_usage),
            file_modified: report_file,
            files_to_send,
        })
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
