//! Gateway command: run as a persistent messaging daemon.

mod account_handler;
#[cfg(feature = "matrix")]
mod matrix_integration;
mod prompt;
mod session_ui;
mod skills_handler;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use clap::Args;
use colored::Colorize;
use eyre::{Result, WrapErr};
use octos_agent::{AgentConfig, HookContext, HookExecutor, SkillsLoader, ToolRegistry};
use octos_bus::{
    ActiveSessionStore, ChannelManager, CliChannel, CronService, HeartbeatService, SessionManager,
    create_bus, validate_topic_name,
};
use octos_core::{MAIN_PROFILE_ID, OutboundMessage, SessionKey};
use octos_llm::{
    AdaptiveConfig, AdaptiveRouter, BaselineEntry, LlmProvider, ProviderChain, ProviderRouter,
    RetryProvider, SwappableProvider,
};
use octos_memory::{EpisodeStore, MemoryStore};
use tokio::sync::{Mutex, RwLock, Semaphore, mpsc};
use tracing::{error, info, warn};

use super::Executable;
use crate::commands::chat::{create_embedder, resolve_provider_policy};
use crate::config::{Config, detect_provider};
use crate::config_watcher::{ConfigChange, ConfigWatcher};
use crate::persona_service::PersonaService;
use crate::session_actor::{
    ActorFactory, ActorRegistry, PendingMessages, PipelineToolFactory, SnapshotToolRegistryFactory,
    ToolRegistryFactory,
};
use crate::status_layers::StatusComposer;

// Re-export for use by prompt module
#[cfg(feature = "matrix")]
use matrix_integration::*;
pub(crate) use prompt::build_system_prompt;
#[cfg(any(
    feature = "telegram",
    feature = "discord",
    feature = "slack",
    feature = "whatsapp",
    feature = "email",
    feature = "feishu",
    feature = "twilio",
    feature = "wecom",
    feature = "wecom-bot",
    feature = "matrix",
    feature = "qq-bot",
    feature = "wechat"
))]
use prompt::settings_str;

/// Provider + model name + optional adaptive router, returned by [`build_llm_stack`].
type LlmStack = (Arc<dyn LlmProvider>, String, Option<Arc<AdaptiveRouter>>);

/// Run as a persistent gateway daemon.
#[derive(Debug, Args)]
pub struct GatewayCommand {
    /// Working directory (defaults to current directory).
    #[arg(short, long)]
    pub cwd: Option<PathBuf>,

    /// Data directory for episodes, memory, sessions (defaults to $OCTOS_HOME or ~/.octos).
    #[arg(long)]
    pub data_dir: Option<PathBuf>,

    /// Path to config file.
    #[arg(long, conflicts_with = "profile")]
    pub config: Option<PathBuf>,

    /// Path to a profile JSON file (used by managed gateways).
    #[arg(long, conflicts_with = "config")]
    pub profile: Option<PathBuf>,

    /// Override WhatsApp bridge URL (used by managed gateways).
    #[arg(long, hide = true)]
    pub bridge_url: Option<String>,

    /// Internal: managed WeChat bridge WebSocket URL.
    #[arg(long, hide = true)]
    pub wechat_bridge_url: Option<String>,

    /// Override Feishu webhook port (used by managed gateways).
    #[arg(long, hide = true)]
    pub feishu_port: Option<u16>,

    /// Override API channel port (used by managed gateways).
    #[arg(long, hide = true)]
    pub api_port: Option<u16>,

    /// LLM provider to use (overrides config).
    #[arg(long)]
    pub provider: Option<String>,

    /// Model to use (overrides config).
    #[arg(long)]
    pub model: Option<String>,

    /// Custom base URL for the API endpoint (overrides config).
    #[arg(long)]
    pub base_url: Option<String>,

    /// Maximum agent iterations per message (default: 50).
    #[arg(long)]
    pub max_iterations: Option<u32>,

    /// Disable automatic retry on transient errors.
    #[arg(long)]
    pub no_retry: bool,

    /// Path to parent profile JSON (sub-accounts inherit provider config).
    #[arg(long, hide = true)]
    pub parent_profile: Option<PathBuf>,

    /// Octos home directory for ProfileStore access (used by managed gateways).
    #[arg(long, hide = true)]
    pub octos_home: Option<PathBuf>,
}

fn resolve_dispatch_profile_id(
    target_profile_id: Option<&str>,
    profile_store: Option<&crate::profiles::ProfileStore>,
) -> Result<Option<String>> {
    let Some(profile_id) = target_profile_id.filter(|value| !value.is_empty()) else {
        return Ok(None);
    };

    let Some(store) = profile_store else {
        warn!(
            profile_id = %profile_id,
            "profile store unavailable; routing target profile to main profile"
        );
        return Ok(None);
    };

    match store.get(profile_id) {
        Ok(Some(_)) => Ok(Some(profile_id.to_string())),
        Ok(None) => {
            warn!(
                profile_id = %profile_id,
                "target profile not found; routing message to main profile"
            );
            Ok(None)
        }
        Err(error) => {
            warn!(
                profile_id = %profile_id,
                %error,
                "failed to load target profile; routing message to main profile"
            );
            Ok(None)
        }
    }
}

fn build_profiled_session_key(
    profile_id: Option<&str>,
    channel: &str,
    chat_id: &str,
    topic: &str,
) -> SessionKey {
    let effective_profile_id = profile_id.unwrap_or(MAIN_PROFILE_ID);
    SessionKey::with_profile_topic(effective_profile_id, channel, chat_id, topic)
}

fn build_llm_stack(config: &Config, no_retry: bool) -> Result<LlmStack> {
    let model = config.model.clone();
    let base_url = config.base_url.clone();
    let provider_name = config
        .provider
        .clone()
        .or_else(|| model.as_deref().and_then(detect_provider).map(String::from))
        .unwrap_or_else(|| "anthropic".to_string());

    use super::chat::create_provider;
    let base_provider = create_provider(&provider_name, config, model, base_url)?;
    let mut adaptive_router_ref: Option<Arc<AdaptiveRouter>> = None;

    let llm: Arc<dyn LlmProvider> = if no_retry {
        base_provider
    } else if config.fallback_models.is_empty() {
        Arc::new(RetryProvider::new(base_provider))
    } else {
        let mut providers: Vec<Arc<dyn LlmProvider>> =
            vec![Arc::new(RetryProvider::new(base_provider))];
        let mut costs: Vec<f64> = vec![0.0]; // primary cost unknown
        for fallback in &config.fallback_models {
            let fallback_config = if fallback.api_key_env.is_some() {
                let mut cloned = config.clone();
                cloned.api_key_env = fallback.api_key_env.clone();
                cloned
            } else {
                config.clone()
            };
            match super::chat::create_provider_with_api_type(
                &fallback.provider,
                &fallback_config,
                fallback.model.clone(),
                fallback.base_url.clone(),
                fallback.api_type.as_deref(),
            ) {
                Ok(provider) => {
                    providers.push(Arc::new(RetryProvider::new(provider)));
                    costs.push(fallback.cost_per_m.unwrap_or(0.0));
                }
                Err(error) => {
                    warn!(
                        provider = %fallback.provider,
                        %error,
                        "skipping profiled fallback provider"
                    );
                }
            }
        }

        if providers.len() > 1 {
            let adaptive_config = config
                .adaptive_routing
                .as_ref()
                .map(AdaptiveConfig::from)
                .unwrap_or_default();
            let routing_config = config.adaptive_routing.as_ref();
            let mode = routing_config
                .map(|value| value.mode.into())
                .unwrap_or(octos_llm::AdaptiveMode::Lane);
            let qos_ranking = routing_config
                .map(|value| value.qos_ranking)
                .unwrap_or(true);
            let router = Arc::new(
                AdaptiveRouter::new(providers, &costs, adaptive_config)
                    .with_adaptive_config(mode, qos_ranking),
            );
            adaptive_router_ref = Some(router.clone());
            router
        } else {
            Arc::new(ProviderChain::new(providers))
        }
    };

    Ok((llm, provider_name, adaptive_router_ref))
}

struct ProfileActorFactoryBuilder {
    profile_store: Arc<crate::profiles::ProfileStore>,
    project_dir: PathBuf,
    tool_config: Arc<octos_agent::ToolConfigStore>,
    memory: Arc<EpisodeStore>,
    memory_store: Arc<MemoryStore>,
    agent_config: AgentConfig,
    session_mgr: Arc<Mutex<SessionManager>>,
    out_tx: mpsc::Sender<OutboundMessage>,
    spawn_inbound_tx: mpsc::Sender<octos_core::InboundMessage>,
    cron_service: Arc<CronService>,
    tool_registry_factory: Arc<dyn ToolRegistryFactory + Send + Sync>,
    pipeline_factory: Option<Arc<dyn PipelineToolFactory + Send + Sync>>,
    max_history: Arc<AtomicUsize>,
    session_timeout_secs: u64,
    shutdown: Arc<AtomicBool>,
    cwd: PathBuf,
    provider_policy: Option<octos_agent::ToolPolicy>,
    worker_prompt: Option<String>,
    provider_router: Option<Arc<ProviderRouter>>,
    active_sessions: Arc<RwLock<ActiveSessionStore>>,
    pending_messages: PendingMessages,
    queue_mode: crate::config::QueueMode,
    plugin_prompt_fragments: Vec<String>,
    no_retry: bool,
    /// Sandbox config for child bot tool registries.
    sandbox_config: octos_agent::SandboxConfig,
}

