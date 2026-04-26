# M8 Runtime Contract Reference

**Status**: Draft (W4 deliverable, M8 Runtime Parity epic)
**Owner**: ymote
**Last updated**: 2026-04-26
**Companion**: [`m8-runtime-parity-prd.md`](./m8-runtime-parity-prd.md), [`m8-runtime-migration-runbook.md`](./m8-runtime-migration-runbook.md)

This document is the contract reference future contributors should consult
when adding new background work paths (pipeline nodes, spawn subagents,
plugins, workflows). The **session actor** is the reference implementation
of the contract; everything else listed here is a peer that must satisfy
the same surface.

If your code introduces a new actor that does any of:

- spawns a child agent
- runs a long-lived plugin
- registers tasks in `task_query_store`
- consumes messages from a session
- emits `tool_progress` SSE frames

…then this is the contract you must adhere to. Search the codebase for the
section anchors below when adapting an existing pattern.

---

## 1. Scope

The M8 contract applies to every **supervised task**: any unit of work the
runtime tracks separately from the parent agent loop, including

| Code site                                             | Tracked as                |
|-------------------------------------------------------|---------------------------|
| `crates/octos-cli/src/session_actor.rs` (main loop)   | Session task              |
| `crates/octos-pipeline/src/handler.rs` (worker nodes) | Pipeline node task        |
| `crates/octos-agent/src/tools/spawn.rs` (subagent)    | Spawn child task          |
| `crates/app-skills/deep-search/src/main.rs` (plugin)  | Plugin invocation task    |
| `crates/app-skills/deep-crawl/src/main.rs` (plugin)   | Plugin invocation task    |
| External `mofa-fm/fm_tts` plugin                      | Plugin invocation task    |
| External `mofa-podcast/podcast_generate` plugin       | Plugin invocation task    |
| External `mofa-slides/mofa_slides` plugin             | Plugin invocation task    |

Synchronous tools (`read_file`, `grep`, `shell`, ...) are **not** supervised
under this contract — they run inline and report through `ToolStarted` /
`ToolCompleted` only. The contract only governs work that outlives a single
agent turn or runs in the background.

## 2. The six required components

Every supervised task must wire all six. Missing any one produces a runtime
gap (the original M8 audit motivated this PRD precisely because pipeline
workers and spawn children were missing several of these).

### 2.1 FileStateCache (M8.4)

**Source**: `crates/octos-agent/src/file_state_cache.rs`
**Wire it**: `Agent::with_file_state_cache(parent.file_state_cache.clone())`
**Why**: short-circuits redundant `read_file` calls when the file content has
not changed since the last read in this transcript. Without it, a sub-agent
re-reads the same files repeatedly across a long task (high-token waste,
slow turn-around, and inconsistent behaviour vs the session actor).

The parent's cache must be **shared by clone** (deep copy via
`FileStateCache::clone_for_subagent`); never share the underlying `Mutex`
state with a subagent because they may be working on different file
universes.

### 2.2 SubAgentOutputRouter (M8.7)

**Source**: `crates/octos-agent/src/subagent_output.rs`
**Wire it**: `Agent::with_subagent_output_router(parent.router.clone())`
**Why**: captures stdout/stderr from `spawn_only` plugin invocations and
writes them to `<data>/subagent-outputs/<session_id>/<task_id>.out` so the
operator (and the session actor) can inspect progress without holding the
full payload in memory. Stripping or skipping the router means failed
plugins lose their failure message and the operator dashboard cannot
surface the per-task tail.

### 2.3 SubAgentSummaryGenerator (M8.7)

**Source**: `crates/octos-agent/src/subagent_summary.rs`
**Wire it**: `Agent::with_subagent_summary_generator(...)`
**Why**: emits periodic `subagent_progress` harness events (cheap-lane LLM
summary over the last N activities). The supervisor folds these into
`BackgroundTask.runtime_detail` so the chat UI shows "Researching topic 3
of 7" instead of "running...".

### 2.4 Recovery loop (M8.9)

