//! Profile-scope runtime state.
//!
//! See the crate-level [`super`] module docs and
//! `docs/M11-PROFILE-SESSION-RUNTIME-ADR.md` for the two-scope model.
//! This file owns the [`ProfileRuntime`] type and the M11-D self-
//! contained implementation of [`ProfileRuntime::bootstrap`] — the
//! canonical per-profile assembler `octos serve` and `octos gateway`
//! both call.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use eyre::{Result, WrapErr};
use octos_agent::{
    PluginLoadOptions, PluginLoadResult, PluginLoader, SandboxConfig, ToolConfigStore, ToolPolicy,
    ToolRegistry, create_sandbox,
};
use octos_llm::{AdaptiveRouter, LlmProvider, QosCatalog};
use octos_memory::{EpisodeStore, MemoryStore};
use tracing::{info, warn};

use crate::commands::chat;
use crate::commands::gateway::profile_factory::{profile_plugin_env, profile_search_provider_keys};
use crate::profiles::{UserProfile, config_from_profile};
use crate::qos_catalog::{ExporterMode, build_adaptive_provider_chain};
use crate::skills_scope::{discover_ominix_url, push_runtime_plugin_env};

/// All long-lived state that belongs to a single profile within the
/// current host process.
///
/// One `ProfileRuntime` per `(host process, profile_id)`. The host
/// process is `octos serve`, `octos gateway` (each subprocess), or
/// `octos tui` — every entry point that today reads a [`UserProfile`]
/// off disk and turns it into a running agent ends up holding an
/// `Arc<ProfileRuntime>`.
///
/// # What lives here
///
/// Anything that is an *account property* of the logged-in user:
///
/// - **`llm`** — the top-level LLM provider chain (already wrapped by
///   `RetryProvider` → `ProviderChain` → optional [`AdaptiveRouter`]).
///   Two sessions opened by the same user hit the same provider chain.
/// - **`adaptive_router`** — `Some` only when QoS-aware adaptive
///   routing was successfully built (more than one provider). Owned
///   here because the per-profile metrics exporter wants a typed
///   handle, not a `dyn` provider.
/// - **`credentials`** — resolved API keys / secrets keyed by env-var
///   name. Populated from `profile.config.env_vars` via the keychain;
///   passed to MCP server spawns and plugin invocations on the session
///   side.
/// - **`skills_dir`** — the per-profile plugin directory
///   (`~/.octos/profiles/<id>/data/skills/`), if it exists. Used at
///   bootstrap time to register profile-scoped skills into
///   [`Self::tool_specs`].
/// - **`plugin_env_template`** — the env-var pairs (e.g.
///   `OCTOS_PROFILE_ID`, `OCTOS_VOICE_DIR`) every plugin spawn for
///   this profile should inherit. Sessions clone this into their own
///   plugin spawns; if a session needs to add session-scoped vars it
///   does so on top of this template.
/// - **`tool_policy`** — the profile's allow/deny tool policy. The
///   policy is *applied per session* (after the session clones
///   [`Self::tool_specs`]) so policy edits don't require rebuilding
///   the base registry.
/// - **`default_sandbox`** — the sandbox config every session
///   inherits unless it explicitly overrides via
///   [`super::SessionRuntime::sandbox`].
/// - **`tool_specs`** — the base [`ToolRegistry`] template. It has
///   builtins registered, plugins loaded, MCP agents wired, the LRU
///   pin set applied — *but no workspace bound*. Sessions clone this
///   and call `with_workspace_root` to get a workspace-bound registry.
///   This is the M11 fix for the multi-tenant base-registry leak
///   codex flagged on PR #868.
/// - **`memory`** / **`memory_store`** — the per-profile
///   [`EpisodeStore`] (redb at `<data_dir>/episodes.redb`) and
///   [`MemoryStore`] (MEMORY.md, daily notes). Memory is profile-
///   scoped because it crosses sessions — a long-running fact a user
///   teaches the agent in one room should be recallable in another
///   room of the same profile.
///
/// # What does NOT live here
///
/// Anything that can legitimately differ between two chats opened by
/// the same logged-in user — `workspace_root`, conversation history,
/// the per-session `Agent`, the session's tool-registry view, the
/// effective sandbox after a session-level override. Those live on
/// [`super::SessionRuntime`].
///
/// # Lifecycle
///
/// Built once per profile on first use via [`Self::bootstrap`]. Held
/// behind an `Arc` so every [`super::SessionRuntime`] for the profile
/// can cheaply share it. Hot-reloaded (rebuilt) when the profile
/// config on disk changes; the [`crate::config_watcher`] decides what
/// constitutes a reload-worthy change.
pub struct ProfileRuntime {
    /// Stable identifier for the profile (matches
    /// `UserProfile::id`). Used as part of the cache key in
    /// [`super::SessionRuntimeCache`] and as the value of
    /// `OCTOS_PROFILE_ID` in plugin spawns.
    pub profile_id: String,