impl ProfileActorFactoryBuilder {
    async fn build(&self, profile_id: &str) -> Result<ActorFactory> {
        let profile = self
            .profile_store
            .get(profile_id)?
            .ok_or_else(|| eyre::eyre!("target profile '{profile_id}' not found"))?;
        let effective_profile =
            crate::profiles::resolve_effective_profile(&self.profile_store, &profile)?;
        let profile_config = crate::profiles::config_from_profile(&effective_profile, None, None);
        let (llm, provider_name, adaptive_router) =
            build_llm_stack(&profile_config, self.no_retry)?;
        let llm_for_compaction = llm.clone();
        let model_id = llm.model_id().to_string();

        let profile_data_dir = self.profile_store.resolve_data_dir(&effective_profile);
        let mut extra_skills_dirs: Vec<PathBuf> = Vec::new();
        if profile_data_dir != self.project_dir {
            if let Some(parent_id) = effective_profile.parent_id.as_deref() {
                if let Some(parent) = self.profile_store.get(parent_id)? {
                    extra_skills_dirs.push(self.profile_store.resolve_data_dir(&parent));
                }
            }
            extra_skills_dirs.push(self.project_dir.clone());
        }

        let mut skills_loader = if profile_data_dir != self.project_dir {
            let mut loader = SkillsLoader::new(&profile_data_dir);
            for dir in &extra_skills_dirs {
                loader.add_skills_dir(dir);
            }
            loader
        } else {
            SkillsLoader::new(&self.project_dir)
        };
        skills_loader.add_skills_path(
            self.project_dir
                .join(octos_agent::bootstrap::BUNDLED_APP_SKILLS_DIR),
        );

        let mut child_plugin_prompt_fragments = Vec::new();
        let mut child_plugin_hooks: Vec<octos_agent::HookConfig> = Vec::new();

        let mut system_prompt = if effective_profile.config.admin_mode {
            if let Some(custom_prompt) = effective_profile.config.gateway.system_prompt.as_deref() {
                custom_prompt.to_string()
            } else {
                let compiled = include_str!("../../prompts/admin_default.txt");
                super::load_prompt("admin", compiled)
            }
        } else {
            build_system_prompt(
                effective_profile.config.gateway.system_prompt.as_deref(),
                &profile_data_dir,
                &self.project_dir,
                &self.memory_store,
                &skills_loader,
                &self.tool_config,
            )
            .await
        };
        for fragment in &self.plugin_prompt_fragments {
            system_prompt.push_str("\n\n");
            system_prompt.push_str(fragment);
        }
        let mut pipeline_factory = self.pipeline_factory.clone();
        let mut provider_policy = self.provider_policy.clone();
        let mut worker_prompt = self.worker_prompt.clone();
        let mut provider_router = self.provider_router.clone();

        // Child bots with admin_mode=false need a full normal-mode tool registry
        // (web_search, web_fetch, browser, plugins, etc.) instead of the parent's
        // admin-only snapshot.
        let tool_registry_factory: Arc<dyn ToolRegistryFactory + Send + Sync> = if effective_profile
            .config
            .admin_mode
        {
            self.tool_registry_factory.clone()
        } else {
            let mut sandbox_config = self.sandbox_config.clone();
            if sandbox_config.read_allow_paths.is_empty() {
                sandbox_config
                    .read_allow_paths
                    .push(self.project_dir.to_string_lossy().into_owned());
            }
            let sandbox = octos_agent::create_sandbox(&sandbox_config);
            let mut tools = ToolRegistry::with_builtins_and_sandbox(&profile_data_dir, sandbox);
            tools.inject_tool_config(self.tool_config.clone());
            if let Some(secs) = effective_profile.config.gateway.browser_timeout_secs {
                tools.register(
                    octos_agent::BrowserTool::with_timeout(std::time::Duration::from_secs(secs))
                        .with_config(self.tool_config.clone()),
                );
            }

            if !profile_config.mcp_servers.is_empty() {
                match octos_agent::McpClient::start(&profile_config.mcp_servers).await {
                    Ok(client) => client.register_tools(&mut tools),
                    Err(e) => warn!(profile_id, "child bot MCP initialization failed: {e}"),
                }
            }

            // Load plugins
            let plugin_work_dir = profile_data_dir.join("skill-output");
            let plugin_env = build_plugin_env(&profile_config, &provider_name);
            let mut plugin_dirs =
                crate::config::Config::plugin_dirs_from_project(&self.project_dir);
            let profile_skills = profile_data_dir.join("skills");
            if profile_skills.exists() && !plugin_dirs.contains(&profile_skills) {
                plugin_dirs.insert(0, profile_skills);
            }
            // Include parent profile skills dir so child bots can use parent's skills
            for dir in &extra_skills_dirs {
                let skills = dir.join("skills");
                if skills.exists() && !plugin_dirs.contains(&skills) {
                    plugin_dirs.push(skills);
                }
            }
            if !plugin_dirs.is_empty() {
                match octos_agent::PluginLoader::load_into_with_work_dir(
                    &mut tools,
                    &plugin_dirs,
                    &plugin_env,
                    Some(&plugin_work_dir),
                ) {
                    Ok(result) => {
                        child_plugin_prompt_fragments = result.prompt_fragments;
                        child_plugin_hooks = result.hooks;
                        if !result.mcp_servers.is_empty() {
                            match octos_agent::McpClient::start(&result.mcp_servers).await {
                                Ok(client) => client.register_tools(&mut tools),
                                Err(e) => warn!(
                                    profile_id,
                                    "child bot skill MCP initialization failed: {e}"
                                ),
                            }
                        }
                    }
                    Err(e) => warn!(profile_id, "child bot plugin loading failed: {e}"),
                }
            }

            tools.register(octos_agent::DeepSearchTool::new(
                profile_data_dir.join("research"),
            ));
            tools.register(octos_agent::SynthesizeResearchTool::new(
                llm.clone(),
                profile_data_dir.clone(),
            ));
            tools.register(octos_agent::ManageSkillsTool::new(
                profile_data_dir.join("skills"),
            ));
            tools.register(octos_agent::RecallMemoryTool::new(
                self.memory_store.clone(),
            ));
            tools.register(octos_agent::SaveMemoryTool::new(self.memory_store.clone()));
            if let Some(ref policy) = profile_config.tool_policy {
                tools.apply_policy(policy);
            }
            if !profile_config.context_filter.is_empty() {
                tools.set_context_filter(profile_config.context_filter.clone());
            }
            if let Some(policy) =
                resolve_provider_policy(&profile_config, &provider_name, &model_id)
            {
                tools.set_provider_policy(policy);
            }
            worker_prompt = Some(super::load_prompt(
                "worker",
                octos_agent::DEFAULT_WORKER_PROMPT,
            ));
            provider_policy = tools.provider_policy().cloned();

            let child_router = if self.provider_router.is_some() {
                self.provider_router.clone()
            } else if profile_config.fallback_models.is_empty() {
                None
            } else {
                let router = Arc::new(ProviderRouter::new());
                router.register_with_full_meta(
                    &model_id,
                    llm.clone(),
                    Some("Primary model".into()),
                    None,
                    None,
                );
                let mut key_counts: std::collections::HashMap<String, usize> =
                    std::collections::HashMap::new();
                let mut registered = 1usize;
                for fb in &profile_config.fallback_models {
                    let fb_config = {
                        let mut c = profile_config.clone();
                        if fb.api_key_env.is_some() {
                            c.api_key_env = fb.api_key_env.clone();
                        } else if fb.provider != profile_config.provider.as_deref().unwrap_or("") {
                            c.api_key_env = None;
                        }
                        c
                    };
                    match super::chat::create_provider_with_api_type(
                        &fb.provider,
                        &fb_config,
                        fb.model.clone(),
                        fb.base_url.clone(),
                        fb.api_type.as_deref(),
                    ) {
                        Ok(p) => {
                            let base_key = fb.model.as_deref().unwrap_or(&fb.provider).to_string();
                            let count = key_counts.entry(base_key.clone()).or_insert(0);
                            let key = if *count == 0 {
                                base_key.clone()
                            } else {
                                format!("{base_key}-{count}")
                            };
                            *count += 1;
                            router.register_with_full_meta(
                                &key,
                                Arc::new(RetryProvider::new(p)),
                                None,
                                None,
                                None,
                            );
                            registered += 1;
                        }
                        Err(e) => warn!(
                            profile_id,
                            provider = %fb.provider,
                            error = %e,
                            "skipping child bot fallback as sub-provider"
                        ),
                    }
                }
                if registered > 1 { Some(router) } else { None }
            };
            provider_router = child_router.clone();

            tools.set_base_tools([
                "run_pipeline",
                "deep_search",
                "deep_crawl",
                "web_search",
                "web_fetch",
                "read_file",
                "write_file",
                "edit_file",
                "shell",
                "list_dir",
                "glob",
                "grep",
                "message",
                "send_file",
                "activate_tools",
            ]);
            let visible = tools.specs().len();
            if visible > 15 {
                for group in &[
                    "group:memory",
                    "group:admin",
                    "group:sessions",
                    "group:web",
                    "group:runtime",
                ] {
                    tools.defer_group(group);
                }
            }
            if tools.has_deferred() {
                tools.register(octos_agent::ActivateToolsTool::new());
            }

            struct ChildPipelineToolFactory {
                llm: Arc<dyn LlmProvider>,
                memory: Arc<octos_memory::EpisodeStore>,
                data_dir: PathBuf,
                policy: Option<octos_agent::ToolPolicy>,
                plugin_dirs: Vec<PathBuf>,
                router: Option<Arc<ProviderRouter>>,
                octos_home: PathBuf,
            }

            impl crate::session_actor::PipelineToolFactory for ChildPipelineToolFactory {
                fn create(&self) -> Arc<dyn octos_agent::Tool> {
                    let mut pt = octos_pipeline::RunPipelineTool::new(
                        self.llm.clone(),
                        self.memory.clone(),
                        self.data_dir.clone(),
                        self.data_dir.clone(),
                    )
                    .with_provider_policy(self.policy.clone())
                    .with_plugin_dirs(self.plugin_dirs.clone())
                    .with_octos_home(self.octos_home.clone());
                    if let Some(ref router) = self.router {
                        pt = pt.with_provider_router(router.clone());
                    }
                    Arc::new(pt)
                }
            }

            pipeline_factory = Some(Arc::new(ChildPipelineToolFactory {
                llm: llm.clone(),
                memory: self.memory.clone(),
                data_dir: profile_data_dir.clone(),
                policy: provider_policy.clone(),
                plugin_dirs: plugin_dirs.clone(),
                router: provider_router.clone(),
                octos_home: self.project_dir.clone(),
            })
                as Arc<dyn crate::session_actor::PipelineToolFactory + Send + Sync>);

            Arc::new(SnapshotToolRegistryFactory::new(tools))
        };

        if !child_plugin_prompt_fragments.is_empty() {
            for fragment in &child_plugin_prompt_fragments {
                system_prompt.push_str("\n\n");
                system_prompt.push_str(fragment);
            }
        }

        let mut all_hooks = effective_profile.config.hooks.clone();
        all_hooks.extend(child_plugin_hooks);
        let hooks = if all_hooks.is_empty() {
            None
        } else {
            Some(Arc::new(HookExecutor::new(all_hooks)))
        };

        Ok(ActorFactory {
            agent_config: self.agent_config.clone(),
            llm: llm.clone(),
            llm_for_compaction,
            memory: self.memory.clone(),
            system_prompt: Arc::new(std::sync::RwLock::new(system_prompt)),
            hooks,
            hook_context_template: Some(HookContext {
                session_id: None,
                profile_id: Some(profile_id.to_string()),
            }),
            data_dir: profile_data_dir,
            session_mgr: self.session_mgr.clone(),
            out_tx: self.out_tx.clone(),
            spawn_inbound_tx: self.spawn_inbound_tx.clone(),
            cron_service: Some(self.cron_service.clone()),
            tool_registry_factory,
            pipeline_factory,
            max_history: self.max_history.clone(),
            idle_timeout: Duration::from_secs(crate::session_actor::DEFAULT_IDLE_TIMEOUT_SECS),
            session_timeout: Duration::from_secs(self.session_timeout_secs),
            shutdown: self.shutdown.clone(),
            cwd: self.cwd.clone(),
            sandbox_config: effective_profile.config.sandbox.clone(),
            provider_policy,
            worker_prompt,
            provider_router,
            embedder: create_embedder(&profile_config)
                .map(|embedder| embedder as Arc<dyn octos_llm::EmbeddingProvider>),
            active_sessions: self.active_sessions.clone(),
            pending_messages: self.pending_messages.clone(),
            queue_mode: self.queue_mode,
            adaptive_router,
            memory_store: Some(self.memory_store.clone()),
        })
    }
}

impl Executable for GatewayCommand {
    fn execute(self) -> Result<()> {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_stack_size(8 * 1024 * 1024) // 8MB stack for deep agent futures
            .build()
            .wrap_err("failed to create tokio runtime")?
            .block_on(self.run_async())
    }
}

impl GatewayCommand {
    async fn run_async(self) -> Result<()> {
        // Use eprintln! for the startup banner so it reaches the server's stderr
        // reader immediately (stderr is unbuffered, unlike piped stdout).
        eprintln!("[gateway] starting");
        println!("{}", "octos gateway".cyan().bold());
        println!();

        let cwd = match self.cwd {
            Some(p) => p,
            None => std::env::current_dir().wrap_err("failed to get current directory")?,
        };

        let mut profile_id: Option<String> = None;
        eprintln!(
            "[gateway] loading config (profile={:?})",
            self.profile.as_deref().map(|p| p.display().to_string())
        );
        let mut admin_mode = false;
        let config = if let Some(ref profile_path) = self.profile {
            // Load config from profile JSON (single source of truth)
            let content = std::fs::read_to_string(profile_path)
                .wrap_err_with(|| format!("failed to read profile: {}", profile_path.display()))?;
            let mut profile: crate::profiles::UserProfile = serde_json::from_str(&content)
                .wrap_err_with(|| format!("failed to parse profile: {}", profile_path.display()))?;
            profile_id = Some(profile.id.clone());
            admin_mode = profile.config.admin_mode;

            // Sub-account: merge LLM provider config from parent profile
            if let Some(ref parent_path) = self.parent_profile {
                if let Ok(parent_content) = std::fs::read_to_string(parent_path) {
                    if let Ok(parent) =
                        serde_json::from_str::<crate::profiles::UserProfile>(&parent_content)
                    {
                        info!(
                            parent = %parent.id,
                            sub_account = %profile.id,
                            "inheriting provider config from parent profile"
                        );
                        profile.config.provider = parent.config.provider;
                        profile.config.model = parent.config.model;
                        profile.config.base_url = parent.config.base_url;
                        profile.config.api_key_env = parent.config.api_key_env;
                        profile.config.api_type = parent.config.api_type;
                        profile.config.fallback_models = parent.config.fallback_models;
                        if profile.config.email.is_none() {
                            profile.config.email = parent.config.email;
                        }
                    }
                }
            }

            crate::profiles::config_from_profile(
                &profile,
                self.bridge_url.as_deref(),
                self.feishu_port,
            )
        } else if let Some(config_path) = &self.config {
            Config::from_file(config_path)?
        } else {
            Config::load(&cwd)?
        };

        let model = self.model.or(config.model.clone());
        let base_url = self.base_url.or(config.base_url.clone());
        let provider_name = self
            .provider
            .or(config.provider.clone())
            .or_else(|| model.as_deref().and_then(detect_provider).map(String::from))
            .unwrap_or_else(|| "anthropic".to_string());

        let gw_config = config
            .gateway
            .clone()
            .unwrap_or_else(|| crate::config::GatewayConfig {
                channels: vec![crate::config::ChannelEntry {
                    channel_type: "cli".into(),
                    allowed_senders: vec![],
                    settings: serde_json::json!({}),
                }],
                max_history: 50,
                ..Default::default()
            });

        eprintln!("[gateway] provider={provider_name}");
        println!("{}: {}", "Provider".green(), provider_name);

        // Create LLM provider (reuses the shared create_provider from chat.rs)
        use super::chat::create_provider;
        let base_provider = create_provider(&provider_name, &config, model, base_url)?;
        eprintln!(
            "[gateway] LLM provider created, model={}",
            base_provider.model_id()
        );

        let model_id = base_provider.model_id().to_string();

        // Build provider chain, keeping a typed reference to AdaptiveRouter
        // (if created) for responsiveness feedback from session actors.
        let mut adaptive_router_ref: Option<Arc<AdaptiveRouter>> = None;

        let llm: Arc<dyn LlmProvider> = if self.no_retry {
            base_provider
        } else if config.fallback_models.is_empty() {
            Arc::new(RetryProvider::new(base_provider))
        } else {
            let mut providers: Vec<Arc<dyn LlmProvider>> =
                vec![Arc::new(RetryProvider::new(base_provider))];
            let mut costs: Vec<f64> = vec![0.0]; // primary cost unknown
            for fb in &config.fallback_models {
                let fb_config = if fb.api_key_env.is_some() {
                    let mut c = config.clone();
                    c.api_key_env = fb.api_key_env.clone();
                    c
                } else {
                    config.clone()
                };
                match super::chat::create_provider_with_api_type(
                    &fb.provider,
                    &fb_config,
                    fb.model.clone(),
                    fb.base_url.clone(),
                    fb.api_type.as_deref(),
                ) {
                    Ok(p) => {
                        providers.push(Arc::new(RetryProvider::new(p)));
                        costs.push(fb.cost_per_m.unwrap_or(0.0));
                    }
                    Err(e) => {
                        warn!(provider = %fb.provider, error = %e, "skipping fallback provider");
                    }
                }
            }
            // Auto-enable adaptive routing when multiple providers exist
            if providers.len() > 1 {
                let adaptive_config = config
                    .adaptive_routing
                    .as_ref()
                    .map(AdaptiveConfig::from)
                    .unwrap_or_default();
                let ar_config = config.adaptive_routing.as_ref();
                info!("adaptive routing enabled ({} providers)", providers.len());
                let mode = ar_config
                    .map(|c| c.mode.into())
                    .unwrap_or(octos_llm::AdaptiveMode::Lane);
                let qos = ar_config.map(|c| c.qos_ranking).unwrap_or(true);
                let router = Arc::new(
                    AdaptiveRouter::new(providers, &costs, adaptive_config)
                        .with_adaptive_config(mode, qos),
                );
                adaptive_router_ref = Some(router.clone());
                router
            } else {
                Arc::new(ProviderChain::new(providers))
            }
        };

        // Wrap LLM in SwappableProvider for runtime model switching
        let swappable = Arc::new(SwappableProvider::new(llm));
        let llm: Arc<dyn LlmProvider> = swappable.clone();

        // Resolve data directory (--data-dir > $OCTOS_HOME > ~/.octos)
        let data_dir = super::resolve_data_dir(self.data_dir)?;

        // Seed adaptive router with baseline benchmark data (if available)
        if let Some(ref router) = adaptive_router_ref {
            // Look in data_dir first, then fall back to ~/.octos/ (shared across profiles)
            let baseline_candidates = [
                data_dir.join("provider_baseline.json"),
                dirs::home_dir()
                    .unwrap_or_default()
                    .join(".octos/provider_baseline.json"),
            ];
            let mut baseline_loaded = false;
            for baseline_path in &baseline_candidates {
                if let Ok(json) = std::fs::read_to_string(baseline_path) {
                    match serde_json::from_str::<Vec<BaselineEntry>>(&json) {
                        Ok(entries) => {
                            router.seed_baseline(&entries);
                            info!(
                                path = %baseline_path.display(),
                                entries = entries.len(),
                                "loaded provider baseline"
                            );
                            baseline_loaded = true;
                            break;
                        }
                        Err(e) => {
                            warn!(error = %e, path = %baseline_path.display(), "failed to parse provider_baseline.json")
                        }
                    }
                }
            }
            if !baseline_loaded {
                info!("no provider_baseline.json found, using cold-start scoring");
            }

            // Seed static catalog fields (type, cost, ds_output) from model_catalog.json
            // Look in data_dir first, then fall back to ~/.octos/ (shared across profiles)
            let catalog_candidates = [
                data_dir.join("model_catalog.json"),
                dirs::home_dir()
                    .unwrap_or_default()
                    .join(".octos/model_catalog.json"),
            ];
            for catalog_path in &catalog_candidates {
                if let Ok(json) = std::fs::read_to_string(catalog_path) {
                    if let Ok(catalog) = serde_json::from_str::<octos_llm::QosCatalog>(&json) {
                        router.seed_catalog(&catalog.models);
                        // Seed the global runtime catalog for context.rs lookups
                        let ctx_entries: Vec<(String, u64, u64)> = catalog
                            .models
                            .iter()
                            .map(|m| (m.provider.clone(), m.context_window, m.max_output))
                            .collect();
                        octos_llm::context::seed_from_catalog(&ctx_entries);
                        // Seed pricing catalog
                        let price_entries: Vec<(String, f64, f64)> = catalog
                            .models
                            .iter()
                            .map(|m| (m.provider.clone(), m.cost_in, m.cost_out))
                            .collect();
                        octos_llm::pricing::seed_pricing_catalog(&price_entries);
                        info!(
                            path = %catalog_path.display(),
                            models = catalog.models.len(),
                            "loaded model catalog"
                        );
                        break;
                    }
                }
            }
        }

        // Expose data_dir to skill binaries (e.g. mofa-fm voice storage)
        // SAFETY: called before spawning any threads; single-threaded at this point
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("OCTOS_DATA_DIR", &data_dir);
        }

