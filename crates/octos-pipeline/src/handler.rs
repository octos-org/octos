//! Handler trait and built-in handler implementations.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use eyre::Result;
use octos_core::{AgentId, Task, TaskContext, TaskKind, TokenUsage};
use octos_llm::{ContextWindowOverride, LlmProvider, ProviderRouter};
use octos_memory::EpisodeStore;
use tracing::{info, warn};

use octos_agent::progress::{ProgressEvent, ProgressReporter};

use crate::condition;
use crate::graph::{HandlerKind, NodeOutcome, OutcomeStatus, PipelineNode};

/// Reporter that bridges worker agent events to the parent pipeline's
/// `report_progress` so they appear in the SSE stream.
struct PipelineNodeReporter {
    node_id: String,
    model: String,
}

impl ProgressReporter for PipelineNodeReporter {
    fn report(&self, event: ProgressEvent) {
        let msg = match &event {
            ProgressEvent::Thinking { iteration } => {
                format!(
                    "{} [{}]: thinking (iteration {})",
                    self.node_id, self.model, iteration
                )
            }
            ProgressEvent::ToolStarted { name, .. } => {
                format!("{} [{}]: running {}", self.node_id, self.model, name)
            }
            ProgressEvent::ToolCompleted {
                name,
                success,
                duration,
                ..
            } => {
                let status = if *success { "done" } else { "failed" };
                format!(
                    "{}: {} {} ({:.0}s)",
                    self.node_id,
                    name,
                    status,
                    duration.as_secs_f64()
                )
            }
            ProgressEvent::StreamDone { iteration } => {
                format!(
                    "{} [{}]: response received (iteration {})",
                    self.node_id, self.model, iteration
                )
            }
            _ => return,
        };
        crate::executor::report_progress(&msg);
    }
}

/// Context passed to handlers during execution.
pub struct HandlerContext {
    /// Concatenated output from predecessor nodes (or user input for root nodes).
    pub input: String,
    /// All completed node outcomes so far.
    pub completed: HashMap<String, NodeOutcome>,
    /// Working directory for tools.
    pub working_dir: PathBuf,
}

/// Trait for pipeline node handlers.
#[async_trait]
pub trait Handler: Send + Sync {
    /// Execute the handler for the given node.
    async fn execute(&self, node: &PipelineNode, ctx: &HandlerContext) -> Result<NodeOutcome>;
}

/// Registry of handlers by kind.
pub struct HandlerRegistry {
    handlers: HashMap<HandlerKind, Arc<dyn Handler>>,
}

impl HandlerRegistry {
    pub fn new() -> Self {
        Self {
            handlers: HashMap::new(),
        }
    }

    pub fn register(&mut self, kind: HandlerKind, handler: Arc<dyn Handler>) {
        self.handlers.insert(kind, handler);
    }

    pub fn get(&self, kind: &HandlerKind) -> Option<&Arc<dyn Handler>> {
        self.handlers.get(kind)
    }
}

impl Default for HandlerRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ---- Built-in Handlers ----

/// Runs a full octos-agent Agent loop at the node.
/// This is the primary handler — creates a sub-agent with the node's prompt.
pub struct CodergenHandler {
    llm: Arc<dyn LlmProvider>,
    memory: Arc<EpisodeStore>,
    working_dir: PathBuf,
    provider_router: Option<Arc<ProviderRouter>>,
    provider_policy: Option<octos_agent::ToolPolicy>,
    plugin_dirs: Vec<PathBuf>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
}

impl CodergenHandler {
    pub fn new(
        llm: Arc<dyn LlmProvider>,
        memory: Arc<EpisodeStore>,
        working_dir: PathBuf,
        shutdown: Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        Self {
            llm,
            memory,
            working_dir,
            provider_router: None,
            provider_policy: None,
            plugin_dirs: Vec::new(),
            shutdown,
        }
    }

    pub fn with_provider_router(mut self, router: Arc<ProviderRouter>) -> Self {
        self.provider_router = Some(router);
        self
    }

    pub fn with_provider_policy(mut self, policy: Option<octos_agent::ToolPolicy>) -> Self {
        self.provider_policy = policy;
        self
    }

