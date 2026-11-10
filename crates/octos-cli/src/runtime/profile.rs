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
use octos_agent::plugins::LoadedSkillAction;
use octos_agent::{
    HookExecutor, PluginLoadOptions, PluginLoadResult, PluginLoader, SandboxConfig,
    ToolConfigStore, ToolPolicy, ToolRegistry, create_sandbox,
};
use octos_bus::{CronDelivery, CronService};
use octos_llm::{AdaptiveRouter, LlmProvider, QosCatalog};
use octos_memory::{EpisodeStore, MemoryStore};
use tracing::{info, warn};

use crate::commands::chat;
use crate::commands::gateway::build_system_prompt;
use crate::commands::gateway::profile_factory::{profile_plugin_env, profile_search_provider_keys};
use crate::config::Config;
use crate::cron_tool::CronTool;
use crate::profiles::{ReviewConfig, UserProfile, config_from_profile};
use crate::qos_catalog::{ExporterMode, build_adaptive_provider_chain};
use crate::skills_scope::{
    build_account_skills_loader, discover_ominix_url, push_runtime_plugin_env,
};

/// Create directories that background profile services expect to exist for
/// the entire runtime lifetime.
///
/// Inbox producers also create this directory defensively before writing, but
/// the serve-side steer sweep starts before the first producer may run. Eager
/// creation keeps an unused inbox from looking like an I/O failure while
/// preserving the sweep's warning for a directory that disappears later.
fn ensure_profile_runtime_directories(data_dir: &Path) -> Result<()> {
    let inbox_dir = data_dir.join("inbox");
    std::fs::create_dir_all(&inbox_dir).wrap_err_with(|| {
        format!(
            "failed to create profile inbox directory {}",
            inbox_dir.display()
        )
    })
}

/// Immutable inputs needed to rebuild only a profile's plugin-derived layer.
/// Long-lived stores, providers, schedulers, and profile services are reused
/// from the existing [`ProfileRuntime`].
#[derive(Clone)]
pub struct ProfilePluginReloadConfig {
    base_tools: Arc<ToolRegistry>,
    plugin_dirs: Vec<PathBuf>,
    plugin_env: Vec<(String, String)>,
    work_dir: PathBuf,
    synthesis_config: Option<octos_agent::plugins::SynthesisConfig>,
    require_signed: bool,
    verified_cache_dir: PathBuf,
    tool_policy: Option<ToolPolicy>,
    context_filter: Vec<String>,
    host_hooks: Vec<octos_agent::HookConfig>,
    gateway_system_prompt: Option<String>,
    skill_filter: Option<octos_agent::SkillFilter>,
}

/// Shared owner for profile services whose shutdown cannot be tied to one
/// replaceable `ProfileRuntime` allocation.
pub struct ProfileRuntimeLifecycle {
    cron_service: Option<Arc<CronService>>,
}

impl Drop for ProfileRuntimeLifecycle {
    fn drop(&mut self) {
        if let Some(ref cron) = self.cron_service {
            cron.shutdown_signal();
        }
    }
}

/// Long-lived profile resources a replacement runtime takes over from the
/// runtime it replaces instead of reopening them. `episodes.redb` admits a
/// single writer, so reopening it while an in-flight turn still holds the
/// previous runtime fails; sharing the handles makes a configuration change
/// (model, key, route) take effect immediately without that lock.
#[derive(Clone)]
pub(crate) struct SharedProfileResources {
    memory: Arc<EpisodeStore>,
    memory_store: Arc<MemoryStore>,
    recall: Arc<octos_memory::RecallStore>,
    embedder: Option<Arc<dyn octos_llm::EmbeddingProvider>>,
    tool_config: Arc<ToolConfigStore>,
    cron_service: Option<Arc<CronService>>,
    runtime_lifecycle: Option<Arc<ProfileRuntimeLifecycle>>,
    /// Handed over, not shared: it is stopped so the replacement can restart
    /// the sweep with its own provider.
    memory_refresh: Option<Arc<crate::memory_refresh::MemoryRefreshService>>,
}

/// Whether stores opened under `previous` stay valid under `next`: the
/// embedder and recall width they were opened with are unchanged.
fn configs_share_storage(previous: &Config, next: &Config) -> bool {
    let embedding = |config: &Config| serde_json::to_value(&config.embedding).ok();
    let recall_dimension = |config: &Config| {
        config
            .memory
            .as_ref()
            .and_then(|memory| memory.recall_dimension)
    };
    if embedding(previous) != embedding(next)
        || recall_dimension(previous) != recall_dimension(next)
    {
        return false;
    }
    // An embedder authenticated by a profile env var must not keep a
    // credential the new configuration replaced.
    next.embedding
        .as_ref()
        .and_then(|embedding| embedding.api_key_env.as_deref())
        .is_none_or(|name| previous.env_vars.get(name) == next.env_vars.get(name))
}

/// The resources of a runtime a configuration commit removed, awaiting its
/// replacement. See [`ProfileRuntime::retire`].
pub(crate) struct RetiredProfileRuntime {
    data_dir: PathBuf,
    custom_session_root: bool,
    // Boxed: callers hold the retiree across awaits, and `Config` is large.
    config: Box<Config>,
    resources: SharedProfileResources,
}

impl RetiredProfileRuntime {
    /// Whether a runtime for `config` rooted at `data_dir` can take over these
    /// stores: same storage location, and nothing that shapes how the stores
    /// were opened (embedder, recall width) has changed.
    pub(crate) fn can_share_with(&self, data_dir: &Path, config: &Config) -> bool {
        self.data_dir == data_dir
            && !self.custom_session_root
            && configs_share_storage(&self.config, config)
    }
}

/// #1935 — the reserved `sub_providers` lane key that selects the
/// INDEPENDENT goal-completion verifier model.
pub const GOAL_VERIFIER_LANE_KEY: &str = "goal_verifier";

/// #1935 — build the INDEPENDENT goal-completion verifier provider from the
/// profile's `sub_providers` (lane key [`GOAL_VERIFIER_LANE_KEY`]).
///
/// The verifier used to run on the SAME provider/lane as the agent being
/// graded, which weakens the "don't grade your own homework" property.
/// Configuring a `sub_providers` entry keyed `goal_verifier` routes every
/// goal-completion verification (the `goal_update` tool, the gateway and
/// serve autonomous sentinel accountants, and the serve interactive sentinel)
/// through a separately-routed model instead.
///
/// Follows the established lane idioms:
/// - Lane selection is LAST-wins on duplicate keys, mirroring
///   `ProviderRouter::register_with_full_meta` and the peer model lane.
/// - The lane's `api_key_env` is applied UNCONDITIONALLY (even when `None`):
///   a lane that omits its own key must CLEAR the primary's `api_key_env`
///   and fall back to its OWN provider's default env var, never borrow the
///   primary provider's credential (#peer-model semantics).
/// - The built provider is wrapped in `RetryProvider`, like every other lane.
///
/// CREDENTIAL ISOLATION (#1935 codex blocker): the key resolver's normal
/// chain consults the GLOBAL provider auth store BEFORE the configured env
/// var (`Config::resolve_api_key`), so a same-provider lane whose
/// `api_key_env` is missing or typo'd would silently grade with the
/// PRIMARY's login credential — defeating the verifier's independence with
/// no signal. The lane build therefore sets `bypass_auth_store` (the same
/// explicit-key-must-win escape hatch octos-ffi uses): the lane's key
/// resolves ONLY from its declared `api_key_env` (profile `env_vars` /
/// keychain, then process env — never the auth store). An unset/empty var
/// fails the build with an explicit error.
///
/// Returns `Ok(None)` only when no `goal_verifier` lane is configured, which
/// preserves the pre-#1935 self-grading default. A configured lane that fails
/// to build returns `Err`; production callers use
/// [`build_goal_verifier_provider_or_degraded`] to retain a persistent
/// fail-closed provider instead of silently self-grading.
/// The LLM provider family a resolved profile config selects: the explicit
/// `provider` (populated from `llm.primary` by `config_from_profile`), else
/// the family detected from the model id.
pub(crate) fn configured_provider_name(config: &Config) -> Option<String> {
    config.provider.clone().or_else(|| {
        config
            .model
            .as_deref()
            .and_then(crate::config::detect_provider)
            .map(String::from)
    })
}

struct FailedGoalVerifierProvider {
    provider: String,
    model: String,
    error: String,
}