        // Open ProfileStore for /account commands and bot management.
        // Derive octos_home from: --octos-home flag > data_dir (which already
        // resolves --data-dir > $OCTOS_HOME > ~/.octos).
        let effective_octos_home = self.octos_home.clone().unwrap_or_else(|| data_dir.clone());
        let profile_store: Option<Arc<crate::profiles::ProfileStore>> =
            crate::profiles::ProfileStore::open(&effective_octos_home)
                .ok()
                .map(Arc::new);

        // Export OCTOS_HOME and OCTOS_PROFILE_ID so plugin tools (e.g. account-manager)
        // can access the profile store and know which profile is running.
        // SAFETY: gateway is single-threaded at this point (before tokio tasks spawn).
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("OCTOS_HOME", &effective_octos_home);
            if let Some(ref pid) = profile_id {
                std::env::set_var("OCTOS_PROFILE_ID", pid);
            }
        }

        // Spawn periodic metrics exporter (writes model_catalog.json every 30s)
        if let Some(ref router) = adaptive_router_ref {
            let metrics_router = router.clone();
            let catalog_path = data_dir.join("model_catalog.json");
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
                loop {
                    interval.tick().await;
                    if let Ok(json) =
                        serde_json::to_string_pretty(&metrics_router.export_model_catalog())
                    {
                        let _ = tokio::fs::write(&catalog_path, &json).await;
                    }
                }
            });
        }

        let media_dir = data_dir.join("media");
        let _ = &media_dir; // used by channel feature gates below

        let voice_config = config.voice.clone();

        eprintln!("[gateway] opening episode store at {}", data_dir.display());
        let memory = Arc::new(
            EpisodeStore::open(&data_dir)
                .await
                .wrap_err("failed to open episode store")?,
        );
        eprintln!("[gateway] episode store opened");

        // Initialize memory store
        eprintln!("[gateway] opening memory store");
        let memory_store = Arc::new(
            MemoryStore::open(&data_dir)
                .await
                .wrap_err("failed to open memory store")?,
        );
        eprintln!("[gateway] memory store opened");

        // Derive project_dir from octos_home (when launched by process_manager)
        // or fall back to cwd/.octos (standalone octos gateway / octos chat mode).
        // This is decoupled from cwd so that narrowing cwd to data_dir for
        // per-profile file isolation doesn't break access to shared skills/configs.
        let project_dir = if let Some(ref octos_home) = self.octos_home {
            octos_home.clone()
        } else {
            cwd.join(".octos")
        };

        // Bootstrap bundled app-skills and platform skills into layered dirs
        let n = octos_agent::bootstrap::bootstrap_bundled_skills(&project_dir);
        if n > 0 {
            info!(count = n, "bootstrapped bundled app-skills");
        }
        let n = octos_agent::bootstrap::bootstrap_platform_skills(&project_dir);
        if n > 0 {
            info!(count = n, "bootstrapped platform skills");
        }

        // Voice transcription via voice platform skill binary (after bootstrap)
        let voice_binary_path = project_dir
            .join(octos_agent::bootstrap::PLATFORM_SKILLS_DIR)
            .join("voice")
            .join("main");
        let ominix_url = std::env::var("OMINIX_API_URL").ok().or_else(|| {
            let home = std::env::var_os("HOME")?;
            let discovery = std::path::Path::new(&home).join(".ominix").join("api_url");
            std::fs::read_to_string(discovery)
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        });
        let asr_binary = if let Some(url) = ominix_url.filter(|_| voice_binary_path.exists()) {
            println!("{}: voice platform skill ({})", "Transcriber".green(), url);
            println!("{}: {} ({})", "Voice".green(), "enabled".green(), url);
            // Export so the voice binary can find the server
            #[allow(unsafe_code)]
            unsafe {
                std::env::set_var("OMINIX_API_URL", &url);
            }
            Some(voice_binary_path)
        } else {
            None
        };
        let asr_language = voice_config.as_ref().and_then(|vc| vc.asr_language.clone());

        // Collect extra skills dirs: parent profile (for sub-accounts) + global
        let mut extra_skills_dirs: Vec<PathBuf> = Vec::new();
        if data_dir != project_dir {
            // Sub-account: also add parent profile's skills dir
            if let Some(ref parent_path) = self.parent_profile {
                if let Ok(parent_content) = std::fs::read_to_string(parent_path) {
                    if let Ok(parent) =
                        serde_json::from_str::<crate::profiles::UserProfile>(&parent_content)
                    {
                        if let Some(ref store) = profile_store {
                            extra_skills_dirs.push(store.resolve_data_dir(&parent));
                        }
                    }
                }
            }
            extra_skills_dirs.push(project_dir.clone());
        }

        // Skills priority (highest first):
        //   1. Profile skills (data_dir/skills or sub-account/skills)
        //   2. Parent profile skills (if sub-account)
        //   3. Global profile skills (project_dir/skills)
        //   4. Bundled app-skills (project_dir/bundled-app-skills)
        // Note: platform skills (voice, etc.) are admin-only — loaded in serve.rs
        let skills_loader = if data_dir != project_dir {
            let mut loader = SkillsLoader::new(&data_dir);
            for dir in &extra_skills_dirs {
                loader.add_skills_dir(dir);
            }
            loader
        } else {
            SkillsLoader::new(&project_dir)
        };
        // Add shared layered dirs (lower priority than profile skills)
        let mut skills_loader = skills_loader;
        skills_loader
            .add_skills_path(project_dir.join(octos_agent::bootstrap::BUNDLED_APP_SKILLS_DIR));
        // Extra skills dirs from OCTOS_SKILLS_PATH env var
        if let Ok(extra) = std::env::var("OCTOS_SKILLS_PATH") {
            for p in extra.split(':') {
                let p = p.trim();
                if !p.is_empty() {
                    skills_loader.add_skills_path(p);
                }
            }
        }

        // Create message bus (before publisher is consumed by channel manager)
        let (mut agent_handle, publisher) = create_bus();

        // Clone senders before publisher is consumed
        let cron_inbound_tx = publisher.inbound_sender();
        let heartbeat_inbound_tx = publisher.inbound_sender();
        let spawn_inbound_tx = publisher.inbound_sender();
        let out_tx = agent_handle.outbound_sender();

        // Initialize cron service
        let cron_service = Arc::new(CronService::new(
            data_dir.join("cron.json"),
            cron_inbound_tx,
        ));
        cron_service.start();

        // Initialize heartbeat service
        let heartbeat_service = Arc::new(HeartbeatService::new(
            &cwd,
            heartbeat_inbound_tx,
            octos_bus::heartbeat::DEFAULT_INTERVAL_SECS,
        ));
        heartbeat_service.start();

        // Build tool registry — admin mode gets only admin API tools + messaging
        let tool_config = Arc::new(
            octos_agent::ToolConfigStore::open(&data_dir)
                .await
                .wrap_err("failed to open tool config store")?,
        );

        // Session-specific tools (message, send_file, spawn, cron, pipeline)
        // are NOT registered in the base registry — they are created per-session
        // by the ActorFactory to eliminate the set_context() race condition.

        // Store config needed for per-session tool creation
        let mut provider_policy_for_factory: Option<octos_agent::ToolPolicy> = None;
        let mut worker_prompt_for_factory: Option<String> = None;
        let mut provider_router_for_factory: Option<Arc<ProviderRouter>> = None;
        let mut pipeline_factory: Option<
            Arc<dyn crate::session_actor::PipelineToolFactory + Send + Sync>,
        > = None;

        // Build env vars to inject into plugin processes so skills can route
        // API calls through the configured provider/gateway (e.g. r9s.ai).
        let mut plugin_env = build_plugin_env(&config, &provider_name);
        // Per-profile data_dir so skills (voice profiles, mofa-fm voices, etc.)
        // resolve storage relative to the correct profile, not the gateway root.
        plugin_env.push(("OCTOS_DATA_DIR".to_string(), data_dir.to_string_lossy().to_string()));
        plugin_env.push(("OCTOS_VOICE_DIR".to_string(), data_dir.join("voice_profiles").to_string_lossy().to_string()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();
        #[cfg(feature = "matrix")]
        let mut matrix_channel: Option<Arc<octos_bus::MatrixChannel>> = None;

        let mut tools;
        let mut plugin_result;
        let mut sandbox_config = config.sandbox.clone();
        if admin_mode {
            // Admin mode: register only admin API tools
            tools = ToolRegistry::new();

            // Register admin API tools (calls REST API on octos serve)
            let serve_url_env = std::env::var("OCTOS_SERVE_URL").ok();
            let serve_url = serve_url_env
                .clone()
                .unwrap_or_else(|| "http://127.0.0.1:8080".to_string());
            let admin_token = std::env::var("OCTOS_ADMIN_TOKEN").unwrap_or_default();
            let admin_ctx = Arc::new(octos_agent::AdminApiContext {
                http: reqwest::Client::new(),
                serve_url,
                admin_token,
            });

            octos_agent::register_admin_api_tools(&mut tools, admin_ctx);

            // Session-specific tools (cron, message, send_file) are created
            // per-session by the ActorFactory — not registered in base registry.

            // Shell tool for direct server access (diagnostics, troubleshooting)
            tools.register(octos_agent::ShellTool::new(&cwd));

            // Memory bank tools
            tools.register(octos_agent::RecallMemoryTool::new(memory_store.clone()));
            tools.register(octos_agent::SaveMemoryTool::new(memory_store.clone()));

            // Load only admin-relevant plugins (not all bundled skills)
            let admin_skills: &[&str] = &["send-email", "account-manager"];
            let bundled_dir = project_dir.join(octos_agent::bootstrap::BUNDLED_APP_SKILLS_DIR);
            for skill_name in admin_skills {
                let skill_dir = bundled_dir.join(skill_name);
                if skill_dir.exists() {
                    match octos_agent::PluginLoader::load_plugin(&skill_dir, &plugin_env) {
                        Ok((plugin_tools, _extras)) => {
                            for t in plugin_tools {
                                tools.register(t);
                            }
                        }
                        Err(e) => warn!("admin plugin {skill_name} failed: {e}"),
                    }
                }
            }

            plugin_result = octos_agent::PluginLoadResult::default();
            info!("admin mode: registered admin API + shell + memory + plugin tools");
        } else {
            // Normal mode: full tool registration
            // Populate read_allow_paths so the shell sandbox restricts reads to
            // this profile's data_dir (via cwd) + shared octos home (project_dir).
            // Without this, macOS SBPL defaults to (allow file-read*) which lets
            // the shell read any file on disk, including other profiles' data.
            if sandbox_config.read_allow_paths.is_empty() {
                sandbox_config
                    .read_allow_paths
                    .push(project_dir.to_string_lossy().into_owned());
            }
            let sandbox = octos_agent::create_sandbox(&sandbox_config);
            tools = ToolRegistry::with_builtins_and_sandbox(&cwd, sandbox);
            tools.inject_tool_config(tool_config.clone());

            // Override browser tool with configured timeout (replaces default 300s)
            if let Some(secs) = gw_config.browser_timeout_secs {
                tools.register(
                    octos_agent::BrowserTool::with_timeout(std::time::Duration::from_secs(secs))
                        .with_config(tool_config.clone()),
                );
            }

            // Register MCP tools
            if !config.mcp_servers.is_empty() {
                match octos_agent::McpClient::start(&config.mcp_servers).await {
                    Ok(client) => client.register_tools(&mut tools),
                    Err(e) => warn!("MCP initialization failed: {e}"),
                }
            }

            // Load plugins with a dedicated work directory for output files
            let plugin_work_dir = data_dir.join("skill-output");
            let mut plugin_dirs = crate::config::Config::plugin_dirs_from_project(&project_dir);
            // Prepend per-profile skills dir (highest priority)
            let profile_skills = data_dir.join("skills");
            if profile_skills.exists() && !plugin_dirs.contains(&profile_skills) {
                plugin_dirs.insert(0, profile_skills);
            }
            // Sub-account: also add parent profile's skills dir
            for dir in &extra_skills_dirs {
                let parent_skills = dir.join("skills");
                if parent_skills.exists() && !plugin_dirs.contains(&parent_skills) {
                    plugin_dirs.push(parent_skills);
                }
            }
            plugin_result = octos_agent::PluginLoadResult::default();
            if !plugin_dirs.is_empty() {
                match octos_agent::PluginLoader::load_into_with_work_dir(
                    &mut tools,
                    &plugin_dirs,
                    &plugin_env,
                    Some(&plugin_work_dir),
                ) {
                    Ok(result) => plugin_result = result,
                    Err(e) => warn!("plugin loading failed: {e}"),
                }
            }

            // Start MCP servers declared in skill manifests
            if !plugin_result.mcp_servers.is_empty() {
                match octos_agent::McpClient::start(&plugin_result.mcp_servers).await {
                    Ok(client) => client.register_tools(&mut tools),
                    Err(e) => warn!("skill MCP initialization failed: {e}"),
                }
            }

            // Apply tool policy from config
            if let Some(ref policy) = config.tool_policy {
                tools.apply_policy(policy);
            }

            // Apply context-based tag filter
            if !config.context_filter.is_empty() {
                tools.set_context_filter(config.context_filter.clone());
            }

            // Apply provider-specific tool policy
            if let Some(policy) = resolve_provider_policy(&config, &provider_name, &model_id) {
                tools.set_provider_policy(policy);
            }

            // Session-specific tools (cron, message, send_file, spawn, pipeline)
            // are created per-session by the ActorFactory — not in base registry.

            // Build sub-provider router from config (explicit sub_providers)
            // or auto-populate from fallback_models so the LLM has a model catalog
            // for pipeline DOT generation.
            let provider_router = {
                let router = Arc::new(ProviderRouter::new());
                let mut registered = 0usize;

                // 1. Register explicit sub_providers (highest priority)
                for sp in &config.sub_providers {
                    let sp_config = if sp.api_key_env.is_some() {
                        let mut c = config.clone();
                        c.api_key_env = sp.api_key_env.clone();
                        c
                    } else {
                        config.clone()
                    };
                    match super::chat::create_provider_with_api_type(
                        &sp.provider,
                        &sp_config,
                        sp.model.clone(),
                        sp.base_url.clone(),
                        sp.api_type.as_deref(),
                    ) {
                        Ok(p) => {
                            router.register_with_full_meta(
                                &sp.key,
                                Arc::new(RetryProvider::new(p)),
                                sp.description.clone(),
                                sp.default_context_window,
                                sp.max_output_tokens,
                            );
                            println!(
                                "  {}: {}/{}",
                                "Sub-provider".green(),
                                sp.key,
                                sp.model.as_deref().unwrap_or("default")
                            );
                            registered += 1;
                        }
                        Err(e) => {
                            warn!(key = %sp.key, provider = %sp.provider, error = %e, "skipping sub-provider");
                        }
                    }
                }

                // 2. Auto-register primary + fallback models so the LLM can see
                //    all available models in the pipeline tool's model catalog.
                //    Keys are "{provider}" or "{provider}-{n}" for duplicates.
                if config.sub_providers.is_empty() {
                    // Register primary provider — use model name as key so the
                    // LLM sees the actual model (e.g. "kimi-k2.5") not the API
                    // provider type (e.g. "openai").
                    let primary_key = model_id.clone();
                    router.register_with_full_meta(
                        &primary_key,
                        llm.clone(),
                        Some("Primary model".into()),
                        None,
                        None,
                    );
                    registered += 1;

                    // Register each fallback — use model name as key
                    let mut key_counts: std::collections::HashMap<String, usize> =
                        std::collections::HashMap::new();
                    for fb in &config.fallback_models {
                        let fb_config = {
                            let mut c = config.clone();
                            if fb.api_key_env.is_some() {
                                c.api_key_env = fb.api_key_env.clone();
                            } else if fb.provider != config.provider.as_deref().unwrap_or("") {
                                // Different provider — clear primary's api_key_env so the
                                // registry resolves the correct env var (e.g. OPENAI_API_KEY)
                                c.api_key_env = None;
                            }
                            c
                        };
                        match super::chat::create_provider_with_api_type(
                            &fb.provider,
                            &fb_config,
                            fb.model.clone(),
                            fb.base_url.clone(),
                            fb.api_type.as_deref(),
                        ) {
                            Ok(p) => {
                                // Build a unique key from model name
                                let base_key =
                                    fb.model.as_deref().unwrap_or(&fb.provider).to_string();
                                let count = key_counts.entry(base_key.clone()).or_insert(0);
                                let key = if *count == 0 {
                                    base_key.clone()
                                } else {
                                    format!("{base_key}-{count}")
                                };
                                *count += 1;

                                router.register_with_full_meta(
                                    &key,
                                    Arc::new(RetryProvider::new(p)),
                                    None,
                                    None,
                                    None,
                                );
                                println!(
                                    "  {}: {}/{}",
                                    "Auto sub-provider".cyan(),
                                    key,
                                    fb.model.as_deref().unwrap_or("default")
                                );
                                registered += 1;
                            }
                            Err(e) => {
                                warn!(provider = %fb.provider, error = %e, "skipping fallback as sub-provider");
                            }
                        }
                    }
                }

                if registered > 0 { Some(router) } else { None }
            };

            // Capture config for per-session SpawnTool and PipelineTool creation
            provider_policy_for_factory = tools.provider_policy().cloned();
            worker_prompt_for_factory = Some(super::load_prompt(
                "worker",
                octos_agent::DEFAULT_WORKER_PROMPT,
            ));
            provider_router_for_factory = provider_router.clone();

            // Seed QoS scores on the router for fallback ranking
            if let Some(ref router) = provider_router {
                let catalog_path = data_dir.join("pipeline_models.json");
                let system_catalog = dirs::home_dir()
                    .unwrap_or_default()
                    .join(".octos/model_catalog.json");
                for path in &[catalog_path, system_catalog] {
                    if let Ok(json) = std::fs::read_to_string(path) {
                        if let Ok(catalog) = serde_json::from_str::<octos_llm::QosCatalog>(&json) {
                            let score_entries: Vec<(String, f64)> = catalog
                                .models
                                .iter()
                                .map(|m| (m.provider.clone(), m.score))
                                .collect();
                            router.seed_qos_scores(&score_entries);
                            info!(
                                models = score_entries.len(),
                                "seeded scores for fallback ranking"
                            );
                            break;
                        }
                    }
                }
            }

            // Skill management tool (install/remove/search skills for this profile)
            tools.register(octos_agent::ManageSkillsTool::new(data_dir.join("skills")));

            // Research synthesis tool (shared, no per-session state)
            tools.register(octos_agent::SynthesizeResearchTool::new(
                llm.clone(),
                data_dir.clone(),
            ));

            // Pipeline tool factory for per-session instances
            {
                let llm_c = llm.clone();
                let mem_c = memory.clone();
                let _cwd_c = cwd.clone();
                let data_c = data_dir.clone();
                let policy_c = tools.provider_policy().cloned();
                let plugins_c = plugin_dirs.clone();
                let router_c = provider_router.clone();
                let octos_home_c = self.octos_home.clone();

                struct DefaultPipelineToolFactory {
                    llm: Arc<dyn LlmProvider>,
                    memory: Arc<octos_memory::EpisodeStore>,
                    cwd: PathBuf,
                    data_dir: PathBuf,
                    policy: Option<octos_agent::ToolPolicy>,
                    plugin_dirs: Vec<PathBuf>,
                    router: Option<Arc<ProviderRouter>>,
                    octos_home: Option<PathBuf>,
                }

                impl crate::session_actor::PipelineToolFactory for DefaultPipelineToolFactory {
                    fn create(&self) -> Arc<dyn octos_agent::Tool> {
                        let mut pt = octos_pipeline::RunPipelineTool::new(
                            self.llm.clone(),
                            self.memory.clone(),
                            self.cwd.clone(),
                            self.data_dir.clone(),
                        )
                        .with_provider_policy(self.policy.clone())
                        .with_plugin_dirs(self.plugin_dirs.clone());
                        if let Some(ref router) = self.router {
                            pt = pt.with_provider_router(router.clone());
                        }
                        if let Some(ref octos_home) = self.octos_home {
                            pt = pt.with_octos_home(octos_home.clone());
                        }
                        Arc::new(pt)
                    }
                }

                pipeline_factory = Some(Arc::new(DefaultPipelineToolFactory {
                    llm: llm_c,
                    memory: mem_c,
                    cwd: data_c.clone(), // Pipeline writes to data_dir, not process cwd
                    data_dir: data_c,
                    policy: policy_c,
                    plugin_dirs: plugins_c,
                    router: router_c,
                    octos_home: octos_home_c,
                })
                    as Arc<dyn crate::session_actor::PipelineToolFactory + Send + Sync>);
            }

            // Memory bank tools
            tools.register(octos_agent::RecallMemoryTool::new(memory_store.clone()));
            tools.register(octos_agent::SaveMemoryTool::new(memory_store.clone()));

            // Runtime model switching tool
            tools.register(crate::tools::SwitchModelTool::new(
                swappable.clone(),
                config.clone(),
                self.profile.clone(),
            ));
        }

        // Build system prompt — admin mode uses a built-in admin prompt
        let system_prompt = if admin_mode {
            let custom = gw_config.system_prompt.as_deref();
            if let Some(custom_prompt) = custom {
                custom_prompt.to_string()
            } else {
                let compiled = include_str!("../../prompts/admin_default.txt");
                super::load_prompt("admin", compiled)
            }
        } else {
            build_system_prompt(
                gw_config.system_prompt.as_deref(),
                &data_dir,
                &project_dir,
                &memory_store,
                &skills_loader,
                &tool_config,
            )
            .await
        };

        // Append skill prompt fragments
        let system_prompt = if plugin_result.prompt_fragments.is_empty() {
            system_prompt
        } else {
            let mut prompt = system_prompt;
            for fragment in &plugin_result.prompt_fragments {
                prompt.push_str("\n\n");
                prompt.push_str(fragment);
            }
            prompt
        };

        // Shared system prompt for hot-reload (factory reads this at actor spawn time)
        let system_prompt = Arc::new(std::sync::RwLock::new(system_prompt));

        // Build agent config (shared by all per-session agents)
        let max_iterations = self.max_iterations.or(config.max_iterations).unwrap_or(50);
        let session_timeout_secs = gw_config
            .session_timeout_secs
            .unwrap_or(octos_agent::DEFAULT_SESSION_TIMEOUT_SECS);
        let agent_config = AgentConfig {
            max_iterations,
            save_episodes: false,
            tool_timeout_secs: gw_config
                .tool_timeout_secs
                .unwrap_or(octos_agent::DEFAULT_TOOL_TIMEOUT_SECS),
            // Agent wall-clock timeout matches session timeout so pipelines
            // can run up to 30 minutes without the agent loop aborting early.
            max_timeout: Some(std::time::Duration::from_secs(session_timeout_secs)),
            chat_max_tokens: gw_config.max_output_tokens,
            ..Default::default()
        };

        let llm_for_compaction = llm.clone();

        // Build hook executor and context template (merge config + skill hooks)
        let mut all_hooks = config.hooks.clone();
        all_hooks.extend(plugin_result.hooks);
        let hooks = if !all_hooks.is_empty() {
            Some(Arc::new(HookExecutor::new(all_hooks)))
        } else {
            None
        };
        let hook_context_template = if profile_id.is_some() || hooks.is_some() {
            Some(HookContext {
                session_id: None,
                profile_id: profile_id.clone(),
            })
        } else {
            None
        };

        // Mark base tools that should never be auto-evicted by LRU.
        tools.set_base_tools([
            "run_pipeline",
            "deep_search",
            "deep_crawl",
            "web_search",
            "web_fetch",
            "read_file",
            "write_file",
            "edit_file",
            "shell",
            "list_dir",
            "glob",
            "grep",
            "message",
            "send_file",
            "activate_tools",
        ]);
        // Pin all plugin/skill tools as base so they are never auto-evicted.
        if !plugin_result.tool_names.is_empty() {
            tools.add_base_tools(plugin_result.tool_names.iter().map(|s| s.as_str()));
        }

        // Auto-defer non-core tool groups when tool count is high to prevent
        // overwhelming weaker LLMs (e.g. GLM) that return empty responses
        // when too many tool definitions are present.
        let visible = tools.specs().len();
        if visible > 15 {
            // Keep research (deep_search, deep_crawl) active — users
            // often call these directly. Defer rarely-used groups only.
            for group in &[
                "group:memory",
                "group:admin",
                "group:sessions",
                "group:web",
                "group:runtime",
            ] {
                tools.defer_group(group);
            }
            let after = tools.specs().len();
            info!(
                before = visible,
                after, "auto-deferred tool groups to reduce tool count"
            );
        }
        // Register activate_tools (wired per-session in session_actor)
        if tools.has_deferred() {
            tools.register(octos_agent::ActivateToolsTool::new());
        }

        // Create the base tool registry snapshot (excludes session-specific tools)
        let tool_registry_factory = Arc::new(SnapshotToolRegistryFactory::new(tools));

        // Create session manager (shared between ActorFactory and main loop for commands)
        let session_mgr = Arc::new(Mutex::new(
            SessionManager::open(&data_dir)
                .wrap_err("failed to open session manager")?
                .with_max_sessions(gw_config.max_sessions),
        ));

        let max_history = Arc::new(std::sync::atomic::AtomicUsize::new(gw_config.max_history));

        // Active session store for multi-session support
        let active_sessions = Arc::new(RwLock::new(
            ActiveSessionStore::open(&data_dir).wrap_err("failed to open active session store")?,
        ));

        // Pending message buffer for inactive sessions
        let pending_messages: crate::session_actor::PendingMessages =
            Arc::new(Mutex::new(std::collections::HashMap::new()));

        // Build ActorFactory with all shared resources
        let actor_factory = ActorFactory {
            agent_config,
            llm: llm.clone(),
            llm_for_compaction: llm_for_compaction.clone(),
            memory: memory.clone(),
            system_prompt: system_prompt.clone(),
            hooks,
            hook_context_template,
            data_dir: data_dir.clone(),
            session_mgr: session_mgr.clone(),
            out_tx: out_tx.clone(),
            spawn_inbound_tx,
            cron_service: Some(cron_service.clone()),
            tool_registry_factory,
            pipeline_factory,
            max_history: max_history.clone(),
            idle_timeout: Duration::from_secs(crate::session_actor::DEFAULT_IDLE_TIMEOUT_SECS),
            session_timeout: Duration::from_secs(session_timeout_secs),
            shutdown: shutdown.clone(),
            cwd: cwd.clone(),
            sandbox_config: sandbox_config.clone(),
            provider_policy: provider_policy_for_factory,
            worker_prompt: worker_prompt_for_factory,
            provider_router: provider_router_for_factory,
            embedder: create_embedder(&config).map(|e| e as Arc<dyn octos_llm::EmbeddingProvider>),
            active_sessions: active_sessions.clone(),
            pending_messages: pending_messages.clone(),
            queue_mode: gw_config.queue_mode,
            adaptive_router: adaptive_router_ref,
            memory_store: Some(memory_store.clone()),
        };
        let profile_factory_builder =
            profile_store
                .as_ref()
                .map(|store| ProfileActorFactoryBuilder {
                    profile_store: store.clone(),
                    project_dir: project_dir.clone(),
                    tool_config: tool_config.clone(),
                    memory: memory.clone(),
                    memory_store: memory_store.clone(),
                    agent_config: actor_factory.agent_config.clone(),
                    session_mgr: session_mgr.clone(),
                    out_tx: out_tx.clone(),
                    spawn_inbound_tx: actor_factory.spawn_inbound_tx.clone(),
                    cron_service: cron_service.clone(),
                    tool_registry_factory: actor_factory.tool_registry_factory.clone(),
                    pipeline_factory: actor_factory.pipeline_factory.clone(),
                    max_history: max_history.clone(),
                    session_timeout_secs,
                    shutdown: shutdown.clone(),
                    cwd: cwd.clone(),
                    provider_policy: actor_factory.provider_policy.clone(),
                    worker_prompt: actor_factory.worker_prompt.clone(),
                    provider_router: actor_factory.provider_router.clone(),
                    active_sessions: active_sessions.clone(),
                    pending_messages: pending_messages.clone(),
                    queue_mode: gw_config.queue_mode,
                    plugin_prompt_fragments: plugin_result.prompt_fragments.clone(),
                    no_retry: self.no_retry,
                    sandbox_config: sandbox_config.clone(),
                });

        // Start config watcher for hot-reload
        let watch_paths = {
            let mut paths = Vec::new();
            if let Some(ref p) = self.profile {
                paths.push(p.clone());
            } else if let Some(ref p) = self.config {
                paths.push(p.clone());
            } else {
                let local = project_dir.join("config.json");
                if local.exists() {
                    paths.push(local);
                }
                if let Some(global) = Config::global_config_path() {
                    if global.exists() {
                        paths.push(global);
                    }
                }
            }
            paths
        };
        let (config_tx, mut config_rx) = tokio::sync::watch::channel(None);
        let _watcher_handle = ConfigWatcher::new(watch_paths, config.clone(), config_tx).spawn();

        // Create channel manager and register channels
        let mut channel_mgr = ChannelManager::new();
        for entry in &gw_config.channels {
            match entry.channel_type.as_str() {
                "cli" => {
                    channel_mgr.register(Arc::new(CliChannel::new(shutdown.clone())));
                }
                #[cfg(feature = "telegram")]
                "telegram" => {
                    let env = settings_str(&entry.settings, "token_env", "TELEGRAM_BOT_TOKEN");
                    let token = std::env::var(&env)
                        .wrap_err_with(|| format!("{env} environment variable not set"))?;
                    let bot_username = entry
                        .settings
                        .get("bot_username")
                        .and_then(|v| v.as_str())
                        .map(String::from);
                    let require_mention = entry
                        .settings
                        .get("require_mention")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    let mut tg = octos_bus::TelegramChannel::new(
                        &token,
                        entry.allowed_senders.clone(),
                        shutdown.clone(),
                        media_dir.clone(),
                    );
                    if require_mention {
                        tg = tg.with_mention_gating(bot_username);
                    }
                    channel_mgr.register(Arc::new(tg));
                }
                #[cfg(feature = "discord")]
                "discord" => {
                    let env = settings_str(&entry.settings, "token_env", "DISCORD_BOT_TOKEN");
                    let token = std::env::var(&env)
                        .wrap_err_with(|| format!("{env} environment variable not set"))?;
                    channel_mgr.register(Arc::new(octos_bus::DiscordChannel::new(
                        &token,
                        entry.allowed_senders.clone(),
                        shutdown.clone(),
                        media_dir.clone(),
                    )));
                }
                #[cfg(feature = "slack")]
                "slack" => {
                    let bot_env = settings_str(&entry.settings, "bot_token_env", "SLACK_BOT_TOKEN");
                    let app_env = settings_str(&entry.settings, "app_token_env", "SLACK_APP_TOKEN");
                    let bot_token = std::env::var(&bot_env)
                        .wrap_err_with(|| format!("{bot_env} environment variable not set"))?;
                    let app_token = std::env::var(&app_env)
                        .wrap_err_with(|| format!("{app_env} environment variable not set"))?;
                    channel_mgr.register(Arc::new(octos_bus::SlackChannel::new(
                        &bot_token,
                        &app_token,
                        entry.allowed_senders.clone(),
                        shutdown.clone(),
                        media_dir.clone(),
                    )));
                }
                #[cfg(feature = "whatsapp")]
                "whatsapp" => {
                    let url = settings_str(&entry.settings, "bridge_url", "ws://localhost:3001");
                    channel_mgr.register(Arc::new(octos_bus::WhatsAppChannel::new(
                        &url,
                        entry.allowed_senders.clone(),
                        shutdown.clone(),
                        media_dir.clone(),
                    )));
                }
                #[cfg(feature = "email")]
                "email" => {
                    let imap_host = settings_str(&entry.settings, "imap_host", "");
                    let imap_port = entry
                        .settings
                        .get("imap_port")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(993) as u16;
                    let smtp_host = settings_str(&entry.settings, "smtp_host", "");
                    let smtp_port = entry
                        .settings
                        .get("smtp_port")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(465) as u16;
                    let user_env = settings_str(&entry.settings, "username_env", "EMAIL_USERNAME");
                    let pass_env = settings_str(&entry.settings, "password_env", "EMAIL_PASSWORD");
                    let username =
                        std::env::var(&user_env).wrap_err_with(|| format!("{user_env} not set"))?;
                    let password =
                        std::env::var(&pass_env).wrap_err_with(|| format!("{pass_env} not set"))?;
                    let from_address = settings_str(&entry.settings, "from_address", &username);
                    let poll_interval = entry
                        .settings
                        .get("poll_interval_secs")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(30);
                    let max_body_chars = entry
                        .settings
                        .get("max_body_chars")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(10000) as usize;

                    let email_config = octos_bus::email_channel::EmailConfig {
                        imap_host,
                        imap_port,
                        smtp_host,
                        smtp_port,
                        username,
                        password,
                        from_address,
                        poll_interval_secs: poll_interval,
                        allowed_senders: entry.allowed_senders.clone(),
                        max_body_chars,
                    };
                    channel_mgr.register(Arc::new(octos_bus::EmailChannel::new(
                        email_config,
                        shutdown.clone(),
                    )));
                }
                #[cfg(feature = "feishu")]
                "feishu" | "lark" => {
                    let id_env = settings_str(&entry.settings, "app_id_env", "FEISHU_APP_ID");
                    let secret_env =
                        settings_str(&entry.settings, "app_secret_env", "FEISHU_APP_SECRET");
                    let region = settings_str(&entry.settings, "region", "cn");
                    let app_id = std::env::var(&id_env)
                        .wrap_err_with(|| format!("{id_env} environment variable not set"))?;
                    let app_secret = std::env::var(&secret_env)
                        .wrap_err_with(|| format!("{secret_env} environment variable not set"))?;
                    let mode = settings_str(&entry.settings, "mode", "ws");
                    let webhook_port: u16 = entry
                        .settings
                        .get("webhook_port")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(9321) as u16;
                    let encrypt_key = entry
                        .settings
                        .get("encrypt_key")
                        .and_then(|v| v.as_str())
                        .map(String::from);
                    let verification_token = entry
                        .settings
                        .get("verification_token")
                        .and_then(|v| v.as_str())
                        .map(String::from);
                    channel_mgr.register(Arc::new(
                        octos_bus::FeishuChannel::new(
                            &app_id,
                            &app_secret,
                            entry.allowed_senders.clone(),
                            shutdown.clone(),
                            &region,
                            media_dir.clone(),
                        )
                        .with_mode(&mode)
                        .with_webhook_port(webhook_port)
                        .with_encrypt_key(encrypt_key)
                        .with_verification_token(verification_token),
                    ));
                }
                #[cfg(feature = "twilio")]
                "twilio" => {
                    let sid_env =
                        settings_str(&entry.settings, "account_sid_env", "TWILIO_ACCOUNT_SID");
                    let token_env =
                        settings_str(&entry.settings, "auth_token_env", "TWILIO_AUTH_TOKEN");
                    let from_number = settings_str(&entry.settings, "from_number", "");
                    let account_sid = std::env::var(&sid_env)
                        .wrap_err_with(|| format!("{sid_env} environment variable not set"))?;
                    let auth_token = std::env::var(&token_env)
                        .wrap_err_with(|| format!("{token_env} environment variable not set"))?;
                    let webhook_port: u16 = entry
                        .settings
                        .get("webhook_port")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(8090) as u16;
                    channel_mgr.register(Arc::new(octos_bus::TwilioChannel::new(
                        &account_sid,
                        &auth_token,
                        &from_number,
                        entry.allowed_senders.clone(),
                        shutdown.clone(),
                        media_dir.clone(),
                        webhook_port,
                    )));
                }
                #[cfg(feature = "wecom")]
                "wecom" => {
                    let corp_id_env = settings_str(&entry.settings, "corp_id_env", "WECOM_CORP_ID");
                    let secret_env =
                        settings_str(&entry.settings, "agent_secret_env", "WECOM_AGENT_SECRET");
                    let corp_id = std::env::var(&corp_id_env)
                        .wrap_err_with(|| format!("{corp_id_env} environment variable not set"))?;
                    let agent_secret = std::env::var(&secret_env)
                        .wrap_err_with(|| format!("{secret_env} environment variable not set"))?;
                    let agent_id = settings_str(&entry.settings, "agent_id", "");
                    let verification_token =
                        settings_str(&entry.settings, "verification_token", "");
                    let encoding_aes_key = settings_str(&entry.settings, "encoding_aes_key", "");
                    let webhook_port: u16 = entry
                        .settings
                        .get("webhook_port")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(9322) as u16;
                    channel_mgr.register(Arc::new(
                        octos_bus::WeComChannel::new(
                            &corp_id,
                            &agent_id,
                            &agent_secret,
                            &verification_token,
                            &encoding_aes_key,
                            entry.allowed_senders.clone(),
                            shutdown.clone(),
                            media_dir.clone(),
                        )
                        .with_webhook_port(webhook_port),
                    ));
                }
                #[cfg(feature = "api")]
                "api" => {
                    let port: u16 = self.api_port.unwrap_or_else(|| {
                        entry
                            .settings
                            .get("port")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(8091) as u16
                    });
                    let auth_token = entry
                        .settings
                        .get("auth_token")
                        .and_then(|v| v.as_str())
                        .map(String::from);
                    channel_mgr.register(Arc::new(octos_bus::ApiChannel::new(
                        port,
                        auth_token,
                        shutdown.clone(),
                        session_mgr.clone(),
                    )));
                }
                #[cfg(feature = "wecom-bot")]
                "wecom-bot" => {
                    let bot_id = settings_str(&entry.settings, "bot_id", "");
                    let secret_env =
                        settings_str(&entry.settings, "secret_env", "WECOM_BOT_SECRET");
                    let secret = std::env::var(&secret_env)
                        .wrap_err_with(|| format!("{secret_env} environment variable not set"))?;
                    if bot_id.is_empty() {
                        eyre::bail!("wecom-bot channel requires settings.bot_id");
                    }
                    channel_mgr.register(Arc::new(octos_bus::WeComBotChannel::new(
                        &bot_id,
                        &secret,
                        entry.allowed_senders.clone(),
                        shutdown.clone(),
                    )));
                }
                #[cfg(feature = "matrix")]
                "matrix" => {
                    let settings = MatrixChannelSettings::from_entry(entry)?;
                    let _ = register_matrix_channel(
                        &mut channel_mgr,
                        &mut matrix_channel,
                        &settings,
                        &shutdown,
                        &data_dir,
                    );
                }
                #[cfg(feature = "qq-bot")]
                "qq-bot" => {
                    let app_id = settings_str(&entry.settings, "app_id", "");
                    let client_secret_env =
                        settings_str(&entry.settings, "client_secret_env", "QQ_BOT_CLIENT_SECRET");
                    let client_secret = std::env::var(&client_secret_env).wrap_err_with(|| {
                        format!("{client_secret_env} environment variable not set")
                    })?;
                    if app_id.is_empty() {
                        eyre::bail!("qq-bot channel requires settings.app_id");
                    }
                    channel_mgr.register(Arc::new(octos_bus::QQBotChannel::new(
                        &app_id,
                        &client_secret,
                        entry.allowed_senders.clone(),
                        shutdown.clone(),
                    )));
                }
                #[cfg(feature = "wechat")]
                "wechat" => {
                    let default_url =
                        settings_str(&entry.settings, "bridge_url", "ws://localhost:3201");
                    let bridge_url = self.wechat_bridge_url.as_deref().unwrap_or(&default_url);
                    channel_mgr.register(Arc::new(octos_bus::WeChatChannel::new(
                        &bridge_url,
                        entry.allowed_senders.clone(),
                        shutdown.clone(),
                    )));
                }
                other => {
                    println!(
                        "{}: channel '{}' not supported, skipping",
                        "Warning".yellow(),
                        other
                    );
                }
            }
        }

        // Determine default channel and chat_id for cron delivery fallback
        let default_cron_channel: String = gw_config
            .channels
            .iter()
            .map(|e| e.channel_type.as_str())
            .find(|t| *t != "cli")
            .unwrap_or("cli")
            .to_string();

        // Default chat_id: first allowed_sender from the first non-CLI channel
        let default_cron_chat_id: String = gw_config
            .channels
            .iter()
            .find(|e| e.channel_type != "cli")
            .and_then(|e| e.allowed_senders.first())
            .cloned()
            .unwrap_or_default();

        // Attach bot manager to Matrix channel for slash command handling
        #[cfg(feature = "matrix")]
        if admin_mode {
            if let Some(ref channel) = matrix_channel {
                if let Some(ref store) = profile_store {
                    let bot_mgr = Arc::new(GatewayBotManager {
                        store: store.clone(),
                        channel: channel.clone(),
                        parent_profile_id: profile_id
                            .clone()
                            .unwrap_or_else(|| MAIN_PROFILE_ID.to_string()),
                    });
                    channel.set_bot_manager(bot_mgr);
                    info!("matrix slash commands enabled (/createbot, /deletebot, /listbots)");
                }
            }
        }

        // Start channels and dispatcher
        eprintln!("[gateway] starting channels");
        channel_mgr.start_all(publisher).await?;
        eprintln!("[gateway] channels started");

        // Set up Ctrl+C handler
        tokio::spawn(async move {
            if let Ok(()) = tokio::signal::ctrl_c().await {
                println!();
                println!("{}", "Shutting down gateway...".yellow());
                shutdown_clone.store(true, Ordering::Release);
            }
        });

        println!("{}: {}", "Max history".green(), gw_config.max_history);
        println!(
            "{}: {}",
            "Max concurrent".green(),
            gw_config.max_concurrent_sessions
        );
        println!();
        eprintln!("[gateway] ready");
        println!(
            "{}",
            "Gateway ready. Type a message or /quit to exit.".dimmed()
        );
        println!();

        // Create status indicators for each channel (used for typing + dynamic status)
        let status_words = PersonaService::read_status_words(&data_dir);
        let status_indicators: Arc<HashMap<String, Arc<StatusComposer>>> = {
            let mut map = HashMap::new();
            for entry in &gw_config.channels {
                if let Some(ch) = channel_mgr.get_channel(&entry.channel_type) {
                    map.insert(
                        entry.channel_type.clone(),
                        Arc::new(StatusComposer::new(ch, status_words.clone())),
                    );
                }
            }
            Arc::new(map)
        };

        // Start persona service (generates communication style from chat history)
        let persona_service = Arc::new(PersonaService::new(
            data_dir.clone(),
            llm_for_compaction.clone(),
            crate::persona_service::DEFAULT_INTERVAL_SECS,
        ));
        {
            let system_prompt_for_persona = system_prompt.clone();
            let base_prompt = gw_config.system_prompt.clone();
            let data_dir_p = data_dir.clone();
            let project_dir_p = project_dir.clone();
            let extra_dirs_p = extra_skills_dirs.clone();
            let memory_store_p = memory_store.clone();
            let tool_config_p = tool_config.clone();
            let indicators = status_indicators.clone();
            persona_service.start(
                move |_persona_text| {
                    // Rebuild the full system prompt with the new persona and hot-update
                    let base = base_prompt.clone();
                    let dd = data_dir_p.clone();
                    let pd = project_dir_p.clone();
                    let eds = extra_dirs_p.clone();
                    let ms = memory_store_p.clone();
                    let tc = tool_config_p.clone();
                    let prompt_lock = system_prompt_for_persona.clone();
                    tokio::spawn(async move {
                        let mut sl = SkillsLoader::new(&dd);
                        for dir in &eds {
                            sl.add_skills_dir(dir);
                        }
                        let new_prompt =
                            build_system_prompt(base.as_deref(), &dd, &pd, &ms, &sl, &tc).await;
                        *prompt_lock.write().unwrap_or_else(|e| e.into_inner()) = new_prompt;
                        info!("system prompt updated with new persona");
                    });
                },
                move |words| {
                    // Update status word pools in all indicators
                    for indicator in indicators.values() {
                        indicator.set_words(words.clone());
                    }
                    info!("status words updated in indicators");
                },
            );
        }

        // Semaphore to bound concurrent session processing
        let concurrency_semaphore = Arc::new(Semaphore::new(gw_config.max_concurrent_sessions));

        // Create ActorRegistry for per-session dispatch
        let mut actor_registry = ActorRegistry::new(
            actor_factory,
            concurrency_semaphore,
            out_tx.clone(),
            pending_messages.clone(),
        );
        let mut profile_prompt_cache: HashMap<String, Option<String>> = HashMap::new();

        // Drop the original out_tx — factory and registry hold their own clones.
        // This ensures the outbound channel closes properly when actors shut down.
        drop(out_tx);

        // Alias for hot-reload (avoids shadowing by ConfigChange::HotReload { system_prompt })
        let system_prompt_lock = system_prompt.clone();

        // Main loop: dispatch inbound messages to concurrent tasks
        while let Some(mut inbound) = agent_handle.recv_inbound().await {
            if shutdown.load(Ordering::Acquire) {
                break;
            }

            // Apply hot-reload config changes (stays on main task)
            if config_rx.has_changed().unwrap_or(false) {
                if let Some(change) = config_rx.borrow_and_update().clone() {
                    match change {
                        ConfigChange::HotReload {
                            system_prompt,
                            max_history: new_max,
                        } => {
                            if let Some(prompt) = system_prompt {
                                *system_prompt_lock
                                    .write()
                                    .unwrap_or_else(|e| e.into_inner()) = prompt;
                                info!(
                                    "System prompt updated via hot-reload (new actors will use it)"
                                );
                            }
                            if let Some(new_max) = new_max {
                                max_history.store(new_max, Ordering::Release);
                                info!("Max history updated to {new_max} via hot-reload");
                            }
                        }
                        ConfigChange::RestartRequired(_) => {
                            // Already logged by ConfigWatcher
                        }
                    }
                }
            }

            // Transcribe audio media and separate images (stays on main task).
            // Always append the audio file path so the agent can use it for
            // voice_clone if needed (user may have expressed clone intent in a
            // previous message). Transcribe as usual too — the agent decides.
            let mut image_media = Vec::new();
            let mut is_voice_message = false;
            if let Some(ref asr_bin) = asr_binary {
                for path in &inbound.media {
                    if octos_bus::media::is_audio(path) {
                        is_voice_message = true;
                        // Show "listening" indicator while transcribing voice
                        if let Some(ch) = channel_mgr.get_channel(&inbound.channel) {
                            let _ = ch.send_listening(&inbound.chat_id).await;
                        }
                        let mut input = serde_json::json!({"audio_path": path});
                        if let Some(ref lang) = asr_language {
                            input["language"] = serde_json::Value::String(lang.clone());
                        }
                        match transcribe_via_skill(asr_bin, &input.to_string()).await {
                            Ok(text) => {
                                if let Some(obj) = inbound.metadata.as_object_mut() {
                                    obj.insert(
                                        "voice_transcript".into(),
                                        serde_json::Value::String(text.clone()),
                                    );
                                }
                                let prefix = format!("[Voice transcription: {text}]\n\n");
                                inbound.content = format!("{prefix}{}", inbound.content);
                            }
                            Err(e) => warn!("transcription failed: {e}"),
                        }
                        // Always append audio file path so agent can use it
                        // for voice_clone / voice_save_profile if conversation
                        // context calls for it.
                        inbound.content.push_str(&format!("\n[Audio file: {path}]"));
                    } else if octos_bus::media::is_image(path) {
                        image_media.push(path.clone());
                    }
                }
            } else {
                // Check for audio even without transcriber (for voice_message flag)
                for path in &inbound.media {
                    if octos_bus::media::is_audio(path) {
                        is_voice_message = true;
                    } else if octos_bus::media::is_image(path) {
                        image_media.push(path.clone());
                    }
                }
            }

            // Tag voice messages in metadata for auto-TTS downstream
            if is_voice_message {
                if let Some(obj) = inbound.metadata.as_object_mut() {
                    obj.insert("voice_message".into(), serde_json::Value::Bool(true));
                }
            }

            // Route cron-triggered messages to their target channel
            let (reply_channel, reply_chat_id) = if inbound.channel == "system" {
                let ch = inbound
                    .metadata
                    .get("deliver_to_channel")
                    .and_then(|v| v.as_str())
                    .and_then(|s| if s.is_empty() { None } else { Some(s) })
                    .unwrap_or(&default_cron_channel)
                    .to_string();
                let cid = inbound
                    .metadata
                    .get("deliver_to_chat_id")
                    .and_then(|v| v.as_str())
                    .and_then(|s| if s.is_empty() { None } else { Some(s) })
                    .unwrap_or_else(|| {
                        if !default_cron_chat_id.is_empty() {
                            &default_cron_chat_id
                        } else {
                            &inbound.chat_id
                        }
                    })
                    .to_string();
                (ch, cid)
            } else {
                (inbound.channel.clone(), inbound.chat_id.clone())
            };

            let target_profile = inbound
                .metadata
                .get("target_profile_id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let mut dispatch_profile_id =
                resolve_dispatch_profile_id(target_profile.as_deref(), profile_store.as_deref())?;
            if let Some(ref pid) = dispatch_profile_id {
                if !actor_registry.has_profile_factory(pid) {
                    if let Some(ref builder) = profile_factory_builder {
                        match builder.build(pid).await {
                            Ok(factory) => {
                                actor_registry.register_profile_factory(pid.clone(), factory)
                            }
                            Err(error) => {
                                error!(profile_id = %pid, %error, "failed to build profiled actor factory; falling back to main profile");
                                dispatch_profile_id = None;
                            }
                        }
                    } else {
                        dispatch_profile_id = None;
                    }
                }
            }

            // Resolve session key with active topic, isolated per effective profile.
            let base_session_key = build_profiled_session_key(
                dispatch_profile_id.as_deref(),
                &inbound.channel,
                &inbound.chat_id,
                "",
            );
            let base_key_str = base_session_key.base_key().to_string();
            let session_key = {
                let store = active_sessions.read().await;
                store.resolve_session_key(&base_key_str)
            };

            // Handle callback queries (inline keyboard button presses)
            if inbound
                .metadata
                .get("callback_query")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                let callback_data = inbound
                    .metadata
                    .get("callback_data")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let callback_message_id = inbound
                    .metadata
                    .get("callback_message_id")
                    .and_then(|v| v.as_str())
                    .map(String::from);

                // Session switch callback: "s:topic" or "s:" for default
                if let Some(topic) = callback_data.strip_prefix("s:") {
                    active_sessions
                        .write()
                        .await
                        .switch_to(&base_key_str, topic)
                        .unwrap_or_else(|e| warn!("switch_to failed: {e}"));

                    // Rebuild keyboard with updated active marker
                    let entries = session_mgr.lock().await.list_user_sessions(&base_key_str);
                    let keyboard = session_ui::build_session_keyboard(&entries, topic);
                    let text = session_ui::build_session_text(&entries, topic);

                    // Edit the picker message in-place
                    if let Some(ref mid) = callback_message_id {
                        if let Some(ch) = channel_mgr.get_channel(&reply_channel) {
                            if let Err(e) = ch
                                .edit_message_with_metadata(&reply_chat_id, mid, &text, &keyboard)
                                .await
                            {
                                warn!("failed to edit session picker: {e}");
                            }
                        }
                    }

                    let label = if topic.is_empty() { "(default)" } else { topic };
                    info!(session = %label, "session switched via inline keyboard");

                    // Flush any buffered messages from the target session
                    let target_key = build_profiled_session_key(
                        dispatch_profile_id.as_deref(),
                        &inbound.channel,
                        &inbound.chat_id,
                        topic,
                    );
                    actor_registry.flush_pending(&target_key.to_string()).await;
                    continue;
                }

                // Forward other callback data to the agent as a user message
                // so skills can use inline keyboards for interactive menus
                inbound.content = format!("[callback] {callback_data}");
                // Fall through to normal message processing
            }

            let cmd = inbound.content.trim();

            // Handle /new command — clear current session or create named session
            if cmd == "/new" || cmd.starts_with("/new ") {
                let name = cmd.strip_prefix("/new").unwrap_or("").trim();
                if name.is_empty() {
                    // Clear current session (existing behavior)
                    match session_mgr.lock().await.clear(&session_key).await {
                        Ok(()) => {
                            let _ = agent_handle
                                .send_outbound(make_reply(
                                    &reply_channel,
                                    &reply_chat_id,
                                    "Session cleared.",
                                ))
                                .await;
                        }
                        Err(e) => {
                            warn!("session clear failed: {e}");
                        }
                    }
                } else {
                    // Create/switch to named session
                    if let Err(reason) = validate_topic_name(name) {
                        let _ = agent_handle
                            .send_outbound(make_reply(
                                &reply_channel,
                                &reply_chat_id,
                                format!("Invalid session name: {reason}"),
                            ))
                            .await;
                    } else {
                        active_sessions
                            .write()
                            .await
                            .switch_to(&base_key_str, name)
                            .unwrap_or_else(|e| warn!("switch_to failed: {e}"));
                        let _ = agent_handle
                            .send_outbound(make_reply(
                                &reply_channel,
                                &reply_chat_id,
                                format!("Switched to session: {name}"),
                            ))
                            .await;
                    }
                }
                continue;
            }

            // Handle /s command — switch to a named session
            if cmd == "/s" || cmd.starts_with("/s ") {
                let name = cmd.strip_prefix("/s").unwrap_or("").trim();
                if name.is_empty() {
                    // Switch to default session
                    active_sessions
                        .write()
                        .await
                        .switch_to(&base_key_str, "")
                        .unwrap_or_else(|e| warn!("switch_to failed: {e}"));
                    let _ = agent_handle
                        .send_outbound(make_reply(
                            &reply_channel,
                            &reply_chat_id,
                            "Switched to default session.",
                        ))
                        .await;

                    // Flush any buffered messages from this session
                    let target_key = build_profiled_session_key(
                        dispatch_profile_id.as_deref(),
                        &inbound.channel,
                        &inbound.chat_id,
                        "",
                    );
                    actor_registry.flush_pending(&target_key.to_string()).await;
                } else if let Err(reason) = validate_topic_name(name) {
                    let _ = agent_handle
                        .send_outbound(make_reply(
                            &reply_channel,
                            &reply_chat_id,
                            format!("Invalid session name: {reason}"),
                        ))
                        .await;
                } else {
                    active_sessions
                        .write()
                        .await
                        .switch_to(&base_key_str, name)
                        .unwrap_or_else(|e| warn!("switch_to failed: {e}"));

                    // Show last 2 messages as context preview
                    let new_key = build_profiled_session_key(
                        dispatch_profile_id.as_deref(),
                        &inbound.channel,
                        &inbound.chat_id,
                        name,
                    );
                    let preview = {
                        let mut mgr = session_mgr.lock().await;
                        let session = mgr.get_or_create(&new_key);
                        let history = session.get_history(2);
                        if history.is_empty() {
                            String::new()
                        } else {
                            let mut lines = String::from("\n---\n");
                            for m in history {
                                let role = m.role.as_str();
                                let text: String = m.content.chars().take(100).collect();
                                lines.push_str(&format!("[{role}] {text}\n"));
                            }
                            lines
                        }
                    };

                    let _ = agent_handle
                        .send_outbound(make_reply(
                            &reply_channel,
                            &reply_chat_id,
                            format!("Switched to session: {name}{preview}"),
                        ))
                        .await;

                    // Flush any buffered messages from this session
                    actor_registry.flush_pending(&new_key.to_string()).await;
                }
                continue;
            }

            // Handle /sessions command — list all sessions with inline keyboard
            if cmd == "/sessions" {
                let entries = session_mgr.lock().await.list_user_sessions(&base_key_str);
                let active_topic = active_sessions
                    .read()
                    .await
                    .get_active_topic(&base_key_str)
                    .to_string();

                if entries.is_empty() {
                    let _ = agent_handle
                        .send_outbound(make_reply(
                            &reply_channel,
                            &reply_chat_id,
                            "No sessions found. Use /new <name> to create one.",
                        ))
                        .await;
                } else {
                    let keyboard = session_ui::build_session_keyboard(&entries, &active_topic);
                    let text = session_ui::build_session_text(&entries, &active_topic);
                    let mut msg = make_reply(&reply_channel, &reply_chat_id, text);
                    msg.metadata = keyboard;
                    let _ = agent_handle.send_outbound(msg).await;
                }
                continue;
            }

            // Handle /back (or /b) command — switch to previous session
            if cmd == "/back" || cmd == "/b" {
                let result = active_sessions.write().await.go_back(&base_key_str);
                match result {
                    Ok(Some(topic)) => {
                        let label = if topic.is_empty() {
                            "(default)".to_string()
                        } else {
                            topic.clone()
                        };
                        let _ = agent_handle
                            .send_outbound(make_reply(
                                &reply_channel,
                                &reply_chat_id,
                                format!("Switched back to session: {label}"),
                            ))
                            .await;

                        // Flush any buffered messages from the target session
                        let target_key = build_profiled_session_key(
                            dispatch_profile_id.as_deref(),
                            &inbound.channel,
                            &inbound.chat_id,
                            &topic,
                        );
                        actor_registry.flush_pending(&target_key.to_string()).await;
                    }
                    Ok(None) => {
                        let _ = agent_handle
                            .send_outbound(make_reply(
                                &reply_channel,
                                &reply_chat_id,
                                "No previous session to switch to.",
                            ))
                            .await;
                    }
                    Err(e) => {
                        warn!("go_back failed: {e}");
                    }
                }
                continue;
            }

            // Handle /delete command — delete a named session
            if cmd.starts_with("/delete ") {
                let name = cmd.strip_prefix("/delete").unwrap_or("").trim();
                if name.is_empty() {
                    let _ = agent_handle
                        .send_outbound(make_reply(
                            &reply_channel,
                            &reply_chat_id,
                            "Usage: /delete <session-name>",
                        ))
                        .await;
                } else {
                    let del_key = build_profiled_session_key(
                        dispatch_profile_id.as_deref(),
                        &inbound.channel,
                        &inbound.chat_id,
                        name,
                    );
                    match session_mgr.lock().await.clear(&del_key).await {
                        Ok(()) => {
                            active_sessions
                                .write()
                                .await
                                .remove_topic(&base_key_str, name)
                                .unwrap_or_else(|e| warn!("remove_topic failed: {e}"));
                            let _ = agent_handle
                                .send_outbound(make_reply(
                                    &reply_channel,
                                    &reply_chat_id,
                                    format!("Deleted session: {name}"),
                                ))
                                .await;
                        }
                        Err(e) => {
                            warn!("delete session failed: {e}");
                        }
                    }
                }
                continue;
            }

            // Handle /config command inline
            if inbound.content.trim() == "/config" || inbound.content.trim().starts_with("/config ")
            {
                let args = inbound
                    .content
                    .trim()
                    .strip_prefix("/config")
                    .unwrap_or("")
                    .trim();
                let response = tool_config.handle_config_command(args).await;
                let _ = agent_handle
                    .send_outbound(make_reply(&reply_channel, &reply_chat_id, response))
                    .await;
                continue;
            }

            // Handle /account command inline — sub-account management
            if inbound.content.trim() == "/account"
                || inbound.content.trim().starts_with("/account ")
            {
                let args = inbound
                    .content
                    .trim()
                    .strip_prefix("/account")
                    .unwrap_or("")
                    .trim();
                let response = account_handler::handle_account_command(
                    args,
                    profile_id.as_deref(),
                    &profile_store,
                )
                .await;
                let _ = agent_handle
                    .send_outbound(make_reply(&reply_channel, &reply_chat_id, response))
                    .await;
                continue;
            }

            // Handle /skills command inline — skill management
            if inbound.content.trim() == "/skills" || inbound.content.trim().starts_with("/skills ")
            {
                let args = inbound
                    .content
                    .trim()
                    .strip_prefix("/skills")
                    .unwrap_or("")
                    .trim();
                let response = skills_handler::handle_skills_command(
                    args,
                    profile_id.as_deref(),
                    &data_dir,
                    &profile_store,
                )
                .await;
                let _ = agent_handle
                    .send_outbound(make_reply(&reply_channel, &reply_chat_id, response))
                    .await;
                continue;
            }

            info!(
                channel = %inbound.channel,
                sender = %inbound.sender_id,
                session = %session_key,
                "dispatching message to session actor"
            );

            // Skip status indicator for cron/heartbeat messages — they're background tasks
            let status_indicator = if inbound.channel == "system" {
                None
            } else {
                status_indicators.get(&reply_channel).cloned()
            };

            let (prompt_override, dispatch_sender_uid) = if let Some(ref pid) = dispatch_profile_id
            {
                let prompt = if actor_registry.has_profile_factory(pid) {
                    None
                } else if !profile_prompt_cache.contains_key(pid.as_str()) {
                    let loaded = if let Some(ref store) = profile_store {
                        match store.get(pid) {
                            Ok(Some(p)) => Some(p.config.gateway.system_prompt),
                            Ok(None) => {
                                warn!(profile_id = %pid, "target profile not found");
                                None
                            }
                            Err(e) => {
                                warn!(profile_id = %pid, error = %e, "failed to load profile");
                                None
                            }
                        }
                    } else {
                        None
                    };
                    let prompt_val = loaded.flatten();
                    profile_prompt_cache.insert(pid.clone(), prompt_val.clone());
                    prompt_val
                } else {
                    profile_prompt_cache.get(pid.as_str()).cloned().flatten()
                };

                #[cfg(feature = "matrix")]
                let sender_uid = if let Some(ref mc) = matrix_channel {
                    let uid = mc.bot_router().reverse_route(pid).await;
                    tracing::debug!(profile_id = %pid, sender_uid = ?uid, "resolved sender_user_id for profile");
                    uid
                } else {
                    None
                };
                #[cfg(not(feature = "matrix"))]
                let sender_uid: Option<String> = None;

                (prompt, sender_uid)
            } else {
                (None, None)
            };

            // Dispatch to per-session actor (creates one if needed)
            tracing::debug!(
                dispatch_profile_id = ?dispatch_profile_id,
                dispatch_sender_uid = ?dispatch_sender_uid,
                "dispatching to actor"
            );
            actor_registry
                .dispatch(crate::session_actor::DispatchParams {
                    message: inbound,
                    image_media,
                    session_key,
                    reply_channel: &reply_channel,
                    reply_chat_id: &reply_chat_id,
                    status_indicator,
                    profile_id: dispatch_profile_id.as_deref(),
                    system_prompt_override: prompt_override,
                    sender_user_id: dispatch_sender_uid,
                })
                .await;

            // Periodically reap dead actors to free resources
            actor_registry.reap_dead_actors();
        }

        // Shut down all session actors gracefully
        actor_registry.shutdown_all().await;

        persona_service.stop().await;
        heartbeat_service.stop().await;
        cron_service.stop().await;
        channel_mgr.stop_all().await?;
        println!("{}", "Gateway stopped.".dimmed());
        Ok(())
    }
}

