//! Session-scope runtime state.
//!
//! See the crate-level [`super`] module docs and
//! `docs/M11-PROFILE-SESSION-RUNTIME-ADR.md` for the two-scope model.
//! This file owns the [`SessionRuntime`] type and the M11-C
//! implementation of [`SessionRuntime::bootstrap`].

use std::path::{Path, PathBuf};
use std::sync::Arc;

use eyre::{Result, WrapErr};
use octos_agent::sandbox::create_sandbox;
use octos_agent::workspace_policy::{WorkspacePolicy, write_workspace_policy_if_absent};
use octos_agent::{
    Agent, AgentConfig, AgentSummaryGenerator, EffectivePermissions, FileStateCache, SandboxConfig,
    SubAgentOutputRouter, ToolRegistry,
};
use octos_bus::SessionManager;
use octos_core::{
    AgentId, DEFAULT_MULTI_TENANT_SHARED_ZONE_NAMES, SessionKey, SessionScope, is_safe_session_id,
};

use super::ProfileRuntime;
use super::execution_environment::ExecutionEnvironmentCache;
use crate::commands::gateway::prompt::{
    SLASH_COMMANDS_SEGMENT_NAME, accepted_client_commands, render_client_commands,
    strip_slash_commands,
};
use crate::cron_tool::CronTool;

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

    /// The effective permission profile for this session. This is the
    /// runtime source of truth used to build shell policy, sandbox behavior,
    /// file-tool scope, and approval behavior.
    pub permissions: EffectivePermissions,

    /// The session's [`ToolRegistry`] — a clone of the profile's
    /// base [`ProfileRuntime::tool_specs`] template that has been
    /// (a) bound to [`Self::workspace_root`] and (b) filtered
    /// through [`ProfileRuntime::tool_policy`]. Distinct
    /// `Arc<ToolRegistry>` per session so workspace state cannot
    /// leak across sessions of the same profile.
    pub tools: Arc<ToolRegistry>,

    /// Cached actual-backend/tool probe. Rebuilt with this runtime after an
    /// effective permission or environment change, never shared across sessions.
    pub execution_environment: ExecutionEnvironmentCache,

    /// The per-session [`Agent`] instance. This is what the
    /// `/api/chat` and UI Protocol v1 dispatchers invoke.
    pub agent: Arc<Agent>,

    /// The on-disk **root** the session transcript store is opened at
    /// (`SessionManager::open(sessions_root)` yields
    /// `sessions_root/sessions/` + `sessions_root/users/<base>/sessions/`).
    ///
    /// Equals [`ProfileRuntime::data_dir`] for the historical behavior
    /// (web-chat, gateway, or any session with no cwd hint). For an
    /// AppUi/coding-agent session opened with a cwd hint **and** the
    /// `appui.sessions_in_cwd` flag on, this is `<cwd>/.octos` — the
    /// per-project store. Sidecars that derive their path from
    /// `sessions.data_dir()` (reasoning-effort, task ledger) therefore
    /// follow the transcript to the same root automatically.
    ///
    /// Held explicitly (rather than re-derived) so callers that need the
    /// root without locking the manager — and tests asserting the per-cwd
    /// relocation — can read it directly.
    pub sessions_root: PathBuf,

    /// The per-session chat history manager. Wrapped in a
    /// [`tokio::sync::Mutex`] because multiple subscribers
    /// (SSE + WS) may observe and persist messages concurrently.
    ///
    /// Opened at [`Self::sessions_root`] (which is
    /// [`ProfileRuntime::data_dir`] unless the session is cwd-scoped).
    pub sessions: Arc<tokio::sync::Mutex<SessionManager>>,

    /// The memory stores this session captures into and is injected from:
    /// the profile's own, or — for a host-bound app peer or one of its
    /// request contexts (UPCR-2026-034) — the bound app/account namespace.
    pub memory: super::memory_namespace::SessionMemory,

    /// The app binding (UPCR-2026-034) this runtime was built for, and the
    /// `peers/` root it was resolved under. A binding decides the workspace,
    /// the memory stores and the permission clamp, so a runtime built for
    /// another binding must be rebuilt, never reused
    /// ([`Self::app_binding_is_current`]).
    pub(crate) app_binding: crate::peers::app_binding::SessionAppBinding,
    pub(crate) binding_peers_root: PathBuf,
    /// The connection whose `client_commands` declaration is currently in
    /// the prompt, so only that connection's close releases it.
    client_commands_owner: std::sync::Mutex<Option<u64>>,
}

/// UPCR-2026-034 `read_parent`: a request context's read-only view of its
/// peer's folder, minus every context's folder (its own stays its
/// workspace): `(peer folder, excluded folders)`. Enforced by the session
/// scope (file tools) and the sandbox (shell), never by convention.
fn context_read_view(
    binding: &crate::peers::app_binding::SessionAppBinding,
) -> Option<(PathBuf, Vec<PathBuf>)> {
    match binding {
        crate::peers::app_binding::SessionAppBinding::Bound {
            read_view: Some(root),
            ..
        } => Some((
            root.clone(),
            vec![crate::peers::app_binding::contexts_folder(root)],
        )),
        _ => None,
    }
}

/// Attach [`context_read_view`] to the session scope.
fn with_context_read_view(
    scope: Option<Arc<SessionScope>>,
    binding: &crate::peers::app_binding::SessionAppBinding,
) -> Result<Option<Arc<SessionScope>>, octos_core::SessionScopeError> {
    match (scope, context_read_view(binding)) {
        (Some(scope), Some((root, excluded))) => Ok(Some(Arc::new(
            (*scope).clone().with_read_only_view(root, excluded)?,
        ))),
        (scope, _) => Ok(scope),
    }
}

impl SessionRuntime {
    /// Whether the durable app binding of this session still equals the one
    /// this runtime was built for. `false` means the runtime is stale (e.g.
    /// cached before `peer/prepare` bound the session) and must be rebuilt.
    pub(crate) fn app_binding_is_current(&self) -> bool {
        crate::peers::app_binding::resolve_session_app_binding(
            &self.binding_peers_root,
            &self.session_key,
        ) == self.app_binding
    }

    /// Construct a [`SessionRuntime`] for the given session key.
    ///
    /// See the M11-C contract in `workstreams/M11-runtime-unification.md`
    /// § "M11-C" and the M11-A doc comments preserved on this file
    /// for the full step-by-step. Summary:
    ///
    /// 1. Resolve `workspace_root` (from `workspace_hint` if
    ///    accepted, else from the conventional
    ///    `<data_dir>/users/<encoded session base>/workspace`
    ///    layout) and `create_dir_all` it.
    /// 2. Write `WorkspacePolicy::for_session()` to
    ///    `<workspace_root>/.octos-workspace.toml` **only if absent**
    ///    — idempotent; never overwrites an operator's manual edits.
    ///    This is the M11 fix for the
    ///    `"workspace policy not found"` failure observed on
    ///    yangmi voice clone.
    /// 3. Resolve the plugin work dir without creating it. Plugin execution
    ///    creates it on demand; read-only sessions use the profile data dir.
    /// 4. Clone `profile.tool_specs` via
    ///    `ToolRegistry::snapshot_excluding(&[])` and bind it to
    ///    the per-session workspace + output-dir hint.
    /// 5. Resolve `sandbox` from `profile.default_sandbox` (M11
    ///    default; per-session overrides are a future workstream).
    /// 6. Build the per-session [`Agent`] from `profile.llm` plus
    ///    the cloned tools. The `Agent::new(...)` + `.with_*` chain
    ///    here is the only per-session agent constructor — the
    ///    pre-M11-F serve-side server-wide agent was deleted.
    ///    AppState-derived plumbing (broadcaster/MetricsReporter/
    ///    HookExecutor/system prompt fragments) layers on at the
    ///    dispatcher (UI Protocol / `/api/chat`).
    /// 7. Open the [`SessionManager`] at the resolved **sessions root**
    ///    (`resolve_sessions_root`). The canonical JSONL store namespaces
    ///    on-disk files by [`SessionKey`] under `<root>/sessions/` +
    ///    `<root>/users/<base>/sessions/`, so the store is fully
    ///    root-parameterized. The root is `profile.data_dir` for every
    ///    no-hint/gateway/web-chat session and while `appui.sessions_in_cwd`
    ///    is off (byte-identical to the historic behavior); a cwd-hinted AppUi
    ///    session with the flag on relocates to `<cwd>/.octos` (per-project
    ///    storage). Stored on [`Self::sessions_root`].
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
    pub async fn bootstrap(
        profile: &Arc<ProfileRuntime>,
        session_key: SessionKey,
        workspace_hint: Option<PathBuf>,
    ) -> Result<Arc<Self>> {
        Self::bootstrap_with_permissions(
            profile,
            session_key,
            workspace_hint,
            EffectivePermissions::workspace_write(),
        )
        .await
    }

