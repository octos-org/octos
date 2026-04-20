# Octos Harness Developer Interface

This document defines the developer-facing harness interface that Phase 3
should expose for customer skills and apps.

The goal is not to make Octos feel "open" in the vague sense. The goal is to
make Octos feel safely programmable.

That is a product position:

- Octos is an execution OS for customer skills/apps
- not a loose agent playground
- not a prompt-only integration surface
- not a system where developers guess which file, tool, or background result
  the runtime will trust

## Why This Matters

Previous-generation agent products often looked flexible because developers
could do almost anything.

The failure mode was:

- no explicit artifact contract
- no explicit validator contract
- no explicit background task supervision contract
- no clear resume/reload/restart semantics
- no stable operator truth for what happened

That kind of openness is dangerous. It shifts correctness onto prompt wording
and developer luck.

Octos should instead offer:

- a small abstract harness API
- runtime-owned enforcement
- durable execution semantics
- clear developer guarantees

## Product Promise

A customer who builds a skill/app on Octos should be able to answer these
questions without reading the runtime internals:

1. What output does my app declare as final?
2. When is my app considered "done"?
3. What checks must pass before the result is shown to the user?
4. How are background tasks supervised and delivered?
5. What survives reload, reconnect, crash, or restart?
6. How does the operator inspect failures?

If those answers are not obvious from the developer interface, the harness is
not ready.

## The Abstract Harness Interface

The external developer surface should stay small.

Every customer skill/app should plug into the same six concepts:

### 1. Workspace

The app owns a declared workspace root.

It may contain:

- source files
- intermediate files
- final artifacts
- metadata/manifests

The runtime should never require the developer to rely on hidden path
conventions alone.

### 2. Artifacts

The app declares:

- primary artifacts
- preview artifacts
- source artifacts

This tells Octos:

- what to deliver
- what to preserve on reload
- what to ignore as stale or intermediate

### 3. Validation

The app declares checks at explicit lifecycle points:

- on source change
- on turn end
- on completion

The runtime decides whether the result is:

- still running
- verifying
- ready
- failed

### 4. Background Tasks

The app declares supervised background work:

- verify
- complete
- deliver
- failure

This means background tasks are not just "fire and hope". They become durable
runtime objects with an explicit contract.

### 5. Lifecycle Hooks

The harness lifecycle should expose a stable abstract interface:

- workspace init
- turn start
- turn end
- resume
- spawn verify
- spawn complete
- spawn failure
- publish

Customer-facing abstractions should map to these hooks even if the runtime
implementation evolves.

### 6. Operator Truth

Every app should automatically inherit:

- structured validator outcomes
- task lifecycle state
- artifact delivery outcomes
- source-level runtime provenance

That operator truth is part of the developer contract, not an afterthought.

## Developer-Facing Contract Shape

The customer-facing harness API should be presented as three layers.

### Layer A: Capability Manifest

This answers:

- what the app can do
- what tools or child skills it needs
- what kinds of outputs it may produce

Examples:

- `presentation`
- `site`
- `audio`
- `report`
- `dataset`

### Layer B: Workspace Policy

This is the durable contract file.

It should declare:

- workspace kind
- tracking/ignore rules
- artifacts
- validation rules
- spawn task policies

This is where the product contract lives.

### Layer C: Runtime Result Model

The runtime should expose a stable result state machine like:

- `queued`
- `running`
- `verifying`
- `ready`
- `failed`

And the final result should include structured fields like:

- `primary_artifact`
- `preview_artifacts`
- `validator_results`
- `delivery_status`
- `task_status`

This gives developers and operators one shared truth.

## Current Runtime Addendum

Until the broader customer-facing schema is finalized, the current runtime
follows these concrete Phase 3 conventions.

### `BeforeSpawnVerify` Semantics

`before_spawn_verify` is the blocking pre-delivery hook for successful
background child sessions.

It runs only after the runtime has:

- received a successful child result
- resolved candidate terminal `output_files`
- identified the workflow/phase context for verification

It runs before the runtime:

- emits `on_spawn_verify`
- marks the task ready
- persists or delivers the terminal artifact set

Hook behavior is:

- allow: keep the runtime-selected `output_files`
- modify: replace `output_files` by returning either a JSON string array or
  `{"output_files":[...]}`
