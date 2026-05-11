# M11 — ProfileRuntime / SessionRuntime ADR

Date: 2026-05-10
Branch (target): `main` (no release branch; M11 is foundational and merges to main directly)
Status: PROPOSED — accepted after the 2026-05-10 yangmi-voice incident
chain.

## Context

Between 2026-05-10 06:00 and 2026-05-10 23:00 four PRs landed on the
serve agent, each fixing one mis-scoped variable surfaced by the
yangmi voice-clone failure on `dspfac.crew.ominix.io`:

- **#866** — serve never read per-profile `cfg.llm`; web `/chat`
  always used the seeded top-level deepseek provider. Fixed by
  overlaying `cfg.llm` onto a transient `Config` and threading
  per-profile credentials via a new `Config::credentials` map.
- **#867** — QoE adaptive routing wiring was duplicated between
  gateway and serve. Lifted into shared `qos_catalog::build_adaptive_provider_chain`.
- **#868** — serve scanned only global skill dirs
  (`~/.octos/{plugins,skills}/`), not the dashboard-installed
  per-profile path (`~/.octos/profiles/<id>/data/skills/`). Fixed by
  adding `Config::profile_skills_dir` and `Config::profile_plugin_env`
  transient fields, and switching the loader to `load_into_with_options`.
- **#869** — runtime contract for `spawn_only` tools required file
  outputs; informational tools like `fm_voice_list` failed with "no
  output files produced" despite returning valid text. Fixed by
  delivering text-only output as a Notification when `files_to_send`
  is empty.

