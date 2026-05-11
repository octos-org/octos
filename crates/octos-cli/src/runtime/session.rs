//! Session-scope runtime state.
//!
//! See the crate-level [`super`] module docs and
//! `docs/M11-PROFILE-SESSION-RUNTIME-ADR.md` for the two-scope model.
//! This file owns the [`SessionRuntime`] type and its `bootstrap`
//! signature; the body lands in M11-C.

use std::path::PathBuf;
use std::sync::Arc;

use eyre::Result;
use octos_agent::{Agent, SandboxConfig, ToolRegistry};
use octos_bus::SessionManager;
use octos_core::SessionKey;

use super::ProfileRuntime;

/// All per-session state derived from a parent [`ProfileRuntime`].
///
/// One `SessionRuntime` per `(profile_id, session_key)` pair, cached
/// by [`super::SessionRuntimeCache`]. Built on first use; cheap to
/// rebuild from disk-persisted session metadata + chat history.
///
/// # What lives here
///
/// Anything that can legitimately differ between two chats opened by
/// the same logged-in user:
///
/// - **`workspace_root`** — the per-session working directory.
///   Resolved either from a caller-supplied hint (coding-agent UIs
///   that point at a specific repo) or from the conventional
///   `<profile.data_dir>/users/<session_key>/workspace/` path. The
///   bootstrap is also responsible for writing a default
///   `.octos-workspace.toml` if one does not already exist — that's
///   the M11 fix for the `"workspace policy not found"` failure on
///   yangmi voice clone.
/// - **`plugin_work_dir`** — the per-session scratch space plugins
///   are allowed to write into. Conventionally
///   `workspace_root.join("skill-output")`; lives under the
///   workspace root so artifacts remain visible to the user but
///   are namespaced away from the session's main work tree. Wired
///   into the tool registry via `set_output_dir_hint`.
/// - **`sandbox`** — the effective sandbox config for this session.
///   Falls back to [`ProfileRuntime::default_sandbox`] unless the
///   session explicitly overrides (e.g. a slides-builder room
///   pinning `no-network`).
/// - **`tools`** — the session's [`ToolRegistry`]. Built by cloning
///   the parent's [`ProfileRuntime::tool_specs`] template, then
///   binding it to `workspace_root` (`with_workspace_root`), then
///   applying [`ProfileRuntime::tool_policy`] filters. Two sessions
///   for the same profile cannot leak workspace paths through their
///   tool registries because each holds a distinct
///   `Arc<ToolRegistry>`.
/// - **`agent`** — the per-session [`Agent`] instance. Wraps the
///   profile's LLM, this session's tools, this session's
///   workspace, and the standard agent config. The agent is what
///   `/api/chat` and the UI Protocol v1 WS dispatcher invoke.
/// - **`sessions`** — the per-session
///   [`tokio::sync::Mutex<SessionManager>`]. Owns the chat history
///   JSONL store. Wrapped in a Mutex so concurrent reads/writes for
///   the same session (e.g. an in-flight tool call observed by both
///   the SSE stream and the WS subscriber) serialize.
///
/// # Lifecycle
///
/// Constructed lazily by
/// [`super::SessionRuntimeCache::get_or_init`] on first dispatch.
/// Cached with TTL/LRU; evicted on idle or capacity pressure.
/// Reconstructible at any time from the profile + on-disk session
/// metadata — the cache is a performance optimization, not the
/// source of truth.
pub struct SessionRuntime {
    /// The session identifier; the second half of the cache key in
    /// [`super::SessionRuntimeCache`].
    pub session_key: SessionKey,

    /// Shared handle to the parent profile runtime. Carries the
    /// LLM, credentials, base tool registry template, memory
    /// stores, etc.
    pub profile: Arc<ProfileRuntime>,

    /// The per-session working directory. Tool filesystem
    /// operations (`read_file`, `write_file`, `edit_file`, ...)
    /// are scoped to this root by [`Self::tools`].
    pub workspace_root: PathBuf,

    /// Per-session plugin scratch directory. Plugins are spawned
    /// with this as their cwd / `OCTOS_PLUGIN_WORK_DIR` so
    /// intermediate files don't collide across sessions.
    pub plugin_work_dir: PathBuf,

    /// The effective sandbox config for this session. Inherited
    /// from [`ProfileRuntime::default_sandbox`] unless the session
    /// supplied an override at bootstrap.
    pub sandbox: SandboxConfig,