    pub fn with_plugin_dirs(mut self, dirs: Vec<PathBuf>) -> Self {
        self.plugin_dirs = dirs;
        self
    }

    /// Resolve LLM provider for a node, following SpawnTool pattern.
    ///
    /// When a model is explicitly specified and a `ProviderRouter` is available,
    /// the resolved provider is wrapped with capability-compatible fallbacks.
    /// This ensures that if the primary model times out or errors, the pipeline
    /// automatically falls back to another provider with sufficient max_output_tokens.
    fn resolve_provider(&self, model: Option<&str>) -> Result<Arc<dyn LlmProvider>> {
        match (model, &self.provider_router) {
            (Some(model_key), Some(router)) => {
                let primary = router.resolve(model_key)?;
                let fallbacks = router.compatible_fallbacks(model_key);
                if !fallbacks.is_empty() {
                    info!(
                        model = model_key,
                        fallback_count = fallbacks.len(),
                        "pipeline node provider resolved with fallbacks"
                    );
                }
                Ok(octos_llm::FallbackProvider::wrap_with_router(
                    primary,
                    fallbacks,
                    router.clone(),
                ))
            }
            (Some(model_key), None) => {
                warn!(
                    model = model_key,
                    "model override specified but no provider router; using default"
                );
                Ok(self.llm.clone())
            }
            _ => Ok(self.llm.clone()),
        }
    }
}