    /// Tell the agent which slash commands the client on connection `owner`
    /// declared on `session/open` (octoscode#664); an empty list clears them.
    /// Per-turn agents inherit it via the snapshot. Returns the names that
    /// were accepted, for the `session/open` result to echo.
    pub fn apply_client_commands(&self, owner: u64, commands: &[String]) -> Vec<String> {
        let mut current = self
            .client_commands_owner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let accepted = accepted_client_commands(commands);
        *current = (!accepted.is_empty()).then_some(owner);
        self.agent.set_prompt_segment(
            SLASH_COMMANDS_SEGMENT_NAME,
            render_client_commands(&accepted),
        );
        accepted
    }

    /// Clear the declared commands when connection `owner` closes, unless a
    /// later `session/open` from another connection has replaced them.
    pub fn release_client_commands(&self, owner: u64) {
        let mut current = self
            .client_commands_owner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *current == Some(owner) {
            *current = None;
            self.agent
                .set_prompt_segment(SLASH_COMMANDS_SEGMENT_NAME, String::new());
        }
    }

    /// [`Self::bootstrap`] with an explicit `sessions_in_cwd` flag. The
    /// convenience [`Self::bootstrap`] hard-codes `false` (legacy per-profile
    /// storage); this variant lets the AppUi path (and tests) request the
    /// per-project relocation when a cwd hint is present.
    pub async fn bootstrap_in_cwd(
        profile: &Arc<ProfileRuntime>,
        session_key: SessionKey,
        workspace_hint: Option<PathBuf>,
        sessions_in_cwd: bool,
    ) -> Result<Arc<Self>> {
        Self::bootstrap_with_permissions_and_sandbox(
            profile,
            session_key,
            workspace_hint,
            EffectivePermissions::workspace_write(),
            None,
            sessions_in_cwd,
        )
        .await
    }

    /// Construct a [`SessionRuntime`] with an explicit effective permission
    /// profile. AppUI integration should resolve and gate requested permission
    /// profiles before calling this hook.
    pub async fn bootstrap_with_permissions(
        profile: &Arc<ProfileRuntime>,
        session_key: SessionKey,
        workspace_hint: Option<PathBuf>,
        permissions: EffectivePermissions,
    ) -> Result<Arc<Self>> {
        Self::bootstrap_with_permissions_and_sandbox(
            profile,
            session_key,
            workspace_hint,
            permissions,
            None,
            // Legacy per-profile storage. The AppUi path opts into per-cwd
            // storage via the `sessions_in_cwd` param on
            // `bootstrap_with_permissions_and_sandbox`, threaded by the
            // session-runtime cache from `appui.sessions_in_cwd`.
            false,
        )
        .await
    }

