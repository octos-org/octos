# Octos Runtime Phase 3 Kickoff Contract

This document turns the remaining Phase 3 work into a bounded delivery program.

It is the execution plan for the still-open issue set after the shipped canary
release slice on:

- `octos` branch `phase3/integrator`
- `octos-web` branch `phase3/web-release`

Use this document together with:

- `docs/OCTOS_RUNTIME_PHASE3_CONTRACT.md`
- `docs/OCTOS_RUNTIME_PHASE3.md`
- `docs/OCTOS_HARNESS_DEVELOPER_INTERFACE.md`

## Program Boundary

Phase 3 is not "keep refactoring until the code feels pure".

Phase 3 from this point forward means:

1. close the still-open user-visible reliability gaps
2. finish the still-open coding acceptance and operator surfaces
3. formalize the harness layer only where it directly improves product truth

Out of scope:

- unrelated infra/provider/admin work
- broad runtime rewrites
- new framework layers without an issue-backed user outcome

## Already Delivered

The following slices are already landed in the current Phase 3 release branch:

- contract-backed artifact truth for contract-owned background slides/site runs
- durable `octos-web` reload recovery for long-running tasks
- tighter shell retry bounds and two live coding hard-case proofs
- operator summary source provenance

This kickoff contract is only for the remaining open Phase 3 work.

## Open Issue Inventory

Primary Phase 3 issues:

- `#412` umbrella
  - close only after all child lanes are complete and validated
- `#413` canary soak and regression triage
  - still needs sustained snapshots and closure criteria
- `#414` coding/debugging loop hardening
  - still needs deeper long-round coding fixes, not just the first retry slice
- `#415` coding hard-case acceptance
  - still needs the remaining live cases converted from scaffold to green
- `#416` operator surface
  - summary truth is better, dashboard/page is still open

Harness issues:

- `#433` harness umbrella
- `#434` policy v1 completion
- `#435` lifecycle hooks
- `#436` validator runner and completion gating
- `#437` artifact truth/delivery unification
- `#438` spawn-only verify/complete/failure lifecycle completion
- `#439` first-party harness templates

## Milestones

### M3.1 Soak Closure

Issues:

- `#413`
- `#412`

User value:

- canary is no longer "green once"; it is trustworthy over repeated use

Deliverables:

- repeatable daily or per-run operator snapshots
- concrete regression list with owner and disposition
- explicit closure note for known flakes vs real bugs
- no hidden widening during soak

Exit criteria:

- at least 3 clean canary verification runs recorded
- top recurring regressions triaged into issue comments or child bugs
- no unresolved blocker in slides/site/research/podcast/TTS reload paths

### M3.2 Coding Guardrails Completion

Issues:

- `#414`
- `#415`

User value:

- long coding/debugging sessions are less chaotic and more trustworthy

Deliverables:

- bounded repair-turn planner for longer shell/test/edit loops
- stronger recovery across long idle/reconnect paths
- bounded child-session fanout/join policy for coding work
- remaining live coding hard-case cases turned into real assertions

Exit criteria:

- `coding-hardcases.spec.ts` has no remaining `fixme` cases for the agreed
  Phase 3 set
- live canary proves:
  - failing test repaired in the same session
  - child-session fanout/join stays bounded
  - long idle resume preserves the same coding turn
  - concurrent coding sessions stay isolated under load

### M3.3 Operator Surface Completion

Issues:

- `#416`

User value:

- operators can diagnose live runtime failures without scraping raw metrics

Deliverables:

- compact admin UI or operator page backed by the same summary JSON
- grouping/filtering for retries, timeouts, child sessions, validation failures,
  delivery failures
- no second truth path; UI must read the same summary contract

Exit criteria:

- operator can identify which runtime/profile/source failed
- operator can see retries/timeouts/duplicate suppression/child issues without
  digging through raw `/metrics`

### M3.4 Harness Runtime Completion

Issues:

- `#433`
- `#434`
- `#435`
- `#436`
- `#437`
- `#438`
- `#439`

User value:

- background-work correctness no longer depends on prompt obedience or scattered
  heuristics

Deliverables:

- policy v1 reaches a stable, documented, additive schema
- lifecycle hooks exist for turn-end, resume, spawn verify/complete/failure
- validator outcomes are persisted and can gate required completion
- send-file/reload/completion share the same artifact truth
- first-party slides/sites templates emit the richer harness defaults

Exit criteria:

- required validators can block finalization
- spawn-only failures can steer correction instead of only logging text
- slides and sites bootstrap with explicit first-party harness policy

### M3.5 Developer Harness Interface

Issues:

- `#433`
- `#434`
- `#435`
- `#436`
- `#437`
- `#438`
- `#439`

User value:

- customer developers get a clear contractual API for building skills/apps on
  Octos instead of relying on prompt conventions and hidden runtime behavior

Deliverables:

- a documented abstract harness interface for customer skills/apps
- clear mapping from developer concepts to runtime concepts:
  - workspace
  - artifacts
  - validation
  - background tasks
  - lifecycle hooks
  - operator truth
- first-party slides/sites/TTS examples that demonstrate the interface

Exit criteria:

- the developer-facing harness story is documented and aligned with real runtime
  behavior