Despite all four, live re-test on mini1 surfaced a fifth gap:
`✗ fm_tts failed: workspace policy not found`. The runtime calls
`enforce_spawn_task_contract` which reads
`tools.workspace_root() + .octos-workspace.toml`. Serve sets
`workspace_root = cwd = /Users/cloud` (the daemon's working dir);
no policy file lives there. Gateway dodges this because its per-session
workspace bootstrap creates the policy under
`profiles/<id>/data/users/<session_key>/workspace/`. Serve never had
the equivalent setup.

The pattern is unmistakable. Every fix this week patched a single
mis-scoped variable on `serve::try_create_agent`'s output. Each fix
surfaces the next gap. We have been retrofitting per-profile and
per-session awareness onto an `Agent` instance that was originally
built for the standalone `octos chat` CLI in a single-tenant
checkout — and it does not match the multi-profile, multi-session
shape the dashboard, web `/chat`, AppUI coding sessions, and TUI
multi-window now demand.

## Decision

Eliminate the embedded server-wide `state.agent` and replace it with
two first-class types, both built from the same code path gateway
subprocesses already use:

- **`ProfileRuntime`** — one per profile per host process. Owns the
  LLM provider, credentials, registered skills, plugin-env template,
  tool policy, default sandbox config, and the base `ToolRegistry`
  template. Long-lived; hot-reloaded on profile-config change.
- **`SessionRuntime`** — one per `(profile_id, session_key)`. Owns
  the per-session `workspace_root`, `plugin_work_dir`, sandbox config
  (with optional per-session override), the session's cloned
  `ToolRegistry` (workspace-bound, policy-filtered), and the per-session
  `Agent` instance. Cached with TTL/LRU; rebuildable from disk.

These map 1:1 onto the two scopes a multi-session product has:

- **Profile scope** — identity, billing, model, secrets, what skills
  are installed. Shared by every session opened by the same logged-in
  user.
- **Session scope** — workspace, what's allowed in this conversation,
  what's been said. Independent across two sessions opened by the
  same user.

Anything that can vary between two chats opened by the same logged-in
user is session-scope. Anything that's an account property is
profile-scope. Every "is this thing per-profile or per-session?"
question now has one canonical answer instead of one PR per question.

## Worked examples

The same two types handle the four shapes we ship today.

### Web — one logged-in user, multiple rooms

```
serve process
└─ AppState
   ├─ profiles: { "dspfac": ProfileRuntime(kimi-k2.5, AUTODL_API_KEY, mofa-fm + slides-skill) }
   └─ sessions:
        ("dspfac", "chat-room-1"):  SessionRuntime { cwd=<data_dir>/users/.../workspace,
                                                     tools=clone(profile.tools), … }
        ("dspfac", "slides-1"):     SessionRuntime { cwd=<data_dir>/users/slides-1/workspace,
                                                     tools=clone(profile.tools).filter(slides),
                                                     sandbox=no-network, … }
        ("dspfac", "chat-room-2"):  SessionRuntime { distinct cwd, history, sandbox … }
```

All three rooms share the LLM and credentials (profile-scope). All
three have independent cwd, tool registry view, sandbox, chat
history (session-scope).

### Coding-agent — N concurrent repos per user

A coding-agent UI opens a session with `workspace_root` set to a
specific repo path. `SessionRuntime::bootstrap` honors that hint
(after `validate_session_workspace_allowed`). A second coding session
in another repo is a separate `SessionRuntime` — separate `Agent`,
separate tools view, separate sandbox state, zero interference.

This is the Codex model. Sessions map to `SessionRuntime`; the LLM
API account + global config maps to `ProfileRuntime`.

### TUI — multiple terminals on one box

Each `octos tui` is its own process. Two terminals get two isolated
state spaces by virtue of OS process separation. Inside one TUI
process you can also have multiple sessions (split panes) sharing the
one `ProfileRuntime`. Different OS users get extra isolation via
`~/.octos` being scoped to `$HOME` — separate auth stores, separate
profile stores, separate processes.

### Gateway — per-profile subprocess

Each gateway subprocess constructs one `ProfileRuntime` and one or
more `SessionRuntime`s (one per Telegram chat, Discord channel, etc.).
The same code path serve uses, just in a child process started by
`ProcessManager`.

## What this dissolves

| Symptom we patched in 2026-05 | Reason it was a symptom | How the new model removes it |
|---|---|---|
| `Config::credentials` transient field (#866) | API key plumbing was bolted onto a global Config | `ProfileRuntime.credentials` — it's profile state, not config state |
| `Config::profile_skills_dir` transient field (#868) | Plugin loader scanned wrong dirs | `ProfileRuntime.skills_dir` populates the base `ToolRegistry` at bootstrap; every session inherits |
| `Config::profile_plugin_env` transient field (#868) | Skill spawns missed per-profile env | `ProfileRuntime.plugin_env_template` injected into every `SessionRuntime` plugin spawn |
| `overlay_profile_llm` in `serve.rs` (#866) | Serve had to retrofit profile awareness onto a globally-scoped Agent | `ProfileRuntime::bootstrap(profile)` builds it correctly from the start |
| `"workspace policy not found"` (today) | Serve's `workspace_root = cwd`; no per-session policy bootstrap | `SessionRuntime::bootstrap` creates `<workspace_root>/.octos-workspace.toml` if missing |
| Multi-tenant base-registry leak (codex's note on #868) | One global ToolRegistry shared by every session | `SessionRuntime.tools` is a per-session clone of `ProfileRuntime.tool_specs` |
| Coding-agent N-sessions-per-profile (M10 deliverable) | One Agent per profile gateway subprocess | Each session has its own `SessionRuntime` with its own `Agent` |

None of the four PRs from 2026-05-10 is wasted; each PR's body becomes
the implementation of `ProfileRuntime::bootstrap` or
`SessionRuntime::bootstrap`. The transient `Config` fields they
introduced go away — they always wanted to live somewhere else.

## What this is NOT

- **Not a subprocess refactor.** Sessions run in-process within the
  host (`serve` or `octos tui`). Sandbox isolation continues to live
  at the shell-tool spawn boundary (`Bwrap`/`Macos sbpl`/`Docker`),
  not at the session level. If we ever need stronger session
  isolation (hostile multi-tenant compute), the abstraction supports a
  "session = subprocess" mode without changing the type signatures.
- **Not a config rewrite.** Profile JSON on disk stays the same. The
  change is internal: how the runtime reads it into in-memory state.
- **Not a wire-protocol change.** `/api/chat`, UI Protocol v1 WS, and
  the bus message shape are unchanged. Only the server-side
  dispatcher changes.
- **Not per-session credentials.** If a session needs a different API
  key, that's a different profile. Credentials are an identity
  property. Codex draws the same line; we keep it.

## Workstream plan

See `workstreams/M11-runtime-unification.md` for the full breakdown.
Eight workstreams (`M11-A` through `M11-H`) with explicit allowed
file sets, deliverables, and acceptance gates. Designed for parallel
agent execution under supervisor gating.

Phase order, with explicit blockers:

```
M11-A (type skeleton)      ── blocks B, C, D
   │
   ├── M11-B (gateway bootstrap → ProfileRuntime)
   ├── M11-C (serve agent → SessionRuntime + workspace bootstrap)
   │
   └── M11-D (AppState refactor, /api/chat dispatcher)  ── blocks E, F, G
          │
          ├── M11-E (UI Protocol per-session workspace wiring)
          ├── M11-F (delete legacy Config transients + overlay)
          │
          └── M11-G (coding-agent multi-session e2e)  ── blocks H
                 │
                 └── M11-H (fleet redeploy + soak gate)
```

`M11-B` and `M11-C` can run in parallel after `M11-A`. `M11-E` and
`M11-F` can run in parallel after `M11-D`. `M11-G` requires both `D`
and `E`. `M11-H` ships only after `F` and `G` are clean on CI and on
mini1/2/3 staging.

## Acceptance: end-state checklist

1. `state.agent: Option<Arc<Agent>>` is gone from `AppState`. Replaced
   by `state.profiles` and `state.sessions`.
2. `try_create_agent` in `commands/serve.rs` is gone. Replaced by
   `SessionRuntime::bootstrap`.
3. `overlay_profile_llm` and `populate_profile_credentials` in
   `commands/serve.rs` are gone. The logic lives inside
   `ProfileRuntime::bootstrap`.
4. `Config::credentials`, `Config::profile_skills_dir`,
   `Config::profile_plugin_env` are gone. The runtime state lives
   on `ProfileRuntime`.
5. Gateway subprocess startup calls the same `ProfileRuntime::bootstrap`
   function serve calls.
6. Web `/chat` and AppUI WS handlers resolve `SessionRuntime` per
   request from `(profile_id, session_key)`.
7. Multi-session test: open two AppUI coding sessions on the same
   profile, point them at different `workspace_root`s, run a
   `read_file` in one and assert the other sees its own workspace.
   Existing unit test (`session_filesystem_profile_for_workspace`)
   stays green.
8. yangmi voice-clone end-to-end on mini1 with the `/Users/cloud/.octos-workspace.toml`
   hotfix DELETED. The workspace policy is bootstrapped under
   `<profile_data_dir>/users/<session_key>/workspace/` per-session.
9. `cargo clippy --workspace --all-targets -- -D warnings` clean.
10. M9-era soak harness (Telegram + web + AppUI) green on
    mini1/mini2/mini3.

## Risks

- **Behavior drift in gateway during refactor.** Mitigated by
  `M11-B` keeping gateway behavior byte-identical (just relocating
  the bootstrap body to a callable function). Codex review required.
- **SessionRuntime cache eviction edge cases.** Mitigated by making
  the cache a perf optimization only — every `SessionRuntime` is
  reconstructible from disk-persisted session metadata + chat
  history.
- **Tool registry clone cost.** Today's `ToolRegistry::snapshot_excluding`
  is already paid by `with_workspace_root`. Per-session clone is one
  more invocation; profile data on big skill graphs is bounded
  (~30 tools max).
- **Migration window.** The four 2026-05-10 PRs land before M11 and
  remain in main. M11 deletes the transient `Config` fields they
  introduced; that's a normal "extract → relocate → delete" cycle, not
  a revert.
