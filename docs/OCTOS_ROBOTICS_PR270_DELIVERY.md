# Octos Robotics — PR #270 Delivery Family (RP)

See also:

- [OCTOS_ROBOTICS_ARCHITECTURE.md](./OCTOS_ROBOTICS_ARCHITECTURE.md) — aspirational architecture targets (R-family)
- [OCTOS_ROBOTICS_FAMILY.md](./OCTOS_ROBOTICS_FAMILY.md) — long-horizon R-family program plan
- [OCTOS_ROBOTICS_CONTRACTS.md](./OCTOS_ROBOTICS_CONTRACTS.md) — R-family per-issue contracts
- [OCTOS_HARNESS_MASTER_PLAN.md](./OCTOS_HARNESS_MASTER_PLAN.md)

## Purpose

This document defines the **Robotics Primitives (RP) family** — a narrow
delivery track that closes PR `#270`'s original intent by landing six
scoped, contract-bound features on clean `main`.

PR `#270` ("feat: add developer examples for 7 robot safety features")
bundled 83 files and ~11k LOC covering safety tiers, hardware lifecycle,
realtime heartbeat, black-box recorder, pipeline deadlines, a dora-rs
bridge, and a vendored Python simulation project. It was closed after
review because the scope was 13× its title, key primitives were declared
but unwired, `LifecycleExecutor` shelled out unsandboxed, and the design
paralleled rather than integrated with `main`'s harness layer.

The RP family captures what was **right about the intent** — robot
builders do need tiered permissions, hardware lifecycle phases, deadlines,
realtime loops, and a dora bridge — and delivers each as an independent
narrow PR that integrates with the Phase 3 harness primitives already on
`main`.

## Relationship to the R-family

The R-family (`R01`–`R10`) is the long-horizon robotics architecture: a
control-plane/data-plane split, safety as a separate trust domain, mission
contracts, multi-robot cells. RP is **not a substitute** for R. RP lands
primitives that fit inside `main`'s current runtime shape, without the
architectural reshape R requires.

| RP issue | R-family mapping | Notes |
|---|---|---|
| RP01 | Subset of `R03` | Tier declared and enforced via existing `ToolPolicy`, not a supervisor |
| RP02 | Adjacent to `R03` | Hardware lifecycle through existing sandbox |
| RP03 | None — fits Phase 3 | Domain-hook pattern on top of `BeforeSpawnVerify` |
| RP04 | Subset of `R02`/`R05` | Deadline/checkpoint fields, real executor enforcement |
| RP05 | Subset of `R01`/`R06` | Realtime config wired in-process, no fast-loop peer |
| RP06 | Subset of `R04` | Real dora-rs forwarding, not a stub |

RP can land entirely against clean `main`. R-family items remain open for
a future architecture slice.

## Prerequisite

`main` must carry the full Phase 3 landing:

- `BeforeSpawnVerify` + `OnSpawnVerify` + `OnSpawnComplete` + `OnSpawnFailure` + `OnTurnEnd` + `OnResume` lifecycle events
- `TaskLifecycleState` stable states
- Policy-driven artifact resolution (`PRIMARY_CONTRACT_ARTIFACT`)
- First-party workspace policies in CLI (`#441` closed)
- SSE buffer bounds, admin-shell `kill_on_drop`, sessions-mutex fix
- `deny_unknown_fields` on nested profile config types

Verified green on `main` at `86fe162` or later.

## Family naming

- Family prefix: `RP`
- Issue identifiers: `RP01`–`RP06`
- Branch convention: `robotics-rp/<rp-slug>` e.g. `robotics-rp/01-tool-policy-tiers`
- Commit scope tag: `robotics(RP0N):`
- Phase: single phase `RPA` (Delivery)
- Label on GitHub: `robotics` + `enhancement`

## Phase RPA — Delivery

All six issues in one phase. Ordering by dependency:

```
RP01 ──┐
RP02 ──┤
RP03 ──┤── parallel start
RP04 ──┘
                │
                ▼
           RP05 (depends on RP03 for payload extension)
                │
                ▼
           RP06 (optional; can also be CLOSE-AS-DROPPED)
```

Each issue ships with its own narrow example, replacing the corresponding
example from PR `#270` — rewritten against actually-wired code.

## PR #270 code-reuse map

Files from PR `#270` that are salvageable per RP issue:

| PR file | RP issue | How reused |
|---|---|---|
| `crates/octos-agent/src/permissions.rs` (SafetyTier enum only) | RP01 | Extract enum + `SafetyTier::from_str`; delete `RobotPermissionPolicy`, `WorkspaceBounds` |
| `crates/octos-plugin/src/lifecycle.rs` (types only) | RP02 | Keep `HardwareLifecycle`, `LifecycleStep`, `LifecyclePhase` types; rewrite `LifecycleExecutor` through sandbox |
| `crates/octos-agent/src/agent/realtime.rs` | RP05 | Keep `RealtimeConfig`, `Heartbeat`, `SensorSnapshot`, `SensorContextInjector`; wire into `loop_runner.rs` |
| `crates/octos-pipeline/src/graph.rs` deadline/checkpoint fields | RP04 | Keep `DeadlineAction`, `MissionCheckpoint`; drop `Invariant` (unparsed) and `HandlerKind::{SensorCheck,Motion,Grasp,SafetyGate,WaitForEvent}` (unregistered) |
| `crates/octos-dora-mcp/src/{config,lib}.rs` scaffold | RP06 | Keep crate scaffold, `BridgeConfig`, `DoraToolMapping`; replace stub `execute()` with real IPC (or delete entire crate) |
| `crates/octos-agent/examples/inspection_safety.rs` | RP01 | Rewrite against `ToolPolicy` groups, not `RobotPermissionPolicy` |
| `crates/octos-plugin/examples/pick_and_place_lifecycle.rs` | RP02 | Rewrite against sandboxed `LifecycleExecutor` |
| `crates/octos-agent/examples/realtime_heartbeat.rs` | RP05 | Keep mostly as-is; wire assertions against real loop ticks |
| `examples/dora-bridge-config/*` | RP06 | Keep tool-map schema + DOT pipeline; drop if dora-mcp is removed |

Files from PR `#270` that are **explicitly rejected** from reuse:

- `architect.md` — describes unimplemented behavior, conflicts with `docs/OCTOS_RUNTIME_PHASE3_KICKOFF.md`
- `crates/octos-agent/src/recorder.rs` (BlackBoxRecorder) — redundant vs `TaskLifecycleState` + `after_tool_call` hooks
- `crates/octos-agent/src/hooks.rs` robot `HookEvent` variants — superseded by `BeforeSpawnVerify` + domain payload (RP03)
- `crates/octos-pipeline/src/graph.rs` `Invariant` struct + 5 new `HandlerKind` variants — unparsed / undispatched
- `typos.toml` allowlist additions for short typo tokens — mask real typos; if needed, use `extend-identifiers`
- `examples/slam-nav-sim/` entire directory — 7,631 LOC including Python reimplementation of octos internals; move to separate repo
- `crates/octos-cli/static/admin/*` bundle swaps — no source changes
- `crates/octos-dora-mcp/src/lib.rs` duplicate `SafetyTier` — use `octos_agent::permissions::SafetyTier`

## Roles

Inherited from `OCTOS_ROBOTICS_FAMILY.md`:

- **Architect** — owns contracts + final merge
- **Program manager** — phase sequencing, dispatch
- **Planner** — per-slice plan
- **Implementer** — one per RP issue, works inside allowed files
- **Verifier** — runs acceptance tests + invariant audit
- **Scope guard** — bounces out-of-allowlist diffs
- **Safety gate** — vetoes regressions of `deny(unsafe_code)`, `BLOCKED_ENV_VARS`, `O_NOFOLLOW`, `SafePolicy`, sandbox bypass

## Swarm dispatch pattern

```
ARCHITECT → authors this doc + per-issue contract
PM        → dispatches RP0N to one implementer + one verifier + shared scope guard
IMPLEMENTER → branches robotics-rp/0N-<slug> from main; ONLY touches allowed files
VERIFIER  → runs acceptance tests; audits allowed-files; checks invariants
SCOPE GUARD → rejects any file touch outside contract allowlist
SAFETY GATE → vetoes new unsandboxed exec, new unsafe blocks, weakened BLOCKED_ENV_VARS
```

RP01–RP04 can dispatch in parallel. RP05 and RP06 dispatch after RP03
lands (RP05 uses the payload extension; RP06 is optional).

## Required invariants for every RP slice

Inherited from `OCTOS_ROBOTICS_FAMILY.md` plus:

1. No file touched outside the RP issue's allowed-files list.
2. All acceptance tests in the contract green.
3. No new `unsafe` block.
4. No new execution path bypasses the sandbox, `ToolPolicy`, `BLOCKED_ENV_VARS`, `O_NOFOLLOW`, or `SafePolicy`.
5. No new doc describes behavior the code does not exhibit.
6. Every new `pub` type has at least one consumer in the binary build (no library-only additions).
7. Every new `HookEvent` firing site has a test that asserts the event fires. (No unfired enum variants.)
8. No new `#[cfg(feature = "...")]` gate added to an existing field without a separate slice for the gate.
9. Rust 2024 let-chain syntax is preserved where already used; no downgrades to nested `if let`.
10. `typos.toml` global allowlist is not widened; use `extend-identifiers` for domain-specific names.

---

## RP01 — SafetyTier as ToolPolicy group family

**Phase:** RPA
**Depends on:** none
**Reuses from PR #270:** `crates/octos-agent/src/permissions.rs` (SafetyTier enum only); `crates/octos-agent/examples/inspection_safety.rs`
**Deletes from PR #270:** `RobotPermissionPolicy`, `WorkspaceBounds`, `Tool::required_safety_tier()` default method, `PermissionDenied` error

### Problem

Robot integrators need to declare, per tool, the minimum supervisory tier
required to execute that tool (observe, safe motion, full actuation,
emergency override). Today octos has no way to express this declaratively.
The PR `#270` approach added a trait method and a standalone policy
struct that the agent loop never consults — declaration without
enforcement is worse than no declaration.

### Stable surfaces

- `Tool` trait signature (no new required methods)
- `crates/octos-agent/src/tools/policy.rs` deny-wins semantics, wildcard matching, named groups

### Architectural surface

- Add a new constant group set to `ToolPolicy`: `group:robot:observe`, `group:robot:safe_motion`, `group:robot:full_actuation`, `group:robot:emergency_override`. Each group resolves to the set of tools that are safe at or below that tier.
- Define the mapping in a new `crates/octos-agent/src/tools/robot_groups.rs` module loaded by the registry.
- Robot integrators configure `ToolPolicy.allow_groups = ["group:robot:safe_motion"]` in their profile to permit only observe + safe-motion tools.
- The `SafetyTier` enum stays in `crates/octos-agent/src/permissions.rs` (reduced module) as a public documentation type for the group vocabulary. `RobotPermissionPolicy` and `WorkspaceBounds` are deleted.

### Allowed files

- `crates/octos-agent/src/permissions.rs` — reduce to `SafetyTier` enum + `impl FromStr` + tests
- `crates/octos-agent/src/tools/policy.rs` — add group resolution for robot groups
- `crates/octos-agent/src/tools/robot_groups.rs` — new, declares the group-to-tool mapping
- `crates/octos-agent/src/tools/mod.rs` — re-export only
- `crates/octos-agent/src/lib.rs` — re-export `SafetyTier`
- `crates/octos-agent/examples/inspection_safety.rs` — rewrite against `ToolPolicy` groups
- `crates/octos-agent/tests/robot_tool_policy.rs` — integration test

### Required invariants

1. `ToolPolicy::evaluate(...)` consults robot groups exactly like existing groups. Verified by a test that blocks `navigate_to` when allow list contains `group:robot:observe` and asserts dispatch denial.
2. `SafetyTier::from_str` accepts the four canonical names case-insensitively; rejects all others with a typed error.
3. No new trait method is added to `Tool`.
4. Deleted types (`RobotPermissionPolicy`, `WorkspaceBounds`, `PermissionDenied`) are not re-exported.
5. The example uses only `ToolPolicy` configuration, no bespoke `authorize()` call.

### Explicit non-goals

1. A separate enforcement layer for motion tools (that is R03 in the aspirational R-family).
2. A UI for editing tier policy (follow-on).
3. Per-session tier escalation requests (that is `#293` `AskOperator` + R08 HITL).

### Acceptance tests

- `should_deny_safe_motion_tool_when_policy_allows_only_observe`
- `should_allow_observe_tool_when_policy_is_empty`
- `should_allow_full_actuation_tool_when_policy_grants_full_actuation_group`
- `should_reject_invalid_tier_string_in_from_str`
- `inspection_safety_example_runs_end_to_end_against_policy`

### Observability

- Counter `octos_tool_policy_denial_total{reason="robot_tier_gate", group}` — lift of existing `tool_policy_denial` counter.

### Rollback

The robot groups are additive. Profiles that don't set `allow_groups` continue to work.

### Review checklist

