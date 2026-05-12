# M11 Runtime Unification — Workstreams

Status: PROPOSED — accepts after `M11-PROFILE-SESSION-RUNTIME-ADR.md`
is APPROVED.

ADR: `docs/M11-PROFILE-SESSION-RUNTIME-ADR.md`

## Goal

Replace the server-wide embedded `state.agent: Option<Arc<Agent>>` in
`octos serve` with two first-class types — `ProfileRuntime` (per
profile) and `SessionRuntime` (per `(profile_id, session_key)`) —
unifying the path serve and gateway take to construct an agent.
Surface coding-agent's N-isolated-sessions-per-profile model. Delete
the four transient `Config::profile_*` fields introduced by PR #866
and PR #868.

## Non-Goals

- **Not** rewriting profile JSON on disk. The schema stays the same.
- **Not** changing any wire protocol (`/api/chat`, UI Protocol v1 WS,
  bus message shape).
- **Not** adding per-session credentials. Credentials are
  profile-scope; sessions inherit.
- **Not** moving sessions into subprocesses. Sandbox isolation
  continues to live at the shell-tool spawn boundary.
- **Not** changing gateway's external behavior. Gateway's startup is
  refactored to call the new `ProfileRuntime::bootstrap`, but the
  observable behavior (channels it serves, env it injects into
  skills, sessions it manages) is byte-identical pre/post.

## Current State (2026-05-10)

`commands/serve.rs::try_create_agent` constructs ONE `Agent` from
`Config` overlaid by `overlay_profile_llm`. It writes per-profile
state onto four transient fields on `Config`:

- `Config::credentials: HashMap<String, String>` (PR #866)
- `Config::profile_skills_dir: Option<PathBuf>` (PR #868)
- `Config::profile_plugin_env: Vec<(String, String)>` (PR #868)
- `Config::content_routing` (already on Config, no extra transient)

`AppState::agent` is `Some(this single Agent)`. Every `/api/chat` and
UI Protocol WS session shares it. `workspace_root` is set once at
startup to the daemon cwd. `plugin_work_dir` is set once to the
top-level data dir's `skill-output`.

`commands/gateway/gateway_runtime.rs` does the same construction
inline (~500 LOC of setup before its bus loop starts), per-profile,
inside each subprocess `ProcessManager` spawns. It correctly sets
`workspace_root` per-session under `<data_dir>/users/<key>/workspace/`
and writes `.octos-workspace.toml` there.

The mismatch caused the yangmi-voice incident chain on 2026-05-10
(four PRs landed; a fifth gap — workspace policy — surfaced after the
fourth and still requires a hotfix on mini1).

## Workstreams

### M11-A: Runtime types skeleton

Repository: `octos`

Owns:

- The type signatures of `ProfileRuntime`, `SessionRuntime`, and
  `SessionRuntimeCache`. No behavior yet — types compile, doc
  comments fully specify the contract.
- A new module `crates/octos-cli/src/runtime/`.

Allowed areas:

- `crates/octos-cli/src/runtime/mod.rs` (new)
- `crates/octos-cli/src/runtime/profile.rs` (new)
- `crates/octos-cli/src/runtime/session.rs` (new)
- `crates/octos-cli/src/runtime/cache.rs` (new)
- `crates/octos-cli/src/lib.rs` (add `mod runtime;`)

Deliverables:

- `pub struct ProfileRuntime` with fields:
  - `profile_id: String`
  - `data_dir: PathBuf` — `~/.octos/profiles/<id>/data`
  - `llm: Arc<dyn LlmProvider>`
  - `adaptive_router: Option<Arc<AdaptiveRouter>>` — from `qos_catalog::AdaptiveProviderBundle`
  - `credentials: HashMap<String, String>`
  - `skills_dir: Option<PathBuf>`
  - `plugin_env_template: Vec<(String, String)>` — `OCTOS_PROFILE_ID`, `OCTOS_VOICE_DIR`, etc.
  - `tool_policy: Option<ToolPolicy>`
  - `default_sandbox: SandboxConfig`
  - `tool_specs: Arc<ToolRegistry>` — base registry, NO workspace bound
  - `memory: Arc<EpisodeStore>`
  - `memory_store: Arc<MemoryStore>`
- `impl ProfileRuntime` signature only:
  - `pub async fn bootstrap(profile: &UserProfile, store: &ProfileStore, data_dir: &Path) -> Result<Arc<Self>>` — body: `todo!()`
- `pub struct SessionRuntime` with fields:
  - `session_key: SessionKey`
  - `profile: Arc<ProfileRuntime>`
  - `workspace_root: PathBuf`
  - `plugin_work_dir: PathBuf`
  - `sandbox: SandboxConfig` — may override profile default
  - `tools: Arc<ToolRegistry>` — cloned from profile, workspace-bound, policy-filtered
  - `agent: Arc<Agent>`
  - `sessions: Arc<tokio::sync::Mutex<SessionManager>>`
- `impl SessionRuntime` signature only:
  - `pub async fn bootstrap(profile: &Arc<ProfileRuntime>, session_key: SessionKey, workspace_hint: Option<PathBuf>) -> Result<Arc<Self>>` — body: `todo!()`
- `pub struct SessionRuntimeCache` — wraps `tokio::sync::RwLock<HashMap<(String, SessionKey), Arc<SessionRuntime>>>`. TTL/LRU policy parameters.
  - `pub fn new(max_size: usize, idle_ttl: Duration) -> Self`
  - `pub async fn get_or_init(&self, profile: &Arc<ProfileRuntime>, session_key: SessionKey, workspace_hint: Option<PathBuf>) -> Result<Arc<SessionRuntime>>` — body: `todo!()`
  - `pub async fn invalidate(&self, key: &(String, SessionKey))`
- Module doc comments (`//!`) at the top of `runtime/mod.rs` explain
  the two-scope model and reference the ADR.

Acceptance:

- `cargo check -p octos-cli --features api` clean with `todo!()` bodies.
- `cargo doc -p octos-cli --no-deps` renders the module docs.
- A unit test asserts the type signatures compile and the cache key
  format is `(String, SessionKey)`.

Blocks: M11-B, M11-C, M11-D.

### M11-B: Extract gateway's profile bootstrap into ProfileRuntime

Repository: `octos`

Owns:

- Moving the per-profile bootstrap body from
  `gateway_runtime.rs::run` (~lines 286–520 today) into
  `ProfileRuntime::bootstrap`.
- Keeping gateway behavior byte-identical pre/post — gateway's
  observable lifecycle (channels it serves, env injected to skills,
  bus startup) does not change.

Depends on: M11-A.

Allowed areas:

- `crates/octos-cli/src/runtime/profile.rs`
- `crates/octos-cli/src/commands/gateway/gateway_runtime.rs`
- `crates/octos-cli/src/skills_scope.rs` (already exports the helpers
  we need; small re-export tweaks only)
- tests under these crates

Deliverables:

- `ProfileRuntime::bootstrap` implementation that:
  1. Calls `crate::profiles::config_from_profile(profile, None, None)` to derive a Config (preserves the existing per-profile-LLM contract).
  2. Wraps the primary LLM via `qos_catalog::build_adaptive_provider_chain(..., ExporterMode::Spawn)` — the helper PR #867 introduced — and stores `(llm, adaptive_router)` on the struct.
  3. Resolves `credentials` from `profile.config.env_vars` via `keychain::resolve_env_vars`.
  4. Resolves `skills_dir = data_dir.join("skills")` (PR #868's logic).
  5. Builds `plugin_env_template` via `skills_scope::push_runtime_plugin_env` (PR #868's helper).
  6. Constructs the base `ToolRegistry` via `with_builtins_and_sandbox` + tool_config + the registration sequence gateway currently does (browser, web_search, MCP, etc.).
  7. Loads plugins via `PluginLoader::load_into_with_options` with the per-profile env + work dir.
  8. Pins plugin tool names as base tools (today's LRU defense from PR #764).
  9. Opens `EpisodeStore` and `MemoryStore` against `data_dir`.
  10. Returns `Arc<Self>`.
- `gateway_runtime.rs::run` refactored to call `ProfileRuntime::bootstrap` once, then use its fields for the bus loop, session actors, etc. The bus / channel / cron / heartbeat setup downstream of the agent stays where it is.
- New module `crates/octos-cli/src/runtime/profile.rs` exports the implementation.

Acceptance:

- `cargo test -p octos-cli --lib commands::gateway` passes unchanged.
- Manual diff: before-PR and after-PR `gateway run` produces the same
  startup log lines (provider list, adaptive routing, skills loaded,
  plugin env keys, base-tool pin count).
- A unit test in `runtime/profile.rs` builds a `ProfileRuntime` from
  a synthetic profile + temp data_dir + stub LLM and asserts
  `tool_specs` contains a known builtin (e.g. `read_file`) and
  `credentials` is populated from `env_vars`.
- Gateway live boot on a dev mini works end-to-end (Telegram echo,
  fm_voice_list, fm_tts) — soak test under
  `scripts/test-local-tenant-deploy.sh` or equivalent.

Blocks: M11-D.

### M11-C: SessionRuntime + per-session workspace bootstrap

Repository: `octos`

Owns:

- The implementation of `SessionRuntime::bootstrap` — the per-session
  agent constructor that fixes the yangmi/workspace gap.
- Per-session workspace dir + `.octos-workspace.toml` creation step.

Depends on: M11-A.

Allowed areas:

- `crates/octos-cli/src/runtime/session.rs`
- `crates/octos-cli/src/runtime/cache.rs`
- `crates/octos-agent/src/workspace_policy.rs` (helper to write a
  default policy; existing `write_workspace_policy` is sufficient —
  no semantic change)
- tests under the above

Deliverables:

- `SessionRuntime::bootstrap(profile, session_key, workspace_hint) -> Result<Arc<Self>>` implementation:
  1. Resolve `workspace_root`:
     - If `workspace_hint` is `Some(path)` and
       `validate_session_workspace_allowed(state, path)` accepts, use
       it (coding-agent path).
     - Else, derive `profile.data_dir.join("users").join(encode(session_key)).join("workspace")` and `create_dir_all`.
  2. If `workspace_root/.octos-workspace.toml` does not exist, write
     `WorkspacePolicy::for_session()` to it via `write_workspace_policy`.
  3. Compute `plugin_work_dir = workspace_root.join("skill-output")` and `create_dir_all`.
  4. Clone `profile.tool_specs` via `ToolRegistry::snapshot_excluding(&[])` and apply:
     - `set_workspace_root(workspace_root.clone())`
     - `set_output_dir_hint(plugin_work_dir.to_string_lossy().into_owned())`
     - Per-session policy filter (no-op default).
  5. Resolve `sandbox` — profile default unless an explicit override
     is provided in the future; for M11 the default is fine.
  6. Construct the `Agent` from `profile.llm` + the cloned tools.
     Today's `Agent` builder pattern — `Agent::new(...)` plus the
     `.with_*` chain — moves verbatim from `try_create_agent` into
     this function.
  7. Construct the per-session `SessionManager` against
     `profile.data_dir.join("users").join(session_key.user_key())`.
  8. Return `Arc<Self>`.
- `SessionRuntimeCache::get_or_init`:
  - Holds an `RwLock<HashMap<(String, SessionKey), Arc<SessionRuntime>>>`.
  - On hit, return.
  - On miss, drop read lock, take write lock, double-check, call
    `SessionRuntime::bootstrap`, insert, return.
  - Eviction is best-effort: a background task scans every 60 s and
    drops entries idle > `idle_ttl`. M11 starts with `idle_ttl =
    30 min`, `max_size = 64`. Eviction is a perf optimization, never
    correctness — every entry is rebuildable.
- Unit tests:
  - Two sessions on the same profile with different `workspace_hint`
    paths produce `SessionRuntime`s with distinct `workspace_root`,
    distinct `plugin_work_dir`, and the same `profile` `Arc`.
  - A session bootstrap with no `workspace_hint` creates the
    auto-derived workspace dir and writes the default policy file.
  - A second `get_or_init` for the same key returns the same `Arc`
    (cache hit).

Acceptance:

- `cargo clippy --workspace --all-targets -- -D warnings` clean.
- All M11-C unit tests pass.
- A focused integration test simulates a yangmi flow (stub LLM, stub
  skill binary, real workspace bootstrap) and asserts
  `enforce_spawn_task_contract` reaches `Satisfied` without any
  hotfix policy file at the daemon cwd.

Blocks: M11-D.

### M11-D: AppState refactor — replace state.agent with profiles/sessions

Repository: `octos`

Owns:

- Replacing `AppState::agent: Option<Arc<Agent>>` with
  `state.profiles: HashMap<ProfileId, Arc<ProfileRuntime>>` and
  `state.sessions: Arc<SessionRuntimeCache>`.
- Wiring `/api/chat` (`api/handlers.rs::chat`) and the synchronous
  `chat_sync` path to resolve the right `SessionRuntime` per request.
- Updating `commands/serve.rs::run_async` to build the `profiles` map
  from `ProfileStore::list()` instead of calling `try_create_agent`.

Depends on: M11-A, M11-B, M11-C.

Allowed areas:

- `crates/octos-cli/src/api/mod.rs` (AppState struct)
- `crates/octos-cli/src/api/handlers.rs`
- `crates/octos-cli/src/commands/serve.rs`
- tests under these crates

Deliverables:

- `AppState`:
  - Replace `pub agent: Option<Arc<Agent>>` with
    `pub profiles: HashMap<String, Arc<ProfileRuntime>>`.
  - Add `pub sessions: Arc<SessionRuntimeCache>`.
  - Keep `pub sessions_manager: Option<Arc<Mutex<SessionManager>>>`
    if any handler still needs the global one for legacy paths; mark
    it deprecated and route new code through `SessionRuntime.sessions`.
- `commands/serve.rs::run_async`:
  - On startup, iterate `ProfileStore::list()` and call
    `ProfileRuntime::bootstrap` for every enabled profile with a
    fully populated `llm.primary`. Log per-profile bootstrap success.
  - If any profile fails bootstrap, log a warning and continue —
    other profiles still serve.
  - Wire the populated `state.profiles` + a fresh
    `SessionRuntimeCache::new(64, Duration::from_secs(1800))` into
    `AppState`.
- `api/handlers.rs::chat`:
  - Resolve `profile_id` from `validate_authenticated_session_scope`
    (already extracted from the session_id format).
  - `let profile = state.profiles.get(profile_id).cloned().ok_or(...)?;`
  - `let session = state.sessions.get_or_init(&profile, session_key, /* workspace_hint */ None).await?;`
  - Dispatch the user message to `session.agent`.
  - Wire response through the same persistence helpers
    (`persist_chat_message_through_canonical`) but against
    `session.sessions` instead of the global one.

Acceptance:

- `cargo test -p octos-cli --lib api::handlers` passes.
- `cargo clippy --workspace --all-targets -- -D warnings` clean.
- Live `curl -X POST /api/chat` against a dev serve completes a
  one-turn echo against the routed profile's LLM.
- yangmi voice-clone end-to-end on a dev mini WITHOUT the
  `/Users/cloud/.octos-workspace.toml` hotfix — the workspace policy
  is bootstrapped per-session by `SessionRuntime::bootstrap`.

Blocks: M11-E, M11-F.

### M11-E: UI Protocol per-session workspace wiring

Repository: `octos`

Owns:

- Threading the `session_workspaces` map already present in
  `ui_protocol.rs` (line 752 today) into `SessionRuntime::bootstrap`
  as the `workspace_hint`.
- Removing the legacy "global tool registry clone with per-session
  cwd rebind" path codex flagged in PR #868's multi-tenant scope note.

Depends on: M11-D.

Allowed areas:

- `crates/octos-cli/src/api/ui_protocol.rs`
- `crates/octos-cli/src/api/ui_protocol_*.rs` bridges
- tests under the same crates

Deliverables:

- `ui_protocol::handle_session_open` (or equivalent) honors the
  client-supplied `cwd` (via the `session.workspace_cwd.v1`
  capability), validates via the existing
  `validate_session_workspace_allowed`, passes it as `workspace_hint`
  into `SessionRuntimeCache::get_or_init`.
- The legacy `clone_session_tools` path that built a per-session
  tool registry from `base_agent.tool_registry()` is removed —
  `SessionRuntime.tools` IS the per-session view.
- `session_workspaces` in-memory map either goes away (preferred —
  `SessionRuntime` is the single store) or becomes a thin
  read-through to the cache.

Acceptance:

- An AppUI coding session opened with a custom `cwd` runs `read_file`
  and observes the supplied workspace, not the daemon cwd.
- Two AppUI sessions on the same profile with different `cwd`s do
  not see each other's files (regression: the multi-tenant scope
  note from PR #868 is now an enforced invariant).
- The existing M10-A unit test
  (`two AppUI sessions with different profiles use different
  effective runtime descriptors`) stays green.

Blocks: M11-G.

### M11-F: Delete legacy Config transients + overlay machinery

Repository: `octos`

Owns:

- Deleting `Config::credentials`, `Config::profile_skills_dir`,
  `Config::profile_plugin_env`.
- Deleting `commands/serve.rs::overlay_profile_llm`,
  `select_serve_profile`, `profile_has_active_primary_llm`,
  `populate_profile_credentials`, `inject_profile_api_key_env` (any
  remnants), and `try_create_agent`.
- Auditing every remaining reader of these fields and migrating to
  `ProfileRuntime` / `SessionRuntime` access.

Depends on: M11-D (the replacement must be live before deletion is
safe).

Allowed areas:

- `crates/octos-cli/src/config.rs`
- `crates/octos-cli/src/commands/serve.rs`
- `crates/octos-cli/src/profiles.rs` (only to update
  `config_from_profile` if it still touches the deleted fields)
- `crates/octos-cli/src/api/handlers.rs` (consumer migration)
- `crates/octos-cli/src/api/ui_protocol.rs` (consumer migration)
- tests under these crates

Deliverables:

- The three transient Config fields are removed; nothing reads them.
- Functions named above are removed; nothing calls them.
- Documentation in `docs/ARCHITECTURE.md` updated to reference the
  new module instead of `try_create_agent`.

Acceptance:

- `cargo build -p octos-cli --features "api,telegram,discord,whatsapp,feishu,twilio,wecom,wecom-bot"` clean.
- `cargo test -p octos-cli --lib` clean.
- `git grep -nE 'profile_skills_dir|profile_plugin_env|overlay_profile_llm|populate_profile_credentials|try_create_agent|Config::credentials\b'` returns 0 hits in non-doc files.

Blocks: M11-H.

### M11-G: Coding-agent multi-session e2e

Repository: `octos` + `octos-web` (if needed for client driver)

Owns:

- An end-to-end test that proves coding-agent's N-sessions-per-profile
  isolation invariant.

Depends on: M11-D, M11-E.

Allowed areas:

- `crates/octos-cli/tests/` (new integration test)
- `e2e/` (Playwright spec) — optional
- the M10 coding-runtime soak harness if reusable

Deliverables:

- An integration test in `crates/octos-cli/tests/coding_multi_session.rs`:
  1. Boot a `serve` instance with two AppUI sessions for the same
     profile.
  2. Session A: `workspace_hint = /tmp/repo-A` (pre-seeded with file `a.txt`).
  3. Session B: `workspace_hint = /tmp/repo-B` (pre-seeded with file `b.txt`).
  4. Run a `read_file("a.txt")` turn on session A; expect 200 + content.
  5. Run a `read_file("a.txt")` turn on session B; expect failure (file not visible from B's workspace).
  6. Run a `read_file("b.txt")` turn on session B; expect 200 + content.
  7. Assert session A and session B accumulate independent chat
     history JSONLs under their respective `user_key`s.
- The test must be `#[tokio::test]` and runnable via `cargo test -p octos-cli --test coding_multi_session`. No external API keys
  required — use the `Stub LLM` already used in `qos_catalog.rs`
  tests.

Acceptance:

- Test passes locally and on CI.
- Removing the workspace_root resolution from `SessionRuntime::bootstrap`
  (mutation experiment) makes the test fail.

Blocks: M11-H.

### M11-H: Fleet redeploy + soak gate

Repository: `octos` (deploy + verify; no code change)

Owns:

- Building the M11 binary, deploying to mini1/2/3, removing the
  hotfix `.octos-workspace.toml` at `/Users/cloud/.octos-workspace.toml`,
  and running a soak that exercises yangmi + a coding-agent-style
  multi-session flow.

Depends on: M11-F, M11-G.

Allowed areas:

- `scripts/` (if a new soak driver is needed)
- deploy automation only — no source changes

Deliverables:

- mini1, mini2, mini3 running an M11 binary, with the hotfix file
  deleted, and the per-session workspace policy created automatically
  on first turn for every new session.
- Smoke results captured for:
  - yangmi voice clone via web `/api/chat` and via Telegram on each
    mini.
  - Two AppUI coding sessions running concurrently against the
    `dspfac` profile, each in a different cwd, completing one
    `read_file` turn each.
- Marathon-30 stress test (mini1+2+3, 30 minutes of mixed chat +
  spawn_only traffic) — no `workspace policy not found` failures, no
  `no output files produced` failures, no cross-session content
  bleed.

Acceptance:

- Soak results posted to the M11 tracking issue (see below).
- Zero "workspace policy not found" entries in `serve.log` across
  the soak window.
- Audio attachments delivered for at least 5 yangmi turns per mini.

## Cross-Workstream Invariants

These hold across every workstream. Any PR that violates them must
be rebased before merge.

1. **No subprocess introduction.** Sessions stay in-process. Sandbox
   isolation continues at the shell-tool boundary.
2. **No wire-protocol change.** `/api/chat`, UI Protocol v1, bus
   message shape stay identical.
3. **`ProfileRuntime` and `SessionRuntime` bodies never read
   `Config::credentials`, `Config::profile_skills_dir`,
   `Config::profile_plugin_env`.** Those fields are slated for
   deletion in M11-F. New code reads from `profile.*` fields
   directly.
4. **Gateway behavior is preserved.** Any gateway PR must keep the
   gateway boot output byte-identical (provider list, adaptive
   routing log, skills loaded count, plugin env keys, base-tool pin
   count).
5. **Workspace policy bootstrap is idempotent.** If a file exists at
   `<workspace_root>/.octos-workspace.toml`, do not overwrite it.

## Swarm Assignment Template

Each worker receives:

- the workstream id (M11-A through M11-H)
- the linked GitHub issue
- the allowed file set verbatim (above)
- the deliverables list
- the acceptance gate
- explicit instruction not to modify other workstreams' files
- explicit instruction to escalate to the supervisor (this thread)
  if a needed API shape change falls outside the allowed file set
- explicit instruction to run codex review on every PR before
  requesting merge

Workers may add focused docs / tests inside their allowed area. Any
needed API shape change outside the allowlist must be escalated
before editing.

## Tracking

Top-level tracker issue: `#TBD-M11` (to be created with this
document). All workstream issues link back to it.

Each workstream issue title format: `M11-<letter>: <one-line>`.

Each workstream issue body includes:

1. Link to ADR.
2. Link to this workstreams doc.
3. Allowed files (copy-paste from above).
4. Deliverables (copy-paste from above).
5. Acceptance gate (copy-paste from above).
6. Blockers / depends-on (which other M11-? issues must close first).

Workers update the tracker by closing their issue with a comment
linking to the merged PR + the codex-approve thread.
