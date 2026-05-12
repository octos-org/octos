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
    Agent, AgentConfig, AgentSummaryGenerator, FileStateCache, SandboxConfig, SubAgentOutputRouter,
    ToolRegistry,
};
use octos_bus::SessionManager;
use octos_core::{AgentId, SessionKey};

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
        let sandbox = profile.default_sandbox.clone();
        let mut tools = profile
            .tool_specs
            .rebind_cwd(&workspace_root, create_sandbox(&sandbox));
        tools.set_output_dir_hint(plugin_work_dir.to_string_lossy().into_owned());
        tools.rebind_plugin_work_dirs(&plugin_work_dir);
        // M11-F regression fix REG-1 follow-up round 2 (codex review):
        // re-register a fresh `ActivateToolsTool` instance on this
        // session's registry. The profile-level template is shared via
        // `Arc<dyn Tool>` clones across every session that snapshots
        // from `profile.tool_specs`; if we let the same instance
        // straddle sessions, the most recently bootstrapped session's
        // `wire_activate_tools()` would rebind the shared tool's
        // `Weak<ToolRegistry>` away from earlier sessions and break
        // their `activate_tools` calls. Minting a fresh tool per
        // session keeps the wiring per-registry.
        if tools.get("activate_tools").is_some() {
            tools.register(octos_agent::ActivateToolsTool::new());
        }
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

        let mut agent = Agent::new_shared(
            AgentId::new("api"),
            profile.llm.clone(),
            Arc::clone(&tools),
            profile.memory.clone(),
        )
        .with_config(AgentConfig {
            max_iterations: 20,
            save_episodes: true,
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
        .with_system_prompt(profile.system_prompt.clone())
        .with_file_state_cache(file_state_cache)
        .with_subagent_output_router(subagent_output_router)
        .with_subagent_summary_generator(subagent_summary_generator)
        .with_sandbox_config(sandbox.clone())
        .with_workspace_root(workspace_root.clone());

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

        // M11-F regression fix REG-1 follow-up (codex review): when
        // `ProfileRuntime::bootstrap` deferred non-core tool groups and
        // registered `activate_tools`, the agent must call
        // `wire_activate_tools()` so the tool's `Weak<ToolRegistry>`
        // back-reference is planted. Without this, `activate_tools`
        // remains a no-op stub (its `set_registry` is never invoked)
        // and the LLM cannot pull a deferred group back on demand.
        // Gateway does the equivalent at `session_actor.rs:2500`.
        agent.wire_activate_tools();

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

/// Resolve a per-session workspace root.
///
/// Honors a caller-supplied `workspace_hint` (coding-agent flow) when
/// the path passes basic safety validation; otherwise derives the
/// canonical `<data_dir>/users/<encoded session base>/workspace`
/// path. Mirrors the encoding produced by
/// `api/handlers.rs::api_session_workspace_dirs` so an existing
/// session can transparently pick up the new code path without
/// losing its workspace.
fn resolve_workspace_root(
    profile: &ProfileRuntime,
    session_key: &SessionKey,
    workspace_hint: Option<PathBuf>,
) -> Result<PathBuf> {
    if let Some(hint) = workspace_hint {
        return validate_workspace_hint(&hint).map(|_| hint);
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
fn validate_workspace_hint(hint: &Path) -> Result<()> {
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

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::SystemTime;

    use octos_agent::sandbox::create_sandbox;
    use octos_agent::workspace_contract::{SpawnTaskContractResult, enforce_spawn_task_contract};
    use octos_agent::workspace_policy::{
        WORKSPACE_POLICY_FILE, WorkspacePolicy, read_workspace_policy,
    };
    use octos_agent::{SandboxConfig, ToolRegistry};
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
            tool_specs: Arc::new(base_tools),
            plugin_tool_names: Vec::new(),
            plugin_dirs: Vec::new(),
            plugin_prompt_fragments: Vec::new(),
            plugin_hooks: Vec::new(),
            system_prompt,
            memory,
            memory_store,
            tool_config,
            cron_service: None,
            hook_executor: None,
        })
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
            tool_specs: Arc::new(base_tools),
            plugin_tool_names: Vec::new(),
            plugin_dirs: Vec::new(),
            plugin_prompt_fragments: Vec::new(),
            plugin_hooks: Vec::new(),
            system_prompt: "test-system-prompt".to_string(),
            memory,
            memory_store,
            tool_config,
            cron_service: None,
            hook_executor: Some(executor),
        })
    }

    /// M11-F regression fix REG-1 follow-up (codex review):
    /// `SessionRuntime::bootstrap` must call `wire_activate_tools()`
    /// on the per-session agent when `ProfileRuntime::bootstrap`
    /// registered `activate_tools` (deferred-group scenario). Without
    /// the wiring, `activate_tools.execute()` returns
    /// `"tool registry not available"` and the LLM cannot pull
    /// deferred groups back on demand.
    #[tokio::test]
    async fn session_runtime_agent_wires_activate_tools() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("profile-data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let memory = Arc::new(EpisodeStore::open(&data_dir).await.unwrap());
        let memory_store = Arc::new(MemoryStore::open(&data_dir).await.unwrap());
        let tool_config = Arc::new(octos_agent::ToolConfigStore::open(&data_dir).await.unwrap());
        let sandbox = SandboxConfig::default();
        // Build a registry with activate_tools + a deferred entry so
        // execute() has something to list.
        let mut base_tools =
            ToolRegistry::with_builtins_and_sandbox(&data_dir, create_sandbox(&sandbox));
        base_tools.defer_group("group:web");
        base_tools.register(octos_agent::ActivateToolsTool::new());
        let profile = Arc::new(ProfileRuntime {
            profile_id: "_main".to_string(),
            data_dir: data_dir.clone(),
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
            tool_specs: Arc::new(base_tools),
            plugin_tool_names: Vec::new(),
            plugin_dirs: Vec::new(),
            plugin_prompt_fragments: Vec::new(),
            plugin_hooks: Vec::new(),
            system_prompt: "test-system-prompt".to_string(),
            memory,
            memory_store,
            tool_config,
            cron_service: None,
            hook_executor: None,
        });
        let key = SessionKey::new("api", "activate-tools-probe");
        let rt = SessionRuntime::bootstrap(&profile, key, None)
            .await
            .expect("bootstrap");

        let registry = rt.agent.tool_registry();
        let tool = registry
            .get("activate_tools")
            .expect("activate_tools must be registered");
        // Executing with empty args lists deferred groups. The path
        // unwraps `registry.upgrade()`; if `wire_activate_tools` did
        // not fire, the unwrap maps to an `Err("tool registry not
        // available")` and the assertion below fails.
        let result = tool
            .execute(&serde_json::json!({}))
            .await
            .expect("activate_tools must be wired so its registry Weak upgrades");
        assert!(
            !result.output.contains("tool registry not available"),
            "activate_tools.execute should not surface 'tool registry not available'; \
             got: {}",
            result.output,
        );
    }

    /// M11-F regression fix REG-1 follow-up round 2 (codex review):
    /// `ActivateToolsTool` is stored on the registry as `Arc<dyn Tool>`,
    /// and `ToolRegistry::rebind_cwd` clones those Arcs verbatim into
    /// the new per-session registry. If we DON'T re-register a fresh
    /// `ActivateToolsTool` per session, both sessions end up sharing
    /// the SAME tool instance — and the second session's
    /// `wire_activate_tools()` rewires the shared `Weak<ToolRegistry>`
    /// off session A's registry onto session B's, breaking session A's
    /// `activate_tools` calls.
    ///
    /// This test bootstraps two sessions from the same profile (both
    /// of which carry `activate_tools` on the base template) and
    /// asserts that session A's activate_tools still resolves to
    /// session A's registry after session B has been bootstrapped.
    #[tokio::test]
    async fn session_runtime_isolates_activate_tools_across_sessions() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("profile-data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let memory = Arc::new(EpisodeStore::open(&data_dir).await.unwrap());
        let memory_store = Arc::new(MemoryStore::open(&data_dir).await.unwrap());
        let tool_config = Arc::new(octos_agent::ToolConfigStore::open(&data_dir).await.unwrap());
        let sandbox = SandboxConfig::default();
        let mut base_tools =
            ToolRegistry::with_builtins_and_sandbox(&data_dir, create_sandbox(&sandbox));
        base_tools.defer_group("group:web");
        base_tools.register(octos_agent::ActivateToolsTool::new());
        let profile = Arc::new(ProfileRuntime {
            profile_id: "_main".to_string(),
            data_dir: data_dir.clone(),
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
            tool_specs: Arc::new(base_tools),
            plugin_tool_names: Vec::new(),
            plugin_dirs: Vec::new(),
            plugin_prompt_fragments: Vec::new(),
            plugin_hooks: Vec::new(),
            system_prompt: "test-system-prompt".to_string(),
            memory,
            memory_store,
            tool_config,
            cron_service: None,
            hook_executor: None,
        });

        let rt_a = SessionRuntime::bootstrap(&profile, SessionKey::new("api", "iso-a"), None)
            .await
            .expect("bootstrap A");
        let rt_b = SessionRuntime::bootstrap(&profile, SessionKey::new("api", "iso-b"), None)
            .await
            .expect("bootstrap B");

        // Both sessions must have a usable `activate_tools`. The
        // pre-fix regression: session A's tool's Weak would have been
        // rewired by session B's bootstrap and now upgrades to
        // session B's registry, mixing per-session state.
        let tool_a = rt_a
            .agent
            .tool_registry()
            .get("activate_tools")
            .expect("session A activate_tools");
        let tool_b = rt_b
            .agent
            .tool_registry()
            .get("activate_tools")
            .expect("session B activate_tools");

        // The fresh-registration step in `SessionRuntime::bootstrap`
        // means the two sessions must hold DISTINCT `Arc<dyn Tool>`
        // instances; if they did not, the second bootstrap would have
        // rewired the shared Weak away from the first.
        assert!(
            !Arc::ptr_eq(tool_a, tool_b),
            "activate_tools must be a fresh instance per session, not a \
             shared Arc cloned from the profile template",
        );

        // Both tools must execute successfully (i.e. their Weak
        // upgrades to a live registry — not "tool registry not
        // available").
        let result_a = tool_a
            .execute(&serde_json::json!({}))
            .await
            .expect("activate_tools A wired");
        assert!(
            !result_a.output.contains("tool registry not available"),
            "session A activate_tools must remain wired after session B bootstrap; got: {}",
            result_a.output,
        );
        let result_b = tool_b
            .execute(&serde_json::json!({}))
            .await
            .expect("activate_tools B wired");
        assert!(
            !result_b.output.contains("tool registry not available"),
            "session B activate_tools must also be wired; got: {}",
            result_b.output,
        );
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
}