#[async_trait::async_trait]
impl LlmProvider for FailedGoalVerifierProvider {
    async fn chat(
        &self,
        _messages: &[octos_core::Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &octos_llm::ChatConfig,
    ) -> Result<octos_llm::ChatResponse> {
        Err(eyre::Report::msg(self.error.clone()))
    }

    fn model_id(&self) -> &str {
        &self.model
    }

    fn provider_name(&self) -> &str {
        &self.provider
    }
}

pub fn build_goal_verifier_provider(config: &Config) -> Result<Option<Arc<dyn LlmProvider>>> {
    let Some(sp) = config
        .sub_providers
        .iter()
        .rev()
        .find(|sp| sp.key == GOAL_VERIFIER_LANE_KEY)
    else {
        return Ok(None);
    };
    let mut sp_config = config.clone();
    sp_config.api_key_env = sp.api_key_env.clone();
    // #1935 codex blocker — never let the global auth store satisfy the
    // verifier lane's credential (see the doc comment above).
    sp_config.bypass_auth_store = true;
    match chat::create_provider_with_api_type(
        &sp.provider,
        &sp_config,
        sp.model.clone(),
        sp.base_url.clone(),
        sp.api_type.as_deref(),
    ) {
        Ok(provider) => Ok(Some(Arc::new(octos_llm::RetryProvider::new(provider)))),
        Err(error) => Err(error).wrap_err_with(|| {
            format!(
                "configured goal_verifier construction failed (operation=build_goal_verifier provider={} model={})",
                sp.provider,
                sp.model.as_deref().unwrap_or("<default>")
            )
        }),
    }
}

pub fn build_goal_verifier_provider_or_degraded(config: &Config) -> Option<Arc<dyn LlmProvider>> {
    match build_goal_verifier_provider(config) {
        Ok(provider) => provider,
        Err(error) => {
            let sp = config
                .sub_providers
                .iter()
                .rev()
                .find(|sp| sp.key == GOAL_VERIFIER_LANE_KEY)
                .expect("a build error requires a configured goal_verifier lane");
            warn!(
                lane = GOAL_VERIFIER_LANE_KEY,
                provider = %sp.provider,
                error = %error,
                "goal_verifier construction failed; completion verification will fail closed"
            );
            Some(Arc::new(FailedGoalVerifierProvider {
                provider: sp.provider.clone(),
                model: sp
                    .model
                    .clone()
                    .unwrap_or_else(|| "goal-verifier-unavailable".to_string()),
                error: format!("{error:#}"),
            }))
        }
    }
}

pub fn select_goal_verifier_provider(
    configured: &Option<Arc<dyn LlmProvider>>,
    session_provider: Arc<dyn LlmProvider>,
) -> Arc<dyn LlmProvider> {
    configured.clone().unwrap_or(session_provider)
}

/// All long-lived state that belongs to a single profile within the
/// current host process.
///
/// One `ProfileRuntime` per `(host process, profile_id)`. The host
/// process is `octos serve`, `octos gateway` (each subprocess), or
/// `octoscode` — every entry point that today reads a [`UserProfile`]
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

    /// Optional local-frontend transcript root. Ephemeral chat keeps profile
    /// memory/tools rooted at `data_dir`, while session JSONL and context/task
    /// sidecars use this temporary directory even with per-cwd storage enabled.
    /// Ordinary Serve/Gateway/ACP runtimes leave this unset.
    pub session_store_root: Option<PathBuf>,

    /// The profile's resolved [`crate::config::Config`] (as produced by
    /// `config_from_profile` at bootstrap, with host memory/plugins merged).
    /// Most runtime state is pre-extracted into the typed fields below; this
    /// is retained for the few paths that must resolve a lane provider
    /// LAZILY from `config.sub_providers` (with the profile's credential /
    /// timeout config), e.g. a peer session that runs its turns on a named
    /// `sub_provider` model lane (`peers/<slug>/model`). Kept whole rather
    /// than re-deriving a `Config` off disk on the hot path.
    pub config: crate::config::Config,

    /// Per-profile delivered-copy retention bounds (50 GiB default, no entry
    /// limit unless configured). Kept out of global/default profile merging.
    pub artifact_retention: octos_bus::session_artifacts::ArtifactRetentionPolicy,

    /// The fully-wrapped LLM provider chain for this profile.
    /// Includes retry, provider failover, and (if `adaptive_router`
    /// is `Some`) adaptive routing. Every session for this profile
    /// uses this same provider.
    pub llm: Arc<dyn LlmProvider>,

    /// #1935 — the INDEPENDENT goal-completion verifier lane, resolved at
    /// profile build from the `sub_providers` entry keyed
    /// [`GOAL_VERIFIER_LANE_KEY`] (see [`build_goal_verifier_provider`]).
    /// `None` only when the lane is unconfigured: verifier call sites then
    /// retain the pre-#1935 self-grading default. A configured construction
    /// failure is stored as a persistent fail-closed provider.
    pub goal_verifier_llm: Option<Arc<dyn LlmProvider>>,

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

    /// Credential pool (M6.5, F-005) built by
    /// `build_adaptive_provider_chain` from `config.credential_pool`.
    /// When [`Self::adaptive_router`] is `Some` this is the SAME `Arc`
    /// attached to every router slot, so the session runtime and the
    /// router share one rotation state (T-0549). `None` when
    /// unconfigured — legacy single-credential flow.
    pub credential_pool: Option<Arc<octos_llm::PersistentCredentialPool>>,

    /// Content classifier (M6.6, F-005) built by
    /// `build_adaptive_provider_chain` from `config.content_routing`
    /// (`enabled: true` only) — the SAME `Arc` installed on the router
    /// (T-0549). `None` keeps the strong-only routing default.
    pub content_classifier: Option<Arc<octos_llm::ContentClassifier>>,

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

    /// Configured agent iteration budget (`config.max_iterations`) that
    /// sessions — and the sub-agents they spawn — inherit. `None` falls back
    /// to [`AgentConfig`]'s default. Captured here so the session runtime
    /// honors the configured value instead of a hardcoded cap (which silently
    /// starved spawned sub-agents doing multi-step work).
    pub max_iterations: Option<u32>,

    /// Local frontend overrides applied by the canonical session bootstrap.
    /// OUP still owns per-turn intent, context, persistence and cancellation.
    pub session_defaults: Option<octos_agent::AgentConfig>,
    /// Optional operator-selected coding tool/agent profile (chat and ACP).
    pub agent_profile: Option<Arc<octos_agent::profile::ProfileDefinition>>,

    /// Post-edit formatting opt-in (`config.format_after_edit`, issue
    /// #1774) that per-session agents inherit. When true, successful
    /// `edit_file` / `write_file` / `diff_edit` calls run the file's
    /// language formatter and echo the formatted content in the tool
    /// result. Default: false.
    pub format_after_edit: bool,

    /// #1768: opt-in workspace-snapshot config per-session agents use to
    /// build their `SnapshotManager` (None/disabled = no snapshots).
    pub snapshots: Option<octos_agent::SnapshotConfig>,

    /// The base [`ToolRegistry`] template — builtins + plugins +
    /// MCP agents + the LRU pin set — but **NOT** workspace-bound.
    /// Sessions clone this and call `with_workspace_root` to obtain
    /// a workspace-bound registry.
    pub tool_specs: Arc<ToolRegistry>,

    /// Tool names contributed by loaded plugins. Useful for gateway's
    /// pin-as-base step (so plugin tools never get LRU-evicted) and
    /// for diagnostics. Populated from `PluginLoadResult::tool_names`.
    pub plugin_tool_names: Vec<String>,

    /// UI-callable actions accepted by the same canonical load that registered
    /// their owning plugin tools.
    pub skill_actions: Vec<LoadedSkillAction>,

    /// Live MCP connections used by `tool_specs`, retained for real
    /// connection/failure/closed status instead of inferring from tool names.
    pub mcp_client: Option<Arc<octos_agent::McpClient>>,

    /// Immutable source for mutation-time plugin-layer replacement.
    pub plugin_reload: Option<Arc<ProfilePluginReloadConfig>>,

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
    /// The same prompt split at the memory slot — per-session agents
    /// compose `pre → [memory segment] → post` to keep the pre-refactor
    /// precedence (memory before skills/tool guidance).
    pub prompt_parts: crate::commands::gateway::prompt::GatewayPromptParts,

    /// Hook configurations contributed by loaded plugins (skill
    /// manifests can declare `before_tool_call` / `after_tool_call` /
    /// `before_llm_call` / `after_llm_call` hooks). Gateway merges
    /// these with `config.hooks` to build its `HookExecutor`. Captured
    /// alongside `plugin_tool_names` / `plugin_prompt_fragments` so
    /// gateway can reuse the bootstrap's `PluginLoadResult` without
    /// re-running plugin discovery.
    pub plugin_hooks: Vec<octos_agent::HookConfig>,

    /// Profile-owned coding review fanout template. `None` means the
    /// AppUI `/review` path should use its built-in default
    /// specialists. Keeping this on `ProfileRuntime` lets the review
    /// workflow resolve specialists from the same profile runtime that
    /// owns model, memory, sandbox, and tools.
    pub review_config: Option<ReviewConfig>,
    /// Phase 4 (docs/ROBRIX-PHASE4-APPROVAL-FLOW-ADR.md): per-profile
    /// human-approval rules, converted once at bootstrap and inherited by
    /// every per-session Agent this profile spawns.
    pub human_approval_rules: Option<octos_agent::HumanApprovalRules>,

