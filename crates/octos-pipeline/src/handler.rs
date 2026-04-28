//! Handler trait and built-in handler implementations.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use eyre::Result;
use octos_core::{AgentId, Task, TaskContext, TaskKind, TokenUsage};
use octos_llm::{ContextWindowOverride, LlmProvider, ProviderRouter};
use octos_memory::EpisodeStore;
use tracing::{info, warn};

use octos_agent::progress::{ProgressEvent, ProgressReporter};
use octos_agent::tools::{TOOL_CTX, Tool, ToolRegistry};

use crate::condition;
use crate::graph::{HandlerKind, NodeOutcome, OutcomeStatus, PipelineNode};

/// Cached snapshot of plugin tools loaded from `plugin_dirs`.
///
/// Computed once per `CodergenHandler` instance and shared across every
/// node execution via an `Arc`. Eliminates the per-node SHA-256
/// verification + executable read that the loader would otherwise
/// perform on every `execute()` call — for ~14 bundled plugins (each
/// up to 100 MB) this used to cost 100 ms–seconds per node and starved
/// the SSE window the chat UI / e2e tests inspect.
///
/// Field semantics:
/// * `tools` — registered plugin tool `Arc`s (cheap to insert into a
///   per-node [`ToolRegistry`]).
/// * `tool_names` — passed to [`ToolRegistry::mark_as_plugin`] so
///   downstream gates that check `is_plugin()` see the same labels
///   pre-cache vs. post-cache.
/// * `spawn_only` — `(name, optional_message)` pairs replayed via
///   [`ToolRegistry::mark_spawn_only`]. The pipeline `execute` path
///   then `clear_spawn_only`s these out, but the marking is preserved
///   so behaviour is byte-for-byte identical to the legacy
///   `PluginLoader::load_into` call.
#[derive(Default)]
pub(crate) struct CachedPluginRegistration {
    pub(crate) tools: Vec<Arc<dyn Tool>>,
    pub(crate) tool_names: Vec<String>,
    pub(crate) spawn_only: Vec<(String, Option<String>)>,
}

impl CachedPluginRegistration {
    /// Apply this cached registration onto a fresh per-node
    /// [`ToolRegistry`]. Mirrors the ordering used by
    /// [`octos_agent::PluginLoader::load_into_with_options`] so the
    /// observable registry state is identical to the legacy code path.
    pub(crate) fn apply_to(&self, registry: &mut ToolRegistry) {
        for tool in &self.tools {
            let name = tool.name().to_string();
            registry.mark_as_plugin(&name);
            registry.register_arc(tool.clone());
        }
        for (name, msg) in &self.spawn_only {
            registry.mark_spawn_only(name, msg.clone());
        }
    }
}

/// Build the cached plugin registration by running the loader against a
/// throw-away registry. Errors are downgraded to a warn (matching the
/// legacy pipeline behaviour) and an empty registration is cached so
/// the warning fires at most once per handler lifetime.
fn build_cached_plugin_registration(plugin_dirs: &[PathBuf]) -> CachedPluginRegistration {
    if plugin_dirs.is_empty() {
        return CachedPluginRegistration::default();
    }

    let started = std::time::Instant::now();
    let mut staging = ToolRegistry::new();
    let load_result = octos_agent::PluginLoader::load_into(&mut staging, plugin_dirs, &[]);
    let elapsed = started.elapsed();

    let load_result = match load_result {
        Ok(r) => r,
        Err(e) => {
            warn!(
                error = %e,
                plugin_dirs = ?plugin_dirs,
                "plugin loading in pipeline handler — caching empty registration"
            );
            return CachedPluginRegistration::default();
        }
    };

    // Pull each registered tool back out by name — `load_into` registers
    // them via `register(...)` which stores them as `Arc<dyn Tool>` we
    // can clone cheaply on every node hit.
    let mut tools: Vec<Arc<dyn Tool>> = Vec::with_capacity(load_result.tool_names.len());
    for name in &load_result.tool_names {
        if let Some(tool) = staging.get_tool(name) {
            tools.push(tool);
        }
    }

    // Capture spawn_only names so per-node registries can replay the
    // marking. We deliberately drop the custom messages here because
    // the pipeline `execute` path immediately calls
    // `tools.clear_spawn_only()` (the comment in `execute` explains
    // why), so the message text is never observed downstream. Storing
    // only the names keeps the cache copy lean.
    let spawn_only: Vec<(String, Option<String>)> = staging
        .spawn_only_tools()
        .iter()
        .map(|name| (name.clone(), None))
        .collect();

    info!(
        tool_count = tools.len(),
        elapsed_ms = elapsed.as_millis() as u64,
        "cached pipeline plugin registration"
    );

    CachedPluginRegistration {
        tools,
        tool_names: load_result.tool_names,
        spawn_only,
    }
}