- deny: fail verification and turn the child run into a terminal failure
- hook error: log the hook failure and continue with the runtime-selected files

### Primary Artifact Convention

The harness contract should name one canonical end-user artifact under
`artifacts.entries.primary`.

That path is the source of truth for the primary artifact. Until every surface
exposes an explicit `primary_artifact` field, clients should treat the
contract's `primary` artifact, or the first terminal `output_files` entry when
only runtime task data is available, as the primary artifact.

First-party Phase 3 workflows currently follow this convention:

- slides: `output/deck.pptx`
- site: the published entrypoint such as `dist/index.html` or `out/index.html`
- audio: the final rendered audio file

Any additional terminal files should be treated as previews or secondary
artifacts, not as competing primary outputs.

### `lifecycle_state` API Field

Task APIs expose both a low-level `status` and a user-facing
`lifecycle_state`.

`lifecycle_state` is the stable UX state machine:

- `queued`: the task was registered but has not started execution yet
- `running`: the child worker is actively executing
- `verifying`: execution finished and the runtime is resolving, validating, or
  delivering outputs
- `ready`: the task reached terminal success and its outputs are ready for the
  user-facing surface
- `failed`: execution or verification failed

Clients should drive progress UI from `lifecycle_state` and treat `status` as
the lower-level supervisor record.

## Guarantees Octos Should Make

For customer skills/apps, Octos should guarantee:

1. Durable contract state
- the harness contract survives context compaction, reload, crash, and restart

2. Contract-owned delivery
- the runtime delivers declared final artifacts from contract truth, not from
  heuristic guessing

3. Structured validation
- validator outcomes are persisted in machine-readable form

4. Supervised background execution
- background tasks do not silently disappear into prompt space

5. Topic/session isolation
- outputs and histories stay scoped to the correct surface/topic/session

6. Operator observability
- failures can be diagnosed from runtime truth instead of log archaeology alone

## What We Must Not Expose

To keep the interface safe and durable, customer-facing harness APIs should not
depend on:

- prompt-only conventions
- undocumented path magic
- ad hoc send-file calls
- hidden per-app runtime branches
- transient browser-only state
- free-form lifecycle names without runtime semantics

If a developer must "just know" how Octos behaves, the contract is too weak.

## UX Value For Developers

This harness interface should make custom skills/apps feel:

- easier to reason about
- safer to ship
- easier to debug
- more portable across workflows
- more native to Octos

The developer should feel:

- "I declare outputs and checks, Octos enforces them"

not:

- "I hope the agent remembers to deliver the right thing"

## UX Value For End Users

A stronger developer harness becomes user-visible as:

- fewer fake-success results
- fewer missing artifacts after reload
- fewer broken previews/decks/audio deliveries
- clearer failure states
- more consistent app behavior across custom skills/apps

## Phase 3 Mapping

This developer interface maps onto the still-open Phase 3 issues:

- `#434`
  - policy schema that developers can actually author
- `#435`
  - stable lifecycle hooks behind the abstract interface
- `#436`
  - validator runner and completion gating
- `#437`
  - contract-backed artifact truth
- `#438`
  - supervised background task lifecycle
- `#439`
  - first-party templates that demonstrate the model
- `#416`
  - operator truth visible to humans

## Delivery Milestones For This Interface

### D1. Internal Contract Completeness

Octos internal first-party workflows must prove the interface on:

- slides
- sites
- TTS/audio

Acceptance:

- first-party apps use the same contract shape we want customers to see

### D2. Developer Beta Surface

Octos should expose a customer-safe beta interface for custom skills/apps:

- documented policy file
- documented lifecycle model
- documented result states
- minimal working example

Acceptance:

- an internal developer can build a custom skill/app from the docs alone

### D3. Productized Harness Position

Octos should be able to present the harness in product language as:

- a clear runtime contract for customer apps
- a safer alternative to loosely-defined agent tooling products

Acceptance:

- product docs, developer docs, and runtime behavior all tell the same story

## Control Rule

If a proposed harness change improves abstraction but does not improve the
developer contract, it should not get priority.

If a proposed harness change improves developer contract clarity and runtime
truth, it belongs in Phase 3.