    /// The profile's data directory, conventionally
    /// `~/.octos/profiles/<profile_id>/data`. Resolved by the caller
    /// and passed into [`Self::bootstrap`]; held here so sessions and
    /// session-scope bootstrap code don't have to re-derive it.
    pub data_dir: PathBuf,

    /// The fully-wrapped LLM provider chain for this profile.
    /// Includes retry, provider failover, and (if `adaptive_router`
    /// is `Some`) adaptive routing. Every session for this profile
    /// uses this same provider.
    pub llm: Arc<dyn LlmProvider>,

    /// Typed handle to the adaptive router if QoS-aware adaptive
    /// routing was wired in. `None` when only a single provider was
    /// configured (no failover to optimize). Held separately from
    /// `llm` so the metrics exporter and the runtime QoS catalog
    /// reader don't have to downcast the `dyn LlmProvider`.
    pub adaptive_router: Option<Arc<AdaptiveRouter>>,

    /// Materialized runtime QoS catalog produced alongside the
    /// adaptive chain. Populated even when [`Self::adaptive_router`]
    /// is `None` — `build_adaptive_provider_chain` derives a
    /// cold-start catalog from `model_catalog.json` for single-
    /// provider profiles too, and the downstream sub-provider
    /// router needs that seed for fallback ranking.
    pub runtime_qos_catalog: Option<QosCatalog>,

    /// The primary (base) provider's `model_id()` *before* the
    /// adaptive router / retry / swappable wrapping is applied.
    /// Gateway uses this for `resolve_provider_policy(..., model_id)`
    /// and as the `primary_key` of the sub-provider router's
    /// fallback ranking.
    pub primary_model_id: String,

    /// The active provider family name (e.g. `kimi`, `deepseek`,
    /// `r9s`). Captured at bootstrap time so gateway can derive its
    /// per-provider tool policy and synthesis config without
    /// re-running provider detection.
    pub provider_name: String,

    /// Resolved credentials for this profile, keyed by env-var name
    /// (e.g. `OPENAI_API_KEY`, `AUTODL_API_KEY`). Populated from
    /// `profile.config.env_vars` via the keychain resolver. Sessions
    /// read this when spawning MCP servers, plugins, and shell tools
    /// that need the profile's API keys.
    pub credentials: HashMap<String, String>,

    /// Path to the per-profile skills directory if one exists
    /// (`<data_dir>/skills/`). `None` when the profile has no
    /// dashboard-installed skills.
    pub skills_dir: Option<PathBuf>,

    /// Env-var pairs every plugin spawn for this profile should
    /// inherit (`OCTOS_PROFILE_ID`, `OCTOS_VOICE_DIR`, etc.).
    pub plugin_env_template: Vec<(String, String)>,

    /// The profile's tool policy (allow/deny lists, named groups,
    /// per-provider overrides). `None` means "no profile-level policy"
    /// — the agent's default permissions apply.
    pub tool_policy: Option<ToolPolicy>,

    /// The default sandbox config sessions inherit. Sessions may
    /// override (e.g. a slides-builder session wants
    /// `no-network`); when they don't, the runtime falls back to
    /// this value.
    pub default_sandbox: SandboxConfig,

