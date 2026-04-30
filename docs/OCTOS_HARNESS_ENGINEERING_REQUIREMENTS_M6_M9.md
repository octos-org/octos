# Octos Harness Engineering Requirements: M6-M9

Date: 2026-04-30
Review baseline: `origin/main` at `119bf782`

## Purpose

This document defines the product and engineering requirements for Octos
Harness milestones M6, M7, M8, and M9. It is intended to be used by critical
reviewers, product owners, and engineering leads to judge implementation
quality consistently.

Octos Harness must provide a durable, observable, policy-governed execution
layer for coding agents, swarm agents, CLI tools, MCP agents, and AppUI
clients. A feature is not complete when the type exists; it is complete only
when it is wired through runtime execution, persistence, recovery,
observability, and client control surfaces.

## Global Quality Bar

Every milestone feature must satisfy these gates.

| Gate | Requirement |
| --- | --- |
| Contract | Stable typed API, versioned schema, and forward-compatible unknown handling. |
| Runtime | Real production path wired, not fixture-only behavior. |
| Durability | Reconnect, restart, and backpressure cannot lose authoritative state. |
| Observability | Operators can inspect state without reading raw logs. |
| Control | Users/operators can interrupt, cancel, approve, deny, retry, or inspect where applicable. |
| Safety | Sandbox, approval, path, network, and policy rules are preserved across all execution paths. |
| Tests | Unit, replay, failure, and end-to-end coverage exercise the real path. |

## M6: Harness Contract Foundation

M6 owns the formal contract between apps, skills, tools, and the Octos runtime.

### Requirements

- Define versioned harness event schemas for task, phase, artifact, validator,
  retry, failure, cost, routing, and dispatch events.
- Define workspace contract rules, including required files, artifact
  expectations, validators, and completion gates.
- Define ABI and schema-versioning policy for harness evolution.
- Provide compatibility gates for first-party and external skills.
- Provide deterministic fixtures that exercise the default harness path, not a
  parallel fake path.
- Ensure compaction and summarization preserve required state and do not drop
  unresolved tool calls, artifacts, task context, or recovery context.

### Definition Of Done

- A new skill or app can declare its contract and run through the harness
  without bespoke runtime code.
- Validator failure blocks false completion.
- Schema additions use open registries or explicit forward-compatible behavior.
- Deterministic fixtures catch contract drift.

## M7: Swarm And External Agent Execution

M7 owns dispatching work to multiple agent backends.

### Requirements

- Support swarm execution through MCP server, CLI-backed agent, and native
  sub-agent backends.
- Normalize all backends into a common dispatch and result model.
- Track dispatch IDs, subtask IDs, attempts, backend, endpoint, outcome, cost,
  and artifacts.
- Support bounded parallel, sequential, fanout, and pipeline execution.
- Persist swarm dispatch state so restart does not duplicate completed work.
- Attribute cost and provenance per backend and per subtask.
- Enforce sandbox, environment allowlist, SSRF/network policy, and tool policy
  equally across MCP, CLI, and native backends.
- Surface swarm dispatch events into the same harness observability channel.

### Definition Of Done

- A failed or retried swarm run is explainable from persisted records.
- A restart can resume or safely short-circuit finalized dispatches.
- MCP, CLI, and native backends do not bypass approval, sandbox, cost, or
  artifact validation.

## M8: Runtime Lifecycle And Observability

M8 owns lifecycle management for every task.

### Requirements

- Every background, swarm, and tool task has a stable task ID.
- Task lifecycle supports queued, running, verifying, ready, failed, and
  cancelled states.
- Runtime state supports fine-grained phases without leaking unstable internal
  detail to clients.
- Cancellation is real: `cancel(task_id)` must set state, notify waiters, be
  observed by workers, and prevent later completion from overwriting cancelled
  state.
- The task supervisor owns registry, active/completed views, per-session
  filtering, and restart recovery.
- Terminal task states must be append-persisted before being exposed to
  clients.
- Orphaned running tasks after restart must become explicit failed/orphaned
  states, not silently remain running.
- Structured output and artifact delivery must be tied to task IDs.
- Failure recovery must be bounded and visible.
- Tool concurrency classes must prevent unsafe parallel writes.

