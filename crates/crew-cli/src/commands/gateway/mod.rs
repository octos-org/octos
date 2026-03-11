//! Gateway command: run as a persistent messaging daemon.

mod account_handler;
mod prompt;
mod session_ui;
mod skills_handler;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use clap::Args;
use colored::Colorize;
use crew_agent::{AgentConfig, HookContext, HookExecutor, SkillsLoader, ToolRegistry};
use crew_bus::{
    ActiveSessionStore, ChannelManager, CliChannel, CronService, HeartbeatService, SessionManager,
    create_bus, validate_topic_name,
};
use crew_core::{OutboundMessage, SessionKey};
use crew_llm::{
    AdaptiveConfig, AdaptiveRouter, LlmProvider, ProviderChain, ProviderRouter, RetryProvider,
    SwappableProvider,
};
use crew_memory::{EpisodeStore, MemoryStore};
use eyre::{Result, WrapErr};
use tokio::sync::{Mutex, Semaphore};
use tracing::{info, warn};

use super::Executable;
use crate::commands::chat::{create_embedder, resolve_provider_policy};
use crate::config::{Config, detect_provider};
use crate::config_watcher::{ConfigChange, ConfigWatcher};
use crate::persona_service::PersonaService;
use crate::session_actor::{ActorFactory, ActorRegistry, SnapshotToolRegistryFactory};
use crate::status_indicator::StatusIndicator;

// Re-export for use by prompt module
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
    feature = "wecom-bot"
))]
use prompt::settings_str;

/// Run as a persistent gateway daemon.
#[derive(Debug, Args)]
pub struct GatewayCommand {
    /// Working directory (defaults to current directory).
    #[arg(short, long)]
    pub cwd: Option<PathBuf>,

    /// Data directory for episodes, memory, sessions (defaults to $CREW_HOME or ~/.crew).
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

    /// Crew home directory for ProfileStore access (used by managed gateways).
    #[arg(long, hide = true)]
    pub crew_home: Option<PathBuf>,
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
        println!("{}", "crew gateway".cyan().bold());
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
                    Ok(p) => providers.push(Arc::new(RetryProvider::new(p))),
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
                    .unwrap_or(crew_llm::AdaptiveMode::Hedge);
                let qos = ar_config.map(|c| c.qos_ranking).unwrap_or(true);
                let router = Arc::new(
                    AdaptiveRouter::new(providers, adaptive_config).with_adaptive_config(mode, qos),
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

        // Resolve data directory (--data-dir > $CREW_HOME > ~/.crew)
        let data_dir = super::resolve_data_dir(self.data_dir)?;

        // Expose data_dir to skill binaries (e.g. mofa-fm voice storage)
        // SAFETY: called before spawning any threads; single-threaded at this point
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("CREW_DATA_DIR", &data_dir);
        }

        // Open ProfileStore for /account commands (if crew-home is available)
        let profile_store: Option<Arc<crate::profiles::ProfileStore>> =
            if let Some(ref crew_home) = self.crew_home {
                crate::profiles::ProfileStore::open(crew_home)
                    .ok()
                    .map(Arc::new)
            } else {
                None
            };

        // Export CREW_HOME and CREW_PROFILE_ID so plugin tools (e.g. account-manager)
        // can access the profile store and know which profile is running.
        // SAFETY: gateway is single-threaded at this point (before tokio tasks spawn).
        #[allow(unsafe_code)]
        unsafe {
            if let Some(ref crew_home) = self.crew_home {
                std::env::set_var("CREW_HOME", crew_home);
            }
            if let Some(ref pid) = profile_id {
                std::env::set_var("CREW_PROFILE_ID", pid);
            }
        }