**Source**: `crates/octos-cli/src/session_actor.rs::build_recovery_prompt`
**Wire it**: wrap your task body so on a retryable failure (signal from
`SpawnOnlyFailureSignal`) you re-engage the LLM with the recovery prompt
once before bubbling. The session actor sets the canonical example via
`TaskSupervisor::set_on_failure_signal`.

The recovery loop **must not** loop indefinitely — at most one retry per
failure, then bubble. The agent itself decides the next action based on
the recovery prompt.

### 2.5 task_query_store registration

**Source**: `crates/octos-agent/src/task_supervisor.rs::TaskSupervisor::register*`
**Wire it**: register your task with `parent_task_id` set to the
originating tool_call_id (or session task id for the session actor itself).
Emit state transitions via the supervisor's `on_change` hook so the B2
bridge translates them to SSE `tool_progress` frames.

Tasks that don't register here are **invisible** to:
- the admin dashboard (`/api/admin/tasks`)
- the session-tasks API (`/api/sessions/:id/tasks`)
- the chat-bubble progress pill
- the operator's cancel/restart UI

### 2.6 Cost reservation handle (F-003)

**Source**: `crates/octos-agent/src/cost_ledger.rs`
**Wire it**: each task creates a `CostReservationHandle` from the parent's
`CostAccountant`, commits on completion (or releases on cancel/error).
Emits `cost_attribution` harness events keyed by `attribution_id`. The
front-end cost-breakdown panel renders these per-task rows.

---

## 3. Plugin protocol

### 3.1 v1 (legacy, still supported)

Invocation: `./plugin <tool_name>` with JSON args on stdin. Plugin writes:

- **stdout**: a single JSON object — `{"output": "...", "success": true|false, "files_to_send": [...]}`
- **stderr**: free-form text, line by line. Each line becomes a
  `ToolProgress { name, tool_id, message: <line> }` event.

Exit codes: 0 on success, non-zero on hard failure. The host treats
non-zero exit as a plugin protocol error.

### 3.2 v2 (M8 parity, additive)

Invocation is unchanged. Plugins remain free-standing binaries.

**stdout**: same shape, with optional new fields:

```json
{
  "output": "human-readable result text",
  "success": true,
  "files_to_send": ["/abs/path/to/artifact.pdf"],
  "summary": { "kind": "deep_search.report", "..." },
  "cost": { "tokens_in": 1234, "tokens_out": 567, "usd": 0.012 }
}
```

The `summary` field feeds `SubAgentSummaryGenerator`. The `cost` field
feeds the cost ledger.

**stderr**: each line is parsed as JSON if and only if it starts with `{`.

- JSON event: validated against the v2 schema below; routed to
  the typed handler (`HarnessProgressEvent`, `HarnessCostAttributionEvent`,
  ...). Invalid JSON falls through to the legacy text-line handler.
- Plain text: legacy `ToolProgress { message }` event, exactly as v1.

This means **v1 plugins keep working unchanged**. v2 adoption is
incremental per plugin.

### 3.3 v2 stderr event schema

Each event is a single line, JSON-encoded, terminated with `\n`. Schema:

```jsonc
// progress event
{
  "schema": "octos.harness.event.v1",
  "kind": "progress",
  "session_id": "<from $OCTOS_HARNESS_SESSION_ID>",
  "task_id":    "<from $OCTOS_HARNESS_TASK_ID>",
  "workflow":   "deep_research",        // optional, plugin-defined
  "phase":      "fetching_sources",     // free-form stable label
  "message":    "Fetched 3/12 sources", // optional human-readable
  "progress":   0.42                    // optional 0..1
}

// cost attribution
{
  "schema": "octos.harness.event.v1",
  "kind": "cost_attribution",
  "session_id": "...",
  "task_id":    "...",
  "attribution_id": "<uuid v7>",
  "contract_id": "<workspace contract or workflow id>",
  "model":      "gpt-5.1",
  "tokens_in":  4321,
  "tokens_out": 567,
  "cost_usd":   0.018,
  "outcome":    "success"
}

// failure (terminal)
{
  "schema": "octos.harness.event.v1",
  "kind": "failure",
  "session_id": "...",
  "task_id":    "...",
  "phase":      "synthesis",
  "message":    "model timeout after 60s",
  "retryable":  true
}
```

