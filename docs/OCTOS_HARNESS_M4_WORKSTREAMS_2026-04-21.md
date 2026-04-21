# Octos Harness M4 Workstreams

Date: 2026-04-21

Base state: `origin/main` at `5481452`.

This document updates the harness engineering progress, records the remaining
gaps, and defines the next milestone workstreams to publish in GitHub.

## Current Progress

The first harness delivery program is complete at the original issue level:

- `#414` coding/debugging loop hardening: closed.
- `#415` coding hard-case live acceptance: closed.
- `#433` harness runtime epic: closed.
- `#434` workspace policy v1: closed.
- `#435` generic lifecycle hooks: closed.
- `#436` validator gating and terminal completion enforcement: closed.
- `#437` policy-driven artifact truth and delivery: closed.
- `#438` deterministic spawn-only lifecycle: closed.
- `#439` first-party harness templates for slides and sites: closed.
- `#441` first-party slides/sites contract ownership out of kernel paths: closed.

The implementation now has the core runtime pieces needed for a contractual
app layer:

- durable workspace policy records for artifact, validation, and spawn-task
  expectations
- `BeforeSpawnVerify` as the blocking pre-delivery hook with deny/modify
  semantics
- observer lifecycle hooks for resume, turn end, spawn verify, spawn complete,
  and spawn failure
- policy-owned primary artifact selection instead of filename-only heuristics
- stable task `lifecycle_state` values for user-facing progress
- first-party slides and site flows using the harness contract shape
- coding-loop recovery and live hard-case tests for bounded shell/debugging
  behavior

The current still-open standing issues are broader program controls, not the
closed implementation slices:

- `#412` Phase 3 umbrella.
- `#413` canary soak and regression triage.
- `#416` operator dashboard beyond the CLI/runtime summary.

## Gap Assessment

The harness layer is strong enough for first-party flows and controlled custom
apps, but it is not yet a polished third-party developer platform. The
remaining gaps are productization gaps, not a reason to reopen the completed
Phase 3 implementation issues.

### Gap 1: Progress ABI Is Not General Enough

Background child workflows expose task state, but fine-grained progress is not
uniformly bridged from child tools, workflow runners, and plugin stderr into the
parent chat stream. Deep research exposed this gap: the task exists and replays
as running, but detailed phase/status updates can be missing from the parent UI.

### Gap 2: Developer Contract Docs Are Still Too Runtime-Shaped

The developer interface explains the concepts, but a third-party developer
still needs working examples for custom app classes such as report generation,
coding assistants, research apps, audio workflows, and non-slides visual apps.

### Gap 3: Policy Authoring Needs Tooling

Workspace policy exists, but developers need a validation command, examples,
schema output, and migration guidance. Today it is possible to author a policy,
but not yet easy to know whether it is complete before running a live app.

### Gap 4: Validator Runner Is Too Narrow

Completion gating exists, and `BeforeSpawnVerify` can block or modify outputs,
but arbitrary declarative validators still need a runner model with typed
results, timeouts, failure categories, and replayable operator evidence.

### Gap 5: Operator Surface Is Not Productized

Runtime truth exists in backend state and summaries, but the dashboard still
needs a compact operator view for lifecycle, phase, artifacts, validator
results, retries, and failure causes.

### Gap 6: Third-Party Compatibility Gate Is Missing

First-party slides and sites prove the model, but release readiness for custom
apps requires a live compatibility gate that installs a skill from Git, runs it
through the harness, validates artifacts, reloads, removes it, and verifies no
state bleed.

### Gap 7: ABI Versioning Is Implicit

Hook payloads, task status events, workspace policy, and artifact roles need
explicit schema versions and compatibility tests before external developers can
depend on them safely.

## Next Milestone Goals

M4 is the productization milestone for the harness layer. The goal is to turn
the working internal runtime into a developer-safe custom app surface.

### M4.1: Parent-Visible Progress ABI

Goal:

- every long-running child workflow can report durable, replayable progress to
  the parent session and chat header

Acceptance:

- deep research shows phase/status progress in the parent chat while running
- progress survives session switching and browser reload
- `/api/sessions/:id/tasks` and the persistent event stream expose the same
  truth
- no duplicate child sessions or cross-session progress bleed

#### M4.1A: Structured Progress Contract