/// Build a simple text reply to send back on the same channel/chat.
fn make_reply(channel: &str, chat_id: &str, content: impl Into<String>) -> OutboundMessage {
    OutboundMessage {
        channel: channel.to_string(),
        chat_id: chat_id.to_string(),
        content: content.into(),
        reply_to: None,
        media: vec![],
        metadata: serde_json::json!({}),
    }
}

/// Merge queued inbound messages by session key.
/// Messages from the same session are concatenated with `\n\n`.
/// Used by Collect queue mode (reserved for future concurrent collect support).
#[allow(dead_code)]
fn merge_queued_by_session(
    messages: Vec<octos_core::InboundMessage>,
) -> Vec<octos_core::InboundMessage> {
    use std::collections::BTreeMap;
    let mut groups: BTreeMap<String, Vec<octos_core::InboundMessage>> = BTreeMap::new();
    let mut order: Vec<String> = Vec::new();
    for msg in messages {
        let key = msg.session_key().to_string();
        if !groups.contains_key(&key) {
            order.push(key.clone());
        }
        groups.entry(key).or_default().push(msg);
    }
    order
        .into_iter()
        .filter_map(|key| {
            let mut msgs = groups.remove(&key)?;
            if msgs.len() == 1 {
                return msgs.pop();
            }
            let mut base = msgs.remove(0);
            for m in &msgs {
                base.content.push_str("\n\n");
                base.content.push_str(&m.content);
            }
            Some(base)
        })
        .collect()
}

