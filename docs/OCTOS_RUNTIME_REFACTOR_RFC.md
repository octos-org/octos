# Octos Runtime Refactor RFC

## Goal

Refactor Octos into a hybrid runtime that keeps free-form chat flexibility while making long-running, deliverable-heavy work reliable.

The target system combines:

- Claude Code style loop governance for long free-form work
- Hermes style child-agent lifecycle and delegation containment
- Octos style artifact verification and durable session delivery
- a new workflow runtime only for task families that need guaranteed deliverables

This is not a generic rewrite. It is a feature-driven runtime refactor focused on the failure classes Octos actually has today:

- long turns that drift, overflow, or silently timeout
- prompt-only spawned workers that over-research or wander
- child work that finishes without strong parent/result semantics
- deliverable workflows that need a stricter definition of `done`

## Non-Goals

- replacing all free-form chat with workflow graphs
- making the LLM generate arbitrary workflow DSL for every task
- rewriting the whole channel/gateway stack
- removing `spawn`, `run_pipeline`, or the existing workspace contract model

## Core Design Principles

1. The loop runtime governs free-form work.
2. The workflow runtime governs deliverable-heavy work.
3. The session actor remains the only authority for user-visible terminal results.
4. Workspace contracts verify artifacts, but do not own delivery.
5. The model may propose plans, but the runtime owns budgets, retries, and completion rules.

## Target Architecture

```text
User Intent
   |
   v
Intent Router
   |-------------------------------> Free-Form Lane
   |                                   |
   |                                   v
   |                            Loop Governor
   |                          (turn state, budgets,
   |                           compaction, retries,
   |                           tool arbitration)
   |
   |-------------------------------> Structured Lane
                                       |
                                       v
                                 Workflow Runtime
                              (typed phases, limits,
                               workflow-local state,
                               retries, checkpoints)
                                       |
                                       v
                                Workspace Contract
                             (artifact verification only)
                                       |
                                       v
                                 Session Result Ledger
                           (persist terminal result first,
                            emit committed event second)
                                       |
                                       v
                                     Web/UI
```

## Runtime Layers

### 1. Loop Governor

Used for:

- debugging loops
- long repo refactors
- coding tasks that cannot be cleanly compiled into a workflow
- open-ended assistant work

Responsibilities:

- explicit turn state
- token and tool-result budgets
- compaction across long sessions
- serial vs concurrent tool partitioning
- transient error recovery
- activity-based timeout policy

The loop governor is the Claude Code style part of the design.

### 2. Subagent Runtime

Used for:

- parallel exploration
- delegated coding slices
- background work that should report back into a parent session

Responsibilities:

- child session creation
- parent-child linkage
- child runtime policy
- child result envelope
- background completion notifications
- bounded child result persistence

The subagent runtime is the Hermes style part of the design.

### 3. Workflow Runtime

Used only for task families where the output has a hard deliverable:

- `research_report`
- `research_podcast`
- `slides`
- `site`
- future file/media/report workflows

Responsibilities:

- typed workflow instances
- named phases
- phase-specific tool allowlists
- per-phase budgets
- workflow-local scratch state
- retry/abort rules
- phase validators

The workflow runtime is not generic graph execution for everything. It is a bounded runtime for known workflow families.

### 4. Workspace Contract

Responsibilities:

- verify output truth
- resolve expected artifacts
- run validators

It must not directly define user-visible completion.

### 5. Session Result Ledger

Responsibilities:

- persist terminal assistant result
- attach media/files to the persisted message
- emit a committed session event after the write succeeds
- let all clients rebuild from committed event truth

This is the completion authority.

## Strong Invariants

1. A background task is not complete when a worker exits.
2. A background task is complete only when the session actor has durably committed a terminal result.
3. Deliverable workflows are not successful because the model says they are done.
4. Deliverable workflows are successful only when their artifacts pass contract verification.
5. Free-form work should never be forced into a workflow family only to gain structure.

## Concrete Runtime Types

### Loop Governor

```rust
struct TurnState {
    turn_id: Uuid,
    session_id: SessionKey,
    turn_count: u32,
    task_budget_remaining: Option<u32>,
    max_tool_result_bytes: usize,
    compact_boundary: Option<u64>,
    pending_tool_summary: Option<String>,
    active_tools: Vec<String>,
}
```

