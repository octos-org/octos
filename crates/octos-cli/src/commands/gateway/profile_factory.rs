//! Profile-based actor factory builder for child bot / sub-account sessions.
//!
//! When the gateway receives a message targeted at a specific profile (e.g. a
//! Matrix child bot), this builder constructs a dedicated [`ActorFactory`] with
//! the profile's own LLM stack, tool registry, skills, and system prompt.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::time::Duration;

use eyre::{Result, WrapErr};
use octos_agent::{AgentConfig, HookContext, HookExecutor, ToolRegistry};
use octos_bus::{ActiveSessionStore, CronService, SessionManager};
use octos_core::OutboundMessage;
use octos_llm::{
    AdaptiveConfig, AdaptiveRouter, LlmProvider, ProviderChain, ProviderRouter, RetryProvider,
};
use octos_memory::{EpisodeStore, MemoryStore};
use tokio::sync::{Mutex, RwLock, mpsc};
use tracing::{info, warn};

use super::build_system_prompt;
use crate::commands::chat::{create_embedder_async, resolve_provider_policy};
use crate::config::{Config, detect_provider};
use crate::session_actor::{
    ActorFactory, PendingMessages, PipelineToolFactory, SessionTaskQueryStore,
    SnapshotToolRegistryFactory, ToolRegistryFactory,
};

const FIRST_PARTY_SKILL_ENV_VARS: &[&str] = &[
    "OPENAI_API_KEY",
    "OPENAI_BASE_URL",
    "GEMINI_API_KEY",
    "GEMINI_BASE_URL",
    "GOOGLE_API_KEY",
    "GOOGLE_BASE_URL",
    "GOOGLE_CLOUD_PROJECT",
    "GOOGLE_CLOUD_LOCATION",
    "VERTEX_BASE_URL",
    "DASHSCOPE_API_KEY",
    "DASHSCOPE_BASE_URL",
    "ARK_API_KEY",
    "ARK_BASE_URL",
    // Research tool operator settings (deep-search / deep-crawl):
    // self-hosted SearXNG base URL, robots.txt (default off) and the
    // search-results scrape opt-in (default off).
    "SEARXNG_URL",
    "OCTOS_RESPECT_ROBOTS",
    "OCTOS_ALLOW_SERP_SCRAPE",
];

/// Google / Vertex credential material: the raw service-account JSON, the
/// application-default-credentials path, and OAuth access tokens. Unlike
/// [`FIRST_PARTY_SKILL_ENV_VARS`], these are forwarded to skill processes
/// ONLY when the profile's own provider chain (primary or a fallback)
/// resolves to a Google-family provider — a profile that merely has Vertex
/// credentials configured but routes through a different provider must not
/// hand its SA JSON to every skill subprocess.
///
/// The names are also force-registered via
/// [`octos_agent::register_secret_env_names`]: `VERTEX_SA_JSON` in
/// particular does not look secret to the `is_secret_env_name` heuristic,
/// and the provider-build-time registration in `Config::resolve_api_key`
/// only fires when Vertex is the ACTIVE provider.
const GOOGLE_VERTEX_CREDENTIAL_ENV_VARS: &[&str] = &[
    "GOOGLE_APPLICATION_CREDENTIALS",
    "VERTEX_SA_JSON",
    "VERTEX_ACCESS_TOKEN",
    "GOOGLE_OAUTH_ACCESS_TOKEN",
];

/// Provider families that authenticate against Google Cloud credentials.
/// Mirrors the provider-name spellings `build_plugin_env` special-cases.
fn is_google_family_provider(provider: &str) -> bool {
    matches!(
        provider,
        "gemini" | "google" | "vertex" | "vertex-ai" | "vertexai"
    )
}

/// True when the profile's provider chain (primary or any fallback)
/// resolves to a Google-family provider. Mirrors the runtime provider
/// resolution: explicit `family_id` first, else `detect_provider` on the
/// selection's model id.
fn profile_uses_google_family_provider(profile: &crate::profiles::UserProfile) -> bool {
    profile.config.llm.as_ref().is_some_and(|llm| {
        llm.primary
            .iter()
            .chain(llm.fallbacks.iter())
            .any(|selection| {
                selection
                    .family_id
                    .as_deref()
                    .or_else(|| selection.model_id.as_deref().and_then(detect_provider))
                    .is_some_and(is_google_family_provider)
            })
    })
}

pub(crate) fn canonical_search_env(provider_id: &str) -> Option<&'static str> {
    match provider_id {
        "tavily" => Some("TAVILY_API_KEY"),
        "perplexity" => Some("PERPLEXITY_API_KEY"),
        "brave" => Some("BRAVE_API_KEY"),
        "you" => Some("YDC_API_KEY"),
        "serper" => Some("SERPER_API_KEY"),
        // Not a secret: the base URL of a self-hosted SearXNG instance
        // (profile `search.providers.searxng.api_key_env` names the variable).
        "searxng" => Some("SEARXNG_URL"),
        _ => None,
    }
}

