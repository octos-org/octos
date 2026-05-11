//! Profile-scope runtime state.
//!
//! See the crate-level [`super`] module docs and
//! `docs/M11-PROFILE-SESSION-RUNTIME-ADR.md` for the two-scope model.
//! This file owns the [`ProfileRuntime`] type and its `bootstrap`
//! signature; the body lands in M11-B.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use eyre::Result;
use octos_agent::{SandboxConfig, ToolPolicy, ToolRegistry};
use octos_llm::{AdaptiveRouter, LlmProvider};
use octos_memory::{EpisodeStore, MemoryStore};

use crate::profiles::{ProfileStore, UserProfile};

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
///   name. Populated from `profile.config.env_vars` via the keychain
///   in M11-B; passed to MCP server spawns and plugin invocations on
///   the session side.
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

    /// Resolved credentials for this profile, keyed by env-var name
    /// (e.g. `OPENAI_API_KEY`, `AUTODL_API_KEY`). Populated from
    /// `profile.config.env_vars` via the keychain resolver. Sessions
    /// read this when spawning MCP servers, plugins, and shell tools
    /// that need the profile's API keys.
    pub credentials: HashMap<String, String>,

    /// Path to the per-profile skills directory if one exists
    /// (`<data_dir>/skills/`). `None` when the profile has no
    /// dashboard-installed skills, in which case the base
    /// [`ToolRegistry`] only carries built-in tools and global
    /// skills.
    pub skills_dir: Option<PathBuf>,

    /// Env-var pairs every plugin spawn for this profile should
    /// inherit (`OCTOS_PROFILE_ID`, `OCTOS_VOICE_DIR`, etc.). Kept
    /// as a vector of `(name, value)` rather than a map so the
    /// session-side spawner can build the child env in stable order.
    /// Sessions are free to add session-scoped vars on top of this
    /// template.
    pub plugin_env_template: Vec<(String, String)>,

    /// The profile's tool policy (allow/deny lists, named groups,
    /// per-provider overrides). Stored on the profile and applied
    /// per session when the session clones [`Self::tool_specs`].
    /// `None` means "no profile-level policy" — the agent's default
    /// permissions apply.
    pub tool_policy: Option<ToolPolicy>,

    /// The default sandbox config sessions inherit. Sessions may
    /// override (e.g. a slides-builder session wants
    /// `no-network`); when they don't, the runtime falls back to
    /// this value.
    pub default_sandbox: SandboxConfig,

    /// The base [`ToolRegistry`] template — builtins + plugins +
    /// MCP agents + the LRU pin set — but **NOT** workspace-bound.
    /// Sessions clone this and call `with_workspace_root` to obtain
    /// a workspace-bound registry. The "no workspace bound" rule is
    /// load-bearing: it's the M11 fix for the codex-flagged
    /// multi-tenant base-registry leak (one global registry shared
    /// across sessions would otherwise let session A's workspace
    /// path leak into session B).
    pub tool_specs: Arc<ToolRegistry>,

    /// Long-lived [`EpisodeStore`] for this profile (redb at
    /// `<data_dir>/episodes.redb`). Shared across all sessions of
    /// the profile so task summaries written in one session are
    /// recallable from another.
    pub memory: Arc<EpisodeStore>,

    /// Long-lived [`MemoryStore`] (MEMORY.md + daily notes + recent
    /// memories window) for this profile. Same sharing rationale as
    /// [`Self::memory`].
    pub memory_store: Arc<MemoryStore>,
}

impl ProfileRuntime {
    /// Construct a [`ProfileRuntime`] for the given profile.
    ///
    /// # Contract (filled in by M11-B)
    ///
    /// 1. Derive a per-profile `Config` via
    ///    `crate::profiles::config_from_profile(profile, None, None)`
    ///    (preserves the per-profile LLM contract PR #866
    ///    introduced).
    /// 2. Wrap the primary LLM via
    ///    `qos_catalog::build_adaptive_provider_chain(..., ExporterMode::Spawn)`
    ///    (PR #867's shared helper). Store both `llm` and
    ///    `adaptive_router` on the returned struct.
    /// 3. Resolve `credentials` from `profile.config.env_vars` via
    ///    `keychain::resolve_env_vars`.
    /// 4. Resolve `skills_dir = data_dir.join("skills")` if it
    ///    exists (PR #868's logic).
    /// 5. Build `plugin_env_template` via
    ///    `skills_scope::push_runtime_plugin_env` (PR #868's
    ///    helper).
    /// 6. Construct the base [`ToolRegistry`] via
    ///    `with_builtins_and_sandbox` + `tool_config` + the
    ///    registration sequence gateway uses today (browser,
    ///    web_search, MCP agents, ...).
    /// 7. Load plugins via
    ///    `PluginLoader::load_into_with_options` with the
    ///    per-profile env + work dir.
    /// 8. Pin plugin tool names as base tools (the LRU defense PR
    ///    #764 added).
    /// 9. Open [`EpisodeStore`] and [`MemoryStore`] against
    ///    `data_dir`.
    /// 10. Return `Arc<Self>`.
    ///
    /// # Parameters
    ///
    /// - `profile` — the parsed [`UserProfile`] from the
    ///   [`ProfileStore`]; carries the on-disk config that drives
    ///   the bootstrap.
    /// - `store` — the [`ProfileStore`] the profile came from;
    ///   needed for lookups (admin/sub-account resolution) the
    ///   bootstrap performs.
    /// - `data_dir` — the resolved per-profile data dir, typically
    ///   `~/.octos/profiles/<id>/data`. The bootstrap creates it if
    ///   missing.
    ///
    /// # Errors
    ///
    /// Returns an error if any of the steps above fail: provider
    /// construction, keychain resolution, skills loading, redb open,
    /// or tool-registry build. The bootstrap is fail-fast — a
    /// partially constructed [`ProfileRuntime`] is never returned.
    #[allow(unused_variables)]
    pub async fn bootstrap(
        profile: &UserProfile,
        store: &ProfileStore,
        data_dir: &Path,
    ) -> Result<Arc<Self>> {
        todo!("M11-B implements this")
    }
}
