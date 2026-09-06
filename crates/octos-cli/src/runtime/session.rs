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
}

impl SessionRuntime {
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
    /// 3. Create `<workspace_root>/skill-output/` (plugin work dir).
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

        // Step 3: plugin work dir.
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
        std::fs::create_dir_all(&plugin_work_dir).wrap_err_with(|| {
            format!(
                "create plugin work dir failed: {}",
                plugin_work_dir.display()
            )
        })?;

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
        let sandbox = sandbox_override
            .unwrap_or_else(|| permissions.apply_to_sandbox(&profile.default_sandbox));
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
        let scope_zones: Vec<PathBuf> = if workspace_under_data {
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
                    let skill_dirs =
                        octos_core::canonicalize_skill_read_zones(&profile.plugin_dirs);
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

        let mut agent = Agent::new_shared(
            AgentId::new("api"),
            profile.llm.clone(),
            Arc::clone(&tools),
            profile.memory.clone(),
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
        .with_system_prompt(profile.prompt_parts.pre_memory.clone())
        .with_file_state_cache(file_state_cache)
        .with_subagent_output_router(subagent_output_router)
        .with_subagent_summary_generator(subagent_summary_generator)
        .with_sandbox_config(sandbox.clone())
        // #1696: session-scoped tools (goal_get/goal_update) resolve their
        // session from ToolContext::parent_session_key — thread it on the
        // runtime-held agent exactly like the per-turn AppUI rebuild does.
        .with_parent_session_key(session_key.to_string())
        .with_workspace_root(workspace_root.clone());

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
        let memory_ctx = profile
            .memory_store
            .get_injectable_context(profile.memory_inject_tokens)
            .await;
        agent.set_prompt_segment(
            octos_agent::MEMORY_SEGMENT_NAME,
            octos_agent::compose_memory_segment(&memory_ctx, profile.memory_refresh_enabled),
        );
        // Contract parity with chat.rs: `memory.refresh.enabled = false`
        // means NO per-turn memory re-read — the segment stays as seeded
        // at session bootstrap. Default-on makes disabled an explicit
        // opt-out.
        if profile.memory_refresh_enabled {
            agent.add_prompt_segment_provider(Arc::new(octos_agent::MemorySegmentProvider::new(
                profile.memory_store.clone(),
                profile.memory_inject_tokens,
                true,
            )));
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

        Ok(Arc::new(Self {
            session_key,
            profile: Arc::clone(profile),
            workspace_root,
            plugin_work_dir,
            sandbox,
            permissions,
            tools,
            agent,
            sessions_root,
            sessions,
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

/// Record `profile_id` as the folder's sticky profile at
/// `<cwd>/.octos/active-profile`, so a later bare launch resumes the brain last
/// opened here — deterministic, beating the store-mtime recency fallback in
/// [`super::launch::derive_sticky_profile`].
///
/// The marker is a SIBLING of the per-profile store dir (NOT inside
/// [`project_sessions_root`]), and the id is written RAW (un-encoded) to
/// byte-match the reader in `scan_folder_sessions`, which trims it. Best-effort:
/// a failure is logged and ignored — stickiness degrades to recency, it never
/// blocks the session open. Atomic write-then-rename so a crash can't leave a
/// torn marker; last writer wins, which is exactly the "last profile opened
/// here" semantics we want.
// The only non-test caller (`session/open` in the AppUI server) is api-gated;
// the writer stays unconditional next to the store layout it records.
#[cfg_attr(not(feature = "api"), allow(dead_code))]
pub(crate) fn write_active_profile_marker(canonical_cwd: &Path, profile_id: &str) {
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
    if let Err(error) = std::fs::write(&tmp, profile_id.as_bytes()) {
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
/// project-local `.octos/config.json`) is left untouched — `AlreadyExists` is
/// treated as success. Best-effort: a failure to write the ignore file must
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
/// spawned sub-agents keep their own, higher default
/// (`DEFAULT_SPAWN_MAX_ITERATIONS` in `octos-agent/src/tools/spawn.rs`). An
/// explicit `0` in config still means unlimited.
pub(crate) const UNATTENDED_MAX_ITERATIONS_FALLBACK: u32 =
    super::turn_policy::AUTONOMOUS_MAX_ITERATIONS;

/// Resolve the per-session agent iteration budget from the profile's
/// configured `gateway.max_iterations`, falling back to
/// [`UNATTENDED_MAX_ITERATIONS_FALLBACK`] when unset. Session actors are an
/// unattended lane, so they never inherit the unlimited interactive default;
/// spawned sub-agents replace the budget with their own finite default at
/// dispatch time.
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::SystemTime;

    #[tokio::test]
    async fn read_only_session_bootstrap_does_not_write_the_selected_workspace() {
        let data = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let profile = make_profile(data.path().to_owned()).await;
        let runtime = SessionRuntime::bootstrap_with_permissions(
            &profile,
            SessionKey::with_profile("main", "cli", "read-only-migration"),
            Some(workspace.path().to_owned()),
            octos_agent::EffectivePermissions::read_only(),
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_dir(workspace.path()).unwrap().count(),
            0,
            "a read-only frontend must not create workspace policy or plugin scratch files"
        );
        assert!(runtime.plugin_work_dir.starts_with(data.path()));
    }

    #[test]
    fn should_fall_back_to_finite_unattended_cap_when_session_max_iterations_unset() {
        // A configured gateway.max_iterations must be respected (the bug was a
        // hardcoded 20 that ignored it and starved spawned sub-agents), and an
        // explicit 0 keeps its documented "unlimited" meaning.
        assert_eq!(resolve_session_max_iterations(Some(120)), 120);
        assert_eq!(resolve_session_max_iterations(Some(5)), 5);
        assert_eq!(resolve_session_max_iterations(Some(0)), 0);
        // Session actors are an UNATTENDED lane: nobody can interrupt a loop
        // that keeps emitting progress, so unset must resolve to a concrete
        // finite backstop — neither the unlimited interactive `AgentConfig`
        // default nor the old fixed 20-call cap.
        assert_eq!(resolve_session_max_iterations(None), 50);
        assert_eq!(
            resolve_session_max_iterations(None),
            UNATTENDED_MAX_ITERATIONS_FALLBACK
        );
        assert_ne!(
            resolve_session_max_iterations(None),
            AgentConfig::default().max_iterations,
            "unset must not inherit the unlimited interactive default"
        );
        assert_ne!(
            resolve_session_max_iterations(None),
            20,
            "unset must not collapse to the old hardcoded cap"
        );
    }

    #[tokio::test]
    async fn session_rebinding_preserves_local_tool_profile() {
        let data = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let mut profile = make_profile(data.path().to_owned()).await;
        let coding = octos_agent::profile::ProfileDefinition::from_toml_str(
            r#"version = 1
name = "minimal"
[tools]
mode = "allow_list"
tools = ["read_file"]
"#,
        )
        .unwrap();
        Arc::get_mut(&mut profile).unwrap().agent_profile = Some(Arc::new(coding));
        let runtime = SessionRuntime::bootstrap_with_permissions(
            &profile,
            SessionKey::with_profile("main", "acp", "narrow"),
            Some(workspace.path().to_owned()),
            octos_agent::EffectivePermissions::workspace_write(),
        )
        .await
        .unwrap();
        assert!(runtime.tools.get("read_file").is_some());
        assert!(
            runtime.tools.get("shell").is_none(),
            "cwd rebinding must not restore excluded tools"
        );
        assert!(runtime.tools.get("run_pipeline").is_none());
    }

    use octos_agent::sandbox::create_sandbox;
    use octos_agent::workspace_contract::{SpawnTaskContractResult, enforce_spawn_task_contract};
    use octos_agent::workspace_policy::{
        WORKSPACE_POLICY_FILE, WorkspacePolicy, read_workspace_policy,
    };
    use octos_agent::{
        ApprovalPolicy, EffectivePermissions, PermissionProfile, RuntimeMode, SandboxConfig,
        SandboxMode, ToolRegistry,
    };
    use octos_core::Message;
    use octos_llm::{ChatConfig, ChatResponse, LlmProvider, ToolSpec};
    use octos_memory::{EpisodeStore, MemoryStore};
    use tempfile::TempDir;

    use crate::runtime::ProfileRuntime;

    struct StubLlm;

    #[async_trait::async_trait]
    impl LlmProvider for StubLlm {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _config: &ChatConfig,
        ) -> eyre::Result<ChatResponse> {
            Err(eyre::eyre!("stub LLM not callable in M11-C tests"))
        }
        fn model_id(&self) -> &str {
            "stub-model"
        }
        fn provider_name(&self) -> &str {
            "stub"
        }
    }

    async fn make_profile(data_dir: PathBuf) -> Arc<ProfileRuntime> {
        make_profile_with_prompt(data_dir, "test-system-prompt".to_string()).await
    }

    async fn make_profile_with_prompt(
        data_dir: PathBuf,
        system_prompt: String,
    ) -> Arc<ProfileRuntime> {
        make_profile_with_prompt_and_sandbox(data_dir, system_prompt, SandboxConfig::default())
            .await
    }

    async fn make_profile_with_prompt_and_sandbox(
        data_dir: PathBuf,
        system_prompt: String,
        sandbox: SandboxConfig,
    ) -> Arc<ProfileRuntime> {
        std::fs::create_dir_all(&data_dir).unwrap();
        let memory = Arc::new(EpisodeStore::open(&data_dir).await.unwrap());
        let memory_store = Arc::new(MemoryStore::open(&data_dir).await.unwrap());
        let tool_config = Arc::new(octos_agent::ToolConfigStore::open(&data_dir).await.unwrap());
        let base_tools =
            ToolRegistry::with_builtins_and_sandbox(&data_dir, create_sandbox(&sandbox));
        Arc::new(ProfileRuntime {
            profile_id: "_main".to_string(),
            data_dir,
            session_store_root: None,
            config: crate::config::Config::default(),
            llm: Arc::new(StubLlm),
            goal_verifier_llm: None,
            adaptive_router: None,
            runtime_qos_catalog: None,
            primary_model_id: "stub-model".to_string(),
            provider_name: "stub".to_string(),
            credentials: HashMap::new(),
            skills_dir: None,
            plugin_env_template: Vec::new(),
            tool_policy: None,
            default_sandbox: sandbox,
            max_iterations: None,
            session_defaults: None,
            agent_profile: None,
            format_after_edit: false,
            snapshots: None,
            tool_specs: Arc::new(base_tools),
            plugin_tool_names: Vec::new(),
            skill_actions: Vec::new(),
            plugin_reload: None,
            plugin_dirs: Vec::new(),
            plugin_prompt_fragments: Vec::new(),
            plugin_hooks: Vec::new(),
            review_config: None,
            human_approval_rules: None,
            prompt_parts: crate::commands::gateway::prompt::GatewayPromptParts {
                pre_memory: system_prompt.clone(),
                post_memory: String::new(),
            },
            system_prompt,
            memory,
            memory_store,
            embedder: None,
            memory_inject_tokens: 2500,
            memory_refresh_enabled: true,
            memory_refresh: None,
            tool_config,
            cron_service: None,
            runtime_lifecycle: None,
            pipeline_factory: None,
            hook_executor: None,
            lane_routing: None,
            voice: crate::config::VoiceConfig::default(),
        })
    }

    async fn make_profile_with_sandbox(
        data_dir: PathBuf,
        sandbox: SandboxConfig,
    ) -> Arc<ProfileRuntime> {
        make_profile_with_prompt_and_sandbox(data_dir, "test-system-prompt".to_string(), sandbox)
            .await
    }

    #[tokio::test]
    async fn session_agent_renders_fresh_memory_segment() {
        // Serve sessions read MEMORY.md via the per-session agent's NAMED
        // segment + turn-start refresh — NOT a value frozen into the
        // profile bootstrap prompt (which fabricated-memory bugs came
        // from). Consolidations landing AFTER the session was created
        // must be visible on the next refresh.
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("profile-data");
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::create_dir_all(data_dir.join("memory")).unwrap();
        std::fs::write(
            data_dir.join("memory/MEMORY.md"),
            "Deploy day is Monday. (updated: 2026-07-08) ^mvzercg\n",
        )
        .unwrap();
        let profile = make_profile(data_dir.clone()).await;

        let rt = SessionRuntime::bootstrap(&profile, SessionKey::new("appui", "mem"), None)
            .await
            .expect("bootstrap");
        rt.agent.refresh_prompt_segments().await;
        let prompt = rt.agent.system_prompt_snapshot();
        assert!(
            prompt.contains("Deploy day is Monday"),
            "session agent must inject MEMORY.md: {prompt}"
        );

        // A consolidation AFTER session creation becomes visible on the
        // next turn-start refresh (fingerprint change).
        std::fs::write(
            data_dir.join("memory/MEMORY.md"),
            "Deploy day is Friday again. (updated: 2026-07-09) ^mvzercg\n",
        )
        .unwrap();
        rt.agent.refresh_prompt_segments().await;
        let prompt = rt.agent.system_prompt_snapshot();
        assert!(
            prompt.contains("Friday again"),
            "read-refresh must pick up post-bootstrap consolidations: {prompt}"
        );
    }

    #[tokio::test]
    async fn session_agent_threads_profile_gateway_llm_knobs() {
        // #2172: a serve / octoscode session must honor the profile's gateway
        // LLM knobs (temperature, sampler, max output) rather than silently
        // falling back to the greedy 0.0 / 16384 / no-sampler defaults.
        let dir = tempfile::tempdir().unwrap();
        let mut profile = make_profile(dir.path().to_path_buf()).await;
        let mut sp = serde_json::Map::new();
        sp.insert("repeat_penalty".to_string(), serde_json::json!(1.1));
        Arc::get_mut(&mut profile).unwrap().config.gateway = Some(crate::config::GatewayConfig {
            max_output_tokens: Some(32768),
            llm_temperature: Some(0.7),
            llm_sampling_params: Some(sp),
            reasoning_effort: Some(octos_llm::ReasoningEffort::High),
            ..Default::default()
        });
        let rt = SessionRuntime::bootstrap(&profile, SessionKey::new("appui", "gw"), None)
            .await
            .expect("bootstrap");
        let cfg = rt.agent.agent_config();
        assert_eq!(cfg.chat_max_tokens, Some(32768));
        assert_eq!(cfg.chat_temperature, Some(0.7));
        assert_eq!(
            cfg.chat_sampling_params
                .and_then(|m| m.get("repeat_penalty").cloned()),
            Some(serde_json::json!(1.1))
        );
        assert_eq!(cfg.reasoning_effort, Some(octos_llm::ReasoningEffort::High));
    }

    #[tokio::test]
    async fn session_agent_prefers_model_inference_defaults_over_gateway_knobs() {
        // #2166 precedence, pinned: the CONFIGURED PRIMARY model's typed
        // inference defaults win over the profile-gateway knobs —
        //   session/turn override → model default → gateway knob → none —
        // and the #2176 gateway sampler passthrough stays intact except for
        // the same-named `top_p` key, which the typed model default
        // overrides. (The session/turn tier is applied per turn on top of
        // this composition by ui_protocol_reasoning_effort.rs.)
        let dir = tempfile::tempdir().unwrap();
        let mut profile = make_profile(dir.path().to_path_buf()).await;
        {
            let cfg = Arc::get_mut(&mut profile).unwrap();
            cfg.config.model_temperature = Some(0.4);
            cfg.config.model_top_p = Some(0.9);
            cfg.config.model_reasoning_effort = Some(octos_llm::ReasoningEffort::High);
            let mut sp = serde_json::Map::new();
            sp.insert("repeat_penalty".to_string(), serde_json::json!(1.1));
            sp.insert("top_p".to_string(), serde_json::json!(0.8));
            cfg.config.gateway = Some(crate::config::GatewayConfig {
                max_output_tokens: Some(32768),
                llm_temperature: Some(0.7),
                llm_sampling_params: Some(sp),
                reasoning_effort: Some(octos_llm::ReasoningEffort::Low),
                ..Default::default()
            });
        }
        let rt = SessionRuntime::bootstrap(&profile, SessionKey::new("appui", "gw3"), None)
            .await
            .expect("bootstrap");
        let cfg = rt.agent.agent_config();
        // Model default beats the gateway knob.
        assert_eq!(
            cfg.chat_temperature,
            Some(0.4),
            "model default must win over gateway llm_temperature"
        );
        assert_eq!(
            cfg.reasoning_effort,
            Some(octos_llm::ReasoningEffort::High),
            "model default must win over gateway reasoning_effort"
        );
        // Typed model top_p overrides the same-named passthrough key…
        let top_p = cfg
            .chat_sampling_params
            .as_ref()
            .and_then(|m| m.get("top_p"))
            .and_then(|value| value.as_f64())
            .expect("typed top_p rides the sampler map");
        assert!(
            (top_p - 0.9).abs() < 1e-6,
            "typed model top_p must win over the gateway passthrough key: {top_p}"
        );
        // …while UNRELATED passthrough keys are untouched.
        assert_eq!(
            cfg.chat_sampling_params
                .as_ref()
                .and_then(|m| m.get("repeat_penalty").cloned()),
            Some(serde_json::json!(1.1)),
            "the #2176 passthrough stays intact for params octos does not model"
        );
        // Gateway-only fields still thread through unchanged.
        assert_eq!(cfg.chat_max_tokens, Some(32768));
    }

    #[tokio::test]
    async fn session_agent_keeps_defaults_when_profile_has_no_gateway_knobs() {
        // Cloud-safety: a profile without the gateway knobs yields None (the
        // built-in defaults), so serve behavior is unchanged.
        let dir = tempfile::tempdir().unwrap();
        let profile = make_profile(dir.path().to_path_buf()).await;
        let rt = SessionRuntime::bootstrap(&profile, SessionKey::new("appui", "gw2"), None)
            .await
            .expect("bootstrap");
        let cfg = rt.agent.agent_config();
        assert_eq!(cfg.chat_max_tokens, None);
        assert_eq!(cfg.chat_temperature, None);
        assert_eq!(cfg.chat_sampling_params, None);
    }

    #[tokio::test]
    async fn memory_segment_keeps_pre_skills_slot() {
        // codex round-3 P2: the memory block must keep its pre-refactor
        // position — after bootstrap/soul, BEFORE the skills/tool-prefs
        // half — or persisted user memory gains precedence over guidance
        // that always followed it.
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("profile-data");
        std::fs::create_dir_all(data_dir.join("memory")).unwrap();
        std::fs::write(
            data_dir.join("memory/MEMORY.md"),
            "A very durable fact. (updated: 2026-07-09) ^mvvvvvv\n",
        )
        .unwrap();
        let profile = make_profile(data_dir.clone()).await;

        let rt = SessionRuntime::bootstrap(&profile, SessionKey::new("appui", "order"), None)
            .await
            .expect("bootstrap");
        rt.agent.refresh_prompt_segments().await;
        let prompt = rt.agent.system_prompt_snapshot();
        let memory_pos = prompt
            .find("A very durable fact")
            .expect("memory segment present");
        if let Some(tool_prefs_pos) = prompt.find("## Tool Preferences") {
            assert!(
                memory_pos < tool_prefs_pos,
                "memory must precede the tool-prefs half: mem={memory_pos} prefs={tool_prefs_pos}"
            );
        }
        if let Some(skills_pos) = prompt.find("## Available Skills") {
            assert!(
                memory_pos < skills_pos,
                "memory must precede the skills half: mem={memory_pos} skills={skills_pos}"
            );
        }
    }

    #[tokio::test]
    async fn bootstrap_with_two_hints_yields_distinct_workspaces() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("profile-data");
        let profile = make_profile(data_dir.clone()).await;

        let hint_a = tmp.path().join("repo-a");
        let hint_b = tmp.path().join("repo-b");

        let key_a = SessionKey::new("appui", "a");
        let key_b = SessionKey::new("appui", "b");

        let rt_a = SessionRuntime::bootstrap(&profile, key_a, Some(hint_a.clone()))
            .await
            .expect("bootstrap A");
        let rt_b = SessionRuntime::bootstrap(&profile, key_b, Some(hint_b.clone()))
            .await
            .expect("bootstrap B");

        assert_ne!(rt_a.workspace_root, rt_b.workspace_root);
        assert_ne!(rt_a.plugin_work_dir, rt_b.plugin_work_dir);
        assert!(rt_a.plugin_work_dir.starts_with(&rt_a.workspace_root));
        assert!(rt_b.plugin_work_dir.starts_with(&rt_b.workspace_root));
        // Same parent profile Arc.
        assert!(Arc::ptr_eq(&rt_a.profile, &rt_b.profile));
    }

    #[tokio::test]
    async fn bootstrap_roots_scope_at_repo_for_workspace_hint_session() {
        // #1377 Phase-3-B: a coding-agent `workspace_hint` session whose
        // repo lives OUTSIDE the profile data dir gets a tenant scope
        // rooted AT the repo (root == workspace, no shared zones). This
        // (a) keeps the per-turn agent tenant-bound so the `up/` upload
        // gate fires, and (b) makes scope.workspace() == workspace_root so
        // the WS per-turn handler propagates it instead of skipping to the
        // unscoped legacy resolver (the old Phase-3-A mismatch branch).
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("profile-data");
        let profile = make_profile(data_dir.clone()).await;

        // Repo hint is a sibling of the data dir -> NOT under it.
        let repo = tmp.path().join("some-repo");
        std::fs::create_dir_all(&repo).unwrap();
        // #1377 (codex pre-merge P1): the hint is now CANONICALIZED before it
        // becomes workspace_root (so a `..`/symlinked hint can't slip through
        // unscoped). Compare against the canonical form (`/tmp`->`/private/tmp`
        // on macOS).
        let repo_canon = std::fs::canonicalize(&repo).unwrap();
        let key = SessionKey::new("appui", "coding-1");
        let rt = SessionRuntime::bootstrap(&profile, key, Some(repo.clone()))
            .await
            .expect("bootstrap with workspace hint");

        assert_eq!(rt.workspace_root, repo_canon);
        assert!(
            !repo_canon.starts_with(&data_dir),
            "test setup: repo must be outside data dir"
        );

        let scope = rt
            .agent
            .session_scope()
            .expect("hint session now carries a tenant scope")
            .clone();
        assert_eq!(scope.tenant_id(), Some(profile.profile_id.as_str()));
        // root == workspace == canonical repo; no shared zones (out-of-tree).
        assert_eq!(scope.workspace(), repo_canon.as_path());
        assert_eq!(scope.root(), repo_canon.as_path());
        assert!(scope.shared_zones().is_empty());
        // Critical for propagation: scope.workspace() == workspace_root.
        assert_eq!(scope.workspace(), rt.workspace_root.as_path());
    }

    #[tokio::test]
    async fn bootstrap_with_explicit_sandbox_overrides_are_per_session() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("profile-data");
        let profile = make_profile_with_sandbox(
            data_dir,
            SandboxConfig {
                allow_network: true,
                ..SandboxConfig::default()
            },
        )
        .await;

        let gamma_sandbox = SandboxConfig {
            allow_network: false,
            ..profile.default_sandbox.clone()
        };

        let gamma = SessionRuntime::bootstrap_with_permissions_and_sandbox(
            &profile,
            SessionKey::new("api", "gamma"),
            Some(tmp.path().join("gamma")),
            EffectivePermissions::workspace_write(),
            Some(gamma_sandbox),
            false,
        )
        .await
        .expect("gamma bootstrap");
        let delta = SessionRuntime::bootstrap_with_permissions_and_sandbox(
            &profile,
            SessionKey::new("api", "delta"),
            Some(tmp.path().join("delta")),
            EffectivePermissions::workspace_write(),
            None,
            false,
        )
        .await
        .expect("delta bootstrap");

        assert!(profile.default_sandbox.allow_network);
        assert!(!gamma.sandbox.allow_network);
        assert!(delta.sandbox.allow_network);
        assert_ne!(gamma.workspace_root, delta.workspace_root);
        assert!(!Arc::ptr_eq(&gamma.tools, &delta.tools));
    }

    #[tokio::test]
    async fn bootstrap_without_hint_writes_default_policy() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("profile-data");
        let profile = make_profile(data_dir.clone()).await;

        let key = SessionKey::new("api", "no-hint");
        let rt = SessionRuntime::bootstrap(&profile, key.clone(), None)
            .await
            .expect("bootstrap");

        let expected_encoded = octos_bus::session::encode_path_component(key.base_key());
        let expected = data_dir
            .join("users")
            .join(expected_encoded)
            .join("workspace");
        assert_eq!(rt.workspace_root, expected);

        // Policy file exists and round-trips as the canonical
        // session policy.
        let policy_path = rt.workspace_root.join(WORKSPACE_POLICY_FILE);
        assert!(
            policy_path.exists(),
            "policy file missing at {}",
            policy_path.display()
        );
        let loaded = read_workspace_policy(&rt.workspace_root)
            .unwrap()
            .expect("policy loadable");
        let expected_policy = WorkspacePolicy::for_session();
        assert_eq!(loaded, expected_policy);

        // Plugin work dir is created and lives under workspace root.
        assert!(rt.plugin_work_dir.is_dir());
        assert!(rt.plugin_work_dir.starts_with(&rt.workspace_root));
    }

    #[tokio::test]
    async fn bootstrap_attaches_session_scope_for_safe_session_id() {
        // Phase 1 of the SessionScope migration (PR #1198 follow-up):
        // bootstrap a SPA-shape session_id (alphanumeric + `-` + `_` +
        // `#`) and confirm the per-session agent carries a
        // multi-tenant SessionScope. Phase 1 only asserts the field
        // is present + the workspace shape matches
        // `<data>/users/<id>/workspace`; no consumer reads it yet.
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("profile-data");
        let profile = make_profile(data_dir.clone()).await;

        let session_id = "web-1779574360679-o8x9kv";
        let key = SessionKey(session_id.to_string());

        let rt = SessionRuntime::bootstrap(&profile, key, None)
            .await
            .expect("bootstrap with safe SPA id");

        let scope = rt
            .agent
            .session_scope()
            .expect("safe session id yields a SessionScope")
            .clone();
        let expected_workspace = data_dir.join("users").join(session_id).join("workspace");
        assert_eq!(scope.workspace(), expected_workspace.as_path());
        assert_eq!(scope.root(), data_dir.as_path());
    }

    #[tokio::test]
    async fn bootstrap_attaches_tenant_scope_for_channel_prefixed_session_id() {
        // #1377 Phase-3-B reconciliation: channel-prefixed (`:`) session
        // ids previously left `session_scope` unset, dropping the per-turn
        // agent onto the unscoped legacy resolver (which decodes a
        // process-global `up/` upload handle with NO tenant check). The
        // scope is now bound to the REAL (percent-encoded) workspace path
        // — not a path re-derived from the raw id — so the upload-
        // ownership gate in `resolve_for_scope` applies.
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("profile-data");
        let profile = make_profile(data_dir.clone()).await;

        // SessionKey with a `:` channel prefix — outside is_safe_session_id.
        let key = SessionKey::new("api", "web-1234");
        let rt = SessionRuntime::bootstrap(&profile, key, None)
            .await
            .expect("bootstrap must succeed for channel-prefixed id");

        let scope = rt
            .agent
            .session_scope()
            .expect("channel-prefixed id now yields a tenant-bound scope")
            .clone();
        // Tenant binding present -> the resolver's `up/` ownership gate fires.
        assert_eq!(scope.tenant_id(), Some(profile.profile_id.as_str()));
        // No-hint session: workspace_root is the raw `<data>/users/<enc>/
        // workspace` path, so scope.root() stays the raw data_dir (the form
        // that prefixes workspace_root).
        assert_eq!(scope.root(), data_dir.as_path());
        // Workspace matches the runtime's workspace_root, so per-turn
        // propagation attaches it.
        assert_eq!(scope.workspace(), rt.workspace_root.as_path());
        assert!(rt.workspace_root.starts_with(&data_dir));
    }

    /// #1377 Phase-3-B reconciliation (formerly a Phase-3-A design-pin
    /// asserting these stay unscoped). The old pin required "reconciling
    /// the encoded-vs-raw workspace path mismatch first" before building
    /// a scope for channel-prefixed ids — that reconciliation is now
    /// done: [`SessionScope::multi_tenant_at_workspace`] binds the scope
    /// to the REAL encoded `workspace_root` rather than re-deriving a
    /// path from the raw id. So every channel-prefixed legacy session now
    /// carries a tenant-bound scope and routes through the gated
    /// resolver, closing the cross-tenant `up/` upload gap. This test
    /// PINS the new contract.
    #[tokio::test]
    async fn channel_prefixed_session_ids_get_tenant_scope() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("profile-data");
        let profile = make_profile(data_dir.clone()).await;

        // Three flavours of channel-prefixed legacy ids that fail
        // `is_safe_session_id` (the `:` is rejected). Each must succeed at
        // bootstrap and now yield a tenant-bound scope.
        let cases = ["api:web-1234", "telegram:12345", "discord:guild#chan"];
        for raw in cases {
            // Split on `:` to round-trip through SessionKey::new's
            // (channel, base) constructor.
            let mut split = raw.splitn(2, ':');
            let channel = split.next().expect("channel half");
            let base = split.next().expect("base half");
            let key = SessionKey::new(channel, base);
            let rt = SessionRuntime::bootstrap(&profile, key.clone(), None)
                .await
                .expect("bootstrap must succeed for legacy channel id");
            let scope = rt.agent.session_scope().unwrap_or_else(|| {
                panic!(
                    "channel-prefixed id {raw:?} (key={key:?}) must now carry a tenant scope \
                     (reconciliation done: scope bound to the real encoded workspace_root)"
                )
            });
            assert_eq!(
                scope.tenant_id(),
                Some(profile.profile_id.as_str()),
                "scope for {raw:?} must be tenant-bound so the upload-ownership gate fires",
            );
            assert_eq!(
                scope.workspace(),
                rt.workspace_root.as_path(),
                "scope for {raw:?} must be rooted at the real workspace_root",
            );
        }
    }

    /// Phase 3-A plumbing follow-up (gap #1 from codex review of
    /// Phase 2-C): the UI Protocol WS turn handler rebuilds an
    /// `Agent` per-turn via `Agent::new_shared(...).with_*(...)` so
    /// per-turn callbacks (reporter, prompt-context-bridge) layer in
    /// without mutating the cached `SessionRuntime`. Before this fix
    /// the rebuild did NOT copy `session_scope` off the runtime
    /// session's agent, so every per-turn agent observed
    /// `session_scope: None` and the Phase-2 consumers fell through
    /// to legacy paths (= mini5 NEW-06 contamination on the WS
    /// chat path).
    ///
    /// This test exercises the EXACT pattern the WS turn handler
    /// uses: bootstrap a SessionRuntime, then build a fresh
    /// per-turn Agent via the same `new_shared(...).with_session_scope(...)`
    /// chain. It asserts the per-turn agent observes the SAME
    /// SessionScope `Arc` the runtime session's agent carries (via
    /// `Arc::ptr_eq`), proving the propagation is a clone of the
    /// runtime's scope rather than an independently-built one.
    #[tokio::test]
    async fn ui_protocol_ws_turn_agent_inherits_session_scope() {
        use octos_agent::Agent;

        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("profile-data");
        let profile = make_profile(data_dir.clone()).await;

        let session_id = "web-1779574360680-ws01ab";
        let key = SessionKey(session_id.to_string());
        let rt = SessionRuntime::bootstrap(&profile, key, None)
            .await
            .expect("bootstrap session runtime");

        // Pre-condition: the cached SessionRuntime's agent carries a
        // SessionScope (Phase 1 contract — already covered by
        // `bootstrap_attaches_session_scope_for_safe_session_id`).
        let runtime_scope = rt
            .agent
            .session_scope()
            .expect("safe session id must yield a SessionScope")
            .clone();

        // Mirror the WS turn handler's per-turn rebuild
        // (`api/ui_protocol.rs::15608`), INCLUDING the codex round-1
        // P1 gate (`scope.workspace() == workspace_root`):
        //   let request_agent = Agent::new_shared(...).with_*(...);
        //   if let Some(scope) = session_runtime.agent.session_scope() {
        //       if scope.workspace() == session_runtime.workspace_root.as_path() {
        //           request_agent = request_agent.with_session_scope(scope.clone());
        //       }
        //   }
        // For default-layout sessions (no workspace hint) the scope's
        // workspace matches `workspace_root` so the gate passes and
        // the scope is propagated.
        let tools = Arc::new(rt.tools.snapshot_excluding(&[]));
        let mut request_agent = Agent::new_shared(
            AgentId::new(format!("ui-protocol-{}", uuid::Uuid::now_v7())),
            profile.llm.clone(),
            tools,
            profile.memory.clone(),
        );
        if let Some(scope) = rt.agent.session_scope() {
            if scope.workspace() == rt.workspace_root.as_path() {
                request_agent = request_agent.with_session_scope(scope.clone());
            }
        }

        let turn_scope = request_agent
            .session_scope()
            .expect("per-turn agent must inherit session_scope from runtime")
            .clone();
        assert!(
            Arc::ptr_eq(&runtime_scope, &turn_scope),
            "per-turn WS agent must point at the SAME SessionScope Arc the cached \
             SessionRuntime holds, not a freshly-built one (proves scope is propagated, \
             not reconstructed)",
        );
        // Workspace shape sanity — the per-turn agent's scope still
        // points at the per-session workspace dir, not the profile root.
        assert_eq!(turn_scope.workspace(), runtime_scope.workspace());
        assert_eq!(turn_scope.root(), data_dir.as_path());
    }

    /// #1377 Phase-3-B (formerly the Phase-3-A round-4 skip-pin): when a
    /// coding-agent UI opens a session with an explicit `cwd` hint, the
    /// cached `SessionRuntime` honours the hint for `workspace_root`, and
    /// bootstrap NOW roots the `SessionScope` at that same
    /// `workspace_root` (repo-rooted, root == workspace). So the cached
    /// scope's workspace MATCHES the runtime's, and the WS turn handler
    /// PROPAGATES it onto the per-turn agent — making the agent
    /// tenant-bound so the `up/` upload-ownership gate fires.
    ///
    /// Earlier rounds skipped propagation here because bootstrap built
    /// the scope from the canonical `<data>/users/<id>/workspace` layout
    /// (a workspace mismatch) and the scoped resolver misclassified
    /// `up/...` handles. Both are resolved: bootstrap reconciles the
    /// workspace, and uploads are materialized into `<workspace>/uploads/`
    /// so no raw `up/` handle reaches the scoped resolver.
    ///
    /// This test mirrors the WS turn rebuild and confirms hint sessions
    /// now carry a propagated, workspace-matched scope.
    #[tokio::test]
    async fn ui_protocol_ws_turn_agent_propagates_session_scope_on_workspace_hint() {
        use octos_agent::Agent;

        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("profile-data");
        let profile = make_profile(data_dir.clone()).await;

        // A safe SPA-shape session id PLUS a hint pointing at a different
        // repo path — i.e. the coding-agent UI flow.
        let session_id = "web-1779574360681-hintsx";
        let key = SessionKey(session_id.to_string());
        let hint_repo = tmp.path().join("coding-agent-repo");
        std::fs::create_dir_all(&hint_repo).unwrap();
        let rt = SessionRuntime::bootstrap(&profile, key, Some(hint_repo.clone()))
            .await
            .expect("bootstrap session runtime with workspace hint");

        // Pre-condition: workspace_root honours the hint AND the cached
        // scope is now rooted at that same workspace_root (the #1377
        // reconciliation), so they match by construction.
        let runtime_scope = rt
            .agent
            .session_scope()
            .expect("hint session yields a tenant-bound SessionScope")
            .clone();
        let canonical_workspace = std::fs::canonicalize(&hint_repo).unwrap();
        let runtime_workspace_canonical = std::fs::canonicalize(&rt.workspace_root).unwrap();
        assert_eq!(
            runtime_workspace_canonical, canonical_workspace,
            "workspace_root must honour the hint"
        );
        assert_eq!(
            runtime_scope.workspace(),
            rt.workspace_root.as_path(),
            "#1377: bootstrap now roots the scope at the hint workspace_root",
        );
        assert_eq!(
            runtime_scope.tenant_id(),
            Some(profile.profile_id.as_str()),
            "hint-session scope must be tenant-bound so the upload gate fires",
        );

        // Mirror the WS turn handler's per-turn rebuild + propagation
        // gate: the workspaces match, so the scope IS propagated.
        let tools = Arc::new(rt.tools.snapshot_excluding(&[]));
        let mut request_agent = Agent::new_shared(
            AgentId::new(format!("ui-protocol-{}", uuid::Uuid::now_v7())),
            profile.llm.clone(),
            tools,
            profile.memory.clone(),
        );
        if let Some(scope) = rt.agent.session_scope() {
            if scope.workspace() == rt.workspace_root.as_path() {
                request_agent = request_agent.with_session_scope(scope.clone());
            }
        }

        let turn_scope = request_agent.session_scope().expect(
            "per-turn agent must now carry the propagated, workspace-matched scope for \
             a workspace-hint session (#1377 Phase-3-B reconciliation)",
        );
        assert!(Arc::ptr_eq(turn_scope, &runtime_scope));
    }

    #[tokio::test]
    async fn bootstrap_attaches_distinct_session_scopes_per_session() {
        // Two SPA-shape sessions on the same profile each get their
        // own SessionScope pointing at their own per-session workspace
        // directory. Verifies the scope tracks `session_id`, not just
        // the parent profile.
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("profile-data");
        let profile = make_profile(data_dir.clone()).await;

        let key_a = SessionKey("web-1779000000000-aaa".to_string());
        let key_b = SessionKey("web-1779000000000-bbb".to_string());

        let rt_a = SessionRuntime::bootstrap(&profile, key_a.clone(), None)
            .await
            .expect("bootstrap A");
        let rt_b = SessionRuntime::bootstrap(&profile, key_b.clone(), None)
            .await
            .expect("bootstrap B");

        let scope_a = rt_a.agent.session_scope().expect("scope A").clone();
        let scope_b = rt_b.agent.session_scope().expect("scope B").clone();
        assert_ne!(scope_a.workspace(), scope_b.workspace());
        // Both still share the same tenant root.
        assert_eq!(scope_a.root(), scope_b.root());
    }

    #[tokio::test]
    async fn bootstrap_preserves_manual_policy_edits() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("profile-data");
        let profile = make_profile(data_dir.clone()).await;

        let hint = tmp.path().join("manual-edit");
        let key = SessionKey::new("api", "edited");

        // First bootstrap writes the default policy.
        let rt1 = SessionRuntime::bootstrap(&profile, key.clone(), Some(hint.clone()))
            .await
            .expect("bootstrap 1");
        let policy_path = rt1.workspace_root.join(WORKSPACE_POLICY_FILE);
        assert!(policy_path.exists());

        // Operator (or earlier session) hand-edits the policy.
        let sentinel = "# operator hand-edit do not overwrite\n";
        let original = std::fs::read_to_string(&policy_path).unwrap();
        let edited = format!("{sentinel}{original}");
        std::fs::write(&policy_path, &edited).unwrap();

        // Second bootstrap at the same workspace root must NOT
        // overwrite the operator's edits.
        let key2 = SessionKey::new("api", "edited-again");
        let _rt2 = SessionRuntime::bootstrap(&profile, key2, Some(hint.clone()))
            .await
            .expect("bootstrap 2");
        let after = std::fs::read_to_string(&policy_path).unwrap();
        assert!(
            after.starts_with(sentinel),
            "policy file was overwritten; expected sentinel preserved"
        );
        assert_eq!(after, edited);
    }

    /// M11 regression fix (#891): `SessionRuntime::bootstrap` must
    /// propagate the parent profile's pre-assembled `system_prompt`
    /// onto the per-session `Agent`. Without this, `/api/chat` and the
    /// UI Protocol WS path miss SKILL.md prompt fragments and the
    /// kimi-k2.5 LLM falls back to a "fm_voice_list precheck" pattern
    /// instead of going straight to `fm_tts`.
    #[tokio::test]
    async fn session_runtime_agent_receives_system_prompt_from_profile() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("profile-data");
        let profile = make_profile_with_prompt(
            data_dir.clone(),
            "DISTINCTIVE-PROFILE-PROMPT-789".to_string(),
        )
        .await;

        let key = SessionKey::new("api", "system-prompt-probe");
        let rt = SessionRuntime::bootstrap(&profile, key, None)
            .await
            .expect("bootstrap");

        let snapshot = rt.agent.system_prompt_snapshot();
        assert!(
            snapshot.contains("DISTINCTIVE-PROFILE-PROMPT-789"),
            "agent system prompt should inherit the profile-level prompt; got: {snapshot}",
        );
    }

    /// Build a `ProfileRuntime` like `make_profile`, but with a
    /// pre-constructed `Arc<HookExecutor>` stashed on the
    /// `hook_executor` field. Used by the M11-F REG-3 regression
    /// test below to assert end-to-end propagation onto the
    /// per-session agent.
    async fn make_profile_with_hooks(
        data_dir: PathBuf,
        executor: Arc<octos_agent::HookExecutor>,
    ) -> Arc<ProfileRuntime> {
        std::fs::create_dir_all(&data_dir).unwrap();
        let memory = Arc::new(EpisodeStore::open(&data_dir).await.unwrap());
        let memory_store = Arc::new(MemoryStore::open(&data_dir).await.unwrap());
        let tool_config = Arc::new(octos_agent::ToolConfigStore::open(&data_dir).await.unwrap());
        let sandbox = SandboxConfig::default();
        let base_tools =
            ToolRegistry::with_builtins_and_sandbox(&data_dir, create_sandbox(&sandbox));
        Arc::new(ProfileRuntime {
            profile_id: "_main".to_string(),
            data_dir,
            session_store_root: None,
            config: crate::config::Config::default(),
            llm: Arc::new(StubLlm),
            goal_verifier_llm: None,
            adaptive_router: None,
            runtime_qos_catalog: None,
            primary_model_id: "stub-model".to_string(),
            provider_name: "stub".to_string(),
            credentials: HashMap::new(),
            skills_dir: None,
            plugin_env_template: Vec::new(),
            tool_policy: None,
            default_sandbox: sandbox,
            max_iterations: None,
            format_after_edit: false,
            session_defaults: None,
            agent_profile: None,
            snapshots: None,
            tool_specs: Arc::new(base_tools),
            plugin_tool_names: Vec::new(),
            skill_actions: Vec::new(),
            plugin_reload: None,
            plugin_dirs: Vec::new(),
            plugin_prompt_fragments: Vec::new(),
            plugin_hooks: Vec::new(),
            review_config: None,
            human_approval_rules: None,
            system_prompt: "test-system-prompt".to_string(),
            prompt_parts: crate::commands::gateway::prompt::GatewayPromptParts {
                pre_memory: "test-system-prompt".to_string(),
                post_memory: String::new(),
            },
            memory,
            memory_store,
            embedder: None,
            memory_inject_tokens: 2500,
            memory_refresh_enabled: true,
            memory_refresh: None,
            tool_config,
            cron_service: None,
            runtime_lifecycle: None,
            pipeline_factory: None,
            hook_executor: Some(executor),
            lane_routing: None,
            voice: crate::config::VoiceConfig::default(),
        })
    }

    /// M11-F regression fix REG-3: when the parent `ProfileRuntime`
    /// carries a `hook_executor`, `SessionRuntime::bootstrap` must
    /// chain `.with_hooks(...)` onto the per-session `Agent` so the
    /// configured `before_tool_call` / `after_tool_call` /
    /// `before_llm_call` / `after_llm_call` hooks fire on api-mode
    /// turns, matching the pre-M11-F `serve.rs:1413` behaviour.
    #[tokio::test]
    async fn session_runtime_agent_inherits_profile_hooks() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("profile-data");
        let hook = octos_agent::HookConfig {
            event: octos_agent::HookEvent::BeforeLlmCall,
            command: vec!["/bin/true".to_string()],
            timeout_ms: 1000,
            tool_filter: Vec::new(),
            path_filter: Vec::new(),
            requires_bin: None,
        };
        let executor = Arc::new(octos_agent::HookExecutor::new(vec![hook]));
        let profile = make_profile_with_hooks(data_dir, executor.clone()).await;

        let key = SessionKey::new("api", "hook-probe");
        let rt = SessionRuntime::bootstrap(&profile, key, None)
            .await
            .expect("bootstrap");

        let agent_hooks = rt
            .agent
            .hooks()
            .expect("session agent must inherit profile hook_executor");
        assert!(
            Arc::ptr_eq(&agent_hooks, &executor),
            "agent.hooks() must be the same Arc as profile.hook_executor",
        );
    }

