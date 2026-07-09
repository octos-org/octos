//! `octos acp`: run octos as an [Agent Client Protocol][acp] (ACP) agent over
//! stdin/stdout so ACP clients (Zed, and other editors/CLIs that speak ACP) can
//! drive the octos agent loop.
//!
//! [acp]: https://agentclientprotocol.com/
//!
//! ## Shape
//!
//! The official `agent-client-protocol` crate (v1.2.0) does **not** expose a
//! single "implement these methods" trait. Instead you build an
//! [`agent_client_protocol::Agent`] role via its builder, registering one
//! `on_receive_request` / `on_receive_notification` handler per ACP message
//! type, then hand the builder a stdio transport and let its runtime drive the
//! JSON-RPC framing:
//!
//! ```ignore
//! Agent.builder()
//!     .on_receive_request(handle_initialize, on_receive_request!())
//!     .on_receive_request(handle_new_session, on_receive_request!())
//!     .on_receive_request(handle_prompt, on_receive_request!())
//!     .on_receive_notification(handle_cancel, on_receive_notification!())
//!     .connect_to(Stdio::new())
//!     .await
//! ```
//!
//! Dispatch is by the request's Rust type: registering a handler for
//! [`InitializeRequest`] wires the `"initialize"` method, `NewSessionRequest`
//! wires `"session/new"`, `PromptRequest` wires `"session/prompt"`,
//! `CancelNotification` wires the `"session/cancel"` notification, and so on.
//!
//! Each handler receives a `Responder<Resp>` (call `.respond(resp)` to reply)
//! and a `ConnectionTo<Client>` (`cx`) on which we push streaming
//! [`SessionNotification`]s (`session/update`) back to the client.
//!
//! ## Bridge
//!
//! `new_session` builds a real octos [`Agent`] (LLM provider + a
//! [`ToolRegistry`] rooted at the requested cwd + episodic memory) and stores
//! it in a session map keyed by the ACP [`SessionId`]. `prompt` extracts the
//! text from the ACP content blocks, runs [`Agent::process_message`] with an
//! [`AcpProgressReporter`] that forwards each [`ProgressEvent`] to the client as
//! a `session/update`, and returns the ACP [`StopReason`].
//!
//! Cancellation: the prompt turn runs in a task spawned off the dispatch loop
//! (`ConnectionTo::spawn`) so an interleaved `session/cancel` notification is
//! processed WHILE the turn is in flight. `session/cancel` flips the agent's
//! shared `Arc<AtomicBool>` shutdown flag (wired via `Agent::with_shutdown`),
//! which aborts the running loop at its next checkpoint; the turn then responds
//! with [`StopReason::Cancelled`].
//!
//! Permissions: octos runs its own tools through its own approval path; we
//! surface them to the client as ACP `tool_call` / `tool_call_update` records
//! but do not block on ACP `session/request_permission` in v1.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use clap::Args;
use eyre::{Result, WrapErr};

use agent_client_protocol::schema::v1::{
    AgentCapabilities, CancelNotification, ContentBlock, ContentChunk, InitializeRequest,
    InitializeResponse, LoadSessionRequest, LoadSessionResponse, NewSessionRequest,
    NewSessionResponse, PromptCapabilities, PromptRequest, PromptResponse, SessionId,
    SessionNotification, SessionUpdate, StopReason, ToolCall, ToolCallContent, ToolCallStatus,
    ToolCallUpdate, ToolCallUpdateFields,
};
use agent_client_protocol::{
    Agent as AcpAgentRole, Client, ConnectionTo, Error as AcpError, Stdio, on_receive_notification,
    on_receive_request,
};

use octos_agent::{
    Agent, AgentConfig, ProgressEvent, ProgressReporter, ToolRegistry, create_sandbox,
};
use octos_core::{AgentId, SessionScope, canonicalize_skill_read_zones};
use octos_llm::{EmbeddingProvider, LlmProvider};
use octos_memory::{EpisodeStore, MemoryStore};
use tokio::sync::Mutex;

use super::Executable;
use crate::config::Config;

/// Run octos as an ACP (Agent Client Protocol) agent over stdin/stdout.
///
/// ACP clients (Zed and other editors) spawn this process and drive it via
/// JSON-RPC on stdio. Provider/model/profile flags mirror `octos chat` so the
/// ACP agent resolves an LLM the same way.
#[derive(Debug, Args)]
pub struct AcpCommand {
    /// Working directory the agent's tools are rooted at (defaults to the
    /// current directory). Note: ACP clients also send a `cwd` with
    /// `session/new`; that per-session value takes precedence when present.
    #[arg(short, long)]
    pub cwd: Option<PathBuf>,

    /// Data directory for episodes/memory (defaults to $OCTOS_HOME or ~/.octos).
    #[arg(long)]
    pub data_dir: Option<PathBuf>,

    /// Path to config file.
    #[arg(long)]
    pub config: Option<PathBuf>,

    /// LLM provider to use (overrides config).
    #[arg(long)]
    pub provider: Option<String>,

    /// Model to use (overrides config).
    #[arg(long)]
    pub model: Option<String>,

    /// Custom base URL for the API endpoint (overrides config).
    #[arg(long)]
    pub base_url: Option<String>,

    /// Maximum tool-call iterations per prompt turn.
    #[arg(long, default_value = "20")]
    pub max_iterations: u32,

    /// Runtime profile hint (accepted for parity with `octos chat`; the ACP
    /// bridge builds a minimal agent and does not apply profile tool filters).
    #[arg(long)]
    pub profile: Option<String>,
}

impl Executable for AcpCommand {
    fn execute(self) -> Result<()> {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_stack_size(8 * 1024 * 1024) // deep agent futures need a big stack
            .build()
            .wrap_err("failed to create tokio runtime")?
            .block_on(self.run_async())
    }
}

/// Everything a spawned prompt turn needs, shared behind the session map.
struct AcpSession {
    agent: Arc<Agent>,
    /// Conversation history accumulated across prompt turns in this session.
    history: Mutex<Vec<octos_core::Message>>,
    /// Flipped to `true` by a `session/cancel` notification; the agent's
    /// shutdown flag is shared so the in-flight loop aborts.
    shutdown: Arc<AtomicBool>,
}

/// Builds a fresh octos [`Agent`] per ACP `session/new`.
///
/// A trait (rather than a concrete type) so the integration test can inject a
/// `MockLlm`-backed agent without a real provider/config, while production uses
/// [`ConfigAgentFactory`] which resolves an LLM exactly like `octos chat`.
#[async_trait::async_trait]
trait SessionAgentFactory: Send + Sync {
    /// The cwd to fall back to when the client sends an empty `session/new` cwd.
    fn default_cwd(&self) -> &std::path::Path;

    /// Build a runnable agent rooted at `cwd`, returning it plus the shared
    /// shutdown flag wired into it (flipped on `session/cancel`).
    async fn build(&self, cwd: PathBuf) -> Result<(Arc<Agent>, Arc<AtomicBool>)>;
}

/// Build the tool registry shared by every ACP session: built-ins rooted at
/// `cwd` under `sandbox_config`. RFC-0 (#1289): LRU tool deferral was removed,
/// so every enabled tool is emitted every turn — no base-tool pinning needed.
fn build_acp_tool_registry(
    cwd: &std::path::Path,
    sandbox_config: &octos_agent::SandboxConfig,
) -> ToolRegistry {
    ToolRegistry::with_builtins_and_sandbox(cwd, create_sandbox(sandbox_config))
}

/// Register the long-term memory-bank tools (recall/save, plus the refresh
/// capture tool when memory-refresh is enabled). Shared by the production
/// assembler and the unit test so both exercise the same wiring.
fn register_memory_bank(
    tools: &mut ToolRegistry,
    memory_store: &Arc<MemoryStore>,
    refresh_enabled: bool,
) {
    tools.register(octos_agent::RecallMemoryTool::new(memory_store.clone()));
    tools.register(octos_agent::SaveMemoryTool::new(memory_store.clone()));
    if refresh_enabled {
        tools.register(octos_agent::MemoryNoteTool::new(memory_store.clone()));
    }
}

/// Apply every registry-narrowing filter LAST, in the same order as `octos chat`:
/// config tool policy -> context tag filter -> provider policy -> resolved
/// profile envelope. Shared by the production assembler and the unit test.
fn finalize_tool_registry(
    tools: &mut ToolRegistry,
    config: &Config,
    provider_name: &str,
    model_id: &str,
    profile: &octos_agent::profile::ProfileDefinition,
) {
    if let Some(policy) = &config.tool_policy {
        tools.apply_policy(policy);
    }
    if !config.context_filter.is_empty() {
        tools.set_context_filter(config.context_filter.clone());
    }
    if let Some(policy) = super::chat::resolve_provider_policy(config, provider_name, model_id) {
        tools.set_provider_policy(policy);
    }
    // M8.3: profile narrowing runs AFTER every other filter (spawn_only preserved).
    profile.apply_to_registry(tools);
}

/// Process-global, **cwd-independent** state, assembled ONCE per `octos acp`
/// process (lazily, on the first `session/new`). The redb episode store is a
/// single-writer resource, so it must live here — never rebuilt per cwd. Also
/// carries the raw `Config` + resolved provider identity so per-cwd registries
/// (which DO depend on cwd) can be built on top.
struct AcpSharedStores {
    llm: Arc<dyn LlmProvider>,
    memory: Arc<EpisodeStore>,
    memory_store: Arc<MemoryStore>,
    embedder: Option<Arc<dyn EmbeddingProvider>>,
    agent_config: AgentConfig,
    memory_refresh_enabled: bool,
    max_inject_tokens: usize,
    data_dir: PathBuf,
    config: Config,
    provider_name: String,
    model_id: String,
}

