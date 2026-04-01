//! Profile-based actor factory builder for child bot / sub-account sessions.
//!
//! When the gateway receives a message targeted at a specific profile (e.g. a
//! Matrix child bot), this builder constructs a dedicated [`ActorFactory`] with
//! the profile's own LLM stack, tool registry, skills, and system prompt.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::time::Duration;

use eyre::Result;
use octos_agent::{AgentConfig, HookContext, HookExecutor, SkillsLoader, ToolRegistry};
use octos_bus::{ActiveSessionStore, CronService, SessionManager};
use octos_core::OutboundMessage;
use octos_llm::{
    AdaptiveConfig, AdaptiveRouter, LlmProvider, ProviderChain, ProviderRouter, RetryProvider,
};
use octos_memory::{EpisodeStore, MemoryStore};
use tokio::sync::{Mutex, RwLock, mpsc};
use tracing::{info, warn};

use super::build_system_prompt;
use crate::commands::chat::{create_embedder, resolve_provider_policy};
use crate::config::{Config, detect_provider};
use crate::session_actor::{
    ActorFactory, PendingMessages, PipelineToolFactory, SnapshotToolRegistryFactory,
    ToolRegistryFactory,
};

/// Provider + model name + optional adaptive router, returned by [`build_llm_stack`].
pub(crate) type LlmStack = (Arc<dyn LlmProvider>, String, Option<Arc<AdaptiveRouter>>);

pub(crate) fn build_llm_stack(config: &Config, no_retry: bool) -> Result<LlmStack> {
    let model = config.model.clone();
    let base_url = config.base_url.clone();
    let provider_name = config
        .provider
        .clone()
        .or_else(|| model.as_deref().and_then(detect_provider).map(String::from))
        .unwrap_or_else(|| "anthropic".to_string());

    use crate::commands::chat::create_provider;
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
            match crate::commands::chat::create_provider_with_api_type(
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

pub(crate) fn build_plugin_env(
    config: &crate::config::Config,
    provider_name: &str,
) -> Vec<(String, String)> {
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

pub(super) struct ProfileActorFactoryBuilder {
    pub(super) profile_store: Arc<crate::profiles::ProfileStore>,
    pub(super) project_dir: PathBuf,
    pub(super) tool_config: Arc<octos_agent::ToolConfigStore>,
    pub(super) memory: Arc<EpisodeStore>,
    pub(super) memory_store: Arc<MemoryStore>,
    pub(super) agent_config: AgentConfig,
    pub(super) session_mgr: Arc<Mutex<SessionManager>>,
    pub(super) out_tx: mpsc::Sender<OutboundMessage>,
    pub(super) spawn_inbound_tx: mpsc::Sender<octos_core::InboundMessage>,
    pub(super) cron_service: Arc<CronService>,
    pub(super) tool_registry_factory: Arc<dyn ToolRegistryFactory + Send + Sync>,
    pub(super) pipeline_factory: Option<Arc<dyn PipelineToolFactory + Send + Sync>>,
    pub(super) max_history: Arc<AtomicUsize>,
    pub(super) session_timeout_secs: u64,
    pub(super) shutdown: Arc<AtomicBool>,
    pub(super) cwd: PathBuf,
    pub(super) provider_policy: Option<octos_agent::ToolPolicy>,
    pub(super) worker_prompt: Option<String>,
    pub(super) provider_router: Option<Arc<ProviderRouter>>,
    pub(super) active_sessions: Arc<RwLock<ActiveSessionStore>>,
    pub(super) pending_messages: PendingMessages,
    pub(super) queue_mode: crate::config::QueueMode,
    pub(super) plugin_prompt_fragments: Vec<String>,
    pub(super) no_retry: bool,
    /// Sandbox config for child bot tool registries.
    pub(super) sandbox_config: octos_agent::SandboxConfig,
}

impl ProfileActorFactoryBuilder {
    pub(super) async fn build(&self, profile_id: &str) -> Result<ActorFactory> {
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

        let mut system_prompt = build_system_prompt(
            effective_profile.config.gateway.system_prompt.as_deref(),
            &profile_data_dir,
            &self.project_dir,
            &self.memory_store,
            &skills_loader,
            &self.tool_config,
        )
        .await;
        for fragment in &self.plugin_prompt_fragments {
            system_prompt.push_str("\n\n");
            system_prompt.push_str(fragment);
        }
        let mut pipeline_factory = self.pipeline_factory.clone();
        let mut provider_policy = self.provider_policy.clone();
        let mut worker_prompt = self.worker_prompt.clone();
        let mut provider_router = self.provider_router.clone();

        // Child bots with admin_mode=true reuse the parent's tool registry snapshot
        // (which already has full tools + admin API). Child bots with admin_mode=false
        // build their own fresh registry (full tools, no admin API).
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
            let mut plugin_env = build_plugin_env(&profile_config, &provider_name);
            plugin_env.push((
                "OCTOS_DATA_DIR".to_string(),
                profile_data_dir.to_string_lossy().to_string(),
            ));
            plugin_env.push((
                "OCTOS_VOICE_DIR".to_string(),
                profile_data_dir
                    .join("voice_profiles")
                    .to_string_lossy()
                    .to_string(),
            ));
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
            worker_prompt = Some(crate::commands::load_prompt(
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
                    match crate::commands::chat::create_provider_with_api_type(
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
