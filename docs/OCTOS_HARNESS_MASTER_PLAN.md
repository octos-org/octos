# Octos Harness Master Plan

This document is the durable control document for the harness track.

Its purpose is not to justify endless refactoring. Its job is to preserve the
ordered phases, the cut lines, and the current release logic if chat context is
compacted or a machine crashes.

## Why The Harness Exists

The harness layer exists to solve concrete product failures:

- long-running background tasks can surface false success
- reload can lose final background deliverables
- UI recovery for background work can be flaky
- freeform agent chat still needs stronger guardrails for coding/debugging work

The harness program should therefore be phased and user-benefit driven.

## Ordered Phases

### Phase 1: Background Task Trust

Goal:

- make long-running background child-session tasks trustworthy enough to ship

Required work:

- `#436`: workspace/contract failure must block terminal success
- minimal `#437`: final deliverable selection must come from contract truth
  instead of filename heuristics

Foundation to reuse, not expand:

- `#434`
- `#438`
- `#439`

Verification tied to this phase:

- `#413` immediate canary sanity
- `#415` reload/recovery, slides/site, and background-research live checks
- `#416` operator-summary/log verification only

### Phase 2: Web Persistence And Reload Reliability

Goal:

- make the current `octos-web` persistent-state layer reliable enough for
  long-running task recovery on the public canary

Rule:

- do not patch `octos-web` unless a concrete post-deploy gate proves a single
  web blocker

### Phase 3: Freeform Coding And Richer Harness Runtime

Goal:

- improve freeform coding behavior and expand the harness layer after the
  background-task path is already trustworthy

This later phase includes work such as:

- `#414`
- coding-harness expansion under `#439`
- `#435` unified lifecycle hooks
- richer `#437` artifact APIs

## Contract Persistence Requirement

Release-critical harness state must not live only in prompt text or chat
context.

For any supervised background workflow, the contract must be durable in runtime
state that survives:

- chat compaction
- process restart
- host restart
- actor crash
- partial replay/recovery

At minimum, that means using durable session/task state such as:

- session JSONL metadata
- child-session contract records
- task supervisor persistence
- topic-scoped persisted session messages

## Current Control Documents

The current tracked control docs are:

- `docs/OCTOS_RELEASE_CONTRACT_2026-04-17.md`
- `docs/OCTOS_RUNTIME_PHASE3_CONTRACT.md`
- `docs/OCTOS_RUNTIME_PHASE2.md`
- `docs/OCTOS_RUNTIME_PHASE3.md`
- `docs/OCTOS_HARNESS_M4_WORKSTREAMS_2026-04-21.md`

## Hard Rules

1. Sync to `origin/main` before release work.
2. Never continue a release from a stale dirty local `main`.
3. Every change must map to:
   - the exact issue
   - the exact user-visible benefit
   - the exact proving test
4. No "while I am here" refactors during a release slice.
5. Browser truth comes from the public canary, not raw backend ports.

## Current Status

Completed harness implementation slice:

- canary background artifact/reload reliability for slides/site/background flows
- coding/debugging loop hardening
- hard-case live acceptance coverage
- workspace policy v1
- generic lifecycle hooks
- completion gating
- policy-driven artifact truth and delivery
- deterministic spawn-only lifecycle states
- first-party slides/sites harness templates
- first-party slides/sites contract ownership moved out of kernel paths

Closed implementation issues:

- `#414`
- `#415`
- `#433`
- `#434`
- `#435`
- `#436`
- `#437`
- `#438`
- `#439`
- `#441`

Still-open program controls:

- `#412`: broad Phase 3 umbrella
- `#413`: canary soak and regression triage
- `#416`: operator dashboard beyond the current summary/runtime data

Remaining gap:

- the harness runtime exists and first-party flows prove it, but the
  developer-facing custom app platform still needs progress ABI productization,
  starter apps, validator runner tooling, third-party compatibility gates,
  operator dashboard coverage, and explicit ABI versioning.

The next work should follow
`docs/OCTOS_HARNESS_M4_WORKSTREAMS_2026-04-21.md`, not restart exploratory
refactoring or reopen completed Phase 3 implementation issues.