        // Spawn periodic metrics exporter (writes provider_metrics.json every 30s)
        if llm.export_metrics().is_some() {
            let metrics_llm = llm.clone();
            let metrics_path = data_dir.join("provider_metrics.json");
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
                loop {
                    interval.tick().await;
                    if let Some(value) = metrics_llm.export_metrics() {
                        if let Ok(json) = serde_json::to_string_pretty(&value) {
                            let _ = tokio::fs::write(&metrics_path, json).await;
                        }
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

        // Initialize skills loader (project-level, from cwd/.crew/)
        let project_dir = cwd.join(".crew");

        // Bootstrap bundled app-skills and platform skills into layered dirs
        let n = crew_agent::bootstrap::bootstrap_bundled_skills(&project_dir);
        if n > 0 {
            info!(count = n, "bootstrapped bundled app-skills");
        }
        let n = crew_agent::bootstrap::bootstrap_platform_skills(&project_dir);
        if n > 0 {
            info!(count = n, "bootstrapped platform skills");
        }

        // Voice transcription via voice platform skill binary (after bootstrap)
        let voice_binary_path = project_dir
            .join(crew_agent::bootstrap::PLATFORM_SKILLS_DIR)
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
            .add_skills_path(project_dir.join(crew_agent::bootstrap::BUNDLED_APP_SKILLS_DIR));
        // Extra skills dirs from CREW_SKILLS_PATH env var
        if let Ok(extra) = std::env::var("CREW_SKILLS_PATH") {
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
            crew_bus::heartbeat::DEFAULT_INTERVAL_SECS,
        ));
        heartbeat_service.start();

        // Build tool registry — admin mode gets only admin API tools + messaging
        let tool_config = Arc::new(
            crew_agent::ToolConfigStore::open(&data_dir)
                .await
                .wrap_err("failed to open tool config store")?,
        );

        // Session-specific tools (message, send_file, spawn, cron, pipeline)
        // are NOT registered in the base registry — they are created per-session
        // by the ActorFactory to eliminate the set_context() race condition.

        // Store config needed for per-session tool creation
        let mut provider_policy_for_factory: Option<crew_agent::ToolPolicy> = None;
        let mut worker_prompt_for_factory: Option<String> = None;
        let mut provider_router_for_factory: Option<Arc<ProviderRouter>> = None;
        let mut pipeline_factory: Option<
            Arc<dyn crate::session_actor::PipelineToolFactory + Send + Sync>,
        > = None;

        let mut tools;
        if admin_mode {
            // Admin mode: register only admin API tools
            tools = ToolRegistry::new();

            // Register admin API tools (calls REST API on crew serve)
            let serve_url = std::env::var("CREW_SERVE_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:8080".to_string());
            let admin_token = std::env::var("CREW_ADMIN_TOKEN").unwrap_or_default();
            let admin_ctx = Arc::new(crew_agent::AdminApiContext {
                http: reqwest::Client::new(),
                serve_url,
                admin_token,
            });
            crew_agent::register_admin_api_tools(&mut tools, admin_ctx);

            // Session-specific tools (cron, message, send_file) are created
            // per-session by the ActorFactory — not registered in base registry.

            // Shell tool for direct server access (diagnostics, troubleshooting)
            tools.register(crew_agent::ShellTool::new(&cwd));

            // Memory bank tools
            tools.register(crew_agent::RecallMemoryTool::new(memory_store.clone()));
            tools.register(crew_agent::SaveMemoryTool::new(memory_store.clone()));

            // Load only admin-relevant plugins (not all bundled skills)
            let admin_skills: &[&str] = &["send-email", "account-manager"];
            let bundled_dir = cwd
                .join(".crew")
                .join(crew_agent::bootstrap::BUNDLED_APP_SKILLS_DIR);
            for skill_name in admin_skills {
                let skill_dir = bundled_dir.join(skill_name);
                if skill_dir.exists() {
                    match crew_agent::PluginLoader::load_plugin(&skill_dir) {
                        Ok(plugin_tools) => {
                            for t in plugin_tools {
                                tools.register(t);
                            }
                        }
                        Err(e) => warn!("admin plugin {skill_name} failed: {e}"),
                    }
                }
            }

            info!("admin mode: registered admin API + shell + memory + plugin tools");
        } else {
            // Normal mode: full tool registration
            let sandbox = crew_agent::create_sandbox(&config.sandbox);
            tools = ToolRegistry::with_builtins_and_sandbox(&cwd, sandbox);
            tools.inject_tool_config(tool_config.clone());

            // Override browser tool with configured timeout (replaces default 300s)
            if let Some(secs) = gw_config.browser_timeout_secs {
                tools.register(
                    crew_agent::BrowserTool::with_timeout(std::time::Duration::from_secs(secs))
                        .with_config(tool_config.clone()),
                );
            }

            // Register MCP tools
            if !config.mcp_servers.is_empty() {
                match crew_agent::McpClient::start(&config.mcp_servers).await {
                    Ok(client) => client.register_tools(&mut tools),
                    Err(e) => warn!("MCP initialization failed: {e}"),
                }
            }

            // Load plugins
            let plugin_dirs = crate::config::Config::plugin_dirs(&cwd);
            if !plugin_dirs.is_empty() {
                if let Err(e) = crew_agent::PluginLoader::load_into(&mut tools, &plugin_dirs) {
                    warn!("plugin loading failed: {e}");
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

            // Build sub-provider router from config
            let provider_router = if !config.sub_providers.is_empty() {
                let router = Arc::new(ProviderRouter::new());
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
                            router.register_with_meta(
                                &sp.key,
                                Arc::new(RetryProvider::new(p)),
                                sp.description.clone(),
                                sp.default_context_window,
                            );
                            println!(
                                "  {}: {}/{}",
                                "Sub-provider".green(),
                                sp.key,
                                sp.model.as_deref().unwrap_or("default")
                            );
                        }
                        Err(e) => {
                            warn!(key = %sp.key, provider = %sp.provider, error = %e, "skipping sub-provider");
                        }
                    }
                }
                Some(router)
            } else {
                None
            };

            // Capture config for per-session SpawnTool and PipelineTool creation
            provider_policy_for_factory = tools.provider_policy().cloned();
            worker_prompt_for_factory = Some(super::load_prompt(
                "worker",
                crew_agent::DEFAULT_WORKER_PROMPT,
            ));
            provider_router_for_factory = provider_router.clone();

            // Skill management tool (install/remove/search skills for this profile)
            tools.register(crew_agent::ManageSkillsTool::new(data_dir.join("skills")));

            // Research synthesis tool (shared, no per-session state)
            tools.register(crew_agent::SynthesizeResearchTool::new(
                llm.clone(),
                data_dir.clone(),
            ));

            // Pipeline tool factory for per-session instances
            {
                let llm_c = llm.clone();
                let mem_c = memory.clone();
                let cwd_c = cwd.clone();
                let data_c = data_dir.clone();
                let policy_c = tools.provider_policy().cloned();
                let plugins_c = plugin_dirs.clone();
                let router_c = provider_router.clone();
                let crew_home_c = self.crew_home.clone();

                struct DefaultPipelineToolFactory {
                    llm: Arc<dyn LlmProvider>,
                    memory: Arc<crew_memory::EpisodeStore>,
                    cwd: PathBuf,
                    data_dir: PathBuf,
                    policy: Option<crew_agent::ToolPolicy>,
                    plugin_dirs: Vec<PathBuf>,
                    router: Option<Arc<ProviderRouter>>,
                    crew_home: Option<PathBuf>,
                }

                impl crate::session_actor::PipelineToolFactory for DefaultPipelineToolFactory {
                    fn create(&self) -> Arc<dyn crew_agent::Tool> {
                        let mut pt = crew_pipeline::RunPipelineTool::new(
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
                        if let Some(ref crew_home) = self.crew_home {
                            pt = pt.with_crew_home(crew_home.clone());
                        }
                        Arc::new(pt)
                    }
                }

                pipeline_factory = Some(Arc::new(DefaultPipelineToolFactory {
                    llm: llm_c,
                    memory: mem_c,
                    cwd: cwd_c,
                    data_dir: data_c,
                    policy: policy_c,
                    plugin_dirs: plugins_c,
                    router: router_c,
                    crew_home: crew_home_c,
                })
                    as Arc<dyn crate::session_actor::PipelineToolFactory + Send + Sync>);
            }

            // Memory bank tools
            tools.register(crew_agent::RecallMemoryTool::new(memory_store.clone()));
            tools.register(crew_agent::SaveMemoryTool::new(memory_store.clone()));

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

        // Shared system prompt for hot-reload (factory reads this at actor spawn time)
        let system_prompt = Arc::new(std::sync::RwLock::new(system_prompt));

        // Build agent config (shared by all per-session agents)
        let max_iterations = self.max_iterations.or(config.max_iterations).unwrap_or(50);
        let agent_config = AgentConfig {
            max_iterations,
            save_episodes: false,
            tool_timeout_secs: gw_config
                .tool_timeout_secs
                .unwrap_or(crew_agent::DEFAULT_TOOL_TIMEOUT_SECS),
            ..Default::default()
        };
        let session_timeout_secs = gw_config
            .session_timeout_secs
            .unwrap_or(crew_agent::DEFAULT_SESSION_TIMEOUT_SECS);

        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();

        let llm_for_compaction = llm.clone();

        // Build hook executor and context template
        let hooks = if !config.hooks.is_empty() {
            Some(Arc::new(HookExecutor::new(config.hooks.clone())))
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
        let active_sessions = Arc::new(Mutex::new(
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
            memory,
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
            sandbox_config: config.sandbox.clone(),
            provider_policy: provider_policy_for_factory,
            worker_prompt: worker_prompt_for_factory,
            provider_router: provider_router_for_factory,
            embedder: create_embedder(&config).map(|e| e as Arc<dyn crew_llm::EmbeddingProvider>),
            active_sessions: active_sessions.clone(),
            pending_messages: pending_messages.clone(),
            queue_mode: gw_config.queue_mode,
            adaptive_router: adaptive_router_ref,
            memory_store: Some(memory_store.clone()),
        };

        // Start config watcher for hot-reload
        let watch_paths = {
            let mut paths = Vec::new();
            if let Some(ref p) = self.profile {
                paths.push(p.clone());
            } else if let Some(ref p) = self.config {
                paths.push(p.clone());
            } else {
                let local = cwd.join(".crew").join("config.json");
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
                    let mut tg = crew_bus::TelegramChannel::new(
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
                    channel_mgr.register(Arc::new(crew_bus::DiscordChannel::new(
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
                    channel_mgr.register(Arc::new(crew_bus::SlackChannel::new(
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
                    channel_mgr.register(Arc::new(crew_bus::WhatsAppChannel::new(
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

                    let email_config = crew_bus::email_channel::EmailConfig {
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
                    channel_mgr.register(Arc::new(crew_bus::EmailChannel::new(
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
                        crew_bus::FeishuChannel::new(
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
                    channel_mgr.register(Arc::new(crew_bus::TwilioChannel::new(
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
                        crew_bus::WeComChannel::new(
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
                    channel_mgr.register(Arc::new(crew_bus::ApiChannel::new(
                        port,
                        auth_token,
                        shutdown.clone(),
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
                    channel_mgr.register(Arc::new(crew_bus::WeComBotChannel::new(
                        &bot_id,
                        &secret,
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
        let status_indicators: Arc<HashMap<String, Arc<StatusIndicator>>> = {
            let mut map = HashMap::new();
            for entry in &gw_config.channels {
                if let Some(ch) = channel_mgr.get_channel(&entry.channel_type) {
                    map.insert(
                        entry.channel_type.clone(),
                        Arc::new(StatusIndicator::new(ch, status_words.clone())),
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
                    if crew_bus::media::is_audio(path) {
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
                    } else if crew_bus::media::is_image(path) {
                        image_media.push(path.clone());
                    }
                }
            } else {
                // Check for audio even without transcriber (for voice_message flag)
                for path in &inbound.media {
                    if crew_bus::media::is_audio(path) {
                        is_voice_message = true;
                    } else if crew_bus::media::is_image(path) {
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

            // Resolve session key with active topic
            let base_session_key = inbound.session_key();
            let base_key_str = base_session_key.0.clone();
            let session_key = {
                let store = active_sessions.lock().await;
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
                        .lock()
                        .await
                        .switch_to(&base_key_str, topic)
                        .unwrap_or_else(|e| warn!("switch_to failed: {e}"));

                    // Rebuild keyboard with updated active marker
                    let entries = session_mgr
                        .lock()
                        .await
                        .list_sessions_for_chat(&base_key_str);
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
                            .lock()
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
                        .lock()
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
                    let target_key = SessionKey::new(&inbound.channel, &inbound.chat_id);
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
                        .lock()
                        .await
                        .switch_to(&base_key_str, name)
                        .unwrap_or_else(|e| warn!("switch_to failed: {e}"));

                    // Show last 2 messages as context preview
                    let new_key = SessionKey::with_topic(&inbound.channel, &inbound.chat_id, name);
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
                let entries = session_mgr
                    .lock()
                    .await
                    .list_sessions_for_chat(&base_key_str);
                let active_topic = active_sessions
                    .lock()
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

            // Handle /back command — switch to previous session
            if cmd == "/back" {
                let result = active_sessions.lock().await.go_back(&base_key_str);
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
                        let target_key =
                            SessionKey::with_topic(&inbound.channel, &inbound.chat_id, &topic);
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
                    let del_key = SessionKey::with_topic(&inbound.channel, &inbound.chat_id, name);
                    match session_mgr.lock().await.clear(&del_key).await {
                        Ok(()) => {
                            active_sessions
                                .lock()
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

            // Dispatch to per-session actor (creates one if needed)
            actor_registry
                .dispatch(
                    inbound,
                    image_media,
                    session_key,
                    &reply_channel,
                    &reply_chat_id,
                    status_indicator,
                )
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
    messages: Vec<crew_core::InboundMessage>,
) -> Vec<crew_core::InboundMessage> {
    use std::collections::BTreeMap;
    let mut groups: BTreeMap<String, Vec<crew_core::InboundMessage>> = BTreeMap::new();
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