1. Is `required_safety_tier()` absent from the `Tool` trait?
2. Are the 4 robot groups registered in the existing `ToolPolicy` group resolution?
3. Does the inspection_safety example compile and run?
4. Does the integration test prove the deny path at the dispatch site?

---

## RP02 — HardwareLifecycle with sandboxed executor

**Phase:** RPA
**Depends on:** none
**Reuses from PR #270:** `crates/octos-plugin/src/lifecycle.rs` type definitions (`HardwareLifecycle`, `LifecycleStep`, `LifecyclePhase`); `crates/octos-plugin/src/manifest.rs` (`hardware_lifecycle` field); `crates/octos-plugin/examples/pick_and_place_lifecycle.rs`
**Rewrites from PR #270:** `LifecycleExecutor` — unsandboxed `sh -c` is non-negotiable to remove

### Problem

Robot plugins need declarative pre-flight, init, ready-check, shutdown,
and emergency-shutdown phases. The PR `#270` design is right-shaped but
the executor bypasses every safety primitive octos documents: no
`BLOCKED_ENV_VARS`, no `SafePolicy`, no sandbox, no kill-on-drop, not
cross-platform.

### Stable surfaces

- `PluginManifest` outer shape
- Existing `Sandbox` backend trait (`Bwrap`, `Macos`, `Docker`, `NoSandbox`)
- `BLOCKED_ENV_VARS` constant and env sanitization pattern
- `SafePolicy` shell denial rules

### Architectural surface