The Rust types live in `crates/octos-agent/src/harness_events.rs` and are
re-exported by the host parser at `crates/octos-agent/src/plugins/tool.rs`.

### 3.4 Cancel signal (SIGTERM contract)

When the supervisor calls `TaskSupervisor::cancel(task_id)`, the host:

1. Sends **SIGTERM** to the plugin process group.
2. Waits up to **10s** for clean exit.
3. If still alive, sends **SIGKILL** to the process group and reaps.

Plugins MUST:

- Install a SIGTERM handler at startup (`signal_hook::iterator::Signals` /
  `tokio::signal::unix`).
- On SIGTERM, stop new work, drain in-flight work to a clean stopping point
  if possible, release external resources (close browsers, terminate
  ffmpeg/python helpers, persist partial state, ...), and exit.
- Exit within 10s. If a graceful exit is not possible, exit immediately.

Plugins that ignore SIGTERM will be SIGKILL-ed and may leak helper
processes (zombie Chromium, hanging ffmpeg). The runbook (section 6)
documents how to verify SIGTERM handling.

Environment variables the plugin can rely on (set by the host):

- `$OCTOS_TASK_ID` — supervisor task id (also `$OCTOS_HARNESS_TASK_ID`).
- `$OCTOS_SESSION_ID` — owning session id.
- `$OCTOS_EVENT_SINK` — file path the plugin can write structured events
  to (used as an alternative to stderr for high-volume cases).
- `$OCTOS_WORK_DIR` — sandboxed working directory.

The shared `BLOCKED_ENV_VARS` list (e.g. `LD_PRELOAD`, `DYLD_*`,
`NODE_OPTIONS`) is **stripped** before invocation; plugins cannot rely on
those.

---

## 4. Per-task data contract

Every supervised task carries the following durable state (persisted in
the supervisor's append-only ledger, surfaced via
`/api/sessions/:id/tasks` and `/api/tasks/:id`):

| Field                | Type                                              | Notes |
|----------------------|---------------------------------------------------|-------|
| `task_id`            | `String` (UUID v7)                                | Supervisor-assigned at register time. |
| `tool_name`          | `String`                                          | The tool/spawn entry point. |
| `tool_call_id`       | `String`                                          | Originating LLM tool_call id (for chat-bubble anchoring). |
| `parent_task_id`     | `Option<String>`                                  | Empty for session tasks; set for pipeline nodes / spawn children. |
| `parent_session_key` | `Option<String>`                                  | Owning session. |
| `child_session_key`  | `Option<String>`                                  | Stable child session key for cross-actor lookup. |
| `lifecycle_state`    | `Queued / Running / Verifying / Ready / Failed`   | Coarse public contract (UI consumes this). |
| `runtime_state`      | internal sub-states (see §4.1)                    | Drives the supervisor state machine; UI may peek. |
| `started_at`         | `DateTime<Utc>`                                   | Set on register. |
| `updated_at`         | `DateTime<Utc>`                                   | Bumped on every transition. |
| `completed_at`       | `Option<DateTime<Utc>>`                           | Set on terminal transition. |
| `output_files`       | `Vec<String>`                                     | Artifacts the task produced. |
| `runtime_detail`     | `Option<String>` (JSON)                           | Folded summary state from `subagent_progress` events. |
| `error`              | `Option<String>`                                  | Tool / contract / wrapper failure text. |
| `tool_input`         | `Option<Value>`                                   | Original input JSON, preserved for failure recovery. |
| `cost`               | `Option<{tokens_in, tokens_out, usd_used, usd_reserved}>` | Surfaced by F-003 cost ledger. |

### 4.1 Runtime state machine