/// Transcribe audio by spawning the voice platform skill binary.
async fn transcribe_via_skill(
    voice_binary: &std::path::Path,
    input_json: &str,
) -> eyre::Result<String> {
    use tokio::io::AsyncWriteExt;

    let mut child = tokio::process::Command::new(voice_binary)
        .arg("voice_transcribe")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .wrap_err("failed to spawn voice skill binary")?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(input_json.as_bytes()).await?;
        drop(stdin);
    }

    let output = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        child.wait_with_output(),
    )
    .await
    .map_err(|_| eyre::eyre!("voice transcription timed out"))??;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let result: serde_json::Value =
        serde_json::from_str(&stdout).wrap_err("invalid voice skill output")?;

    if result.get("success").and_then(|v| v.as_bool()) == Some(true) {
        Ok(result["output"].as_str().unwrap_or("").to_string())
    } else {
        let msg = result["output"].as_str().unwrap_or("unknown error");
        eyre::bail!("voice skill failed: {msg}")
    }
}

/// Build environment variables to inject into plugin processes so skills can
/// route API calls through the configured provider/gateway.
///
/// Maps octos provider config → env vars that downstream skills understand:
/// - `GEMINI_API_KEY`, `GEMINI_BASE_URL` — for Gemini-backed skills (mofa, etc.)
/// - `DASHSCOPE_API_KEY` — for Dashscope/Qwen skills
/// - `OPENAI_API_KEY`, `OPENAI_BASE_URL` — for OpenAI-compatible skills
fn build_plugin_env(config: &crate::config::Config, provider_name: &str) -> Vec<(String, String)> {
    let mut env = Vec::new();

    // Resolve the provider's base URL (config override > registry default)
    let base_url = config.base_url.clone().or_else(|| {
        octos_llm::registry::lookup(provider_name)
            .and_then(|e| e.default_base_url)
            .map(String::from)
    });

    // AI gateway providers (r9s, etc.) support multiple downstream APIs with
    // the same credentials. Inject env vars for ALL downstream APIs so skills
    // like mofa-slides (Gemini), mofa-infographic (Gemini + Dashscope) work.
    let is_gateway = matches!(provider_name, "r9s" | "r9s.ai");

    if let Ok(api_key) = config.get_api_key(provider_name) {
        if is_gateway {
            // Gateway: same API key works for all downstream providers
            env.push(("GEMINI_API_KEY".to_string(), api_key.clone()));
            env.push(("DASHSCOPE_API_KEY".to_string(), api_key.clone()));
            env.push(("OPENAI_API_KEY".to_string(), api_key));
        } else {
            let key_var = match provider_name {
                "gemini" | "google" => "GEMINI_API_KEY",
                "dashscope" | "qwen" => "DASHSCOPE_API_KEY",
                _ => "OPENAI_API_KEY",
            };
            env.push((key_var.to_string(), api_key));
        }
    }

    if let Some(ref url) = base_url {
        if is_gateway {
            // Gateway: each downstream API has its own path prefix.
            // The registry base_url is the OpenAI-compatible endpoint (e.g. https://api.r9s.ai/v1).
            // Derive the Gemini and Dashscope URLs by replacing the path.
            let origin = url.trim_end_matches('/');
            let origin_base = origin.rfind("/v").map(|i| &origin[..i]).unwrap_or(origin);
            env.push((
                "GEMINI_BASE_URL".to_string(),
                format!("{origin_base}/v1beta"),
            ));
            env.push((
                "DASHSCOPE_BASE_URL".to_string(),
                format!("{origin_base}/compatible-mode/v1"),
            ));
            env.push(("OPENAI_BASE_URL".to_string(), url.clone()));
        } else {
            let url_var = match provider_name {
                "gemini" | "google" => "GEMINI_BASE_URL",
                "dashscope" | "qwen" => "DASHSCOPE_BASE_URL",
                _ => "OPENAI_BASE_URL",
            };
            env.push((url_var.to_string(), url.clone()));
        }
    }

    // Also inject keys for any secondary providers configured as fallbacks,
    // so skills that call multiple APIs (e.g. Gemini for image + Dashscope for OCR)
    // can access all configured keys.
    for fb in &config.fallback_models {
        let fb_provider = fb.provider.as_str();
        let fb_config = if fb.api_key_env.is_some() {
            let mut c = config.clone();
            c.api_key_env = fb.api_key_env.clone();
            c
        } else {
            config.clone()
        };

        if let Ok(key) = fb_config.get_api_key(fb_provider) {
            let key_var = match fb_provider {
                "gemini" | "google" => "GEMINI_API_KEY",
                "dashscope" | "qwen" => "DASHSCOPE_API_KEY",
                _ => continue, // don't overwrite primary OPENAI_API_KEY
            };
            if !env.iter().any(|(k, _)| k == key_var) {
                env.push((key_var.to_string(), key));
            }
        }

        if let Some(ref url) = fb.base_url {
            let url_var = match fb_provider {
                "gemini" | "google" => "GEMINI_BASE_URL",
                "dashscope" | "qwen" => "DASHSCOPE_BASE_URL",
                _ => continue,
            };
            if !env.iter().any(|(k, _)| k == url_var) {
                env.push((url_var.to_string(), url.clone()));
            }
        }
    }

    if !env.is_empty() {
        info!(
            count = env.len(),
            vars = ?env.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
            "injecting provider env vars into plugin processes"
        );
    }

    env
}

