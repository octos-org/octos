# Octos Server Feature Requirements

Status: proposed test contract.
Owner: octos server/runtime.
Applies to: `octos serve`, AppUI/UI Protocol, harness runtime, task supervisor,
approval gate, durable ledger, dashboard/API surfaces, and client integration.

## Purpose

This document defines the server-side feature requirements that Octos must
satisfy before the server can be treated as a production AppUI/harness runtime.
It is intended to be used as a merge gate for `octos` main and as a shared
contract for `octos-tui`, `octos-app`, dashboard, API clients, and future app
harnesses.

The server is responsible for runtime truth. Clients may render, organize, and
request state, but they must not need to guess the runtime lifecycle from raw
logs, prompt text, or private implementation details.

## Non-Negotiable Product Principles

- AppUI is the stable client contract. Server behavior that clients depend on
  must be represented in shared `octos-core` protocol types and protocol golden
  tests.
- Runtime truth lives on the server. The server owns session state, approval
  state, task state, sandbox policy, workspace cwd, artifact truth, and replay
  cursors.
- Durable state must not be overwritten by stale replay. A disk snapshot or old
  ledger segment must never regress a newer live session.
- Terminal states must survive backpressure. Completed, failed, cancelled, and
  approval-decided/cancelled events are not optional progress noise.
- App harnesses are not prompt conventions. Artifact contracts, validators,
  background tasks, and operator truth must be explicit runtime objects.
- Additive protocol changes must preserve old clients where possible and must
  advertise capabilities before clients rely on them.

## Requirement Matrix

