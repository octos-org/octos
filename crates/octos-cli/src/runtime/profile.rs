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
    HookExecutor, PluginLoadOptions, PluginLoadResult, PluginLoader, SandboxConfig,
    ToolConfigStore, ToolPolicy, ToolRegistry, create_sandbox,
};
use octos_bus::CronService;
use octos_llm::{AdaptiveRouter, LlmProvider, QosCatalog};
use octos_memory::{EpisodeStore, MemoryStore};
use tracing::{info, warn};

use crate::commands::chat;
use crate::commands::gateway::build_system_prompt;
use crate::commands::gateway::profile_factory::{profile_plugin_env, profile_search_provider_keys};
use crate::config::Config;
use crate::cron_tool::CronTool;
use crate::profiles::{UserProfile, config_from_profile};
use crate::qos_catalog::{ExporterMode, build_adaptive_provider_chain};
use crate::skills_scope::{
    build_account_skills_loader, discover_ominix_url, push_runtime_plugin_env,
};

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

    /// Fully pre-assembled system prompt for this profile. Built once
    /// at bootstrap by calling [`build_system_prompt`] (the gateway's
    /// canonical assembler) and then appending every fragment in
    /// [`Self::plugin_prompt_fragments`]. Every [`super::SessionRuntime`]
    /// bootstrapped from this profile copies the value onto its
    /// per-session [`octos_agent::Agent`] via
    /// [`octos_agent::Agent::with_system_prompt`]. This is the M11-F
    /// regression fix (#891) — the previous serve-mode
    /// `try_create_agent` helper called the same build + append loop
    /// inline, but M11-F deleted that helper and routed everything
    /// through [`super::SessionRuntime::bootstrap`], which never
    /// re-derived the prompt. The result was that SKILL.md auto-
    /// injected guidance (e.g. the mofa-fm "call fm_tts directly"
    /// note) never reached the LLM on `/api/chat` or the UI Protocol
    /// WebSocket path. Pre-assembling once on `ProfileRuntime` keeps
    /// the heavy work (memory context, skills summary, bootstrap
    /// files) off the per-request hot path.
    pub system_prompt: String,

    /// Hook configurations contributed by loaded plugins (skill
    /// manifests can declare `before_tool_call` / `after_tool_call` /
    /// `before_llm_call` / `after_llm_call` hooks). Gateway merges
    /// these with `config.hooks` to build its `HookExecutor`. Captured
    /// alongside `plugin_tool_names` / `plugin_prompt_fragments` so
    /// gateway can reuse the bootstrap's `PluginLoadResult` without
    /// re-running plugin discovery.
    pub plugin_hooks: Vec<octos_agent::HookConfig>,

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

    /// Profile-scope cron service (M11-F regression fix REG-2).
    ///
    /// Pre-M11-F `serve.rs::try_create_agent` constructed one
    /// [`CronService`] per server, called `start()`, and registered a
    /// [`CronTool`] backed by it. M11-F deleted that helper and never
    /// re-instated the wiring, so `/api/chat` and the UI Protocol path
    /// lost the `cron` tool entirely. We restore the registration at
    /// the profile scope (the cron jobs persist to `cron.json` under
    /// the profile's `data_dir`, matching the per-profile isolation
    /// the rest of `ProfileRuntime` already enforces) and hold the
    /// resulting `Arc<CronService>` here so the tokio timer task
    /// `start()` spawns survives for the lifetime of the runtime.
    /// Dropping the `Arc` would let the underlying service drop, which
    /// would in turn drop the timer's `JoinHandle` and silently
    /// terminate scheduled job execution.
    pub cron_service: Option<Arc<CronService>>,

    /// Pre-built lifecycle hook executor (M11-F regression fix REG-3).
    ///
    /// Pre-M11-F `serve.rs::try_create_agent` merged `config.hooks +
    /// plugin_result.hooks` and called `agent.with_hooks(Arc::new(
    /// HookExecutor::new(all_hooks)))`. M11-F lost that wiring on every
    /// per-session agent build. We assemble the executor once at
    /// profile-bootstrap time and propagate it onto every per-session
    /// [`octos_agent::Agent`] (via [`super::SessionRuntime::bootstrap`]'s
    /// `with_hooks`) AND onto the request-rebuilt agents in both
    /// `ws_standalone_agent` and the UI Protocol per-turn rebuild
    /// loop. `None` keeps the legacy behaviour when no hooks are
    /// configured (the agent's default `hooks: None` field).
    pub hook_executor: Option<Arc<HookExecutor>>,
}