    /// Construct a [`SessionRuntime`] with explicit effective permissions and
    /// an optional, already-validated sandbox override. The override must be
    /// derived from and no wider than the profile-level sandbox policy.
    pub async fn bootstrap_with_permissions_and_sandbox(
        profile: &Arc<ProfileRuntime>,
        session_key: SessionKey,
        workspace_hint: Option<PathBuf>,
        permissions: EffectivePermissions,
        sandbox_override: Option<SandboxConfig>,
        // When `true` AND a `workspace_hint` (cwd) is present, the session
        // transcript store is relocated from `profile.data_dir` to
        // `<cwd>/.octos` (per-project storage, `appui.sessions_in_cwd`). No
        // hint → `profile.data_dir` regardless, so gateway/web-chat are inert.
        // Threaded from the `SessionRuntimeCache`'s process-global flag.
        sessions_in_cwd: bool,
    ) -> Result<Arc<Self>> {
        // Step 1: resolve workspace_root. Capture whether a hint was supplied
        // BEFORE it is consumed — the sessions-root resolution below keys off
        // "was this a cwd/coding-agent session" (a hint), not off the derived
        // workspace path.
        //
        // UPCR-2026-034: a host-bound app peer (or one of its request
        // contexts) runs ONLY in its bound workspace and on its bound memory
        // namespace; a closed or never-opened binding refuses to run at all.
        // The binding is durable kernel state written by `peer/prepare` /
        // `peer/context/open`, never taken from this open's parameters.
        let binding_peers_root = profile.data_dir.join("peers");
        let app_binding = crate::peers::app_binding::resolve_session_app_binding(
            &binding_peers_root,
            &session_key,
        );
        // Kept on the runtime: a cached runtime whose binding no longer
        // matches the durable one (the session was bound by `peer/prepare` or
        // `peer/context/open` after it was cached) is never reused.
        let bootstrapped_binding = app_binding.clone();
        let (workspace_hint, bound_memory_namespace) = match app_binding {
            crate::peers::app_binding::SessionAppBinding::Unbound => (workspace_hint, None),
            crate::peers::app_binding::SessionAppBinding::Refused(reason) => {
                eyre::bail!("session {session_key} cannot run: {reason}");
            }
            crate::peers::app_binding::SessionAppBinding::Bound {
                cwd,
                memory_namespace,
                read_view,
            } => {
                // The bound folder (and a context's read-only view of its
                // peer's folder) must still be the canonical directory the
                // binding recorded: a symlink swapped in for it (or an
                // ancestor) would otherwise re-root the session, or widen
                // the view, wherever it points.
                for dir in std::iter::once(&cwd).chain(read_view.iter()) {
                    if let Err(reason) = crate::peers::app_binding::verify_bound_dir(dir) {
                        eyre::bail!("session {session_key} cannot run: {reason}");
                    }
                }
                if let Some(hint) = workspace_hint.as_ref() {
                    let hint_canon = dunce::canonicalize(hint).unwrap_or_else(|_| hint.clone());
                    let cwd_canon = dunce::canonicalize(&cwd).unwrap_or_else(|_| cwd.clone());
                    if hint_canon != cwd_canon {
                        eyre::bail!(
                            "session {session_key} is bound to workspace {}; refusing {}",
                            cwd.display(),
                            hint.display()
                        );
                    }
                }
                (Some(cwd), Some(memory_namespace))
            }
        };
        // UPCR-2026-035: a host-bound app session never runs with host
        // filesystem access. `Host` (danger_full_access, e.g. a Solo profile
        // run with `--danger-full-access`) leaves file tools unscoped and the
        // sandbox off, so an app's read tools would accept any absolute path.
        // Clamp it to workspace access, keeping the approval policy; the
        // session scope is then attached below like for any other session.
        let (permissions, sandbox_override) =
            if bound_memory_namespace.is_some() && permissions.filesystem_scope.is_host() {
                tracing::warn!(
                    session = %session_key,
                    "host-bound app session: clamping host filesystem access to its workspace"
                );
                (
                    EffectivePermissions {
                        approval_policy: permissions.approval_policy,
                        ..EffectivePermissions::workspace_write()
                    },
                    None,
                )
            } else {
                (permissions, sandbox_override)
            };
        let had_workspace_hint = workspace_hint.is_some();
        let workspace_root = resolve_workspace_root(profile, &session_key, workspace_hint)?;
        let workspace_profile = profile.for_workspace(&workspace_root).await?;
        let profile = &workspace_profile;
        std::fs::create_dir_all(&workspace_root).wrap_err_with(|| {
            format!("create workspace root failed: {}", workspace_root.display())
        })?;

        // Step 2: idempotent, atomic policy write. We never overwrite
        // an existing `.octos-workspace.toml` — operators (or earlier
        // sessions) may have hand-edited it. Using
        // `OpenOptions::create_new` is a single atomic syscall that
        // fails with `AlreadyExists` if anything got there first,
        // closing the TOCTOU window an `if !exists() { write }`
        // pattern would leave open under concurrent bootstrap or
        // operator edit. `AlreadyExists` is treated as success.
        if permissions.file_access.allows_write() {
            bootstrap_session_policy(&workspace_root)?;
        }

        // Step 3: resolve the plugin work dir without materializing it.
        // PluginTool::execute creates its effective work dir before spawning,
        // so sessions that never invoke a plugin leave no unused output dir.
        let plugin_work_dir = if permissions.file_access.allows_write() {
            workspace_root.join("skill-output")
        } else {
            use sha2::{Digest, Sha256};
            // Read-only applies to bootstrap too. Keep host-generated scratch
            // outside the selected project, isolated by both cwd and session.
            let workspace_hash = format!(
                "{:x}",
                Sha256::digest(workspace_root.as_os_str().as_encoded_bytes())
            );
            profile
                .data_dir
                .join("runtime")
                .join("read-only")
                .join(workspace_hash)
                .join(octos_bus::session::encode_path_component(&session_key.0))
                .join("skill-output")
        };

        // Step 4: clone the profile tool registry and ACTUALLY rebind
        // it to this session's workspace. `set_workspace_root` only
        // updates registry metadata; `rebind_cwd` re-registers every
        // cwd-bound tool (`shell`, `read_file`, `write_file`, …) with
        // the new workspace path AND a fresh sandbox bound to the
        // session, so the agent's tool calls operate on this
        // session's tree instead of the profile-template `cwd` that
        // happened to be on `profile.tool_specs` at bootstrap. The
        // snapshot is a distinct `Arc<ToolRegistry>` so workspace
        // state cannot leak across sessions of the same profile (M11
        // fix for the multi-tenant base-registry leak codex flagged
        // on PR #868).
        //
        // We also rebind plugin work dirs in the same step so
        // `fm_tts` and friends emit into this session's
        // `<workspace>/skill-output/` rather than the profile-template
        // path.
        let mut sandbox = sandbox_override
            .unwrap_or_else(|| permissions.apply_to_sandbox(&profile.default_sandbox));
        if let Some((root, excluded)) = context_read_view(&bootstrapped_binding) {
            sandbox.read_only_view = Some(Box::new(octos_agent::SandboxReadOnlyView {
                root,
                excluded,
            }));
        }
        let mut tools = profile.tool_specs.rebind_cwd_with_permissions(
            &workspace_root,
            create_sandbox(&sandbox),
            permissions,
        );
        tools.set_output_dir_hint(plugin_work_dir.to_string_lossy().into_owned());
        tools.rebind_plugin_work_dirs(if profile.session_defaults.is_some() {
            &workspace_root
        } else {
            &plugin_work_dir
        });
        // Session-created cron jobs need the exact session/profile origin for
        // acknowledged serve delivery. Replace only an enabled cron tool; a
        // policy that removed it must not re-add it.
        if let Some(cron_service) = profile.cron_service.clone() {
            if tools.get_tool("cron").is_some() {
                let tool = CronTool::with_context(cron_service, "api", session_key.to_string());
                tool.set_origin(octos_bus::CronOrigin {
                    session_id: Some(session_key.to_string()),
                    profile_id: Some(profile.profile_id.clone()),
                    loop_id: None,
                });
                tools.register(tool);
            }
        }
        // #1607 (codex round 4): `run_pipeline` is NOT a CWD-bound tool, so the
        // `rebind_cwd_with_permissions` snapshot above carried the PROFILE-time
        // `run_pipeline` instance — which baked in the profile default sandbox.
        // Re-register it from the profile's pipeline factory with the
        // SESSION-effective `sandbox` so a read-only (or otherwise overridden)
        // session's pipeline command validators run under the session sandbox
        // instead of regaining the profile default's writes/network. The
        // spawn_only marker persists across `register_arc` (it is registry
        // metadata carried by the snapshot), and re-marking is idempotent.
        // Only REPLACE `run_pipeline` (with the session-sandbox instance) when
        // the profile policy actually left it in the rebound registry. A profile
        // that denies `run_pipeline` (or an allow-list excluding it) removed it
        // during `ProfileRuntime::bootstrap` (`apply_policy` → `retain`), so it's
        // absent here; re-adding it unconditionally would make a policy-disabled
        // pipeline visible + callable, bypassing the tool policy (#1607 codex
        // round 5). `register_arc` replaces the existing entry by name.
        if let Some(ref pf) = profile.pipeline_factory {
            if tools.get_tool("run_pipeline").is_some() {
                tools.register_arc(pf.create(&sandbox));
                tools.mark_spawn_only(
                    "run_pipeline",
                    Some(
                        "Pipeline started in background. The final result and any artifacts will be sent here when complete. You can keep chatting in the meantime."
                            .to_string(),
                    ),
                );
            }
        }
        // RFC-0 (#1289): the `activate_tools` meta-tool was removed — every
        // enabled tool is emitted every turn, so there is no per-session
        // meta-tool to re-register or wire.
        profile.apply_tool_envelope(&mut tools);
        let memory = match bound_memory_namespace.as_deref() {
            Some(namespace) => {
                let memory =
                    super::memory_namespace::SessionMemory::namespaced(profile, namespace).await?;
                super::memory_namespace::rebind_memory_tools(
                    &mut tools,
                    &memory,
                    profile.embedder.clone(),
                );
                // `run_pipeline` captures episodes into the PROFILE's memory;
                // a namespaced session must not write there.
                tools.retain(|name| name != "run_pipeline");
                memory
            }
            None => super::memory_namespace::SessionMemory::profile(profile),
        };
        let tools = Arc::new(tools);

        // Step 5: build the per-session Agent. This is the only
        // per-session agent constructor (M11-F deleted the legacy
        // serve-side server-wide agent). AppState-derived wiring
        // (broadcaster-backed MetricsReporter, hooks, skill prompt
        // fragments) layers on at the dispatcher (UI Protocol /
        // `/api/chat`) when it resolves the SessionRuntime per
        // request.
        //
        // Crucially, we hand the agent the SAME `Arc<ToolRegistry>`
        // the SessionRuntime holds (via `Agent::new_shared`). This is
        // what makes `enforce_spawn_task_contract(&rt.tools, ...)`
        // and the agent's actual tool calls observe the same
        // workspace, supervisor, task lifecycle state, and
        // background-result sender. Building a second registry via
        // `snapshot_excluding` would mint a fresh `TaskSupervisor`
        // and split per-session tool state across the two views.
        let subagent_output_root = profile.data_dir.join("subagent-outputs");
        let subagent_output_router = Arc::new(SubAgentOutputRouter::new(subagent_output_root));
        let supervisor_for_summary = (*tools.supervisor()).clone();
        let subagent_summary_generator = Arc::new(AgentSummaryGenerator::new(
            profile.llm.clone(),
            subagent_output_router.clone(),
            supervisor_for_summary,
        ));
        let file_state_cache = Arc::new(FileStateCache::new());

        // SessionScope construction (#1377 Phase-3-B reconciliation).
        //
        // Bind a tenant-scoped filesystem contract to EVERY serve session
        // and root it at the session's REAL `workspace_root` — not the
        // canonical `<data>/users/<id>/workspace` derivation. This closes
        // the round-9 cross-tenant gap: previously, channel-prefixed (`:`)
        // and coding-agent `workspace_hint` sessions were left with
        // `session_scope: None`, so their per-turn agent ran the unscoped
        // legacy resolver, which decodes a process-global `up/` upload
        // handle with NO tenant check (another tenant's pasted handle
        // would resolve). A tenant-bound scope routes those sessions
        // through the gated `resolve_for_scope` instead, where the
        // upload-ownership gate fires.
        //
        // Why this is now safe (the Phase-3-A blockers are resolved): the
        // earlier rounds left these sessions unscoped because the scoped
        // resolver misclassified `up/...` handles and absolute upload-
        // tmpdir paths as OutOfScope. Uploads are now MATERIALIZED into
        // `<workspace>/uploads/` at turn start and read by their
        // workspace-relative path, so the scoped resolver sees a real
        // InWorkspace file — there are no raw `up/` handles left to
        // misclassify. The legacy resolver already confined absolute
        // paths to {workspace, upload-tmpdir, profile-root}, so the only
        // behavioural delta for newly-scoped sessions is losing raw
        // absolute upload-tmpdir reads (replaced by materialized uploads)
        // and `pf/` profile-handle reads (tracked: #1367).
        //
        // Root selection respects the SessionScope WorkspaceEscapesRoot
        // invariant: canonical/encoded sessions whose workspace lives
        // under the profile data dir keep `root = data_dir` + the
        // standard research/skills shared zones; coding-agent hint
        // sessions whose repo is OUTSIDE the data dir root the scope AT
        // the repo (`root == workspace`) with no shared zones.
        let session_id_raw = session_key.base_key().to_string();
        // The scope's session-id field is informational only
        // (classification keys off `workspace`, never this id), but it
        // must satisfy `is_safe_session_id`; collapse the `:` of
        // channel-prefixed shapes (and any other out-of-alphabet byte).
        let scope_session_id = sanitize_scope_session_id(&session_id_raw);
        // #1377 (codex pre-merge P2): decide in-profile membership by comparing
        // CANONICAL forms (firmlink-safe: macOS `/var` vs `/private/var`), but
        // pick the `scope_root` that is an actual PREFIX of `workspace_root`.
        // `workspace_root` is canonical for hint sessions (the `..`/symlink fix
        // above) but RAW for the common no-hint case (it is built but not yet
        // created on disk when `resolve_workspace_root` runs, so it can't be
        // canonicalized there). Comparing a canonical workspace to a raw
        // data_dir (or vice-versa) would misclassify; comparing canonical
        // forms is correct, and rooting at whichever data_dir form prefixes
        // the actual `workspace_root` keeps `scope.classify_*` containment
        // intact for both shapes. `canonicalize` falls back to the raw path
        // when the dir is absent.
        let raw_data_dir = profile.data_dir.clone();
        let canon_data_dir =
            std::fs::canonicalize(&raw_data_dir).unwrap_or_else(|_| raw_data_dir.clone());
        let canon_ws =
            std::fs::canonicalize(&workspace_root).unwrap_or_else(|_| workspace_root.clone());
        let workspace_under_data = canon_ws.starts_with(&canon_data_dir);
        // Use the data_dir form that actually prefixes workspace_root so the
        // scope's root is a true ancestor (raw-vs-raw for no-hint, else canon).
        let scope_root = if workspace_under_data {
            if workspace_root.starts_with(&raw_data_dir) {
                raw_data_dir.clone()
            } else {
                canon_data_dir.clone()
            }
        } else {
            workspace_root.clone()
        };
        // A host-bound app session (UPCR-2026-034) gets no shared zones:
        // a kernel-provisioned app workspace lives under the data dir, but
        // the profile's `research/` and `skills/` hold what the system agent
        // and other apps wrote.
        let scope_zones: Vec<PathBuf> = if workspace_under_data && bound_memory_namespace.is_none()
        {
            DEFAULT_MULTI_TENANT_SHARED_ZONE_NAMES
                .iter()
                .map(|name| scope_root.join(name))
                .collect()
        } else {
            Vec::new()
        };
        // Closure so the skill-zone-rejected fallback can rebuild the
        // identical scope without re-deriving every input.
        let build_scope = || {
            SessionScope::multi_tenant_at_workspace(
                scope_root.clone(),
                workspace_root.clone(),
                profile.profile_id.clone(),
                scope_session_id.clone(),
                scope_zones.clone(),
            )
        };
        let session_scope = if permissions.filesystem_scope.is_host() {
            // Codex round-10 P1: a `danger_full_access` session resolves
            // to `FilesystemScope::Host` — file tools are deliberately
            // allowed to target absolute host paths outside the workspace,
            // and the sandbox is disabled. But the file tools PREFER an
            // attached `ctx.session_scope` over their `filesystem_scope`,
            // so attaching a scope here would silently re-fence a session
            // the operator explicitly granted host access. Host access is
            // a Solo-runtime single-user escape hatch, not a multi-tenant
            // isolation context, so the cross-tenant upload gate does not
            // apply. Leave the scope unset to honour the Host permission.
            tracing::debug!(
                profile_id = %profile.profile_id,
                session = %session_key,
                "skipping SessionScope: permissions grant Host filesystem access \
                 (danger_full_access) — file tools must keep their host reach",
            );
            None
        } else {
            match build_scope() {
                Ok(scope) => {
                    // PR-A: thread the per-profile plugin install directories
                    // through so file tools can reach the SKILL.md content the
                    // system prompt references. Codex round-2 BLOCKER 2: SKIP
                    // dirs that fail canonicalize (fail-closed) — a raw path
                    // later replaced by a symlink to `/etc` would otherwise be
                    // legitimised as `InSkillDir`.
                    let skill_dirs = crate::skills_scope::account_skill_read_zones(
                        &profile.data_dir,
                        &profile.plugin_dirs,
                    );
                    let scope = scope.with_skill_read_zones(skill_dirs).unwrap_or_else(|err| {
                        tracing::warn!(
                            profile_id = %profile.profile_id,
                            session = %session_key,
                            error = %err,
                            "with_skill_read_zones rejected one or more plugin_dirs; \
                             continuing without skill_read_zones (read_file may not reach SKILL.md references)",
                        );
                        build_scope().expect(
                            "scope was buildable above; rebuilding with the same inputs must succeed",
                        )
                    });
                    Some(Arc::new(scope))
                }
                Err(err) => {
                    // A scope that cannot be built (e.g. a non-absolute hint
                    // workspace) falls back to the unscoped legacy resolver —
                    // a no-regression degrade, not a hard failure.
                    tracing::warn!(
                        profile_id = %profile.profile_id,
                        session = %session_key,
                        workspace_root = %workspace_root.display(),
                        error = %err,
                        "SessionScope construction failed; bootstrap continues without scope (legacy resolver path)",
                    );
                    None
                }
            }
        };
        // A host-bound app session is always fenced to its workspace: never
        // fall back to the unscoped legacy resolver.
        if bound_memory_namespace.is_some() && session_scope.is_none() {
            eyre::bail!("session {session_key} is host-bound but its workspace scope failed");
        }
        let session_scope = with_context_read_view(session_scope, &bootstrapped_binding)
            .wrap_err_with(|| {
                format!("session {session_key}: the read view of its peer's folder")
            })?;

        // The prompt's slash commands (`/router`, `/queue`, …) are handled by
        // bus channels only; serve sessions get the client's own commands
        // instead, via `apply_client_commands` (octoscode#664).
        let base_prompt =
            strip_slash_commands(&profile.prompt_parts.pre_memory_for_registry(&tools));
        let mut agent = Agent::new_shared(
            AgentId::new("api"),
            profile.llm.clone(),
            Arc::clone(&tools),
            memory.episodes.clone(),
        )
        .with_config(
            profile
                .session_defaults
                .clone()
                .unwrap_or_else(|| configured_agent_defaults(profile)),
        )
        // M11-F regression fix (#891): propagate the pre-assembled
        // profile-scope system prompt onto the per-session agent. The
        // profile assembled it once during `ProfileRuntime::bootstrap`
        // via `build_system_prompt` + the SKILL.md fragment-append
        // loop, so every session for the profile inherits the same
        // skill-aware guidance (the mofa-fm "call fm_tts directly"
        // note, future skill-injected guidance, etc.). Without this
        // line, the agent's prompt would fall back to the
        // `Agent::new_shared` default and the LLM would lose its
        // skill-aware routing.
        .with_system_prompt(base_prompt)
        .with_file_state_cache(file_state_cache)
        .with_subagent_output_router(subagent_output_router)
        .with_subagent_summary_generator(subagent_summary_generator)
        .with_sandbox_config(sandbox.clone())
        // #1696: session-scoped tools (goal_get/goal_update) resolve their
        // session from ToolContext::parent_session_key — thread it on the
        // runtime-held agent exactly like the per-turn AppUI rebuild does.
        .with_parent_session_key(session_key.to_string())
        .with_workspace_root(workspace_root.clone())
        .with_recall(memory.recall.clone());

        if let Some(coding_profile) = profile.agent_profile.clone() {
            let definitions = Arc::new(octos_agent::agents::AgentDefinitions::load_dir(
                &workspace_root.join("agents"),
            )?);
            coding_profile.validate_against_registry(&definitions)?;
            agent = agent
                .with_profile(coding_profile)
                .with_agent_definitions(definitions);
        }

        // Phase 1 of the SessionScope migration: attach the constructed
        // scope to the per-session agent. `None` keeps pre-Phase-1
        // behaviour byte-for-byte (no consumer reads the field yet).
        if let Some(scope) = session_scope {
            agent = agent.with_session_scope(scope);
        }

        // #1768: opt-in git-backed workspace snapshots before mutating tools
        // (chat.rs parity). Separate git dir under `<data_dir>/snapshots/` —
        // the session's own repo/index is never touched; silently unavailable
        // without a git binary (`SnapshotManager::new` returns None, logs once).
        if let Some(snapshot_cfg) = profile.snapshots.as_ref().filter(|cfg| cfg.enabled) {
            if let Some(manager) = octos_agent::SnapshotManager::new(
                profile.data_dir.join("snapshots"),
                workspace_root.clone(),
                snapshot_cfg.keep_last,
            ) {
                agent = agent.with_snapshot_manager(std::sync::Arc::new(manager));
            }
        }

        // Memory rides the per-session agent as a NAMED prompt segment
        // (chat.rs pattern) instead of being frozen into the profile's
        // bootstrap prompt String: serve profiles bootstrapped before any
        // consolidation carried an empty block forever (the model then
        // fabricates a "memory bank" when asked). The provider re-renders
        // the segment at each turn start when MEMORY.md / daily notes /
        // bank change on disk (one fingerprint stat per turn otherwise).
        agent.set_prompt_segment(SLASH_COMMANDS_SEGMENT_NAME, String::new());
        let memory_ctx = memory
            .memory_store
            .get_injectable_context(profile.memory_inject_tokens)
            .await;
        agent.set_prompt_segment(
            octos_agent::MEMORY_SEGMENT_NAME,
            octos_agent::compose_memory_segment(&memory_ctx, memory.refresh_enabled),
        );
        // Contract parity with chat.rs: `memory.refresh.enabled = false`
        // means NO per-turn memory re-read — the segment stays as seeded
        // at session bootstrap. Default-on makes disabled an explicit
        // opt-out.
        // A namespaced session always re-renders per turn (its own
        // `save_memory` writes must show up), without the capture policy that
        // advertises the profile-only `memory_note` path.
        if profile.memory_refresh_enabled || memory.namespace.is_some() {
            agent.add_prompt_segment_provider(Arc::new(
                octos_agent::MemorySegmentProvider::new(
                    memory.memory_store.clone(),
                    profile.memory_inject_tokens,
                    memory.refresh_enabled,
                )
                .with_recall(memory.recall.clone(), profile.embedder.clone()),
            ));
        }
        // Post-memory half AFTER the named segment — the pre-refactor
        // order (memory before skills/tool guidance).
        if !profile.prompt_parts.post_memory.is_empty() {
            agent.append_system_prompt(&profile.prompt_parts.post_memory);
        }

        // M11-F regression fix REG-3: propagate the profile-scope
        // [`octos_agent::HookExecutor`] onto the per-session agent.
        // `ProfileRuntime::bootstrap` assembled it once from
        // `config.hooks + plugin_result.hooks`; without this chain
        // call, the api-mode agent would silently lose every
        // `before_tool_call` / `after_tool_call` / `before_llm_call` /
        // `after_llm_call` hook configured for the profile, breaking
        // parity with `octos gateway`.
        if let Some(hooks) = profile.hook_executor.clone() {
            agent = agent.with_hooks(hooks);
        }

        // #2246 — populate the hook payload context at session construction,
        // where both ids are known; before this, chat/serve-stdio turns fired
        // `before_llm_call` / `after_llm_call` / `after_tool_call` payloads
        // with no `session_id` / `profile_id` (gateway sessions were already
        // covered via `ActorFactory::hook_context_template`).
        agent = agent.with_hook_context(octos_agent::HookContext {
            session_id: Some(session_key.to_string()),
            profile_id: Some(profile.profile_id.clone()),
        });

        // RFC-1 (issue #1290): same pattern for the `mofa_make`
        // dispatcher. The loader registered it but its `Weak<ToolRegistry>`
        // back-reference needs the Arc-wrapped registry; we plant it here.
        agent.wire_mofa_make_dispatcher();

        let agent = Arc::new(agent);

        // Step 6: open the SessionManager at the resolved sessions root.
        //
        // The on-disk layout (`<root>/sessions/` +
        // `<root>/users/<base>/sessions/<topic>.jsonl`) already namespaces by
        // SessionKey via `encode_path_component`, so re-rooting it at
        // `<cwd>/.octos` (per-project storage) instead of `profile.data_dir`
        // relocates the whole store with zero storage-code change — the store
        // is fully root-parameterized.
        //
        // `resolve_sessions_root` returns `profile.data_dir` for every
        // no-hint session (web-chat, gateway) and for every session while
        // `sessions_in_cwd` is off, so this is byte-identical to the historic
        // `SessionManager::open(&profile.data_dir)` unless a cwd-scoped AppUi
        // session has explicitly opted in. Sidecars that build their path
        // from `sessions.data_dir()` (reasoning-effort, task ledger) follow to
        // the same root by construction.
        let sessions_root = resolve_sessions_root(
            profile,
            &workspace_root,
            had_workspace_hint,
            sessions_in_cwd,
        );
        // Keep a project-local `.gitignore` under a freshly-created
        // `<cwd>/.octos` so transcripts never leak into the user's repo. No-op
        // for the profile-data-dir root (not a project working tree).
        if sessions_root != profile.data_dir {
            ensure_session_store_gitignore(&sessions_root);
        }
        let sessions = Arc::new(tokio::sync::Mutex::new(
            SessionManager::open(&sessions_root).wrap_err("failed to open session manager")?,
        ));
        // Remember only an actual project transcript store. A gateway's
        // derived workspace, an app-owned namespace or an ephemeral chat must
        // not be rediscovered as a project session after a restart.
        if !bootstrapped_binding.is_bound_or_refused()
            && sessions_root == project_sessions_root(&workspace_root, &profile.profile_id)
        {
            if let Err(error) =
                super::workspace_history::remember(&profile.data_dir, &workspace_root)
            {
                tracing::warn!(%error, profile_id = %profile.profile_id, "could not remember project session store");
            }
        }

        Ok(Arc::new(Self {
            session_key,
            profile: Arc::clone(profile),
            workspace_root,
            plugin_work_dir,
            sandbox,
            permissions,
            tools,
            execution_environment: ExecutionEnvironmentCache::default(),
            agent,
            sessions_root,
            sessions,
            memory,
            app_binding: bootstrapped_binding,
            binding_peers_root,
            client_commands_owner: std::sync::Mutex::new(None),
        }))
    }
}

