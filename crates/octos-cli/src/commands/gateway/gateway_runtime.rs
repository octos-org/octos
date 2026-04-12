//! Gateway runtime: initialization and main message loop.
//!
//! Phases:
//! 1. Config & LLM provider setup
//! 2. Data stores & environment
//! 3. Tool registry & plugins
//! 4. Channels & services
//! 5. Main message loop

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use colored::Colorize;
use eyre::{Result, WrapErr};
use octos_agent::{AgentConfig, HookContext, HookExecutor, SkillsLoader, ToolRegistry};
use octos_bus::{
    ActiveSessionStore, ChannelManager, CronService, HeartbeatService, SessionManager, create_bus,
};
use octos_llm::{
    AdaptiveConfig, AdaptiveRouter, BaselineEntry, LlmProvider, ProviderChain, ProviderRouter,
    QosCatalog, RetryProvider, SwappableProvider,
};
use octos_memory::{EpisodeStore, MemoryStore};
use tokio::sync::{Mutex, RwLock, Semaphore};
use tracing::{info, warn};

use super::build_system_prompt;
use super::message_preprocessing;
use super::profile_factory::{ProfileActorFactoryBuilder, build_plugin_env};
use super::{account_handler, adapters, skills_handler};
use super::{build_profiled_session_key, resolve_dispatch_profile_id};
use crate::commands::chat::{self, create_embedder, resolve_provider_policy};
use crate::commands::{load_prompt, resolve_data_dir};
use crate::config::{Config, detect_provider};
use crate::config_watcher::{ConfigChange, ConfigWatcher};
use crate::persona_service::PersonaService;
use crate::session_actor::{ActorFactory, ActorRegistry, SnapshotToolRegistryFactory};
use crate::status_layers::StatusComposer;

#[cfg(feature = "matrix")]
use super::matrix_integration::*;

const PROFILE_PROMPT_CACHE_CAP: usize = 128;

/// Holds all state needed by the gateway's main message loop.
///
/// Constructed by [`init()`](Self::init) from a `GatewayCommand`, then
/// consumed by [`run()`](Self::run) which drives the dispatch loop.
pub(super) struct GatewayRuntime {
    profile_id: Option<String>,
    data_dir: PathBuf,

    // Messaging
    agent_handle: octos_bus::AgentHandle,
    channel_mgr: ChannelManager,

    // ASR / voice
    asr_binary: Option<PathBuf>,
    asr_language: Option<String>,

    // Cron defaults
    default_cron_channel: String,
    default_cron_chat_id: String,

    // Session dispatch
    actor_registry: ActorRegistry,
    session_dispatcher: crate::gateway_dispatcher::GatewayDispatcher,
    profile_factory_builder: Option<ProfileActorFactoryBuilder>,
    profile_store: Option<Arc<crate::profiles::ProfileStore>>,
    active_sessions: Arc<RwLock<ActiveSessionStore>>,

    // Config / hot-reload
    system_prompt: Arc<std::sync::RwLock<String>>,
    max_history: Arc<AtomicUsize>,
    config_rx: tokio::sync::watch::Receiver<Option<ConfigChange>>,
    tool_config: Arc<octos_agent::ToolConfigStore>,
    shutdown: Arc<AtomicBool>,

    // Status
    status_indicators: Arc<HashMap<String, Arc<StatusComposer>>>,

    // Services (for shutdown)
    persona_service: Arc<PersonaService>,
    heartbeat_service: Arc<HeartbeatService>,
    cron_service: Arc<CronService>,

    // Matrix (feature-gated)
    #[cfg(feature = "matrix")]
    matrix_channel: Option<Arc<octos_bus::MatrixChannel>>,
}

fn load_seed_qos_catalog(data_dir: &Path) -> Option<QosCatalog> {
    let candidates = [
        data_dir.join("model_catalog.json"),
        dirs::home_dir()
            .unwrap_or_default()
            .join(".octos/model_catalog.json"),
    ];
    for path in &candidates {
        if let Ok(json) = std::fs::read_to_string(path) {
            if let Ok(catalog) = serde_json::from_str::<QosCatalog>(&json) {
                return Some(catalog);
            }
        }
    }
    None
}

