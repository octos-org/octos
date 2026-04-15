# Octos Runtime Refactor Execution Plan

## Purpose

This document turns the runtime RFC into a concrete implementation program that can be executed by
parallel agents with minimal merge pain.

It is intentionally feature-driven:

- each lane owns a narrow code surface
- each lane has a required regression test boundary
- each lane has a merge dependency rule
- no lane is allowed to fix one failure class by quietly expanding into another subsystem

## Planning Baseline

- Architecture baseline: `docs/OCTOS_RUNTIME_REFACTOR_RFC.md`
- Review-backed issue map: `#384`, `#385`, `#389`-`#402`

## Global Rules

1. One lane, one owner.
2. No lane expands into another lane's primary files without an explicit handoff.
3. Every lane must land tests for the failure class it fixes.
4. Deliverable-heavy behavior is not considered correct until the terminal result is durable.
5. Workflow-family lanes must not reintroduce prompt-only orchestration as a shortcut.

## Merge Order

1. `#393` Loop governor state machine
2. `#394` Tool-result budget and compaction
3. `#402` Activity-based timeout policy
4. `#384` Persist background task state across restarts
5. `#385` Persist long-turn timeout/error results
6. `#395` Child-session runtime
7. `#396` Contract engine v2
8. `#389` Resumable per-session event feed
9. `#390` Route report outputs through committed result path
10. `#391` Topic-aware committed session events
11. `#397` Workflow runtime core
12. `#398` `research_report` workflow
13. `#399` `research_podcast` workflow
14. `#400` slides/site workflow families
15. `#401` live browser long-task smoke suite

## Parallel Lane Map

### Lane A

- Issue: `#393`
- Branch: `runtime/393-loop-governor-state-machine`
- PR title: `Add typed loop governor state machine for long free-form turns`
- Primary ownership:
  - `crates/octos-agent/src/agent/loop_runner.rs`
  - `crates/octos-agent/src/agent/mod.rs`
  - new helper modules under `crates/octos-agent/src/agent/`
- Must not touch:
  - `session_actor.rs`
  - `workspace_contract.rs`
  - `octos-web/*`
- Required tests:
  - long tool-heavy turn preserves message/tool ordering
  - explicit terminal reason is returned on budget stop

### Lane B

- Issue: `#394`
- Branch: `runtime/394-tool-result-budget-compaction`
- PR title: `Extract tool-result budget and compaction subsystem`
- Primary ownership:
  - `crates/octos-agent/src/agent/loop_runner.rs`
  - new compaction/budget helper modules
- Must not touch:
  - session result delivery
  - workflow routing
- Required tests:
  - deterministic truncation of oversized tool outputs
  - repaired tool chains remain valid after compaction

### Lane C

- Issue: `#402`
- Branch: `runtime/402-activity-based-timeouts`
- PR title: `Add activity-based timeout policy for long-running turns`
- Primary ownership:
  - loop-governor timeout hooks
  - child-session timeout hooks once available
- Must not touch:
  - web stores
  - workflow family logic
- Required tests:
  - active long run does not timeout on wall clock alone
  - idle wedged run still times out durably

### Lane D

- Issue: `#384`
- Branch: `runtime/384-persist-task-state`
- PR title: `Persist background task state and terminal results across restarts`
- Primary ownership:
  - `crates/octos-agent/src/task_supervisor.rs`
  - `crates/octos-bus/src/session.rs`
  - minimal persistence hooks in `crates/octos-cli/src/session_actor.rs`
- Must not touch:
  - workflow families
  - `octos-web/*`
- Required tests:
  - task state survives restart
  - completed/failed task truth can be rebuilt from persisted state

### Lane E

- Issue: `#385`
- Branch: `runtime/385-persist-long-turn-failures`
- PR title: `Persist long-turn timeout and error results to session history`
- Primary ownership:
  - `crates/octos-cli/src/session_actor.rs`
- Must not touch:
  - workflow routing
  - task supervisor persistence
- Required tests:
  - timeout is durable in session history
  - reload reconstructs real terminal failure

### Lane F

- Issue: `#395`
- Branch: `runtime/395-child-session-runtime`
- PR title: `Promote delegated runs into a real child-session runtime`
- Primary ownership:
  - `crates/octos-agent/src/tools/spawn.rs`
  - `crates/octos-agent/src/task_supervisor.rs`
  - `crates/octos-cli/src/session_actor.rs`
  - `crates/octos-bus/src/session.rs`
- Depends on:
  - `#384`
- Must not touch:
  - contract validators
  - workflow-family policies
- Required tests:
  - child session survives parent reconnect
  - parent gets exactly one terminal child outcome

### Lane G

- Issue: `#396`
- Branch: `runtime/396-contract-engine-v2`
- PR title: `Unify contract enforcement and workspace inspection`
- Primary ownership:
  - `crates/octos-agent/src/workspace_contract.rs`
  - `crates/octos-agent/src/behaviour.rs`
  - `crates/octos-agent/src/workspace_git.rs`
  - `crates/octos-agent/src/workspace_policy.rs`