### Definition Of Done

- A cancelled task cannot later become completed because a worker finished late.
- `/tasks`, `check_background_tasks`, or equivalent surfaces report the same
  truth as the supervisor.
- Crash, restart, reconnect, and backpressure tests preserve terminal task
  state.
- Long-running tasks remain inspectable without reading process logs.

## M9: AppUI Protocol And Coding UX

M9 owns the protocol and client-facing coding experience.

### Requirements

- AppUI is the only supported client contract for TUI and app clients.
- AppUI exposes typed session, turn, approval, diff preview, task update, task
  output, warning, replay-lossy, and terminal events.
- Durable ledger replay preserves event order and never applies stale disk
  snapshots over newer live session state.
- Backpressure must not drop terminal state, approvals, decisions,
  cancellations, or durable task output deltas.
- Approval UX supports approve once, approve scope/session, deny, cancellation,
  typed risk, and durable decision replay.
- Diff preview resolves paths relative to session cwd and is durable enough for
  post-reconnect inspection.
- Task output supports cursorable reads. If only snapshot projection is
  available, the protocol must say so explicitly.
- TUI and app render markdown, user messages, task cards, activity cards,
  status rows, approvals, diff previews, and long output collapse/expand
  consistently.
- Slash and control surfaces such as `/ps`, `/stop`, task cancel, task output
  read, and approval actions map to real protocol commands.
- UX parity is judged against real Codex/Claude-style long-running coding
  sessions, not toy demos.

### Definition Of Done

- A real multi-hour coding session can be resumed and inspected.
- Terminal states never leave UI stuck on running.
- The user can tell what is running, what finished, what failed, what is
  blocked, and what needs approval.
- TUI and app remain decoupled from M8/M9 internals as long as AppUI does not
  change.

## Implementation Quality Scorecard

Use this rubric for every feature.

| Score | Meaning |
| --- | --- |
| 0 | Not implemented. |
| 1 | Type or schema exists only. |
| 2 | Fixture or demo path works. |
| 3 | Production path is wired. |
| 4 | Durable under reconnect, restart, and backpressure. |
| 5 | Fully observable, controllable, tested, and client-rendered. |

A milestone item should not be called complete below score 4. Coding UX-facing
items should require score 5.

## Current Main Gauge

This section captures the reviewer baseline from `origin/main` at
`119bf782`.

| Area | Status | Notes |
| --- | --- | --- |
| Task creation/spawn | Complete | `SpawnTool` registers background tasks through `TaskSupervisor`. |
| Task IDs | Complete | Task IDs are generated and persisted. |
| Task lifecycle | Complete | Lifecycle includes queued, running, verifying, ready, failed, and cancelled. |
| AppUI cancelled mapping | Complete | `TaskRuntimeState::Cancelled` is present and mapped. |
| Task cancellation API | Implemented, needs race hardening | `TaskSupervisor::cancel(task_id)` exists; reviewers should verify cancelled tasks cannot later be overwritten by late worker completion. |
| Terminal task delivery under backpressure | Complete | Terminal updates are retried through awaited send with timeout. |
| Structured task output | Partial | `task/output/read` supports snapshot projection. |
| Stdout/stderr live-tail | Missing | Requires disk-routed output stream plus cursorable server read path. |
| Background task registry | Complete | Supervisor plus `check_background_tasks` provide task inspection. |
| Task-state replay/durability | Complete | Task snapshots are append-persisted and replayed. |
| Diff preview replay/durability | In progress | Must preserve preview data across reconnect/restart. |
| In-memory approval/scope/cwd stores | Outstanding | Requires explicit persistence or recovery semantics. |
| TUI `/ps`, `/stop`, full Codex-style activity labels | Outstanding | Client-side M9 UX work. |

## Review Guidance

Reviewers should avoid scoring by source presence alone. The required question
is always:

1. Is the feature represented in the contract?
2. Is it wired into the production runtime path?
3. Is it durable under restart, reconnect, and backpressure?
4. Is it visible and controllable from the operator/client surface?
5. Is the behavior covered by tests that exercise the real path?

Any answer below score 4 should be reported as incomplete for harness runtime
features. Any coding UX feature below score 5 should remain open.