| ID | Requirement | Priority | Acceptance Criteria | Verification |
|---|---|---:|---|---|
| SRV-001 | AppUI protocol source of truth | P0 | All client-visible methods, notifications, params, results, error codes, and feature flags are defined in `octos-core`; no server/client private wire extensions. | core golden tests and API grep gate |
| SRV-002 | Capability advertisement | P0 | Server advertises only supported AppUI methods/features, including mode-specific limitations or typed `runtime_not_ready` behavior. | protocol e2e capability test |
| SRV-003 | Session open and replay | P0 | `session/open` creates or rehydrates a session, returns active profile, workspace root, cursor, pane snapshots when supported, and typed errors on invalid cursor/session/cwd. | `m9-protocol-session-open` e2e |
| SRV-004 | Workspace cwd enforcement | P0 | Requested cwd is canonicalized and accepted only under approved readable/writable roots. Tools execute relative to session cwd, not server launch cwd. | unit tests plus live cwd fixture |
| SRV-005 | Turn lifecycle | P0 | `turn/start`, `turn/started`, streamed `message/delta`, `turn/completed`, and `turn/error` form a deterministic lifecycle with one active turn per session unless explicitly supported otherwise. | protocol e2e and race tests |
| SRV-006 | Turn interruption | P0 | `turn/interrupt` is idempotent, scoped to session and turn id, drains pending approvals, cancels active work safely, and emits terminal state. | TOCTOU and approval-drain tests |
| SRV-007 | Typed error taxonomy | P0 | AppUI errors use stable JSON-RPC/app code ranges and typed `error.data.kind`; no collisions with reserved JSON-RPC codes. | core taxonomy tests |
| SRV-008 | Approval request shape | P0 | Approval requests include stable id, session, turn, tool name, title/body, risk, approval kind, typed details, render hints, and optional diff preview id. | approval protocol golden tests |
| SRV-009 | Approval response semantics | P0 | `approval/respond` accepts approve/deny decisions, enforces scope, rejects stale decisions with typed errors, records manual and auto decisions, and resumes runtime only when appropriate. | approval unit and e2e tests |
| SRV-010 | Approval lifecycle notifications | P0 | `approval/requested`, `approval/auto_resolved`, `approval/decided`, and `approval/cancelled` are durable enough for reconnecting clients to render the true state. | replay and interrupt tests |
| SRV-011 | Approval backpressure safety | P0 | Approval send failures/backpressure cancel or drain pending runtime waits so the model cannot continue as if the user approved. | fault injection test |
| SRV-012 | Manifest-declared risk | P0 | Tool risk shown to clients comes from trusted manifests or explicit `unspecified`; missing risk must not silently downgrade to low risk. | plugin manifest tests |
| SRV-013 | Sandbox policy preservation | P0 | AppUI and tool execution preserve profile/session sandbox config, network policy, writable roots, and approval policy. Defaults are used only when no explicit policy exists. | sandbox parity tests |
| SRV-014 | Diff preview API | P0 | File mutation progress and diff approvals produce stable preview ids; `diff/preview/get` resolves paths against session cwd, supports multi-file previews, and returns typed missing/expired errors. | diff preview e2e |
| SRV-015 | Diff preview durability | P1 | Diff preview metadata survives reconnect while relevant, with clear expiry semantics. Stale previews return typed errors, not wrong content. | replay/expiry tests |
| SRV-016 | Task lifecycle model | P0 | Task runtime state includes pending, running, completed, failed, and cancelled; mappings from internal supervisor states are deterministic. | core/server mapping tests |
| SRV-017 | Task registry | P0 | Server maintains a scoped background task registry with stable task ids, title/tool, session lineage, state, runtime detail, output cursor, and timestamps. | task supervisor tests |
| SRV-018 | Task updates | P0 | Task updates are emitted as AppUI notifications with task id, session id, title, state, runtime detail, and terminal states. Terminal updates survive backpressure. | backpressure tests |
| SRV-019 | Task output read | P0 | `task/output/read` returns a bounded snapshot projection with cursor, output text, output files, source, and limitations. It must not pretend to be full live-tail unless live-tail is implemented. | task output protocol tests |
| SRV-020 | Task output live-tail | P1 | Active task output deltas are streamed through `task/output/delta` with cursor monotonicity and duplicate prevention. | live-tail e2e |
| SRV-021 | Task control API | P1 | AppUI supports task list, cancel, and restart-from-node when advertised. Requests are session/profile scoped and reject missing/invalid scope with typed errors. | task-control protocol e2e |
| SRV-022 | Restart-from-node semantics | P1 | Restart creates a successor task only when runtime can either execute it or explicitly marks it as accepted/pending-runtime. Clients must not mistake placeholder registration for completed restart. | supervisor and protocol tests |
| SRV-023 | Swarm task lifecycle | P0 | Swarm/subagent tasks expose creation, task id, pending/running/completed/failed/cancelled states, cancellation, progress, structured output, and registry visibility. | swarm/task supervisor tests |
| SRV-024 | Swarm observability | P1 | Server exposes enough task and progress data for clients to implement `/ps`, status rows, expandable task cards, and agent labels without scraping logs. | AppUI contract tests |
| SRV-025 | MCP/CLI/subagent parity | P1 | Swarm work launched as MCP server calls, CLI calls, or subagents maps into the same task lifecycle and observability model. | integration tests |
| SRV-026 | Durable UI ledger | P0 | Durable AppUI notifications are appended to a ledger with monotonic cursors and schema versioning. Replay by cursor must be deterministic. | ledger unit and e2e tests |
| SRV-027 | Ledger recovery safety | P0 | Disk replay, rotation, and snapshot recovery must never apply a stale snapshot over newer live state. | crash/replay regression test |
| SRV-028 | Replay lossy signal | P0 | If durable notifications are dropped or cursor continuity is broken, server emits `protocol/replay_lossy` with dropped count and last durable cursor when known. | backpressure/fault tests |
| SRV-029 | Backpressure policy | P0 | Non-terminal progress can coalesce or drop with accounting. Terminal state, approval lifecycle, and replay-lossy signals must use delivery paths that survive transient backpressure. | stress tests |
| SRV-030 | Pane snapshots | P1 | `session/open` can include workspace, artifacts, and git snapshots when capability is advertised; missing panes degrade gracefully. | pane snapshot tests |
| SRV-031 | Artifact truth | P0 | Final artifacts come from declared contract/runtime truth, not filename heuristics. Failed validators block success. | harness artifact tests |
| SRV-032 | Validator enforcement | P0 | Harness validators run at declared lifecycle points and can prevent a task/session from being marked ready. | harness policy tests |
| SRV-033 | Workspace policy | P0 | App/workspace policy declares roots, artifacts, validation, spawn rules, and sandbox limits; runtime enforces it before publish/delivery. | policy fixture tests |
| SRV-034 | Session/task persistence | P0 | Release-critical session/task state survives chat compaction, process restart, host restart, actor crash, and reconnect where required by the contract. | restart/recovery tests |
| SRV-035 | Client compatibility | P0 | Any AppUI enum or wire change is tested against known clients or made forward-compatible through fallback variants/wildcard-safe client guidance. | downstream compile checks |
| SRV-036 | Protocol change governance | P0 | Every AppUI behavior change has a UPCR or protocol doc update, shared type changes, golden tests, server tests, and client migration notes. | PR checklist |
| SRV-037 | Security and secret hygiene | P0 | API keys, auth tokens, bearer headers, and sensitive env values are never emitted through AppUI notifications, logs intended for clients, e2e captures, or task output summaries. | redaction tests |
| SRV-038 | Auth and profile isolation | P0 | WebSocket/API requests are authenticated, profile scoped, and cannot read/control sessions or tasks outside the authorized profile/session. | auth tests |
| SRV-039 | Metrics and audit | P1 | Server records counters for dropped sends, replay-lossy, approval decisions, task terminal states, tool failures, and task-control commands. | metrics tests |
| SRV-040 | Live coding UX support | P1 | Server emits enough structured data for clients to show model working state, tool cards, approvals, diffs, plan/task progress, final recap, and background task status without parsing assistant prose. | long coding tmux harness |