    #[tokio::test]
    async fn bootstrap_closes_workspace_policy_not_found_gap() {
        // This is the yangmi-gap proof: after bootstrap,
        // `enforce_spawn_task_contract` must NOT return
        // `NotConfigured { required: true, reason: "workspace policy not found" }`.
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("profile-data");
        let profile = make_profile(data_dir.clone()).await;

        let key = SessionKey::new("api", "yangmi");
        let rt = SessionRuntime::bootstrap(&profile, key, None)
            .await
            .expect("bootstrap");

        let result = enforce_spawn_task_contract(
            &rt.tools,
            "fm_tts",
            "test-tc",
            &[],
            SystemTime::now(),
            None,
            Arc::new(octos_agent::sandbox::NoSandbox),
        )
        .await;

        // The exact terminal outcome depends on which artefacts exist
        // on disk — without an `*.mp3` produced by the stub skill we
        // expect a `Failed` (no artefacts) rather than a `Satisfied`
        // — but the M11-C contract is that we MUST be past the
        // "workspace policy not found" `NotConfigured` rejection.
        match &result {
            SpawnTaskContractResult::NotConfigured { required, reason }
                if *required && reason.as_deref() == Some("workspace policy not found") =>
            {
                panic!("M11-C bootstrap failed to close the yangmi gap: {result:?}");
            }
            _ => {}
        }
    }