    /// Long-lived [`EpisodeStore`] for this profile (redb at
    /// `<data_dir>/episodes.redb`). Shared across all sessions of
    /// the profile so task summaries written in one session are
    /// recallable from another.
    pub memory: Arc<EpisodeStore>,

    /// Long-lived [`MemoryStore`] (MEMORY.md + daily notes + recent
    /// memories window) for this profile.
    pub memory_store: Arc<MemoryStore>,

    /// Long-lived Recall/Knowledge index (`<data_dir>/recall.redb` +
    /// `recall-index/`) — app records, mirrored episodes and bank pages
    /// behind `memory_search` / `memory_load` and `memory/ingest`
    /// (docs/adr/personal-memory-tiers.md).
    pub recall: Arc<octos_memory::RecallStore>,

    /// The profile's embedding provider (None when no `embedding`
    /// config and no resolvable key). Sessions hand this to
    /// SpawnTool / DelegateTool so worker agents embed the episodes
    /// they save and run hybrid scored+filtered recall — without it
    /// workers stored episodes vectorless and recall silently skipped.
    pub embedder: Option<Arc<dyn octos_llm::EmbeddingProvider>>,
    /// Resolved `memory.max_inject_tokens` for per-session memory segments.
    pub memory_inject_tokens: usize,
    /// Resolved `memory.refresh.enabled` — gates the capture-policy text in
    /// the memory segment and the per-turn refresh provider.
    pub memory_refresh_enabled: bool,
    /// Background memory-refresh sweep (extraction over idle sessions).
    /// `Some` only when `memory.refresh.enabled` and this process won the
    /// profile's refresh lock; dropping the runtime stops the sweep and
    /// releases the lock.
    pub memory_refresh: Option<Arc<crate::memory_refresh::MemoryRefreshService>>,

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

    /// Shared shutdown owner retained across replacement runtimes.
    pub runtime_lifecycle: Option<Arc<ProfileRuntimeLifecycle>>,

    /// Reserved host extension hook. Octos does not register a DOT engine.
    pub pipeline_factory: Option<Arc<dyn crate::session_actor::PipelineToolFactory + Send + Sync>>,

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

    /// RFC-3 (#1292) — per-topic model lane routing config (overrides
    /// only; built-in defaults always apply on top of this).
    ///
    /// When `Some`, the session-actor and the WS turn handler use this
    /// to resolve `session.topic()` to a [`octos_llm::Lane`] and pass
    /// it to the chat call via [`octos_llm::with_lane_context`]. When
    /// `None`, the built-in defaults from `octos_llm::lane` still apply
    /// for the well-known prefixes (slides / site / podcast / research
    /// / code); profiles that haven't opted into RFC-3 see no behavior
    /// change because the [`octos_llm::AdaptiveRouter`] silently falls
    /// through when zero candidates match.
    pub lane_routing: Option<octos_llm::LaneRoutingConfig>,

    /// The profile's resolved voice (ASR/TTS) configuration, captured at
    /// bootstrap from `config.voice` (defaults applied when the profile has no
    /// `voice` block). The serve voice-turn path reads this for the STT
    /// language hint and the TTS voice / route (`tts_provider`) so those are
    /// configurable per profile instead of hardcoded.
    pub voice: crate::config::VoiceConfig,
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

async fn build_profile_plugin_layer(
    profile_id: &str,
    reload: &ProfilePluginReloadConfig,
    fail_on_loader_error: bool,
) -> Result<(ToolRegistry, PluginLoadResult)> {
    let mut tools = reload.base_tools.snapshot_excluding(&[]);
    let mut plugin_result = PluginLoadResult::default();
    if !reload.plugin_dirs.is_empty() {
        let load_result = PluginLoader::load_into_with_options_and_filter(
            &mut tools,
            &reload.plugin_dirs,
            &reload.plugin_env,
            PluginLoadOptions {
                work_dir: Some(&reload.work_dir),
                synthesis_config: reload.synthesis_config.clone(),
                require_signed: reload.require_signed,
                verified_cache_dir: Some(reload.verified_cache_dir.clone()),
            },
            reload.skill_filter.as_ref(),
        );
        match load_result {
            Ok(result) if fail_on_loader_error && !result.plugin_errors.is_empty() => {
                let details = result
                    .plugin_errors
                    .iter()
                    .map(|error| format!("{}: {}", error.plugin_dir.display(), error.message))
                    .collect::<Vec<_>>()
                    .join("; ");
                return Err(eyre::eyre!(
                    "plugin loading failed during profile {profile_id} reload: {details}"
                ));
            }
            Ok(result) => plugin_result = result,
            Err(error) if fail_on_loader_error => {
                return Err(error).wrap_err_with(|| {
                    format!("plugin loading failed during profile {profile_id} reload")
                });
            }
            Err(error) => warn!(profile_id, %error, "plugin loading failed"),
        }
        octos_agent::plugins::register_http_skills_on_startup(&mut tools, &reload.plugin_dirs)
            .await
            .wrap_err_with(|| {
                format!("HTTP tool discovery failed during profile {profile_id} plugin reload")
            })?;
    }

    if !plugin_result.mcp_servers.is_empty() {
        match octos_agent::McpClient::start(&plugin_result.mcp_servers).await {
            Ok(client) => client.register_tools(&mut tools),
            Err(error) => warn!(
                profile_id,
                %error,
                "skill MCP initialization failed"
            ),
        }
    }
    if let Some(ref policy) = reload.tool_policy {
        tools.apply_policy(policy);
    }
    if !reload.context_filter.is_empty() {
        tools.set_context_filter(reload.context_filter.clone());
    }
    Ok((tools, plugin_result))
}

impl ProfileRuntime {
    /// Bind project plugins at session scope. Local adapters may open multiple
    /// cwds on one profile; none may inherit another project's executables.
    pub(crate) async fn for_workspace(self: &Arc<Self>, workspace: &Path) -> Result<Arc<Self>> {
        if self.session_defaults.is_none() {
            return Ok(self.clone());
        }
        let Some(reload) = &self.plugin_reload else {
            return Ok(self.clone());
        };
        let mut dirs = Config::plugin_dirs_from_project(&workspace.join(".octos"));
        if dirs.is_empty() {
            return Ok(self.clone());
        }
        for dir in &reload.plugin_dirs {
            if !dirs.contains(dir) {
                dirs.push(dir.clone());
            }
        }
        let mut reload = (**reload).clone();
        reload.plugin_dirs = dirs;
        self.rebuild_plugin_layer_using(&Arc::new(reload)).await
    }

    /// Reapply the effective envelope after cwd rebinding or dynamic tool
    /// registration. A cloned registry must never resurrect excluded tools.
    pub(crate) fn apply_tool_envelope(&self, tools: &mut ToolRegistry) {
        if let Some(policy) = &self.tool_policy {
            tools.apply_policy(policy);
        }
        if let Some(profile) = &self.agent_profile {
            profile.apply_to_registry(tools);
            if !profile.tools.allows("run_pipeline") {
                tools.retain(|name| name != "run_pipeline");
            }
        }
    }

    /// Rebuild plugin-derived tools, trusted actions, prompt fragments, and
    /// hooks while sharing all long-lived profile resources with `self`.
    pub async fn rebuild_plugin_layer(self: &Arc<Self>) -> Result<Arc<Self>> {
        let reload = self.plugin_reload.as_ref().ok_or_else(|| {
            eyre::eyre!(
                "profile '{}' does not retain plugin reload inputs",
                self.profile_id
            )
        })?;
        self.rebuild_plugin_layer_using(reload).await
    }