- Must not touch:
  - web event/store code
  - prompt-based workflow routing
- Required tests:
  - one validator behaves identically in inspection and runtime enforcement
  - verified output resolution no longer double-sends files

### Lane H

- Issue: `#389`
- Branch: `runtime/389-session-event-ledger`
- PR title: `Add resumable per-session event feed and projection-based web stores`
- Primary ownership:
  - `crates/octos-bus/src/session.rs`
  - `crates/octos-bus/src/api_channel.rs`
  - `octos-web/src/runtime/*`
  - `octos-web/src/store/*`
- Must not touch:
  - workflow-family behavior
  - loop-governor internals
- Required tests:
  - session truth can be rebuilt from the event feed
  - fixed-number post-completion polling is no longer a correctness dependency

### Lane I

- Issue: `#390`
- Branch: `runtime/390-committed-report-results`
- PR title: `Route background report outputs through committed session results`
- Primary ownership:
  - `crates/octos-cli/src/session_actor.rs`
  - `crates/octos-bus/src/api_channel.rs`
- Must not touch:
  - generic loop-governor logic
  - unrelated workflow routing
- Required tests:
  - report outputs appear exactly once
  - reload shows the same report result without legacy notification timing

### Lane J

- Issue: `#391`
- Branch: `runtime/391-topic-aware-session-events`
- PR title: `Make committed background session events topic-aware across surfaces`
- Primary ownership:
  - `crates/octos-bus/src/session.rs`
  - `crates/octos-bus/src/api_channel.rs`
  - `octos-web/src/runtime/*`
- Must not touch:
  - low-level loop-governor work
  - workflow-family logic except event-topic identity
- Required tests:
  - topic-scoped sessions and non-chat surfaces use the same event truth model

### Lane K

- Issue: `#397`
- Branch: `runtime/397-workflow-runtime-core`
- PR title: `Add workflow runtime core for deliverable-heavy task families`
- Primary ownership:
  - `crates/octos-cli/src/session_actor.rs`
  - new workflow runtime modules
- Depends on:
  - `#395`
  - `#396`
- Must not touch:
  - loop compaction internals
  - web optimistic reconciliation
- Required tests:
  - workflow phase changes persist
  - phase-owned terminal conditions prevent partial-success leakage

### Lane L

- Issue: `#398`
- Branch: `runtime/398-research-report-workflow`
- PR title: `Implement bounded research_report workflow family`
- Primary ownership:
  - workflow runtime core
  - deep research integration points
- Must not touch:
  - podcast-specific delivery semantics
- Required tests:
  - exactly one report success or one durable failure
  - scratch artifacts are not delivered as final outputs

### Lane M

- Issue: `#399`
- Branch: `runtime/399-research-podcast-workflow`
- PR title: `Implement runtime-owned research_podcast workflow family`
- Primary ownership:
  - workflow runtime core
  - `crates/octos-cli/src/session_actor.rs`
  - podcast/tool integration points
- Must not touch:
  - generic site/slides logic
- Required tests:
  - no duplicate MP3 delivery
  - no `_report.md` leakage
  - `podcast_generate` failure cannot fall through into partial TTS success paths

### Lane N

- Issue: `#400`
- Branch: `runtime/400-slides-site-workflows`
- PR title: `Implement slides and site workflow families with template-aware deliverables`
- Primary ownership:
  - `crates/octos-agent/src/workspace_policy.rs`
  - project template metadata integration
  - workflow runtime integration only where needed
- Must not touch:
  - research-family budgets and heuristics
- Required tests:
  - site templates validate correct output directory
  - slide generation yields one verified deliverable outcome

### Lane O

- Issue: `#401`
- Branch: `runtime/401-live-browser-smoke-suite`
- PR title: `Add live browser smoke suite for long-running task regressions`
- Primary ownership:
  - `octos-web/tests/*`
  - supporting test helpers
- Must not touch:
  - runtime code unless a minimal test hook is strictly required
- Required tests:
  - short TTS renders exactly one audio attachment
  - long research reload does not synthesize bogus turns
  - research podcast yields exactly one final audio attachment after reload

## Recommended First Dispatch

Start these three lanes first because they have the lowest cross-lane overlap:

1. Lane A `#393`
2. Lane D `#384`
3. Lane G `#396`

This gives the program early progress on:

- free-form loop governance
- durable task state
- contract-engine cleanup

without immediately colliding on the same files.

## Required Test Discipline

Every PR must include:

- one focused unit/integration regression test for its primary failure mode
- one short “side effects avoided” note in the PR description
- a clear statement of which later lanes now depend on it

## Final Acceptance Gate

Before the overall refactor is considered complete:

- `#399` and `#401` must pass on live browser canaries
- committed session results must be the only terminal delivery authority
- no long-task lane may rely on prompt wording alone for correctness