GitHub milestone:
<https://github.com/octos-org/octos/milestone/1>

Trigger:

- mini1 validation of the first deep-research progress patch still replayed only
  the initial `task_status`; no later phase updates appeared after 15s or 45s.

Contract decision:

- stderr is diagnostics only
- stdout remains the tool/plugin result channel where applicable
- runtime progress must be emitted as structured `octos.harness.event.v1`
  records
- structured progress must update durable parent `task_status`
- browser progress UI must consume the same replayable backend truth as the
  task API
- the event transport is language-neutral; Rust, Python, JavaScript, shell, and
  opaque binaries all use the same sink contract
- the sink is addressed as a transport URI; local transports are mandatory,
  distributed pub/sub transports are long-term evolution, not M4.1A blocking
  scope

Required event shape:

```json
{
  "schema": "octos.harness.event.v1",
  "kind": "progress",
  "session_id": "...",
  "task_id": "...",
  "workflow": "deep_research",
  "phase": "fetching_sources",
  "message": "Fetching source 3/12",
  "progress": 0.42
}
```

Deliverables:

- typed progress event ABI with schema versioning and field limits
- runtime-provided `OCTOS_EVENT_SINK` for child tools/workflows
- transport abstraction for `OCTOS_EVENT_SINK`:
  - required: local file/JSONL or Unix-domain socket transport
  - optional: stdio/fd transport for sandboxed children
- language-neutral event emitter helpers:
  - Rust `tracing`/helper crate
  - Python helper package or copyable single-file emitter
  - JavaScript/Node helper package or copyable single-file emitter
  - CLI fallback: `octos-event emit --kind progress --phase ...`
- bounded runtime consumer that bridges structured events into task supervisor
  `runtime_detail`
- `TaskStatusChanged` emission and session-event replay for every accepted
  structured progress update
- deep-search migration from stderr-only progress to structured events while
  preserving stderr as human diagnostics
- browser/API proof that progress survives session switching and reload
- live mini fleet validation before closing the milestone

Published workstreams:

- `#470`: Core ABI for structured progress events
- `#471`: Runtime event sink to durable `task_status`
- `#472`: Deep-search structured progress emission
- `#473`: UI/API parent-visible progress replay
- `#474`: Release gate and mini fleet validation
- `#475`: Non-Rust bridge for Python and JavaScript emitters

Transport policy:

- M4.1A must not require a network broker for same-host child progress.
- Pub/sub backends are long-term evolution only. They are acceptable later only
  if they implement the same `octos.harness.event.v1` schema and feed the same
  durable parent `task_status` path.
- Zenoh is tracked separately as `#476` and must not block the local reliability
  fix for mini1 deep research.

Agent ownership model:

- ABI agent owns event types, schema tests, and docs only.
- Runtime agent owns child process environment injection, sink consumption,
  task supervisor updates, and replay tests.
- Deep-search agent owns only `crates/app-skills/deep-search` emission changes.
- Non-Rust bridge agent owns Python/JavaScript emitter helpers and compatibility
  fixtures.
- UI/API agent owns chat header/replay behavior and browser regression tests.
- Release agent owns deploy/test evidence across mini1, mini2, mini3, and mini5.

Merge rule:

- land `#470` first, then stack `#471` and `#472`; `#473` can proceed against
  the API contract in parallel, and `#474` closes only after one integrated SHA
  is deployed and verified.

### M4.2: Developer Contract Docs And Starters

Goal:

- a third-party developer can build a harnessed custom app from docs alone

Acceptance:

- docs include a minimal custom app, a report app, an audio app, and a coding
  assistant app
- each example declares artifacts, validators, lifecycle behavior, and delivery
  expectations
- examples run through the same harness path as first-party apps

### M4.3: Declarative Validator Runner

Goal:

- workspace policy can declare validators that produce durable typed outcomes

Acceptance:

- validators support command, tool, and file-existence checks
- validator output records status, reason, duration, and evidence path
- completion cannot report ready when a required validator fails
- validator results replay through task APIs and operator surfaces

### M4.4: Third-Party Skill Compatibility Gate

Goal:

- custom skills installed from Git or Octos Hub can prove harness compatibility
  without runtime-specific code branches

Acceptance:

- live test installs a custom skill, runs it, verifies declared artifacts,
  reloads, removes it, and confirms binaries/state are removed