/// Per-cwd assembled state, cached per distinct `session/new` cwd (so a second
/// project handled by the same process gets its OWN skills/agents/scope instead
/// of the first cwd's). Holds the tool registry (plugins/MCP/skills rooted at
/// THIS cwd), profile, agent defs, hooks, compaction, and the canonicalized
/// skill dirs used to build the per-session filesystem `SessionScope`. Mirrors
/// what `octos chat` assembles for its single embedded agent.
struct AcpBootstrap {
    /// Base registry template for this cwd. Each session clones + rebinds it via
    /// `rebind_cwd_with_permissions`.
    tool_specs: Arc<ToolRegistry>,
    sandbox: octos_agent::SandboxConfig,
    /// System-prompt override from the profile template (None = keep the agent's
    /// built-in default). Bootstrap files + memory segment are attached per session.
    profile_system_prompt: Option<String>,
    plugin_prompt_fragments: Vec<String>,
    hook_executor: Option<Arc<octos_agent::HookExecutor>>,
    agent_definitions: Arc<octos_agent::agents::AgentDefinitions>,
    profile: Arc<octos_agent::profile::ProfileDefinition>,
    compaction: Option<(
        Arc<octos_agent::compaction::CompactionRunner>,
        octos_agent::WorkspacePolicy,
    )>,
    /// Canonicalized skill dirs → `SessionScope` skill-read zones (fail-closed).
    skill_read_zones: Vec<PathBuf>,
}

impl AcpSharedStores {
    /// Open the process-global stores + build the failover provider chain. Runs
    /// exactly once; the redb episode store is opened here and never re-opened.
    #[allow(clippy::too_many_arguments)]
    async fn build(
        config: Config,
        provider_name: String,
        model: Option<String>,
        base_url: Option<String>,
        data_dir: PathBuf,
        max_iterations: u32,
    ) -> Result<Self> {
        let base_provider: Arc<dyn LlmProvider> =
            super::chat::create_provider(&provider_name, &config, model, base_url)?;
        let model_id = base_provider.model_id().to_string();
        let bundle = crate::qos_catalog::build_adaptive_provider_chain(
            base_provider,
            &config,
            &data_dir,
            /* no_retry */ false,
            crate::qos_catalog::ExporterMode::Spawn,
        );
        let llm = bundle.llm.clone();

        let memory = Arc::new(
            EpisodeStore::open(&data_dir)
                .await
                .wrap_err("failed to open episode store")?,
        );
        let memory_store = Arc::new(
            MemoryStore::open(&data_dir)
                .await
                .wrap_err("failed to open memory store")?,
        );
        let embedder = super::chat::create_embedder(&config);
        let memory_refresh_enabled =
            crate::config::MemoryConfig::refresh_enabled(config.memory.as_ref());
        let max_inject_tokens =
            crate::config::MemoryConfig::effective_max_inject_tokens(config.memory.as_ref());
        let agent_config = AgentConfig {
            max_iterations,
            save_episodes: true,
            chat_max_tokens: config.gateway.as_ref().and_then(|g| g.max_output_tokens),
            reasoning_effort: config.gateway.as_ref().and_then(|g| g.reasoning_effort),
            ..Default::default()
        };

        // NOTE(acp): the background `MemoryRefreshService` (auto-sweep of MEMORY.md)
        // is intentionally NOT started. The memory-bank tools + MEMORY.md injection
        // already give recall/save/capture (chat parity); the long-running sweep is
        // a serve-only concern (flock-arbitrated), enable later behind config.

        Ok(Self {
            llm,
            memory,
            memory_store,
            embedder,
            agent_config,
            memory_refresh_enabled,
            max_inject_tokens,
            data_dir,
            config,
            provider_name,
            model_id,
        })
    }
}

impl AcpBootstrap {
    /// Assemble the per-cwd agent state (tool registry rooted at `cwd`, profile,
    /// agents, hooks, compaction, skill read-zones) on top of the process-global
    /// [`AcpSharedStores`]. Mirrors `octos chat`'s assembly; the owned aliases
    /// below keep the body identical. Diagnostics go to stderr (ACP owns stdout).
    async fn build(
        shared: &AcpSharedStores,
        cwd: PathBuf,
        profile_arg: &Option<String>,
    ) -> Result<Self> {
        // Owned aliases so the assembly body reads exactly like `octos chat`.
        let config = shared.config.clone();
        let data_dir = shared.data_dir.clone();
        let llm = shared.llm.clone();
        let memory = shared.memory.clone();
        let memory_store = shared.memory_store.clone();
        let memory_refresh_enabled = shared.memory_refresh_enabled;
        let provider_name = shared.provider_name.clone();
        let model_id = shared.model_id.clone();

        // --- profile (resolved now; applied to the registry LAST via finalize) ---
        let (profile, profile_source_label) = super::chat::resolve_profile(profile_arg)?;
        tracing::info!(
            "profile resolved: name={} source={}",
            profile.name,
            profile_source_label
        );

        // --- tool registry ---
        let sandbox = config.sandbox.clone();
        // RFC-0 (#1289): no base-tool pinning — deferral removed.
        let mut tools = ToolRegistry::with_builtins_and_sandbox(&cwd, create_sandbox(&sandbox));

        let tool_config = Arc::new(
            octos_agent::ToolConfigStore::open(&data_dir)
                .await
                .wrap_err("failed to open tool config store")?,
        );
        tools.inject_tool_config(tool_config.clone());
        if let Some(gw) = &config.gateway {
            if let Some(secs) = gw.browser_timeout_secs {
                tools.register(
                    octos_agent::BrowserTool::with_timeout(std::time::Duration::from_secs(secs))
                        .with_config(tool_config.clone()),
                );
            }
        }

        // spawn (sync sub-agents; background delivery is a dummy channel) + research synth
        let (spawn_tx, _spawn_rx) = tokio::sync::mpsc::channel(1);
        let worker_prompt = super::load_prompt("worker", octos_agent::DEFAULT_WORKER_PROMPT);
        tools.register(
            octos_agent::SpawnTool::new(llm.clone(), memory.clone(), cwd.clone(), spawn_tx)
                .with_worker_prompt(worker_prompt)
                // Embed-on-save + recall parity: ACP spawn subagents save
                // episodes; without the shared embedder they store them
                // vectorless and their episodic recall silently skips.
                .with_optional_embedder(shared.embedder.clone()),
        );
        tools.register(octos_agent::SynthesizeResearchTool::new(
            llm.clone(),
            data_dir.clone(),
        ));

        // long-term memory bank tools
        register_memory_bank(&mut tools, &memory_store, memory_refresh_enabled);

        // MCP tools declared in config
        if !config.mcp_servers.is_empty() {
            match octos_agent::McpClient::start(&config.mcp_servers).await {
                Ok(client) => client.register_tools(&mut tools),
                Err(e) => eprintln!("Warning: MCP initialization failed: {e}"),
            }
        }

        // Bootstrap bundled app-skills / platform-skills / pipelines BEFORE plugin load.
        let project_dir = cwd.join(".octos");
        let n = octos_agent::bootstrap::bootstrap_bundled_skills(&project_dir);
        if n > 0 {
            eprintln!("Bootstrapped {n} app-skills");
        }
        let n = octos_agent::bootstrap::bootstrap_platform_skills(&project_dir);
        if n > 0 {
            eprintln!("Bootstrapped {n} platform skills");
        }
        let n = octos_agent::bootstrap::bootstrap_bundled_pipelines(&data_dir);
        if n > 0 {
            eprintln!("Bootstrapped {n} bundled pipelines");
        }

        // Load plugins/skills (binary-protocol + HTTP tool discovery).
        let plugin_dirs = Config::plugin_dirs_from_project(&cwd.join(".octos"));
        let mut plugin_result = octos_agent::PluginLoadResult::default();
        if !plugin_dirs.is_empty() {
            match octos_agent::PluginLoader::load_into_with_options(
                &mut tools,
                &plugin_dirs,
                &[],
                octos_agent::PluginLoadOptions {
                    // P1 (codex): plugins execute in the SESSION cwd, not the
                    // ACP process launch dir.
                    work_dir: Some(cwd.as_path()),
                    synthesis_config: None,
                    require_signed: config.plugins.require_signed,
                    verified_cache_dir: None,
                },
            ) {
                Ok(result) => plugin_result = result,
                Err(e) => eprintln!("Warning: plugin loading failed: {e}"),
            }
            octos_agent::plugins::register_http_skills_on_startup(&mut tools, &plugin_dirs)
                .await
                .wrap_err("HTTP tool discovery failed at agent boot")?;
        }
        if !plugin_result.mcp_servers.is_empty() {
            match octos_agent::McpClient::start(&plugin_result.mcp_servers).await {
                Ok(client) => client.register_tools(&mut tools),
                Err(e) => eprintln!("Warning: skill MCP initialization failed: {e}"),
            }
        }

        // Pipeline tool (DOT workflows), embedder-aware, marked spawn_only.
        let pipeline_tool = super::chat::build_run_pipeline_tool(
            llm.clone(),
            memory.clone(),
            cwd.clone(),
            data_dir.clone(),
            // P3 (codex): resolve provider policy NOW (finalize runs later) so
            // pipeline workers inherit tool_policy_by_provider denials.
            super::chat::resolve_provider_policy(&config, &provider_name, &model_id),
            plugin_dirs.clone(),
            config.plugins.require_signed,
            shared.embedder.clone(),
        );
        tools.register(pipeline_tool);
        tools.mark_spawn_only(
            "run_pipeline",
            Some(
                "Pipeline started in background. The final result and any artifacts will be sent here when complete."
                    .to_string(),
            ),
        );

        // Policy + context filter + provider policy + profile narrowing (LAST).
        finalize_tool_registry(&mut tools, &config, &provider_name, &model_id, &profile);

        // Sub-agent manifests from `<cwd>/agents/` layered on the built-ins.
        let agents_dir = cwd.join("agents");
        let agent_definitions = match octos_agent::agents::AgentDefinitions::load_dir(&agents_dir) {
            Ok(defs) => Arc::new(defs),
            Err(err) => {
                eprintln!(
                    "Warning: failed to load agent manifests from {}: {err}",
                    agents_dir.display()
                );
                Arc::new(octos_agent::agents::AgentDefinitions::with_builtins())
            }
        };
        profile
            .validate_against_registry(&agent_definitions)
            .wrap_err("profile references missing agent_definition ids")?;

        // System-prompt override from the profile template (bootstrap files +
        // memory segment are attached per session in `build`, since they are
        // cwd-specific / live-refreshing).
        let profile_system_prompt = profile
            .system_prompt_template
            .as_ref()
            .and_then(|tpl| super::load_profile_prompt_template(&profile.name, tpl));

        // Hooks (config + skill-declared).
        let mut all_hooks = config.hooks.clone();
        all_hooks.extend(plugin_result.hooks.clone());
        let hook_executor = if all_hooks.is_empty() {
            None
        } else {
            Some(Arc::new(octos_agent::HookExecutor::new(all_hooks)))
        };

        // Declarative compaction runner from the cwd's workspace policy.
        let compaction = match octos_agent::read_workspace_policy(&cwd) {
            Ok(Some(ws)) => {
                if let Some(cp) = ws.compaction.clone() {
                    let runner = match cp.summarizer {
                        octos_agent::CompactionSummarizerKind::LlmIterative => {
                            octos_agent::compaction::CompactionRunner::with_provider(
                                cp,
                                llm.clone(),
                            )
                        }
                        octos_agent::CompactionSummarizerKind::Extractive => {
                            octos_agent::compaction::CompactionRunner::new(cp)
                        }
                    }
                    .with_workspace_policy(&ws);
                    Some((Arc::new(runner), ws))
                } else {
                    None
                }
            }
            Ok(None) => None,
            Err(error) => {
                eprintln!("Warning: failed to read workspace policy for compaction: {error}");
                None
            }
        };

        // Canonicalized skill dirs → per-session `SessionScope` read-zones (P1).
        let skill_read_zones = canonicalize_skill_read_zones(&plugin_dirs);

        Ok(Self {
            tool_specs: Arc::new(tools),
            sandbox,
            profile_system_prompt,
            plugin_prompt_fragments: plugin_result.prompt_fragments,
            hook_executor,
            agent_definitions,
            profile: Arc::new(profile),
            compaction,
            skill_read_zones,
        })
    }
}