#[async_trait]
impl Handler for CodergenHandler {
    async fn execute(&self, node: &PipelineNode, ctx: &HandlerContext) -> Result<NodeOutcome> {
        static WORKER_COUNT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

        let worker_num = WORKER_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let worker_id = AgentId::new(format!("pipeline-{}-{worker_num}", node.id));

        // Resolve LLM provider
        let base_provider = self.resolve_provider(node.model.as_deref())?;
        let provider: Arc<dyn LlmProvider> = match node.context_window {
            Some(cw) => Arc::new(ContextWindowOverride::new(base_provider, cw)),
            None => base_provider,
        };

        // Build tool registry (same pattern as SpawnTool sync, spawn.rs:269-278)
        let mut tools = octos_agent::ToolRegistry::with_builtins(&self.working_dir);

        // Load plugin tools (app-skills like deep-search, deep-crawl, etc.)
        if !self.plugin_dirs.is_empty() {
            if let Err(e) = octos_agent::PluginLoader::load_into(&mut tools, &self.plugin_dirs, &[])
            {
                warn!("plugin loading in pipeline handler: {e}");
            }
        }

        // Filter out empty tool names (from tools="" in DOT)
        let allowed: Vec<String> = node
            .tools
            .iter()
            .filter(|t| !t.trim().is_empty())
            .cloned()
            .collect();

        // If tools="" was specified (explicit empty), remove ALL tools
        // so the agent does text-only processing (no tool calls).
        let has_tools_attr = !node.tools.is_empty();
        let policy = if has_tools_attr && allowed.is_empty() {
            // Explicit tools="" → deny everything
            octos_agent::ToolPolicy {
                deny: vec!["*".into()],
                ..Default::default()
            }
        } else {
            octos_agent::ToolPolicy {
                allow: allowed,
                deny: vec![
                    "spawn".into(),
                    "run_pipeline".into(),
                    "send_file".into(),
                    "message".into(),
                ],
                ..Default::default()
            }
        };
        tools.apply_policy(&policy);
        if let Some(ref pp) = self.provider_policy {
            tools.set_provider_policy(pp.clone());
        }

        // Build system prompt from node prompt template
        let mut system_prompt = match &node.prompt {
            Some(p) => p.clone(),
            None => "Complete the task given to you.".to_string(),
        };

        // If the node has write_file tool, instruct the agent to save the full report
        // to a file in ONE call and return a concise executive summary as text.
        // Without explicit "single call" instruction, some models (e.g. kimi-k2.5)
        // chunk output into ~4K token pieces across many iterations, causing timeouts.
        if node.tools.iter().any(|t| t == "write_file") {
            system_prompt.push_str(
                "\n\nIMPORTANT: You MUST do two things:\n\
                 1. Save your COMPLETE report in ONE SINGLE write_file call (choose a descriptive \
                 filename). Do NOT split the report across multiple write_file calls — put the \
                 ENTIRE content in one call, even if it is very long.\n\
                 2. After saving, return a concise executive summary (key findings, conclusions, \
                 recommendations) as your final text response — around 1000 words. \
                 The full report file will be delivered to the user separately.",
            );
        }

        // Analyze-node guidance: when the node has deep_crawl or read_file but
        // NOT write_file, it is an analysis/convergence node that receives
        // merged search results.  Inject structure so the output is easy for
        // the downstream synthesize node to consume.
        let has_analysis_tool = node
            .tools
            .iter()
            .any(|t| t == "deep_crawl" || t == "read_file");
        let has_write = node.tools.iter().any(|t| t == "write_file");
        if has_analysis_tool && !has_write {
            system_prompt.push_str(
                "\n\nOUTPUT STRUCTURE — you MUST organise your analysis using these sections:\n\
                 ## Key Findings\n\
                 Numbered list of the most important facts, data points, and conclusions \
                 drawn from the input sources. Each finding must cite its source.\n\n\
                 ## Contradictions & Conflicts\n\
                 List any claims that contradict each other across sources. For each, \
                 state the conflicting positions and which source supports each side.\n\n\
                 ## Gaps & Open Questions\n\
                 Identify topics or questions that the sources do NOT adequately address. \
                 If you used deep_crawl to fill a gap, note what you found.\n\n\
                 ## Sourced Claims\n\
                 A reference-style list mapping each major claim to its originating URL \
                 or document. Format: `[claim summary] — source: <URL or filename>`\n\n\
                 Keep your language precise and factual. Do NOT pad with filler. \
                 The next stage will use this structured output to write the final report.",
            );
        }

        // Create and run the agent.
        // When max_output_tokens is not set in the DOT graph, use the
        // provider's actual max output capability instead of the global
        // default (4096) which truncates long-form synthesis.
        let max_tokens = node
            .max_output_tokens
            .or_else(|| Some(provider.max_output_tokens()));
        let config = octos_agent::AgentConfig {
            max_iterations: 30,
            max_timeout: node.timeout_secs.map(Duration::from_secs),
            save_episodes: false,
            chat_max_tokens: max_tokens,
            ..Default::default()
        };

        let resolved_model = format!("{}/{}", provider.provider_name(), provider.model_id());

        let reporter: Arc<dyn ProgressReporter> = Arc::new(PipelineNodeReporter {
            node_id: node.id.clone(),
            model: resolved_model.clone(),
        });

        let worker =
            octos_agent::Agent::new(worker_id.clone(), provider, tools, self.memory.clone())
                .with_config(config)
                .with_system_prompt(system_prompt)
                .with_shutdown(self.shutdown.clone())
                .with_reporter(reporter);

        let task = Task::new(
            TaskKind::Code {
                instruction: ctx.input.clone(),
                files: vec![],
            },
            TaskContext {
                working_dir: self.working_dir.clone(),
                ..Default::default()
            },
        );
        info!(
            node = %node.id,
            worker = %worker_id,
            resolved_model = %resolved_model,
            tools = node.tools.join(","),
            timeout_secs = node.timeout_secs.unwrap_or(0),
            max_output_tokens = max_tokens.unwrap_or(0),
            "executing codergen node"
        );

        match worker.run_task(&task).await {
            Ok(result) => {
                if !result.files_modified.is_empty() {
                    info!(
                        node = %node.id,
                        files = ?result.files_modified.iter().map(|f| f.display().to_string()).collect::<Vec<_>>(),
                        "node wrote files"
                    );
                }
                Ok(NodeOutcome {
                    node_id: node.id.clone(),
                    status: if result.success {
                        OutcomeStatus::Pass
                    } else {
                        OutcomeStatus::Fail
                    },
                    content: result.output,
                    token_usage: result.token_usage,
                    files_modified: result.files_modified,
                })
            }
            Err(e) => Ok(NodeOutcome {
                node_id: node.id.clone(),
                status: OutcomeStatus::Error,
                content: format!("Agent error: {e}"),
                token_usage: TokenUsage::default(),
                files_modified: vec![],
            }),
        }
    }
}