- app-specific delivery works without modifying core runtime code
- sandbox, secret, and executable expectations are documented and enforced

### M4.5: Operator Harness Dashboard

Goal:

- operators can diagnose harnessed apps without reading logs

Acceptance:

- dashboard shows task lifecycle, phase, child session, artifact, validator,
  retry, timeout, and failure state
- stale/zombie task and missing-artifact conditions are visible
- dashboard and CLI summary read the same backend truth

### M4.6: Harness ABI Versioning

Goal:

- external developers can depend on stable harness schemas

Acceptance:

- workspace policy, hook payloads, progress events, and task responses carry
  explicit schema versions
- compatibility tests cover old policy files and old hook payload shapes
- docs describe the compatibility promise and deprecation process

## Detailed Workstream Issues

These are the published GitHub issues for M4.

### H4.1: Generalize Parent-Visible Progress ABI

GitHub issue: `#464`
<https://github.com/octos-org/octos/issues/464>

Scope:

- define the canonical progress event shape for child workflows
- bridge workflow/plugin progress into parent-visible `task_status`
- persist and replay progress through the session event stream
- make chat header and task APIs consume the same state

Out of scope:

- redesigning the whole task supervisor
- app-specific prompt changes as the primary fix

Acceptance:

- deep research, podcast, and one custom app all show live progress
- progress survives reload and session switching
- no duplicated research sessions appear from one user request

### H4.2: Publish Harness Developer Contract And Starter Apps

GitHub issue: `#465`
<https://github.com/octos-org/octos/issues/465>

Scope:

- write the developer-facing contract guide
- add starter policies and sample apps for report, audio, coding, and generic
  artifact workflows
- document `BeforeSpawnVerify`, artifact roles, validators, and
  `lifecycle_state`

Acceptance:

- a developer can create a custom harnessed skill without reading runtime code
- each starter has a runnable smoke test
- docs distinguish stable API from internal implementation details

### H4.3: Build Declarative Validator Runner

GitHub issue: `#466`
<https://github.com/octos-org/octos/issues/466>

Scope:

- implement validator execution from workspace policy
- persist typed validator outcomes
- expose validator outcomes through task APIs and operator summary
- enforce required validator failures before ready/delivery

Acceptance:

- required validator failure blocks terminal success
- optional validator failure records a warning without blocking delivery
- validator timeout and stderr are visible to operators

### H4.4: Add Third-Party Skill Compatibility Gate

GitHub issue: `#467`
<https://github.com/octos-org/octos/issues/467>

Scope:

- add live test for Git or Octos Hub skill install, run, reload, removal
- verify declared binaries and artifacts under a profile directory
- assert no skill state remains after removal
- cover one non-slides custom app path

Acceptance:

- compatibility test passes on mini1
- custom app delivery does not require new runtime branches
- install/remove failures are actionable in UI and logs

### H4.5: Build Operator Harness Dashboard

GitHub issue: `#468`
<https://github.com/octos-org/octos/issues/468>

Scope:

- add dashboard panel for harness task state
- show lifecycle state, current phase, child session, artifacts, validators,
  retries, timeouts, and failures
- link dashboard state to the existing backend summary/state APIs

Acceptance:

- an operator can diagnose a failed harnessed app from the dashboard alone
- dashboard state matches CLI/operator summary
- stale task and missing-artifact cases are visible

### H4.6: Version Harness ABI Schemas

GitHub issue: `#469`
<https://github.com/octos-org/octos/issues/469>

Scope:

- add explicit schema versions to workspace policy, hook payloads, progress
  events, and task responses
- add compatibility tests for old schemas
- document migration and deprecation rules

Acceptance:

- old supported policy files still load or fail with actionable errors
- hook consumers can branch on schema version
- docs define which fields are stable for external developers

## Release Gate For M4

M4 is complete only when all of these pass:

- canonical Rust/workspace CI
- dashboard/web build and lint
- milestone live browser suites on mini1, mini2, mini3, and mini5
- live deep research progress replay test
- live custom skill install-run-reload-remove test
- written closure note in each M4 workstream issue

## Control Rule

Do not call the harness developer platform ready because first-party apps work.
Call it ready when a third-party developer can build, validate, run, observe,
and debug a custom app through documented contracts without modifying core
runtime code.