/// Production agent factory: raw config inputs + a lazily-built process-global
/// [`AcpSharedStores`] and a per-cwd cache of [`AcpBootstrap`]. The shared stores
/// (provider chain, redb episode store, memory) build ONCE on the first
/// `session/new`; each distinct session cwd builds its own tool
/// registry/plugins/agents/scope on top. Lazy build keeps `initialize` cheap and
/// turns a misconfiguration (e.g. missing provider key) into a `session/new`
/// error rather than a startup crash before the ACP handshake.
struct ConfigAgentFactory {
    config: Config,
    provider_name: String,
    model: Option<String>,
    base_url: Option<String>,
    data_dir: PathBuf,
    default_cwd: PathBuf,
    max_iterations: u32,
    profile: Option<String>,
    shared: tokio::sync::OnceCell<Arc<AcpSharedStores>>,
    cwd_cache: Mutex<HashMap<PathBuf, Arc<AcpBootstrap>>>,
}

#[async_trait::async_trait]
impl SessionAgentFactory for ConfigAgentFactory {
    fn default_cwd(&self) -> &std::path::Path {
        &self.default_cwd
    }

    async fn build(&self, cwd: PathBuf) -> Result<(Arc<Agent>, Arc<AtomicBool>)> {
        // Normalize the client-supplied cwd (resolve `..` + symlinks) up front so
        // the per-cwd cache key is stable and the session scope's `..`-rejecting
        // resolvers accept normal relative reads like `read_file("Cargo.toml")`
        // (codex P2). Falls back to an absolutized path if the dir doesn't exist.
        let cwd = match std::fs::canonicalize(&cwd) {
            Ok(c) => c,
            Err(_) => {
                if cwd.is_absolute() {
                    cwd
                } else if let Ok(d) = std::env::current_dir() {
                    d.join(&cwd)
                } else {
                    cwd
                }
            }
        };

        // Process-global stores (provider chain, redb episode store, memory) built
        // once, lazily, on the first session.
        let shared = self
            .shared
            .get_or_try_init(|| async {
                AcpSharedStores::build(
                    self.config.clone(),
                    self.provider_name.clone(),
                    self.model.clone(),
                    self.base_url.clone(),
                    self.data_dir.clone(),
                    self.max_iterations,
                )
                .await
                .map(Arc::new)
            })
            .await?
            .clone();

        // Per-cwd assembled state (plugins/agents/hooks/compaction/scope rooted at
        // THIS cwd), cached so a second project handled by the same process gets
        // its own skills/agents/scope instead of the first cwd's (codex P2).
        let b = {
            let mut cache = self.cwd_cache.lock().await;
            if let Some(existing) = cache.get(&cwd) {
                existing.clone()
            } else {
                let built =
                    Arc::new(AcpBootstrap::build(&shared, cwd.clone(), &self.profile).await?);
                cache.insert(cwd.clone(), built.clone());
                built
            }
        };

        // Per-session: rebind this cwd's base registry so the cwd-bound tools
        // (shell/read_file/…) run against the client's repo, and give this session
        // its own Arc-able registry.
        let tools = b.tool_specs.rebind_cwd_with_permissions(
            &cwd,
            create_sandbox(&b.sandbox),
            octos_agent::EffectivePermissions::workspace_write(),
        );

        // Per-session filesystem scope (codex P1): contain file tools + plugins to
        // cwd, with skill read-zones so `read_file` reaches SKILL.md references.
        // `cwd` is already normalized above, so the scope's resolvers accept
        // relative reads.
        let session_scope = SessionScope::solo(cwd.clone(), Vec::new())
            .ok()
            .map(|base| {
                base.with_skill_read_zones(b.skill_read_zones.clone())
                    .unwrap_or_else(|_| {
                        SessionScope::solo(cwd.clone(), Vec::new())
                            .expect("normalized cwd is valid")
                    })
            });

        // Per-session sub-agent output routing + file-state cache (cheap).
        let subagent_output_router = Arc::new(octos_agent::SubAgentOutputRouter::new(
            shared.data_dir.join("subagent-outputs"),
        ));
        let supervisor_for_summary = (*tools.supervisor()).clone();
        let subagent_summary_generator = Arc::new(octos_agent::AgentSummaryGenerator::new(
            shared.llm.clone(),
            subagent_output_router.clone(),
            supervisor_for_summary,
        ));

        let shutdown = Arc::new(AtomicBool::new(false));
        let mut agent = Agent::new(
            AgentId::new("acp"),
            shared.llm.clone(),
            tools,
            shared.memory.clone(),
        )
        .with_config(shared.agent_config.clone())
        .with_agent_definitions(b.agent_definitions.clone())
        .with_profile(b.profile.clone())
        .with_file_state_cache(Arc::new(octos_agent::FileStateCache::new()))
        .with_subagent_output_router(subagent_output_router)
        .with_subagent_summary_generator(subagent_summary_generator)
        .with_shutdown(shutdown.clone());
        if let Some(scope) = session_scope {
            agent = agent.with_session_scope(Arc::new(scope));
        }
        if let Some(embedder) = &shared.embedder {
            agent = agent.with_embedder(embedder.clone());
        }
        if let Some(hooks) = &b.hook_executor {
            agent = agent.with_hooks(hooks.clone());
        }
        if let Some((runner, ws)) = &b.compaction {
            agent = agent
                .with_compaction_runner(runner.clone())
                .with_compaction_workspace(ws.clone());
        }

        // System prompt: profile template (opt) + bootstrap files (per-cwd) +
        // token-capped MEMORY.md segment (+ live provider when refresh on) + skill
        // fragments — mirrors `octos chat`.
        if let Some(sp) = &b.profile_system_prompt {
            agent.set_system_prompt(sp.clone());
        }
        let bootstrap_files = super::load_bootstrap_files(&cwd.join(".octos"));
        if !bootstrap_files.is_empty() {
            agent.append_system_prompt(&bootstrap_files);
        }
        let memory_ctx = shared
            .memory_store
            .get_injectable_context(shared.max_inject_tokens)
            .await;
        agent.set_prompt_segment(
            octos_agent::MEMORY_SEGMENT_NAME,
            octos_agent::compose_memory_segment(&memory_ctx, shared.memory_refresh_enabled),
        );
        if shared.memory_refresh_enabled {
            agent.add_prompt_segment_provider(Arc::new(octos_agent::MemorySegmentProvider::new(
                shared.memory_store.clone(),
                shared.max_inject_tokens,
                true,
            )));
        }
        for fragment in &b.plugin_prompt_fragments {
            agent.append_system_prompt(fragment);
        }

        Ok((Arc::new(agent), shutdown))
    }
}

/// Map of ACP session id -> live octos session.
type SessionMap = Arc<Mutex<HashMap<SessionId, Arc<AcpSession>>>>;

