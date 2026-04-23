# Octos Harness M5 Coding Runner Contract

Date: 2026-04-22

Base state: `main` at `edde8a9`.

This document defines the next harness milestone: make free-form coding and
exploration run on the same contractual runtime that now supports long-running
apps, progress events, validators, replay, and operator truth.

The milestone does not create a second coding runner. It extends the existing
agent runtime with a coding policy layer.

## Product Goal

Free-form coding should remain exploratory, but completion must become
contractual.

The user-visible promise:

- the agent can inspect, edit, build, test, and repair freely
- progress is typed and replayable
- readiness is decided by runtime evidence, not assistant prose
- failed validation produces actionable repair state
- reload, session switch, crash, and resume preserve task truth

## Non-Goals

- Do not create a separate parallel coding runtime.
- Do not make DOT/pipeline mandatory for free-form coding.
- Do not rely on LLM self-reported phase text as source of truth.
- Do not reintroduce global chat spinners or latest-message coupling.
- Do not treat "assistant says done" as terminal success.

DOT remains useful for explicit workflows. Free-form coding needs a runtime
wrapper policy because the path is discovered while the agent works.

## Architecture Decision

Use the existing runtime:

- agent loop
- tool executor
- task supervisor
- harness event sink
- workspace policy
- validator runner
- durable task state
- UI task anchors and replay

Add a coding-specific policy layer:

```text
existing agent loop
  -> observed tool calls
  -> CodingHarnessPolicy
  -> typed task phases
  -> validators and evidence
  -> ready / failed / repair
```

The LLM may propose work. The runtime classifies observed behavior and decides
readiness.

## M5.1 Task Kind And Policy Registry

Goal:

- every supervised task declares a durable `task_kind`, starting with `coding`

Deliverables:

- `TaskKind` or equivalent durable field on supervised task state
- policy registry that resolves `TaskKind::Coding` to `CodingHarnessPolicy`
- task creation path that records task kind before first tool execution
- migration/default behavior for existing generic tasks
- API response field so UI and operators can distinguish coding, research,
  audio, site, slides, robotics, and generic tasks

Acceptance:

- coding tasks replay with `task_kind = "coding"` after restart
- unknown task kinds fail closed or fall back to generic policy explicitly
- no UI behavior depends on guessing task kind from display title

Suggested workstream:

- Runtime owner: task state schema, migration, API response.
- UI owner: consume `task_kind` without changing rendering semantics yet.
- Test owner: persistence and unknown-kind compatibility tests.

## M5.2 Coding Phase Mapper

Goal:

- emit coding phases from observed runtime actions, not from LLM claims

Canonical phases:

- `planning`
- `inspecting`
- `editing`
- `building`
- `testing`
- `repairing`
- `verifying`
- `ready`
- `failed`

Initial mapping:

- read/list/search tools: `inspecting`
- patch/write/edit tools: `editing`
- shell commands that build artifacts: `building`
- shell commands that run tests or checks: `testing`
- validator failure followed by more edits: `repairing`
- validator execution: `verifying`
- all required validators pass: `ready`
- unrecoverable failure, timeout, or budget stop: `failed`

Deliverables:

- central mapper from tool events and validator outcomes to coding phases
- stable `octos.harness.event.v1` progress events for every phase transition
- bounded phase-detail payloads with evidence references rather than raw logs
- tests proving LLM text alone cannot advance the phase

Acceptance:

- "I am testing now" in assistant text does not produce `testing`
- an actual test command or validator outcome does produce `testing`
- phase history is replayable from durable task state
- session switch does not move progress to the wrong message

Suggested workstream:

- Runtime owner: mapper and phase transition rules.
- Security owner: command classifier boundaries and secret-safe details.
- UI owner: render phase labels from typed state only.

## M5.3 Coding Completion Gate

Goal:

- terminal coding success requires evidence

Required completion contract:

- changed files or explicit no-op justification
- final diff or no-op evidence
- build/check/test validator outcome, or explicit policy waiver
- no unresolved required validator failures
- final assistant answer must cite the runtime evidence summary

Deliverables:

- default coding workspace policy
- required validators for common repo types
- no-op completion path with explicit justification
- policy waiver mechanism with operator-visible reason
- terminal success blocked when evidence is missing

Acceptance:

- assistant prose cannot mark a coding task ready without validator evidence
- failed tests keep the task in `failed` or `repairing`
- no-op tasks can complete only with persisted no-op justification
- release docs explain default validator policy and override points

Suggested workstream:

- Validator owner: default command/file validators.
- Runtime owner: terminal gate integration.
- Docs owner: coding policy examples and waiver guidance.

