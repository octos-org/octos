# Octos Runtime Phase 3 Contract

This document is the execution contract for Phase 3.

Phase 3 is not permission to keep refactoring forever. It exists to finish a
small number of user-visible improvements on top of the Phase 2 foundation.

## User Promise

Phase 3 should produce three concrete outcomes:

1. long-running background tasks are durably supervised and trustworthy
2. reload/recovery in `octos-web` is reliable for long-running background work
3. freeform agent chat, especially coding/debugging work, is more bounded and
   less flaky

If a change does not improve one of those three outcomes, it is probably not
Phase 3 work.

## Hard Constraints

- Do not restart the architecture from scratch.
- Do not widen into endless harness abstraction work before the user-facing
  gates are green.
- Persist release-critical contract state outside prompt context.
- Keep runtime truth in backend/session/task persistence, not in web-only state.
- Keep public canary as the release truth.

## Ordered Phase 3 Sequence

### Phase 3A: Canary Trust And Regression Closure

Primary issues:

- `#412`
- `#413`
- `#436`
- `#437`
- `#438`

Must prove:

- background child-session failures cannot surface as false success
- final deliverables persist and survive reload
- slides/site/background flows are stable on canary

### Phase 3B: Persistent State And Reload Recovery

Primary issues:

- `#413`
- `#415`
- web persistent-state work already underway in `octos-web`

Must prove:

- reload preserves active task state
- reload preserves final artifact/preview visibility
- topic/session history does not bleed across surfaces
- no ghost or empty turns after reload

### Phase 3C: Freeform Coding Guardrails

Primary issues:

- `#414`
- `#415`
- later parts of `#439`

Must prove:

- coding/debugging loops do not spiral on shell retries
- repair turns stay bounded
- freeform chat borrows the right guardrails from Claude Code / Hermes style
  coding discipline without breaking normal chat

### Phase 3D: Operator Surface

Primary issue:

- `#416`

Must prove:

- operators can quickly see retries, timeouts, orphaned children, and delivery
  failures from a compact summary
- this surface reflects real child/runtime activity, not only top-level process
  counters

### Phase 3E: Harness Runtime Formalization

Primary issues:

- `#433`
- `#434`
- `#435`
- `#436`
- `#437`
- `#438`
- `#439`

Rule:

- this phase only expands after the earlier user-facing phases are green

## Current Lane Status

Based on the current code/review state:

- `#438` spawn-only lifecycle is mostly landed
- `#434` policy struct exists, but schema/design is only partial
- `#436` gating and durable enforcement were the highest-value missing piece
- `#437` canonical artifact truth is still only partially landed
- `#435` unified lifecycle hooks are still architectural debt, not the first
  release blocker
- `#439` slides/site template work exists, but coding-harness expansion is not
  done

## Explicitly Out Until Earlier Phases Are Green

- broad schema redesign for its own sake
- broad canonical artifact API cleanup across every workflow
- lifecycle unification without an immediate product win
- operator UI polish without operator truth
- unrelated provider/admin/ingress work

## Persistence Contract

Phase 3 state must survive:

- context compaction
- actor crash
- gateway restart
- host restart
- reload/replay

Required durable storage surfaces:

- session JSONL history and metadata
- child-session contract records
- task supervisor persisted state
- topic-scoped file/media/session-result persistence

No critical runtime contract may exist only in:

- prompt wording
- browser memory
- transient tool output
- non-durable actor state

## Acceptance Gate For Phase 3

Phase 3 is complete only when all of the following are true:

1. public canary long-running background flows are trustworthy
2. reload/recovery is green for the targeted long-running workflows
3. freeform coding hard cases have real live acceptance coverage and are green
4. operator summary is good enough to diagnose runtime failures from live data
5. the remaining harness runtime work is tracked as explicit product-backed
   deltas, not open-ended redesign

## Control Rule

When in doubt:

- ship the user benefit first
- persist the contract durably
- prove it on the public canary
- only then widen the harness abstraction