/// Assemble the ACP [`Agent`](AcpAgentRole) builder with all handlers wired and
/// drive it over `transport` until the connection closes.
///
/// Production passes [`Stdio::new()`]; the integration test passes an in-process
/// client [`Builder`](agent_client_protocol::Builder) (which implements
/// `ConnectTo<Agent>`), so the same handler wiring is exercised without any OS
/// pipes or subprocess.
async fn spawn_acp_agent(
    factory: Arc<dyn SessionAgentFactory>,
    sessions: SessionMap,
    transport: impl agent_client_protocol::ConnectTo<AcpAgentRole> + 'static,
) -> std::result::Result<(), AcpError> {
    let factory_for_new = factory.clone();
    let sessions_for_new = sessions.clone();
    let sessions_for_prompt = sessions.clone();
    let sessions_for_cancel = sessions;

    AcpAgentRole
        .builder()
        .name("octos-acp")
        // initialize: advertise capabilities.
        .on_receive_request(
            async move |req: InitializeRequest, responder, _cx: ConnectionTo<Client>| {
                responder.respond(build_initialize_response(&req))
            },
            on_receive_request!(),
        )
        // authenticate: octos auth is env/config-key based; no interactive auth.
        // We advertise zero auth methods at initialize, so a spec-abiding client
        // never calls this; if one does, succeed.
        .on_receive_request(
            async move |_req: agent_client_protocol::schema::v1::AuthenticateRequest,
                        responder,
                        _cx: ConnectionTo<Client>| {
                responder.respond(agent_client_protocol::schema::v1::AuthenticateResponse::new())
            },
            on_receive_request!(),
        )
        // session/new: build an octos agent rooted at the client's cwd.
        .on_receive_request(
            async move |req: NewSessionRequest, responder, _cx: ConnectionTo<Client>| {
                match handle_new_session(factory_for_new.as_ref(), &sessions_for_new, req).await {
                    Ok(resp) => responder.respond(resp),
                    Err(err) => responder.respond_with_error(err),
                }
            },
            on_receive_request!(),
        )
        // session/prompt: run a turn, stream session/update, return stop reason.
        // The turn runs in a spawned task (off the dispatch loop) so an
        // interleaved `session/cancel` notification can be processed WHILE the
        // turn is in flight; the `responder` is moved into that task and fires
        // when the turn actually completes.
        .on_receive_request(
            async move |req: PromptRequest,
                        responder: agent_client_protocol::Responder<PromptResponse>,
                        cx: ConnectionTo<Client>| {
                handle_prompt(&sessions_for_prompt, req, cx, responder)
            },
            on_receive_request!(),
        )
        // session/cancel: abort the in-flight turn for that session.
        .on_receive_notification(
            async move |notif: CancelNotification, _cx: ConnectionTo<Client>| {
                handle_cancel(&sessions_for_cancel, notif).await;
                Ok(())
            },
            on_receive_notification!(),
        )
        .connect_to(transport)
        .await
}

/// Test-support factory that wraps a caller-supplied [`LlmProvider`] (e.g. a
/// `MockLlm`) so the end-to-end ACP integration test can drive the real handler
/// wiring without a network-backed provider or on-disk config.
///
/// Not for production use — production goes through [`ConfigAgentFactory`].
#[doc(hidden)]
pub struct TestAgentFactory {
    llm: Arc<dyn LlmProvider>,
    memory_dir: PathBuf,
    default_cwd: PathBuf,
}

#[doc(hidden)]
impl TestAgentFactory {
    /// Build a factory that hands every session an agent backed by `llm`, with
    /// episodic memory in `memory_dir` and tools rooted at `default_cwd`.
    pub fn new(llm: Arc<dyn LlmProvider>, memory_dir: PathBuf, default_cwd: PathBuf) -> Self {
        Self {
            llm,
            memory_dir,
            default_cwd,
        }
    }
}

#[async_trait::async_trait]
impl SessionAgentFactory for TestAgentFactory {
    fn default_cwd(&self) -> &std::path::Path {
        &self.default_cwd
    }

    async fn build(&self, cwd: PathBuf) -> Result<(Arc<Agent>, Arc<AtomicBool>)> {
        let memory = Arc::new(EpisodeStore::open(&self.memory_dir).await?);
        let tools = build_acp_tool_registry(&cwd, &octos_agent::SandboxConfig::default());
        let shutdown = Arc::new(AtomicBool::new(false));
        let agent = Agent::new(AgentId::new("acp-test"), self.llm.clone(), tools, memory)
            .with_shutdown(shutdown.clone());
        Ok((Arc::new(agent), shutdown))
    }
}

/// Test-support transport: the octos ACP **agent** exposed as a
/// `ConnectTo<Client>` so an in-process ACP client can use it directly as its
/// transport (no OS pipes, no subprocess, no network).
///
/// The `agent-client-protocol` runtime treats one endpoint as the "transport"
/// for the other; the agent side is `ConnectTo<Client>` (since `Client` is the
/// `Agent` role's counterpart). The integration test does:
///
/// ```ignore
/// Client.builder()
///     .on_receive_notification(record_updates, ...)
///     .connect_with(OctosAcpAgentTransport::new(factory), |conn| async {
///         conn.send_request(InitializeRequest::new(V1)).block_task().await?;
///         // session/new, session/prompt, assert stop reason ...
///     })
///     .await
/// ```
#[doc(hidden)]
pub struct OctosAcpAgentTransport {
    factory: TestAgentFactory,
}

#[doc(hidden)]
impl OctosAcpAgentTransport {
    /// Wrap a [`TestAgentFactory`] so it can serve one in-process ACP client.
    pub fn new(factory: TestAgentFactory) -> Self {
        Self { factory }
    }
}

impl agent_client_protocol::ConnectTo<Client> for OctosAcpAgentTransport {
    async fn connect_to(
        self,
        client: impl agent_client_protocol::ConnectTo<AcpAgentRole> + 'static,
    ) -> std::result::Result<(), AcpError> {
        let sessions: SessionMap = Arc::new(Mutex::new(HashMap::new()));
        spawn_acp_agent(Arc::new(self.factory), sessions, client).await
    }
}

impl AcpCommand {
    async fn run_async(self) -> Result<()> {
        // Resolve config the same way `octos chat` does.
        let cwd = match &self.cwd {
            Some(c) => c.clone(),
            None => std::env::current_dir().wrap_err("failed to get current directory")?,
        };
        let ctx = super::resolve_command_context(self.data_dir.clone())?;
        let data_dir = ctx.data_dir.clone();
        let config = if let Some(config_path) = &self.config {
            Config::from_file(config_path)?
        } else {
            Config::load_with_context(&cwd, &ctx)?
        };

        let model = self.model.clone().or(config.model.clone());
        let base_url = self.base_url.clone().or(config.base_url.clone());
        let provider_name = self
            .provider
            .clone()
            .or(config.provider.clone())
            .or_else(|| {
                model
                    .as_deref()
                    .and_then(crate::config::detect_provider)
                    .map(String::from)
            })
            .ok_or_else(|| {
                eyre::eyre!(
                    "no LLM provider configured. Run `octos init` or set provider in config.json"
                )
            })?;

        // The full shared agent stack (provider chain, memory, plugins, MCP,
        // hooks, system prompt, compaction) is assembled lazily on the first
        // `session/new` — see `ConfigAgentFactory`.
        let factory: Arc<dyn SessionAgentFactory> = Arc::new(ConfigAgentFactory {
            config,
            provider_name,
            model,
            base_url,
            data_dir,
            default_cwd: cwd,
            max_iterations: self.max_iterations,
            profile: self.profile.clone(),
            shared: tokio::sync::OnceCell::new(),
            cwd_cache: Mutex::new(HashMap::new()),
        });

        let sessions: SessionMap = Arc::new(Mutex::new(HashMap::new()));

        // Drive the ACP agent over stdio until the client disconnects.
        spawn_acp_agent(factory, sessions, Stdio::new())
            .await
            .map_err(|e| eyre::eyre!("ACP connection ended with error: {e}"))
    }
}

/// Build the `initialize` response advertising octos's agent capabilities.
///
/// Per ACP the agent must reply with a protocol version it actually supports —
/// NOT whatever the client requested. This handler implements v1 only, so we
/// echo the client's version only when it is a version we support (V1);
/// otherwise (a newer/unknown version) we reply with the latest version we do
/// support (`ProtocolVersion::LATEST`, currently V1) rather than falsely
/// advertising support for the requested one.
fn build_initialize_response(req: &InitializeRequest) -> InitializeResponse {
    use agent_client_protocol::schema::ProtocolVersion;

    // Prompt capabilities: octos consumes plain text prompt blocks today.
    // Image/audio/embedded-context are left false (v1 extracts text only).
    let prompt = PromptCapabilities::new();
    let caps = AgentCapabilities::new()
        // We accept `session/load` no better than `session/new` today, so do
        // NOT advertise loadSession (leave it false / default).
        .prompt_capabilities(prompt);

    let negotiated = if req.protocol_version == ProtocolVersion::V1 {
        req.protocol_version
    } else {
        ProtocolVersion::LATEST
    };
    InitializeResponse::new(negotiated).agent_capabilities(caps)
}

/// Handle `session/new`: build a fresh octos agent and register it.
async fn handle_new_session(
    factory: &dyn SessionAgentFactory,
    sessions: &SessionMap,
    req: NewSessionRequest,
) -> std::result::Result<NewSessionResponse, AcpError> {
    // MCP servers requested by the client are ignored in v1 (logged only).
    if !req.mcp_servers.is_empty() {
        tracing::info!(
            count = req.mcp_servers.len(),
            "ACP session/new requested MCP servers; ignored in v1 (octos runs its own tools)"
        );
    }

    let cwd = if req.cwd.as_os_str().is_empty() {
        factory.default_cwd().to_path_buf()
    } else {
        req.cwd.clone()
    };

    let (agent, shutdown) = factory
        .build(cwd)
        .await
        .map_err(|e| agent_client_protocol::util::internal_error(format!("build agent: {e}")))?;

    let session_id = new_session_id();
    let session = Arc::new(AcpSession {
        agent,
        history: Mutex::new(Vec::new()),
        shutdown,
    });
    sessions.lock().await.insert(session_id.clone(), session);

    Ok(NewSessionResponse::new(session_id))
}