    async fn rebuild_plugin_layer_using(
        self: &Arc<Self>,
        reload: &Arc<ProfilePluginReloadConfig>,
    ) -> Result<Arc<Self>> {
        let (mut tools, plugin_result) =
            build_profile_plugin_layer(&self.profile_id, reload, true).await?;
        let pipeline_factory = self.pipeline_factory.as_ref().map(|factory| {
            factory
                .with_plugin_dirs(reload.plugin_dirs.clone())
                .unwrap_or_else(|| factory.clone())
        });

        tools.register(
            octos_agent::RecallMemoryTool::new(self.memory_store.clone())
                .with_recall(self.recall.clone(), self.embedder.clone()),
        );
        tools.register(octos_agent::MemorySearchTool::new(
            self.recall.clone(),
            self.embedder.clone(),
        ));
        tools.register(octos_agent::MemoryLoadTool::new(
            self.recall.clone(),
            self.memory_store.clone(),
        ));
        tools.register(octos_agent::SaveMemoryTool::new(self.memory_store.clone()));
        tools.register(octos_agent::RecordMemoryUseTool::new(
            self.memory_store.clone(),
        ));
        if self.memory_refresh_enabled {
            tools.register(octos_agent::MemoryNoteTool::new(self.memory_store.clone()));
        }
        if let Some(ref factory) = pipeline_factory {
            tools.register_arc(factory.create(&self.default_sandbox));
            tools.mark_spawn_only(
                "run_pipeline",
                Some(
                    "Pipeline started in background. The final result and any artifacts will be sent here when complete. You can keep chatting in the meantime."
                        .to_string(),
                ),
            );
        }
        if let Some(cron) = self.tool_specs.get("cron") {
            tools.register_arc(cron.clone());
        }
        if let Some(ref policy) = reload.tool_policy {
            tools.apply_policy(policy);
        }

        self.apply_tool_envelope(&mut tools);
        let skills_loader = build_account_skills_loader(&self.data_dir)
            .with_skill_filter(reload.skill_filter.clone());
        let mut prompt_parts = build_system_prompt(
            reload.gateway_system_prompt.as_deref(),
            &self.data_dir,
            &self.data_dir,
            &skills_loader,
            &self.tool_config,
        )
        .await;
        for fragment in &plugin_result.prompt_fragments {
            prompt_parts.post_memory.push_str("\n\n");
            prompt_parts.post_memory.push_str(fragment);
        }
        if let Some(profile) = &self.agent_profile
            && let Some(template) = &profile.system_prompt_template
            && let Some(template) =
                crate::commands::load_profile_prompt_template(&profile.name, template)
        {
            prompt_parts.pre_memory = template;
            prompt_parts.uses_bundled_base = false;
        }
        let system_prompt = prompt_parts.joined();

        // #2129: the coding default hooks (cargo check / eslint / ruff after
        // edits) merge at THIS shared assembly point so every host —
        // bootstrap sessions, WS per-turn rebuilds, chat, gateway — gets
        // them, not just one consumer. They are SELF-GATING: each declares a
        // path_filter (fires only on matching source edits) and requires_bin
        // (skips when the checker is absent), so a podcast workspace never
        // runs cargo. Defaults first, operator hooks after, per the
        // coding_default_hooks contract. The hook child's working directory
        // comes from the per-turn payload cwd (the workspace root), not the
        // executor, so one profile-level executor serves every session.
        let mut all_hooks = octos_agent::workspace_policy::coding_default_hooks();
        all_hooks.extend(reload.host_hooks.clone());
        all_hooks.extend(plugin_result.hooks.clone());
        // #2153 finding 2: coalesce a burst of edits so a whole-project
        // `cargo check` (up to its 60s timeout) does not run once per edit.
        // The window is measured from the previous check's completion, so
        // several `edit_file` calls in one assistant turn collapse to a single
        // check while a later edit (a new thinking step) still gets a fresh
        // one. Breaker + debounce state are per session (see HookExecutor).
        let hook_executor = Some(Arc::new(
            HookExecutor::new(all_hooks)
                .with_after_event_debounce(std::time::Duration::from_millis(2000)),
        ));
        let skills_dir_candidate = self.data_dir.join("skills");

        // #20b — install the main-tree sovereignty provider for the shell
        // tool. The main tree is the process working directory (the tree the
        // serve/master was launched from); the closure re-reads its branch and
        // the caller goal's ledger per command, fail-open when solo/unowned.
        // Process-global: the LAST profile to bootstrap wins the shared slot,
        // which is correct for the single-profile serve/master loop this
        // guards; multi-profile hosts re-install the same rule with their own
        // data dir.
        if let Ok(cwd) = std::env::current_dir() {
            crate::autonomy::agent_orchestrator::InProcessAgentOrchestrator::install_main_tree_sovereignty(
                self.data_dir.clone(),
                cwd,
            );
        }

        Ok(Arc::new(Self {
            profile_id: self.profile_id.clone(),
            data_dir: self.data_dir.clone(),
            session_store_root: self.session_store_root.clone(),
            config: self.config.clone(),
            artifact_retention: self.artifact_retention,
            llm: self.llm.clone(),
            goal_verifier_llm: self.goal_verifier_llm.clone(),
            adaptive_router: self.adaptive_router.clone(),
            runtime_qos_catalog: self.runtime_qos_catalog.clone(),
            credential_pool: self.credential_pool.clone(),
            content_classifier: self.content_classifier.clone(),
            primary_model_id: self.primary_model_id.clone(),
            provider_name: self.provider_name.clone(),
            credentials: self.credentials.clone(),
            skills_dir: skills_dir_candidate
                .exists()
                .then_some(skills_dir_candidate),
            plugin_env_template: self.plugin_env_template.clone(),
            tool_policy: self.tool_policy.clone(),
            default_sandbox: self.default_sandbox.clone(),
            max_iterations: self.max_iterations,
            session_defaults: self.session_defaults.clone(),
            agent_profile: self.agent_profile.clone(),
            format_after_edit: self.format_after_edit,
            snapshots: self.snapshots.clone(),
            tool_specs: Arc::new(tools),
            plugin_tool_names: plugin_result.tool_names.clone(),
            skill_actions: plugin_result.loaded_actions.clone(),
            mcp_client: self.mcp_client.clone(),
            plugin_reload: Some(reload.clone()),
            plugin_dirs: reload.plugin_dirs.clone(),
            plugin_prompt_fragments: plugin_result.prompt_fragments.clone(),
            plugin_hooks: plugin_result.hooks.clone(),
            review_config: self.review_config.clone(),
            human_approval_rules: self.human_approval_rules.clone(),
            system_prompt,
            prompt_parts,
            memory: self.memory.clone(),
            memory_store: self.memory_store.clone(),
            recall: self.recall.clone(),
            embedder: self.embedder.clone(),
            memory_inject_tokens: self.memory_inject_tokens,
            memory_refresh_enabled: self.memory_refresh_enabled,
            memory_refresh: self.memory_refresh.clone(),
            tool_config: self.tool_config.clone(),
            cron_service: self.cron_service.clone(),
            runtime_lifecycle: self.runtime_lifecycle.clone(),
            pipeline_factory,
            hook_executor,
            lane_routing: self.lane_routing.clone(),
            voice: self.voice.clone(),
        }))
    }

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
    /// Returns an error when runtime directories cannot be created, when the
    /// LLM provider construction fails (typically a missing API key), when the
    /// redb episode store cannot open, or when the tool config store cannot be
    /// opened.
    /// Plugin / MCP loading failures are logged at `warn` and do not
    /// fail bootstrap (the profile still serves with builtins only).
    pub async fn bootstrap(
        profile: &UserProfile,
        data_dir: &Path,
        octos_home: Option<&Path>,
        role: BootstrapRole,
    ) -> Result<Arc<Self>> {
        Self::bootstrap_with_host_plugins(profile, data_dir, octos_home, role, None, None, None)
            .await
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
        host_voice: Option<&crate::config::VoiceConfig>,
        host_memory: Option<&crate::config::MemoryConfig>,
    ) -> Result<Arc<Self>> {
        Self::bootstrap_replacing(
            profile,
            data_dir,
            octos_home,
            role,
            host_plugins,
            host_voice,
            host_memory,
            &mut None,
        )
        .await
    }

    /// [`Self::bootstrap_with_host_plugins`] for a configuration change of a
    /// profile whose previous stores may still be open (an in-flight turn's
    /// agent holds them even after the runtime itself is gone). When the new
    /// configuration keeps the same storage, the new runtime takes over the
    /// retired stores and long-lived services; every configuration-derived
    /// part is rebuilt exactly as a cold bootstrap would. When it does not,
    /// `previous` is released (set to `None`) before the cold bootstrap so
    /// this process no longer pins the stores; otherwise it is left for the
    /// caller to keep on failure.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn bootstrap_replacing(
        profile: &UserProfile,
        data_dir: &Path,
        octos_home: Option<&Path>,
        role: BootstrapRole,
        host_plugins: Option<&crate::config::PluginsConfig>,
        host_voice: Option<&crate::config::VoiceConfig>,
        host_memory: Option<&crate::config::MemoryConfig>,
        previous: &mut Option<RetiredProfileRuntime>,
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
        // Host memory settings apply field-by-field when the profile doesn't
        // override them (same host-default pattern as plugins/voice). A
        // profile serialized with an empty `memory: {}` block must still
        // inherit the host budget.
        crate::config::merge_host_memory_into_profile(&mut config.memory, host_memory);

        let shared = match previous {
            Some(retired) if retired.can_share_with(data_dir, &config) => {
                Some(retired.resources.clone())
            }
            _ => {
                *previous = None;
                None
            }
        };
        Self::bootstrap_resolved_sharing(
            profile, data_dir, octos_home, role, config, host_voice, false, None, shared,
        )
        .await
    }

    /// Keep this runtime's stores and long-lived services for its
    /// replacement. Held strongly: an in-flight turn's agent can keep the
    /// single-writer episode store open after every runtime handle is gone.
    #[cfg_attr(not(feature = "api"), allow(dead_code))]
    pub(crate) fn retire(&self) -> RetiredProfileRuntime {
        RetiredProfileRuntime {
            data_dir: self.data_dir.clone(),
            custom_session_root: self.session_store_root.is_some(),
            config: Box::new(self.config.clone()),
            resources: SharedProfileResources {
                memory: self.memory.clone(),
                memory_store: self.memory_store.clone(),
                recall: self.recall.clone(),
                embedder: self.embedder.clone(),
                tool_config: self.tool_config.clone(),
                cron_service: self.cron_service.clone(),
                runtime_lifecycle: self.runtime_lifecycle.clone(),
                memory_refresh: self.memory_refresh.clone(),
            },
        }
    }

