//! Serve command: start the REST API server.

use std::path::PathBuf;
use std::sync::Arc;

use clap::Args;
use colored::Colorize;
use eyre::{Result, WrapErr};
use octos_agent::{Agent, AgentConfig, HookExecutor, ToolRegistry};
use octos_bus::SessionManager;
use octos_core::AgentId;
use octos_llm::{LlmProvider, RetryProvider};
use octos_memory::{EpisodeStore, MemoryStore};

use super::Executable;
use super::chat::create_provider;
use crate::api::metrics::MetricsReporter;
use crate::api::{AppState, SseBroadcaster, build_router, init_metrics};
use crate::config::Config;

/// Start the REST API server.
#[derive(Debug, Args)]
pub struct ServeCommand {
    /// Port to listen on.
    #[arg(short, long, default_value = "8080")]
    pub port: u16,

    /// Host address to bind to. Defaults to localhost for security.
    /// Use 0.0.0.0 to accept connections from all interfaces.
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,

    /// Working directory (defaults to current directory).
    #[arg(short, long)]
    pub cwd: Option<PathBuf>,

    /// Data directory for episodes, memory, sessions (defaults to $OCTOS_HOME or ~/.octos).
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

    /// Auth token for API access (overrides config).
    #[arg(long)]
    pub auth_token: Option<String>,

    /// Disable automatic retry on transient errors.
    #[arg(long)]
    pub no_retry: bool,
}

impl Executable for ServeCommand {
    fn execute(self) -> Result<()> {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .wrap_err("failed to create tokio runtime")?
            .block_on(self.run_async())
    }
}