/// Handle `session/prompt`: spawn a turn off the dispatch loop, stream
/// `session/update`s to the client, and fire `responder` with the ACP stop
/// reason once the turn completes.
///
/// Returns immediately (marking the request handled) so the dispatch loop stays
/// free to process an interleaved `session/cancel` while the turn runs. The
/// actual JSON-RPC response is sent from the spawned task via `responder`.
fn handle_prompt(
    sessions: &SessionMap,
    req: PromptRequest,
    cx: ConnectionTo<Client>,
    responder: agent_client_protocol::Responder<PromptResponse>,
) -> std::result::Result<(), AcpError> {
    // Look up the session synchronously (try_lock avoids awaiting in the
    // dispatch loop; the map is only briefly held by session/new + cancel).
    let session = {
        let map = match sessions.try_lock() {
            Ok(map) => map,
            // Contended (a session/new or cancel is mid-flight): fall back to a
            // blocking lock inside the spawn instead. Rare; keep it simple by
            // cloning the Arc<Mutex> and resolving in the task below.
            Err(_) => {
                let sessions = sessions.clone();
                let session_id = req.session_id.clone();
                return cx.clone().spawn(run_prompt_turn_deferred(
                    sessions, session_id, req, cx, responder,
                ));
            }
        };
        map.get(&req.session_id).cloned()
    };

    let Some(session) = session else {
        return responder.respond_with_error(agent_client_protocol::util::internal_error(format!(
            "unknown session id: {}",
            req.session_id.0
        )));
    };

    // Clear any stale cancel flag from a previous cancelled turn SYNCHRONOUSLY
    // on the dispatch loop, before spawning the turn. The turn runs in a
    // spawned task, so any `session/cancel` dispatched AFTER this handler
    // returns (i.e. for THIS turn) must win — resetting here (not inside the
    // spawned task) guarantees a later cancel's `store(true)` is not clobbered.
    session.shutdown.store(false, Ordering::Release);

    cx.clone()
        .spawn(run_prompt_turn(session, req, cx, responder))
}

/// Resolve the session from the map inside the spawned task (used only when the
/// session map was contended at handler time), then run the turn.
async fn run_prompt_turn_deferred(
    sessions: SessionMap,
    session_id: SessionId,
    req: PromptRequest,
    cx: ConnectionTo<Client>,
    responder: agent_client_protocol::Responder<PromptResponse>,
) -> std::result::Result<(), AcpError> {
    let session = {
        let map = sessions.lock().await;
        map.get(&session_id).cloned()
    };
    match session {
        Some(session) => {
            // Reset the stale cancel flag before running the turn. This path is
            // rare (taken only when the session map was contended at handler
            // time), and unlike `handle_prompt` the reset here happens inside
            // the spawned task rather than synchronously on the dispatch loop —
            // so a `session/cancel` racing the very first poll could still be
            // clobbered. Best-effort for the contended path; the common path
            // (`handle_prompt`) resets synchronously and closes the race.
            session.shutdown.store(false, Ordering::Release);
            run_prompt_turn(session, req, cx, responder).await
        }
        None => responder.respond_with_error(agent_client_protocol::util::internal_error(format!(
            "unknown session id: {}",
            session_id.0
        ))),
    }
}

/// The actual prompt turn: attach a streaming reporter, run the octos agent
/// loop, persist history, and respond with the ACP stop reason.
async fn run_prompt_turn(
    session: Arc<AcpSession>,
    req: PromptRequest,
    cx: ConnectionTo<Client>,
    responder: agent_client_protocol::Responder<PromptResponse>,
) -> std::result::Result<(), AcpError> {
    // NOTE: the stale-cancel-flag reset does NOT happen here. It is done
    // synchronously on the dispatch loop in `handle_prompt` (and in
    // `run_prompt_turn_deferred` for the rare contended path) BEFORE this turn
    // is spawned, so a `session/cancel` dispatched for this turn correctly wins
    // instead of being clobbered by a reset in this spawned task.
    let user_text = extract_prompt_text(&req.prompt);
    let history = { session.history.lock().await.clone() };

    // Attach a per-turn reporter that streams `session/update` to this client.
    // `set_reporter` takes `&self` (RwLock interior mutability) precisely so the
    // agent can stay behind an `Arc` across turns.
    let reporter: Arc<dyn ProgressReporter> =
        Arc::new(AcpProgressReporter::new(req.session_id.clone(), cx));
    session.agent.set_reporter(reporter);

    let outcome = session
        .agent
        .process_message(&user_text, &history, vec![])
        .await;

    let cancelled = session.shutdown.load(Ordering::Acquire);
    let response = match outcome {
        Ok(resp) => {
            // Persist this turn's messages (user + assistant + tool messages).
            // `ConversationResponse.messages` is THIS-TURN-ONLY (the user
            // message at front + the assistant/tool messages produced this
            // turn); it does NOT include prior history. The stored history
            // still holds the earlier turns (we only cloned it above, never
            // cleared it), so APPEND — replacing would drop every earlier turn
            // and the agent would forget context after the second prompt.
            {
                let mut h = session.history.lock().await;
                let assistant_reply = resp.content.clone();
                h.extend(resp.messages);
                // For a plain-text turn, `resp.messages` carries the user row but
                // NOT the final assistant text (that is streamed live via
                // `content`). Persist it explicitly so later prompts can refer
                // back to what the agent SAID — mirroring the chat REPL, which
                // also pushes `response.content` as an assistant message. Guard
                // against double-append for turns where messages already ends
                // with that assistant reply. Skip entirely on a CANCELLED turn:
                // `content` there is a partial/aborted stream, and persisting it
                // would condition the next prompt on a reply the client never
                // completed.
                let already_persisted = matches!(
                    h.last(),
                    Some(last) if last.role == octos_core::MessageRole::Assistant && last.content == assistant_reply
                );
                if !cancelled && !assistant_reply.is_empty() && !already_persisted {
                    h.push(octos_core::Message {
                        role: octos_core::MessageRole::Assistant,
                        content: assistant_reply,
                        media: vec![],
                        tool_calls: None,
                        tool_call_id: None,
                        reasoning_content: None,
                        client_message_id: None,
                        thread_id: None,
                        timestamp: chrono::Utc::now(),
                    });
                }
            }
            // Distinguish a client-driven cancel from a natural end-of-turn.
            let stop_reason = if cancelled {
                StopReason::Cancelled
            } else {
                StopReason::EndTurn
            };
            PromptResponse::new(stop_reason)
        }
        Err(err) => {
            // If the turn ended because we were cancelled, report Cancelled
            // rather than surfacing an error frame.
            if cancelled {
                PromptResponse::new(StopReason::Cancelled)
            } else {
                return responder.respond_with_error(agent_client_protocol::util::internal_error(
                    format!("prompt turn failed: {err}"),
                ));
            }
        }
    };
    responder.respond(response)
}

/// Handle `session/cancel`: abort the in-flight turn for the given session.
///
/// octos exposes a shared `Arc<AtomicBool>` shutdown flag on the agent (set via
/// `Agent::with_shutdown`); flipping it aborts the running loop at its next
/// checkpoint. Unknown session ids are ignored (the client may race a cancel
/// against session teardown).
async fn handle_cancel(sessions: &SessionMap, notif: CancelNotification) {
    let map = sessions.lock().await;
    if let Some(session) = map.get(&notif.session_id) {
        session.shutdown.store(true, Ordering::Release);
        tracing::info!(session_id = %notif.session_id.0, "ACP session/cancel: aborting turn");
    } else {
        tracing::debug!(
            session_id = %notif.session_id.0,
            "ACP session/cancel for unknown session; ignoring"
        );
    }
}

/// Handle `session/load` — NOT supported in v1: octos does not persist ACP
/// sessions across process restarts. Kept as a documented stub so the intent is
/// explicit; not wired into the builder (an unregistered method returns
/// `method_not_found` automatically).
#[allow(dead_code)]
fn handle_load_session_unsupported(
    _req: LoadSessionRequest,
) -> std::result::Result<LoadSessionResponse, AcpError> {
    Err(AcpError::method_not_found())
}

/// Generate a fresh, unique ACP session id.
fn new_session_id() -> SessionId {
    SessionId::new(format!("octos-{}", uuid::Uuid::new_v4()))
}

/// Extract the plain text from a prompt's ACP content blocks.
///
/// octos's agent loop consumes a single text prompt; we concatenate the text of
/// every `ContentBlock::Text` (and the text payload of embedded text
/// resources), joining multiple blocks with newlines. `ContentBlock::ResourceLink`
/// is BASELINE ACP prompt content (file/context attachments), so we surface each
/// link as a `[Resource: <title|name> (<uri>)]` reference (plus its description on
/// the next line, if any) rather than dropping it. Binary non-text blocks (image,
/// audio) are still skipped in v1 — we advertise text-only prompt capabilities at
/// `initialize`.
fn extract_prompt_text(blocks: &[ContentBlock]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text(t) => parts.push(t.text.clone()),
            ContentBlock::Resource(r) => {
                if let agent_client_protocol::schema::v1::EmbeddedResourceResource::TextResourceContents(tr) =
                    &r.resource
                {
                    parts.push(tr.text.clone());
                }
            }
            // Resource links are baseline ACP prompt content (file/context
            // attachments). Surface the reference as usable context text so a
            // prompt made entirely of links still reaches octos with content.
            ContentBlock::ResourceLink(link) => {
                let label = link.title.as_deref().unwrap_or(&link.name);
                let mut part = format!("[Resource: {label} ({})]", link.uri);
                if let Some(desc) = &link.description {
                    part.push('\n');
                    part.push_str(desc);
                }
                parts.push(part);
            }
            // Remaining non-text blocks (image, audio) and any future variants
            // are not consumed by the text-only agent loop. `ContentBlock`
            // is `#[non_exhaustive]`, so a catch-all is required.
            _ => {}
        }
    }
    parts.join("\n")
}