#[cfg(all(test, feature = "matrix"))]
mod tests {
    use super::*;
    use chrono::Utc;
    use octos_agent::ToolConfigStore;
    use octos_bus::BotManager;
    use octos_memory::{EpisodeStore, MemoryStore};
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use tokio::sync::{Mutex, RwLock, mpsc};

    fn make_profile(id: &str, system_prompt: Option<&str>) -> crate::profiles::UserProfile {
        crate::profiles::UserProfile {
            id: id.to_string(),
            name: id.to_string(),
            enabled: false,
            data_dir: None,
            parent_id: None,
            config: crate::profiles::ProfileConfig {
                gateway: crate::profiles::GatewaySettings {
                    system_prompt: system_prompt.map(str::to_string),
                    ..Default::default()
                },
                ..Default::default()
            },
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn test_child_bot_from_admin_parent_gets_normal_tooling() {
        let dir = tempfile::TempDir::new().unwrap();
        let project_dir = dir.path().join("octos-home");
        std::fs::create_dir_all(&project_dir).unwrap();
        let _ = octos_agent::bootstrap::bootstrap_bundled_skills(&project_dir);

        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("OPENAI_API_KEY", "test-key");
        }

        let store = Arc::new(crate::profiles::ProfileStore::open(dir.path()).unwrap());

        let mut parent = make_profile("botfather", Some("admin parent"));
        parent.config.provider = Some("openai".into());
        parent.config.model = Some("gpt-4o-mini".into());
        parent.config.api_key_env = Some("OPENAI_API_KEY".into());
        parent.config.fallback_models = vec![crate::profiles::FallbackModelConfig {
            provider: "openai".into(),
            model: Some("gpt-4o".into()),
            api_key_env: Some("OPENAI_API_KEY".into()),
            ..Default::default()
        }];
        parent.config.admin_mode = true;
        store.save(&parent).unwrap();

        let mut child = make_profile("botfather--researcher", Some("child prompt"));
        child.parent_id = Some(parent.id.clone());
        store.save(&child).unwrap();

        let base_data_dir = dir.path().join("data");
        std::fs::create_dir_all(&base_data_dir).unwrap();
        let tool_config = Arc::new(ToolConfigStore::open(&base_data_dir).await.unwrap());
        let memory = Arc::new(EpisodeStore::open(&base_data_dir).await.unwrap());
        let memory_store = Arc::new(MemoryStore::open(&base_data_dir).await.unwrap());
        let session_mgr = Arc::new(Mutex::new(SessionManager::open(&base_data_dir).unwrap()));
        let active_sessions = Arc::new(RwLock::new(
            ActiveSessionStore::open(&base_data_dir).unwrap(),
        ));
        let pending_messages: crate::session_actor::PendingMessages =
            Arc::new(Mutex::new(std::collections::HashMap::new()));
        let (out_tx, _out_rx) = mpsc::channel(4);
        let (spawn_inbound_tx, _spawn_inbound_rx) = mpsc::channel(4);
        let (cron_in_tx, _cron_in_rx) = mpsc::channel(1);
        let cron_service = Arc::new(CronService::new(base_data_dir.join("cron"), cron_in_tx));

        let builder = ProfileActorFactoryBuilder {
            profile_store: store,
            project_dir: project_dir.clone(),
            tool_config,
            memory,
            memory_store,
            agent_config: AgentConfig::default(),
            session_mgr,
            out_tx,
            spawn_inbound_tx,
            cron_service,
            tool_registry_factory: Arc::new(SnapshotToolRegistryFactory::new(ToolRegistry::new())),
            pipeline_factory: None,
            max_history: Arc::new(AtomicUsize::new(50)),
            session_timeout_secs: octos_agent::DEFAULT_SESSION_TIMEOUT_SECS,
            shutdown: Arc::new(AtomicBool::new(false)),
            cwd: project_dir.clone(),
            provider_policy: None,
            worker_prompt: None,
            provider_router: None,
            active_sessions,
            pending_messages,
            queue_mode: crate::config::QueueMode::Followup,
            plugin_prompt_fragments: vec![],
            no_retry: false,
            sandbox_config: octos_agent::SandboxConfig::default(),
        };

        let factory = builder.build("botfather--researcher").await.unwrap();
        let registry = factory.tool_registry_factory.create_base_registry();
        let expected_data_dir = dir
            .path()
            .join("profiles")
            .join("botfather--researcher")
            .join("data");

        assert!(
            registry.get("web_search").is_some(),
            "child bot should expose normal-mode web_search"
        );
        assert!(
            registry.get("deep_search").is_some(),
            "child bot should expose bundled deep_search skill"
        );
        assert!(
            registry.get("synthesize_research").is_some(),
            "child bot should expose research synthesis tooling"
        );
        assert!(
            factory.pipeline_factory.is_some(),
            "child bot should build its own pipeline factory instead of inheriting admin-only None"
        );
        assert!(
            factory.provider_router.is_some(),
            "child bot should build a provider router for fallback-aware spawn/pipeline"
        );
        assert_eq!(
            factory.data_dir, expected_data_dir,
            "child bot should use its own data dir for sessions/status"
        );
    }

    fn matrix_entry(settings: serde_json::Value) -> crate::config::ChannelEntry {
        crate::config::ChannelEntry {
            channel_type: MATRIX_CHANNEL_TYPE.to_string(),
            allowed_senders: Vec::new(),
            settings,
        }
    }

    #[test]
    fn matrix_channel_settings_use_defaults() {
        let entry = matrix_entry(serde_json::json!({
            MATRIX_SETTING_AS_TOKEN: "as-token",
            MATRIX_SETTING_HS_TOKEN: "hs-token",
        }));

        let settings = MatrixChannelSettings::from_entry(&entry).unwrap();

        assert_eq!(settings.homeserver, MATRIX_DEFAULT_HOMESERVER);
        assert_eq!(settings.server_name, MATRIX_DEFAULT_SERVER_NAME);
        assert_eq!(settings.sender_localpart, MATRIX_DEFAULT_SENDER_LOCALPART);
        assert_eq!(settings.user_prefix, MATRIX_DEFAULT_USER_PREFIX);
        assert_eq!(settings.port, MATRIX_DEFAULT_PORT);
        assert!(settings.allowed_senders.is_empty());
    }

    #[test]
    fn matrix_channel_settings_copy_allowed_senders() {
        let entry = crate::config::ChannelEntry {
            channel_type: MATRIX_CHANNEL_TYPE.to_string(),
            allowed_senders: vec!["@alice:localhost".into(), "@bob:localhost".into()],
            settings: serde_json::json!({
                MATRIX_SETTING_AS_TOKEN: "as-token",
                MATRIX_SETTING_HS_TOKEN: "hs-token",
            }),
        };

        let settings = MatrixChannelSettings::from_entry(&entry).unwrap();

        assert_eq!(
            settings.allowed_senders,
            vec!["@alice:localhost".to_string(), "@bob:localhost".to_string()]
        );
    }

    #[test]
    fn matrix_channel_settings_require_tokens() {
        let entry = matrix_entry(serde_json::json!({}));

        let err = MatrixChannelSettings::from_entry(&entry).unwrap_err();

        assert!(err.to_string().contains(MATRIX_MISSING_TOKENS_ERROR));
    }

    #[test]
    fn matrix_channel_settings_reject_out_of_range_port() {
        let entry = matrix_entry(serde_json::json!({
            MATRIX_SETTING_AS_TOKEN: "as-token",
            MATRIX_SETTING_HS_TOKEN: "hs-token",
            "port": 70000,
        }));

        let err = MatrixChannelSettings::from_entry(&entry).unwrap_err();

        assert!(err.to_string().contains("port"));
    }

    #[test]
    fn test_gateway_registers_matrix_channel() {
        let entry = matrix_entry(serde_json::json!({
            MATRIX_SETTING_AS_TOKEN: "as-token",
            MATRIX_SETTING_HS_TOKEN: "hs-token",
        }));
        let settings = MatrixChannelSettings::from_entry(&entry).unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let data_dir = tempfile::TempDir::new().unwrap();
        let mut channel_mgr = ChannelManager::new();
        let mut matrix_channel = None;

        let channel = register_matrix_channel(
            &mut channel_mgr,
            &mut matrix_channel,
            &settings,
            &shutdown,
            data_dir.path(),
        );

        assert!(channel_mgr.get_channel(MATRIX_CHANNEL_TYPE).is_some());
        assert!(matrix_channel.is_some());
        assert!(Arc::ptr_eq(
            &channel,
            matrix_channel
                .as_ref()
                .expect("matrix channel should be cached")
        ));
    }

    #[test]
    fn test_dispatch_unknown_profile_falls_back() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = crate::profiles::ProfileStore::open(dir.path()).unwrap();
        store
            .save(&make_profile("weather", Some("weather prompt")))
            .unwrap();

        let resolved = resolve_dispatch_profile_id(Some("missing-profile"), Some(&store)).unwrap();

        assert_eq!(resolved, None);
    }

