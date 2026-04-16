# Octos Runtime Phase 2

Phase 1 established the runtime foundation:
- loop-governor scaffolding
- child-session lineage and durable background notes
- workflow runtime core
- workflow families for report, podcast, slides, and site deliverables
- committed session-result delivery
- topic-aware background hydration
- live browser smoke coverage

Phase 2 is the hardening and generalization round. The goal is to make the runtime stronger for open-ended coding/debugging work, more disciplined around child-session supervision, broader in workflow compilation, and easier to observe and regression-test before users hit failures.

## Goals

1. Add runtime observability before changing deeper supervision behavior.
2. Strengthen the loop governor for open-ended coding and debugging sessions.
3. Upgrade child sessions from lineage-only tracking to supervised runtime contracts.
4. Generalize workflow selection into a typed plan compiler and family registry.
5. Expand artifact contracts and live acceptance coverage across more failure modes.

## Hard Constraints

- Keep the runtime modular. Do not create a monolithic workflow/agent/session manager.
- Keep `workflow_runtime` pure: workflow types, planning, compilation, and phase metadata only.
- Keep family-specific policy in family modules, not generic runtime code.
- Keep web projection logic separate from runtime truth and delivery semantics.
- Keep child-session supervision separate from workflow selection logic.

## Phase 2 Lanes

### Epic: Phase 2 Runtime Hardening and Generalization (`#406`)

Tracks the full phase and its acceptance gate.

### Lane A: Runtime Observability (`#407`)

Owns instrumentation and telemetry only.

Primary surfaces:
- `crates/octos-bus/src/session.rs`
- `crates/octos-bus/src/api_channel.rs`
- `crates/octos-cli/src/session_actor.rs`
- `crates/octos-agent/src/tools/spawn.rs`
- `crates/octos-agent/src/task_supervisor.rs`
- `crates/octos-web/src/runtime/*`

Must produce:
- per-session and per-child-session lifecycle events
- workflow phase transition events
- terminal-result reasons
- timeout and retry reasons
- counters for duplicate result writes, replay fallback, topic mismatch, orphaned child sessions

Should avoid touching:
- workflow family policy
- contract validation rules
- generic message-store merge behavior unless required for telemetry plumbing

### Lane B: Loop Governor V2 (`#408`)

Owns open-ended coding/debugging turn supervision.

Primary surfaces:
- `crates/octos-agent/src/agent/*`

Must produce:
- explicit turn-state machine for long coding/debugging loops
- compaction budget accounting attached to runtime state
- structured retry/repair reasons
- stricter per-turn tool batching and caps

Should avoid touching:
- session delivery
- workflow family modules
- web runtime code

### Lane C: Child-Session Supervisor (`#409`)

Owns parent/child supervision and terminal contracts.

Primary surfaces:
- `crates/octos-agent/src/tools/spawn.rs`
- `crates/octos-agent/src/task_supervisor.rs`
- `crates/octos-bus/src/session.rs`
- `crates/octos-cli/src/session_actor.rs`

Must produce:
- structured child terminal states
- parent/child join semantics
- fanout limits and timeout policy
- explicit child retry/escalation rules

Should avoid touching:
- workflow plan compilation
- web-specific topic projection
- contract engine policy beyond terminal envelopes

### Lane D: Workflow Plan Compiler (`#410`)

Owns typed intent classification and workflow compilation.

Primary surfaces:
- `crates/octos-cli/src/workflow_runtime.rs`
- `crates/octos-cli/src/workflow_families/*`
- narrow integration points in `session_actor.rs`

Must produce:
- workflow family registry
- typed plan compiler
- bounded workflow parameters per family
- clean separation between plan selection and execution

Should avoid touching:
- child-session supervision internals
- contract validator internals
- web runtime code

### Lane E: Contracts and Live Acceptance (`#411`)

Owns artifact truth and browser/runtime acceptance coverage.

Primary surfaces:
- `crates/octos-agent/src/workspace_contract.rs`
- `crates/octos-agent/src/workspace_policy.rs`
- `crates/octos-agent/src/behaviour.rs`
- `e2e/*`
- `/Users/yuechen/home/octos-web/tests/*`

Must produce:
- broader artifact truth for multi-file and mixed-media outputs
- failure-mode assertions for reconnect storms, concurrent topics, idle resume, and child-session recovery
- live browser acceptance for additional deliverable families

Should avoid touching:
- loop-governor internals
- workflow compiler internals
- child-session supervision internals unless needed for test hooks

## Acceptance Gate

Phase 2 is complete only when:
- all child lanes land without collapsing into shared monolithic ownership
- live browser smoke and long suites remain green
- new observability makes long-task failures diagnosable from runtime events alone
- coding/debugging loops are measurably more stable under long multi-turn runs
- child-session failures and retries are explicit in runtime state
- workflow selection becomes typed and bounded, not prompt-only family selection