    /// The session's [`ToolRegistry`] — a clone of the profile's
    /// base [`ProfileRuntime::tool_specs`] template that has been
    /// (a) bound to [`Self::workspace_root`] and (b) filtered
    /// through [`ProfileRuntime::tool_policy`]. Distinct
    /// `Arc<ToolRegistry>` per session so workspace state cannot
    /// leak across sessions of the same profile.
    pub tools: Arc<ToolRegistry>,

    /// The per-session [`Agent`] instance. This is what the
    /// `/api/chat` and UI Protocol v1 dispatchers invoke.
    pub agent: Arc<Agent>,

    /// The per-session chat history manager. Wrapped in a
    /// [`tokio::sync::Mutex`] because multiple subscribers
    /// (SSE + WS) may observe and persist messages concurrently.
    pub sessions: Arc<tokio::sync::Mutex<SessionManager>>,
}

impl SessionRuntime {
    /// Construct a [`SessionRuntime`] for the given session key.
    ///
    /// # Contract (filled in by M11-C)
    ///
    /// 1. Resolve `workspace_root`:
    ///    - If `workspace_hint` is `Some(path)` and
    ///      `validate_session_workspace_allowed(state, path)`
    ///      accepts, use it as-is (coding-agent flow).
    ///    - Otherwise default to
    ///      `profile.data_dir.join("users").join(encode(session_key)).join("workspace")`
    ///      and `create_dir_all` it.
    /// 2. If `<workspace_root>/.octos-workspace.toml` is missing,
    ///    write `WorkspacePolicy::for_session()` to it via
    ///    `octos_agent::workspace_policy::write_workspace_policy`.
    ///    This is the M11 fix for the
    ///    `"workspace policy not found"` failure.
    /// 3. Compute
    ///    `plugin_work_dir = workspace_root.join("skill-output")`
    ///    and `create_dir_all` it.
    /// 4. Clone `profile.tool_specs` via
    ///    `ToolRegistry::snapshot_excluding(&[])` and apply:
    ///    - `set_workspace_root(workspace_root.clone())`
    ///    - `set_output_dir_hint(plugin_work_dir.to_string_lossy().into_owned())`
    ///    - per-session policy filter (no-op default for M11).
    /// 5. Resolve `sandbox`: [`ProfileRuntime::default_sandbox`]
    ///    is fine for M11; an explicit per-session override hook
    ///    is left for a future workstream.
    /// 6. Build the per-session [`Agent`] from `profile.llm` plus
    ///    the cloned tools. Today's `Agent::new(...) + .with_*`
    ///    chain in `commands/serve.rs::try_create_agent` relocates
    ///    here verbatim.
    /// 7. Open the [`SessionManager`] via
    ///    `SessionManager::open(&profile.data_dir)` — the canonical
    ///    JSONL session store already namespaces on-disk files by
    ///    [`SessionKey`] under `data_dir/sessions/`, so the
    ///    profile data dir is the correct root. The
    ///    `SessionManager` is shared across sessions of the same
    ///    profile (wrapped in [`tokio::sync::Mutex`]); M11-C may
    ///    choose to surface the existing profile-scoped manager
    ///    via [`ProfileRuntime`] instead of opening a new one per
    ///    session — either way the on-disk layout matches today's
    ///    `commands/gateway/gateway_runtime.rs` and
    ///    `commands/serve.rs` call sites.
    /// 8. Return `Arc<Self>`.
    ///
    /// # Parameters
    ///
    /// - `profile` — the parent [`ProfileRuntime`] this session
    ///   inherits from. Held as `&Arc<...>` so the new session
    ///   bumps the `Arc` count rather than cloning the profile.
    /// - `session_key` — the session identifier. Used both as
    ///   the cache key half and to derive the conventional
    ///   workspace/plugin paths under `profile.data_dir`.
    /// - `workspace_hint` — optional caller-supplied workspace
    ///   root. `Some` for coding-agent UIs that point at a
    ///   specific repo; `None` for the default "data-dir-relative"
    ///   layout used by web chat and gateway sessions.
    ///
    /// # Errors
    ///
    /// Returns an error if workspace validation fails, directory
    /// creation fails, policy write fails, registry clone fails,
    /// agent construction fails, or session-manager load fails.
    /// A partially constructed [`SessionRuntime`] is never
    /// returned.
    #[allow(unused_variables)]
    pub async fn bootstrap(
        profile: &Arc<ProfileRuntime>,
        session_key: SessionKey,
        workspace_hint: Option<PathBuf>,
    ) -> Result<Arc<Self>> {
        todo!("M11-C implements this")
    }
}