/// Shared configured defaults for OUP, chat and ACP session assembly.
pub(crate) fn configured_agent_defaults(profile: &ProfileRuntime) -> AgentConfig {
    AgentConfig {
        // Honor the configured `max_iterations` instead of a hardcoded cap.
        // The previous fixed `20` ignored config AND propagated to spawned
        // sub-agents (which inherit this config), starving multi-step
        // background tasks that need more iterations.
        max_iterations: resolve_session_max_iterations(profile.max_iterations),
        save_episodes: true,
        // Phase 4 (docs/ROBRIX-PHASE4-APPROVAL-FLOW-ADR.md)
        human_approval_rules: profile.human_approval_rules.clone(),
        // #1774: opt-in post-edit formatting (rustfmt/prettier/black/gofmt).
        format_after_edit: profile.format_after_edit,
        // #2172: thread the profile's gateway LLM knobs onto serve /
        // octoscode sessions, exactly as `octos chat` does. Without this a
        // profile-driven session silently ran with the built-in defaults
        // (greedy temperature=0.0, no sampler, 16384 max output) — dropping
        // the local-model repetition-collapse mitigations. Each is `None`
        // unless the operator set it, so cloud sessions are unchanged.
        //
        // #2166 precedence (documented contract): the CONFIGURED MODEL's
        // typed inference defaults win over the profile-gateway knobs,
        // which win over the provider defaults —
        //   session/turn override → model default → gateway knob → none.
        // The session/turn tier is applied per turn in the AppUI turn
        // path (ui_protocol_reasoning_effort.rs) and sits on top of
        // whatever this bootstrap composition produced.
        chat_max_tokens: profile
            .config
            .gateway
            .as_ref()
            .and_then(|g| g.max_output_tokens),
        chat_temperature: profile.config.model_temperature.or_else(|| {
            profile
                .config
                .gateway
                .as_ref()
                .and_then(|g| g.llm_temperature)
        }),
        chat_sampling_params: {
            let mut sampling = profile
                .config
                .gateway
                .as_ref()
                .and_then(|g| g.llm_sampling_params.clone());
            // #2166 × #2176 coordination: the typed per-model `top_p`
            // default overrides a same-named `top_p` key in the gateway
            // sampler passthrough map; every OTHER passthrough key
            // (`repeat_penalty`, …) is untouched. The passthrough stays
            // the escape hatch for params octos does not model.
            if let Some(top_p) = profile.config.model_top_p {
                sampling
                    .get_or_insert_with(serde_json::Map::new)
                    .insert("top_p".into(), serde_json::json!(top_p));
            }
            sampling
        },
        reasoning_effort: profile.config.model_reasoning_effort.or_else(|| {
            profile
                .config
                .gateway
                .as_ref()
                .and_then(|g| g.reasoning_effort)
        }),
        ..Default::default()
    }
}