- first-party harnessed workflows use the same contract shape we want customer
  developers to use
- Octos can clearly position itself as an execution OS with a contractual app
  interface rather than a loosely-defined agent shell

## Milestone Order

The required order is:

1. `M3.1` soak closure
2. `M3.2` coding guardrails completion
3. `M3.3` operator surface completion
4. `M3.4` harness runtime completion
5. `M3.5` developer harness interface completion

`M3.4` can progress in parallel in design/spikes, but it does not take release
priority over `M3.1` to `M3.3`.

`M3.5` documentation and product positioning can progress in parallel with
`M3.4`, but it does not count as complete until the runtime contract it
describes is actually true.

## Branch Strategy

`octos`:

- integration branch: `phase3/integrator`
- per-lane branches should fork from the current integration head

`octos-web`:

- integration branch: `phase3/web-release`
- per-lane branches should fork from the current integration head

Rules:

- one integrator branch per repo
- one owner per write scope
- no direct edits on dirty local `main`

## Subagent Lanes

### Integrator

Owner:

- main controller

Responsibilities:

- maintain scope
- merge lane commits
- run deploys
- decide release go/no-go

Write scope:

- integration branches only

### Kierkegaard

Role:

- canary browser gate and soak verification

Responsibilities:

- run public canary Playwright suites
- capture exact repro steps for failures
- distinguish stable regressions from likely proxy/test flakes

Write scope:

- none by default

Outputs:

- pass/fail command log
- failing test names
- precise user-visible repro notes

### Anscombe

Role:

- host verification and runtime log triage

Responsibilities:

- verify deployed binary/version/asset hashes on canary-serving hosts
- inspect service health and logs
- confirm whether failures are host/runtime/proxy related

Write scope:

- none by default

Outputs:

- host reachability
- binary SHA/version
- service state
- concrete log evidence

### Halley

Role:

- coding runtime and hard-case lane

Responsibilities:

- `#414` runtime hardening in backend
- `#415` live coding acceptance cases

Primary files:

- `crates/octos-agent/src/agent/*`
- `crates/octos-cli/src/session_actor.rs`
- `e2e/tests/coding-hardcases.spec.ts`
- `e2e/tests/runtime-regression.spec.ts`

Outputs:

- one bounded coding-runtime patch at a time
- matching targeted tests

### Wegener

Role:

- operator surface lane

Responsibilities:

- complete `#416`
- preserve the existing summary endpoint as the only truth source

Primary files:

- `crates/octos-cli/src/api/admin.rs`
- `crates/octos-cli/src/api/metrics.rs`
- `crates/octos-cli/src/commands/admin.rs`
- admin/operator UI surfaces that consume the same JSON

Outputs:

- operator UX improvements tied directly to summary truth

### Mendel

Role:

- harness/runtime lane

Responsibilities:

- `#434` to `#439`
- keep harness work additive and issue-backed

Primary files:

- `crates/octos-agent/src/workspace_policy.rs`
- `crates/octos-agent/src/workspace_git.rs`
- `crates/octos-agent/src/tools/spawn.rs`
- `crates/octos-agent/src/hooks.rs`
- `crates/octos-cli/src/workspace_contract.rs`
- `crates/octos-cli/src/workflows/*`

Outputs:

- one harness slice per issue
- parser/runtime/tests together

### Maxwell

Role:

- `octos-web` persistence and operator UI consumer lane

Responsibilities:

- reload/recovery follow-through
- operator page that consumes backend summary truth

Primary files:

- `src/store/message-store.ts`
- `src/runtime/*`
- `src/components/*`
- `tests/session-recovery.spec.ts`

Outputs:

- web persistence/operator patches only
- browser or typecheck proof for each patch

## Lane Rules

- no two agents may edit the same file set in parallel
- every lane must tie each patch to one issue number and one proving test
- if a lane discovers unrelated drift, it reports it instead of widening scope
- browser and host verification lanes stay read-only unless the integrator
  explicitly reassigns ownership

## Kickoff Checklist

1. sync both integration branches to latest remote
2. record current canary SHA, asset hash, and operator snapshot
3. re-open only the still-open Phase 3 issues in the tracker view
4. assign each subagent to exactly one lane
5. create per-lane clean worktrees
6. define the first milestone cut line before coding starts

Kickoff is blocked if:

- canary truth host is unclear
- integration branches are behind remote
- lane ownership overlaps

## Acceptance Gates

Each milestone must pass:

- targeted unit/integration tests for touched code
- deploy to public canary
- browser/runtime verification on `https://dspfac.crew.ominix.io`
- host verification on canary-serving hosts
- written milestone closure note in the issue or contract doc

## Closure Rules

Issue closure should follow this order:

1. child issue code lands
2. canary validation is green
3. contract doc records the delivered slice
4. issue is closed with the proving commands/results

Umbrellas close last:

- `#433` closes after `#434` to `#439`
- `#412` closes after `#413` to `#416`

## Final Definition Of Done

Phase 3 is truly done only when:

- the remaining open primary issues are closed
- the remaining open harness issues are closed or explicitly deferred out of
  Phase 3
- the public canary proves the user-facing coding/background/reload/operator
  promises
- the contract docs remain the durable source of truth for the shipped state