/// Reporter that bridges worker agent events to the parent pipeline's
/// `report_progress` so they appear in the SSE stream.
pub(crate) struct PipelineNodeReporter {
    pub(crate) node_id: String,
    pub(crate) model: String,
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
            // Bug 3 / Gap 3.3 — per-node cost updates from inner agents must
            // reach the parent SSE stream. The legacy `_ => return` arm
            // swallowed these silently, leaving the W1.G4 CostBreakdown
            // panel data-blind. Forward as a structured progress message
            // the chat UI can render alongside the per-node tree.
            ProgressEvent::CostUpdate {
                session_input_tokens,
                session_output_tokens,
                response_cost,
                ..
            } => {
                let cost_label = response_cost
                    .map(|c| format!("${c:.4}"))
                    .unwrap_or_else(|| "$?".to_string());
                format!(
                    "{}: tokens {}+{}, {}",
                    self.node_id, session_input_tokens, session_output_tokens, cost_label
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
    /// Declared compaction policy to propagate onto child Agents
    /// (coding-blue FA-7). `None` = legacy path, no compaction runner
    /// attached to the worker.
    compaction_policy: Option<octos_agent::workspace_policy::CompactionPolicy>,
    /// Workspace policy backing the compaction runner — lets the runner
    /// resolve declared artifact names against glob patterns.
    compaction_workspace: Option<octos_agent::workspace_policy::WorkspacePolicy>,
    /// Agent LLM provider used to construct
    /// `CompactionRunner::with_provider(...)`. Defaults to `self.llm`
    /// when unset so extractive compaction still works without the
    /// caller threading a dedicated provider in.
    compaction_llm_provider: Option<Arc<dyn LlmProvider>>,
    /// M8 parity (W1.A1): inherited resources from the parent session
    /// — wired onto every per-node Agent so file tools see the same
    /// FileStateCache, sub-agent output goes to the same router, and
    /// the same summary generator drives periodic LLM digests for any
    /// background task the worker triggers.
    host_context: crate::host_context::PipelineHostContext,
    /// Backend bug #1 fix: cached snapshot of the plugin-tool registry
    /// produced from `plugin_dirs`. Lazily populated on the first node
    /// `execute()` (or test warm-up call) so all subsequent nodes skip
    /// the SHA-256 verification + 100 MB executable read that would
    /// otherwise re-run on every node and starve the SSE window.
    plugin_cache: Arc<OnceLock<Arc<CachedPluginRegistration>>>,
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
            compaction_policy: None,
            compaction_workspace: None,
            compaction_llm_provider: None,
            host_context: crate::host_context::PipelineHostContext::default(),
            plugin_cache: Arc::new(OnceLock::new()),
        }
    }

    /// M8 parity (W1.A1): attach the parent session's
    /// [`PipelineHostContext`] so the per-node Agent inherits the
    /// shared FileStateCache / SubAgentOutputRouter /
    /// AgentSummaryGenerator handles. The default empty context keeps
    /// pre-M8 callers byte-for-byte identical.
    pub fn with_host_context(
        mut self,
        host_context: crate::host_context::PipelineHostContext,
    ) -> Self {
        self.host_context = host_context;
        self
    }

    /// Doc-hidden test accessor — used by W1.A1 acceptance tests to
    /// confirm the host context was propagated from the
    /// [`PipelineExecutor::build_codergen`] wiring.
    #[doc(hidden)]
    pub fn host_context(&self) -> &crate::host_context::PipelineHostContext {
        &self.host_context
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
        // Backend bug #1: reset the plugin-load cache when dirs change
        // so a builder reordering can't end up with a stale cached set.
        self.plugin_cache = Arc::new(OnceLock::new());
        self
    }