## Major Server Flows

### 1. Session Open And Rehydrate

Precondition: an authenticated AppUI client connects to the WebSocket endpoint.

Expected flow:

1. Client sends `session/open` with session id, profile id, optional cwd, and
   optional cursor.
2. Server validates auth, profile scope, cwd policy, and cursor.
3. Server opens or rehydrates the session.
4. Server sends a typed success result and durable replay notifications after
   the requested cursor when applicable.
5. If replay cannot be exact, server emits `protocol/replay_lossy` and gives
   clients enough state to rehydrate.

### 2. Coding Turn

Precondition: session is open and no incompatible active turn exists.

Expected flow:

1. Client sends `turn/start`.
2. Server records turn identity and emits `turn/started`.
3. Server streams `message/delta`, tool lifecycle notifications, progress, task
   updates, approval requests, diff preview ids, and task output deltas as
   structured AppUI events.
4. Server enforces sandbox, approval, workspace policy, and validator gates.
5. Server emits exactly one terminal turn event: `turn/completed` or
   `turn/error`.

### 3. Approval-Gated Tool Execution

Precondition: a tool requires user approval.

Expected flow:

1. Server creates a stable approval id and stores pending approval state.
2. Server emits `approval/requested` with risk and typed details.
3. Runtime waits for manual response or auto-resolution.
4. `approval/respond` is scoped and validates decision, approval id, session,
   turn, and policy.
5. Server emits `approval/decided` or `approval/auto_resolved`.
6. If turn is interrupted or send fails, server emits `approval/cancelled` and
   unblocks runtime safely.

### 4. Background Task And Swarm Execution

Precondition: server spawns background work as CLI, MCP, or subagent work.

Expected flow:

1. Server allocates stable task id and records lineage/session scope.
2. Server emits task state transitions and progress.
3. Server captures structured output and bounded output previews.
4. Terminal task state is durable and survives transient send backpressure.
5. Task list/read/cancel/restart APIs operate only within authorized session
   and profile scope.

### 5. Diff Preview

Precondition: a file mutation or diff approval has a previewable patch.

Expected flow:

1. Server stores preview metadata keyed by preview id.
2. Server emits preview id through typed approval details or file mutation
   progress.
3. Client requests `diff/preview/get`.
4. Server resolves file paths relative to session cwd and returns a bounded
   multi-file diff preview.
5. Stale or unknown preview ids return typed errors.