```
                       cancel()
   ┌─────────┐  ─────────────────┐
   │ Spawned │                    │
   └────┬────┘                    │
        │                          │
        ▼                          │
┌────────────────┐     plugin      │
│ ExecutingTool  │─────  fail ─────│──→ ┌────────┐
└──────┬─────────┘                 │    │ Failed │
       │                            │    └────────┘
       ▼                            │
┌──────────────────┐                │
│ ResolvingOutputs │                │
└──────┬───────────┘                │
       ▼                            │
┌──────────────────┐                │
│ VerifyingOutputs │                │
└──────┬───────────┘                │
       ▼                            │
┌─────────────────────┐             │
│ DeliveringOutputs   │             │
└──────┬──────────────┘             │
       ▼                            │
┌────────────┐                      │
│ CleaningUp │                      │
└──────┬─────┘                      │
       ▼                            ▼
┌────────────┐              ┌────────────┐
│ Completed  │              │   Failed   │
└────────────┘              └────────────┘
```

All terminal transitions emit a `tool_progress` SSE frame and (for
spawn_only / plugin tasks) trigger the `on_failure_signal` callback so
the session actor can build a recovery prompt.

---

## 5. Public surface

### 5.1 SSE `tool_progress` frame

```json
{
  "type": "tool_progress",
  "name": "fm_tts",
  "tool_call_id": "call_abc123",   // anchors to chat bubble (M8.B1+B2)
  "message": "delivering: voice synthesised, 1 file"
}
```

Every supervisor state transition produces one of these frames. The chat
client pins the bubble using `tool_call_id`; without it the bubble may
move to a different turn after compaction.

### 5.2 HTTP API (M7.9 supervisor exposure)

| Endpoint                                       | Action                          | Response                           |
|------------------------------------------------|---------------------------------|------------------------------------|
| `GET  /api/sessions/:id/tasks`                  | List tasks for a session        | `[BackgroundTask, ...]`            |
| `GET  /api/tasks/:task_id`                      | Single task                     | `BackgroundTask`                   |
| `POST /api/tasks/:task_id/cancel`               | Cancel a running task           | 200 / 404 / 409                    |
| `POST /api/tasks/:task_id/restart-from-node`    | Restart from a failed node      | 200 with new task_id / 404 / 409   |

Both POST endpoints are bearer-token protected (admin token or session
owner token). Cancel returns 409 if the task is already terminal.

### 5.3 Harness event sink

When a plugin sets `$OCTOS_EVENT_SINK=/path/to/sink.jsonl`, the host
appends one JSON event per line. The session actor and supervisor read
this sink and fold structured events into `BackgroundTask.runtime_detail`.

The plugin can write events directly to `$OCTOS_EVENT_SINK` instead of (or
in addition to) stderr. The sink path is per-task — concurrent plugins
get separate sinks. Same schema as §3.3.

---

## 6. Validation matrix

A new background work path is "M8-compliant" iff all of the following hold.
The runbook (`m8-runtime-migration-runbook.md`) walks through how to verify
each on a live canary.

### 6.1 Unit tests

| Test                                                                         | Asserts |
|------------------------------------------------------------------------------|---------|
| `your_actor::has_file_state_cache_attached`                                  | M8.4 wired |
| `your_actor::registers_with_supervisor_on_start`                             | task_query_store wired |
| `your_actor::recovery_loop_fires_on_simulated_retryable_failure`             | M8.9 wired |
| `your_actor::cost_reservation_handle_committed_on_success`                   | F-003 wired |
| `plugin::v2_progress_event_round_trip`                                       | protocol v2 parser |
| `plugin::v1_text_event_falls_back_to_legacy`                                 | backward compat |
| `plugin::sigterm_within_10s`                                                 | cancel contract |

### 6.2 Live integration tests

Live against `mini2` (`https://dspfac.bot.ominix.io`) — never `mini5`.