    /// Local OUP adapters use the same assembler with their already-resolved
    /// CLI config. Do not round-trip this through ProfileConfig: doing so loses
    /// custom endpoints, API styles and explicit CLI policy overrides.
    /// A supplied provider is an embedding seam, not a second runtime path.
    #[allow(clippy::too_many_arguments)]
    #[cfg_attr(not(feature = "api"), allow(dead_code))]
    pub(crate) async fn bootstrap_resolved(
        profile: &UserProfile,
        data_dir: &Path,
        octos_home: Option<&Path>,
        role: BootstrapRole,
        config: Config,
        host_voice: Option<&crate::config::VoiceConfig>,
        no_retry: bool,
        provider_override: Option<Arc<dyn LlmProvider>>,
    ) -> Result<Arc<Self>> {
        Self::bootstrap_resolved_sharing(
            profile,
            data_dir,
            octos_home,
            role,
            config,
            host_voice,
            no_retry,
            provider_override,
            None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn bootstrap_resolved_sharing(
        profile: &UserProfile,
        data_dir: &Path,
        octos_home: Option<&Path>,
        role: BootstrapRole,
        config: Config,
        host_voice: Option<&crate::config::VoiceConfig>,
        no_retry: bool,
        provider_override: Option<Arc<dyn LlmProvider>>,
        shared: Option<SharedProfileResources>,
    ) -> Result<Arc<Self>> {
        ensure_profile_runtime_directories(data_dir).wrap_err_with(|| {
            format!(
                "failed to initialize runtime directories for profile '{}'",
                profile.id
            )
        })?;

        // Step 2: resolve the provider name. `config_from_profile`
        // populates `provider`/`model` from `llm.primary` when set,
        // else falls back to `detect_provider(model)`.
        let model = config.model.clone();
        let base_url = config.base_url.clone();
        let provider_name = configured_provider_name(&config).ok_or_else(|| {
            eyre::eyre!("profile '{}' has no LLM provider configured", profile.id)
        })?;

        // Step 3: build the LLM provider chain.
        let base_provider = match provider_override {
            Some(provider) => provider,
            None => chat::create_provider(&provider_name, &config, model, base_url).wrap_err_with(
                || format!("failed to create LLM provider for profile '{}'", profile.id),
            )?,
        };
        let primary_model_id = base_provider.model_id().to_string();
        let bundle = build_adaptive_provider_chain(
            base_provider,
            &config,
            data_dir,
            no_retry,
            ExporterMode::Spawn,
        )?;
        let llm = bundle.llm.clone();
        let adaptive_router = bundle.adaptive_router.clone();
        let runtime_qos_catalog = bundle.runtime_qos_catalog.clone();
        // T-0549: the SAME pool/classifier objects the router holds.
        let credential_pool = bundle.credential_pool.clone();
        let content_classifier = bundle.content_classifier.clone();

        // Step 4: open the memory stores.
        //
        // The opener variant depends on the caller's role (see
        // [`BootstrapRole`] docs). Serve must hold the canonical
        // EpisodeStore; gateway falls back to a degraded handle when
        // serve already owns the redb lock so it doesn't crashloop on
        // every startup. Tracked by issue #899.
        //
        // The embedder is resolved FIRST because the episodic HNSW index is
        // built at one fixed width and silently drops any vector of a
        // different length. Sizing it from the configured provider is what
        // makes a non-1536-d embedder (e.g. in-process EmbeddingGemma at 768)
        // actually reach the vector lane instead of degrading to BM25-only.
        let (memory, memory_store, recall, embedder, tool_config) = if let Some(shared) =
            shared.as_ref()
        {
            (
                shared.memory.clone(),
                shared.memory_store.clone(),
                shared.recall.clone(),
                shared.embedder.clone(),
                shared.tool_config.clone(),
            )
        } else {
            let embedder = chat::create_embedder_async(&config)
                .await
                .map(|e| e as Arc<dyn octos_llm::EmbeddingProvider>);
            let index_dimension = embedder
                .as_ref()
                .map_or(octos_memory::EPISODIC_INDEX_DIMENSION, |e| e.dimension());

            let memory_open_result = match role {
                BootstrapRole::Serve => {
                    EpisodeStore::open_with_dimension(data_dir, index_dimension).await
                }
                BootstrapRole::Gateway => {
                    EpisodeStore::open_or_degraded_with_dimension(data_dir, index_dimension).await
                }
            };
            let memory = Arc::new(memory_open_result.wrap_err_with(|| {
                format!("failed to open episode store for profile '{}'", profile.id)
            })?);
            let memory_store = Arc::new(MemoryStore::open(data_dir).await.wrap_err_with(|| {
                format!("failed to open memory store for profile '{}'", profile.id)
            })?);
            let recall = open_recall_store(data_dir, &config, embedder.as_deref())
                .await
                .wrap_err_with(|| {
                    format!("failed to open recall store for profile '{}'", profile.id)
                })?;

            // Step 5: tool config store.
            let tool_config =
                Arc::new(ToolConfigStore::open(data_dir).await.wrap_err_with(|| {
                    format!(
                        "failed to open tool config store for profile '{}'",
                        profile.id
                    )
                })?);
            (memory, memory_store, recall, embedder, tool_config)
        };

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
        crate::commands::gateway::profile_factory::apply_resolved_profile_llm_env(
            &mut plugin_env_template,
            &config,
            &profile.updated_at.to_rfc3339(),
        );
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
        let mcp_client = if !config.mcp_servers.is_empty() {
            match octos_agent::McpClient::start_scoped(
                &config.mcp_servers,
                Some(&profile.id),
                Some(data_dir),
            )
            .await
            {
                Ok(client) => {
                    let client = Arc::new(client);
                    client.register_tools(&mut tools);
                    Some(client)
                }
                Err(e) => {
                    warn!(profile_id = %profile.id, error = %e, "MCP initialization failed");
                    None
                }
            }
        } else {
            None
        };
        let plugin_base_tools = Arc::new(tools.snapshot_excluding(&[]));

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
        let mut plugin_dirs = Config::plugin_dirs_from_project(&effective_octos_home);
        let platform_dir = effective_octos_home.join(octos_agent::bootstrap::PLATFORM_SKILLS_DIR);
        if platform_dir.exists() && !plugin_dirs.contains(&platform_dir) {
            plugin_dirs.push(platform_dir);
        }
        let profile_skills_dir = data_dir.join("skills");
        if !plugin_dirs.contains(&profile_skills_dir) {
            plugin_dirs.push(profile_skills_dir);
        }
        // --- Skill layering v1 ---
        // Resolve the profile's inherited skill-selection layer (parent +
        // global defaults already merged by `resolve_runtime_profile`) into a
        // crate-agnostic filter handed to BOTH the plugin loader (tool specs)
        // and the SkillsLoader (prompt / content injection) below. `None` ⇒ no
        // skills layer ⇒ every discovered skill loads, exactly as before.
        let skill_filter = profile.config.skills.as_ref().map(|s| s.to_agent_filter());
        if profile.config.skills.is_some() {
            let discovered_skill_ids: Vec<String> = build_account_skills_loader(data_dir)
                .list_skills()
                .await
                .map(|skills| skills.into_iter().map(|s| s.name).collect())
                .unwrap_or_default();
            let catalog =
                crate::skills_scope::resolve_profile_skills(profile, &discovered_skill_ids);
            if catalog.has_disabled() {
                info!(
                    profile_id = %profile.id,
                    mode = ?catalog.mode,
                    disabled = ?catalog.disabled,
                    "skill layering: installed skills disabled by profile config"
                );
            }
        }
        let plugin_reload = Arc::new(ProfilePluginReloadConfig {
            base_tools: plugin_base_tools,
            plugin_dirs: plugin_dirs.clone(),
            plugin_env: plugin_env_template.clone(),
            work_dir: plugin_work_dir,
            synthesis_config:
                crate::commands::gateway::profile_factory::build_research_synthesis_config(&config),
            require_signed: config.plugins.require_signed,
            verified_cache_dir: effective_octos_home.join("cache").join("verified"),
            tool_policy: config.tool_policy.clone(),
            context_filter: config.context_filter.clone(),
            host_hooks: config.hooks.clone(),
            gateway_system_prompt: profile.config.gateway.system_prompt.clone(),
            skill_filter: skill_filter.clone(),
        });
        let (rebuilt_tools, plugin_result) =
            build_profile_plugin_layer(&profile.id, &plugin_reload, false).await?;
        tools = rebuilt_tools;

        // RFC-0 (#1289): LRU tool deferral was removed — the base-tool pin
        // list is no longer needed; every enabled tool is emitted every turn.

        // Memory bank tools — registered profile-side so every
        // session inherits the same memory_store.
        tools.register(
            octos_agent::RecallMemoryTool::new(memory_store.clone())
                .with_recall(recall.clone(), embedder.clone()),
        );
        tools.register(octos_agent::MemorySearchTool::new(
            recall.clone(),
            embedder.clone(),
        ));
        tools.register(octos_agent::MemoryLoadTool::new(
            recall.clone(),
            memory_store.clone(),
        ));
        tools.register(octos_agent::SaveMemoryTool::new(memory_store.clone()));
        tools.register(octos_agent::RecordMemoryUseTool::new(memory_store.clone()));
        if crate::config::MemoryConfig::refresh_enabled(config.memory.as_ref()) {
            tools.register(octos_agent::MemoryNoteTool::new(memory_store.clone()));
        }

        // DOT graph execution is retired from Octos. Leave the host extension
        // hook empty, including for spawned agents and plugin reloads.
        let pipeline_factory = None;

        #[derive(Clone)]
        struct ServeCronDelivery {
            profile_id: String,
        }

        #[async_trait::async_trait]
        impl CronDelivery for ServeCronDelivery {
            #[cfg(feature = "api")]
            async fn deliver(&self, job: &octos_bus::CronJob) -> Result<(), String> {
                let Some(session_id) = job.origin.session_id.as_deref() else {
                    return Err("session_not_found".to_owned());
                };
                if job.origin.profile_id.as_deref() != Some(self.profile_id.as_str()) {
                    return Err("session_profile_mismatch".to_owned());
                }
                let session = octos_core::SessionKey(session_id.to_owned());
                // A newly opened session has no transcript file yet; its
                // rehydrated workspace is the authoritative live-session check.
                if crate::api::ui_protocol_transport::session_workspace_root_for_profile(
                    Some(&self.profile_id),
                    &session,
                )
                .is_none()
                {
                    return Err("session_open_required".to_owned());
                }
                let scheduled_fire_at_ms = job
                    .state
                    .delivery_fire_at_ms
                    .or(job.state.next_run_at_ms)
                    .unwrap_or(job.created_at_ms);
                // Ordinary timer delivery is not "late" merely because the
                // scheduled instant has arrived. Only startup catch-up or a
                // delayed retry beyond the grace window is marked late.
                let late = chrono::Utc::now().timestamp_millis()
                    > scheduled_fire_at_ms.saturating_add(60_000);
                match crate::autonomy::agent_orchestrator::default_agent_orchestrator()
                    .enqueue_cron_fire_continuation(
                        &session,
                        &self.profile_id,
                        &job.id,
                        scheduled_fire_at_ms,
                        &job.payload.message,
                        late,
                    )
                {
                    crate::autonomy::master_continuation_scheduler::MasterContinuationEnqueueOutcome::Queued(_)
                    | crate::autonomy::master_continuation_scheduler::MasterContinuationEnqueueOutcome::Duplicate { .. } => Ok(()),
                }
            }

            #[cfg(not(feature = "api"))]
            async fn deliver(&self, _job: &octos_bus::CronJob) -> Result<(), String> {
                let _ = &self.profile_id;
                Err("session_open_required".to_owned())
            }
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
        // Serve uses an acknowledged delivery adapter: an exact existing
        // session's durable continuation queue must accept the occurrence
        // before the store records ok/deletes it. The gateway keeps its own
        // inbound mpsc path unchanged. We keep the `Arc<CronService>` alive
        // by stashing it on `ProfileRuntime::cron_service`; without that field
        // the tokio task `start()` spawned would be cancelled on return.
        // A replacement runtime keeps the running service and its shutdown
        // owner: a second `start()` on the same `cron.json` would fire jobs
        // twice, and dropping the previous lifecycle would stop the timer.
        let (cron_service, runtime_lifecycle) = match shared.as_ref().and_then(|shared| {
            Some((
                shared.cron_service.clone()?,
                shared.runtime_lifecycle.clone()?,
            ))
        }) {
            Some((cron_service, lifecycle)) => (cron_service, Some(lifecycle)),
            None => {
                let cron_service = Arc::new(CronService::new_with_delivery(
                    data_dir.join("cron.json"),
                    Arc::new(ServeCronDelivery {
                        profile_id: profile.id.clone(),
                    }),
                ));
                cron_service.start();
                let lifecycle = Arc::new(ProfileRuntimeLifecycle {
                    cron_service: Some(cron_service.clone()),
                });
                (cron_service, Some(lifecycle))
            }
        };
        tools.register(CronTool::with_context(cron_service.clone(), "api", ""));
        // Hand the same service to the AppUI orchestrator so `loop/delete` can
        // reap the cron jobs a loop created. Without this the orchestrator has
        // no cron handle at all and the reap silently does nothing.
        #[cfg(feature = "api")]
        crate::autonomy::agent_orchestrator::default_agent_orchestrator()
            .set_cron_service(cron_service.clone());
        // #1935 — resolve the INDEPENDENT goal-completion verifier lane
        // (`sub_providers` key `goal_verifier`) once at profile build. It is
        // threaded into the `goal_update` tool below and stored on the
        // runtime so the serve turn accountants (autonomous + interactive
        // sentinel) verify on it too. `None` ⇒ the lane is genuinely
        // unconfigured and the pre-#1935 self-grading default remains;
        // configured construction failures stay fail-closed.
        let goal_verifier_llm = build_goal_verifier_provider_or_degraded(&config);

        // #1696 — structured goal tools: goal_get (objective + remaining
        // budget) and goal_update (model-owned complete|blocked ONLY,
        // executor-enforced). Session resolved per-call from
        // ToolContext::parent_session_key; profile scope pinned here.
        // The orchestrator itself now lives in the un-gated `crate::autonomy`
        // (so `goal_tool` compiles without `api`), but REGISTRATION stays
        // `api`-gated: only the serve/AppUI runtime drives goal turns today.
        // Wiring `octos chat` onto the same engine is a separate change.
        #[cfg(feature = "api")]
        {
            // Peer-agent-based goal: pass the profile's data_dir so
            // `goal_get` can aggregate BOTH live peer findings (under
            // `<data_dir>/peers/<slug>/goal`) AND durable ledger findings
            // (under `<data_dir>/goal-ledgers/<goal_id>.db`).
            tools.register(
                crate::goal_tool::GoalGetTool::new(profile.id.clone())
                    .with_data_dir(data_dir.to_path_buf()),
            );
            tools.register(crate::goal_tool::GoalCreateTool::new(profile.id.clone()));
            {
                // #1935 — completion claims are graded on the independent
                // verifier lane when one is configured.
                let mut goal_update = crate::goal_tool::GoalUpdateTool::new(profile.id.clone())
                    .with_data_dir(data_dir.to_path_buf());
                if let Some(ref verifier) = goal_verifier_llm {
                    goal_update = goal_update.with_verifier_provider(verifier.clone());
                }
                tools.register(goal_update);
            }
            // #1857 PR 5a — the goal keeper's fleet controls: decompose the
            // objective onto a durable fleet (`goal_plan`) and launch its ready
            // tasks onto the live worker pool (`goal_dispatch`). `goal_get`
            // (above) folds in the fleet plan view + self-detects completion.
            tools.register(crate::goal_tool::GoalPlanTool::new(profile.id.clone()));
            tools.register(crate::goal_tool::GoalDispatchTool::new(profile.id.clone()));
            // PR B — the keeper's escalation controls: approve a worker's
            // mid-task grant-widen request (`goal_grant`, resumes the task) or
            // refuse it (`goal_deny`, fails the task). A Blocked task surfaced by
            // goal_get needs exactly one of these. #1964 — `goal_deny` carries
            // the profile data_dir like goal_get/goal_update, so a deny that
            // renders the fleet un-completable syncs the per-goal ledger.
            tools.register(crate::goal_tool::GoalGrantTool::new(profile.id.clone()));
            tools.register(
                crate::goal_tool::GoalDenyTool::new(profile.id.clone())
                    .with_data_dir(data_dir.to_path_buf()),
            );
            // #1977 — zero-token event watchers. Keeper-gated like the
            // goal_plan family (peers cannot arm monitors). `monitor_create`
            // carries the profile data_dir so the probe's sandboxed cwd and
            // the monitor-notes wake sidecar both root there.
            tools.register(
                crate::goal_tool::MonitorCreateTool::new(profile.id.clone())
                    .with_data_dir(data_dir.to_path_buf()),
            );
            tools.register(crate::goal_tool::MonitorListTool::new(profile.id.clone()));
            tools.register(crate::goal_tool::MonitorDeleteTool::new(profile.id.clone()));
        }

        // Step 17: re-apply tool policy AFTER plugin / memory-bank
        // registration so deny entries can target plugin-declared
        // tool names too (PR #688 follow-up — MEDIUM #4).
        if let Some(ref policy) = config.tool_policy {
            tools.apply_policy(policy);
        }

        // RFC-0 (#1289): LRU tool deferral + the `activate_tools` meta-tool
        // were removed. Every enabled tool is now emitted every turn (full
        // schema), so the former auto-defer-non-core-groups pass is gone.

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
        let skills_loader = build_account_skills_loader(data_dir).with_skill_filter(skill_filter);
        let max_inject_tokens =
            crate::config::MemoryConfig::effective_max_inject_tokens(config.memory.as_ref());
        let memory_refresh_enabled =
            crate::config::MemoryConfig::refresh_enabled(config.memory.as_ref());
        let mut prompt_parts = build_system_prompt(
            profile.config.gateway.system_prompt.as_deref(),
            data_dir,
            data_dir,
            &skills_loader,
            &tool_config,
        )
        .await;
        for fragment in &plugin_result.prompt_fragments {
            prompt_parts.post_memory.push_str("\n\n");
            prompt_parts.post_memory.push_str(fragment);
        }
        let system_prompt = prompt_parts.joined();
        let prompt_parts_for_runtime = prompt_parts.clone();

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
        // #2129: the coding default hooks (cargo check / eslint / ruff after
        // edits) merge at THIS shared assembly point so every host —
        // bootstrap sessions, WS per-turn rebuilds, chat, gateway — gets
        // them, not just one consumer. They are SELF-GATING: each declares a
        // path_filter (fires only on matching source edits) and requires_bin
        // (skips when the checker is absent), so a podcast workspace never
        // runs cargo. Defaults first, operator hooks after, per the
        // coding_default_hooks contract. The hook child's working directory
        // comes from the per-turn payload cwd (the workspace root), not the
        // executor, so one profile-level executor serves every session.
        let mut all_hooks = octos_agent::workspace_policy::coding_default_hooks();
        all_hooks.extend(config.hooks.clone());
        all_hooks.extend(plugin_result.hooks.clone());
        // #2153 finding 2: coalesce a burst of edits so a whole-project
        // `cargo check` (up to its 60s timeout) does not run once per edit.
        // The window is measured from the previous check's completion, so
        // several `edit_file` calls in one assistant turn collapse to a single
        // check while a later edit (a new thinking step) still gets a fresh
        // one. Breaker + debounce state are per session (see HookExecutor).
        let hook_executor = Some(Arc::new(
            HookExecutor::new(all_hooks)
                .with_after_event_debounce(std::time::Duration::from_millis(2000)),
        ));

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

        // Validate the per-profile approval policy with the SAME checks the
        // top-level config load applies, so a bad profile rule fails fast
        // instead of gating unexpectedly / creating unanswerable or
        // instantly-expiring requests (review finding #4).
        if let Some(policy) = profile.config.approval_policy.as_ref() {
            policy
                .validate()
                .wrap_err("invalid profile approval_policy")?;
        }

        // Recall/Knowledge index upkeep (docs/adr/personal-memory-tiers.md):
        // mirror bank pages, embed records that arrived without vectors,
        // and apply the heat policy. Runs off the bootstrap path so a large
        // bank never delays the first turn; errors only log.
        // A replacement shares stores whose upkeep already ran.
        if shared.is_none() {
            spawn_recall_maintenance(
                recall.clone(),
                memory_store.clone(),
                embedder.clone(),
                profile.id.clone(),
                config.embedding.is_none(),
            );
        }

        // Start the background memory-refresh sweep when enabled. The
        // flock decides ownership when serve and gateway share a profile
        // dir; the loser just logs and skips. A replacement first stops the
        // sweep it takes over so the restarted one uses the new provider.
        if let Some(previous) = shared
            .as_ref()
            .and_then(|shared| shared.memory_refresh.as_ref())
        {
            previous.shutdown().await;
        }
        let memory_refresh = if memory_refresh_enabled {
            let refresh_cfg = config.memory.as_ref().and_then(|m| m.refresh.as_ref());
            crate::memory_refresh::MemoryRefreshService::try_start(
                data_dir.to_path_buf(),
                memory_store.clone(),
                crate::memory_refresh::resolve_refresh_provider(
                    &config,
                    llm.clone(),
                    refresh_cfg.and_then(|r| r.extract_model.as_deref()),
                ),
                crate::memory_refresh::resolve_refresh_provider(
                    &config,
                    llm.clone(),
                    refresh_cfg.and_then(|r| r.consolidate_model.as_deref()),
                ),
                crate::config::MemoryRefreshConfig::knobs(config.memory.as_ref()),
            )
            .map(Arc::new)
        } else {
            None
        };

        Ok(Arc::new(Self {
            profile_id: profile.id.clone(),
            data_dir: data_dir.to_path_buf(),
            session_store_root: None,
            // Retained whole for lazy per-lane provider resolution (e.g. a
            // peer running on a named `sub_provider` model lane); the typed
            // fields below carry the pre-extracted hot-path state.
            config: config.clone(),
            artifact_retention: profile.config.artifact_retention.unwrap_or_default(),
            llm,
            goal_verifier_llm,
            adaptive_router,
            runtime_qos_catalog,
            credential_pool,
            content_classifier,
            primary_model_id,
            provider_name,
            credentials,
            skills_dir,
            plugin_env_template,
            tool_policy: config.tool_policy.clone(),
            default_sandbox,
            max_iterations: config.max_iterations,
            session_defaults: None,
            agent_profile: None,
            format_after_edit: config.format_after_edit,
            snapshots: config.snapshots.clone(),
            tool_specs: Arc::new(tools),
            plugin_tool_names: plugin_result.tool_names.clone(),
            skill_actions: plugin_result.loaded_actions.clone(),
            mcp_client,
            plugin_reload: Some(plugin_reload),
            plugin_dirs,
            plugin_prompt_fragments: plugin_result.prompt_fragments.clone(),
            plugin_hooks: plugin_result.hooks.clone(),
            review_config: profile.config.review.clone(),
            human_approval_rules: profile
                .config
                .approval_policy
                .as_ref()
                .map(|policy| policy.to_runtime_rules()),
            system_prompt,
            prompt_parts: prompt_parts_for_runtime,
            memory_inject_tokens: max_inject_tokens,
            memory_refresh_enabled,
            memory,
            memory_store,
            recall,
            embedder,
            memory_refresh,
            tool_config,
            cron_service: Some(cron_service),
            runtime_lifecycle,
            pipeline_factory,
            hook_executor,
            lane_routing: profile.config.lane_routing.clone(),
            // Voice (ASR/TTS) route/ASR settings are a serve-level platform
            // setting living on the top-level config.json, not on per-profile
            // JSON. `config_from_profile` drops it, so the caller (serve/gateway)
            // passes the host's `config.voice` here; fall back to defaults when
            // absent. Per-tenant settings (*timbre*, TTS route, cloud config) are
            // overlaid: `voice_default` (reply voice via `PUT /api/my/voice`),
            // `tts_provider` (route: auto/local/cloud), and `tts_cloud` (cloud
            // credentials).
            voice: config
                .voice
                .clone()
                .or_else(|| host_voice.cloned())
                .unwrap_or_default()
                .with_default_voice_override(profile.config.voice_default.as_deref())
                .with_tts_provider_override(profile.config.tts_provider.as_deref())
                .with_cloud_override(profile.config.tts_cloud.as_ref())
                .with_cloud_token_from_env(&profile.config.env_vars),
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
/// Open the profile's Recall/Knowledge index sized to the embedder.
///
/// Vectors are Matryoshka-truncated to `memory.recall_dimension` (default
/// 256) and quantised, so the index never needs the embedder's full width;
/// an embedder narrower than that lowers the width instead. Without an
/// embedder the store still opens and answers keyword (BM25) queries.
pub(crate) async fn open_recall_store(
    data_dir: &Path,
    config: &Config,
    embedder: Option<&dyn octos_llm::EmbeddingProvider>,
) -> Result<Arc<octos_memory::RecallStore>> {
    // One handle per data dir per process: profiles routed by the gateway
    // and the serve/gateway bootstrap share it instead of contending for
    // the redb lock (a second open in the same process would only get the
    // in-memory fallback).
    static SHARED: std::sync::OnceLock<
        std::sync::Mutex<HashMap<PathBuf, Arc<octos_memory::RecallStore>>>,
    > = std::sync::OnceLock::new();
    let key = std::fs::canonicalize(data_dir).unwrap_or_else(|_| data_dir.to_path_buf());
    if let Some(existing) = SHARED
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
        .cloned()
    {
        return Ok(existing);
    }
    let recall_config = recall_config_for(config, embedder);
    let dir = data_dir.to_path_buf();
    let store = tokio::task::spawn_blocking(move || {
        octos_memory::RecallStore::open_or_degraded(&dir, recall_config)
    })
    .await
    .wrap_err("recall store open task failed")??;
    let store = Arc::new(store);
    SHARED
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key, store.clone());
    Ok(store)
}

/// Strict opener for one-shot commands (`octos memory …`): refuses when a
/// running serve/gateway owns the store instead of silently working on an
/// in-memory copy, and resolves the SAME geometry as the runtime so vectors
/// are never misread.
pub(crate) async fn open_recall_store_strict(
    data_dir: &Path,
    config: &Config,
    embedder: Option<&dyn octos_llm::EmbeddingProvider>,
) -> Result<octos_memory::RecallStore> {
    let recall_config = recall_config_for(config, embedder);
    let dir = data_dir.to_path_buf();
    tokio::task::spawn_blocking(move || octos_memory::RecallStore::open(&dir, recall_config))
        .await
        .wrap_err("recall store open task failed")?
}

/// The Recall geometry the runtime uses for `config` + `embedder`.
pub(crate) fn recall_config_for(
    config: &Config,
    embedder: Option<&dyn octos_llm::EmbeddingProvider>,
) -> octos_memory::RecallConfig {
    let mut recall_config = octos_memory::RecallConfig::default();
    if let Some(dim) = config.memory.as_ref().and_then(|m| m.recall_dimension) {
        recall_config.dimension = dim.max(8);
    }
    if let Some(e) = embedder {
        recall_config.dimension = recall_config.dimension.min(e.dimension());
    }
    // The embedder id is only claimed when an embedder actually loaded: a
    // non-empty id lets the store adopt a new geometry (purging foreign
    // vectors), which must never happen because credentials or a model file
    // were merely unavailable this run.
    let embedder = embedder.filter(|provider| provider.is_ready());
    recall_config.embedder_id = match (embedder.is_some(), config.embedding.as_ref()) {
        (false, _) => String::new(),
        // No `embedding` section but an embedder loaded ⇒ the bundled default.
        (true, None) => crate::embed_model::DEFAULT_MODEL_ID.to_string(),
        (true, Some(e)) => {
            let model = e.model.clone().or_else(|| e.model_path.clone());
            match model {
                // The bundled model, referenced by its default path or none.
                None => crate::embed_model::DEFAULT_MODEL_ID.to_string(),
                Some(m) => format!("{}/{}", e.provider, m),
            }
        }
    };
    recall_config
}

/// Background upkeep for the Recall/Knowledge index at profile bootstrap.
pub(crate) fn spawn_recall_maintenance(
    recall: Arc<octos_memory::RecallStore>,
    memory_store: Arc<MemoryStore>,
    embedder: Option<Arc<dyn octos_llm::EmbeddingProvider>>,
    profile_id: String,
    default_bundled_embedder: bool,
) {
    tokio::spawn(async move {
        if let Err(e) =
            octos_agent::memory_index::sync_bank(&memory_store, &recall, embedder.as_deref()).await
        {
            tracing::warn!(profile = %profile_id, error = %e, "recall: bank sync failed");
        }
        if let Some(e) = embedder.as_deref() {
            let total = backfill_vectors_with_bounded_retry(
                &recall,
                e,
                &profile_id,
                default_bundled_embedder,
                std::time::Instant::now() + std::time::Duration::from_secs(15 * 60),
                std::time::Duration::from_secs(10),
                std::time::Duration::from_secs(2),
            )
            .await;
            if total > 0 {
                tracing::info!(profile = %profile_id, vectors = total, "recall: backfilled vectors");
            }
        }
        let aged = {
            let recall = recall.clone();
            tokio::task::spawn_blocking(move || {
                let report = recall.age(chrono::Utc::now())?;
                recall.persist_index()?;
                Ok::<_, eyre::Report>(report)
            })
            .await
        };
        match aged {
            Ok(Ok(report)) if report.records_deleted > 0 || report.vectors_evicted > 0 => {
                tracing::info!(profile = %profile_id, ?report, "recall: aged index")
            }
            Ok(Ok(_)) => {}
            Ok(Err(e)) => tracing::warn!(profile = %profile_id, error = %e, "recall: aging failed"),
            Err(e) => {
                tracing::warn!(profile = %profile_id, error = %e, "recall: aging task failed")
            }
        }
    });
}

/// Drain the vector backlog in bounded batches within one bounded window.
/// T-0317: a shed [`octos_llm::EmbedQueueFull`] (bounded background queue
/// backpressure) is retried at a fixed interval until the deadline instead of
/// being abandoned until restart — no busy loop, no unbounded retry, no extra
/// background tasks. A REAL embedding error still stops the drain
/// (keyword-only indexing already degraded cleanly). The intervals are
/// parameters so tests exercise the same logic without waiting minutes.
async fn backfill_vectors_with_bounded_retry(
    recall: &Arc<octos_memory::RecallStore>,
    embedder: &dyn octos_llm::EmbeddingProvider,
    profile_id: &str,
    default_bundled_embedder: bool,
    deadline: std::time::Instant,
    queue_full_retry_interval: std::time::Duration,
    not_ready_poll_interval: std::time::Duration,
) -> usize {
    let mut total = 0usize;
    loop {
        if !embedder.is_ready() {
            if std::time::Instant::now() >= deadline {
                tracing::warn!(profile = %profile_id, "recall: embedding model was not ready before the backfill deadline");
                break;
            }
            tokio::time::sleep(not_ready_poll_interval).await;
            continue;
        }
        if default_bundled_embedder && recall.config().embedder_id.is_empty() {
            if let Err(error) = recall.adopt_embedder(crate::embed_model::DEFAULT_MODEL_ID) {
                tracing::warn!(profile = %profile_id, %error, "recall: failed to adopt the ready embedder");
                break;
            }
        }
        match octos_agent::memory_index::backfill_vectors(recall, embedder, 512).await {
            Ok(0) => break,
            Ok(n) => total += n,
            Err(err) => {
                if err.downcast_ref::<octos_llm::EmbedQueueFull>().is_some() {
                    if std::time::Instant::now() >= deadline {
                        tracing::warn!(profile = %profile_id, error = %err, "recall: vector backfill still shed at the deadline; pending records stay keyword-only until the next restart");
                        break;
                    }
                    tokio::time::sleep(queue_full_retry_interval).await;
                    continue;
                }
                tracing::warn!(profile = %profile_id, error = %err, "recall: vector backfill failed");
                break;
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::{
        GatewaySettings, LlmModelSelectionConfig, LlmProfileConfig, LlmRouteConfig, ProfileConfig,
    };
    use chrono::Utc;

    fn fixture_profile(id: &str, key_env: &str) -> UserProfile {
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
                env_vars: HashMap::from([(key_env.to_string(), "test-key-sk-fake".to_string())]),
                ..Default::default()
            },
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    /// #2691 deliberately retired DOT execution. An old graph on disk must not
    /// recreate the tool through profile bootstrap, plugin reload, session
    /// construction, or the model-facing discovery gateway.
    #[tokio::test]
    async fn profile_runtime_does_not_expose_dot_pipeline() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("profile-data");
        let graphs = data_dir.join("pipelines");
        std::fs::create_dir_all(&graphs).unwrap();
        std::fs::write(graphs.join("deep_research.dot"), "digraph deep_research {}").unwrap();
        let profile = fixture_profile("no-dot", "OCTOS_NO_DOT_TEST_KEY");
        let rt = ProfileRuntime::bootstrap(&profile, &data_dir, None, BootstrapRole::Serve)
            .await
            .expect("bootstrap should succeed");

        assert!(rt.pipeline_factory.is_none());
        assert!(rt.tool_specs.get("run_pipeline").is_none());
        assert!(
            !rt.tool_specs
                .specs()
                .iter()
                .any(|tool| tool.name == "run_pipeline")
        );

        let reloaded = rt.rebuild_plugin_layer().await.unwrap();
        assert!(reloaded.pipeline_factory.is_none());
        assert!(reloaded.tool_specs.get("run_pipeline").is_none());
        let session = super::super::SessionRuntime::bootstrap(
            &reloaded,
            octos_core::SessionKey::new("api", "no-dot"),
            None,
        )
        .await
        .unwrap();
        assert!(session.tools.get("run_pipeline").is_none());
        let error = match session
            .tools
            .execute(
                "discover_tools",
                &serde_json::json!({"name": "run_pipeline"}),
            )
            .await
        {
            Err(error) => error,
            Ok(_) => panic!("retired tool discovery must report unknown/unavailable"),
        };
        assert!(
            error
                .to_string()
                .contains("requested tool is unknown or unavailable"),
            "unexpected discovery error: {error}"
        );
    }

    /// The native replacement must retain the profile's shared credential when
    /// the plugin layer is rebuilt; retiring DOT must not retire synthesis.
    #[tokio::test]
    async fn native_research_synthesis_is_retained_for_plugin_reload() {
        let tmp = tempfile::tempdir().unwrap();
        let profile = fixture_profile("native-research", "OCTOS_NATIVE_RESEARCH_TEST_KEY");
        let rt = ProfileRuntime::bootstrap(&profile, tmp.path(), None, BootstrapRole::Serve)
            .await
            .unwrap();
        let reloaded = rt.rebuild_plugin_layer().await.unwrap();

        for runtime in [rt, reloaded] {
            let config = runtime
                .plugin_reload
                .as_ref()
                .unwrap()
                .synthesis_config
                .as_ref()
                .expect("native research needs synthesis config across reload");
            assert_eq!(config.provider, "openai");
            assert_eq!(config.model, "gpt-4o-mini");
            assert_eq!(config.api_key, "test-key-sk-fake");
        }
    }
}