/// Translate a single octos [`ProgressEvent`] into the ACP [`SessionUpdate`]
/// that should be streamed to the client, or `None` if the event has no ACP
/// projection.
///
/// This is the pure, side-effect-free core of the streaming bridge — unit
/// tested without any live connection.
///
/// Mapping:
/// - `StreamChunk` / `Response`   -> `AgentMessageChunk` (assistant text)
/// - `ReasoningChunk`             -> `AgentThoughtChunk` (thinking)
/// - `ToolStarted`                -> `ToolCall` (status = InProgress)
/// - `ToolCompleted`              -> `ToolCallUpdate` (Completed/Failed + output)
/// - everything else              -> `None`
pub(crate) fn progress_event_to_acp(event: &ProgressEvent) -> Option<SessionUpdate> {
    match event {
        ProgressEvent::StreamChunk { text, .. } => Some(SessionUpdate::AgentMessageChunk(
            ContentChunk::new(ContentBlock::from(text.clone())),
        )),
        ProgressEvent::Response { content, .. } => Some(SessionUpdate::AgentMessageChunk(
            ContentChunk::new(ContentBlock::from(content.clone())),
        )),
        ProgressEvent::ReasoningChunk { text, .. } => Some(SessionUpdate::AgentThoughtChunk(
            ContentChunk::new(ContentBlock::from(text.clone())),
        )),
        ProgressEvent::ToolStarted { name, tool_id } => {
            let call = ToolCall::new(tool_id.clone(), name.clone())
                .kind(tool_kind_for(name))
                .status(ToolCallStatus::InProgress);
            Some(SessionUpdate::ToolCall(call))
        }
        ProgressEvent::ToolCompleted {
            tool_id,
            success,
            output_preview,
            ..
        } => {
            let status = if *success {
                ToolCallStatus::Completed
            } else {
                ToolCallStatus::Failed
            };
            let mut fields = ToolCallUpdateFields::new().status(status);
            if !output_preview.is_empty() {
                let content: ToolCallContent =
                    ToolCallContent::from(ContentBlock::from(output_preview.clone()));
                fields = fields.content(vec![content]);
            }
            Some(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                tool_id.clone(),
                fields,
            )))
        }
        // No ACP projection: lifecycle/bookkeeping/token/cost events.
        _ => None,
    }
}

/// Best-effort mapping from an octos tool name to an ACP [`ToolKind`] so clients
/// can render an appropriate icon. Unknown tools fall back to `Other`.
fn tool_kind_for(name: &str) -> agent_client_protocol::schema::v1::ToolKind {
    use agent_client_protocol::schema::v1::ToolKind;
    match name {
        "read_file" | "list_dir" => ToolKind::Read,
        "write_file" | "edit_file" => ToolKind::Edit,
        "glob" | "grep" => ToolKind::Search,
        "shell" => ToolKind::Execute,
        "web_search" | "web_fetch" => ToolKind::Fetch,
        _ => ToolKind::Other,
    }
}

/// A [`ProgressReporter`] that forwards octos agent progress to an ACP client as
/// `session/update` notifications on the connection.
///
/// The translation itself lives in [`progress_event_to_acp`]; this type just
/// wraps it with the connection handle and session id and fires the
/// notification (best-effort — a send failure is logged, never fatal to the
/// turn).
struct AcpProgressReporter {
    session_id: SessionId,
    cx: ConnectionTo<Client>,
    /// Whether any streaming `StreamChunk` was already forwarded this turn. The
    /// agent loop emits streaming deltas AND a final full `Response` carrying the
    /// same text; forwarding both makes an ACP client render the assistant
    /// message twice, so once we've streamed, the final `Response` is dropped.
    streamed: std::sync::atomic::AtomicBool,
}