fn persist_qos_catalog(path: &Path, catalog: &QosCatalog) {
    match serde_json::to_string_pretty(catalog) {
        Ok(json) => {
            if let Err(error) = std::fs::write(path, json) {
                warn!(path = %path.display(), %error, "failed to persist runtime model catalog");
            }
        }
        Err(error) => {
            warn!(path = %path.display(), %error, "failed to serialize runtime model catalog");
        }
    }
}

fn materialize_runtime_qos_catalog(
    seed_catalog: Option<&QosCatalog>,
    adaptive_export: Option<QosCatalog>,
    config: &AdaptiveConfig,
    qos_ranking: bool,
) -> Option<QosCatalog> {
    adaptive_export.or_else(|| {
        seed_catalog.map(|catalog| {
            octos_llm::derive_cold_start_catalog(&catalog.models, config, qos_ranking)
        })
    })
}

impl GatewayRuntime {
    /// Initialize the gateway runtime from CLI command arguments.
    ///
    /// Phases: config → LLM → stores → tools → channels → services.
    pub(super) async fn init(cmd: super::GatewayCommand) -> Result<Self> {
        // Use eprintln! for the startup banner so it reaches the server's stderr
        // reader immediately (stderr is unbuffered, unlike piped stdout).
        eprintln!("[gateway] starting");
        println!("{}", "octos gateway".cyan().bold());
        println!();

        let cwd = match cmd.cwd {
            Some(p) => p,
            None => std::env::current_dir().wrap_err("failed to get current directory")?,
        };
        let data_dir = resolve_data_dir(cmd.data_dir.clone())?;

        let mut profile_id: Option<String> = None;
        eprintln!(
            "[gateway] loading config (profile={:?})",
            cmd.profile.as_deref().map(|p| p.display().to_string())
        );
        let mut admin_mode = false;
        let config = if let Some(ref profile_path) = cmd.profile {
            // Load config from profile JSON (single source of truth)
            let content = std::fs::read_to_string(profile_path)
                .wrap_err_with(|| format!("failed to read profile: {}", profile_path.display()))?;
            let mut profile: crate::profiles::UserProfile = serde_json::from_str(&content)
                .wrap_err_with(|| format!("failed to parse profile: {}", profile_path.display()))?;
            profile_id = Some(profile.id.clone());
            admin_mode = profile.config.admin_mode;

            // Sub-account: merge LLM provider config from parent profile
            if let Some(ref parent_path) = cmd.parent_profile {
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
                cmd.bridge_url.as_deref(),
                cmd.feishu_port,
            )
        } else if let Some(config_path) = &cmd.config {
            Config::from_file(config_path)?
        } else {
            Config::load(&cwd, &data_dir)?
        };

        let model = cmd.model.or(config.model.clone());
        let base_url = cmd.base_url.or(config.base_url.clone());
        let provider_name = cmd
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
        let base_provider = chat::create_provider(&provider_name, &config, model, base_url)?;
        eprintln!(
            "[gateway] LLM provider created, model={}",
            base_provider.model_id()
        );

        let model_id = base_provider.model_id().to_string();

        // Build provider chain, keeping a typed reference to AdaptiveRouter
        // (if created) for responsiveness feedback from session actors.
        let mut adaptive_router_ref: Option<Arc<AdaptiveRouter>> = None;

        let llm: Arc<dyn LlmProvider> = if cmd.no_retry {
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
                match chat::create_provider_with_api_type(
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
        let catalog_path = data_dir.join("model_catalog.json");
        let qos_scoring_config = config
            .adaptive_routing
            .as_ref()
            .map(AdaptiveConfig::from)
            .unwrap_or_default();
        let qos_ranking_enabled = config
            .adaptive_routing
            .as_ref()
            .map(|cfg| cfg.qos_ranking)
            .unwrap_or(true);
        let seed_catalog = load_seed_qos_catalog(&data_dir);
        let runtime_qos_catalog: Option<QosCatalog>;

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

            if let Some(ref catalog) = seed_catalog {
                router.seed_catalog(&catalog.models);
                info!(models = catalog.models.len(), "loaded model catalog");
            }

            runtime_qos_catalog = materialize_runtime_qos_catalog(
                seed_catalog.as_ref(),
                Some(router.export_model_catalog()),
                &qos_scoring_config,
                qos_ranking_enabled,
            );
        } else {
            runtime_qos_catalog = materialize_runtime_qos_catalog(
                seed_catalog.as_ref(),
                None,
                &qos_scoring_config,
                qos_ranking_enabled,
            );
        }

        if let Some(ref catalog) = runtime_qos_catalog {
            let ctx_entries: Vec<(String, u64, u64)> = catalog
                .models
                .iter()
                .map(|m| (m.provider.clone(), m.context_window, m.max_output))
                .collect();
            octos_llm::context::seed_from_catalog(&ctx_entries);
            let price_entries: Vec<(String, f64, f64)> = catalog
                .models
                .iter()
                .map(|m| (m.provider.clone(), m.cost_in, m.cost_out))
                .collect();
            octos_llm::pricing::seed_pricing_catalog(&price_entries);
            persist_qos_catalog(&catalog_path, catalog);
        }

        // Open ProfileStore for /account commands and bot management.
        // Derive octos_home from: --octos-home flag > data_dir (which already
        // resolves --data-dir > $OCTOS_HOME > ~/.octos).
        let effective_octos_home = cmd.octos_home.clone().unwrap_or_else(|| data_dir.clone());
        let profile_store: Option<Arc<crate::profiles::ProfileStore>> =
            crate::profiles::ProfileStore::open(&effective_octos_home)
                .ok()
                .map(Arc::new);

        // Export env vars for skill/plugin child processes. These are read by
        // spawned binaries (mofa-fm, account-manager, voice skill), not by
        // concurrent Rust threads. Consolidated here before any tokio::spawn
        // calls to minimize the UB window (tokio worker threads exist but are
        // idle at this point).
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("OCTOS_DATA_DIR", &data_dir);
            std::env::set_var("OCTOS_HOME", &effective_octos_home);
            if let Some(ref pid) = profile_id {
                std::env::set_var("OCTOS_PROFILE_ID", pid);
            }
        }

        // Spawn periodic metrics exporter (writes model_catalog.json every 30s)
        if let Some(ref router) = adaptive_router_ref {
            let metrics_router = router.clone();
            let catalog_path = catalog_path.clone();
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

        #[allow(unused_variables)] // used by feature-gated channel registration
        let media_dir = data_dir.join("media");

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
        let project_dir = if let Some(ref octos_home) = cmd.octos_home {
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
            // Export so the voice skill binary can find the OminiX server.
            // Read by child processes only, not concurrent Rust threads.
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
            if let Some(ref parent_path) = cmd.parent_profile {
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
        let (agent_handle, publisher) = create_bus();

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
        let provider_policy_for_factory: Option<octos_agent::ToolPolicy>;
        let worker_prompt_for_factory: Option<String>;
        let provider_router_for_factory: Option<Arc<ProviderRouter>>;
        let pipeline_factory: Option<
            Arc<dyn crate::session_actor::PipelineToolFactory + Send + Sync>,
        >;

        // Build env vars to inject into plugin processes so skills can route
        // API calls through the configured provider/gateway (e.g. r9s.ai).
        let mut plugin_env = build_plugin_env(&config, &provider_name);
        // Per-profile data_dir so skills (voice profiles, mofa-fm voices, etc.)
        // resolve storage relative to the correct profile, not the gateway root.
        plugin_env.push((
            "OCTOS_DATA_DIR".to_string(),
            data_dir.to_string_lossy().to_string(),
        ));
        plugin_env.push((
            "OCTOS_VOICE_DIR".to_string(),
            data_dir
                .join("voice_profiles")
                .to_string_lossy()
                .to_string(),
        ));
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();
        #[cfg(feature = "matrix")]
        let mut matrix_channel: Option<Arc<octos_bus::MatrixChannel>> = None;

        let mut tools;
        let mut plugin_result;
        let mut sandbox_config = config.sandbox.clone();
        let plugin_dirs_for_spawn: Vec<std::path::PathBuf>;
        {
            // Full tool registration for all modes.
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
                    match chat::create_provider_with_api_type(
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

                    // Register each fallback — use model name as key.
                    // Clone config once outside the loop to avoid per-iteration clones.
                    let base_fb_config = config.clone();
                    let mut key_counts: std::collections::HashMap<String, usize> =
                        std::collections::HashMap::new();
                    for fb in &config.fallback_models {
                        let fb_config = {
                            let mut c = base_fb_config.clone();
                            if fb.api_key_env.is_some() {
                                c.api_key_env = fb.api_key_env.clone();
                            } else if fb.provider != config.provider.as_deref().unwrap_or("") {
                                // Different provider — clear primary's api_key_env so the
                                // registry resolves the correct env var (e.g. OPENAI_API_KEY)
                                c.api_key_env = None;
                            }
                            c
                        };
                        match chat::create_provider_with_api_type(
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
            worker_prompt_for_factory =
                Some(load_prompt("worker", octos_agent::DEFAULT_WORKER_PROMPT));
            provider_router_for_factory = provider_router.clone();

            // Seed QoS scores on the router for fallback ranking
            if let Some(ref router) = provider_router {
                if let Some(ref catalog) = runtime_qos_catalog {
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
                let data_c = data_dir.clone();
                let policy_c = tools.provider_policy().cloned();
                let plugins_c = plugin_dirs.clone();
                let router_c = provider_router.clone();
                let octos_home_c = cmd.octos_home.clone();

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
                cmd.profile.clone(),
            ));
            plugin_dirs_for_spawn = plugin_dirs;
        }

        // admin_mode adds admin API tools on top of the full tool set
        // (profile management, server diagnostics via REST).
        if admin_mode {
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
            info!("admin mode: added admin API tools on top of full tool set");
        }

        // Build system prompt (always the full prompt with persona, memory, skills)
        let system_prompt = build_system_prompt(
            gw_config.system_prompt.as_deref(),
            &data_dir,
            &project_dir,
            &memory_store,
            &skills_loader,
            &tool_config,
        )
        .await;

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
        let max_iterations = cmd.max_iterations.or(config.max_iterations).unwrap_or(50);
        let session_timeout_secs = gw_config
            .session_timeout_secs
            .unwrap_or(octos_agent::DEFAULT_SESSION_TIMEOUT_SECS);
        let agent_config = AgentConfig {
            max_iterations,
            save_episodes: true,
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
            "spawn",
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
                "group:admin",
                "group:sessions",
                "group:web",
                "group:runtime",
                "group:media", // mofa_comic, mofa_slides, mofa_infographic, mofa_cards, fm_tts
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

        // Extract supervisor before consuming tools into the factory snapshot.
        // Used by the API channel's task query callback (gated behind api feature).
        #[cfg(feature = "api")]
        let supervisor = tools.supervisor();

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
            plugin_dirs: plugin_dirs_for_spawn.clone(),
            plugin_extra_env: plugin_env.clone(),
            llm_strong: super::profile_factory::build_strong_chain(
                &config,
                &config
                    .provider
                    .clone()
                    .unwrap_or_else(|| "anthropic".to_string()),
                false,
            )
            .unwrap_or_else(|_| llm_for_compaction.clone()),
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
                    no_retry: cmd.no_retry,
                    sandbox_config: sandbox_config.clone(),
                });

        // Start config watcher for hot-reload
        let watch_paths = {
            let mut paths = Vec::new();
            if let Some(ref p) = cmd.profile {
                paths.push(p.clone());
            } else if let Some(ref p) = cmd.config {
                paths.push(p.clone());
            } else {
                let local = project_dir.join("config.json");
                if local.exists() {
                    paths.push(local);
                }
                let data_dir_config = Config::data_dir_config_path(&data_dir);
                if data_dir_config.exists() {
                    paths.push(data_dir_config);
                }
            }
            paths
        };
        let (config_tx, config_rx) = tokio::sync::watch::channel(None);
        let _watcher_handle = ConfigWatcher::new(watch_paths, config.clone(), config_tx).spawn();

        // Create channel manager and register channels.
        // If --api-port is passed but no Api channel is configured (serve mode
        // auto-allocation), inject a synthetic Api channel entry so the gateway
        // starts an HTTP listener that the serve API can proxy to.
        let mut channels_for_reg = gw_config.channels.clone();
        if cmd.api_port.is_some() && !channels_for_reg.iter().any(|c| c.channel_type == "api") {
            channels_for_reg.push(crate::config::ChannelEntry {
                channel_type: "api".into(),
                allowed_senders: vec![],
                settings: serde_json::json!({}),
            });
        }

        let mut channel_mgr = ChannelManager::new();
        {
            let mut reg_ctx = adapters::ChannelRegistrationCtx {
                shutdown: &shutdown,
                media_dir: &media_dir,
                data_dir: &data_dir,
                session_mgr: &session_mgr,
                gateway_profile_id: profile_id.as_deref(),
                api_port_override: cmd.api_port,
                wechat_bridge_url: cmd.wechat_bridge_url.as_deref(),
                #[cfg(feature = "matrix")]
                matrix_channel: &mut matrix_channel,
            };
            adapters::register_all(&mut channel_mgr, &channels_for_reg, &mut reg_ctx)?;
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

        // Create status indicators for each channel (used for typing + dynamic status).
        // Use channels_for_reg (not gw_config.channels) so the API channel is included.
        let status_words = PersonaService::read_status_words(&data_dir);
        let status_indicators: Arc<HashMap<String, Arc<StatusComposer>>> = {
            let mut map = HashMap::new();
            for entry in &channels_for_reg {
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
        let actor_registry = ActorRegistry::new(
            actor_factory,
            concurrency_semaphore,
            out_tx.clone(),
            pending_messages.clone(),
        );

        // Create session command dispatcher (testable extraction of /new, /s, /sessions, /back, /delete, /soul)
        let session_dispatcher = crate::gateway_dispatcher::GatewayDispatcher::new(
            session_mgr.clone(),
            active_sessions.clone(),
            pending_messages.clone(),
            out_tx.clone(),
        )
        .with_data_dir(data_dir.clone());

        // Drop the original out_tx — factory and registry hold their own clones.
        // This ensures the outbound channel closes properly when actors shut down.
        drop(out_tx);

        // Assemble runtime and hand off to the main loop
        let runtime = Self {
            profile_id,
            data_dir,
            agent_handle,
            channel_mgr,
            asr_binary,
            asr_language,
            default_cron_channel,
            default_cron_chat_id,
            actor_registry,
            session_dispatcher,
            profile_factory_builder,
            profile_store,
            active_sessions,
            system_prompt,
            max_history,
            config_rx,
            tool_config,
            shutdown,
            status_indicators,
            persona_service,
            heartbeat_service,
            cron_service,
            #[cfg(feature = "matrix")]
            matrix_channel,
        };
        Ok(runtime)
    }

    pub(super) async fn run(mut self) -> Result<()> {
        let mut profile_prompt_cache: HashMap<String, Option<String>> = HashMap::new();

        // Main loop: dispatch inbound messages to concurrent tasks
        while let Some(mut inbound) = self.agent_handle.recv_inbound().await {
            if self.shutdown.load(Ordering::Acquire) {
                break;
            }

            // Apply hot-reload config changes (stays on main task)
            if self.config_rx.has_changed().unwrap_or(false) {
                if let Some(change) = self.config_rx.borrow_and_update().clone() {
                    match change {
                        ConfigChange::HotReload {
                            system_prompt,
                            max_history: new_max,
                        } => {
                            if let Some(prompt) = system_prompt {
                                *self
                                    .system_prompt
                                    .write()
                                    .unwrap_or_else(|e| e.into_inner()) = prompt;
                                info!(
                                    "System prompt updated via hot-reload (new actors will use it)"
                                );
                            }
                            if let Some(new_max) = new_max {
                                self.max_history.store(new_max, Ordering::Release);
                                info!("Max history updated to {new_max} via hot-reload");
                            }
                        }
                        ConfigChange::RestartRequired(_) => {
                            // Already logged by ConfigWatcher
                        }
                    }
                }
            }

            // Transcribe audio, separate images, and tag voice metadata.
            let media_result = message_preprocessing::process_media(
                &mut inbound,
                self.asr_binary.as_deref(),
                self.asr_language.as_deref(),
                &self.channel_mgr,
            )
            .await;
            let image_media = media_result.image_media;

            // Route cron-triggered messages to their target channel
            let (reply_channel, reply_chat_id) = message_preprocessing::resolve_reply_target(
                &inbound,
                &self.default_cron_channel,
                &self.default_cron_chat_id,
            );

            let target_profile = inbound
                .metadata
                .get("target_profile_id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let mut dispatch_profile_id = resolve_dispatch_profile_id(
                self.profile_id.as_deref(),
                target_profile.as_deref(),
                self.profile_store.as_deref(),
            )?;
            if let Some(ref pid) = dispatch_profile_id {
                let is_current_gateway_profile = self
                    .profile_id
                    .as_deref()
                    .is_some_and(|current| current == pid);
                if !is_current_gateway_profile && !self.actor_registry.has_profile_factory(pid) {
                    if let Some(ref builder) = self.profile_factory_builder {
                        match builder.build(pid).await {
                            Ok(factory) => self
                                .actor_registry
                                .register_profile_factory(pid.clone(), factory),
                            Err(error) => {
                                tracing::error!(profile_id = %pid, %error, "failed to build profiled actor factory; falling back to main profile");
                                dispatch_profile_id = None;
                            }
                        }
                    } else {
                        dispatch_profile_id = None;
                    }
                }
            }

            // Update dispatcher's profile ID for this message.
            self.session_dispatcher.dispatch_profile_id = dispatch_profile_id.clone();

            // Resolve session key with the current profile-scoped base key only.
            let base_session_key = build_profiled_session_key(
                dispatch_profile_id.as_deref(),
                &inbound.channel,
                &inbound.chat_id,
                "",
            );
            let base_key_str = base_session_key.base_key().to_string();
            let session_key = {
                let store = self.active_sessions.read().await;
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

                if let Some(crate::gateway_dispatcher::DispatchResult::Handled) = self
                    .session_dispatcher
                    .handle_session_callback(
                        &callback_data,
                        callback_message_id.as_deref(),
                        &inbound,
                        &reply_channel,
                        &reply_chat_id,
                        &base_key_str,
                        Some(&self.channel_mgr),
                    )
                    .await
                {
                    continue;
                }

                // Forward other callback data to the agent as a user message
                // so skills can use inline keyboards for interactive menus
                inbound.content = format!("[callback] {callback_data}");
                // Fall through to normal message processing
            }

            let cmd = inbound.content.trim();

            // Dispatch session lifecycle commands (/new, /s, /sessions, /back, /delete)
            if let crate::gateway_dispatcher::DispatchResult::Handled = self
                .session_dispatcher
                .try_dispatch_session_command(
                    cmd,
                    &inbound,
                    &session_key,
                    &reply_channel,
                    &reply_chat_id,
                    &base_key_str,
                )
                .await
            {
                // For API channel: send a completion signal so the SSE stream closes
                // and the web client's assistant message transitions from "streaming" to "complete".
                if reply_channel == "api" {
                    let _ = self
                        .agent_handle
                        .send_outbound(octos_core::OutboundMessage {
                            channel: reply_channel.clone(),
                            chat_id: reply_chat_id.clone(),
                            content: String::new(),
                            reply_to: None,
                            media: vec![],
                            metadata: serde_json::json!({"_completion": true}),
                        })
                        .await;
                }
                continue;
            }

            // Handle /config command inline
            if cmd == "/config" || cmd.starts_with("/config ") {
                let args = cmd.strip_prefix("/config").unwrap_or("").trim();
                let response = self.tool_config.handle_config_command(args).await;
                let _ = self
                    .agent_handle
                    .send_outbound(message_preprocessing::make_reply(
                        &reply_channel,
                        &reply_chat_id,
                        response,
                    ))
                    .await;
                continue;
            }

            // Handle /account command inline — sub-account management
            if cmd == "/account" || cmd.starts_with("/account ") {
                let args = cmd.strip_prefix("/account").unwrap_or("").trim();
                let response = account_handler::handle_account_command(
                    args,
                    self.profile_id.as_deref(),
                    &self.profile_store,
                )
                .await;
                let _ = self
                    .agent_handle
                    .send_outbound(message_preprocessing::make_reply(
                        &reply_channel,
                        &reply_chat_id,
                        response,
                    ))
                    .await;
                continue;
            }

            // Handle /skills command inline — skill management
            if cmd == "/skills" || cmd.starts_with("/skills ") {
                let args = cmd.strip_prefix("/skills").unwrap_or("").trim();
                let response = skills_handler::handle_skills_command(
                    args,
                    self.profile_id.as_deref(),
                    &self.data_dir,
                    &self.profile_store,
                )
                .await;
                let _ = self
                    .agent_handle
                    .send_outbound(message_preprocessing::make_reply(
                        &reply_channel,
                        &reply_chat_id,
                        response,
                    ))
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
                self.status_indicators.get(&reply_channel).cloned()
            };

            let (prompt_override, dispatch_sender_uid) = if let Some(ref pid) = dispatch_profile_id
            {
                let prompt = if self.actor_registry.has_profile_factory(pid) {
                    None
                } else if !profile_prompt_cache.contains_key(pid.as_str()) {
                    let loaded = if let Some(ref store) = self.profile_store {
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
                    if profile_prompt_cache.len() >= PROFILE_PROMPT_CACHE_CAP {
                        profile_prompt_cache.clear();
                    }
                    profile_prompt_cache.insert(pid.clone(), prompt_val.clone());
                    prompt_val
                } else {
                    profile_prompt_cache.get(pid.as_str()).cloned().flatten()
                };

                #[cfg(feature = "matrix")]
                let sender_uid = if let Some(ref mc) = self.matrix_channel {
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

            // Check for session-specific prompt override (e.g. /new slides <name>)
            let prompt_override = if let Some(topic) = session_key.topic() {
                if let Some(session_prompt) =
                    crate::project_templates::read_session_prompt(&self.data_dir, topic)
                {
                    match prompt_override {
                        Some(base) => Some(format!("{base}\n\n{session_prompt}")),
                        None => Some(session_prompt),
                    }
                } else {
                    prompt_override
                }
            } else {
                prompt_override
            };

            // Dispatch to per-session actor (creates one if needed)
            tracing::debug!(
                dispatch_profile_id = ?dispatch_profile_id,
                dispatch_sender_uid = ?dispatch_sender_uid,
                "dispatching to actor"
            );
            self.actor_registry
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
            self.actor_registry.reap_dead_actors();
        }

        // ── Shutdown ────────────────────────────────────────────────────
        // Timeout prevents hung actors from blocking the entire sequence.
        let shutdown_timeout = Duration::from_secs(30);
        if tokio::time::timeout(shutdown_timeout, self.actor_registry.shutdown_all())
            .await
            .is_err()
        {
            warn!("actor shutdown timed out after {shutdown_timeout:?}, forcing exit");
        }

        // Stop background services concurrently
        let (_, _, _, ch_result) = tokio::join!(
            self.persona_service.stop(),
            self.heartbeat_service.stop(),
            self.cron_service.stop(),
            self.channel_mgr.stop_all(),
        );
        ch_result?;
        println!("{}", "Gateway stopped.".dimmed());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use octos_llm::{ModelCatalogEntry, ModelType};
    use tempfile::tempdir;

    fn sample_catalog(scores: [f64; 2]) -> QosCatalog {
        QosCatalog {
            updated_at: "2026-04-11T00:00:00Z".to_string(),
            models: vec![
                ModelCatalogEntry {
                    provider: "zai/glm-5-turbo".to_string(),
                    model_type: ModelType::Fast,
                    stability: 0.97,
                    tool_avg_ms: 900,
                    p95_ms: 1500,
                    score: scores[0],
                    cost_in: 0.5,
                    cost_out: 2.0,
                    ds_output: 1200,
                    context_window: 128_000,
                    max_output: 8_192,
                },
                ModelCatalogEntry {
                    provider: "dashscope/qwen3.5-plus".to_string(),
                    model_type: ModelType::Strong,
                    stability: 0.92,
                    tool_avg_ms: 1400,
                    p95_ms: 2400,
                    score: scores[1],
                    cost_in: 0.8,
                    cost_out: 3.2,
                    ds_output: 800,
                    context_window: 128_000,
                    max_output: 16_384,
                },
            ],
        }
    }

    #[test]
    fn load_seed_qos_catalog_reads_profile_local_catalog() {
        let temp = tempdir().unwrap();
        let data_dir = temp.path().join("profile-data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let path = data_dir.join("model_catalog.json");
        let catalog = sample_catalog([0.0, 0.0]);
        std::fs::write(&path, serde_json::to_string_pretty(&catalog).unwrap()).unwrap();

        let loaded = load_seed_qos_catalog(&data_dir).expect("catalog should load");
        assert_eq!(loaded.models.len(), 2);
        assert_eq!(loaded.models[0].provider, "zai/glm-5-turbo");
        assert_eq!(loaded.models[1].provider, "dashscope/qwen3.5-plus");
    }

    #[test]
    fn persist_qos_catalog_round_trips_runtime_scores() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("model_catalog.json");
        let catalog = sample_catalog([0.21857142857142858, 0.4]);

        persist_qos_catalog(&path, &catalog);

        let json = std::fs::read_to_string(&path).unwrap();
        let loaded: QosCatalog = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.models.len(), 2);
        assert!((loaded.models[0].score - 0.21857142857142858).abs() < 1e-12);
        assert!((loaded.models[1].score - 0.4).abs() < 1e-12);
    }

    #[test]
    fn materialize_runtime_qos_catalog_prefers_adaptive_export() {
        let seed = sample_catalog([0.0, 0.0]);
        let live = sample_catalog([0.21, 0.41]);

        let materialized = materialize_runtime_qos_catalog(
            Some(&seed),
            Some(live.clone()),
            &AdaptiveConfig::default(),
            true,
        )
        .expect("catalog should materialize");

        assert_eq!(materialized.models[0].score, live.models[0].score);
        assert_eq!(materialized.models[1].score, live.models[1].score);
    }

    #[test]
    fn materialize_runtime_qos_catalog_derives_non_zero_scores_from_seed() {
        let seed = sample_catalog([0.0, 0.0]);

        let materialized =
            materialize_runtime_qos_catalog(Some(&seed), None, &AdaptiveConfig::default(), true)
                .expect("catalog should materialize");

        assert_eq!(materialized.models.len(), seed.models.len());
        assert!(materialized.models.iter().all(|entry| entry.score > 0.0));
    }
}