/// Resolve the on-disk **root** for a session's transcript store.
///
/// This is the one seam that makes per-project (`appui.sessions_in_cwd`)
/// storage possible: the session store is fully root-parameterized
/// (`SessionManager::open(root)`), so relocating it is "pass a different
/// root", not "re-architect the store".
///
/// An explicit ephemeral `profile.session_store_root` takes precedence over
/// both rules below, keeping local chat history out of the profile/workspace.
///
/// - `sessions_in_cwd && had_hint` → `<cwd>/.octos/<profile_id>` (see
///   [`project_sessions_root`]), where `<cwd>` is the canonical hinted
///   workspace (`workspace_root` already canonicalizes a coding-agent hint).
///   The `<profile_id>` segment is load-bearing: two authenticated profiles
///   that point at the SAME project cwd must NOT share on-disk files — without
///   it, raw `web-*` session ids (whose key carries no profile) would collide
///   in `users/<base>/` and one profile could list/open/mutate another's
///   transcripts. It mirrors how the global store isolates profiles by giving
///   each its own `data_dir` root.
/// - otherwise → [`ProfileRuntime::data_dir`] — the historical per-profile
///   store. This includes **every** no-hint session (web-chat, gateway) and
///   **every** session while the flag is off, so per-cwd storage is inert for
///   the gateway/web-chat paths by construction and flipping the flag off is a
///   guaranteed no-op.
///
/// Only a hinted (coding-agent) session can relocate: a no-hint session's
/// `workspace_root` is the conventional `<data_dir>/users/<base>/workspace`
/// path, which is NOT a project the user launched in — rooting a store at
/// `<that>/.octos` would be meaningless, so `had_hint` gates it.
pub(crate) fn resolve_sessions_root(
    profile: &ProfileRuntime,
    workspace_root: &Path,
    had_hint: bool,
    sessions_in_cwd: bool,
) -> PathBuf {
    if let Some(root) = &profile.session_store_root {
        root.clone()
    } else if sessions_in_cwd && had_hint {
        project_sessions_root(workspace_root, &profile.profile_id)
    } else {
        profile.data_dir.clone()
    }
}