impl ServeCommand {
    async fn run_async(self) -> Result<()> {
        let cwd = match &self.cwd {
            Some(p) => p.clone(),
            None => std::env::current_dir().wrap_err("failed to get current directory")?,
        };

        let (config, resolved_config_path) = if let Some(config_path) = &self.config {
            (Config::from_file(config_path)?, Some(config_path.clone()))
        } else {
            // Resolve config path the same way Config::load does
            let local_config = cwd.join(".octos").join("config.json");
            if local_config.exists() {
                (Config::from_file(&local_config)?, Some(local_config))
            } else if let Some(global_config) = Config::global_config_path() {
                if global_config.exists() {
                    (Config::from_file(&global_config)?, Some(global_config))
                } else {
                    (Config::default(), None)
                }
            } else {
                (Config::default(), None)
            }
        };

        // Resolve data directory (--data-dir > $OCTOS_HOME > ~/.octos)
        let data_dir = super::resolve_data_dir(self.data_dir.clone())?;
        tracing::info!(data_dir = %data_dir.display(), "data directory resolved");

        let broadcaster = Arc::new(SseBroadcaster::new(256));

        // Try to create the LLM provider + agent, but don't fail if no API key.
        // The admin dashboard works without it.
        let agent_and_sessions = self
            .try_create_agent(&config, &cwd, &data_dir, broadcaster.clone())
            .await;

        let (agent, sessions) = match agent_and_sessions {
            Ok((a, s)) => (Some(Arc::new(a)), Some(s)),
            Err(e) => {
                tracing::warn!("LLM agent not available: {e}");
                tracing::info!("Admin dashboard will still work. Configure profiles via /admin/");
                (None, None)
            }
        };
        let metrics_handle = Some(init_metrics());

        // Security: warn if binding to non-localhost without auth token
        // Check CLI arg, then OCTOS_AUTH_TOKEN env var
        let auth_token = if self.auth_token.is_some() {
            self.auth_token
        } else if let Ok(env_token) = std::env::var("OCTOS_AUTH_TOKEN") {
            Some(env_token)
        } else if let Some(ref cfg_token) = config.auth_token {
            if !cfg_token.is_empty() {
                Some(cfg_token.clone())
            } else {
                None
            }
        } else if self.host != "127.0.0.1" && self.host != "localhost" && self.host != "::1" {
            tracing::warn!(
                "Binding to {} without --auth-token is dangerous! \
                 Generating a random token for this session.",
                self.host
            );
            // Generate cryptographically random token
            use rand::Rng;
            let mut rng = rand::thread_rng();
            let a: u64 = rng.r#gen();
            let b: u64 = rng.r#gen();
            let token = format!("{a:016x}{b:016x}");
            println!(
                "{}: {} (auto-generated, pass --auth-token to set your own)",
                "Auth token".yellow(),
                token
            );
            Some(token)
        } else {
            None
        };

        // Initialize profile store and process manager for admin dashboard
        tracing::info!("initializing profile store and process manager");
        let profile_store = Arc::new(
            crate::profiles::ProfileStore::open(&data_dir)
                .wrap_err("failed to open profile store")?,
        );
        let bridge_js_path = data_dir.join("whatsapp-bridge").join("bridge.js");
        let process_manager = Arc::new(
            crate::process_manager::ProcessManager::new(profile_store.clone())
                .with_bridge_js(bridge_js_path)
                .with_serve_config(self.port, auth_token.clone()),
        );
        process_manager.set_self_ref();

        // Initialize user store and auth manager for multi-user support
        let user_store = Arc::new(
            crate::user_store::UserStore::open(&data_dir).wrap_err("failed to open user store")?,
        );
        let auth_manager = {
            let auth_config = config.dashboard_auth.clone();
            if auth_config.is_none() {
                tracing::warn!(
                    "no dashboard_auth.smtp configured — OTP codes will be logged to console only"
                );
            }
            let mut mgr = crate::otp::AuthManager::new(auth_config.clone(), user_store.clone())
                .with_sessions_path(data_dir.join("auth_sessions.json"));

            // Resolve SMTP password from profile env_vars as fallback
            // (covers nohup startup where LaunchAgent env vars aren't available)
            if let Some(ref auth_cfg) = auth_config {
                let pw_env = &auth_cfg.smtp.password_env;
                if std::env::var(pw_env).is_err() {
                    let profiles_for_smtp = profile_store.list().unwrap_or_default();
                    for p in &profiles_for_smtp {
                        if let Some(pw) = p.config.env_vars.get(pw_env) {
                            if pw == crate::auth::keychain::KEYCHAIN_MARKER {
                                // Resolve from keychain
                                if let Ok(Some(secret)) = crate::auth::keychain::get_secret(pw_env)
                                {
                                    tracing::info!(
                                        var = %pw_env,
                                        "SMTP password resolved from keychain"
                                    );
                                    mgr = mgr.with_smtp_password(secret);
                                    break;
                                }
                            } else if !pw.is_empty() {
                                tracing::info!(
                                    var = %pw_env,
                                    profile = %p.id,
                                    "SMTP password resolved from profile env_vars"
                                );
                                mgr = mgr.with_smtp_password(pw.clone());
                                break;
                            }
                        }
                    }
                }
            }

            Some(Arc::new(mgr))
        };

        // Spawn auth cleanup task if auth manager is active
        if let Some(ref am) = auth_manager {
            let am_clone = am.clone();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(300));
                loop {
                    interval.tick().await;
                    am_clone.cleanup().await;
                }
            });
        }

        // Pre-create watchdog/alerts flags for both Monitor and AppState
        let (watchdog_flag, alerts_flag) = {
            let wf = config
                .monitor
                .as_ref()
                .map(|m| Arc::new(std::sync::atomic::AtomicBool::new(m.watchdog_enabled)));
            let af = config
                .monitor
                .as_ref()
                .map(|m| Arc::new(std::sync::atomic::AtomicBool::new(m.alerts_enabled)));
            (wf, af)
        };

        let state = Arc::new(AppState {
            agent,
            sessions,
            broadcaster,
            started_at: chrono::Utc::now(),
            auth_token,
            metrics_handle,
            profile_store: Some(profile_store.clone()),
            process_manager: Some(process_manager.clone()),
            user_store: Some(user_store),
            auth_manager,
            http_client: reqwest::Client::new(),
            config_path: resolved_config_path,
            watchdog_enabled: watchdog_flag.clone(),
            alerts_enabled: alerts_flag.clone(),
            sysinfo: tokio::sync::Mutex::new(sysinfo::System::new_all()),
            tenant_store: crate::tenant::TenantStore::open(&data_dir)
                .ok()
                .map(Arc::new),
            tunnel_domain: std::env::var("TUNNEL_DOMAIN").ok(),
            frps_server: std::env::var("FRPS_SERVER").ok(),
            frps_port: std::env::var("FRPS_PORT").ok().and_then(|p| p.parse().ok()),
        });

        // Auto-start enabled profiles
        let profiles = profile_store.list().unwrap_or_default();
        let enabled_count = profiles.iter().filter(|p| p.enabled).count();
        tracing::info!(
            total = profiles.len(),
            enabled = enabled_count,
            "loaded profiles"
        );
        if enabled_count > 0 {
            for p in &profiles {
                if p.enabled {
                    tracing::info!(profile = %p.id, "auto-starting gateway");
                    if let Err(e) = process_manager.start(p).await {
                        tracing::warn!(profile = %p.id, error = %e, "failed to auto-start gateway");
                    }
                }
            }
        }

        // Profile file watcher: auto-restart gateways when profile JSON changes.
        {
            let ps = profile_store.clone();
            let pm = process_manager.clone();
            tokio::spawn(async move {
                use crate::profiles::{ProfileChange, UserProfile, diff_profiles};
                use sha2::{Digest, Sha256};
                use std::collections::HashMap;

                // Snapshot of known profile states: (hash, profile)
                let mut known: HashMap<String, ([u8; 32], UserProfile)> = HashMap::new();
                // Seed with current profiles
                if let Ok(list) = ps.list() {
                    for p in list {
                        if let Ok(bytes) = std::fs::read(ps.profile_path(&p.id)) {
                            let hash: [u8; 32] = Sha256::digest(&bytes).into();
                            known.insert(p.id.clone(), (hash, p));
                        }
                    }
                }

                // NOTE(#149): The 5-second poll interval is hardcoded. This could be made
                // configurable (e.g. via a CLI flag or config field) for deployments that
                // need faster detection or want to reduce filesystem polling overhead.
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
                loop {
                    interval.tick().await;
                    let current = match ps.list() {
                        Ok(list) => list,
                        Err(_) => continue,
                    };
                    for profile in &current {
                        let bytes = match std::fs::read(ps.profile_path(&profile.id)) {
                            Ok(b) => b,
                            Err(_) => continue,
                        };
                        let hash: [u8; 32] = Sha256::digest(&bytes).into();

                        if let Some((old_hash, old_profile)) = known.get(&profile.id) {
                            if hash == *old_hash {
                                continue; // no change
                            }
                            let status = pm.status(&profile.id).await;

                            // Handle enable/disable transitions
                            if !old_profile.enabled && profile.enabled && !status.running {
                                // disabled → enabled: start gateway
                                tracing::info!(
                                    profile = %profile.id,
                                    "profile enabled, starting gateway"
                                );
                                if let Err(e) = pm.start(profile).await {
                                    tracing::warn!(
                                        profile = %profile.id,
                                        error = %e,
                                        "failed to start gateway after enable"
                                    );
                                }
                            } else if old_profile.enabled && !profile.enabled && status.running {
                                // enabled → disabled: stop gateway
                                tracing::info!(
                                    profile = %profile.id,
                                    "profile disabled, stopping gateway"
                                );
                                if let Err(e) = pm.stop(&profile.id).await {
                                    tracing::warn!(
                                        profile = %profile.id,
                                        error = %e,
                                        "failed to stop gateway after disable"
                                    );
                                }
                            } else if status.running {
                                // Config changed while running — check if restart needed
                                match diff_profiles(old_profile, profile) {
                                    ProfileChange::RestartRequired(fields) => {
                                        tracing::info!(
                                            profile = %profile.id,
                                            fields = ?fields,
                                            "profile changed (restart-required fields), restarting gateway"
                                        );
                                        if let Err(e) = pm.restart(profile).await {
                                            tracing::warn!(
                                                profile = %profile.id,
                                                error = %e,
                                                "failed to restart gateway after profile change"
                                            );
                                        }
                                    }
                                    ProfileChange::HotReloadable => {
                                        tracing::debug!(
                                            profile = %profile.id,
                                            "profile changed (hot-reloadable only), gateway watcher will handle"
                                        );
                                    }
                                    ProfileChange::Unchanged => {}
                                }
                            } else if profile.enabled && !status.running {
                                // Profile changed & enabled but not running — start it
                                tracing::info!(
                                    profile = %profile.id,
                                    "profile changed and enabled but not running, starting gateway"
                                );
                                if let Err(e) = pm.start(profile).await {
                                    tracing::warn!(
                                        profile = %profile.id,
                                        error = %e,
                                        "failed to start gateway"
                                    );
                                }
                            }
                        } else if profile.enabled {
                            // New profile detected — auto-start its gateway
                            tracing::info!(
                                profile = %profile.id,
                                "new profile detected, starting gateway"
                            );
                            if let Err(e) = pm.start(profile).await {
                                tracing::warn!(
                                    profile = %profile.id,
                                    error = %e,
                                    "failed to auto-start gateway for new profile"
                                );
                            }
                        }
                        known.insert(profile.id.clone(), (hash, profile.clone()));
                    }
                }
            });
        }

        // Start monitor (watchdog + health checks + alerts)
        {
            use crate::monitor::{FeishuAlertSender, Monitor, TelegramAlertSender};
            use std::sync::atomic::AtomicBool;
            use std::time::Duration;

            let monitor_cfg = config.monitor.clone();

            if let Some(ref mon_cfg) = monitor_cfg {
                let shutdown = Arc::new(AtomicBool::new(false));
                let (alert_tx, alert_rx) = tokio::sync::mpsc::channel(256);

                // Use shared flags from AppState
                let watchdog_enabled = watchdog_flag
                    .clone()
                    .unwrap_or_else(|| Arc::new(AtomicBool::new(mon_cfg.watchdog_enabled)));
                let alerts_enabled = alerts_flag
                    .clone()
                    .unwrap_or_else(|| Arc::new(AtomicBool::new(mon_cfg.alerts_enabled)));

                // Wire alert sender into process manager
                process_manager.set_alert_sender(alert_tx);

                let mut monitor = Monitor::new(
                    profile_store.clone(),
                    process_manager.clone(),
                    alert_rx,
                    watchdog_enabled.clone(),
                    alerts_enabled.clone(),
                    mon_cfg.max_restart_attempts,
                    Duration::from_secs(mon_cfg.health_check_interval_secs),
                    shutdown,
                );

                // Add Telegram alert sender if configured
                if let Some(ref token_env) = mon_cfg.telegram_token_env {
                    if let Ok(token) = std::env::var(token_env) {
                        if !mon_cfg.telegram_alert_chat_ids.is_empty() {
                            monitor.add_sender(Box::new(TelegramAlertSender::new(
                                token,
                                mon_cfg.telegram_alert_chat_ids.clone(),
                            )));
                        }
                    }
                }

                // Add Feishu alert sender if configured
                if let Some(ref app_id_env) = mon_cfg.feishu_app_id_env {
                    if let Ok(app_id) = std::env::var(app_id_env) {
                        let secret_env = mon_cfg
                            .feishu_app_secret_env
                            .as_deref()
                            .unwrap_or("FEISHU_APP_SECRET");
                        if let Ok(app_secret) = std::env::var(secret_env) {
                            if !mon_cfg.feishu_alert_user_ids.is_empty() {
                                monitor.add_sender(Box::new(FeishuAlertSender::new(
                                    app_id,
                                    app_secret,
                                    mon_cfg.feishu_alert_user_ids.clone(),
                                    "cn",
                                )));
                            }
                        }
                    }
                }

                tokio::spawn(async move { monitor.run().await });
                tracing::info!("monitor started (watchdog + health checks + alerts)");
            }
        }

        let app = build_router(state);
        let addr = format!("{}:{}", self.host, self.port);

        tracing::info!(address = %addr, "octos API server starting");
        tracing::info!(dashboard = %format!("http://{}/admin/", addr), "dashboard available");
        if enabled_count > 0 {
            tracing::info!(count = enabled_count, "gateway profiles auto-started");
        }

        println!("{}", "octos API server".cyan().bold());
        println!("{}: http://{}", "Listening".green(), addr);
        println!("{}: http://{}/admin/", "Dashboard".green(), addr);
        if enabled_count > 0 {
            println!(
                "{}: {} profiles auto-started",
                "Gateways".green(),
                enabled_count
            );
        }
        println!();

        let listener = tokio::net::TcpListener::bind(&addr).await?;
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = tokio::signal::ctrl_c().await;
                println!();
                println!("{}", "Shutting down server...".yellow());
            })
            .await?;

        // Stop all gateway child processes before exiting
        tracing::info!("stopping all gateway child processes");
        println!("{}", "Stopping gateways...".yellow());
        let stopped = process_manager.stop_all().await;
        if stopped > 0 {
            tracing::info!(count = stopped, "gateways stopped");
            println!("  stopped {} gateway(s)", stopped);
        }

        // Force exit — background tokio tasks (profile watcher, auth cleanup,
        // admin bot) have no shutdown signal and would hang indefinitely.
        std::process::exit(0);
    }

    /// Try to create an Agent + SessionManager. Returns Err if API key is missing etc.
    async fn try_create_agent(
        &self,
        config: &Config,
        cwd: &std::path::Path,
        data_dir: &std::path::Path,
        broadcaster: Arc<crate::api::SseBroadcaster>,
    ) -> Result<(Agent, Arc<tokio::sync::Mutex<SessionManager>>)> {
        let model = self.model.clone().or(config.model.clone());
        let base_url = config.base_url.clone();
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
            .unwrap_or_else(|| "anthropic".to_string());

        let base_provider: Arc<dyn LlmProvider> =
            create_provider(&provider_name, config, model, base_url)?;

        let llm: Arc<dyn LlmProvider> = if self.no_retry {
            base_provider
        } else {
            Arc::new(RetryProvider::new(base_provider))
        };

        let memory = Arc::new(
            EpisodeStore::open(data_dir)
                .await
                .wrap_err("failed to open episode store")?,
        );

        let memory_store = Arc::new(
            MemoryStore::open(data_dir)
                .await
                .wrap_err("failed to open memory store")?,
        );

        let sandbox = octos_agent::create_sandbox(&config.sandbox);
        let mut tools = ToolRegistry::with_builtins_and_sandbox(cwd, sandbox);

        // Open tool config store for user-customizable tool defaults
        let tool_config = std::sync::Arc::new(
            octos_agent::ToolConfigStore::open(data_dir)
                .await
                .wrap_err("failed to open tool config store")?,
        );
        tools.inject_tool_config(tool_config);

        // Memory bank tools
        tools.register(octos_agent::RecallMemoryTool::new(memory_store.clone()));
        tools.register(octos_agent::SaveMemoryTool::new(memory_store.clone()));

        // Cron service — jobs persist to cron.json but fire through the API channel.
        // The inbound_tx is a dummy sender; actual cron firing requires gateway mode.
        // This still enables cron CRUD (create/list/delete) for later execution.
        let (cron_tx, _cron_rx) = tokio::sync::mpsc::channel(64);
        let cron_service = Arc::new(octos_bus::CronService::new(
            data_dir.join("cron.json"),
            cron_tx,
        ));
        cron_service.start();
        let cron_tool = crate::cron_tool::CronTool::with_context(cron_service.clone(), "api", "");
        tools.register(cron_tool);

        // MCP tools
        if !config.mcp_servers.is_empty() {
            match octos_agent::McpClient::start(&config.mcp_servers).await {
                Ok(client) => client.register_tools(&mut tools),
                Err(e) => tracing::warn!("MCP initialization failed: {e}"),
            }
        }

        // Bootstrap bundled app-skills and platform skills
        let octos_home = cwd.join(".octos");
        octos_agent::bootstrap::bootstrap_bundled_skills(&octos_home);
        octos_agent::bootstrap::bootstrap_platform_skills(&octos_home);

        // Plugins (includes bootstrapped skills from bundled-app-skills/)
        let mut plugin_dirs = Config::plugin_dirs_from_project(&octos_home);
        // Platform skills are admin-only — add them here (not in Config::plugin_dirs_from_project)
        let platform_dir = octos_home.join(octos_agent::bootstrap::PLATFORM_SKILLS_DIR);
        if platform_dir.exists() {
            plugin_dirs.push(platform_dir);
        }
        let mut plugin_result = octos_agent::PluginLoadResult::default();
        if !plugin_dirs.is_empty() {
            if let Ok(result) = octos_agent::PluginLoader::load_into(&mut tools, &plugin_dirs, &[])
            {
                plugin_result = result;
            }
        }

        // Start MCP servers declared in skill manifests
        if !plugin_result.mcp_servers.is_empty() {
            match octos_agent::McpClient::start(&plugin_result.mcp_servers).await {
                Ok(client) => client.register_tools(&mut tools),
                Err(e) => tracing::warn!("skill MCP initialization failed: {e}"),
            }
        }

        let reporter: Arc<dyn octos_agent::ProgressReporter> =
            Arc::new(MetricsReporter::new(broadcaster));

        let mut agent = Agent::new(AgentId::new("api"), llm, tools, memory)
            .with_config(AgentConfig {
                max_iterations: 20,
                save_episodes: true,
                chat_max_tokens: config.gateway.as_ref().and_then(|g| g.max_output_tokens),
                ..Default::default()
            })
            .with_reporter(reporter);

        // Inject skill prompt fragments
        for fragment in &plugin_result.prompt_fragments {
            agent.append_system_prompt(fragment);
        }

        // Merge config hooks with skill-declared hooks
        let mut all_hooks = config.hooks.clone();
        all_hooks.extend(plugin_result.hooks);
        if !all_hooks.is_empty() {
            agent = agent.with_hooks(Arc::new(HookExecutor::new(all_hooks)));
        }

        let sessions = Arc::new(tokio::sync::Mutex::new(
            SessionManager::open(data_dir).wrap_err("failed to open session manager")?,
        ));

        Ok((agent, sessions))
    }
}