| Test                                          | Validates |
|-----------------------------------------------|-----------|
| `live-pipeline-end-to-end.spec.ts`            | Full deep research run, cards, cost, cancel mid-flight, restart-from-node |
| `live-spawn-end-to-end.spec.ts`               | slides_delivery / podcast: per-phase progress, cancel, recovery |
| `live-cost-tracking.spec.ts`                  | Per-pipeline cost breakdown across multiple runs |
| `m8-runtime-invariants-live.spec.ts`          | M8.4 / M8.6 / M8.7 / M8.9 invariants surface end-to-end |

### 6.3 Manual smoke checklist

Per release:

- [ ] Trigger deep research — see node tree fill in
- [ ] Cancel mid-flight — task transitions to Cancelled within 15s
- [ ] Trigger pipeline with fault injection — verify failure surfaces with reason
- [ ] Click restart-from-node — verify only downstream nodes re-run
- [ ] Open cost breakdown panel — verify per-node values
- [ ] Trigger slides delivery — verify per-phase status
- [ ] Trigger podcast — verify cancellation works (no orphan ffmpeg/python processes)
- [ ] Verify `deep_search` returns synthesized prose, not a Bing dump

---

## 7. Adding a new actor

When introducing a new background work path (e.g. a new pipeline executor,
a new spawn entry point, a new plugin):

1. Identify the parent context that owns the request. That parent must
   already be M8-compliant — if not, fix it first.
2. Wire the six required components (§2). Mirror the session actor's
   pattern in `crates/octos-cli/src/session_actor.rs`.
3. Register with the supervisor at task start; deregister (terminal
   transition) on completion / failure / cancel.
4. If the actor invokes a plugin, the plugin must also be v2-compliant
   (or accept the v1 fallback explicitly).
5. Add unit tests per §6.1 and at least one live integration spec per
   §6.2.
6. Document the new actor's lifecycle states under §1's table.

The "RFC bar" for new actors: open a PR titled `feat(runtime): add <actor>
M8 compliance` with a checkbox list pointing at each subsection of §2.

---

## 8. Anti-patterns (do not do these)

- **Forking your own task tracker.** Use the `TaskSupervisor` — it is the
  single source of truth. A second tracker means double persistence, two
  failure paths, and inconsistent state.
- **Holding the parent's `FileStateCache` `Arc<Mutex<...>>` directly.** Use
  `clone_for_subagent` so each agent has its own deep copy. Sharing the
  inner mutex causes a sub-agent's reads to evict the parent's entries.
- **Emitting `tool_progress` frames without a `tool_call_id`.** The chat
  bubble loses its anchor and the frame surfaces under the wrong turn after
  compaction. Always pass the originating `tool_call_id`.
- **Spawning a plugin without setting `$OCTOS_TASK_ID` /
  `$OCTOS_SESSION_ID`.** The plugin cannot emit valid v2 events without
  these. The host always sets them; if you bypass the host, set them
  yourself.
- **Sleep-loop polling `BackgroundTask` instead of using the on_change
  callback.** The supervisor already fans out transitions; subscribers
  must register a callback, not poll.
- **Calling SIGKILL directly without going through `cancel()`.** The
  supervisor's `cancel()` runs the SIGTERM-then-SIGKILL escalation and
  emits the right state transitions; bypassing it leaves the task in an
  inconsistent state.

---

## 9. Glossary

- **M8.4** — `FileStateCache`: per-actor cache of file-state claims.
- **M8.6** — Resume sanitizer: validates a resumed session's worktree.
- **M8.7** — `SubAgentOutputRouter` + `SubAgentSummaryGenerator`.
- **M8.9** — Runtime failure recovery (`build_recovery_prompt`).
- **M7.9** — PM supervisor primitives (cancel / steer / relaunch).
- **F-003** — Cost reservation handle.
- **F-005** — Workspace contract enforcement.
- **B1+B2** — SSE `tool_call_id` + supervisor → ToolProgress bridge
  (`9687fa95` + `889e5e05`).
- **Tool call id** — UUID v7 generated by the LLM provider for each tool
  invocation; used by the chat UI as the bubble anchor.
- **`task_query_store`** — alias for `TaskSupervisor`'s queryable surface;
  what `/api/sessions/:id/tasks` reads from.