/// The per-project, per-profile session-store root: `<cwd>/.octos/<profile_id>`.
///
/// The single source of truth for the on-disk location of a cwd-scoped store,
/// shared by the write path ([`resolve_sessions_root`] /
/// [`resolve_sessions_root_from_hint`]) and the `session/list` cwd branch so
/// listing reads exactly where the runtime persisted. `profile_id` is
/// percent-encoded so an exotic profile id (`:`, `/`, …) can't escape the
/// project's `.octos` directory.
pub(crate) fn project_sessions_root(canonical_cwd: &Path, profile_id: &str) -> PathBuf {
    canonical_cwd
        .join(".octos")
        .join(octos_bus::session::encode_path_component(profile_id))
}

/// Who opened the profile recorded by a folder's sticky marker. Admin-owned
/// markers are the only implicit sticky an unscoped admin launch may follow.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum ActiveProfileOwner {
    Admin,
    User { profile_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ActiveProfileMarker {
    pub(crate) profile_id: String,
    /// `None` is an unowned legacy/unknown marker, never an admin-owned one.
    pub(crate) owner: Option<ActiveProfileOwner>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct ActiveProfileMarkerFile {
    version: u32,
    profile_id: String,
    owner: ActiveProfileOwner,
}

pub(crate) fn read_active_profile_marker(octos_dir: &Path) -> Option<ActiveProfileMarker> {
    let raw = std::fs::read_to_string(octos_dir.join("active-profile")).ok()?;
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if raw.starts_with('{')
        && let Ok(marker) = serde_json::from_str::<ActiveProfileMarkerFile>(raw)
        && marker.version == 1
        && !marker.profile_id.trim().is_empty()
    {
        let profile_id = marker.profile_id.trim().to_string();
        let owner = match marker.owner {
            ActiveProfileOwner::User {
                profile_id: owner_id,
            } if owner_id != profile_id => None,
            owner => Some(owner),
        };
        return Some(ActiveProfileMarker { profile_id, owner });
    }
    // Plain ids and unknown formats are legacy/unowned. Ordinary-user launch
    // semantics remain compatible, while admin resolution fails closed.
    Some(ActiveProfileMarker {
        profile_id: raw.to_string(),
        owner: None,
    })
}

/// Record `profile_id` and its opener as the folder's sticky profile at
/// `<cwd>/.octos/active-profile`. The marker is a SIBLING of the per-profile
/// store dir. Best-effort: a failure is logged and ignored; atomic
/// write-then-rename prevents a torn marker. Legacy plain-id markers are read
/// as unowned and are not migrated by this write path.
// The only non-test caller (`session/open` in the AppUI server) is api-gated;
// the writer stays unconditional next to the store layout it records.
#[cfg_attr(not(feature = "api"), allow(dead_code))]
pub(crate) fn write_active_profile_marker(
    canonical_cwd: &Path,
    profile_id: &str,
    owner: ActiveProfileOwner,
) {
    let octos_dir = canonical_cwd.join(".octos");
    if let Err(error) = std::fs::create_dir_all(&octos_dir) {
        tracing::warn!(
            dir = %octos_dir.display(),
            error = %error,
            "failed to create .octos dir for active-profile marker",
        );
        return;
    }
    let path = octos_dir.join("active-profile");
    let tmp = octos_dir.join("active-profile.tmp");
    let contents = match serde_json::to_vec(&ActiveProfileMarkerFile {
        version: 1,
        profile_id: profile_id.to_string(),
        owner,
    }) {
        Ok(contents) => contents,
        Err(error) => {
            tracing::warn!(error = %error, "failed to encode active-profile marker");
            return;
        }
    };
    if let Err(error) = std::fs::write(&tmp, contents) {
        tracing::warn!(
            path = %tmp.display(),
            error = %error,
            "failed to write active-profile marker",
        );
        return;
    }
    if let Err(error) = std::fs::rename(&tmp, &path) {
        tracing::warn!(
            path = %path.display(),
            error = %error,
            "failed to install active-profile marker",
        );
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Resolve the sessions root from a **raw** (possibly non-canonical) workspace
/// hint, as the [`super::SessionRuntimeCache`] sees it before bootstrap
/// canonicalizes it.
///
/// This is the cache-key form of [`resolve_sessions_root`]: it best-effort
/// canonicalizes the hint (idempotent when it is already canonical, which it is
/// on the `session/open` and turn/read paths — both pass a canonicalized cwd or
/// the stored canonical `workspace_root`) so the cache identity a session is
/// stored under matches the `sessions_root` bootstrap computes from the
/// canonical `workspace_root`. A canonicalization failure (e.g. a hint that
/// bootstrap will itself reject as banned/nonexistent) falls back to the raw
/// path; nothing is cached under a rejected hint, so the fallback is inert.
/// The ephemeral override takes precedence here too, so cache identity and
/// actual persistence cannot disagree about the local frontend's store root.
pub(crate) fn resolve_sessions_root_from_hint(
    profile: &ProfileRuntime,
    workspace_hint: Option<&Path>,
    sessions_in_cwd: bool,
) -> PathBuf {
    if let Some(root) = &profile.session_store_root {
        return root.clone();
    }
    match (sessions_in_cwd, workspace_hint) {
        (true, Some(hint)) => {
            let canonical = std::fs::canonicalize(hint).unwrap_or_else(|_| hint.to_path_buf());
            project_sessions_root(&canonical, &profile.profile_id)
        }
        _ => profile.data_dir.clone(),
    }
}

/// Idempotently write a `.gitignore` into a per-project session-store root
/// (`<cwd>/.octos`) so chat transcripts and runtime state never get committed
/// into the user's repository.
///
/// Uses `OpenOptions::create_new` (single `open(O_CREAT|O_EXCL)`), so a
/// pre-existing `.gitignore` (e.g. one an operator hand-wrote alongside a
/// project-local `.octos/config.json`) never has its existing content
/// rewritten — `AlreadyExists` only appends any MISSING managed patterns
/// (T-0531), leaving operator content intact. Best-effort: a failure to write
/// the ignore file must
/// not fail session bootstrap (the transcript store still works), it only
/// risks leaking runtime state into git, which we log.
///
/// Selective (not `*`) to match the `octos init` convention
/// (`init.rs`: `sessions/`, `tasks/`, `*.redb`) and to leave a
/// deliberately-committed `<cwd>/.octos/config.json` untouched; `users/` is
/// added because per-project transcripts + their sidecars land under
/// `<cwd>/.octos/users/<base>/sessions/`; `context_ledgers/` because the
/// per-turn context-manager snapshots (verbatim conversation content) are
/// rooted at this store alongside the transcript (#1666).
fn ensure_session_store_gitignore(sessions_root: &Path) {
    use std::io::Write;

    const GITIGNORE_BODY: &str = "\
# Managed by octos (appui.sessions_in_cwd): per-project session store.
# Chat transcripts and runtime state must not be committed to the repo.
sessions/
users/
tasks/
context_ledgers/
.session-lifecycle/
*.redb
";
    if let Err(error) = std::fs::create_dir_all(sessions_root) {
        tracing::warn!(
            root = %sessions_root.display(),
            error = %error,
            "failed to create per-project session store root for .gitignore",
        );
        return;
    }
    let path = sessions_root.join(".gitignore");
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut file) => {
            if let Err(error) = file.write_all(GITIGNORE_BODY.as_bytes()) {
                tracing::warn!(
                    path = %path.display(),
                    error = %error,
                    "failed to write per-project session-store .gitignore",
                );
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            // Operator/earlier bootstrap already placed one — never clobber.
            // T-0531: older stores were created before `.session-lifecycle/`
            // was managed, so the kernel's own lock/threads files still leak
            // into the user's `git status`. Append ONLY the missing managed
            // patterns (idempotent), preserving any operator content — this
            // never removes or rewrites what is already there.
            match std::fs::read_to_string(&path) {
                Ok(existing) => {
                    let mut missing: Vec<&str> = Vec::new();
                    let pat = ".session-lifecycle/";
                    if !existing.lines().any(|l| l.trim() == pat) {
                        missing.push(pat);
                    }
                    if !missing.is_empty() {
                        if let Ok(mut file) = std::fs::OpenOptions::new().append(true).open(&path) {
                            let mut body = String::new();
                            if !existing.ends_with('\n') && !existing.is_empty() {
                                body.push('\n');
                            }
                            body.push_str("# octos: kernel session-lifecycle files (T-0531)\n");
                            for pat in &missing {
                                body.push_str(pat);
                                body.push('\n');
                            }
                            let _ = file.write_all(body.as_bytes());
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %error,
                        "failed to read per-project session-store .gitignore for backfill",
                    );
                }
            }
        }
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                error = %error,
                "failed to create per-project session-store .gitignore",
            );
        }
    }
}

/// Write `WorkspacePolicy::for_session()` to
/// `<workspace_root>/.octos-workspace.toml` atomically, treating an
/// already-present policy file as success.
///
/// The atomicity matters under concurrent bootstrap or operator
/// edit: the M11-A doc-comment contract is "never overwrites a
/// manual edit". An `if !exists() { write }` pattern would leave a
/// TOCTOU window where two same-key bootstraps both see the file as
/// absent and both call `write_workspace_policy` — the second
/// truncates the first via `std::fs::write`. We delegate to
/// `octos_agent::workspace_policy::write_workspace_policy_if_absent`,
/// which uses `OpenOptions::create_new` — a single
/// `open(O_CREAT|O_EXCL)` syscall on Unix and the equivalent on
/// Windows — so it fails closed with `AlreadyExists` instead of
/// clobbering. M11-C added that helper alongside the existing
/// `write_workspace_policy` (no semantic change to the legacy
/// function).
fn bootstrap_session_policy(workspace_root: &Path) -> Result<()> {
    // Audit Gap-1 wiring (#2129): `detect_workspace_policy_kind` and
    // `WorkspacePolicy::for_coding` were authored for exactly this call
    // site but never called — every session workspace was bootstrapped as
    // the generic `for_session()` policy, so a repo full of Rust got the
    // same contract as a podcast workspace. The detector keys on observable
    // manifests (Cargo.toml / package.json / pyproject.toml), not LLM input.
    let policy = match octos_agent::workspace_policy::detect_workspace_policy_kind(workspace_root) {
        octos_agent::workspace_policy::WorkspacePolicyKind::Coding => WorkspacePolicy::for_coding(),
        _ => WorkspacePolicy::for_session(),
    };
    write_workspace_policy_if_absent(workspace_root, &policy)
        .wrap_err("failed to bootstrap session workspace policy")
}

/// Finite iteration backstop for the UNATTENDED lanes (`octos gateway`,
/// `octos serve` session actors) when neither the CLI flag nor
/// `gateway.max_iterations` is configured.
///
/// `AgentConfig::default().max_iterations` is `0` (unlimited) on purpose for
/// the INTERACTIVE lanes (`octos chat`, `octos acp`), where a human is attached
/// and can interrupt. An unattended channel session has no such operator: the
/// idle/activity timeouts never fire for a loop that keeps emitting progress,
/// `max_tokens` defaults to `None`, and loop detection is non-terminal, so this
/// cap is the only remaining backstop for an actively-looping agent. `50`
/// restores the ceiling these lanes had before the default became unlimited;
/// native spawned sub-agents resolve a separate dispatch policy (unlimited
/// by default, with explicit request/host overrides). An explicit `0` in
/// session config still means unlimited.
pub(crate) const UNATTENDED_MAX_ITERATIONS_FALLBACK: u32 =
    super::turn_policy::AUTONOMOUS_MAX_ITERATIONS;

/// Resolve the per-session agent iteration budget from the profile's
/// configured `gateway.max_iterations`, falling back to
/// [`UNATTENDED_MAX_ITERATIONS_FALLBACK`] when unset. Session actors are an
/// unattended lane, so they never inherit the unlimited interactive default;
/// native spawned sub-agents resolve their separate dispatch policy instead
/// of silently inheriting this unattended-session fallback.
fn resolve_session_max_iterations(configured: Option<u32>) -> u32 {
    super::turn_policy::max_iterations(configured, super::turn_policy::TurnIntent::Autonomous)
}

/// Resolve a per-session workspace root.
///
/// Honors a caller-supplied `workspace_hint` (coding-agent flow) when
/// the path passes basic safety validation; otherwise derives the
/// canonical `<data_dir>/users/<encoded session base>/workspace`
/// path. Mirrors the encoding produced by
/// `api/handlers.rs::api_session_workspace_dirs` so an existing
/// session can transparently pick up the new code path without
/// losing its workspace.
/// Map a raw session base-key into the [`is_safe_session_id`] alphabet
/// for use as a [`SessionScope`]'s INFORMATIONAL session-id field.
///
/// Channel-prefixed ids (`api:web-1234`) carry a `:` that the scope's id
/// field rejects. Scope path classification keys off the explicit
/// workspace, never this id, so collapsing every out-of-alphabet byte to
/// `-` is lossless for the field's only purpose (diagnostics/logging)
/// while guaranteeing the constructor's `is_safe_session_id` check
/// passes.
pub(crate) fn sanitize_scope_session_id(raw: &str) -> String {
    let mut out: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '#') {
                c
            } else {
                '-'
            }
        })
        .collect();
    // `is_safe_session_id` also rejects empty / "." / "..". The map above
    // already turns every `.` into `-`, so only the empty case remains.
    if out.is_empty() {
        out.push('-');
    }
    debug_assert!(
        is_safe_session_id(&out),
        "sanitize_scope_session_id must yield an is_safe_session_id-valid id: {out:?}",
    );
    out
}