pub(crate) fn profile_search_provider_keys(
    profile: &crate::profiles::UserProfile,
) -> HashMap<String, String> {
    let resolved_env_vars = crate::auth::keychain::resolve_env_vars(&profile.config.env_vars);
    profile
        .config
        .search
        .as_ref()
        .map(|search| {
            search
                .providers
                .iter()
                .filter_map(|(provider_id, provider)| {
                    let source_key = provider.api_key_env.as_deref()?;
                    let secret = resolved_env_vars
                        .get(source_key)
                        .cloned()
                        .or_else(|| std::env::var(source_key).ok())?;
                    Some((provider_id.clone(), secret))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn push_env_once(env: &mut Vec<(String, String)>, key: impl Into<String>, value: String) {
    let key = key.into();
    if value.is_empty() || env.iter().any(|(existing, _)| existing == &key) {
        return;
    }
    env.push((key, value));
}

/// Export the immutable, resolved runtime route, replacing raw profile metadata.
pub(crate) fn apply_resolved_profile_llm_env(
    env: &mut Vec<(String, String)>,
    config: &Config,
    config_revision: &str,
) {
    let values = [
        (
            "OCTOS_PROFILE_LLM_PROVIDER",
            crate::runtime::profile::configured_provider_name(config),
        ),
        ("OCTOS_PROFILE_LLM_MODEL", config.model.clone()),
        ("OCTOS_PROFILE_LLM_BASE_URL", config.base_url.clone()),
        ("OCTOS_PROFILE_LLM_API_TYPE", config.api_type.clone()),
        (
            "OCTOS_PROFILE_LLM_CONFIG_REVISION",
            Some(config_revision.to_string()),
        ),
    ];
    env.retain(|(name, _)| !values.iter().any(|(key, _)| name == key));
    env.extend(values.into_iter().filter_map(|(name, value)| {
        value
            .filter(|value| !value.trim().is_empty())
            .map(|value| (name.to_string(), value))
    }));
}

pub(crate) fn profile_plugin_env(profile: &crate::profiles::UserProfile) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = profile_search_provider_keys(profile)
        .into_iter()
        .filter_map(|(provider_id, secret)| {
            Some((
                canonical_search_env(provider_id.as_str())?.to_string(),
                secret,
            ))
        })
        .collect();

    let resolved_env_vars = crate::auth::keychain::resolve_env_vars(&profile.config.env_vars);
    for key in FIRST_PARTY_SKILL_ENV_VARS {
        if let Some(value) = resolved_env_vars
            .get(*key)
            .cloned()
            .or_else(|| std::env::var(key).ok())
        {
            push_env_once(&mut env, *key, value);
        }
    }

    // Give first-party skills the same non-secret model selection that the
    // profile runtime uses. Skills still declare these names in their manifest
    // before the strict environment gate will expose them, and an explicit
    // skill-specific override remains free to take precedence inside the
    // skill. Provider credentials continue through the existing, separately
    // allowlisted secret path above.
    if let Some(primary) = profile
        .config
        .llm
        .as_ref()
        .and_then(|llm| llm.primary.as_ref())
    {
        if let Some(provider) = primary
            .family_id
            .as_deref()
            .or_else(|| primary.model_id.as_deref().and_then(detect_provider))
        {
            push_env_once(&mut env, "OCTOS_PROFILE_LLM_PROVIDER", provider.to_string());
        }
        if let Some(model) = primary.model_id.as_deref() {
            push_env_once(&mut env, "OCTOS_PROFILE_LLM_MODEL", model.to_string());
        }
    }

    // Google / Vertex credentials: ALWAYS secret-register the names (their
    // values may sit in the process env regardless of the active provider,
    // and `VERTEX_SA_JSON` doesn't trip the secret-name heuristic), but
    // forward them ONLY to skills of a profile whose provider chain actually
    // uses a Google-family provider. This keeps the sanctioned path working —
    // a Vertex-routed profile's skills still receive the SA JSON — without
    // leaking it into every skill subprocess of unrelated profiles.
    octos_agent::register_secret_env_names(GOOGLE_VERTEX_CREDENTIAL_ENV_VARS.iter().copied());
    if profile_uses_google_family_provider(profile) {
        for key in GOOGLE_VERTEX_CREDENTIAL_ENV_VARS {
            if let Some(value) = resolved_env_vars
                .get(*key)
                .cloned()
                .or_else(|| std::env::var(key).ok())
            {
                push_env_once(&mut env, *key, value);
            }
        }
    }

    if let Some(slides) = profile
        .config
        .apps
        .as_ref()
        .and_then(|apps| apps.slides.as_ref())
    {
        if let Some(template_dir) = slides.template_dir.as_ref() {
            push_env_once(&mut env, "PPT_TEMPLATE_DIR", template_dir.clone());
        }
        if let Some(default_theme) = slides.default_theme.as_ref() {
            push_env_once(&mut env, "PPT_DEFAULT_THEME", default_theme.clone());
        }
    }

    // Smart-home bridge: forward the RESOLVED bridge config to the
    // `smart-home` skill as `SMART_HOME_BRIDGE_URL` / `SMART_HOME_BRIDGE_TOKEN`.
    // Callers pass the runtime-resolved profile (parent + defaults merged) and
    // `resolved_env_vars` is keychain-aware, so this covers two cases the
    // skill's own profile-JSON fallback cannot: a sub-account inheriting
    // `config.smart_home` from its parent, and a `token_env` whose value is a
    // keychain marker.
    if let Some(smart_home) = profile.config.smart_home.as_ref() {
        for (key, value) in smart_home.to_env_vars(&resolved_env_vars) {
            push_env_once(&mut env, key, value);
        }
    }

    env
}

fn push_runtime_plugin_env(
    plugin_env: &mut Vec<(String, String)>,
    data_dir: &std::path::Path,
    octos_home: &std::path::Path,
    profile_id: &str,
    ominix_url: Option<&str>,
) {
    plugin_env.push((
        "OCTOS_DATA_DIR".to_string(),
        data_dir.to_string_lossy().to_string(),
    ));
    plugin_env.push((
        "OCTOS_HOME".to_string(),
        octos_home.to_string_lossy().to_string(),
    ));
    plugin_env.push(("OCTOS_PROFILE_ID".to_string(), profile_id.to_string()));
    plugin_env.push((
        "OCTOS_VOICE_DIR".to_string(),
        data_dir
            .join("voice_profiles")
            .to_string_lossy()
            .to_string(),
    ));
    if let Some(ominix_url) = ominix_url {
        plugin_env.push(("OMINIX_API_URL".to_string(), ominix_url.to_string()));
    }
}

/// Provider + model name + optional adaptive router, returned by [`build_llm_stack`].
/// (full LLM, provider name, adaptive router, strong-only LLM for slides)
pub(crate) type LlmStack = (
    Arc<dyn LlmProvider>,
    String,
    Option<Arc<AdaptiveRouter>>,
    Arc<dyn LlmProvider>,
);

pub(crate) fn build_llm_stack(
    config: &Config,
    no_retry: bool,
    data_dir: &std::path::Path,
) -> Result<LlmStack> {
    let model = config.model.clone();
    let base_url = config.base_url.clone();
    let provider_name = config
        .provider
        .clone()
        .or_else(|| model.as_deref().and_then(detect_provider).map(String::from))
        .ok_or_else(|| {
            eyre::eyre!("no LLM provider configured. Set provider in config or profile JSON")
        })?;

    use crate::commands::chat::create_provider;
    let base_provider = create_provider(&provider_name, config, model, base_url)?;
    let mut adaptive_router_ref: Option<Arc<AdaptiveRouter>> = None;

    // T-0549: build the credential pool + content classifier from config
    // at the same config→builder point `build_adaptive_provider_chain`
    // uses, so the gateway profile factory's router is wired identically.
    let credential_pool =
        crate::qos_catalog::build_credential_pool(config.credential_pool.as_ref(), data_dir);
    let content_classifier =
        crate::qos_catalog::build_content_classifier(config.content_routing.as_ref());

    // #2142: operator override of the primary's effective context window.
    let base_provider = crate::qos_catalog::apply_context_window_override(
        base_provider,
        config.context_window,
        "primary",
    );

    let llm: Arc<dyn LlmProvider> = if no_retry {
        base_provider
    } else if config.fallback_models.is_empty() {
        Arc::new(RetryProvider::new(base_provider))
    } else {
        let mut providers: Vec<Arc<dyn LlmProvider>> =
            vec![Arc::new(RetryProvider::new(base_provider))];
        let mut costs: Vec<f64> = vec![0.0]; // primary cost unknown
        for fallback in &config.fallback_models {
            match crate::qos_catalog::create_fallback_provider(config, &provider_name, fallback) {
                Ok(provider) => {
                    // #2142: per-fallback context-window override.
                    let provider = crate::qos_catalog::apply_context_window_override(
                        provider,
                        fallback.context_window,
                        "fallback",
                    );
                    providers.push(Arc::new(RetryProvider::new(provider)));
                    costs.push(fallback.cost_per_m.unwrap_or(0.0));
                }
                Err(error) => {
                    return Err(error).wrap_err_with(|| {
                        format!(
                            "configured profile fallback construction failed (operation=build_fallback_lane provider={} model={})",
                            fallback.provider,
                            fallback.model.as_deref().unwrap_or("<default>")
                        )
                    });
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
            let slot_count = providers.len();
            let router = Arc::new(
                AdaptiveRouter::new(providers, &costs, adaptive_config)
                    .with_adaptive_config(mode, qos_ranking),
            );
            // T-0549: attach the SAME credential pool to every provider
            // slot and install the content classifier — same wiring as
            // `qos_catalog::build_adaptive_provider_chain`.
            crate::qos_catalog::wire_router_pool_and_classifier(
                &router,
                slot_count,
                credential_pool.as_ref(),
                content_classifier.as_ref(),
            );
            // Wave-4c: surface AutoEscalationConfig from config.json so
            // operators can disable the latency feedback loop on the
            // gateway-side too. `qos_catalog::build_adaptive_provider_chain`
            // does the equivalent for the serve / ProfileRuntime path;
            // this is the same wiring for `commands::gateway`.
            if let Some(ar) = routing_config {
                router.set_auto_escalation_config(octos_llm::AutoEscalationConfig::from(
                    &ar.auto_escalation,
                ));
            }
            adaptive_router_ref = Some(router.clone());
            router
        } else {
            Arc::new(ProviderChain::new(providers))
        }
    };

    let llm_strong = build_strong_chain(config, &provider_name, no_retry)?;

    Ok((llm, provider_name, adaptive_router_ref, llm_strong))
}

/// Build a provider chain using only fallback models marked `strong: true`.
/// Used by slides sessions that need reliable providers for 30+ tool payloads.
pub(crate) fn build_strong_chain(
    config: &Config,
    provider_name: &str,
    no_retry: bool,
) -> Result<Arc<dyn LlmProvider>> {
    use crate::commands::chat::create_provider;
    let primary = create_provider(
        provider_name,
        config,
        config.model.clone(),
        config.base_url.clone(),
    )?;
    let strong_fallbacks: Vec<_> = config
        .fallback_models
        .iter()
        .filter(|fb| fb.strong)
        .collect();
    if no_retry {
        return Ok(primary);
    }
    if strong_fallbacks.is_empty() {
        return Ok(Arc::new(RetryProvider::new(primary)));
    }
    let mut providers: Vec<Arc<dyn LlmProvider>> = vec![Arc::new(RetryProvider::new(primary))];
    for fallback in strong_fallbacks {
        let provider = crate::qos_catalog::create_fallback_provider(config, provider_name, fallback)
        .wrap_err_with(|| {
            format!(
                "configured strong fallback construction failed (operation=build_strong_lane provider={} model={})",
                fallback.provider,
                fallback.model.as_deref().unwrap_or("<default>")
            )
        })?;
        providers.push(Arc::new(RetryProvider::new(provider)));
    }
    Ok(Arc::new(ProviderChain::new(providers)))
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
                "gemini" | "google" => Some("GEMINI_API_KEY"),
                "dashscope" | "qwen" => Some("DASHSCOPE_API_KEY"),
                // Vertex authenticates with a service-account JSON, not an API
                // key. It is not a usable credential for any plugin, and must
                // never be forwarded under the wrong `OPENAI_API_KEY` name —
                // so inject nothing for it.
                "vertex" | "vertex-ai" | "vertexai" => None,
                _ => Some("OPENAI_API_KEY"),
            };
            if let Some(key_var) = key_var {
                env.push((key_var.to_string(), api_key));
            }
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

/// S2 plumbing: build a synthesis-LLM provider config from the agent's
/// current `Config`.
///
/// Used to populate plugin args (e.g. `deep_search`'s `synthesis_config`) so
/// the plugin no longer needs to read `DEEPSEEK_API_KEY` / etc. from the
/// process environment. Returns `None` when:
///   1. No API key can be resolved for the active provider, OR
///   2. We can't determine an OpenAI-compatible base URL for the provider.
///
/// Tokens MUST NOT be logged. We log only the provider name on success.
pub(crate) fn build_research_synthesis_config(
    config: &crate::config::Config,
) -> Option<octos_agent::SynthesisConfig> {
    // Native research has one synthesis model. Reuse the saved strong lane
    // when present, including its inherited chat-provider credentials.
    let resolved = config
        .sub_providers
        .iter()
        .find(|lane| lane.key == "strong")
        .map(|lane| config.for_sub_provider(lane))
        .unwrap_or_else(|| config.clone());
    let provider = crate::runtime::profile::configured_provider_name(&resolved)?;
    build_synthesis_config(&resolved, &provider)
}

pub(crate) fn build_synthesis_config(
    config: &crate::config::Config,
    provider_name: &str,
) -> Option<octos_agent::SynthesisConfig> {
    // Resolve base URL (config override > registry default).
    let base_url = config.base_url.clone().or_else(|| {
        octos_llm::registry::lookup(provider_name)
            .and_then(|e| e.default_base_url)
            .map(String::from)
    })?;

    // Resolve API key via auth store / env.
    let api_key = config.get_api_key(provider_name).ok()?;
    if api_key.is_empty() {
        return None;
    }

    // Resolve model: default to the configured model, else fall back to a
    // sensible per-provider default that matches the registry catalog.
    let model = config
        .model
        .clone()
        .or_else(|| {
            octos_llm::registry::lookup(provider_name)
                .and_then(|e| e.default_model())
                .map(String::from)
        })
        .unwrap_or_else(|| match provider_name {
            "deepseek" => "deepseek-chat".to_string(),
            "openai" => "gpt-4o-mini".to_string(),
            "gemini" | "google" => "gemini-2.0-flash".to_string(),
            "dashscope" | "qwen" => "qwen-plus".to_string(),
            "moonshot" | "kimi" => "kimi-2.5".to_string(),
            "anthropic" => "claude-3-5-haiku-20241022".to_string(),
            _ => "gpt-4o-mini".to_string(),
        });

    info!(
        provider = %provider_name,
        endpoint = %base_url,
        model = %model,
        "built synthesis_config for plugin injection"
    );

    Some(octos_agent::SynthesisConfig {
        endpoint: base_url,
        api_key,
        model,
        provider: provider_name.to_string(),
    })
}

pub(super) struct ProfileActorFactoryBuilder {
    pub(super) profile_store: Arc<crate::profiles::ProfileStore>,
    pub(super) project_dir: PathBuf,
    /// Resolved host data root for profile isolation and plugin routing.
    pub(super) effective_octos_home: PathBuf,
    pub(super) tool_config: Arc<octos_agent::ToolConfigStore>,
    pub(super) memory: Arc<EpisodeStore>,
    pub(super) memory_store: Arc<MemoryStore>,
    pub(super) recall: Arc<octos_memory::RecallStore>,
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
    pub(super) task_query_store: SessionTaskQueryStore,
    /// M8 fix-first item 8 (gap 2): shared SubAgentOutputRouter cloned
    /// into every ActorFactory built by this builder.
    pub(super) subagent_output_router: Arc<octos_agent::SubAgentOutputRouter>,
    /// Section B (codex review round-4): host-level plugin policy, OR'd
    /// with the profile's own `plugins.require_signed` so a host config
    /// can mandate strict signing even when individual profile JSONs omit
    /// the flag. Mirrors `ProfileRuntime::bootstrap_with_host_plugins`.
    pub(super) host_plugins: crate::config::PluginsConfig,
    /// Host-level memory settings from the gateway's resolved config (which
    /// already applied the config-beats-env precedence in
    /// `Config::from_file`). Routed child profiles that omit `memory` fall
    /// back to this, NOT to a re-read of the ambient env var, so they follow
    /// the exact precedence of the main actor.
    pub(super) host_memory: Option<crate::config::MemoryConfig>,
}

impl ProfileActorFactoryBuilder {
    pub(super) async fn build(&self, profile_id: &str) -> Result<ActorFactory> {
        let profile = self
            .profile_store
            .get(profile_id)?
            .ok_or_else(|| eyre::eyre!("target profile '{profile_id}' not found"))?;
        // Resolve through the single shared resolver so routed child bots get
        // BOTH parent/sub-account inheritance AND the store's global
        // `profile-defaults.json` base (hooks / sandbox / plugin signing /
        // memory). `resolve_effective_profile` alone dropped the defaults
        // layer, so a child bot silently missed operator-mandated hooks and
        // sandbox restrictions.
        let effective_profile = self.profile_store.resolve_runtime_profile(&profile);
        let mut profile_config =
            crate::profiles::config_from_profile(&effective_profile, None, None);
        // Section B (codex review round-4): OR-merge the host's
        // `plugins.require_signed` onto the profile-derived flag so a
        // child profile that omits the new `plugins` block still honours
        // the host-level strict-signing policy.
        if self.host_plugins.require_signed {
            profile_config.plugins.require_signed = true;
        }
        // Host memory settings apply field-by-field when the child profile
        // doesn't override them (same pattern as bootstrap_with_host_plugins).
        crate::config::merge_host_memory_into_profile(
            &mut profile_config.memory,
            self.host_memory.as_ref(),
        );
        let profile_data_dir = self.profile_store.resolve_data_dir(&effective_profile);
        let (llm, provider_name, adaptive_router, llm_strong) =
            build_llm_stack(&profile_config, self.no_retry, &profile_data_dir)?;
        let llm_for_compaction = llm.clone();
        let model_id = llm.model_id().to_string();

        // Skill layering v1: the child bot inherits the resolved profile's skill
        // selection. `None` ⇒ no skills layer ⇒ every discovered skill loads.
        let skill_filter = effective_profile
            .config
            .skills
            .as_ref()
            .map(|s| s.to_agent_filter());
        let skills_loader = crate::skills_scope::build_account_skills_loader(&profile_data_dir)
            .with_skill_filter(skill_filter.clone());

        let mut child_plugin_prompt_fragments = Vec::new();
        let mut child_plugin_hooks: Vec<octos_agent::HookConfig> = Vec::new();

        let max_inject_tokens = crate::config::MemoryConfig::effective_max_inject_tokens(
            profile_config.memory.as_ref(),
        );
        let memory_refresh_enabled =
            crate::config::MemoryConfig::refresh_enabled(profile_config.memory.as_ref());
        let mut system_prompt = build_system_prompt(
            effective_profile.config.gateway.system_prompt.as_deref(),
            &profile_data_dir,
            &self.project_dir,
            &skills_loader,
            &self.tool_config,
        )
        .await;
        for fragment in &self.plugin_prompt_fragments {
            system_prompt.post_memory.push_str("\n\n");
            system_prompt.post_memory.push_str(fragment);
        }
        let mut pipeline_factory = self.pipeline_factory.clone();
        let mut provider_policy = self.provider_policy.clone();
        let mut worker_prompt = self.worker_prompt.clone();
        let mut provider_router = self.provider_router.clone();
        // Collected for SpawnTool subagents (set inside the else branch below).
        let mut actor_plugin_dirs: Vec<PathBuf> = Vec::new();
        let mut actor_plugin_env: Vec<(String, String)> = Vec::new();

        // Resolve the profile's embedding provider ONCE; the child
        // pipeline factory and the ActorFactory below share this handle
        // (codex P3: duplicate resolves broke the single-handle
        // invariant and doubled keychain lookups).
        let profile_embedder = create_embedder_async(&profile_config)
            .await
            .map(|e| e as Arc<dyn octos_llm::EmbeddingProvider>);
        // The routed profile's OWN recall index (its personal records live
        // under its data dir); the gateway's store is only right when both
        // are the same directory.
        let profile_recall: Arc<octos_memory::RecallStore> = if profile_data_dir
            == self.effective_octos_home
        {
            self.recall.clone()
        } else {
            crate::runtime::profile::open_recall_store(
                &profile_data_dir,
                &profile_config,
                profile_embedder.as_deref(),
            )
            .await
            .wrap_err_with(|| format!("failed to open recall store for profile '{profile_id}'"))?
        };

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
            tools.set_output_dir_hint(
                profile_data_dir
                    .join("skill-output")
                    .to_string_lossy()
                    .to_string(),
            );
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
            plugin_env.extend(profile_plugin_env(&effective_profile));
            push_runtime_plugin_env(
                &mut plugin_env,
                &profile_data_dir,
                &self.project_dir,
                profile_id,
                crate::skills_scope::discover_ominix_url().as_deref(),
            );
            let plugin_dirs = crate::skills_scope::build_account_plugin_dirs(&profile_data_dir);
            if !plugin_dirs.is_empty() {
                // S2 plumbing: pass profile-scoped synthesis config so per-tenant
                // routing of synthesis credentials works.
                let synthesis_config = build_research_synthesis_config(&profile_config);
                match octos_agent::PluginLoader::load_into_with_options_and_filter(
                    &mut tools,
                    &plugin_dirs,
                    &plugin_env,
                    octos_agent::PluginLoadOptions {
                        work_dir: Some(&plugin_work_dir),
                        synthesis_config,
                        // Section B: opt-in strict signature enforcement.
                        // Honours `plugins.require_signed` from the
                        // profile-derived config; default is `false`
                        // (backward compatible — unsigned plugins still
                        // load with a warning).
                        require_signed: profile_config.plugins.require_signed,
                        verified_cache_dir: None,
                    },
                    skill_filter.as_ref(),
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
                // SPEC-VENDOR-NODE-V1 HTTP tool discovery — hard-fail per
                // @ymote's Finding 2 contract (see chat.rs).
                octos_agent::plugins::register_http_skills_on_startup(&mut tools, &plugin_dirs)
                    .await
                    .wrap_err_with(|| {
                        format!("HTTP tool discovery failed for child bot profile {profile_id}")
                    })?;
            }
            actor_plugin_dirs = plugin_dirs.clone();
            actor_plugin_env = plugin_env;
            let search_provider_keys = profile_search_provider_keys(&effective_profile);
            if !search_provider_keys.is_empty() {
                tools.register(
                    octos_agent::WebSearchTool::new()
                        .with_config(self.tool_config.clone())
                        .with_provider_keys(search_provider_keys.clone()),
                );
            }

            tools.register(
                octos_agent::DeepSearchTool::new(profile_data_dir.join("research"))
                    .with_provider_keys(search_provider_keys),
            );
            tools.register(octos_agent::SynthesizeResearchTool::new(
                llm.clone(),
                profile_data_dir.clone(),
            ));
            tools.register(octos_agent::ManageSkillsTool::new(
                profile_data_dir.join("skills"),
            ));
            tools.register(
                octos_agent::RecallMemoryTool::new(self.memory_store.clone())
                    .with_recall(profile_recall.clone(), profile_embedder.clone()),
            );
            tools.register(octos_agent::MemorySearchTool::new(
                profile_recall.clone(),
                profile_embedder.clone(),
            ));
            tools.register(octos_agent::MemoryLoadTool::new(
                profile_recall.clone(),
                self.memory_store.clone(),
            ));
            tools.register(octos_agent::SaveMemoryTool::new(self.memory_store.clone()));
            tools.register(octos_agent::RecordMemoryUseTool::new(
                self.memory_store.clone(),
            ));
            if memory_refresh_enabled {
                tools.register(octos_agent::MemoryNoteTool::new(self.memory_store.clone()));
            }
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
                    match crate::qos_catalog::create_fallback_provider(
                        &profile_config,
                        &provider_name,
                        fb,
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

            // RFC-0 (#1289): LRU tool deferral + the `activate_tools`
            // meta-tool were removed. Every enabled tool is emitted every
            // turn (full schema) — no base-tool pin list or auto-defer pass.

            // PR #688 follow-up — codex finding: re-apply tool_policy
            // AFTER all base-registry tools have been registered. The
            // first pass at line ~648 ran before some late-registered base
            // tools existed, so a `tool_policy.deny` entry targeting them
            // was bypassed at the base level. Per-session re-apply in
            // `ActorFactory::spawn` still covers `run_pipeline`.
            if let Some(ref policy) = profile_config.tool_policy {
                tools.apply_policy(policy);
            }

            // Child profiles must not bring back retired DOT execution.
            pipeline_factory = None;

            Arc::new(SnapshotToolRegistryFactory::new(tools))
        };

        if !child_plugin_prompt_fragments.is_empty() {
            for fragment in &child_plugin_prompt_fragments {
                system_prompt.post_memory.push_str("\n\n");
                system_prompt.post_memory.push_str(fragment);
            }
        }

        let mut all_hooks = effective_profile.config.hooks.clone();
        all_hooks.extend(child_plugin_hooks);
        let hooks = if all_hooks.is_empty() {
            None
        } else {
            Some(Arc::new(HookExecutor::new(all_hooks)))
        };
        let usage_ledger = Arc::new(
            crate::usage_ledger::PersistentUsageLedger::open_sync(&profile_data_dir)
                .wrap_err("failed to open profile usage ledger")?,
        );

        Ok(ActorFactory {
            agent_config: self.agent_config.clone(),
            llm: llm.clone(),
            llm_for_compaction,
            memory: self.memory.clone(),
            memory_inject_tokens: max_inject_tokens,
            memory_refresh_enabled,
            system_prompt: Arc::new(std::sync::RwLock::new(system_prompt)),
            hooks,
            hook_context_template: Some(HookContext {
                session_id: None,
                profile_id: Some(profile_id.to_string()),
            }),
            data_dir: profile_data_dir,
            usage_ledger: Some(usage_ledger),
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
            // PR #688 follow-up — MEDIUM #4: thread the profile's
            // `tool_policy` so `ActorFactory::spawn` re-applies it AFTER
            // per-session `run_pipeline` registration. The non-admin
            // branch above already applied it to the base registry, but
            // per-session tools registered later (notably the spawn_only
            // pipeline tool) bypass that initial pass.
            tool_policy: profile_config.tool_policy.clone(),
            worker_prompt,
            provider_router,
            embedder: profile_embedder,
            active_sessions: self.active_sessions.clone(),
            pending_messages: self.pending_messages.clone(),
            queue_mode: self.queue_mode,
            adaptive_router,
            // RFC-3 (#1292): thread per-profile topic→lane overrides
            // through to the ActorFactory so the SessionActor's
            // agent_task spawn can build the lane context off
            // `profile.config.lane_routing`. None = built-in defaults.
            lane_routing: effective_profile.config.lane_routing.clone(),
            memory_store: Some(self.memory_store.clone()),
            recall: Some(profile_recall.clone()),
            // Codex round-2 MAJOR 3 (PR #1327 review): expose the
            // profile_id so `ActorFactory::spawn` can build a per-
            // session SessionScope (multi-tenant) and attach the
            // canonicalised skill_read_zones to gateway-spawned actors.
            profile_id: Some(profile_id.to_string()),
            plugin_dirs: actor_plugin_dirs,
            plugin_extra_env: actor_plugin_env,
            // Section B (codex review P1.1): propagate the profile's
            // strict-signing policy to SpawnTool subagents so unsigned
            // plugins are also rejected under spawn.
            plugin_require_signed: profile_config.plugins.require_signed,
            llm_strong,
            // #1935 — per-profile INDEPENDENT goal-completion verifier lane
            // (`sub_providers` key `goal_verifier`); `None` ⇒ genuinely
            // unconfigured; construction failures stay fail-closed.
            goal_verifier_llm: crate::runtime::profile::build_goal_verifier_provider_or_degraded(
                &profile_config,
            ),
            task_query_store: self.task_query_store.clone(),
            subagent_output_router: self.subagent_output_router.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CredentialPoolConfig;
    use octos_core::Message;
    use std::path::Path;

    /// T-0549: the gateway profile-factory construction path must wire the
    /// configured credential pool and content classifier into the
    /// `AdaptiveRouter` it builds — same behavior as
    /// `qos_catalog::build_adaptive_provider_chain`.
    #[tokio::test]
    async fn should_wire_pool_and_classifier_in_profile_factory_router() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.provider = Some("local".into());
        config.model = Some("t0549-primary".into());
        config.base_url = Some("http://127.0.0.1:1".into());
        config.fallback_models = vec![crate::config::FallbackModel {
            provider: "local".into(),
            model: Some("t0549-fallback".into()),
            base_url: Some("http://127.0.0.1:1".into()),
            api_key_env: None,
            model_hints: None,
            api_type: None,
            cost_per_m: None,
            strong: true,
            context_window: None,
        }];
        config.credential_pool = Some(CredentialPoolConfig {
            state_path: Some(dir.path().join("pool.redb").to_string_lossy().into_owned()),
            credential_ids: vec!["t0549-cred-a".into()],
            ..Default::default()
        });
        config.content_routing = Some(octos_llm::RoutingConfig {
            enabled: true,
            min_strong_length: 400,
            strong_keywords: Vec::new(),
        });

        let (_llm, _name, router, _strong) =
            build_llm_stack(&config, false, dir.path()).expect("build_llm_stack");
        let router = router.expect("two lanes must produce an adaptive router");
        assert!(
            router.acquire_credential(0, "test").await.is_some(),
            "profile-factory router must have the configured pool attached"
        );
        assert!(
            router.classify_turn(&[Message::user("hi")]).is_some(),
            "profile-factory router must have the configured classifier installed"
        );

        let mut broken = config.clone();
        broken.fallback_models[0].provider = "t0549-no-such-provider".into();
        broken.fallback_models[0].base_url = None;
        let error = match build_llm_stack(&broken, false, dir.path()) {
            Ok(_) => panic!("invalid configured fallback must fail profile construction"),
            Err(error) => error,
        };
        let detail = format!("{error:#}");
        assert!(
            detail.contains(
                "operation=build_fallback_lane provider=t0549-no-such-provider model=t0549-fallback"
            ) && detail.contains("unknown provider: t0549-no-such-provider"),
            "{detail}"
        );
    }

    /// Exercise the public stacks, including their real HTTP auth headers, in a
    /// subprocess so ambient logins, key env vars and parallel tests cannot mix.
    #[tokio::test]
    async fn should_isolate_credentials_when_profile_falls_back() {
        const CHILD: &str = "OCTOS_TEST_PROFILE_CREDENTIALS";
        let Ok(auth_home) = std::env::var(CHILD) else {
            let dir = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "commands::gateway::profile_factory::tests::should_isolate_credentials_when_profile_falls_back", "--nocapture"])
                .env(CHILD, dir.path())
                .env("OCTOS_CONFIG_DIR", dir.path())
                .env("DEEPSEEK_API_KEY", "")
                .env("LOCAL_API_KEY", "")
                .env("CUSTOM_API_KEY", "")
                .env("T0593_MISSING_KEY", "")
                .env_remove("HTTP_PROXY").env_remove("HTTPS_PROXY").env_remove("ALL_PROXY")
                .env_remove("http_proxy").env_remove("https_proxy").env_remove("all_proxy")
                .output().unwrap();
            assert!(String::from_utf8_lossy(&output.stdout).contains("running 1 test"));
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        };
        assert_eq!(
            crate::config_context::resolve_config_context(None).auth_home,
            Path::new(&auth_home)
        );
        let mut auth = crate::auth::AuthStore::at(Path::new(&auth_home)).unwrap();
        let mut failures = Vec::new();
        // label, family, explicit env, default key present, OAuth family, expected key/error
        for (label, family, key_env, default_key, oauth, expected) in [
            (
                "default",
                "deepseek",
                None,
                true,
                None,
                Some("fallback-secret"),
            ),
            (
                "explicit",
                "deepseek",
                Some("T0593_OWN_KEY"),
                true,
                None,
                Some("own-secret"),
            ),
            (
                "shared",
                "deepseek",
                Some("T0593_PRIMARY_KEY"),
                true,
                None,
                Some("primary-secret"),
            ),
            ("same", "moonshot", None, true, None, Some("primary-secret")),
            (
                "alias-inferred",
                "KIMI",
                None,
                true,
                None,
                Some("primary-secret"),
            ),
            ("keyless", "local", None, false, None, Some("no-key")),
            (
                "local-explicit",
                "local",
                Some("T0593_OWN_KEY"),
                false,
                None,
                Some("own-secret"),
            ),
            (
                "fallback-oauth",
                "deepseek",
                None,
                false,
                Some("deepseek"),
                Some("oauth-secret"),
            ),
            (
                "primary-oauth",
                "deepseek",
                None,
                true,
                Some("moonshot"),
                Some("fallback-secret"),
            ),
            ("missing", "deepseek", None, false, None, None),
            (
                "explicit-missing",
                "deepseek",
                Some("T0593_MISSING_KEY"),
                true,
                None,
                None,
            ),
            ("unknown", "t0593-unknown", None, true, None, None),
            ("custom-implicit", "custom", None, true, None, None),
            ("custom-login", "custom", None, true, Some("custom"), None),
            (
                "custom-own",
                "custom",
                Some("T0593_OWN_KEY"),
                true,
                Some("custom"),
                Some("own-secret"),
            ),
            (
                "custom-share",
                "custom",
                Some("T0593_PRIMARY_KEY"),
                true,
                None,
                Some("primary-secret"),
            ),
            (
                "custom-missing",
                "custom",
                Some("T0593_MISSING_KEY"),
                true,
                Some("custom"),
                None,
            ),
            (
                "custom-same",
                "custom",
                None,
                true,
                None,
                Some("primary-secret"),
            ),
        ] {
            if let Some(provider) = oauth {
                auth.set(
                    provider,
                    crate::auth::AuthCredential {
                        provider: provider.into(),
                        access_token: "oauth-secret".into(),
                        refresh_token: None,
                        expires_at: None,
                        auth_method: "oauth".into(),
                    },
                )
                .unwrap();
            }
            for lane in ["profile", "strong", "shared", "registered"] {
                use wiremock::{
                    Mock, MockServer, ResponseTemplate,
                    matchers::{body_partial_json, method, path},
                };
                // Constructor probes may outlive a rejected stack. Dedicated
                // servers prevent a later case from receiving those requests.
                let server = MockServer::builder().start().await;
                let secondary = MockServer::builder().start().await;
                let primary_family = if family == "custom" {
                    "custom"
                } else {
                    "moonshot"
                };
                let same_endpoint = label == "custom-same";
                let target = if family == "custom" && !same_endpoint {
                    &secondary
                } else {
                    &server
                };
                let prefix = if same_endpoint {
                    "/primary"
                } else {
                    "/fallback"
                };
                if family == "custom" && !same_endpoint {
                    Mock::given(method("GET"))
                        .and(path("/primary/props"))
                        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                            "default_generation_settings":{"n_ctx":65536}
                        })))
                        .mount(&server)
                        .await;
                }
                let probe_count = if matches!(family, "local" | "custom") {
                    (if lane == "profile" { 2 } else { 1 })
                        * if same_endpoint && lane != "registered" {
                            2
                        } else {
                            1
                        }
                } else {
                    0
                };
                let probes = Mock::given(method("GET"))
                    .and(path(format!("{prefix}/props")))
                    .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "default_generation_settings":{"n_ctx":65536}
                    })))
                    .expect(if expected.is_none() { 0 } else { probe_count }..=probe_count)
                    .mount_as_scoped(target)
                    .await;
                Mock::given(method("POST"))
                    .and(path("/primary/chat/completions"))
                    .and(body_partial_json(serde_json::json!({"model":"kimi-k2"})))
                    .respond_with(ResponseTemplate::new(401).set_body_json(
                        serde_json::json!({"error":{"message":"fixture auth failure"}}),
                    ))
                    .mount(&server)
                    .await;
                Mock::given(method("POST")).and(path(format!("{prefix}/chat/completions")))
                    .and(body_partial_json(serde_json::json!({"model":"fixture-fallback"})))
                    .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "choices":[{"message":{"role":"assistant","content":"fallback-ok"},"finish_reason":"stop"}],
                        "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
                    }))).mount(target).await;
                let mut config: Config = serde_json::from_value(serde_json::json!({
                    "provider": if label == "alias-inferred" { None } else { Some(primary_family) },
                    "model":"kimi-k2", "base_url":format!("{}/primary",server.uri()),
                    "api_key_env":"T0593_PRIMARY_KEY",
                    "adaptive_routing":{"mode":"off","probe_probability":0},
                    "env_vars":{"T0593_PRIMARY_KEY":"primary-secret", "T0593_OWN_KEY":"own-secret"},
                    "fallback_models":[{"provider":family,"model":"fixture-fallback",
                        "base_url":format!("{}{prefix}",target.uri()),"api_key_env":key_env,"strong":true}]
                })).unwrap();
                if default_key {
                    config.env_vars.insert(
                        if family == "custom" {
                            "CUSTOM_API_KEY"
                        } else {
                            "DEEPSEEK_API_KEY"
                        }
                        .into(),
                        "fallback-secret".into(),
                    );
                }
                let result = match lane {
                    "profile" => {
                        build_llm_stack(&config, false, Path::new(&auth_home)).map(|stack| stack.0)
                    }
                    "strong" => build_strong_chain(&config, primary_family, false),
                    "shared" => crate::commands::chat::create_provider(
                        primary_family,
                        &config,
                        config.model.clone(),
                        config.base_url.clone(),
                    )
                    .and_then(|primary| {
                        crate::qos_catalog::build_adaptive_provider_chain(
                            primary,
                            primary_family,
                            &config,
                            Path::new(&auth_home),
                            false,
                            crate::qos_catalog::ExporterMode::Disabled,
                        )
                        .map(|bundle| bundle.llm)
                    }),
                    "registered" => crate::qos_catalog::create_fallback_provider(
                        &config,
                        primary_family,
                        &config.fallback_models[0],
                    ),
                    _ => unreachable!(),
                };
                match (result, expected) {
                    (Ok(provider), expected) => {
                        if expected.is_none() {
                            failures
                                .push(format!("{label} lane={lane}: missing credential accepted"));
                        }
                        let key = expected.unwrap_or("no-key");
                        let response = tokio::time::timeout(
                            Duration::from_secs(5),
                            provider.chat(
                                &[Message::user("offline fixture")],
                                &[],
                                &Default::default(),
                            ),
                        )
                        .await
                        .expect("offline failover must finish")
                        .unwrap();
                        assert_eq!(response.content.as_deref(), Some("fallback-ok"));
                        if matches!(family, "local" | "custom") && expected.is_some() {
                            tokio::time::timeout(
                                Duration::from_secs(5),
                                probes.wait_until_satisfied(),
                            )
                            .await
                            .expect("all local-provider probes must finish");
                        }
                        let mut requests = server.received_requests().await.unwrap();
                        requests.extend(secondary.received_requests().await.unwrap());
                        let chats: Vec<_> =
                            requests.iter().filter(|r| r.method == "POST").collect();
                        let mut keys = vec![key];
                        if lane != "registered" {
                            keys.insert(
                                0,
                                if oauth == Some(primary_family) {
                                    "oauth-secret"
                                } else {
                                    "primary-secret"
                                },
                            );
                        }
                        assert_eq!(chats.len(), keys.len(), "{label} lane={lane}");
                        for (request, expected_key) in chats.iter().zip(keys) {
                            if request.headers.get("authorization").unwrap()
                                != &format!("Bearer {expected_key}")
                            {
                                failures.push(format!(
                                    "{label} lane={lane}: wrong credential at {}",
                                    request.url.path()
                                ));
                            }
                        }
                        for request in requests.iter().filter(|r| r.method == "GET") {
                            let probe_key = if request.url.path().starts_with("/primary/") {
                                if oauth == Some(primary_family) {
                                    "oauth-secret"
                                } else {
                                    "primary-secret"
                                }
                            } else {
                                key
                            };
                            let probe_auth =
                                (probe_key != "no-key").then(|| format!("Bearer {probe_key}"));
                            if request
                                .headers
                                .get("authorization")
                                .map(|v| v.to_str().unwrap())
                                != probe_auth.as_deref()
                            {
                                failures
                                    .push(format!("{label} lane={lane}: wrong probe credential"));
                            }
                        }
                    }
                    (Err(error), None) => {
                        let detail = format!("{error:#}");
                        if lane != "registered" {
                            assert!(
                                detail.contains(if lane == "strong" {
                                    "operation=build_strong_lane"
                                } else {
                                    "operation=build_fallback_lane"
                                }),
                                "{detail}"
                            );
                            assert!(
                                detail
                                    .contains(&format!("provider={family} model=fixture-fallback")),
                                "{detail}"
                            );
                        }
                        assert!(
                            detail.contains(match label {
                                "missing" => "DEEPSEEK_API_KEY not set or empty",
                                "explicit-missing" => "T0593_MISSING_KEY not set or empty",
                                "unknown" => "unknown provider: t0593-unknown",
                                "custom-implicit" | "custom-login" => "distinct custom endpoints require an explicit fallback api_key_env",
                                "custom-missing" => "T0593_MISSING_KEY not set or empty",
                                _ => unreachable!(),
                            }),
                            "{detail}"
                        );
                        for secret in [
                            "primary-secret",
                            "fallback-secret",
                            "own-secret",
                            "oauth-secret",
                        ] {
                            assert!(!detail.contains(secret), "error exposed credential");
                        }
                        assert!(server.received_requests().await.unwrap().iter().all(|r| {
                            family == "custom"
                                && r.method == "GET"
                                && r.url.path().starts_with("/primary/")
                        }));
                        assert!(secondary.received_requests().await.unwrap().is_empty());
                        // Rejected custom targets must never start discovery.
                        drop(probes);
                    }
                    (Err(error), Some(_)) => failures.push(format!(
                        "{label} lane={lane}: unexpected construction failure: {error:#}"
                    )),
                }
            }
            if let Some(provider) = oauth {
                auth.remove(provider).unwrap();
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }
}