- `PluginManifest.hardware_lifecycle: Option<HardwareLifecycle>` (already in PR #270 shape)
- `LifecycleExecutor::run_phase(phase, context)` — dispatches every step through the existing `Sandbox` backend (`Sandbox::execute(step.command, ...)`) instead of spawning `sh -c` directly
- Env sanitization via `BLOCKED_ENV_VARS` on every step
- Cross-platform: the existing `Sandbox` already handles Windows via `NoSandbox`/`cmd /C`
- `tokio::process::Command::new(...).kill_on_drop(true)` at the backend level so timeout kills the child
- Step timeout enforced with `tokio::time::timeout` AND the child is explicitly killed on expiry (not just orphaned)

### Allowed files

- `crates/octos-plugin/src/lifecycle.rs` — rewrite executor; keep type definitions
- `crates/octos-plugin/src/manifest.rs` — add optional `hardware_lifecycle` field
- `crates/octos-plugin/Cargo.toml` — add dependency on `octos-agent` sandbox module (or extract to shared crate if cycle)
- `crates/octos-plugin/examples/pick_and_place_lifecycle.rs` — rewrite against sandboxed executor
- `crates/octos-plugin/tests/lifecycle_sandbox.rs` — integration test
- `crates/octos-agent/src/sandbox/mod.rs` — expose `Sandbox` trait publicly if needed (extend, do not replace)

### Required invariants

1. Every lifecycle step runs through `Sandbox::execute(...)`. No direct `tokio::process::Command::new("sh")` call in `lifecycle.rs`.
2. `BLOCKED_ENV_VARS` is applied to every step's environment.
3. A step that exceeds its `timeout_ms` has its child process killed (verified by a test that spawns `sleep 30` with `timeout_ms=100` and asserts the child PID is gone within 500ms).
4. Windows path: lifecycle runs via `cmd /C` when on Windows.
5. A `critical=true` step's failure aborts the phase; a `critical=false` step's failure records an error event and continues.
6. `hardware_lifecycle` field is optional; manifests without it behave exactly as before.
7. No new `unsafe` code.
8. `SafePolicy` applies — lifecycle commands that would be denied as shell commands (rm -rf /, dd, mkfs) are rejected before dispatch.

### Explicit non-goals

1. A new sandbox backend for hardware-specific constraints (out of scope; use existing backends).
2. An e-stop bus that emergency_shutdown can post to asynchronously (that is R08/HITL).
3. Per-step retry logic beyond what the existing `Sandbox` provides.

### Acceptance tests

- `should_run_preflight_steps_through_sandbox`
- `should_apply_blocked_env_vars_to_lifecycle_steps`
- `should_kill_child_when_step_timeout_exceeded`
- `should_abort_phase_when_critical_step_fails`
- `should_continue_phase_when_non_critical_step_fails`
- `should_reject_safepolicy_denied_command_before_dispatch`
- `pick_and_place_lifecycle_example_runs_end_to_end`

### Observability

- Counter `octos_lifecycle_step_total{phase, outcome}`
- Counter `octos_lifecycle_step_killed_total{phase, reason}` (reason ∈ `timeout`, `critical_failure`, `sandbox_deny`)
- Operator summary key `lifecycle.phase_durations_p95_ms` (bucketed)

### Rollback

`hardware_lifecycle` is opt-in per manifest. Plugins without the field take no lifecycle path.

### Review checklist

1. Is there any `Command::new("sh")` or `Command::new("cmd")` in `lifecycle.rs`? There must not be.
2. Does the timeout test actually assert the child PID is dead (via `/proc` or equivalent) rather than just that the function returned?
3. Is `BLOCKED_ENV_VARS` applied?
4. Does the example run on Linux AND macOS AND Windows (or document the last is CI-only)?

---

## RP03 — Domain-hook pattern via BeforeSpawnVerify + payload extension

**Phase:** RPA
**Depends on:** `main` at `86fe162` or later (`BeforeSpawnVerify` present)
**Reuses from PR #270:** nothing verbatim; salvages the *intent* of robot-specific hook events
**Deletes from PR #270:** `HookEvent::{BeforeMotion, AfterMotion, ForceLimit, WorkspaceBoundary, EmergencyStop}`, `RobotPayload` struct

### Problem

Robot integrators need to veto or modify a dispatched tool call based on
domain context (force/torque limits, workspace-bounds violation,
e-stop status). PR `#270` added five robot-specific `HookEvent` variants
that were never fired by any runtime site — five dead enum labels.

`main` already has `BeforeSpawnVerify` with `Deny` (exit 1) and `Modified`
(exit 2 returning JSON) semantics. The right primitive is: extend the
existing hook payload with an opaque `domain_data` field so integrators
can attach robot context and filter on it in their hook scripts.

### Stable surfaces

- Existing `HookEvent` enum variants — do not add new variants
- Existing `HookPayload` shape — additive only
- Existing `BeforeSpawnVerify` firing sites in `spawn.rs`

### Architectural surface

- Add `HookPayload.domain_data: Option<serde_json::Value>` (serialize-skipped when `None`)
- Add a new agent-level hook-input enricher: `HookPayloadEnricher` trait that, before a hook fires, populates `domain_data` from a runtime-supplied source (robot context, deployment metadata, etc.)
- Document the pattern: robot integrators register an enricher that reads force/torque from their sensor bus and puts it into `domain_data`. Their before-hook script filters on `domain_data.force_n > 40` and denies.
- Provide a tiny reference enricher `StaticDomainDataEnricher` for tests.

### Allowed files

- `crates/octos-agent/src/hooks.rs` — add `domain_data` field + `HookPayloadEnricher` trait
- `crates/octos-agent/src/agent/execution.rs` — thread enricher through call path (additive param on `HookExecutor::new_with_enricher`)
- `crates/octos-agent/src/agent/loop_runner.rs` — plumb context
- `crates/octos-agent/tests/domain_hook.rs` — integration test with a real before-hook script
- `crates/octos-agent/examples/robot_domain_hook.rs` — new example

### Required invariants

1. `HookPayload.domain_data` is `Option<Value>`, serialize-skipped when `None`. Existing payloads unchanged on the wire.
2. `HookPayloadEnricher` is a trait with one method: `fn enrich(&self, event: &HookEvent, payload: &mut HookPayload)`.
3. Payload truncation (`MAX_PAYLOAD_FIELD_BYTES=1024`) applies to `domain_data` serialized form.
4. An integrator who does not register an enricher sees no behavior change.
5. No new `HookEvent` variant is added.
6. The robot example demonstrates denial via `BeforeSpawnVerify` + `domain_data`, not via new events.

### Explicit non-goals

1. Built-in robot-specific enrichers (integrators provide their own).
2. An async enricher that makes IPC calls (enrichers are synchronous, fast-path only).
3. Deprecating any existing hook events.

### Acceptance tests

- `should_include_domain_data_when_enricher_registered`
- `should_omit_domain_data_when_no_enricher`
- `should_truncate_domain_data_at_max_payload_field_bytes`
- `should_deny_before_spawn_verify_when_domain_data_violates`
- `robot_domain_hook_example_runs_end_to_end`

### Observability

- Counter `octos_hook_domain_data_enriched_total{event}` — how often enrichment ran.

### Rollback

`domain_data` is optional; absence of an enricher means no change.

### Review checklist

1. Are any new `HookEvent` variants added? There must not be.
2. Is `domain_data` truncated at `MAX_PAYLOAD_FIELD_BYTES`?
3. Does the example actually deny a motion via `BeforeSpawnVerify`?
4. Is `RobotPayload` from PR #270 absent from the diff?

---

## RP04 — Pipeline deadline and checkpoint enforcement

**Phase:** RPA
**Depends on:** none
**Reuses from PR #270:** `DeadlineAction` enum, `MissionCheckpoint` struct, the `deadline_secs`/`deadline_action`/`checkpoints` fields on `PipelineNode` (`crates/octos-pipeline/src/graph.rs`); the DOT parser extensions for those fields (`crates/octos-pipeline/src/parser.rs`)
**Deletes from PR #270:** `Invariant` struct and its field (unparsed), `HandlerKind::{SensorCheck, Motion, Grasp, SafetyGate, WaitForEvent}` (unregistered)

### Problem

Robot missions need per-node deadlines and checkpoints. PR `#270` added
the data-model fields but no executor actually enforces them — nodes
with `deadline_secs=5.0` ran without any timeout. Checkpoints were
declared but never persisted.

### Stable surfaces

- Existing `HandlerKind` variants (`Codergen`, `Shell`, `Gate`, `Noop`, `DynamicParallel`)
- Existing artifact store pattern for persistence
- Pipeline graph DOT parser

### Architectural surface

- Keep `deadline_secs: f64`, `deadline_action: Option<DeadlineAction>`, `checkpoints: Vec<MissionCheckpoint>` on `PipelineNode`
- Executor wraps node execution in `tokio::time::timeout(deadline)` when `deadline_secs` is set
- On timeout, `deadline_action` determines behavior: `Abort` (fail pipeline), `Skip` (mark node skipped, continue), `Retry { max_attempts }` (retry up to N), `Escalate` (fire `OnSpawnFailure` hook with reason)
- Checkpoint persistence via existing artifact store — after a node with a `MissionCheckpoint`, write a checkpoint record with timestamp + node id + outcome
- Resume semantics: on pipeline restart, read latest checkpoint and skip nodes before it

### Allowed files

- `crates/octos-pipeline/src/graph.rs` — keep deadline/checkpoint fields (already added by PR #270)
- `crates/octos-pipeline/src/parser.rs` — keep deadline/checkpoint DOT parsing (already added)
- `crates/octos-pipeline/src/executor.rs` — enforce deadline + persist checkpoints
- `crates/octos-pipeline/src/checkpoint.rs` — new module, checkpoint store trait
- `crates/octos-pipeline/tests/deadline_enforcement.rs`
- `crates/octos-pipeline/tests/checkpoint_resume.rs`
- `examples/dora-bridge-config/inspection_mission.dot` — rewrite against real enforced deadlines

### Required invariants

1. A node with `deadline_secs=1.0` that sleeps 5s is aborted at 1s. Verified by `deadline_enforcement.rs`.
2. Each `DeadlineAction` variant has distinct, testable behavior.
3. A checkpoint after a node N causes a restart to skip all nodes before and including N.
4. Checkpoints are written atomically (temp file + rename).
5. `Invariant` struct from PR #270 is absent. Pipeline definitions do not parse an `invariants` field.
6. The 5 new `HandlerKind` variants from PR #270 are absent. Pipelines use `Shell` / `Gate` / `Noop` for robot mission steps.

### Explicit non-goals

1. A new handler kind for motion or grasp (these belong to R02 substrate work).
2. Real-time deadline guarantees (`tokio::time::timeout` is best-effort; that's R01).
3. Cross-pipeline checkpoint sharing (per-pipeline only).

### Acceptance tests

- `should_abort_node_when_deadline_exceeded`
- `should_skip_node_when_deadline_action_is_skip`
- `should_retry_node_up_to_max_attempts`
- `should_fire_spawn_failure_hook_when_deadline_action_is_escalate`
- `should_persist_checkpoint_after_node_completion`
- `should_skip_completed_nodes_on_resume_from_checkpoint`

### Observability

- Counter `octos_pipeline_deadline_exceeded_total{action}`
- Counter `octos_pipeline_checkpoint_persisted_total`
- Counter `octos_pipeline_checkpoint_resumed_total`

### Rollback

`deadline_secs` is optional; nodes without it behave as today.

### Review checklist

1. Is the `Invariant` struct absent from `graph.rs`?
2. Are the 5 new `HandlerKind` variants absent?
3. Does the deadline test actually kill the node, not just return early?
4. Is checkpoint write atomic (temp+rename)?

---

## RP05 — Realtime heartbeat + sensor context injection

**Phase:** RPA
**Depends on:** RP03 (domain payload extension)
**Reuses from PR #270:** `crates/octos-agent/src/agent/realtime.rs` (`RealtimeConfig`, `Heartbeat`, `HeartbeatState`, `SensorSnapshot`, `SensorContextInjector`); `crates/octos-agent/examples/realtime_heartbeat.rs`

### Problem

PR `#270` added a 261-LOC `realtime.rs` module that defined
`RealtimeConfig`, `Heartbeat`, and `SensorContextInjector` but wired
none of them to the agent loop. The types existed; no consumer did.

### Stable surfaces

- Existing agent loop shape
- Existing message-building pipeline
- Existing `HookPayloadEnricher` (from RP03)

### Architectural surface

- Keep the five types from PR #270's `realtime.rs` mostly as-is.
- Wire `RealtimeConfig` into `loop_runner.rs`: each loop iteration calls `heartbeat.beat()` and checks `HeartbeatState::Stalled` before proceeding.
- Wire `SensorContextInjector` into message building: when configured, the injector produces a short text summary appended to the system prompt once per turn. Budget-gated (max tokens per turn from config).
- Provide a `RealtimeHookEnricher: HookPayloadEnricher` implementation that attaches the latest `SensorSnapshot` to `HookPayload.domain_data`.
- Config: `ProfileConfig.robot.realtime` typed section (new), with cadence_hz, sensor_budget_tokens, stall_threshold_ms.

### Allowed files

- `crates/octos-agent/src/agent/realtime.rs` — keep mostly as-is from PR #270
- `crates/octos-agent/src/agent/loop_runner.rs` — call `heartbeat.beat()`, check stall
- `crates/octos-agent/src/agent/execution.rs` — inject sensor summary into prompt
- `crates/octos-agent/src/agent/mod.rs` — re-export
- `crates/octos-agent/src/lib.rs` — re-export types (already done by PR #270)
- `crates/octos-agent/examples/realtime_heartbeat.rs` — rewrite assertions against real loop
- `crates/octos-cli/src/profiles.rs` — add `RealtimeConfig` typed section
- `crates/octos-agent/tests/realtime_loop.rs` — integration test

### Required invariants

1. Each agent-loop iteration beats the heartbeat. Verified by asserting beat-count equals iteration-count after a test run.
2. A stalled heartbeat aborts the next iteration with a typed error.
3. Sensor context injection stays within `sensor_budget_tokens`. Over-budget summaries are truncated, not omitted.
4. Missing sensor source degrades silently without aborting the turn.
5. `ProfileConfig.robot.realtime` classified as `RestartRequired` in `diff_profiles`.
6. When realtime config is absent, the agent loop behaves exactly as today.

### Explicit non-goals

1. A fast-loop peer process (that is R01).
2. Sub-millisecond deadline enforcement (that is R01).
3. Specific sensor protocol support (integrators supply their own).

### Acceptance tests

- `should_beat_heartbeat_once_per_loop_iteration`
- `should_abort_iteration_when_heartbeat_stalled`
- `should_inject_sensor_summary_within_budget`
- `should_truncate_oversize_sensor_summary`
- `should_degrade_silently_when_sensor_source_stalls`
- `should_classify_realtime_config_as_restart_required`
- `realtime_heartbeat_example_runs_end_to_end`

### Observability

- Counter `octos_realtime_heartbeat_beats_total`
- Counter `octos_realtime_heartbeat_stalls_total`
- Histogram `octos_realtime_sensor_injection_tokens`
- Operator summary key `realtime.stalls_last_minute`

### Rollback

All realtime behavior is gated by `ProfileConfig.robot.realtime.enabled`. Default false.

### Review checklist

1. Is `loop_runner.rs` actually calling `heartbeat.beat()`?
2. Is the sensor budget a hard ceiling?
3. Does the example assert against real loop ticks, not just construct types?
4. Does `diff_profiles` classify the section as restart-required?

---

## RP06 — octos-dora-mcp real forwarding OR removal

**Phase:** RPA
**Depends on:** none
**Reuses from PR #270:** `crates/octos-dora-mcp/Cargo.toml`, `config.rs` (`BridgeConfig`), `lib.rs` (`DoraToolBridge`, `DoraToolMapping`); `examples/dora-bridge-config/*`

### Problem

PR `#270` added an `octos-dora-mcp` crate whose `DoraToolBridge::execute`
returns a placeholder string. Registering the bridge advertises tools to
the LLM that do not execute. This is a stub that must either become real
or be deleted.

### Stable surfaces

- `Tool` trait signature
- `octos_agent::permissions::SafetyTier` (from RP01)

### Architectural surface

**Option A — Real forwarding (preferred if dora-rs support is wanted):**

- `DoraToolBridge::execute` forwards to a dora-rs dataflow via a configurable transport (default: local zenoh or dora-arrow IPC).
- Feature-gated: `octos-dora-mcp/real-forwarding` controls whether the crate builds with actual dora deps.
- Schema-validated ingress/egress. Backpressure: bounded channels, overflow drops with counter.
- Reuse `octos_agent::permissions::SafetyTier`. Delete the duplicate enum in `octos-dora-mcp`.
- Real integration test against a fixture dora node (CI-gated, skipped when dora is not installed).

**Option B — Removal (if Option A is not funded):**

- Delete the crate entirely.
- Remove from workspace `Cargo.toml` members.
- Delete `examples/dora-bridge-config/`.
- Follow-up issue captures "if we want dora integration, build it real."

The contract owner picks A or B before slice opens. No middle path.

### Allowed files (Option A)

- `crates/octos-dora-mcp/Cargo.toml`
- `crates/octos-dora-mcp/src/config.rs`
- `crates/octos-dora-mcp/src/lib.rs` — rewrite `execute` + delete duplicate `SafetyTier`
- `crates/octos-dora-mcp/src/transport/mod.rs` — new
- `crates/octos-dora-mcp/src/transport/zenoh.rs` — new
- `crates/octos-dora-mcp/tests/forwarding_integration.rs` — new
- `examples/dora-bridge-config/dora_tool_map.json`
- `examples/dora-bridge-config/inspection_mission.dot`
- `examples/dora-bridge-config/README.md`

### Allowed files (Option B)

- `Cargo.toml` workspace member removal
- Deletion of `crates/octos-dora-mcp/`
- Deletion of `examples/dora-bridge-config/`

### Required invariants (Option A)

1. `DoraToolBridge::execute` actually forwards to a dora runtime and returns its real output. Verified by integration test.
2. No duplicate `SafetyTier` enum — the crate imports `octos_agent::permissions::SafetyTier`.
3. Overflow drops are counted; no unbounded buffering.
4. The feature gate `real-forwarding` is off by default on the workspace build; CI enables it for the integration test job.
5. Schema validation rejects malformed messages at ingress.

### Required invariants (Option B)

1. Workspace still compiles after deletion.
2. No remaining `use octos_dora_mcp::...` imports.
3. A successor issue is opened documenting the deferred work.

### Explicit non-goals

1. Support for every dora transport (start with one, ship it).
2. Hot-reload of `BridgeConfig` (restart-required is fine).

### Acceptance tests (Option A)

- `should_forward_execute_to_dora_runtime_and_return_output`
- `should_drop_on_overflow_when_queue_full`
- `should_reject_malformed_dora_message`
- `should_use_octos_agent_safety_tier_not_duplicate`
- `dora_bridge_config_example_runs_end_to_end`

### Observability (Option A)

- Counter `octos_dora_bridge_forwarded_total{node}`
- Counter `octos_dora_bridge_dropped_total{node, reason}`
- Counter `octos_dora_bridge_schema_violations_total`

### Rollback (Option A)

Feature gate off = bridge not registered. Safe default.

### Review checklist

1. If Option A: does `execute()` actually hit a dora runtime? No placeholder strings.
2. If Option A: is `SafetyTier` the one from `octos_agent`?
3. If Option B: is the workspace `Cargo.toml` clean?

---

## Family persistence

This doc is the canonical record for the RP family. Individual issues
live on GitHub under `robotics-rp/<issue-slug>` branches with titles
`RP0N — <title>`. Release contracts per slice follow the format of
`docs/OCTOS_ROBOTICS_RELEASE_<yyyy-mm-dd>.md`.

The family is complete when all of RP01–RP06 are merged to `main` and
the following hold on canary:

- One real-world robot skill is installable from the registry that uses
  `group:robot:safe_motion`, declares a `hardware_lifecycle`, registers a
  domain enricher, and runs a deadlined mission pipeline.
- The example suite (4 examples) is green on CI.
- No `SafetyTier` duplicate exists in the workspace.
- No unfired `HookEvent` variant exists in the workspace.
- Either `octos-dora-mcp` forwards real dora traffic (Option A) or it is
  deleted (Option B).