    #[test]
    fn test_dispatch_known_profile_keeps_target() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = crate::profiles::ProfileStore::open(dir.path()).unwrap();
        store
            .save(&make_profile("weather", Some("weather prompt")))
            .unwrap();

        let resolved = resolve_dispatch_profile_id(Some("weather"), Some(&store)).unwrap();

        assert_eq!(resolved.as_deref(), Some("weather"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_delete_bot_keeps_route_when_profile_delete_fails() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = Arc::new(crate::profiles::ProfileStore::open(dir.path()).unwrap());
        let mut parent = make_profile("botfather", None);
        parent
            .config
            .channels
            .push(crate::profiles::ChannelCredentials::Matrix {
                homeserver: "http://localhost:6167".to_string(),
                as_token: "as-token".to_string(),
                hs_token: "hs-token".to_string(),
                server_name: "localhost".to_string(),
                sender_localpart: "bot".to_string(),
                user_prefix: "bot_".to_string(),
                port: MATRIX_DEFAULT_PORT,
                allowed_senders: vec![],
            });
        store.save(&parent).unwrap();

        let mut sub = make_profile("botfather--weatherbot", None);
        sub.parent_id = Some(parent.id.clone());
        store.save(&sub).unwrap();

        let channel = Arc::new(
            octos_bus::MatrixChannel::new(
                "http://localhost:6167",
                "as-token",
                "hs-token",
                "localhost",
                "bot",
                "bot_",
                6166,
                Arc::new(AtomicBool::new(false)),
            )
            .with_bot_router(dir.path()),
        );
        channel
            .bot_router()
            .register_entry(
                "@bot_weatherbot:localhost",
                &sub.id,
                "@alice:localhost",
                octos_bus::BotVisibility::Private,
            )
            .await
            .unwrap();

        let profiles_dir = dir.path().join("profiles");
        let original_mode = std::fs::metadata(&profiles_dir)
            .unwrap()
            .permissions()
            .mode();
        let mut perms = std::fs::metadata(&profiles_dir).unwrap().permissions();
        perms.set_mode(0o555);
        std::fs::set_permissions(&profiles_dir, perms).unwrap();

        let manager = GatewayBotManager {
            store: store.clone(),
            channel: channel.clone(),
            parent_profile_id: parent.id.clone(),
        };

        let result = manager
            .delete_bot("@bot_weatherbot:localhost", "@alice:localhost")
            .await;

        let mut restore = std::fs::metadata(&profiles_dir).unwrap().permissions();
        restore.set_mode(original_mode);
        std::fs::set_permissions(&profiles_dir, restore).unwrap();

        assert!(
            result.is_err(),
            "delete should fail when profile cannot be removed"
        );
        assert_eq!(
            channel
                .bot_router()
                .route("@bot_weatherbot:localhost")
                .await,
            Some(sub.id.clone()),
            "route should remain registered when profile deletion fails"
        );
    }

    #[tokio::test]
    async fn test_delete_bot_rejects_non_owner() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = Arc::new(crate::profiles::ProfileStore::open(dir.path()).unwrap());
        let mut parent = make_profile("botfather", None);
        parent
            .config
            .channels
            .push(crate::profiles::ChannelCredentials::Matrix {
                homeserver: "http://localhost:6167".to_string(),
                as_token: "as-token".to_string(),
                hs_token: "hs-token".to_string(),
                server_name: "localhost".to_string(),
                sender_localpart: "bot".to_string(),
                user_prefix: "bot_".to_string(),
                port: MATRIX_DEFAULT_PORT,
                allowed_senders: vec![],
            });
        store.save(&parent).unwrap();

        let mut sub = make_profile("botfather--weatherbot", None);
        sub.parent_id = Some(parent.id.clone());
        store.save(&sub).unwrap();

        let channel = Arc::new(
            octos_bus::MatrixChannel::new(
                "http://localhost:6167",
                "as-token",
                "hs-token",
                "localhost",
                "bot",
                "bot_",
                6166,
                Arc::new(AtomicBool::new(false)),
            )
            .with_bot_router(dir.path()),
        );
        channel
            .bot_router()
            .register_entry(
                "@bot_weatherbot:localhost",
                &sub.id,
                "@alice:localhost",
                octos_bus::BotVisibility::Public,
            )
            .await
            .unwrap();

        let manager = GatewayBotManager {
            store: store.clone(),
            channel: channel.clone(),
            parent_profile_id: parent.id.clone(),
        };

        let result = manager
            .delete_bot("@bot_weatherbot:localhost", "@mallory:localhost")
            .await;

        let err = result.expect_err("non-owner delete should fail");
        assert!(
            err.to_string().contains("only delete bots you created"),
            "unexpected error: {err}"
        );
        assert_eq!(
            channel
                .bot_router()
                .route("@bot_weatherbot:localhost")
                .await,
            Some(sub.id.clone())
        );
    }