/// Which OS process is calling [`ProfileRuntime::bootstrap`].
///
/// Used to decide whether [`EpisodeStore::open`] should fail loudly
/// on redb lock contention (the canonical owner — `Serve`) or degrade
/// gracefully (the companion process — `Gateway`).
///
/// See the type-level docs on
/// [`octos_memory::EpisodeStore`](EpisodeStore) for why the role
/// split exists: redb is single-writer-single-process, and `octos
/// serve` + `octos gateway` are separate OS processes that both
/// bootstrap the same profile. Serve owns the canonical store;
/// gateway is allowed to degrade so channel polling stays alive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootstrapRole {
    /// Caller owns the canonical EpisodeStore. `EpisodeStore::open`
    /// runs in strict mode and fails if the redb file lock is already
    /// held. Use this from `octos serve` and other entry points whose
    /// correctness depends on persistence being intact.
    Serve,
    /// Caller is a companion process that should keep running even
    /// when the canonical EpisodeStore is owned elsewhere.
    /// `EpisodeStore::open_or_degraded` runs and silently installs a
    /// no-op store on lock contention. Use this from
    /// `octos gateway` subprocesses.
    Gateway,
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
        role: BootstrapRole,
    ) -> Result<Arc<Self>> {
        Self::bootstrap_with_host_plugins(profile, data_dir, octos_home, role, None).await
    }

    /// Section B (codex review round-3): bootstrap a profile runtime while
    /// honouring the host-level `plugins.require_signed` policy. When the
    /// caller (e.g. `octos serve`) has the top-level [`Config`] in scope,
    /// it passes the host plugin policy here so the per-profile plugin
    /// load enforces strict signing even when the profile JSON doesn't
    /// repeat the setting. Profile-level `plugins.require_signed` is OR'd
    /// with the host setting — neither side can silently relax the other.
    pub async fn bootstrap_with_host_plugins(
        profile: &UserProfile,
        data_dir: &Path,
        octos_home: Option<&Path>,
        role: BootstrapRole,
        host_plugins: Option<&crate::config::PluginsConfig>,
    ) -> Result<Arc<Self>> {
        // Step 1: derive the per-profile Config. Apply the host plugin
        // policy on top of the profile-derived one before any downstream
        // step inspects `config.plugins.require_signed`.
        let mut config = config_from_profile(profile, None, None);
        if let Some(host) = host_plugins {
            if host.require_signed {
                config.plugins.require_signed = true;
            }
        }

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
        //
        // The opener variant depends on the caller's role (see
        // [`BootstrapRole`] docs). Serve must hold the canonical
        // EpisodeStore; gateway falls back to a degraded handle when
        // serve already owns the redb lock so it doesn't crashloop on
        // every startup. Tracked by issue #899.
        let memory_open_result = match role {
            BootstrapRole::Serve => EpisodeStore::open(data_dir).await,
            BootstrapRole::Gateway => EpisodeStore::open_or_degraded(data_dir).await,
        };
        let memory = Arc::new(memory_open_result.wrap_err_with(|| {
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

        // Step 13: plugin loading.
        //
        // M11-F regression fix REG-5: replace the hand-rolled
        // per-profile-only assembly with `Config::plugin_dirs_from_project`
        // (the canonical helper pre-M11-F serve.rs used) so the resulting
        // set includes the deployment-scoped `<octos_home>/plugins`,
        // `<octos_home>/skills`, the colon-separated `OCTOS_SKILLS_PATH`
        // env var, and the already-scanned `<octos_home>/bundled-app-skills/`.
        // Platform skills (`<octos_home>/platform-skills/`, admin-only) and
        // the per-profile `data_dir/skills/` are layered on top so the
        // gateway behaviour is matched 1:1.
        //
        // Legacy HOME-rooted globals (`~/.octos/plugins`, `~/.octos/skills`)
        // are NO LONGER scanned — `Config::plugin_dirs_from_project` emits a
        // one-shot migration warning on first detection.
        let plugin_work_dir = data_dir.join("skill-output");
        let _ = std::fs::create_dir_all(&plugin_work_dir);
        let mut plugin_dirs: Vec<PathBuf> = Config::plugin_dirs_from_project(&effective_octos_home);
        let platform_dir = effective_octos_home.join(octos_agent::bootstrap::PLATFORM_SKILLS_DIR);
        if platform_dir.exists() && !plugin_dirs.contains(&platform_dir) {
            plugin_dirs.push(platform_dir);
        }
        if let Some(ref dir) = skills_dir {
            if !plugin_dirs.contains(dir) {
                plugin_dirs.push(dir.clone());
            }
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
                    // Section B: honour the profile-derived
                    // `plugins.require_signed`. Default is `false`
                    // (backward compatible). Profile bootstrap reads
                    // the flag from the flattened `Config` produced by
                    // `config_from_profile` so operators can opt into
                    // strict signature enforcement per deployment.
                    require_signed: config.plugins.require_signed,
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

        // REG-7 follow-up: register `run_pipeline` at profile scope so
        // the serve path (`/api/sessions/*`, UI Protocol WS) exposes
        // it just like the gateway path does at
        // `crates/octos-cli/src/session_actor.rs:2283-2305`. The serve
        // path is the one `octos serve` mounts for web clients; prior
        // to this, only the gateway (octos chat / bus channels)
        // registered `run_pipeline`, so the LLM in serve mode received
        // `"No tools matched"` when it tried `activate_tools(["run_pipeline"])`
        // for `深度研究X` queries (per PR #930's ACT-DIRECTLY rule).
        //
        // The original M11-D split-out at `e01a07e4` (PR #764) called
        // this gap out as a follow-up but never landed; PR #903
        // restored 6 of 10 regressions and explicitly deferred this
        // one. PR #930's prompt rewrite — which makes the LLM call
        // `run_pipeline` directly rather than wrapping it in `spawn`
        // — turned the latent gap into an observable production
        // failure on the dspfac profile (May 13 2026).
        //
        // Profile scope is sufficient: `RunPipelineTool` only captures
        // `llm` / `memory` / `data_dir` / `plugin_dirs` / optional
        // `adaptive_router` / `provider_policy`, all of which are
        // profile-level. Per-session workspace context is threaded
        // separately via `PipelineHostContext` at execute time (see
        // `crates/octos-pipeline/src/tool.rs::execute`).
        //
        // `mark_spawn_only` keeps the tool out of LRU eviction and
        // tells the execution loop to background the call so the chat
        // bubble doesn't block on the long-running pipeline. The
        // message text mirrors session_actor.rs:2287-2291 verbatim.
        {
            // `RunPipelineTool::with_provider_router` takes
            // `octos_llm::ProviderRouter` (a sub-provider routing
            // registry assembled from `config.sub_providers` in the
            // gateway path). The serve path doesn't build that table
            // — the adaptive router that lives on `ProfileRuntime`
            // is `AdaptiveRouter`, a distinct concrete type for
            // top-level multi-provider QoS routing. Skipping
            // `with_provider_router` here is correct; the
            // `default_provider` we hand in (`llm`) is already wrapped
            // by `RetryProvider` → `ProviderChain` → `AdaptiveRouter`
            // when adaptive is configured, so per-node calls still
            // fan out through the adaptive layer.
            let pt = octos_pipeline::RunPipelineTool::new(
                llm.clone(),
                memory.clone(),
                data_dir.to_path_buf(),
                data_dir.to_path_buf(),
            )
            .with_provider_policy(config.tool_policy.clone())
            .with_plugin_dirs(plugin_dirs.clone())
            .with_octos_home(effective_octos_home.clone());
            tools.register(pt);
            tools.mark_spawn_only(
                "run_pipeline",
                Some(
                    "Pipeline started in background. The final result and any artifacts will be sent here when complete. You can keep chatting in the meantime."
                        .to_string(),
                ),
            );
        }

        // M11-F regression fix REG-2: restore the CronTool registration.
        //
        // Pre-M11-F `serve.rs::try_create_agent` built one `CronService`
        // per server rooted at `data_dir/cron.json`, called `start()`,
        // and registered `CronTool::with_context(cron_service, "api",
        // "")`. M11-F removed the helper without porting this wiring,
        // so `/api/chat` and the UI Protocol WS path silently lost the
        // `cron` tool. We restore it at the profile scope so cron jobs
        // are per-profile-isolated, matching the persistent stores
        // (`episodes.redb`, `memory.json`) that already live in
        // `data_dir`.
        //
        // The `cron_tx` here is a dummy channel: serve mode does not
        // route cron fires through the gateway-style inbound bus, so
        // the timer-driven sends will fill the bounded channel and be
        // dropped when the receiver is dropped at the end of this
        // function. That preserves the pre-M11-F semantics — cron CRUD
        // (`add` / `list` / `remove` / `enable` / `disable`) works in
        // serve mode but actual firing only happens under `octos
        // gateway`. We keep the `Arc<CronService>` alive by stashing it
        // on `ProfileRuntime::cron_service`; without that field the
        // tokio task `start()` spawned would be cancelled the moment
        // this function returned.
        let (cron_tx, _cron_rx) = tokio::sync::mpsc::channel(64);
        let cron_service = Arc::new(CronService::new(data_dir.join("cron.json"), cron_tx));
        cron_service.start();
        tools.register(CronTool::with_context(cron_service.clone(), "api", ""));

        // Step 17: re-apply tool policy AFTER plugin / memory-bank
        // registration so deny entries can target plugin-declared
        // tool names too (PR #688 follow-up — MEDIUM #4).
        if let Some(ref policy) = config.tool_policy {
            tools.apply_policy(policy);
        }

        // M11-F regression fix REG-1: auto-defer non-core tool groups
        // when the visible tool count is high so weaker LLMs (notably
        // GLM, kimi-k2 cold-start) don't return empty responses under
        // the weight of 30+ tool definitions.
        //
        // Mirrors `gateway_runtime.rs:1048-1070` byte-for-byte: defer
        // `group:admin` / `group:sessions` / `group:web` /
        // `group:runtime` / `group:media`, then register the
        // `ActivateToolsTool` when anything ended up deferred so the
        // LLM can pull a group back on demand mid-loop. Keeps research
        // (deep_search, deep_crawl) active by leaving `group:search`
        // alone — users call those directly often enough that hiding
        // them behind an extra round-trip would regress latency.
        let visible = tools.specs().len();
        if visible > 15 {
            for group in &[
                "group:admin",
                "group:sessions",
                "group:web",
                "group:runtime",
                "group:media",
            ] {
                tools.defer_group(group);
            }
            let after = tools.specs().len();
            info!(
                profile_id = %profile.id,
                before = visible,
                after,
                "auto-deferred non-core tool groups to reduce LLM tool-count load"
            );
        }
        if tools.has_deferred() {
            tools.register(octos_agent::ActivateToolsTool::new());
        }

        // Step 18: pre-assemble the profile-scope system prompt.
        //
        // This is the M11-F regression fix (#891). Before M11-F, serve
        // mode's `try_create_agent` helper called `build_system_prompt`
        // + the fragment-append loop inline, so every per-request agent
        // observed the SKILL.md guidance. M11-F deleted that helper and
        // routed everything through `SessionRuntime::bootstrap`, which
        // never re-derived the prompt — meaning `/api/chat` and the UI
        // Protocol WS path lost the mofa-fm "call fm_tts directly"
        // teaching (and any future skill-injected guidance).
        //
        // We assemble once per profile and stash it on the runtime so
        // every `SessionRuntime` bootstrapped from this profile inherits
        // the same prompt onto its per-session `Agent`. The gateway path
        // is unaffected — `profile_factory::build` continues to call
        // `build_system_prompt` itself for child-bot sub-agents, and
        // `plugin_prompt_fragments` is still populated for that path.
        //
        // `project_dir` is `data_dir` in serve mode. The bootstrap-files
        // assembly (`load_bootstrap_files`) reads AGENTS.md / SOUL.md /
        // USER.md from this dir — gateway uses its `--cwd` / project
        // dir, but serve mode has no project_dir concept, and the
        // per-profile data dir is the only profile-scoped directory we
        // can hand to the helper. Operators who want per-profile
        // bootstrap files drop them in `<data_dir>/`, which matches the
        // pre-M11-F serve-mode behavior.
        let skills_loader = build_account_skills_loader(data_dir);
        let mut system_prompt = build_system_prompt(
            profile.config.gateway.system_prompt.as_deref(),
            data_dir,
            data_dir,
            &memory_store,
            &skills_loader,
            &tool_config,
        )
        .await;
        for fragment in &plugin_result.prompt_fragments {
            system_prompt.push_str("\n\n");
            system_prompt.push_str(fragment);
        }

        // M11-F regression fix REG-3: assemble the lifecycle hook
        // executor once per profile and propagate the `Arc` onto every
        // per-session [`octos_agent::Agent`].
        //
        // Pre-M11-F `serve.rs::try_create_agent` merged `config.hooks +
        // plugin_result.hooks` into `Vec<HookConfig>`, wrapped it in
        // `HookExecutor::new`, and called `agent.with_hooks(...)`. M11-F
        // stored `plugin_hooks` on `ProfileRuntime` but never built the
        // executor or attached it. We do both here so the
        // `before_tool_call` / `after_tool_call` / `before_llm_call` /
        // `after_llm_call` hooks fire on the api-mode agent the same
        // way they fire under `octos gateway`.
        //
        // `SessionRuntime::bootstrap` reads this back and chains
        // `.with_hooks(executor.clone())` onto the per-session agent;
        // the per-request rebuild paths in `ws_standalone_agent` and
        // the UI Protocol per-turn builder do the same. Storing as
        // `Option<Arc<HookExecutor>>` preserves the pre-M11-F default
        // when neither source carries any hooks (the agent's
        // `hooks: None` field remains untouched).
        let mut all_hooks = config.hooks.clone();
        all_hooks.extend(plugin_result.hooks.clone());
        let hook_executor = if all_hooks.is_empty() {
            None
        } else {
            Some(Arc::new(HookExecutor::new(all_hooks)))
        };

        info!(
            profile_id = %profile.id,
            provider = %provider_name,
            model = %primary_model_id,
            plugin_count = plugin_result.tool_names.len(),
            tool_count = tools.specs().len(),
            system_prompt_len = system_prompt.len(),
            prompt_fragment_count = plugin_result.prompt_fragments.len(),
            hook_count = hook_executor.is_some() as u8,
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
            plugin_hooks: plugin_result.hooks.clone(),
            system_prompt,
            memory,
            memory_store,
            tool_config,
            cron_service: Some(cron_service),
            hook_executor,
        }))
    }
}

/// Tear down the profile-scope cron service when the runtime drops.
///
/// `CronService::start` spawns a tokio timer task that re-arms via
/// `Arc::clone(self)`, so the task self-holds an `Arc<CronService>`.
/// Without a `Drop` signal that flips `running = false` and aborts the
/// in-flight `tokio::time::sleep`, the timer task would survive
/// `ProfileRuntime` drop until its next scheduled fire (potentially
/// hours in the future), holding the service `Arc` alive past the
/// runtime that owns the profile's filesystem layout. We call the
/// synchronous [`CronService::shutdown_signal`] helper from `Drop` to
/// flip the flag and best-effort abort the JoinHandle; once the
/// running flag is `false` the reschedule chain in `on_timer` →
/// `arm_timer` terminates on the next tick and the task drops its
/// self-held `Arc`.
///
/// This is a code-quality fix (the cron task does no harm if it
/// continues firing — `inbound_tx` is a dummy channel whose receiver
/// is already dropped — but readers reasonably expect the runtime to
/// own its background tasks). Codex flagged this on the M11-F serve
/// regression bundle review.
impl Drop for ProfileRuntime {
    fn drop(&mut self) {
        if let Some(ref cron) = self.cron_service {
            cron.shutdown_signal();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::{
        GatewaySettings, LlmModelSelectionConfig, LlmProfileConfig, LlmRouteConfig, ProfileConfig,
    };
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

        let err = ProfileRuntime::bootstrap(&profile, &data_dir, None, BootstrapRole::Serve)
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

        let err = ProfileRuntime::bootstrap(&profile, &data_dir, None, BootstrapRole::Serve)
            .await
            .err()
            .expect("bootstrap must fail without a provider");
        assert!(
            err.to_string().contains("named-err"),
            "error should mention profile id: {err}",
        );
    }

    /// M11 regression fix (#891): `ProfileRuntime::bootstrap` must
    /// pre-assemble the full system prompt so every `SessionRuntime`
    /// built from it observes the SKILL.md prompt fragments. Without
    /// this, `/api/chat` and the UI Protocol WS path miss the
    /// mofa-fm SKILL.md (and any future skill-injected guidance) and
    /// the LLM falls back to its prior over the bare tool list.
    ///
    /// Fixture: a single skill (no executable required — the loader's
    /// "extras-only" path handles manifests with empty tools) that
    /// declares `prompts.include = ["SKILL.md"]` and ships a SKILL.md
    /// with a recognizable token. We then bootstrap a profile pointing
    /// at this skills dir and assert the token surfaces on
    /// `ProfileRuntime::system_prompt`.
    #[tokio::test]
    #[allow(unsafe_code)]
    async fn profile_runtime_bootstrap_includes_skill_prompt_fragments() {
        // Uniquely-named env var to avoid contention with other tests.
        const KEY_NAME: &str = "OCTOS_M11_891_TEST_API_KEY";
        // SAFETY: this env var name is unique to this test; nothing
        // else in the test suite reads or writes it. We also unset it
        // on the way out via the guard below.
        unsafe {
            std::env::set_var(KEY_NAME, "test-key-sk-fake");
        }
        struct EnvGuard;
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                // SAFETY: see set_var above.
                unsafe {
                    std::env::remove_var(KEY_NAME);
                }
            }
        }
        let _guard = EnvGuard;

        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("profiles").join("test").join("data");
        std::fs::create_dir_all(&data_dir).unwrap();

        // Plant a fixture skill with a recognizable token in SKILL.md.
        let skills_dir = data_dir.join("skills").join("test-fragment-skill");
        std::fs::create_dir_all(&skills_dir).unwrap();
        std::fs::write(
            skills_dir.join("manifest.json"),
            r#"{
                "name": "test-fragment-skill",
                "version": "1.0.0",
                "tools": [],
                "prompts": { "include": ["SKILL.md"] }
            }"#,
        )
        .unwrap();
        std::fs::write(
            skills_dir.join("SKILL.md"),
            "## Test Fragment Skill\n\nMARKER-FRAGMENT-XYZ — call fm_tts directly.\n",
        )
        .unwrap();

        let profile = UserProfile {
            id: "with-skill".to_string(),
            name: "With Skill".to_string(),
            enabled: true,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: ProfileConfig {
                gateway: GatewaySettings::default(),
                llm: Some(LlmProfileConfig {
                    primary: Some(LlmModelSelectionConfig {
                        family_id: Some("openai".to_string()),
                        model_id: Some("gpt-4o-mini".to_string()),
                        route: Some(LlmRouteConfig {
                            route_id: None,
                            label: None,
                            base_url: None,
                            api_key_env: Some(KEY_NAME.to_string()),
                            api_type: None,
                        }),
                        ..Default::default()
                    }),
                    fallbacks: Vec::new(),
                }),
                ..Default::default()
            },
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let rt = ProfileRuntime::bootstrap(&profile, &data_dir, None, BootstrapRole::Serve)
            .await
            .expect("bootstrap should succeed with a valid provider config");

        assert!(
            rt.system_prompt.contains("MARKER-FRAGMENT-XYZ"),
            "system_prompt should contain SKILL.md fragment; got: {}",
            rt.system_prompt
        );
        // Sanity: it's not _only_ the fragment — base prompt content
        // (e.g. the date marker injected by `build_system_prompt`)
        // should also be present.
        assert!(
            rt.system_prompt.contains("Current date:"),
            "system_prompt should also contain the base prompt body; got: {}",
            rt.system_prompt
        );
        // The plugin_prompt_fragments field also still carries the
        // raw fragment (gateway path consumers depend on it).
        assert!(
            rt.plugin_prompt_fragments
                .iter()
                .any(|f| f.contains("MARKER-FRAGMENT-XYZ")),
            "plugin_prompt_fragments should still surface the fragment for gateway",
        );
    }

    /// Regression test for the M11-F production crashloop tracked in
    /// `octos-org/octos#899`:
    ///
    /// `octos serve` and `octos gateway` are separate OS processes,
    /// both calling `ProfileRuntime::bootstrap` against the same
    /// per-profile data dir. Before this fix the second bootstrap
    /// crashed inside `EpisodeStore::open` with
    /// `redb::DatabaseError::DatabaseAlreadyOpen`, gateway exited,
    /// launchd auto-restarted it, and every profile crashlooped every
    /// ~2 seconds. Now the second bootstrap must succeed with the
    /// EpisodeStore in degraded mode.
    ///
    /// We simulate the cross-process race by bootstrapping the same
    /// profile twice in a row in the same test — the first handle on
    /// `rt_owner.memory` keeps the redb lock held while the second
    /// `ProfileRuntime::bootstrap` call runs, exercising the same
    /// `DatabaseAlreadyOpen` path the gateway subprocess hits in
    /// production.
    #[tokio::test]
    #[allow(unsafe_code)]
    async fn bootstrap_succeeds_when_redb_already_owned_by_sibling_process() {
        const KEY_NAME: &str = "OCTOS_GH899_TEST_API_KEY";
        // SAFETY: env var name is unique to this test.
        unsafe {
            std::env::set_var(KEY_NAME, "test-key-sk-fake");
        }
        struct EnvGuard;
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                // SAFETY: see set_var above.
                unsafe {
                    std::env::remove_var(KEY_NAME);
                }
            }
        }
        let _guard = EnvGuard;

        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("profiles").join("gh899").join("data");
        std::fs::create_dir_all(&data_dir).unwrap();

        let profile = UserProfile {
            id: "gh899".to_string(),
            name: "GH899".to_string(),
            enabled: true,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: ProfileConfig {
                gateway: GatewaySettings::default(),
                llm: Some(LlmProfileConfig {
                    primary: Some(LlmModelSelectionConfig {
                        family_id: Some("openai".to_string()),
                        model_id: Some("gpt-4o-mini".to_string()),
                        route: Some(LlmRouteConfig {
                            route_id: None,
                            label: None,
                            base_url: None,
                            api_key_env: Some(KEY_NAME.to_string()),
                            api_type: None,
                        }),
                        ..Default::default()
                    }),
                    fallbacks: Vec::new(),
                }),
                ..Default::default()
            },
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        // Simulates `octos serve`: bootstraps first as `Serve`,
        // takes the redb lock. `rt_owner` stays live for the whole
        // test so the lock remains held.
        let rt_owner = ProfileRuntime::bootstrap(&profile, &data_dir, None, BootstrapRole::Serve)
            .await
            .expect("first bootstrap (owner) should succeed");
        assert!(
            !rt_owner.memory.is_degraded(),
            "first bootstrap should hold the canonical redb",
        );

        // Simulates `octos gateway` running as a subprocess of serve:
        // hits the lock contention. Before #899 this returned
        // `Err(failed to open episode store ... Database already open)`.
        // Now it must succeed because the `Gateway` role opts into
        // the degraded fallback; the resulting handle's EpisodeStore
        // operates in degraded mode.
        let rt_sibling =
            ProfileRuntime::bootstrap(&profile, &data_dir, None, BootstrapRole::Gateway)
                .await
                .expect(
                    "second bootstrap (Gateway role) should succeed even \
                     when redb is already locked — this is the crashloop \
                     fix from issue #899",
                );
        assert!(
            rt_sibling.memory.is_degraded(),
            "Gateway-role bootstrap's episode store must be degraded",
        );
    }

    /// Companion to the crashloop test: a *second* `Serve`-role
    /// bootstrap must NOT silently degrade. This prevents a
    /// gateway-first/dev-workflow misordering from flipping canonical
    /// ownership to the gateway and quietly degrading serve's
    /// persistence — a concern codex raised on the round-1 review of
    /// #899. Serve must fail loudly so the operator sees the
    /// deployment misconfiguration.
    #[tokio::test]
    #[allow(unsafe_code)]
    async fn second_serve_role_bootstrap_fails_loudly_when_redb_already_owned() {
        const KEY_NAME: &str = "OCTOS_GH899_SERVE_STRICT_TEST_API_KEY";
        // SAFETY: env var name is unique to this test.
        unsafe {
            std::env::set_var(KEY_NAME, "test-key-sk-fake");
        }
        struct EnvGuard;
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                // SAFETY: see set_var above.
                unsafe {
                    std::env::remove_var(KEY_NAME);
                }
            }
        }
        let _guard = EnvGuard;

        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("profiles").join("gh899s").join("data");
        std::fs::create_dir_all(&data_dir).unwrap();

        let profile = UserProfile {
            id: "gh899s".to_string(),
            name: "GH899S".to_string(),
            enabled: true,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: ProfileConfig {
                gateway: GatewaySettings::default(),
                llm: Some(LlmProfileConfig {
                    primary: Some(LlmModelSelectionConfig {
                        family_id: Some("openai".to_string()),
                        model_id: Some("gpt-4o-mini".to_string()),
                        route: Some(LlmRouteConfig {
                            route_id: None,
                            label: None,
                            base_url: None,
                            api_key_env: Some(KEY_NAME.to_string()),
                            api_type: None,
                        }),
                        ..Default::default()
                    }),
                    fallbacks: Vec::new(),
                }),
                ..Default::default()
            },
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let _rt_owner = ProfileRuntime::bootstrap(&profile, &data_dir, None, BootstrapRole::Serve)
            .await
            .expect("first Serve bootstrap should succeed");

        // Second Serve-role bootstrap must error — never silently
        // degrade. This is the property codex's round-1 review asked
        // us to lock down.
        let err = ProfileRuntime::bootstrap(&profile, &data_dir, None, BootstrapRole::Serve)
            .await
            .err()
            .expect(
                "second Serve-role bootstrap must fail loudly on redb \
                 lock contention — silent degradation would risk \
                 flipping canonical ownership",
            );
        let msg = err.to_string() + " " + &format!("{err:?}");
        assert!(
            msg.contains("Database already open") || msg.contains("Cannot acquire lock"),
            "error must surface the redb lock contention; got: {err:?}",
        );
    }

    /// Build a minimal profile that bootstraps successfully against a
    /// stubbed env-var-backed API key. Used by the M11-F regression
    /// fix tests below to keep their fixture identical.
    fn fixture_profile(id: &str, key_env: &'static str) -> UserProfile {
        UserProfile {
            id: id.to_string(),
            name: id.to_string(),
            enabled: true,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: ProfileConfig {
                gateway: GatewaySettings::default(),
                llm: Some(LlmProfileConfig {
                    primary: Some(LlmModelSelectionConfig {
                        family_id: Some("openai".to_string()),
                        model_id: Some("gpt-4o-mini".to_string()),
                        route: Some(LlmRouteConfig {
                            route_id: None,
                            label: None,
                            base_url: None,
                            api_key_env: Some(key_env.to_string()),
                            api_type: None,
                        }),
                        ..Default::default()
                    }),
                    fallbacks: Vec::new(),
                }),
                ..Default::default()
            },
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    /// Set an env-var-backed fake API key with the supplied name for the
    /// duration of the test. Drops the var on scope exit so tests do not
    /// pollute the shared process environment.
    struct ScopedEnvKey {
        name: &'static str,
    }
    impl ScopedEnvKey {
        #[allow(unsafe_code)]
        fn set(name: &'static str) -> Self {
            // SAFETY: each test passes a uniquely-named env var that no
            // other test reads or writes; we also remove it on drop.
            unsafe {
                std::env::set_var(name, "test-key-sk-fake");
            }
            Self { name }
        }
    }
    impl Drop for ScopedEnvKey {
        #[allow(unsafe_code)]
        fn drop(&mut self) {
            // SAFETY: see set().
            unsafe {
                std::env::remove_var(self.name);
            }
        }
    }

    /// M11-F regression fix REG-2: `ProfileRuntime::bootstrap` must
    /// register the `cron` tool so `/api/chat` and the UI Protocol WS
    /// path see it under api mode, matching the pre-M11-F serve flow
    /// (`serve.rs:1207`). The `Arc<CronService>` must also be retained
    /// on the runtime so the tokio timer task `start()` spawned does
    /// not get dropped when bootstrap returns.
    #[tokio::test]
    async fn profile_runtime_bootstrap_registers_cron_tool() {
        let _key = ScopedEnvKey::set("OCTOS_M11F_REG2_KEY");
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("profile-data");
        std::fs::create_dir_all(&data_dir).unwrap();

        let profile = fixture_profile("reg2", "OCTOS_M11F_REG2_KEY");
        let rt = ProfileRuntime::bootstrap(&profile, &data_dir, None, BootstrapRole::Serve)
            .await
            .expect("bootstrap should succeed");

        assert!(
            rt.tool_specs.specs().iter().any(|s| s.name == "cron"),
            "cron tool must be registered on the base ToolRegistry",
        );
        assert!(
            rt.cron_service.is_some(),
            "Arc<CronService> must be retained on ProfileRuntime so the \
             timer task survives bootstrap",
        );
    }

    /// M11-F regression fix REG-1: when the visible tool count exceeds
    /// 15 (the gateway threshold), bootstrap must defer the five
    /// non-core groups and register `activate_tools`. This mirrors
    /// `gateway_runtime.rs:1048-1070` and is essential for weaker LLMs
    /// (kimi-k2, GLM) that return empty responses under tool-spec
    /// overload.
    #[tokio::test]
    async fn profile_runtime_bootstrap_defers_groups_and_registers_activate_tools() {
        let _key = ScopedEnvKey::set("OCTOS_M11F_REG1_KEY");
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("profile-data");
        std::fs::create_dir_all(&data_dir).unwrap();

        let profile = fixture_profile("reg1", "OCTOS_M11F_REG1_KEY");
        let rt = ProfileRuntime::bootstrap(&profile, &data_dir, None, BootstrapRole::Serve)
            .await
            .expect("bootstrap should succeed");

        // The base builtin set + memory tools + cron exceeds 15 visible
        // tools, so the defer pass MUST fire and the activate_tools tool
        // MUST be registered.
        assert!(
            rt.tool_specs.has_deferred(),
            "bootstrap should auto-defer non-core groups when visible > 15",
        );
        assert!(
            rt.tool_specs
                .specs()
                .iter()
                .any(|s| s.name == "activate_tools"),
            "activate_tools must be registered when any tool is deferred",
        );
    }

    /// M11-F regression fix REG-5: bootstrap's plugin_dirs must include
    /// the *global* `~/.octos/plugins` and `~/.octos/skills` (via
    /// `Config::plugin_dirs_from_project`) so admin-installed skills
    /// are visible to every profile, matching the pre-M11-F serve
    /// behaviour at `serve.rs:1224`.
    ///
    /// We construct an `octos_home` override and plant a fake skill
    /// under `<octos_home>/plugins/`, then assert the resulting
    /// `plugin_dirs` set includes that directory. We do not require
    /// the skill to load (loaders gate on a manifest); we only assert
    /// the dir was *scanned*.
    #[tokio::test]
    async fn profile_runtime_bootstrap_includes_global_plugin_dirs() {
        let _key = ScopedEnvKey::set("OCTOS_M11F_REG5_KEY");
        let tmp = tempfile::tempdir().unwrap();
        let octos_home = tmp.path().join("octos-home");
        let data_dir = octos_home.join("profiles").join("reg5").join("data");
        std::fs::create_dir_all(&data_dir).unwrap();

        // Plant the global "<octos_home>/plugins" dir so
        // `Config::plugin_dirs_from_project` picks it up.
        let global_plugins = octos_home.join("plugins");
        std::fs::create_dir_all(&global_plugins).unwrap();

        let profile = fixture_profile("reg5", "OCTOS_M11F_REG5_KEY");
        let rt =
            ProfileRuntime::bootstrap(&profile, &data_dir, Some(&octos_home), BootstrapRole::Serve)
                .await
                .expect("bootstrap should succeed");

        assert!(
            rt.plugin_dirs.contains(&global_plugins),
            "plugin_dirs should include `<octos_home>/plugins`; got: {:?}",
            rt.plugin_dirs
        );
    }

    /// Section B (codex review round-3): the host's `plugins.require_signed`
    /// policy must reach the per-profile bootstrap so an unsigned skill
    /// installed under `<data_dir>/skills/` is rejected even when the
    /// profile JSON omits the flag. We plant an unsigned skill and assert
    /// it does NOT load when `bootstrap_with_host_plugins` is invoked
    /// with `host_plugins.require_signed = true`.
    #[cfg(unix)]
    #[tokio::test]
    async fn profile_runtime_bootstrap_honours_host_require_signed() {
        use std::os::unix::fs::PermissionsExt;

        let _key = ScopedEnvKey::set("OCTOS_HOST_SIGN_KEY");
        let tmp = tempfile::tempdir().unwrap();
        let octos_home = tmp.path().join("octos-home");
        let data_dir = octos_home.join("profiles").join("sigtest").join("data");
        let skills_dir = data_dir.join("skills");
        std::fs::create_dir_all(&skills_dir).unwrap();

        // Plant an unsigned per-profile skill — manifest omits sha256.
        let plugin_dir = skills_dir.join("unsigned-skill");
        std::fs::create_dir(&plugin_dir).unwrap();
        std::fs::write(
            plugin_dir.join("manifest.json"),
            r#"{
                "name": "unsigned-skill",
                "version": "1.0",
                "tools": [{"name": "ut", "description": "d"}]
            }"#,
        )
        .unwrap();
        let exec_path = plugin_dir.join("unsigned-skill");
        std::fs::write(&exec_path, b"#!/bin/sh\necho unsigned").unwrap();
        std::fs::set_permissions(&exec_path, std::fs::Permissions::from_mode(0o755)).unwrap();

        let profile = fixture_profile("sigtest", "OCTOS_HOST_SIGN_KEY");
        let host_plugins = crate::config::PluginsConfig {
            require_signed: true,
        };

        let rt = ProfileRuntime::bootstrap_with_host_plugins(
            &profile,
            &data_dir,
            Some(&octos_home),
            BootstrapRole::Serve,
            Some(&host_plugins),
        )
        .await
        .expect("bootstrap should succeed (the rejection only suppresses the plugin)");

        let specs = rt.tool_specs.specs();
        let registered: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        assert!(
            !registered.iter().any(|n| n == &"ut"),
            "unsigned skill tool `ut` must NOT load when host strict policy is on; \
             registered: {registered:?}"
        );
    }

    /// M11-F regression fix REG-2 follow-up (codex review): when the
    /// `ProfileRuntime` drops, the cron service must observe a
    /// shutdown signal so the self-armed timer task does not survive
    /// the runtime owning its filesystem layout. The signal is
    /// synchronous (we call it from `Drop`) and flips
    /// `CronService::running` to false; the next reschedule tick
    /// inside `on_timer` → `arm_timer` then short-circuits and the
    /// timer task drops its self-held `Arc<CronService>`.
    ///
    /// We assert by holding a weak reference to the inner
    /// `Arc<CronService>` after dropping the `ProfileRuntime` and
    /// checking that `running` flipped. The strong-count check (i.e.
    /// "service deallocated") would race with the in-flight timer
    /// task, so we settle for the durable observable (`running` flag).
    #[tokio::test]
    async fn profile_runtime_drop_signals_cron_shutdown() {
        let _key = ScopedEnvKey::set("OCTOS_M11F_REG2_DROP_KEY");
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("profile-data");
        std::fs::create_dir_all(&data_dir).unwrap();

        let profile = fixture_profile("reg2-drop", "OCTOS_M11F_REG2_DROP_KEY");
        let rt = ProfileRuntime::bootstrap(&profile, &data_dir, None, BootstrapRole::Serve)
            .await
            .expect("bootstrap should succeed");

        let cron = rt
            .cron_service
            .clone()
            .expect("cron_service must be Some after bootstrap");
        // Hold an extra Arc so we can inspect the service after the
        // runtime drops.
        drop(rt);

        // After drop, the runtime's `Drop` impl signals shutdown — the
        // running flag must be false, which causes the timer's next
        // reschedule to terminate and the self-held Arc to release.
        assert!(
            !cron.is_running(),
            "Drop must flip CronService::running to false",
        );
    }

    /// M11-F regression fix REG-3: when `config.hooks` is non-empty,
    /// bootstrap must build a `HookExecutor` and stash the `Arc` on
    /// `ProfileRuntime::hook_executor` so per-session agents (and
    /// per-request rebuild paths) can inherit it.
    ///
    /// Since the per-profile `Config` derived from `UserProfile` does
    /// not currently expose `hooks` (those come from the top-level
    /// `Config`, not the profile), this test asserts the inverse: an
    /// empty hook set yields `None`, and the bootstrap structurally
    /// builds and exposes the field. End-to-end hook propagation onto
    /// the per-session agent is asserted by
    /// `session.rs::session_runtime_agent_inherits_profile_hooks`.
    #[tokio::test]
    async fn profile_runtime_bootstrap_initializes_hook_executor_field() {
        let _key = ScopedEnvKey::set("OCTOS_M11F_REG3_KEY");
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("profile-data");
        std::fs::create_dir_all(&data_dir).unwrap();

        let profile = fixture_profile("reg3", "OCTOS_M11F_REG3_KEY");
        let rt = ProfileRuntime::bootstrap(&profile, &data_dir, None, BootstrapRole::Serve)
            .await
            .expect("bootstrap should succeed");

        // With no config-side hooks and no plugin-side hooks the field
        // must be None so the agent's default `hooks: None` is kept.
        assert!(
            rt.hook_executor.is_none(),
            "hook_executor must be None when neither config nor plugins supply hooks",
        );
    }
}