## M5.4 Evidence Bundle

Goal:

- every coding task produces a durable evidence bundle

Bundle contents:

- task id, parent session id, workspace root
- changed files
- final diff or no-op justification
- commands run
- build/test summaries
- validator outcomes
- failure category and repair hint when failed
- links to bounded raw logs when available

Deliverables:

- evidence bundle schema
- persisted evidence file under task/workspace state
- API endpoint or task response field with evidence summary
- operator dashboard section for coding evidence
- redaction of secrets and oversized logs

Acceptance:

- reloading the browser shows the same evidence summary
- operators can diagnose why a coding task is not ready
- evidence survives process restart
- secret-looking values are redacted before persistence or UI exposure

Suggested workstream:

- Runtime owner: bundle writer and schema.
- Operator UI owner: compact evidence surface.
- Security owner: redaction and size limits.

## M5.5 Resume, Replay, And UI Anchoring

Goal:

- coding task state is rendered per task/message anchor and never leaks into
  unrelated chat bubbles

Deliverables:

- coding task anchors use stable task ids
- replay hydrates anchors from `/tasks` and event history
- active task status survives session switch and reload
- completed/failed task status stops spinners deterministically
- multiple concurrent coding tasks render independently

Acceptance:

- asking a normal chat question while coding runs does not show coding progress
  on that normal answer
- switching sessions and returning restores the coding task in the same
  transcript location
- completed tasks do not keep spinning after final answer delivery
- stale stream updates for old sessions are ignored

Suggested workstream:

- UI owner: per-task anchor rendering and hydration.
- Runtime owner: durable task/status replay API.
- E2E owner: browser tests for switch/reload/concurrent tasks.

## M5.6 Coding Prompt Contract

Goal:

- the model understands the harness contract without owning the contract truth

Deliverables:

- coding system prompt fragment describing validator-gated completion
- instructions for evidence citation in final answers
- repair loop instructions when validators fail
- policy context injection from workspace contract
- tests that prompt contract does not bypass runtime enforcement

Acceptance:

- model attempts to declare success before validation are blocked by runtime
- final answers cite actual evidence bundle fields
- validator failure produces a repair-oriented continuation, not false success

Suggested workstream:

- Prompt owner: coding prompt fragment.
- Runtime owner: ensure prompt-only success is not terminal.
- Test owner: false-success and repair-loop regression tests.

## M5.7 Live Gate And Regression Suite

Goal:

- prove the coding harness under real browser and mini fleet conditions

Required gates:

- local `cargo fmt --all -- --check`
- local `cargo clippy --workspace -- -D warnings`
- local `cargo test --workspace`
- dashboard build
- browser coding task progress test
- browser session switch/reload test
- failed-test repair test
- no-op completion test
- concurrent coding task isolation test
- mini1 and mini5 live verification before release promotion

Acceptance:

- live browser shows `inspect -> edit -> test -> ready` for a small coding fix
- failed test keeps task non-ready until repaired
- normal chat during coding has no task spinner
- reloading mid-run reconstructs the same task state
- evidence bundle is visible after completion

Suggested workstream:

- CI owner: fast local and GitHub gates.
- Live-test owner: mini browser coverage.
- Release owner: publish exact SHA, validation set, and mini results.

## Issue Cut Plan

Publish M5 as small parallel workstreams:

- `M5-1`: TaskKind and coding policy registry
- `M5-2`: coding phase mapper from tool and validator events
- `M5-3`: coding completion gate and default validators
- `M5-4`: evidence bundle schema and persistence
- `M5-5`: UI task anchors and replay for coding tasks
- `M5-6`: coding prompt contract and repair loop
- `M5-7`: live gate and regression suite

Merge order:

1. `M5-1` first, because all later work needs durable task kind.
2. `M5-2` and `M5-4` can run in parallel after `M5-1`.
3. `M5-3` depends on `M5-2` and `M5-4`.
4. `M5-5` can run after task kind and phase events exist.
5. `M5-6` can run in parallel but cannot close before `M5-3`.
6. `M5-7` closes last and is the release gate.

## Definition Of Done

M5 is complete when:

- free-form coding uses the existing runtime plus `CodingHarnessPolicy`
- coding progress is typed, durable, and replayable
- completion is validator-gated
- evidence is persisted and visible
- UI renders per-task state without cross-chat contamination
- live browser tests prove reload, session switch, false-success blocking, and
  repair after failure

At that point, Octos has a real software-factory loop for free-form coding:
exploration remains flexible, but output quality is bounded by runtime evidence.