### Subagent Runtime

```rust
struct ChildRun {
    child_session_id: SessionKey,
    parent_session_id: SessionKey,
    delegation_id: Uuid,
    purpose: String,
    lane: ChildLane,
    status: ChildRunStatus,
}
```

### Workflow Runtime

```rust
enum WorkflowKind {
    ResearchReport,
    ResearchPodcast,
    Slides,
    Site,
}

struct WorkflowInstance {
    id: Uuid,
    kind: WorkflowKind,
    phase: WorkflowPhase,
    state_path: PathBuf,
    scratch_dir: PathBuf,
    limits: WorkflowLimits,
    artifacts: ArtifactManifest,
}
```

## Why Not Raw DOT Or Pregel Everywhere

DOT is useful for authoring and visualization, but not enough as runtime truth. It does not define:

- retries
- checkpoints
- budgets
- completion semantics
- delivery semantics

Pregel-style graph engines are too heavy for Octos’s main workflow needs. Most Octos workflows are small phase machines, not distributed graph workloads.

The recommended model is:

- typed workflow runtime in Rust
- optional declarative phase specs in TOML/YAML/DOT as an authoring layer
- runtime-owned execution semantics

## Priority Workstreams

### Workstream A: Loop Governor Core

Scope:

- explicit turn-state object
- task-budget plumbing
- turn lifecycle transitions
- activity heartbeat and idle timeout hooks

Owned files:

- `crates/octos-agent/src/agent/*`
- `crates/octos-cli/src/session_actor.rs`

Parallel safety:

- do not edit workflow or web code in this stream

### Workstream B: Long-Context Compaction

Scope:

- tool-result budget
- context compaction boundaries
- persisted compact markers
- bounded summaries for oversized tool output

Owned files:

- `crates/octos-agent/src/agent/*`
- `crates/octos-agent/src/tools/*` where oversized results are emitted

Parallel safety:

- do not edit session event/web delivery code

### Workstream C: Child Session Runtime

Scope:

- parent-child session model
- child run metadata
- completion notification path
- child result persistence

Owned files:

- `crates/octos-agent/src/tools/spawn.rs`
- `crates/octos-cli/src/session_actor.rs`
- `crates/octos-bus/src/session.rs`

Parallel safety:

- do not change workflow runtime internals in this stream

### Workstream D: Session Event Ledger

Scope:

- resumable event feed
- committed event model
- web projections from ordered events

Owned files:

- `crates/octos-bus/src/session.rs`
- `crates/octos-bus/src/api_channel.rs`
- `octos-web/src/runtime/*`
- `octos-web/src/store/*`

Parallel safety:

- no loop-governor or workflow internals here

### Workstream E: Workflow Runtime Core

Scope:

- `WorkflowKind`
- `WorkflowInstance`
- `WorkflowPhase`
- typed limits and state
- phase executor

Owned files:

- new `crates/octos-agent/src/workflow/*`
- workflow-related integration points in `session_actor`

Parallel safety:

- no web code
- no child-session delivery rewrites

### Workstream F: Contract Engine V2

Scope:

- shared action engine
- richer validators
- policy-driven enforcement

Owned files:

- `crates/octos-agent/src/behaviour.rs`
- `crates/octos-agent/src/workspace_contract.rs`
- `crates/octos-agent/src/workspace_policy.rs`

Parallel safety:

- no workflow phase logic beyond generic hooks

### Workstream G: Workflow Family Implementations

Scope:

- `research_report`
- `research_podcast`
- `slides`
- `site`

Owned files:

- workflow family modules only
- family-specific prompt/tool orchestration

Parallel safety:

- one agent per family

### Workstream H: Live Smoke / Soak CI

Scope:

- live browser canaries
- long-task soak suites
- delivery and ordering invariants

Owned files:

- `octos-web/tests/*`
- `docs/LIVE_BROWSER_SMOKE_TESTS.md`
- CI workflow files

Parallel safety:

- no runtime logic changes

## Feature-Driven Issue Map

Each issue below should be assigned a narrow write set, explicit tests, and a single acceptance invariant.

### Issue 1: Loop Governor State Machine (#393)

Outcome:

- long free-form turns use explicit `TurnState`

Acceptance tests:

- long debug/refactor turn survives multiple tool cycles without losing budget state
- turn state is preserved across compaction boundaries

### Issue 2: Tool Result Budget And Compaction (#394)

Outcome:

- oversized tool results are summarized or externalized instead of poisoning the live context

Acceptance tests:

- giant terminal output no longer causes follow-up tool-call degradation
- compacted sessions still preserve enough tool history to continue

### Issue 3: Activity-Based Timeout Policy (#402)

Outcome:

- active long-running work is not killed by wall-clock timeout alone

Acceptance tests:

- a long but active pipeline survives
- an idle wedged turn times out with a durable failure result

### Issue 4: Parent/Child Session Runtime (#395)

Outcome:

- child runs are first-class linked sessions with parent result semantics

Acceptance tests:

- child completion appears in the parent session
- child failure appears durably in the parent session

### Issue 5: Resumable Session Event Ledger (#389, #390, #391, plus #384)

Outcome:

- web and other clients consume ordered session events instead of reconstructing truth from separate polling surfaces

Acceptance tests:

- reload during background completion still hydrates the final result
- no duplicate terminal media bubble after reconnect

### Issue 6: Contract Engine V2 (#396)

Outcome:

- one shared action engine powers workspace checks and spawn-task verification

Acceptance tests:

- the same validator behaves identically in both project and session contexts
- missing configured contract fails deterministically

### Issue 7: Workflow Runtime Core (#397)

Outcome:

- known deliverable-heavy tasks run as typed workflow instances

Acceptance tests:

- workflow phase transitions are explicit and persisted
- retries do not skip required phases

### Issue 8: Research Report Workflow (#398)

Outcome:

- deep research becomes a bounded workflow instead of open-ended worker prompting

Acceptance tests:

- final report always lands durably or fails durably
- research budget is enforced

### Issue 9: Research Podcast Workflow (#399)

Outcome:

- `research -> script -> podcast_generate -> deliver` is runtime-owned

Acceptance tests:

- exactly one final MP3 is delivered
- intermediate research/script files are not sent to chat
- `podcast_generate` failure stops fallback audio spam

### Issue 10: Slides And Site Workflows (#400)

Outcome:

- slides/site deliverables are backed by typed workflow families and contracts

Acceptance tests:

- output deck/site is verified before completion
- template-specific build output paths are respected

### Issue 11: Live Browser Long-Task Smoke Suite (#401)

Outcome:

- CI catches delivery/order/reload regressions for long workflows

Acceptance tests:

- deep research reload durability
- research podcast final audio delivery
- no duplicate attachments
- question-before-answer ordering

## Parallel Agent Strategy

This refactor is explicitly designed for parallel YOLO-mode execution.

Rules:

- one issue per agent
- one bounded write set per issue
- cross-stream integration only through reviewed contracts and tests
- no agent edits `session_actor`, `api_channel`, and `workflow/*` in the same issue unless that issue is explicitly an integration issue

Recommended lane assignment:

- Agent 1: Issue 1
- Agent 2: Issue 2
- Agent 3: Issue 3
- Agent 4: Issue 4
- Agent 5: Issue 5
- Agent 6: Issue 6
- Agent 7: Issue 7
- Agent 8: Issue 8
- Agent 9: Issue 9
- Agent 10: Issue 10
- Agent 11: Issue 11

## Test Strategy

### Unit Tests

- state transitions
- contract validators
- child run metadata
- event stream sequencing

### Integration Tests

- session actor + child completion
- workflow phase persistence
- contract verification + session result persistence

### Browser/Live Tests

- reload during running task
- final media hydration
- duplicate attachment prevention
- cross-session tracker correctness

### Soak Tests

- 5x deep research live matrix
- 5x research podcast live matrix
- long idle vs active timeout matrix

## Migration Order

1. Land loop governor primitives.
2. Land child session runtime.
3. Land event-ledger projections.
4. Land contract engine unification.
5. Land workflow runtime core.
6. Move `research_report` and `research_podcast` first.
7. Move `slides` and `site` next.
8. Expand live smoke coverage.

## Merge Policy

Only merge workstreams that satisfy both:

- their own acceptance tests
- no regression in live browser smoke for background result delivery

This is the critical discipline for avoiding “fixed A, broke B” during the refactor.
