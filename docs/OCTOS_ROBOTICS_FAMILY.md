# Octos Robotics Family — Program Plan

See also:

- [OCTOS_ROBOTICS_ARCHITECTURE.md](./OCTOS_ROBOTICS_ARCHITECTURE.md)
- [OCTOS_ROBOTICS_CONTRACTS.md](./OCTOS_ROBOTICS_CONTRACTS.md)
- [OCTOS_HARNESS_MASTER_PLAN.md](./OCTOS_HARNESS_MASTER_PLAN.md)

## Purpose

This document is the durable control plan for the robotics family. Its job is
to sequence the architectural work, name the roles, define the release gates,
and give a program manager or architect a single source of truth to drive an
agent swarm against.

It follows the same shape as
[OCTOS_HARNESS_MASTER_PLAN.md](./OCTOS_HARNESS_MASTER_PLAN.md): phases, release
contracts, roles, allowed files, decision rules, stop conditions, definition
of done.

Every item in the family is a contract-bound issue. The architect or PM does
not drive by chatting with the swarm. They drive by authoring a contract,
pointing the swarm at it, and verifying against the contract.

## Family Naming

- Family prefix: `R`
- Issue identifiers: `R01` through `R10`
- Branch convention: `robotics/<yyyy-mm-dd>-<issue-slug>`
- Commit scope tag: `robotics(R0N):`
- Phase prefix: `Phase RA`, `Phase RB`, `Phase RC`, `Phase RD`
- Release-contract filename: `docs/OCTOS_ROBOTICS_RELEASE_<yyyy-mm-dd>.md`

These names are mandatory. They exist so the swarm can dispatch work by
string match, and so out-of-family drift is rejected at review time.

## Prerequisite

The robotics family does not start until
`release/2026-04-17-harness-gate-local` lands with its three blocking fixes
from `OCTOS_RUNTIME_PHASE3_REVIEW.md` applied. The mission contract in
Phase RB is an extension of the workspace contract landed by that branch.

## Phase Ordering

The phases are in strict landing order. A phase does not start until the
prior phase is green on canary.

### Phase RA — Foundations

Goal: make the two-cadence runtime real, land the safety trust domain, and
land the typed bridge so higher phases have something to build on.

Issues:

- `R01` fast-loop peer (Tier 1.1)
- `R03` safety supervisor skeleton (Tier 1.3)
- `R04` robotics bridge with backpressure (Tier 2.1)

Out of this phase:

- mission contract — not ready until `R01` is green
- sensor-context policy — needs `R04`
- black box — needs `R03`
- anything touching the pipeline engine

Release gate:

- a sim hardware loop ticks at declared cadence under concurrent LLM load
- safety supervisor vetoes a declared violation and transitions the fast
  loop to Safe without slow-loop cooperation
- bridge drops on overflow without buffering past the declared budget

### Phase RB — Substrate

Goal: make the pipeline the execution substrate for robot tasks, extend
workspace contract to temporal state, land the sensor-to-context policy.

Issues:

- `R02` pipeline-as-substrate (Tier 1.2)
- `R05` mission contract (Tier 2.2)
- `R06` sensor-context policy (Tier 2.3)

Out of this phase:

- multi-robot cell — not ready until all of RA and RB
- HITL beyond current channel primitives

Release gate:

- a mission contract failure blocks terminal success the same way a
  workspace contract failure does today
- the agent loop is not the per-tick driver of the sim workflow
- sensor injection stays under a declared prompt budget across a full
  mission

### Phase RC — Operability

Goal: make the program observable, auditable, and reproducible.

Issues:

- `R07` black-box recorder (Tier 3.1)
- `R08` HITL authorization (Tier 3.2)
- `R09` simulation parity (Tier 3.3)

Release gate:

- a sim-mode incident can be reproduced bit-for-bit from the recorder
- a HITL authorization request defaults to deny on timeout
- the same agent code runs against MuJoCo in CI and the sim rig

### Phase RD — Scale

Goal: multi-robot coordination when single-robot is proven.

Issues:

- `R10` cell orchestration (Tier 3.4)

Phase RD is explicitly deferred until RC canary soak passes.

## Release Contracts

Each phase produces one or more release contracts named
`OCTOS_ROBOTICS_RELEASE_<yyyy-mm-dd>.md`. The contract is modeled after the
`## Release Contract — 2026-04-17` section of
`OCTOS_HARNESS_MASTER_PLAN.md`.

A release contract must carry:

- baseline worktree paths
- public canary truth URL
- in-scope issue identifiers, exact
- existing foundation to reuse, not expand
- explicitly out-of-scope issues for the slice
- allowed backend files — exact paths, no glob
- decision rules
- roles with named workers
- mandatory pre-deploy checks
- mandatory post-deploy release gate
- stop conditions
- definition of done

A release contract is immutable once the slice starts. Drift during a slice
is handled by opening the next slice, not by editing the current contract.

## Decision Rules

Every change in an open slice must answer:

1. Which `R` issue does it serve?
2. What user-visible or operator-visible behavior does it fix or add?
3. What exact test proves it?
4. Does it touch any file outside the slice allowlist?

If the change cannot map to a single `R` issue, it does not go in. If it
requires files outside the allowlist, stop and rescope before coding. If
the test does not exist, write it first.

## Roles

Human roles:

- **Architect.** Owns this document, the architecture note, and the
  per-issue contracts. Final go/no-go on merge. Authors every release
  contract.
- **Program manager.** Owns phase sequencing, slice scope, swarm
  dispatch. Does not write code. Authors no contracts but may propose
  edits to a draft release contract before the slice starts.

Swarm roles. Each role is a dedicated agent. Agents may be automated or
human. The role is defined by the files and outputs it is permitted to
produce, not by who executes it.

- **Planner.** Reads architecture, family, and contract. Produces an
  ordered task list for the slice. May not edit code.
- **Implementer.** Works inside the slice's allowlist. Writes code and
  tests. May not edit docs in `docs/` and may not open new `R` issues.
- **Verifier.** Runs acceptance tests, checks the allowlist was honored,
  runs the required invariant assertions. Produces a pass/fail report.
  May not edit code.
- **Scope guard.** Rejects any change that crosses issue boundaries or
  touches files outside the allowlist. May veto a PR. Models the role
  `Nash` plays in the harness family.
- **Safety gate.** Rejects any change that weakens a safety invariant,
  adds a new unsandboxed execution path, or weakens `BLOCKED_ENV_VARS`,
  `deny(unsafe_code)`, `O_NOFOLLOW`, or `SafePolicy`. Unique to the `R`
  family. Has veto power overriding every other role.

Named workers carry role tags across slices so the swarm is dispatched by
name. Workers may rotate. Roles may not.

## How The PM Drives

A slice is driven by a four-step loop.

1. **Author the release contract.** The architect writes
   `OCTOS_ROBOTICS_RELEASE_<date>.md` by copying the template from the
   harness master plan and filling in the slice fields. The PM reviews
   scope and sequencing before the slice opens.
2. **Dispatch by contract, not by chat.** The PM hands workers the
   contract URL, the issue ID, and the role. No scope negotiation over
   chat. The contract is the scope.
3. **Gate at the allowlist.** The scope guard audits every diff against
   the allowed backend files in the release contract. Any touch outside
   is bounced.
4. **Verify at the definition of done.** The verifier runs the exact
   tests listed in the contract and the full required-invariant set from
   the architecture doc. If one of them is red, the slice is not done,
   regardless of how close the work is.

The PM does not accept "almost green" at the definition of done. The PM
does accept reopening the next slice with a narrower scope.

## Swarm Dispatch Pattern

To dispatch the swarm:

- identify the slice and open the release contract
- assign one implementer per issue in the slice
- assign one verifier per issue
- assign one scope guard for the slice
- assign the safety gate as a standing role across all `R` slices
- set all agents to read-only on files outside the allowlist

Parallelism is per-issue, not per-file. Two implementers never touch the
same file in the same slice without a documented merge plan in the release
contract.

## Required Invariants For Every Slice

These invariants are checked by the verifier and the safety gate at every
slice. Any failure kills the merge.

1. No file outside the release-contract allowlist was touched.
2. All acceptance tests in the per-issue contract are green.
3. All required invariants in
   [OCTOS_ROBOTICS_ARCHITECTURE.md](./OCTOS_ROBOTICS_ARCHITECTURE.md)
   still hold after the slice.
4. No new `unsafe` block was added.
5. No new execution path bypasses the sandbox, `ToolPolicy`,
   `BLOCKED_ENV_VARS`, `O_NOFOLLOW`, or `SafePolicy`.
6. No new doc describes behavior the code does not yet exhibit.
7. No new `#[cfg(feature = "...")]` gate was added to an existing struct
   field or function signature without a separate slice for the gate
   itself.
8. No root-level `architect.md` or equivalent authoritative-but-aspirational
   document was added.
9. No Python reimplementation of any Rust runtime component was added to
   the monorepo.
10. A new crate, if added, forwards real traffic — `execute` is not a
    placeholder.

If any invariant turns red, the slice stops and opens a follow-up slice
to correct the regression before proceeding.

## Stop Conditions

Stop the slice if any of these appear:

- a required test is red and will not go green inside the slice
- a proposed fix needs a file outside the allowlist
- the safety gate vetoes
- a second web patch is needed for the same slice
- a post-deploy gate fails

The stop condition is not a negotiation. It is a signal to close the
slice, open the next one with a corrected scope, and continue.

## Definition Of Done — Program Level

The robotics program is complete only if all of the following hold on the
public canary:

- a single-robot sim runs end-to-end through the pipeline substrate
- a safety veto transitions the fast loop to Safe state without slow-loop
  cooperation
- a mission contract failure blocks terminal success
- a black-box recording reproduces a declared incident bit-for-bit
- operator surface includes robot counters alongside the existing runtime
  summary
- the same agent code runs sim and hardware without source changes
- no safety invariant from the architecture doc is carried by prompt text

Anything less is not a complete robotics program, even if individual `R`
issues are merged.

## Persistence

This family doc is the canonical program record.

Per-issue contracts live in
[OCTOS_ROBOTICS_CONTRACTS.md](./OCTOS_ROBOTICS_CONTRACTS.md). Release
contracts are per-date files named
`docs/OCTOS_ROBOTICS_RELEASE_<yyyy-mm-dd>.md`. Architecture targets and
required invariants live in
[OCTOS_ROBOTICS_ARCHITECTURE.md](./OCTOS_ROBOTICS_ARCHITECTURE.md).