    /// Attach a declarative compaction policy (coding-blue FA-7).
    /// Each worker [`Agent`] built by this handler will receive a
    /// [`CompactionRunner`] constructed from `policy` via
    /// [`CompactionRunner::with_provider`] so LLM-iterative
    /// summarisation fires when declared.
    ///
    /// [`CompactionRunner`]: octos_agent::compaction::CompactionRunner
    /// [`CompactionRunner::with_provider`]: octos_agent::compaction::CompactionRunner::with_provider
    pub fn with_compaction_policy(
        mut self,
        policy: Option<octos_agent::workspace_policy::CompactionPolicy>,
    ) -> Self {
        self.compaction_policy = policy;
        self
    }

    /// Attach the workspace policy backing the compaction runner so
    /// declared artifact names resolve against glob patterns. Consumed
    /// via [`Agent::with_compaction_workspace`].
    ///
    /// [`Agent::with_compaction_workspace`]: octos_agent::Agent::with_compaction_workspace
    pub fn with_compaction_workspace(
        mut self,
        workspace: Option<octos_agent::workspace_policy::WorkspacePolicy>,
    ) -> Self {
        self.compaction_workspace = workspace;
        self
    }

    /// Attach the agent LLM provider used for
    /// [`CompactionRunner::with_provider`]. When unset the worker's
    /// resolved LLM provider is used — always safe because extractive
    /// summarisation ignores the provider entirely and LLM-iterative
    /// routes through the same Agent provider that serves the node.
    ///
    /// [`CompactionRunner::with_provider`]: octos_agent::compaction::CompactionRunner::with_provider
    pub fn with_compaction_llm_provider(mut self, provider: Option<Arc<dyn LlmProvider>>) -> Self {
        self.compaction_llm_provider = provider;
        self
    }

    /// Accessor used by acceptance tests to confirm that the
    /// compaction block was propagated from the parent
    /// [`PipelineContext`]. Not part of the public API surface.
    #[doc(hidden)]
    pub fn has_compaction_policy(&self) -> bool {
        self.compaction_policy.is_some()
    }

    /// Accessor used by acceptance tests to confirm that the workspace
    /// policy was propagated for compaction artifact resolution.
    #[doc(hidden)]
    pub fn has_compaction_workspace(&self) -> bool {
        self.compaction_workspace.is_some()
    }

    /// Lazily build (or return the already-built) plugin-tool cache.
    ///
    /// Backend bug #1 fix — collapses the per-node SHA-256 verification and
    /// 100 MB-bounded executable read into a single up-front scan.
    ///
    /// The returned `Arc<CachedPluginRegistration>` is shared across every
    /// node in the same pipeline run, and via `Arc<OnceLock>` across every
    /// `Arc<dyn Handler>` clone of the same handler instance.
    ///
    /// First-call latency equals the legacy load cost (SHA-256 plus
    /// `.<name>_verified` write). Subsequent calls reduce to a single atomic
    /// load and an `Arc::clone`.
    fn cached_plugin_registration(&self) -> Arc<CachedPluginRegistration> {
        self.plugin_cache
            .get_or_init(|| Arc::new(build_cached_plugin_registration(&self.plugin_dirs)))
            .clone()
    }

    /// Doc-hidden test accessor — populates the plugin cache so tests
    /// can assert that subsequent loads do NOT re-touch disk.
    #[doc(hidden)]
    pub fn warm_plugin_cache_for_test(&self) {
        let _ = self.cached_plugin_registration();
    }

