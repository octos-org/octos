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

    /// The per-session chat history manager. Wrapped in a
    /// [`tokio::sync::Mutex`] because multiple subscribers
    /// (SSE + WS) may observe and persist messages concurrently.
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
    /// 7. Open the [`SessionManager`] via
    ///    `SessionManager::open(&profile.data_dir)` — the canonical
    ///    JSONL session store namespaces on-disk files by
    ///    [`SessionKey`] under `data_dir/sessions/`, so the
    ///    profile data dir is the correct root.
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
    ) -> Result<Arc<Self>> {
        // Step 1: resolve workspace_root.
        let workspace_root = resolve_workspace_root(profile, &session_key, workspace_hint)?;
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
        bootstrap_session_policy(&workspace_root)?;

        // Step 3: plugin work dir.
        let plugin_work_dir = workspace_root.join("skill-output");
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
        tools.rebind_plugin_work_dirs(&plugin_work_dir);
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
        // Per-session policy filter is a no-op for M11; future work
        // may add session-level policy overrides on top of
        // `profile.tool_policy`. The profile-level policy itself is
        // applied at registry-build time by `ProfileRuntime::bootstrap`
        // (M11-B), so the rebound registry already inherits it.

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
        .with_config(AgentConfig {
            // Honor the configured `max_iterations` instead of a hardcoded cap.
            // The previous fixed `20` ignored config AND propagated to spawned
            // sub-agents (which inherit this config), starving multi-step
            // background tasks that need more iterations.
            max_iterations: resolve_session_max_iterations(profile.max_iterations),
            save_episodes: true,
            // Phase 4 (docs/ROBRIX-PHASE4-APPROVAL-FLOW-ADR.md)
            human_approval_rules: profile.human_approval_rules.clone(),
            ..Default::default()
        })
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
        .with_workspace_root(workspace_root.clone());

        // Phase 1 of the SessionScope migration: attach the constructed
        // scope to the per-session agent. `None` keeps pre-Phase-1
        // behaviour byte-for-byte (no consumer reads the field yet).
        if let Some(scope) = session_scope {
            agent = agent.with_session_scope(scope);
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

        // Step 6: open the per-profile SessionManager. The on-disk
        // layout (`<data_dir>/sessions/`) already namespaces by
        // SessionKey via `encode_path_component`, so the profile
        // data_dir is the correct root. Sharing one SessionManager
        // per profile (vs per session) matches today's serve +
        // gateway call sites.
        let sessions = Arc::new(tokio::sync::Mutex::new(
            SessionManager::open(&profile.data_dir).wrap_err("failed to open session manager")?,
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
            sessions,
        }))
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
    write_workspace_policy_if_absent(workspace_root, &WorkspacePolicy::for_session())
        .wrap_err("failed to bootstrap session workspace policy")
}

/// Resolve the per-session agent iteration budget from the profile's
/// configured `gateway.max_iterations`, falling back to the `AgentConfig`
/// default when unset. Spawned sub-agents inherit the resulting config, so
/// this is also the cap for background workers — `None` must not collapse to a
/// small hardcoded value the way the previous fixed `20` did.
fn resolve_session_max_iterations(configured: Option<u32>) -> u32 {
    configured.unwrap_or_else(|| AgentConfig::default().max_iterations)
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

    #[test]
    fn resolve_session_max_iterations_honors_config_else_default() {
        // A configured gateway.max_iterations must be respected (the bug was a
        // hardcoded 20 that ignored it and starved spawned sub-agents).
        assert_eq!(resolve_session_max_iterations(Some(120)), 120);
        assert_eq!(resolve_session_max_iterations(Some(5)), 5);
        // Unset falls back to the AgentConfig default (50), not the old 20.
        assert_eq!(
            resolve_session_max_iterations(None),
            AgentConfig::default().max_iterations
        );
        assert_ne!(
            resolve_session_max_iterations(None),
            20,
            "unset must not collapse to the old hardcoded cap"
        );
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
            llm: Arc::new(StubLlm),
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
        )
        .await
        .expect("gamma bootstrap");
        let delta = SessionRuntime::bootstrap_with_permissions_and_sandbox(
            &profile,
            SessionKey::new("api", "delta"),
            Some(tmp.path().join("delta")),
            EffectivePermissions::workspace_write(),
            None,
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
            llm: Arc::new(StubLlm),
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
}