    #[tokio::test]
    async fn test_delete_bot_allows_operator_override() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = Arc::new(crate::profiles::ProfileStore::open(dir.path()).unwrap());
        let mut parent = make_profile("botfather", None);
        parent
            .config
            .channels
            .push(crate::profiles::ChannelCredentials::Matrix {
                homeserver: "http://localhost:6167".to_string(),
                as_token: "as-token".to_string(),
                hs_token: "hs-token".to_string(),
                server_name: "localhost".to_string(),
                sender_localpart: "bot".to_string(),
                user_prefix: "bot_".to_string(),
                port: MATRIX_DEFAULT_PORT,
                allowed_senders: vec!["@admin:localhost".to_string()],
            });
        store.save(&parent).unwrap();

        let mut sub = make_profile("botfather--weatherbot", None);
        sub.parent_id = Some(parent.id.clone());
        store.save(&sub).unwrap();

        let channel = Arc::new(
            octos_bus::MatrixChannel::new(
                "http://localhost:6167",
                "as-token",
                "hs-token",
                "localhost",
                "bot",
                "bot_",
                6166,
                Arc::new(AtomicBool::new(false)),
            )
            .with_admin_allowed_senders(vec!["@admin:localhost".to_string()])
            .with_bot_router(dir.path()),
        );
        channel
            .bot_router()
            .register_entry(
                "@bot_weatherbot:localhost",
                &sub.id,
                "@alice:localhost",
                octos_bus::BotVisibility::Private,
            )
            .await
            .unwrap();

        let manager = GatewayBotManager {
            store: store.clone(),
            channel: channel.clone(),
            parent_profile_id: parent.id.clone(),
        };

        let result = manager
            .delete_bot("@bot_weatherbot:localhost", "@admin:localhost")
            .await;

        assert!(
            result.is_ok(),
            "operator override should succeed: {result:?}"
        );
        assert_eq!(
            channel
                .bot_router()
                .route("@bot_weatherbot:localhost")
                .await,
            None
        );
    }
}