    /// Doc-hidden test accessor — exposes the cached plugin tool names.
    /// Used by the regression test for backend bug #1 to confirm the
    /// cached registration is stable across calls.
    #[doc(hidden)]
    pub fn cached_plugin_tool_names_for_test(&self) -> Vec<String> {
        self.cached_plugin_registration().tool_names.clone()
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

        // Backend bug #1: load plugin tools from a process-shared cache.
        // The cache is populated on the first node's execute() call (or
        // via the test warm-up accessor) so subsequent nodes skip the
        // SHA-256 verification + executable read that used to add
        // 100 ms–seconds of per-node latency, starving the SSE window
        // the chat UI / e2e tests inspect.
        if !self.plugin_dirs.is_empty() {
            let cached = self.cached_plugin_registration();
            cached.apply_to(&mut tools);
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
        // Strip spawn_only flags so plugin tools that *would* normally be
        // backgrounded by the spawn_only branch in agent::execution run
        // synchronously inside the pipeline worker instead. Two cascade
        // bugs are killed by this:
        //
        //   1. The bg branch tries `bg_tools.execute("send_file", ...)`,
        //      but apply_policy above just denied send_file → "unknown
        //      tool" + 3-retry waste + bg_supervisor.mark_failed even
        //      though the tool itself succeeded.
        //   2. for_session()-policy tools (fm_tts / voice_synthesize /
        //      podcast_generate) trip enforce_spawn_task_contract when
        //      run as spawn_only with pipeline working_dir = data_dir
        //      (no .octos-workspace.toml at root) → spurious
        //      NotConfigured failures on tools that succeeded.
        //
        // Mirrors the proven sync/async-spawn pattern in
        // crates/octos-agent/src/tools/spawn.rs::spawn_subagent_inner.
        tools.clear_spawn_only();
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

        #[cfg(windows)]
        {
            system_prompt.push_str(
                "\n\nWINDOWS RUNTIME RULES:\n\
                 - You are running on Windows.\n\
                 - If you use shell, write cmd.exe-compatible commands only.\n\
                 - Do NOT use Unix-only commands like `ps`, `grep`, `head`, `rm`, `ls`, `cat`, `which`, or `bash`.\n\
                 - Prefer built-in tools over shell whenever possible.\n\
                 - If a required tool or binary is unavailable on this host, state that explicitly and stop instead of retrying via shell.",
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
            // Pipeline workers don't have a channel-bound send_file tool
            // registered (deny-listed above + outer pipeline orchestration
            // handles delivery via PipelineResult.modified_files). Without
            // this flag the auto-send path inside execution.rs tries
            // `tools.execute("send_file", ...)` and trips
            // "unknown tool: send_file" warnings on every spawn_only
            // result (deep_search reports etc).
            suppress_auto_send_files: true,
            ..Default::default()
        };

        let resolved_model = format!("{}/{}", provider.provider_name(), provider.model_id());

        let reporter: Arc<dyn ProgressReporter> = Arc::new(PipelineNodeReporter {
            node_id: node.id.clone(),
            model: resolved_model.clone(),
        });

        let inherited_harness_sink = TOOL_CTX
            .try_with(|ctx| ctx.harness_event_sink.clone())
            .ok()
            .flatten();

        let mut worker = octos_agent::Agent::new(
            worker_id.clone(),
            provider.clone(),
            tools,
            self.memory.clone(),
        )
        .with_config(config)
        .with_system_prompt(system_prompt)
        .with_shutdown(self.shutdown.clone())
        .with_reporter(reporter);
        if let Some(sink) = inherited_harness_sink {
            worker = worker.with_harness_event_sink(sink);
        }

        // M8 parity (W1.A1): wire the parent session's shared
        // resources onto the per-node worker so file tools see the
        // shared FileStateCache, sub-agent output flows through the
        // shared SubAgentOutputRouter, and AgentSummaryGenerator drives
        // periodic LLM digests for any background task the worker
        // triggers. Each handle is optional so legacy callers (no host
        // context) keep their pre-M8 behaviour bitwise identical.
        if let Some(cache) = self.host_context.file_state_cache.clone() {
            worker = worker.with_file_state_cache(cache);
        }
        if let Some(router) = self.host_context.subagent_output_router.clone() {
            worker = worker.with_subagent_output_router(router);
        }
        if let Some(generator) = self.host_context.subagent_summary_generator.clone() {
            worker = worker.with_subagent_summary_generator(generator);
        }
        if let Some(accountant) = self.host_context.cost_accountant.clone() {
            worker = worker.with_cost_accountant(accountant);
        }
        if let Some(ref session_key) = self.host_context.parent_session_key {
            worker = worker.with_parent_session_key(session_key.clone());
        }

        // coding-blue FA-7: propagate parent's declarative compaction
        // onto every LLM-call node so the worker honours preflight +
        // post-call compaction and the policy's preserved-artifacts
        // rail. Both the compaction policy AND the backing workspace
        // policy must be present — the workspace is how the runner
        // resolves declared artifact names to glob patterns.
        if let Some(compaction_policy) = self.compaction_policy.clone() {
            let compaction_provider = self.compaction_llm_provider.clone().unwrap_or(provider);
            let runner = octos_agent::compaction::CompactionRunner::with_provider(
                compaction_policy,
                compaction_provider,
            );
            let runner = if let Some(ref workspace) = self.compaction_workspace {
                runner.with_workspace_policy(workspace)
            } else {
                runner
            };
            worker = worker.with_compaction_runner(Arc::new(runner));
            if let Some(workspace) = self.compaction_workspace.clone() {
                worker = worker.with_compaction_workspace(workspace);
            }
        }

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use octos_agent::progress::{ProgressEvent, ProgressReporter};
    use octos_agent::tools::{TOOL_CTX, ToolContext};

    /// Capture reporter that stores every event so tests can assert which
    /// pieces of progress reached the parent SSE stream.
    struct CapturingReporter {
        events: Arc<Mutex<Vec<ProgressEvent>>>,
    }

    impl ProgressReporter for CapturingReporter {
        fn report(&self, event: ProgressEvent) {
            self.events.lock().unwrap().push(event);
        }
    }

    /// Gap 3.3 — `PipelineNodeReporter::report` must forward
    /// `ProgressEvent::CostUpdate` events through
    /// `crate::executor::report_progress` instead of swallowing them via
    /// the `_ => return` arm. This is the bridge that lets per-node cost
    /// updates from inner-agent loops reach the parent SSE stream so the
    /// W1.G4 CostBreakdown panel can render them inline with the node tree.
    #[tokio::test]
    async fn pipeline_node_reporter_forwards_cost_update_to_parent_sse() {
        let captured = Arc::new(Mutex::new(Vec::<ProgressEvent>::new()));
        let parent_reporter: Arc<dyn ProgressReporter> = Arc::new(CapturingReporter {
            events: captured.clone(),
        });

        let mut ctx = ToolContext::zero();
        ctx.tool_id = "call_test".to_string();
        ctx.reporter = parent_reporter;

        let node_reporter = PipelineNodeReporter {
            node_id: "draft".to_string(),
            model: "claude-sonnet".to_string(),
        };

        // Scope TOOL_CTX so report_progress() can find the parent reporter,
        // then dispatch a CostUpdate event the way an inner Agent would.
        TOOL_CTX
            .scope(ctx, async {
                node_reporter.report(ProgressEvent::CostUpdate {
                    session_input_tokens: 320,
                    session_output_tokens: 110,
                    response_cost: Some(0.0008),
                    session_cost: Some(0.0008),
                });
            })
            .await;

        let events = captured.lock().unwrap();
        let forwarded = events
            .iter()
            .find_map(|e| match e {
                ProgressEvent::ToolProgress { name, message, .. } if name == "run_pipeline" => {
                    Some(message.clone())
                }
                _ => None,
            })
            .expect("CostUpdate must be forwarded as a ToolProgress event on the parent reporter");
        assert!(
            forwarded.contains("draft"),
            "forwarded message should reference the originating node_id"
        );
        assert!(
            forwarded.contains("320") && forwarded.contains("110"),
            "forwarded message should carry the per-node token totals; got {forwarded:?}"
        );
    }

    /// Sanity check — the existing event arms still forward as before so
    /// the new CostUpdate arm is purely additive.
    #[tokio::test]
    async fn pipeline_node_reporter_still_forwards_thinking_events() {
        let captured = Arc::new(Mutex::new(Vec::<ProgressEvent>::new()));
        let parent_reporter: Arc<dyn ProgressReporter> = Arc::new(CapturingReporter {
            events: captured.clone(),
        });

        let mut ctx = ToolContext::zero();
        ctx.tool_id = "call_test".to_string();
        ctx.reporter = parent_reporter;

        let node_reporter = PipelineNodeReporter {
            node_id: "refine".to_string(),
            model: "claude-haiku".to_string(),
        };

        TOOL_CTX
            .scope(ctx, async {
                node_reporter.report(ProgressEvent::Thinking { iteration: 1 });
            })
            .await;

        let events = captured.lock().unwrap();
        assert!(events.iter().any(|e| matches!(
            e,
            ProgressEvent::ToolProgress { name, .. } if name == "run_pipeline"
        )));
    }
}
