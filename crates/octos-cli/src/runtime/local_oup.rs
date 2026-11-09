//! Local frontend bootstrap for the canonical OUP dispatcher.
//!
//! No model loop, transcript vector or compaction implementation lives here.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use eyre::{Result, WrapErr};
use octos_agent::profile::ProfileDefinition;
use octos_llm::LlmProvider;

use super::{BootstrapRole, ProfileRuntime};
use crate::api::AppState;
use crate::config::Config;
use crate::profiles::{ProfileConfig, UserProfile};

pub(crate) struct LocalOupOptions {
    pub config: Config,
    pub profile: UserProfile,
    pub data_dir: PathBuf,
    pub config_home: PathBuf,
    pub no_retry: bool,
    pub provider: Option<Arc<dyn LlmProvider>>,
    pub tool_profile: Option<ProfileDefinition>,
    pub save_episodes: bool,
}

/// Metadata for an ambient-config invocation. The resolved Config remains
/// authoritative; this only supplies profile identity and profile-only knobs.
pub(crate) fn local_profile(id: &str, config: &Config) -> UserProfile {
    let mut settings = ProfileConfig {
        env_vars: config.env_vars.clone(),
        sandbox: config.sandbox.clone(),
        approval_policy: config.approval_policy.clone(),
        hooks: config.hooks.clone(),
        memory: config.memory.clone(),
        ..Default::default()
    };
    if let Some(gateway) = &config.gateway {
        settings.gateway.system_prompt = gateway.system_prompt.clone();
        settings.gateway.browser_timeout_secs = gateway.browser_timeout_secs;
        settings.gateway.max_output_tokens = gateway.max_output_tokens;
    }
    settings.gateway.max_iterations = config.max_iterations;
    UserProfile {
        id: id.to_owned(),
        name: id.to_owned(),
        enabled: false,
        public_subdomain: None,
        parent_id: None,
        data_dir: None,
        config: settings,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    }
}

pub(crate) async fn bootstrap(options: LocalOupOptions) -> Result<Arc<AppState>> {
    let profile_data_dir = options.data_dir.clone();
    bootstrap_with_profile_root(options, &profile_data_dir, false).await
}

/// An ephemeral frontend keeps profile context and explicitly invoked shared
/// tools, but writes its transcript and OUP/session sidecars only to its
/// temporary runtime root. No databases are copied or reopened under aliases.
pub(crate) async fn bootstrap_ephemeral(
    options: LocalOupOptions,
    profile_data_dir: &Path,
) -> Result<Arc<AppState>> {
    bootstrap_with_profile_root(options, profile_data_dir, true).await
}

async fn bootstrap_with_profile_root(
    options: LocalOupOptions,
    profile_data_dir: &Path,
    ephemeral: bool,
) -> Result<Arc<AppState>> {
    std::fs::create_dir_all(&options.data_dir).wrap_err("create local OUP runtime directory")?;
    let mut runtime = ProfileRuntime::bootstrap_resolved(
        &options.profile,
        profile_data_dir,
        Some(&options.config_home),
        if ephemeral {
            BootstrapRole::Gateway
        } else {
            BootstrapRole::Serve
        },
        options.config,
        None,
        options.no_retry,
        options.provider,
    )
    .await?;
    {
        let runtime = Arc::get_mut(&mut runtime)
            .ok_or_else(|| eyre::eyre!("local profile published before tool policy binding"))?;
        runtime.session_store_root = ephemeral.then(|| options.data_dir.clone());
        runtime.session_defaults = Some(octos_agent::AgentConfig {
            max_iterations: super::turn_policy::max_iterations(
                runtime.max_iterations,
                super::turn_policy::TurnIntent::Interactive,
            ),
            save_episodes: options.save_episodes && !ephemeral,
            ..super::session::configured_agent_defaults(runtime)
        });
    }
    if let Some(tool_profile) = options.tool_profile {
        let runtime = Arc::get_mut(&mut runtime)
            .ok_or_else(|| eyre::eyre!("local profile published before tool policy binding"))?;
        let mut tools = runtime.tool_specs.snapshot_excluding(&[]);
        // A profile template is trusted operator configuration, like its
        // existing CLI counterpart; workspace instructions are loaded later
        // by OUP against the actual opened session cwd.
        if let Some(template) = &tool_profile.system_prompt_template {
            if let Some(template) =
                crate::commands::load_profile_prompt_template(&tool_profile.name, template)
            {
                runtime.prompt_parts.pre_memory = template;
                runtime.prompt_parts.uses_bundled_base = false;
                runtime.system_prompt = runtime.prompt_parts.joined();
            }
        }
        runtime.agent_profile = Some(Arc::new(tool_profile));
        runtime.apply_tool_envelope(&mut tools);
        runtime.tool_specs = Arc::new(tools);
    }
    let mut state = AppState::without_services(&options.data_dir);
    state.sessions = Some(Arc::new(tokio::sync::Mutex::new(
        octos_bus::SessionManager::open(&options.data_dir)
            .wrap_err("open local OUP session store")?,
    )));
    // This state is reachable only through an in-process pipe. No network
    // listener or login endpoint is exposed by local frontend bootstrap.
    state.solo_login_enabled = true;
    state.default_network_denied = !runtime.default_sandbox.allow_network;
    state.task_query_store = Some(crate::session_actor::SessionTaskQueryStore::default());
    state.profiles.insert(runtime.profile_id.clone(), runtime);
    Ok(Arc::new(state))
}

pub(crate) fn resolve_stored_profile(
    id: Option<&str>,
    data_dir: &Path,
) -> Result<Option<UserProfile>> {
    let Some(id) = id.filter(|id| !id.contains('/') && !id.contains(std::path::MAIN_SEPARATOR))
    else {
        return Ok(None);
    };
    let store = crate::profiles::ProfileStore::open_unified(data_dir)?;
    Ok(store
        .get(id)?
        .map(|profile| store.resolve_runtime_profile(&profile)))
}