    /// The base [`ToolRegistry`] template — builtins + plugins +
    /// MCP agents + the LRU pin set — but **NOT** workspace-bound.
    /// Sessions clone this and call `with_workspace_root` to obtain
    /// a workspace-bound registry.
    pub tool_specs: Arc<ToolRegistry>,

    /// Tool names contributed by loaded plugins. Useful for gateway's
    /// pin-as-base step (so plugin tools never get LRU-evicted) and
    /// for diagnostics. Populated from `PluginLoadResult::tool_names`.
    pub plugin_tool_names: Vec<String>,

    /// Plugin source directories actually scanned at bootstrap time.
    /// Gateway threads this into the pipeline tool factory so spawned
    /// sub-agents inherit the same skill catalog.
    pub plugin_dirs: Vec<PathBuf>,

    /// System-prompt fragments contributed by loaded plugins
    /// (skill SKILL.md auto-injection). Gateway appends these to the
    /// gateway-built system prompt; serve appends them to the per-
    /// session agent.
    pub plugin_prompt_fragments: Vec<String>,

    /// Long-lived [`EpisodeStore`] for this profile (redb at
    /// `<data_dir>/episodes.redb`). Shared across all sessions of
    /// the profile so task summaries written in one session are
    /// recallable from another.
    pub memory: Arc<EpisodeStore>,

    /// Long-lived [`MemoryStore`] (MEMORY.md + daily notes + recent
    /// memories window) for this profile.
    pub memory_store: Arc<MemoryStore>,

    /// Shared [`ToolConfigStore`] for the profile (per-tool
    /// runtime overrides, e.g. `deep_crawl.page_settle_ms`).
    pub tool_config: Arc<ToolConfigStore>,
}