/// Execute a shell command. The node prompt is treated as the command.
pub struct ShellHandler {
    working_dir: PathBuf,
}

impl ShellHandler {
    pub fn new(working_dir: PathBuf) -> Self {
        Self { working_dir }
    }
}

#[async_trait]
impl Handler for ShellHandler {
    async fn execute(&self, node: &PipelineNode, ctx: &HandlerContext) -> Result<NodeOutcome> {
        let command = node.prompt.as_deref().unwrap_or(&ctx.input);
        let timeout = Duration::from_secs(node.timeout_secs.unwrap_or(300));

        info!(node = %node.id, command = %command, "executing shell node");

        let result = tokio::time::timeout(timeout, {
            #[cfg(windows)]
            let fut = tokio::process::Command::new("cmd")
                .arg("/C")
                .arg(command)
                .current_dir(&self.working_dir)
                .output();
            #[cfg(not(windows))]
            let fut = tokio::process::Command::new("sh")
                .arg("-c")
                .arg(command)
                .current_dir(&self.working_dir)
                .output();
            fut
        })
        .await;

        match result {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                let success = output.status.success();

                Ok(NodeOutcome {
                    node_id: node.id.clone(),
                    status: if success {
                        OutcomeStatus::Pass
                    } else {
                        OutcomeStatus::Fail
                    },
                    content: if stderr.is_empty() {
                        stdout
                    } else {
                        format!("{stdout}\n--- stderr ---\n{stderr}")
                    },
                    token_usage: TokenUsage::default(),
                    files_modified: vec![],
                })
            }
            Ok(Err(e)) => Ok(NodeOutcome {
                node_id: node.id.clone(),
                status: OutcomeStatus::Error,
                content: format!("Shell error: {e}"),
                token_usage: TokenUsage::default(),
                files_modified: vec![],
            }),
            Err(_) => Ok(NodeOutcome {
                node_id: node.id.clone(),
                status: OutcomeStatus::Error,
                content: format!("Shell timed out after {}s", timeout.as_secs()),
                token_usage: TokenUsage::default(),
                files_modified: vec![],
            }),
        }
    }
}

/// Evaluate a condition without any LLM call.
/// The node prompt is treated as a condition expression evaluated against
/// the last completed node's outcome.
pub struct GateHandler;

#[async_trait]
impl Handler for GateHandler {
    async fn execute(&self, node: &PipelineNode, ctx: &HandlerContext) -> Result<NodeOutcome> {
        let cond_str = node.prompt.as_deref().unwrap_or("true");

        // Find the last completed outcome to evaluate against
        let last_outcome = ctx
            .completed
            .values()
            .last()
            .cloned()
            .unwrap_or_else(|| NodeOutcome {
                node_id: "none".into(),
                status: OutcomeStatus::Pass,
                content: ctx.input.clone(),
                token_usage: TokenUsage::default(),
                files_modified: vec![],
            });

        if cond_str == "true" {
            return Ok(NodeOutcome {
                node_id: node.id.clone(),
                status: OutcomeStatus::Pass,
                content: last_outcome.content,
                token_usage: TokenUsage::default(),
                files_modified: vec![],
            });
        }

        let expr = condition::parse_condition(cond_str)?;
        let passed = condition::evaluate(&expr, &last_outcome);

        Ok(NodeOutcome {
            node_id: node.id.clone(),
            status: if passed {
                OutcomeStatus::Pass
            } else {
                OutcomeStatus::Fail
            },
            content: last_outcome.content,
            token_usage: TokenUsage::default(),
            files_modified: vec![],
        })
    }
}

/// Pass-through handler. Returns immediately with the input as content.
pub struct NoopHandler;

#[async_trait]
impl Handler for NoopHandler {
    async fn execute(&self, node: &PipelineNode, ctx: &HandlerContext) -> Result<NodeOutcome> {
        Ok(NodeOutcome {
            node_id: node.id.clone(),
            status: OutcomeStatus::Pass,
            content: ctx.input.clone(),
            token_usage: TokenUsage::default(),
            files_modified: vec![],
        })
    }
}