fn resolve_workspace_root(
    profile: &ProfileRuntime,
    session_key: &SessionKey,
    workspace_hint: Option<PathBuf>,
) -> Result<PathBuf> {
    if let Some(hint) = workspace_hint {
        // #1377 (codex pre-merge P1): return the CANONICAL hint, not the raw
        // one. A raw hint carrying `..` (or symlinked ancestors) would flow
        // into `workspace_root`, and `SessionScope::multi_tenant_at_workspace`
        // rejects `..` — leaving the session UNSCOPED and back on the legacy
        // resolver that decodes global `up/` handles with no tenant check.
        // Canonicalizing collapses `..` and resolves symlinks so the scope is
        // always built and the upload-ownership gate applies.
        return validate_workspace_hint(&hint);
    }

    let encoded_base = octos_bus::session::encode_path_component(session_key.base_key());
    let path = profile
        .data_dir
        .join("users")
        .join(encoded_base)
        .join("workspace");
    Ok(path)
}

/// Basic safety validation for a caller-supplied workspace hint.
///
/// For M11 this replicates the lightweight checks
/// `validate_session_workspace_allowed` performs in
/// `api/ui_protocol.rs`. Full integration with the AppState-scoped
/// helper requires AppState, which `SessionRuntime::bootstrap`
/// does not see; lifting the workspace allowlist onto
/// `ProfileRuntime` is tracked as post-M11 work.
///
/// TODO(post-M11): extract a shared helper that both
/// `api/ui_protocol.rs::validate_session_workspace_allowed` and this
/// function can call. Today the two paths must stay synchronized by
/// inspection.
fn validate_workspace_hint(hint: &Path) -> Result<PathBuf> {
    // The hint must canonicalize (so we reject symlink traps and
    // nonexistent paths early). Callers that want to *create* a
    // workspace should pre-create the directory before passing the
    // hint, mirroring how the coding-agent UI today materializes the
    // repo before opening a session.
    if !hint.exists() {
        std::fs::create_dir_all(hint)
            .wrap_err_with(|| format!("create hinted workspace failed: {}", hint.display()))?;
    }
    let canonical = std::fs::canonicalize(hint)
        .wrap_err_with(|| format!("canonicalize workspace hint failed: {}", hint.display()))?;

    // Reject obviously-system locations. The list mirrors codex's
    // long-standing default; not exhaustive, but catches the
    // "ground truth" foot-guns that would let a session escape into
    // the host filesystem.
    let mut components = canonical.components();
    // Skip the root component.
    let _ = components.next();
    if let Some(first) = components.next() {
        let first = first.as_os_str();
        let banned: &[&str] = &[
            "etc", "sbin", "bin", "boot", "dev", "proc", "sys", "usr", "var", "root",
        ];
        for entry in banned {
            if first == std::ffi::OsStr::new(entry) {
                return Err(eyre::eyre!(
                    "workspace hint {} is rooted under a system path /{}",
                    canonical.display(),
                    entry
                ));
            }
        }
    }

    // Return the CANONICAL path (collapses `..`, resolves symlinks) so the
    // caller roots the session scope at a normalized workspace_root.
    Ok(canonical)
}
