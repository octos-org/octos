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

use crate::condition;
use crate::graph::{HandlerKind, NodeOutcome, OutcomeStatus, PipelineNode};

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
}

impl CodergenHandler {
    pub fn new(llm: Arc<dyn LlmProvider>, memory: Arc<EpisodeStore>, working_dir: PathBuf) -> Self {
        Self {
            llm,
            memory,
            working_dir,
            provider_router: None,
            provider_policy: None,
            plugin_dirs: Vec::new(),
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
    fn resolve_provider(&self, model: Option<&str>) -> Result<Arc<dyn LlmProvider>> {
        match (model, &self.provider_router) {
            (Some(model_key), Some(router)) => router.resolve(model_key),
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

        let policy = octos_agent::ToolPolicy {
            allow: node.tools.clone(),
            deny: vec!["spawn".into(), "run_pipeline".into()],
            ..Default::default()
        };
        tools.apply_policy(&policy);
        if let Some(ref pp) = self.provider_policy {
            tools.set_provider_policy(pp.clone());
        }

        // Build system prompt from node prompt template
        let system_prompt = match &node.prompt {
            Some(p) => p.clone(),
            None => "Complete the task given to you.".to_string(),
        };

        // Create and run the agent
        let config = octos_agent::AgentConfig {
            max_iterations: 30,
            max_timeout: node.timeout_secs.map(Duration::from_secs),
            save_episodes: false,
            chat_max_tokens: node.max_output_tokens,
            ..Default::default()
        };

        let worker =
            octos_agent::Agent::new(worker_id.clone(), provider, tools, self.memory.clone())
                .with_config(config)
                .with_system_prompt(system_prompt);

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

        info!(node = %node.id, worker = %worker_id, "executing codergen node");

        match worker.run_task(&task).await {
            Ok(result) => Ok(NodeOutcome {
                node_id: node.id.clone(),
                status: if result.success {
                    OutcomeStatus::Pass
                } else {
                    OutcomeStatus::Fail
                },
                content: result.output,
                token_usage: result.token_usage,
            }),
            Err(e) => Ok(NodeOutcome {
                node_id: node.id.clone(),
                status: OutcomeStatus::Error,
                content: format!("Agent error: {e}"),
                token_usage: TokenUsage::default(),
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
                })
            }
            Ok(Err(e)) => Ok(NodeOutcome {
                node_id: node.id.clone(),
                status: OutcomeStatus::Error,
                content: format!("Shell error: {e}"),
                token_usage: TokenUsage::default(),
            }),
            Err(_) => Ok(NodeOutcome {
                node_id: node.id.clone(),
                status: OutcomeStatus::Error,
                content: format!("Shell timed out after {}s", timeout.as_secs()),
                token_usage: TokenUsage::default(),
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
            });

        if cond_str == "true" {
            return Ok(NodeOutcome {
                node_id: node.id.clone(),
                status: OutcomeStatus::Pass,
                content: last_outcome.content,
                token_usage: TokenUsage::default(),
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
        })
    }
}