### 6. Ledger Replay And Crash Recovery

Precondition: server restarts or client reconnects with a cursor.

Expected flow:

1. Server loads durable ledger segments and current live state.
2. Server refuses to apply stale snapshots over newer live session state.
3. Server replays durable notifications in cursor order.
4. Server surfaces gaps with `protocol/replay_lossy`.
5. Client can call `session/open`, `task/list`, `task/output/read`, and pane
   snapshot APIs to rehydrate current visible state.

## Required Test Coverage

### Rust Unit Tests

- AppUI method/result mapping and golden JSON.
- Error taxonomy code ranges and `error.data.kind` shapes.
- Session key parsing, profile/cwd/scope validation.
- Approval state transitions including stale respond, auto-resolution,
  cancellation, and interruption.
- Task state mapping including cancelled.
- Task supervisor terminal delivery under backpressure.
- Ledger append, cursor replay, rotation, snapshot recovery, and stale snapshot
  prevention.
- Diff preview path resolution and unknown/expired preview errors.
- Sandbox config preservation across AppUI request handling.

### Protocol E2E Tests

- `session/open` with valid cwd, invalid cwd, cursor replay, cursor out of
  range, and pane snapshots.
- `turn/start` with streaming, tools, approval, diff preview, task output, and
  terminal events.
- `turn/interrupt` during model thinking, during approval wait, and during tool
  execution.
- `approval/respond` for approve once, approve session, deny, stale approval,
  profile mismatch, and auto-resolved approval.
- `task/output/read` with cursor, limit, unknown task, and projection
  limitations.
- Task list/cancel/restart when task-control is advertised.
- Backpressure/fault injection for terminal task updates and approval sends.

### Integration And Live Tests

- Long-running coding session with real provider and many tool calls.
- Multi-agent/swarm session where task registry shows all child work.
- Server restart/reconnect during active or recently completed background work.
- Dashboard/API/TUI compatibility smoke.
- Downstream `octos-tui` and `octos-app` compile and protocol smoke after
  AppUI changes.

## Current Implementation Notes

Current implementation already includes many server primitives that should be
preserved and tested:

- Shared AppUI/UI Protocol types in `crates/octos-core/src/ui_protocol.rs` and
  `crates/octos-core/src/app_ui.rs`.
- WebSocket AppUI handler in `crates/octos-cli/src/api/ui_protocol.rs`.
- Approval helpers in `crates/octos-cli/src/api/ui_protocol_approvals.rs`.
- Durable UI ledger in `crates/octos-cli/src/api/ui_protocol_ledger.rs`.
- Diff preview helper in `crates/octos-cli/src/api/ui_protocol_diff.rs`.
- Task output projection in `crates/octos-cli/src/api/ui_protocol_task_output.rs`.
- Task supervisor in `crates/octos-agent/src/task_supervisor.rs`.
- Harness event/error primitives in `crates/octos-agent/src/harness_events.rs`
  and `crates/octos-agent/src/harness_errors.rs`.
- Harness developer contract docs under `docs/OCTOS_HARNESS_*`.

Known gaps to track against this document:

- AppUI task-control support must be merged only with compatible `octos-tui`
  and `octos-app` client handling.
- `task/output/read` is currently a snapshot projection; true disk-routed
  stdout/stderr live-tail is a separate feature.
- Task-control capability advertisement must define whether support is binary
  level or runtime-mode level.
- Restart-from-node semantics must distinguish accepted placeholder from actual
  re-execution when no relaunch callback is wired.
- Some client UX needs, such as `/ps`, `/stop`, expandable task cards, and
  Codex-style status rows, depend on this server exposing stable structured
  task/progress data.

## Release Gate

No server branch should merge to main as AppUI/harness complete unless:

- all P0 requirements touched by the branch have focused tests
- AppUI protocol changes update shared types and golden tests
- terminal states survive backpressure in tests
- replay/ledger changes include stale snapshot regression coverage
- approval and sandbox policy behavior is preserved
- downstream `octos-tui` and `octos-app` compatibility is verified or the
  branch is explicitly marked server-only with a migration plan