    #[tokio::test]
    async fn bootstrap_with_never_workspace_permissions_keeps_sandbox_and_workspace_scope() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("profile-data");
        let profile = make_profile(data_dir.clone()).await;
        let workspace = tmp.path().join("workspace-never");
        let outside = tmp.path().join("outside-never.txt");
        std::fs::write(&outside, "outside\n").unwrap();

        let permissions =
            EffectivePermissions::workspace_write().with_approval_policy(ApprovalPolicy::Never);
        let rt = SessionRuntime::bootstrap_with_permissions(
            &profile,
            SessionKey::new("api", "never-workspace"),
            Some(workspace),
            permissions,
        )
        .await
        .expect("bootstrap");

        assert_eq!(rt.permissions.approval_policy, ApprovalPolicy::Never);
        assert!(rt.sandbox.enabled);
        assert_eq!(rt.sandbox.mode, SandboxMode::Auto);

        let ask_result = rt
            .tools
            .execute(
                "shell",
                &serde_json::json!({ "command": "sudo printf nope" }),
            )
            .await
            .expect("shell result");
        assert!(!ask_result.success);
        assert!(ask_result.output.contains("approval_policy is never"));

        let outside_write = rt
            .tools
            .execute(
                "write_file",
                &serde_json::json!({
                    "path": outside.to_string_lossy(),
                    "content": "blocked\n"
                }),
            )
            .await
            .expect("write_file result");
        assert!(!outside_write.success);
        assert!(outside_write.output.contains("outside working directory"));
        assert_eq!(std::fs::read_to_string(outside).unwrap(), "outside\n");
    }

    #[tokio::test]
    async fn bootstrap_with_dangerous_solo_permissions_disables_sandbox_and_uses_host_scope() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("profile-data");
        let profile = make_profile(data_dir.clone()).await;
        let workspace = tmp.path().join("workspace-danger");
        let outside = tmp.path().join("outside-danger.txt");

        let permissions = EffectivePermissions::for_runtime(
            PermissionProfile::DangerFullAccess,
            RuntimeMode::Solo,
        )
        .expect("solo dangerous permissions");
        let rt = SessionRuntime::bootstrap_with_permissions(
            &profile,
            SessionKey::new("api", "dangerous-solo"),
            Some(workspace),
            permissions,
        )
        .await
        .expect("bootstrap");

        assert_eq!(
            rt.permissions.permission_profile,
            PermissionProfile::DangerFullAccess
        );
        // Codex round-10 P1: a Host (danger_full_access) session must NOT
        // carry a SessionScope, or the file tools would prefer it over the
        // Host filesystem_scope and silently re-fence the operator-granted
        // host access. This session is `:`-keyed AND hinted — exactly the
        // shapes #1377 newly scopes — so the assertion proves the Host gate
        // wins over scope attachment.
        assert!(
            rt.agent.session_scope().is_none(),
            "Host (danger_full_access) sessions must not carry a SessionScope",
        );
        assert!(!rt.sandbox.enabled);
        assert_eq!(rt.sandbox.mode, SandboxMode::None);
        assert!(rt.sandbox.allow_network);

        let shell = rt
            .tools
            .execute(
                "shell",
                &serde_json::json!({ "command": "printf danger-ok # rm -rf /" }),
            )
            .await
            .expect("shell result");
        assert!(shell.success, "shell failed: {}", shell.output);
        assert!(shell.output.contains("danger-ok"));

        let write = rt
            .tools
            .execute(
                "write_file",
                &serde_json::json!({
                    "path": outside.to_string_lossy(),
                    "content": "host\n"
                }),
            )
            .await
            .expect("write_file result");
        assert!(write.success, "write_file failed: {}", write.output);
        assert_eq!(std::fs::read_to_string(outside).unwrap(), "host\n");
    }

    // ---- Per-project session storage (appui.sessions_in_cwd) --------------

    #[tokio::test]
    async fn resolve_sessions_root_relocates_only_with_flag_and_hint() {
        // The one seam that gates per-cwd storage. Only (flag ON + a hint)
        // relocates; every other combination stays on `profile.data_dir` so
        // gateway/web-chat and a flag-off server are inert by construction.
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("profile-data");
        let profile = make_profile(data_dir.clone()).await;
        let cwd = tmp.path().join("proj");

        // flag ON + hint -> per-project, per-PROFILE store (the profile
        // segment isolates two profiles that share a project cwd).
        assert_eq!(
            resolve_sessions_root(&profile, &cwd, true, true),
            cwd.join(".octos").join(&profile.profile_id),
        );
        assert_eq!(
            resolve_sessions_root(&profile, &cwd, true, true),
            project_sessions_root(&cwd, &profile.profile_id),
        );
        // flag OFF + hint -> legacy (regression guard: flipping off is a no-op).
        assert_eq!(resolve_sessions_root(&profile, &cwd, true, false), data_dir,);
        // flag ON + NO hint (web-chat / gateway) -> legacy.
        assert_eq!(resolve_sessions_root(&profile, &cwd, false, true), data_dir,);
        // flag OFF + NO hint -> legacy.
        assert_eq!(
            resolve_sessions_root(&profile, &cwd, false, false),
            data_dir,
        );
    }

    #[test]
    fn active_profile_marker_round_trips_through_scanner() {
        let tmp = TempDir::new().unwrap();
        // Write the marker, then read it back through the SAME path the launch
        // scanner uses — proving the write byte-matches the read, and that the
        // explicit marker drives `derive_sticky_profile`.
        write_active_profile_marker(tmp.path(), "glm");
        let folder = crate::runtime::launch::scan_folder_sessions(tmp.path(), &["glm".to_string()]);
        assert_eq!(folder.active_profile.as_deref(), Some("glm"));
        assert_eq!(
            crate::runtime::launch::derive_sticky_profile(&folder).as_deref(),
            Some("glm"),
        );

        // Last writer wins — reopening under a different brain updates the marker.
        write_active_profile_marker(tmp.path(), "deepseek");
        let folder2 =
            crate::runtime::launch::scan_folder_sessions(tmp.path(), &["deepseek".to_string()]);
        assert_eq!(folder2.active_profile.as_deref(), Some("deepseek"));
    }

    #[tokio::test]
    async fn bootstrap_relocates_store_to_cwd_when_flag_on() {
        // A cwd-hinted AppUi session with the flag on persists its transcript
        // under `<cwd>/.octos`, NOT under `profile.data_dir`. Sidecars that
        // derive their path from `sessions.data_dir()` follow to the same
        // root by construction (asserted via data_dir()).
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("profile-data");
        let profile = make_profile(data_dir.clone()).await;
        let cwd = tmp.path().join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let cwd_canon = std::fs::canonicalize(&cwd).unwrap();

        let key = SessionKey::new("api", "coding");
        let rt = SessionRuntime::bootstrap_in_cwd(&profile, key.clone(), Some(cwd.clone()), true)
            .await
            .expect("bootstrap in cwd");

        // sessions_root and the manager's data_dir both point at
        // <cwd>/.octos/<profile_id>.
        let expected_root = cwd_canon.join(".octos").join(&profile.profile_id);
        assert_eq!(rt.sessions_root, expected_root);
        {
            let mgr = rt.sessions.lock().await;
            assert_eq!(mgr.data_dir(), expected_root);
        }

        // Persist a message and confirm the JSONL lives under <cwd>/.octos and
        // NOT under the profile data dir.
        let session_path = {
            let mut mgr = rt.sessions.lock().await;
            mgr.add_message(&key, Message::user("hello from the project"))
                .await
                .unwrap();
            mgr.session_path(&key)
        };
        assert!(
            session_path.starts_with(cwd_canon.join(".octos")),
            "transcript must live under <cwd>/.octos: {}",
            session_path.display()
        );
        assert!(
            !session_path.starts_with(&data_dir),
            "transcript must NOT live under profile.data_dir: {}",
            session_path.display()
        );
        assert!(
            session_path.exists(),
            "transcript file should exist on disk"
        );
        // The profile data dir has no `users/` session tree for this key.
        assert!(
            !data_dir.join("users").exists()
                || std::fs::read_dir(data_dir.join("users"))
                    .map(|mut d| d.next().is_none())
                    .unwrap_or(true),
            "profile data dir must not accrue this session's transcript"
        );
    }

    #[tokio::test]
    async fn bootstrap_keeps_store_in_profile_when_flag_off() {
        // Regression: with the flag OFF a cwd-hinted session is byte-identical
        // to today — the store stays under profile.data_dir.
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("profile-data");
        let profile = make_profile(data_dir.clone()).await;
        let cwd = tmp.path().join("project");
        std::fs::create_dir_all(&cwd).unwrap();

        let key = SessionKey::new("api", "coding");
        let rt = SessionRuntime::bootstrap_in_cwd(&profile, key.clone(), Some(cwd.clone()), false)
            .await
            .expect("bootstrap flag off");

        assert_eq!(rt.sessions_root, data_dir);
        let session_path = {
            let mut mgr = rt.sessions.lock().await;
            mgr.add_message(&key, Message::user("legacy"))
                .await
                .unwrap();
            mgr.session_path(&key)
        };
        assert!(
            session_path.starts_with(&data_dir),
            "flag OFF must keep the store under profile.data_dir: {}",
            session_path.display()
        );
        // Nothing was created under <cwd>/.octos.
        assert!(
            !cwd.join(".octos").exists(),
            "flag OFF must not create a per-project store"
        );
    }

    #[tokio::test]
    async fn no_hint_session_stays_in_profile_even_with_flag_on() {
        // The gateway/web-chat guarantee: a NO-hint session ignores the flag
        // (its "workspace" is the conventional data-dir path, not a project),
        // so its store stays on profile.data_dir. This is what keeps per-cwd
        // storage inert for the gateway path by construction.
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("profile-data");
        let profile = make_profile(data_dir.clone()).await;

        let key = SessionKey::new("api", "web-chat");
        let rt = SessionRuntime::bootstrap_in_cwd(&profile, key, None, true)
            .await
            .expect("bootstrap no hint, flag on");

        assert_eq!(
            rt.sessions_root, data_dir,
            "a no-hint session must stay on profile.data_dir even with the flag on"
        );
    }

    #[tokio::test]
    async fn two_cwds_same_key_do_not_collide() {
        // The sharpest risk: the SAME logical session key opened against two
        // different projects must not conflate transcripts. Per-cwd roots are
        // separate file trees, so two runtimes with the same key + distinct
        // hints persist to distinct files with no cross-contamination.
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("profile-data");
        let profile = make_profile(data_dir.clone()).await;

        let proj_a = tmp.path().join("project-a");
        let proj_b = tmp.path().join("project-b");
        std::fs::create_dir_all(&proj_a).unwrap();
        std::fs::create_dir_all(&proj_b).unwrap();

        // Same key, two projects.
        let key = SessionKey::new("api", "default");
        let rt_a =
            SessionRuntime::bootstrap_in_cwd(&profile, key.clone(), Some(proj_a.clone()), true)
                .await
                .expect("bootstrap A");
        let rt_b =
            SessionRuntime::bootstrap_in_cwd(&profile, key.clone(), Some(proj_b.clone()), true)
                .await
                .expect("bootstrap B");

        // Distinct roots — the isolation boundary — each the project's own
        // profile-namespaced `<cwd>/.octos/<profile_id>`.
        assert_ne!(rt_a.sessions_root, rt_b.sessions_root);
        let proj_a_canon = std::fs::canonicalize(&proj_a).unwrap();
        let proj_b_canon = std::fs::canonicalize(&proj_b).unwrap();
        assert_eq!(
            rt_a.sessions_root,
            project_sessions_root(&proj_a_canon, &profile.profile_id)
        );
        assert_eq!(
            rt_b.sessions_root,
            project_sessions_root(&proj_b_canon, &profile.profile_id)
        );

        // Write a distinct message through each runtime's own manager.
        {
            let mut mgr = rt_a.sessions.lock().await;
            mgr.add_message(&key, Message::user("message-for-A"))
                .await
                .unwrap();
        }
        {
            let mut mgr = rt_b.sessions.lock().await;
            mgr.add_message(&key, Message::user("message-for-B"))
                .await
                .unwrap();
        }

        // Reload each project's store from scratch (fresh manager, no shared
        // in-memory cache) and confirm each holds ONLY its own message.
        let mut reload_a = SessionManager::open(&rt_a.sessions_root).unwrap();
        let a = reload_a.get_or_create(&key).await;
        assert_eq!(
            a.messages.len(),
            1,
            "project A must have exactly its message"
        );
        assert_eq!(a.messages[0].content, "message-for-A");

        let mut reload_b = SessionManager::open(&rt_b.sessions_root).unwrap();
        let b = reload_b.get_or_create(&key).await;
        assert_eq!(
            b.messages.len(),
            1,
            "project B must have exactly its message"
        );
        assert_eq!(b.messages[0].content, "message-for-B");
    }

    #[tokio::test]
    async fn cwd_store_writes_gitignore_idempotently() {
        // A freshly-created per-project store gets a `.gitignore` so chat logs
        // never leak into the user's repo; a pre-existing one is never
        // clobbered.
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("profile-data");
        let profile = make_profile(data_dir).await;
        let cwd = tmp.path().join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let cwd_canon = std::fs::canonicalize(&cwd).unwrap();

        let rt = SessionRuntime::bootstrap_in_cwd(
            &profile,
            SessionKey::new("api", "gi"),
            Some(cwd.clone()),
            true,
        )
        .await
        .expect("bootstrap");

        let gitignore = rt.sessions_root.join(".gitignore");
        assert_eq!(
            rt.sessions_root,
            cwd_canon.join(".octos").join(&profile.profile_id)
        );
        assert!(
            gitignore.exists(),
            "per-project store must have a .gitignore"
        );
        let body = std::fs::read_to_string(&gitignore).unwrap();
        assert!(body.contains("sessions/"));
        assert!(body.contains("users/"));
        // Context-manager snapshots hold verbatim conversation content and
        // are rooted at this store alongside the transcript (#1666).
        assert!(body.contains("context_ledgers/"));

        // Idempotent + non-clobbering: hand-edit it, re-run the helper, and
        // confirm the operator's content survives.
        std::fs::write(&gitignore, "custom operator content\n").unwrap();
        ensure_session_store_gitignore(&rt.sessions_root);
        assert_eq!(
            std::fs::read_to_string(&gitignore).unwrap(),
            "custom operator content\n",
            "existing .gitignore must never be clobbered"
        );
    }

    #[tokio::test]
    async fn fork_and_rollback_resolve_through_cwd_root() {
        // hydrate/rollback/fork all operate on the session runtime's own
        // `SessionManager` (resolved via `resolve_sessions_for_lookup` in the
        // dispatcher), so once the manager is rooted at `<cwd>/.octos` they
        // follow automatically. Prove it for fork + rollback: a child forked
        // from a cwd-scoped session lands under the SAME `<cwd>/.octos` root,
        // and a rollback rewrites there too.
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("profile-data");
        let profile = make_profile(data_dir.clone()).await;
        let cwd = tmp.path().join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let cwd_canon = std::fs::canonicalize(&cwd).unwrap();

        let key = SessionKey::new("api", "parent");
        let rt = SessionRuntime::bootstrap_in_cwd(&profile, key.clone(), Some(cwd), true)
            .await
            .expect("bootstrap");

        let child_path = {
            let mut mgr = rt.sessions.lock().await;
            // User rows only: the new write path requires a caller-supplied
            // thread_id for Assistant/Tool rows, and this test only cares
            // about the on-disk ROOT, not the transcript shape.
            mgr.add_message(&key, Message::user("q1")).await.unwrap();
            mgr.add_message(&key, Message::user("q2")).await.unwrap();
            let child = mgr.fork(&key, "child-1", 2).await.expect("fork");
            mgr.session_path(&child)
        };
        assert!(
            child_path.starts_with(cwd_canon.join(".octos")),
            "forked child must live under <cwd>/.octos: {}",
            child_path.display()
        );
        assert!(
            !child_path.starts_with(&data_dir),
            "forked child must NOT live under profile.data_dir"
        );

        // Rollback rewrites in-place under the same root.
        {
            let mut mgr = rt.sessions.lock().await;
            mgr.rollback_last_n_user_turns(&key, 1)
                .await
                .expect("rollback");
            let parent_path = mgr.session_path(&key);
            assert!(
                parent_path.starts_with(cwd_canon.join(".octos")),
                "rollback must rewrite under <cwd>/.octos: {}",
                parent_path.display()
            );
        }
    }
}