impl AcpProgressReporter {
    fn new(session_id: SessionId, cx: ConnectionTo<Client>) -> Self {
        Self {
            session_id,
            cx,
            streamed: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

impl ProgressReporter for AcpProgressReporter {
    fn report(&self, event: ProgressEvent) {
        if let Some(update) = project_progress_event_deduped(&event, &self.streamed) {
            let notif = SessionNotification::new(self.session_id.clone(), update);
            if let Err(err) = self.cx.send_notification(notif) {
                tracing::warn!(error = %err, "failed to send ACP session/update");
            }
        }
    }
}

/// Stateful wrapper over [`progress_event_to_acp`] that de-duplicates assistant
/// text: a `StreamChunk` marks the turn as streamed; a subsequent full
/// `Response` (which repeats the streamed text) is then dropped so ACP clients
/// don't render the message twice. A `Response` with NO prior streaming (a
/// non-streaming provider) is still forwarded. `streamed` is per-turn (the
/// reporter is constructed per prompt).
pub(crate) fn project_progress_event_deduped(
    event: &ProgressEvent,
    streamed: &std::sync::atomic::AtomicBool,
) -> Option<SessionUpdate> {
    use std::sync::atomic::Ordering;
    match event {
        ProgressEvent::StreamChunk { .. } => streamed.store(true, Ordering::Relaxed),
        ProgressEvent::Response { .. } if streamed.load(Ordering::Relaxed) => return None,
        _ => {}
    }
    progress_event_to_acp(event)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ACP assembler wires the long-term memory bank (chat parity), so the
    /// agent is no longer a bare-builtins skeleton. Exercises the real
    /// `register_memory_bank` helper the production `AcpBootstrap` uses. Offline:
    /// `MemoryStore::open` is local (no network / no provider key).
    #[tokio::test]
    async fn should_register_memory_bank_tools_in_acp_assembly() {
        let dir = std::env::temp_dir().join(format!("octos-acp-membank-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(MemoryStore::open(&dir).await.expect("open memory store"));

        let mut tools = ToolRegistry::with_builtins_and_sandbox(
            &dir,
            create_sandbox(&octos_agent::SandboxConfig::default()),
        );
        assert!(
            !tools.is_tool_visible("recall_memory"),
            "baseline builtins should not carry the long-term memory bank"
        );
        register_memory_bank(&mut tools, &store, /* refresh_enabled */ true);
        for t in ["recall_memory", "save_memory", "memory_note"] {
            assert!(
                tools.is_tool_visible(t),
                "{t} must be registered by the ACP memory-bank wiring"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// RFC-0 (#1289): LRU tool deferral was removed, so `shell` /
    /// `exec_command` / `bash` (like every enabled tool) are always visible
    /// across a long ACP session.
    #[test]
    fn should_keep_exec_tools_visible_when_long_session_only_uses_file_tools() {
        let reg = build_acp_tool_registry(
            std::path::Path::new("."),
            &octos_agent::SandboxConfig::default(),
        );
        for t in ["shell", "exec_command", "bash"] {
            assert!(
                reg.is_tool_visible(t),
                "{t} must always be visible (no LRU eviction after RFC-0)"
            );
        }
    }
    use agent_client_protocol::schema::ProtocolVersion;
    use octos_agent::SilentReporter;
    use std::time::Duration;

    fn text_of_chunk(update: &SessionUpdate) -> Option<&str> {
        match update {
            SessionUpdate::AgentMessageChunk(c) | SessionUpdate::AgentThoughtChunk(c) => {
                match &c.content {
                    ContentBlock::Text(t) => Some(&t.text),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    #[test]
    fn should_map_stream_chunk_to_agent_message_chunk_when_streaming_text() {
        let ev = ProgressEvent::StreamChunk {
            text: "hello world".into(),
            iteration: 1,
        };
        let update = progress_event_to_acp(&ev).expect("stream chunk maps to an update");
        assert!(matches!(update, SessionUpdate::AgentMessageChunk(_)));
        assert_eq!(text_of_chunk(&update), Some("hello world"));
    }

    #[test]
    fn should_map_response_to_agent_message_chunk_when_non_streamed() {
        let ev = ProgressEvent::Response {
            content: "final answer".into(),
            iteration: 1,
        };
        let update = progress_event_to_acp(&ev).expect("response maps to an update");
        assert!(matches!(update, SessionUpdate::AgentMessageChunk(_)));
        assert_eq!(text_of_chunk(&update), Some("final answer"));
    }

    #[test]
    fn should_drop_final_response_when_text_was_already_streamed() {
        // Live-test regression: the agent loop emits streaming deltas AND a final
        // full `Response` with the same text. Forwarding both doubles the message
        // in an ACP client. Once a StreamChunk is seen, the trailing Response is
        // dropped; a Response with no prior streaming is still forwarded.
        use std::sync::atomic::AtomicBool;
        let streamed = AtomicBool::new(false);

        let chunk = ProgressEvent::StreamChunk {
            text: "final answer".into(),
            iteration: 1,
        };
        assert!(
            matches!(
                project_progress_event_deduped(&chunk, &streamed),
                Some(SessionUpdate::AgentMessageChunk(_))
            ),
            "stream chunk is forwarded and marks the turn streamed"
        );

        let response = ProgressEvent::Response {
            content: "final answer".into(),
            iteration: 1,
        };
        assert!(
            project_progress_event_deduped(&response, &streamed).is_none(),
            "the duplicate final Response must be dropped after streaming"
        );

        // A Response with NO prior streaming (non-streaming provider) still emits.
        let fresh = AtomicBool::new(false);
        assert!(
            matches!(
                project_progress_event_deduped(&response, &fresh),
                Some(SessionUpdate::AgentMessageChunk(_))
            ),
            "a Response without prior streaming is forwarded"
        );
    }

    #[test]
    fn should_map_reasoning_chunk_to_agent_thought_chunk_when_thinking() {
        let ev = ProgressEvent::ReasoningChunk {
            text: "let me think".into(),
            iteration: 2,
        };
        let update = progress_event_to_acp(&ev).expect("reasoning maps to a thought update");
        assert!(matches!(update, SessionUpdate::AgentThoughtChunk(_)));
        assert_eq!(text_of_chunk(&update), Some("let me think"));
    }

    #[test]
    fn should_map_tool_started_to_tool_call_in_progress_when_tool_begins() {
        let ev = ProgressEvent::ToolStarted {
            name: "shell".into(),
            tool_id: "call-1".into(),
        };
        let update = progress_event_to_acp(&ev).expect("tool start maps to a tool call");
        match update {
            SessionUpdate::ToolCall(call) => {
                assert_eq!(call.tool_call_id.0.as_ref(), "call-1");
                assert_eq!(call.title, "shell");
                assert!(matches!(call.status, ToolCallStatus::InProgress));
                assert!(matches!(
                    call.kind,
                    agent_client_protocol::schema::v1::ToolKind::Execute
                ));
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn should_map_tool_completed_success_to_completed_update_with_output() {
        let ev = ProgressEvent::ToolCompleted {
            name: "read_file".into(),
            tool_id: "call-2".into(),
            success: true,
            output_preview: "file contents".into(),
            duration: Duration::from_millis(5),
        };
        let update = progress_event_to_acp(&ev).expect("tool completion maps to an update");
        match update {
            SessionUpdate::ToolCallUpdate(u) => {
                assert_eq!(u.tool_call_id.0.as_ref(), "call-2");
                assert!(matches!(u.fields.status, Some(ToolCallStatus::Completed)));
                let content = u.fields.content.expect("output content present");
                assert_eq!(content.len(), 1);
                match &content[0] {
                    ToolCallContent::Content(c) => match &c.content {
                        ContentBlock::Text(t) => assert_eq!(t.text, "file contents"),
                        other => panic!("expected text content, got {other:?}"),
                    },
                    other => panic!("expected Content variant, got {other:?}"),
                }
            }
            other => panic!("expected ToolCallUpdate, got {other:?}"),
        }
    }

    #[test]
    fn should_map_tool_completed_failure_to_failed_status() {
        let ev = ProgressEvent::ToolCompleted {
            name: "shell".into(),
            tool_id: "call-3".into(),
            success: false,
            output_preview: String::new(),
            duration: Duration::from_millis(1),
        };
        let update = progress_event_to_acp(&ev).expect("failed tool maps to an update");
        match update {
            SessionUpdate::ToolCallUpdate(u) => {
                assert!(matches!(u.fields.status, Some(ToolCallStatus::Failed)));
                // Empty output_preview must not emit a content vec.
                assert!(u.fields.content.is_none());
            }
            other => panic!("expected ToolCallUpdate, got {other:?}"),
        }
    }

    #[test]
    fn should_return_none_for_lifecycle_events_without_acp_projection() {
        // A representative sample of events that must not stream to the client.
        let cases = [
            ProgressEvent::TaskStarted {
                task_id: "t".into(),
            },
            ProgressEvent::Thinking { iteration: 1 },
            ProgressEvent::TokenUsage {
                input_tokens: 10,
                output_tokens: 20,
            },
            ProgressEvent::StreamDone { iteration: 1 },
            ProgressEvent::TaskCompleted {
                success: true,
                iterations: 1,
                duration: Duration::from_millis(1),
            },
        ];
        for ev in cases {
            assert!(
                progress_event_to_acp(&ev).is_none(),
                "event {ev:?} must have no ACP projection"
            );
        }
    }

    #[test]
    fn should_extract_and_join_text_blocks_when_prompt_has_multiple_blocks() {
        use agent_client_protocol::schema::v1::{ImageContent, TextContent};
        let blocks = vec![
            ContentBlock::Text(TextContent::new("first line")),
            // An image block must be skipped, not crash.
            ContentBlock::Image(ImageContent::new("base64data", "image/png")),
            ContentBlock::Text(TextContent::new("second line")),
        ];
        let text = extract_prompt_text(&blocks);
        assert_eq!(text, "first line\nsecond line");
    }

    #[test]
    fn should_extract_text_from_embedded_text_resource() {
        use agent_client_protocol::schema::v1::{
            EmbeddedResource, EmbeddedResourceResource, TextResourceContents,
        };
        let blocks = vec![ContentBlock::Resource(EmbeddedResource::new(
            EmbeddedResourceResource::TextResourceContents(TextResourceContents::new(
                "resource body",
                "file:///tmp/x.txt",
            )),
        ))];
        assert_eq!(extract_prompt_text(&blocks), "resource body");
    }

    #[test]
    fn should_include_resource_link_reference_when_prompt_has_resource_link() {
        use agent_client_protocol::schema::v1::ResourceLink;
        // A resource link with a title + description: the reference (title +
        // uri) and the description must both surface in the extracted text.
        let link = ResourceLink::new("main.rs", "file:///repo/src/main.rs")
            .title("Main entrypoint")
            .description("The program entrypoint");
        let blocks = vec![ContentBlock::ResourceLink(link)];
        let text = extract_prompt_text(&blocks);
        assert!(
            text.contains("file:///repo/src/main.rs"),
            "resource-link uri must be surfaced; got {text:?}"
        );
        assert!(
            text.contains("Main entrypoint"),
            "resource-link title must be surfaced; got {text:?}"
        );
        assert!(
            text.contains("The program entrypoint"),
            "resource-link description must be surfaced; got {text:?}"
        );
    }

    #[test]
    fn should_use_resource_link_name_when_title_absent() {
        use agent_client_protocol::schema::v1::ResourceLink;
        // No title -> fall back to the link name; no description -> single line.
        let link = ResourceLink::new("notes.txt", "file:///repo/notes.txt");
        let blocks = vec![ContentBlock::ResourceLink(link)];
        let text = extract_prompt_text(&blocks);
        assert_eq!(text, "[Resource: notes.txt (file:///repo/notes.txt)]");
    }

    #[test]
    fn should_extract_from_string_convertible_block_via_from_impl() {
        // The crate provides `From<Into<String>> for ContentBlock` (Text). This
        // is the ergonomic path used by the reporter mapping above.
        let block: ContentBlock = "just text".into();
        assert_eq!(
            extract_prompt_text(std::slice::from_ref(&block)),
            "just text"
        );
    }

    #[test]
    fn should_build_initialize_response_echoing_protocol_version_and_advertising_text_prompt() {
        let req = InitializeRequest::new(ProtocolVersion::V1);
        let resp = build_initialize_response(&req);
        // We support V1, so a V1 request is echoed back as V1.
        assert_eq!(resp.protocol_version, ProtocolVersion::V1);
        // Text-only prompt capabilities: image/audio/embedded_context all false.
        assert!(!resp.agent_capabilities.prompt_capabilities.image);
        assert!(!resp.agent_capabilities.prompt_capabilities.audio);
        // We do not advertise loadSession in v1.
        assert!(!resp.agent_capabilities.load_session);
    }

    #[test]
    fn should_downgrade_to_latest_supported_version_when_client_requests_newer() {
        // A client requesting a newer/unsupported protocol version must NOT get
        // that version echoed back (that would falsely advertise support this
        // v1-only handler doesn't implement). We reply with the latest version
        // we actually support (`ProtocolVersion::LATEST`, currently V1).
        let newer = ProtocolVersion::from(2u16);
        assert_ne!(newer, ProtocolVersion::V1, "sanity: 2 is not V1");
        let req = InitializeRequest::new(newer);
        let resp = build_initialize_response(&req);
        assert_eq!(resp.protocol_version, ProtocolVersion::LATEST);
        assert_eq!(resp.protocol_version, ProtocolVersion::V1);
    }

    #[test]
    fn should_generate_unique_session_ids() {
        let a = new_session_id();
        let b = new_session_id();
        assert_ne!(a.0, b.0, "session ids must be unique");
        assert!(a.0.starts_with("octos-"));
    }

    /// A no-op reporter is a valid `ProgressReporter`; this pins that the
    /// silent path (used when a turn runs without a live ACP connection, e.g.
    /// in unit tests) compiles and does nothing.
    #[test]
    fn should_accept_silent_reporter_as_progress_reporter() {
        let reporter: Arc<dyn ProgressReporter> = Arc::new(SilentReporter);
        reporter.report(ProgressEvent::Thinking { iteration: 1 });
    }

    // ---- Cancellation-race coverage (Fix: move the stale-flag reset onto the
    // dispatch loop). These drive the REAL handler wiring in-process (the same
    // `spawn_acp_agent` `octos acp` uses), so the reset relocation from
    // `run_prompt_turn` to `handle_prompt` is exercised end to end. ----

    /// A `MockLlm` whose FIRST `chat()` blocks on a barrier until the test
    /// releases it (announcing entry first), so the test can deliver a
    /// `session/cancel` while the turn is genuinely in flight. Later calls
    /// return immediately. Always returns `EndTurn` — the turn's `Cancelled`
    /// outcome must come from the shutdown flag surviving into the turn, not
    /// from the LLM.
    struct BarrierLlm {
        calls: std::sync::atomic::AtomicUsize,
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
        /// One entry per `chat()` call: the `content` of every incoming message,
        /// so a test can assert what history a later turn was handed.
        seen: Arc<Mutex<Vec<Vec<String>>>>,
    }

    #[async_trait::async_trait]
    impl octos_llm::LlmProvider for BarrierLlm {
        async fn chat(
            &self,
            messages: &[octos_core::Message],
            _tools: &[octos_llm::ToolSpec],
            _config: &octos_llm::ChatConfig,
        ) -> eyre::Result<octos_llm::ChatResponse> {
            self.seen
                .lock()
                .await
                .push(messages.iter().map(|m| m.content.clone()).collect());
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                // Announce that the turn is executing, then wait for the test
                // to deliver the cancel and release us.
                self.entered.notify_one();
                self.release.notified().await;
            }
            Ok(octos_llm::ChatResponse {
                content: Some("done".to_string()),
                reasoning_content: None,
                tool_calls: vec![],
                stop_reason: octos_llm::StopReason::EndTurn,
                usage: octos_llm::TokenUsage::default(),
                provider_index: None,
            })
        }
        fn provider_name(&self) -> &str {
            "barrier-mock"
        }
        fn model_id(&self) -> &str {
            "barrier-1"
        }
    }

    /// Factory that hands every session an agent backed by a caller-supplied
    /// `LlmProvider` (so we can inject the `BarrierLlm`). Mirrors
    /// `TestAgentFactory` but takes the provider by `Arc` so the barrier
    /// handles are shared with the test.
    struct InjectedLlmFactory {
        llm: Arc<dyn octos_llm::LlmProvider>,
        memory_dir: PathBuf,
        default_cwd: PathBuf,
    }

    #[async_trait::async_trait]
    impl SessionAgentFactory for InjectedLlmFactory {
        fn default_cwd(&self) -> &std::path::Path {
            &self.default_cwd
        }
        async fn build(&self, cwd: PathBuf) -> Result<(Arc<Agent>, Arc<AtomicBool>)> {
            let memory = Arc::new(EpisodeStore::open(&self.memory_dir).await?);
            let tools = ToolRegistry::with_builtins_and_sandbox(
                &cwd,
                create_sandbox(&octos_agent::SandboxConfig::default()),
            );
            let shutdown = Arc::new(AtomicBool::new(false));
            let agent = Agent::new(
                AgentId::new("acp-cancel-test"),
                self.llm.clone(),
                tools,
                memory,
            )
            .with_shutdown(shutdown.clone());
            Ok((Arc::new(agent), shutdown))
        }
    }

    /// Test transport: expose the octos ACP agent (backed by `factory`) as a
    /// `ConnectTo<Client>` so an in-process client can drive it. The session
    /// map is caller-supplied so tests can observe cancel flags directly.
    struct CancelTestTransport {
        factory: InjectedLlmFactory,
        sessions: SessionMap,
    }

    impl agent_client_protocol::ConnectTo<Client> for CancelTestTransport {
        async fn connect_to(
            self,
            client: impl agent_client_protocol::ConnectTo<AcpAgentRole> + 'static,
        ) -> std::result::Result<(), AcpError> {
            spawn_acp_agent(Arc::new(self.factory), self.sessions, client).await
        }
    }

    /// Wait until `session/cancel` has been DISPATCHED for `sid` (its shutdown
    /// flag is observably set). `send_notification` only queues the frame; on a
    /// loaded runner the barrier release could otherwise win the race against
    /// the dispatch loop, the turn would complete un-cancelled, and the test
    /// would flake to `EndTurn` (seen repeatedly on CI under load).
    async fn wait_for_cancel_dispatch(sessions: &SessionMap, sid: &SessionId) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let flagged = sessions
                .lock()
                .await
                .get(sid)
                .map(|s| s.shutdown.load(Ordering::Acquire))
                .unwrap_or(false);
            if flagged {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "session/cancel was not dispatched within 10s"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    /// A `session/cancel` delivered WHILE the turn is in flight must yield
    /// `StopReason::Cancelled`. With the reset moved onto the dispatch loop
    /// (`handle_prompt`), the cancel that arrives after the prompt is accepted
    /// sets the flag and it survives into the turn, which reports `Cancelled`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn should_report_cancelled_when_session_cancel_arrives_during_turn() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cwd = tmp.path().to_path_buf();
        let memory_dir = tmp.path().join("memory");
        std::fs::create_dir_all(&memory_dir).unwrap();

        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let llm: Arc<dyn octos_llm::LlmProvider> = Arc::new(BarrierLlm {
            calls: std::sync::atomic::AtomicUsize::new(0),
            entered: entered.clone(),
            release: release.clone(),
            seen: Arc::new(Mutex::new(Vec::new())),
        });
        let sessions: SessionMap = Arc::new(Mutex::new(HashMap::new()));
        let sessions_for_canceller = sessions.clone();
        let transport = CancelTestTransport {
            factory: InjectedLlmFactory {
                llm,
                memory_dir,
                default_cwd: cwd.clone(),
            },
            sessions,
        };

        let stop_reason: Arc<Mutex<Option<StopReason>>> = Arc::new(Mutex::new(None));
        let stop_for_main = stop_reason.clone();
        let prompt_cwd = cwd.clone();

        Client
            .builder()
            .name("octos-acp-cancel-client")
            .on_receive_notification(
                async move |_notif: SessionNotification, _cx: ConnectionTo<AcpAgentRole>| Ok(()),
                on_receive_notification!(),
            )
            .connect_with(
                transport,
                |connection: ConnectionTo<AcpAgentRole>| async move {
                    connection
                        .send_request(InitializeRequest::new(
                            agent_client_protocol::schema::ProtocolVersion::V1,
                        ))
                        .block_task()
                        .await?;
                    let new_session = connection
                        .send_request(NewSessionRequest::new(prompt_cwd.clone()))
                        .block_task()
                        .await?;
                    let session_id = new_session.session_id;

                    // Concurrently: once the turn is executing, deliver a cancel,
                    // wait until it has been dispatched (flag observably set),
                    // then release the barrier so the LLM call returns.
                    let cancel_conn = connection.clone();
                    let cancel_sid = session_id.clone();
                    let canceller = tokio::spawn(async move {
                        entered.notified().await;
                        cancel_conn
                            .send_notification(CancelNotification::new(cancel_sid.clone()))
                            .expect("send cancel");
                        wait_for_cancel_dispatch(&sessions_for_canceller, &cancel_sid).await;
                        release.notify_one();
                    });

                    let prompt = connection
                        .send_request(PromptRequest::new(
                            session_id.clone(),
                            vec![ContentBlock::Text(
                                agent_client_protocol::schema::v1::TextContent::new("hello"),
                            )],
                        ))
                        .block_task()
                        .await?;
                    canceller.await.expect("canceller joins");
                    *stop_for_main.lock().await = Some(prompt.stop_reason);
                    Ok(())
                },
            )
            .await
            .expect("ACP client run completes");

        let got = *stop_reason.lock().await;
        assert!(
            matches!(got, Some(StopReason::Cancelled)),
            "cancel during the turn must yield Cancelled, got {got:?}"
        );
    }

    /// After a turn is cancelled, a FRESH prompt on the same session (no cancel)
    /// must return `EndTurn` — proving the stale-flag reset was RELOCATED onto
    /// `handle_prompt` (run before each turn spawns), not simply deleted. If the
    /// reset were gone entirely, the stale `true` from the cancelled turn would
    /// leak and wrongly report `Cancelled` again.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn should_report_end_turn_for_fresh_prompt_after_prior_turn_was_cancelled() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cwd = tmp.path().to_path_buf();
        let memory_dir = tmp.path().join("memory");
        std::fs::create_dir_all(&memory_dir).unwrap();

        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let seen: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));
        let llm: Arc<dyn octos_llm::LlmProvider> = Arc::new(BarrierLlm {
            calls: std::sync::atomic::AtomicUsize::new(0),
            entered: entered.clone(),
            release: release.clone(),
            seen: seen.clone(),
        });
        let sessions: SessionMap = Arc::new(Mutex::new(HashMap::new()));
        let sessions_for_canceller = sessions.clone();
        let transport = CancelTestTransport {
            factory: InjectedLlmFactory {
                llm,
                memory_dir,
                default_cwd: cwd.clone(),
            },
            sessions,
        };

        let second_stop: Arc<Mutex<Option<StopReason>>> = Arc::new(Mutex::new(None));
        let second_for_main = second_stop.clone();
        let prompt_cwd = cwd.clone();

        Client
            .builder()
            .name("octos-acp-cancel-then-fresh-client")
            .on_receive_notification(
                async move |_notif: SessionNotification, _cx: ConnectionTo<AcpAgentRole>| Ok(()),
                on_receive_notification!(),
            )
            .connect_with(
                transport,
                |connection: ConnectionTo<AcpAgentRole>| async move {
                    connection
                        .send_request(InitializeRequest::new(
                            agent_client_protocol::schema::ProtocolVersion::V1,
                        ))
                        .block_task()
                        .await?;
                    let new_session = connection
                        .send_request(NewSessionRequest::new(prompt_cwd.clone()))
                        .block_task()
                        .await?;
                    let session_id = new_session.session_id;

                    // Turn 1: cancel it mid-flight (as in the test above),
                    // holding the barrier until the cancel is dispatched.
                    let cancel_conn = connection.clone();
                    let cancel_sid = session_id.clone();
                    let canceller = tokio::spawn(async move {
                        entered.notified().await;
                        cancel_conn
                            .send_notification(CancelNotification::new(cancel_sid.clone()))
                            .expect("send cancel");
                        wait_for_cancel_dispatch(&sessions_for_canceller, &cancel_sid).await;
                        release.notify_one();
                    });
                    let first = connection
                        .send_request(PromptRequest::new(
                            session_id.clone(),
                            vec![ContentBlock::Text(
                                agent_client_protocol::schema::v1::TextContent::new("first"),
                            )],
                        ))
                        .block_task()
                        .await?;
                    canceller.await.expect("canceller joins");
                    assert!(
                        matches!(first.stop_reason, StopReason::Cancelled),
                        "sanity: turn 1 should be Cancelled, got {:?}",
                        first.stop_reason
                    );

                    // Turn 2: no cancel. The BarrierLlm only blocks its FIRST call,
                    // so this returns immediately. Must be EndTurn — the stale flag
                    // from turn 1 must have been reset before turn 2 spawned.
                    let second = connection
                        .send_request(PromptRequest::new(
                            session_id.clone(),
                            vec![ContentBlock::Text(
                                agent_client_protocol::schema::v1::TextContent::new("second"),
                            )],
                        ))
                        .block_task()
                        .await?;
                    *second_for_main.lock().await = Some(second.stop_reason);
                    Ok(())
                },
            )
            .await
            .expect("ACP client run completes");

        let got = *second_stop.lock().await;
        assert!(
            matches!(got, Some(StopReason::EndTurn)),
            "fresh prompt after a cancelled turn must be EndTurn (reset relocated, \
             not deleted), got {got:?}"
        );

        // codex round-3 regression: the CANCELLED turn's aborted assistant reply
        // ("done") must NOT be persisted into history, so the fresh second turn's
        // chat() must not see it. Without the `!cancelled` guard, the partial
        // reply leaks into the next prompt's context.
        //
        // Select the fresh turn by the "second" user prompt it carries — a
        // cancelled first turn may retry (stream fallback while the shutdown flag
        // is set), so it is NOT necessarily snapshot index 1 (codex round-4).
        let snapshots = seen.lock().await;
        let fresh = snapshots
            .iter()
            .find(|snap| snap.iter().any(|c| c.contains("second")))
            .expect("the fresh second prompt must have reached the LLM");
        assert!(
            !fresh.iter().any(|c| c.contains("done")),
            "the cancelled turn's aborted assistant reply must NOT persist into the \
             fresh prompt's history; fresh-turn messages: {fresh:?}"
        );
    }
}