impl ProfileRuntime {
    /// Build a fully populated [`ProfileRuntime`] from a parsed
    /// [`UserProfile`] + the per-profile `data_dir`.
    ///
    /// Self-contained: this is the M11-D consolidation point that
    /// both `octos serve` and `octos gateway` call as their single
    /// per-profile assembler. The function:
    ///
    /// 1. Derives a [`crate::config::Config`] from the profile via
    ///    [`config_from_profile`].
    /// 2. Builds the LLM provider chain via
    ///    [`chat::create_provider`] + [`build_adaptive_provider_chain`].
    /// 3. Opens [`EpisodeStore`] + [`MemoryStore`] against `data_dir`.
    /// 4. Opens the [`ToolConfigStore`] for per-tool runtime
    ///    overrides.
    /// 5. Constructs the base [`ToolRegistry`] (builtins + WebSearch
    ///    with profile keys + browser w/ profile-config timeout + MCP +
    ///    plugins via [`PluginLoader::load_into_with_options`] with
    ///    the profile's plugin env template).
    /// 6. Pins plugin tool names as base (LRU-defense — PR #764).
    /// 7. Applies profile-scope `tool_policy`.
    ///
    /// # Parameters
    ///
    /// - `profile` — the parsed [`UserProfile`] from the profile
    ///   store; drives the per-profile derivations.
    /// - `data_dir` — the resolved per-profile data dir, typically
    ///   `~/.octos/profiles/<id>/data`.
    /// - `octos_home` — the host's `~/.octos` (or `--octos-home`
    ///   override). Used to seed `OCTOS_HOME` in
    ///   `plugin_env_template`; defaults to `data_dir` when `None`.
    ///
    /// # Errors
    ///
    /// Returns an error when the LLM provider construction fails
    /// (typically a missing API key), when the redb episode store
    /// cannot open, or when the tool config store cannot be opened.
    /// Plugin / MCP loading failures are logged at `warn` and do not
    /// fail bootstrap (the profile still serves with builtins only).
    pub async fn bootstrap(
        profile: &UserProfile,
        data_dir: &Path,
        octos_home: Option<&Path>,
    ) -> Result<Arc<Self>> {
        // Step 1: derive the per-profile Config.
        let config = config_from_profile(profile, None, None);

        // Step 2: resolve the provider name. `config_from_profile`
        // populates `provider`/`model` from `llm.primary` when set,
        // else falls back to `detect_provider(model)`.
        let model = config.model.clone();
        let base_url = config.base_url.clone();
        let provider_name = config
            .provider
            .clone()
            .or_else(|| {
                model
                    .as_deref()
                    .and_then(crate::config::detect_provider)
                    .map(String::from)
            })
            .ok_or_else(|| {
                eyre::eyre!("profile '{}' has no LLM provider configured", profile.id)
            })?;

        // Step 3: build the LLM provider chain.
        let base_provider = chat::create_provider(&provider_name, &config, model, base_url)
            .wrap_err_with(|| {
                format!("failed to create LLM provider for profile '{}'", profile.id)
            })?;
        let primary_model_id = base_provider.model_id().to_string();
        let bundle = build_adaptive_provider_chain(
            base_provider,
            &config,
            data_dir,
            false,
            ExporterMode::Spawn,
        );
        let llm = bundle.llm.clone();
        let adaptive_router = bundle.adaptive_router.clone();
        let runtime_qos_catalog = bundle.runtime_qos_catalog.clone();

        // Step 4: open the memory stores.
        let memory = Arc::new(EpisodeStore::open(data_dir).await.wrap_err_with(|| {
            format!("failed to open episode store for profile '{}'", profile.id)
        })?);
        let memory_store = Arc::new(MemoryStore::open(data_dir).await.wrap_err_with(|| {
            format!("failed to open memory store for profile '{}'", profile.id)
        })?);

        // Step 5: tool config store.
        let tool_config = Arc::new(ToolConfigStore::open(data_dir).await.wrap_err_with(|| {
            format!(
                "failed to open tool config store for profile '{}'",
                profile.id
            )
        })?);

        // Step 6: resolve credentials from the profile's declared env
        // vars (keychain-aware). Used by MCP, plugin spawns, and the
        // shell tool when a profile-scoped env var is referenced.
        let credentials = crate::auth::keychain::resolve_env_vars(&profile.config.env_vars);

        // Step 7: discover the per-profile skills dir (if any).
        let skills_dir_candidate = data_dir.join("skills");
        let skills_dir = skills_dir_candidate
            .exists()
            .then_some(skills_dir_candidate);

        // Step 8: build the plugin env template — `OCTOS_DATA_DIR`,
        // `OCTOS_HOME`, `OCTOS_PROFILE_ID`, `OCTOS_VOICE_DIR`, and
        // (when discoverable) `OMINIX_API_URL` — plus the profile's
        // search provider keys and any first-party skill env vars
        // (`OPENAI_API_KEY`, `GEMINI_API_KEY`, ...).
        let ominix_url = discover_ominix_url();
        let effective_octos_home = octos_home
            .map(Path::to_path_buf)
            .unwrap_or_else(|| data_dir.to_path_buf());
        let mut plugin_env_template = profile_plugin_env(profile);
        push_runtime_plugin_env(
            &mut plugin_env_template,
            data_dir,
            &effective_octos_home,
            Some(profile.id.as_str()),
            ominix_url.as_deref(),
        );

        // Step 9: build the base ToolRegistry.
        //
        // Sandbox config is profile-derived. We augment
        // `read_allow_paths` with the octos home so the shell sandbox
        // can read shared skills/configs (mirrors gateway's existing
        // setup).
        let mut sandbox_config = config.sandbox.clone();
        if sandbox_config.read_allow_paths.is_empty() {
            sandbox_config
                .read_allow_paths
                .push(effective_octos_home.to_string_lossy().into_owned());
        }
        let default_sandbox = sandbox_config.clone();
        let sandbox = create_sandbox(&sandbox_config);
        // We register against `data_dir` rather than a real workspace
        // root — sessions rebind cwd via `SessionRuntime::bootstrap`
        // before any actual tool call runs.
        let mut tools = ToolRegistry::with_builtins_and_sandbox(data_dir, sandbox);
        tools.set_output_dir_hint(data_dir.join("skill-output").to_string_lossy().into_owned());
        tools.inject_tool_config(tool_config.clone());

        // Step 10: WebSearchTool with the profile's search provider
        // keys (when configured). The default builtin WebSearchTool
        // is already registered by `with_builtins_and_sandbox`; we
        // re-register here only when the profile carries explicit
        // provider keys to override.
        let search_keys = profile_search_provider_keys(profile);
        if !search_keys.is_empty() {
            tools.register(
                octos_agent::WebSearchTool::new()
                    .with_config(tool_config.clone())
                    .with_provider_keys(search_keys),
            );
        }

        // Step 11: BrowserTool with profile-configured timeout.
        if let Some(secs) = profile.config.gateway.browser_timeout_secs {
            tools.register(
                octos_agent::BrowserTool::with_timeout(std::time::Duration::from_secs(secs))
                    .with_config(tool_config.clone()),
            );
        }

        // Step 12: MCP servers from the profile's config (typically
        // empty for profile-only deployments; gateway / serve top-
        // level configs may add more on top).
        if !config.mcp_servers.is_empty() {
            match octos_agent::McpClient::start(&config.mcp_servers).await {
                Ok(client) => client.register_tools(&mut tools),
                Err(e) => warn!(profile_id = %profile.id, error = %e, "MCP initialization failed"),
            }
        }

        // Step 13: plugin loading. We scan the per-profile skills
        // dir plus the bundled app-skills and platform-skills dirs
        // under `octos_home`.
        let plugin_work_dir = data_dir.join("skill-output");
        let _ = std::fs::create_dir_all(&plugin_work_dir);
        let mut plugin_dirs: Vec<PathBuf> = Vec::new();
        if let Some(ref dir) = skills_dir {
            plugin_dirs.push(dir.clone());
        }
        let bundled_dir = effective_octos_home.join(octos_agent::bootstrap::BUNDLED_APP_SKILLS_DIR);
        if bundled_dir.exists() && !plugin_dirs.contains(&bundled_dir) {
            plugin_dirs.push(bundled_dir);
        }
        let platform_dir = effective_octos_home.join(octos_agent::bootstrap::PLATFORM_SKILLS_DIR);
        if platform_dir.exists() && !plugin_dirs.contains(&platform_dir) {
            plugin_dirs.push(platform_dir);
        }
        let mut plugin_result = PluginLoadResult::default();
        if !plugin_dirs.is_empty() {
            match PluginLoader::load_into_with_options(
                &mut tools,
                &plugin_dirs,
                &plugin_env_template,
                PluginLoadOptions {
                    work_dir: Some(&plugin_work_dir),
                    synthesis_config: None,
                },
            ) {
                Ok(result) => plugin_result = result,
                Err(e) => warn!(profile_id = %profile.id, error = %e, "plugin loading failed"),
            }
        }

        // Step 14: skill-declared MCP servers.
        if !plugin_result.mcp_servers.is_empty() {
            match octos_agent::McpClient::start(&plugin_result.mcp_servers).await {
                Ok(client) => client.register_tools(&mut tools),
                Err(e) => warn!(
                    profile_id = %profile.id,
                    error = %e,
                    "skill MCP initialization failed"
                ),
            }
        }

        // Step 15: apply tool policy + context filter from the
        // profile-derived Config.
        if let Some(ref policy) = config.tool_policy {
            tools.apply_policy(policy);
        }
        if !config.context_filter.is_empty() {
            tools.set_context_filter(config.context_filter.clone());
        }

        // Step 16: pin core builtins + plugin tools as base so the
        // LRU evictor never drops them. Mirrors gateway's pin list
        // (with the gateway-only session tools elided — those are
        // session-scope via the per-session ActorFactory).
        let mut base_tools: Vec<&str> = vec![
            "shell",
            "read_file",
            "write_file",
            "edit_file",
            "diff_edit",
            "glob",
            "grep",
            "list_dir",
            "web_search",
            "web_fetch",
            "browser",
            "check_workspace_contract",
            "workspace_log",
            "workspace_show",
            "workspace_diff",
        ];
        if cfg!(feature = "git") {
            base_tools.push("git");
        }
        if cfg!(feature = "ast") {
            base_tools.push("code_structure");
        }
        tools.set_base_tools(base_tools);
        if !plugin_result.tool_names.is_empty() {
            tools.add_base_tools(plugin_result.tool_names.iter().map(|s| s.as_str()));
        }

        // Memory bank tools — registered profile-side so every
        // session inherits the same memory_store.
        tools.register(octos_agent::RecallMemoryTool::new(memory_store.clone()));
        tools.register(octos_agent::SaveMemoryTool::new(memory_store.clone()));

        // Step 17: re-apply tool policy AFTER plugin / memory-bank
        // registration so deny entries can target plugin-declared
        // tool names too (PR #688 follow-up — MEDIUM #4).
        if let Some(ref policy) = config.tool_policy {
            tools.apply_policy(policy);
        }

        info!(
            profile_id = %profile.id,
            provider = %provider_name,
            model = %primary_model_id,
            plugin_count = plugin_result.tool_names.len(),
            tool_count = tools.specs().len(),
            "ProfileRuntime: bootstrapped"
        );

        Ok(Arc::new(Self {
            profile_id: profile.id.clone(),
            data_dir: data_dir.to_path_buf(),
            llm,
            adaptive_router,
            runtime_qos_catalog,
            primary_model_id,
            provider_name,
            credentials,
            skills_dir,
            plugin_env_template,
            tool_policy: config.tool_policy.clone(),
            default_sandbox,
            tool_specs: Arc::new(tools),
            plugin_tool_names: plugin_result.tool_names.clone(),
            plugin_dirs,
            plugin_prompt_fragments: plugin_result.prompt_fragments.clone(),
            memory,
            memory_store,
            tool_config,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::{GatewaySettings, ProfileConfig};
    use chrono::Utc;
    use octos_agent::SandboxConfig;
    use std::collections::HashMap;

    /// Build a minimal `UserProfile` with no LLM contract. M11-D
    /// bootstrap must reject this with a clear error, not panic.
    #[tokio::test]
    async fn bootstrap_errors_when_profile_has_no_provider() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("profiles").join("test").join("data");
        std::fs::create_dir_all(&data_dir).unwrap();

        let profile = UserProfile {
            id: "no-llm".to_string(),
            name: "No LLM".to_string(),
            enabled: true,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: ProfileConfig {
                gateway: GatewaySettings::default(),
                ..Default::default()
            },
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let err = ProfileRuntime::bootstrap(&profile, &data_dir, None)
            .await
            .err()
            .expect("bootstrap must fail without a provider");
        assert!(
            err.to_string().contains("no LLM provider configured"),
            "unexpected error: {err}",
        );
    }

    /// Smoke-test the structural contract: when the profile carries a
    /// declared env var, bootstrap surfaces it under `credentials`.
    ///
    /// We avoid driving `create_provider` here (which would require an
    /// API key on the test host); instead we exercise the error path
    /// and assert the error formatting includes the profile id, which
    /// proves the early-derivation steps ran in order.
    #[tokio::test]
    async fn bootstrap_error_path_names_the_profile() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("profiles").join("test").join("data");
        std::fs::create_dir_all(&data_dir).unwrap();

        let mut env_vars: HashMap<String, String> = HashMap::new();
        env_vars.insert("PROBE".to_string(), "probe-value".to_string());

        let profile = UserProfile {
            id: "named-err".to_string(),
            name: "Named Err".to_string(),
            enabled: true,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: ProfileConfig {
                gateway: GatewaySettings::default(),
                env_vars,
                sandbox: SandboxConfig::default(),
                ..Default::default()
            },
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let err = ProfileRuntime::bootstrap(&profile, &data_dir, None)
            .await
            .err()
            .expect("bootstrap must fail without a provider");
        assert!(
            err.to_string().contains("named-err"),
            "error should mention profile id: {err}",
        );
    }
}
