# Octos Robotics Contracts — R01 through R10

See also:

- [OCTOS_ROBOTICS_ARCHITECTURE.md](./OCTOS_ROBOTICS_ARCHITECTURE.md)
- [OCTOS_ROBOTICS_FAMILY.md](./OCTOS_ROBOTICS_FAMILY.md)

## Purpose

This document is the contract library for the robotics family. One contract
per issue, `R01` through `R10`. Every slice opens against one or more of
these contracts. A slice does not ship without the contract's acceptance
tests, invariants, observability hooks, and rollback plan.

Contracts are the unit of swarm dispatch. A program manager or architect
hands a worker the contract identifier and the release-slice allowlist. The
worker executes against the contract. The verifier checks against the
contract. The scope guard rejects anything outside the contract.

## Contract Template

Every contract below follows this shape. Nothing in a contract is optional.

- **Phase.** Which family phase from `OCTOS_ROBOTICS_FAMILY.md`.
- **Tier.** Which architectural target from `OCTOS_ROBOTICS_ARCHITECTURE.md`.
- **Blocks.** Contracts that cannot start until this one is green.
- **Blocked by.** Contracts that must be green before this one starts.
- **Problem.** User-visible or operator-visible failure this closes.
- **Stable surfaces.** What this contract must not touch.
- **Architectural surface.** What gets added. New crate, module, trait,
  config section. No hand-waving.
- **Allowed files.** Exact paths. No globs. Adjacent helper files only if
  the helper already exists.
- **Required invariants.** Numbered. Enforceable. Each verifiable by a test
  or a runtime assertion.
- **Explicit non-goals.** Numbered. These close common drift paths before
  they open.
- **Acceptance tests.** Named tests. Locations. `should_<expected>_when_<condition>`.
- **Observability.** Counters, events, operator-summary keys.
- **Rollback.** Feature flag, config default, shadow mode.
- **Review checklist.** Yes-or-no questions the verifier answers before
  green.

## Dependency Graph

```
Phase RA:  R01 ──┬──► R03 ──► R04
                 │
Phase RB:        ├──► R02
                 │
                 └──► R05 ──► R06
                               │
Phase RC:                      ├──► R07
                               ├──► R08
                               └──► R09
                                    │
Phase RD:                           └──► R10
```

- `R01` unblocks the whole family
- `R03` depends on `R01` because the supervisor lives in the fast loop
- `R04` depends on `R01` because the bridge feeds the fast loop
- `R05` depends on the existing workspace contract being green on canary
- `R06` depends on `R04` because sensors arrive through the bridge
- `R07` depends on `R03` because the recorder captures safety events
- `R08` depends on `R03` because HITL is a safety primitive
- `R09` depends on `R04` because sim is another bridge adapter
- `R10` depends on `R01`, `R03`, `R05`

No contract outside this graph may be opened under the `R` family.

---

## R01 — Fast-Loop Peer

**Phase:** RA
**Tier:** T1.1 (Control-plane / data-plane split)
**Blocks:** R02, R03, R04, R10
**Blocked by:** harness branch `release/2026-04-17-harness-gate-local` green on canary

### Problem

A blocked LLM tool call can stall the only runtime loop. On a physical
robot this is a mechanical failure or a safety event. Current Octos does
not separate LLM cadence from hardware cadence.

### Stable Surfaces

- `crates/octos-agent/src/agent/loop_runner.rs` public shape
- existing `Tool` trait
- existing session, topic, workspace identity
- `crates/octos-pipeline` public API

### Architectural Surface

New crate `crates/octos-realtime/`.

- binary-target `octos-realtime` runs a deterministic tick loop with a
  configurable cadence
- in-process fallback: a dedicated tokio task with its own runtime, not
  sharing the agent runtime
- typed `Goal` protocol posted from the agent, `StateReport` protocol
  published by the fast loop
- transport: local stdio JSON-lines for process mode, in-process mpsc for
  task mode; shaped so a later transport swap does not change the Rust
  call sites
- new `ProfileConfig.robot.realtime` typed section carrying cadence_hz,
  goal_timeout_ms, transport, missed_tick_policy

### Allowed Files

- `crates/octos-realtime/Cargo.toml`
- `crates/octos-realtime/src/lib.rs`
- `crates/octos-realtime/src/goal.rs`
- `crates/octos-realtime/src/state.rs`
- `crates/octos-realtime/src/tick.rs`
- `crates/octos-realtime/src/main.rs`
- `crates/octos-realtime/tests/tick_jitter.rs`
- `crates/octos-cli/src/profiles.rs` — add `RealtimeConfig` typed section
- `crates/octos-cli/src/commands/gateway/gateway_runtime.rs` — wire config load
- `Cargo.toml` workspace member add

### Required Invariants

1. The fast loop's tick jitter stays under the declared cadence budget under
   sustained LLM load. Verified by `tick_jitter.rs`.
2. A 30-second blocking tool call on the agent loop does not cause any
   fast-loop tick to be skipped. Verified by a test that spawns a blocking
   tool and asserts zero missed ticks.
3. No agent-loop code imports from `octos-realtime` except through the
   typed `Goal`/`StateReport` protocol.
4. No `unsafe` code is added.
5. The fast loop does not take a dependency on `octos-llm`.
6. The `RealtimeConfig` section is hot-reload-classified in `diff_profiles`
   as `RestartRequired`.

### Explicit Non-Goals

1. Implementing any motion primitives in this contract.
2. Implementing any safety checks in this contract — those are `R03`.
3. Implementing any bridge to ROS2 or dora-rs — that is `R04`.
4. Adding any `HookEvent` variant.

### Acceptance Tests

- `should_hold_tick_cadence_when_agent_loop_blocked` in `tests/tick_jitter.rs`
- `should_deliver_goal_with_bounded_latency_when_queue_non_empty`
- `should_reject_goal_with_invalid_schema`
- `diff_profiles::should_mark_realtime_section_as_restart_required`

### Observability

- Counter `octos_realtime_ticks_total`
- Counter `octos_realtime_missed_ticks_total`
- Histogram `octos_realtime_tick_jitter_us`
- Counter `octos_realtime_goals_received_total`
- Counter `octos_realtime_goals_rejected_total{reason}`
- Operator summary key `realtime.missed_ticks`

### Rollback

Gate the whole fast-loop wiring behind `ProfileConfig.robot.realtime.enabled`.
Default false. Without the flag, Octos behaves exactly as before.

### Review Checklist

1. Is the fast loop unreachable from any `octos-agent` code path except the
   typed protocol?
2. Does `diff_profiles` classify `realtime` as restart-required?
3. Is the feature off by default?
4. Is `tick_jitter.rs` green with the declared cadence?

---

## R02 — Pipeline-As-Substrate

**Phase:** RB
**Tier:** T1.2
**Blocks:** R05, R10
**Blocked by:** R01

### Problem

Today the agent loop drives every step of a task. For robot missions this
puts LLM latency in the hot path of deterministic execution. The pipeline
engine already exists but is not the default execution substrate for robot
tasks.

### Stable Surfaces

- existing `octos-pipeline` public API
- existing `octos-pipeline::graph::HandlerKind` variants and their executors
- existing agent loop shape
- existing session and workspace identity

### Architectural Surface

- new module `crates/octos-pipeline/src/mission.rs` introducing a
  `MissionExecutor` that drives a pipeline graph to completion, reporting
  progress as typed events
- new module `crates/octos-agent/src/agent/mission_bridge.rs` that lets
  the agent loop hand a mission to `MissionExecutor` and re-enter only on
  `MissionException`
- new `ProfileConfig.robot.mission` typed section with default
  replan-on-exception policy, max replans per mission, replan budget
- new `HandlerKind` variants are not added in this contract

### Allowed Files

- `crates/octos-pipeline/src/mission.rs`
- `crates/octos-pipeline/src/lib.rs` — re-export
- `crates/octos-pipeline/tests/mission_executor.rs`
- `crates/octos-agent/src/agent/mission_bridge.rs`
- `crates/octos-agent/src/agent/mod.rs` — re-export only
- `crates/octos-agent/src/agent/loop_runner.rs` — new branch calling
  mission bridge when mission present on the turn
- `crates/octos-cli/src/profiles.rs` — add `MissionConfig` typed section

### Required Invariants

1. When a mission is active and has not raised `MissionException`, no LLM
   call is issued for the duration of the mission.
2. The agent loop re-enters exactly once per `MissionException`.
3. `MissionExecutor` never mutates session state that belongs to the agent
   loop, and vice versa.
4. Existing non-mission turns go through the existing `loop_runner` path
   unchanged. Verified by existing agent tests still being green.
5. `MissionConfig.max_replans` is strictly enforced.

### Explicit Non-Goals

1. Adding new `HandlerKind` variants.
2. Adding motion-specific executors.
3. Adding mission contract predicates — those are `R05`.

### Acceptance Tests

- `should_drive_pipeline_without_agent_reentry_when_mission_runs_clean`
- `should_reenter_agent_exactly_once_per_mission_exception`
- `should_respect_max_replans_budget`
- `should_leave_existing_non_mission_turns_unchanged`

### Observability

- Counter `octos_mission_runs_total`
- Counter `octos_mission_exceptions_total{kind}`
- Counter `octos_mission_replans_total`
- Histogram `octos_mission_wall_seconds`
- Operator summary key `mission.in_flight`

### Rollback

Mission path is opt-in per turn by presence of a mission descriptor. No
mission descriptor, no new code path.

### Review Checklist

1. Is the non-mission turn path byte-for-byte unchanged in
   `loop_runner.rs`?
2. Does `mission_bridge` own all cross-crate glue, leaving `octos-pipeline`
   free of `octos-agent` imports?
3. Is `max_replans` a hard stop, not a warning?

---

## R03 — Safety Supervisor

**Phase:** RA
**Tier:** T1.3
**Blocks:** R07, R08, R10
**Blocked by:** R01

### Problem

Octos has no trust-domain separation between the LLM and safety. Any
safety claim we ship today is carried by prompt text or trait declarations
that no enforcer reads. On a physical robot that is unsafe.

### Stable Surfaces

- `Tool` trait `spec()` and `execute()` signatures
- existing `ToolPolicy` allow/deny semantics
- existing sandbox, `BLOCKED_ENV_VARS`, `SafePolicy`

### Architectural Surface

New crate `crates/octos-safety/`.

- types: `SafetyTier`, `SafetyInvariant`, `Violation`, `SafeState`
- trait `SafetySupervisor` with `authorize(tier, context) -> Decision`,
  `evaluate(state) -> Vec<Violation>`, `on_violation(violation) -> SafeState`
- new `Tool::required_safety_tier()` trait method with default
  `SafetyTier::Observe`
- enforcement wired in `agent/loop_runner.rs` at tool dispatch:
  `supervisor.authorize(tool.required_safety_tier(), ctx)` must return
  `Decision::Allow` or the tool call is denied with a `Violation` record
- fast-loop integration from `R01`: supervisor runs on every tick; a
  `Violation` transitions the fast loop to `SafeState` without slow-loop
  cooperation
- new `ProfileConfig.robot.safety` typed section with tier policy, invariant
  configuration, safe-state definition

### Allowed Files

- `crates/octos-safety/Cargo.toml`
- `crates/octos-safety/src/lib.rs`
- `crates/octos-safety/src/tier.rs`
- `crates/octos-safety/src/invariant.rs`
- `crates/octos-safety/src/supervisor.rs`
- `crates/octos-safety/tests/enforcement.rs`
- `crates/octos-agent/src/tools/mod.rs` — add trait default method
- `crates/octos-agent/src/agent/loop_runner.rs` — authorize at dispatch
- `crates/octos-realtime/src/tick.rs` — evaluate on tick
- `crates/octos-cli/src/profiles.rs` — add `SafetyConfig` typed section
- `Cargo.toml` workspace member add

### Required Invariants

1. The LLM has no code path that overrides a supervisor veto.
2. A supervisor veto is deterministic given its input state. No RNG, no
   LLM, no network.
3. Tool dispatch in `loop_runner.rs` always consults `authorize()` before
   `execute()`. Verified by a test that mocks `authorize()` to deny and
   asserts the tool's `execute` was never called.
4. An invariant violation on the fast loop transitions to `SafeState`
   within one tick.
5. `SafetyConfig` is hot-reload-classified as `RestartRequired`.
6. The crate has zero dependency on `octos-llm`.

### Explicit Non-Goals

1. Landing specific motion-safety invariants — this contract lands the
   framework only. Specific invariants come in dedicated follow-up
   contracts outside this family scope.
2. Landing a black-box recorder — that is `R07`.
3. Landing HITL authorization — that is `R08`.

### Acceptance Tests

- `should_deny_tool_call_when_supervisor_vetoes`
- `should_transition_fast_loop_to_safe_state_within_one_tick_on_violation`
- `should_reject_llm_path_that_tries_to_override_veto`
- `should_classify_safety_config_as_restart_required`
- `should_compose_with_existing_tool_policy_without_regression`

### Observability

- Counter `octos_safety_authorize_total{decision}`
- Counter `octos_safety_violations_total{invariant}`
- Counter `octos_safety_safe_state_transitions_total`
- Operator summary keys `safety.violations`, `safety.safe_state_active`

### Rollback

Gate supervisor enforcement behind `ProfileConfig.robot.safety.enabled`.
Default false. When false, `authorize()` always returns `Allow` and
tier evaluation is skipped.

### Review Checklist

1. Does `loop_runner.rs` consult `authorize()` at every tool dispatch
   when safety is enabled?
2. Is there any code path where an LLM decision can override the
   supervisor? Prove the negative.
3. Is the supervisor's decision function deterministic?

---

## R04 — Robotics Bridge

**Phase:** RA
**Tier:** T2.1
**Blocks:** R06, R09
**Blocked by:** R01

### Problem

Octos has no typed, backpressure-aware bridge to existing robotics stacks.
MCP over stdio is too slow and too unstructured for high-frequency
topics. A naive bridge buffers sensor streams without bound.

### Stable Surfaces

- existing MCP server shape for tool-shaped calls (tools keep working)
- `Tool` trait
- `BLOCKED_ENV_VARS`, `SafePolicy`, sandbox

### Architectural Surface

New crate `crates/octos-robotics-bridge/`.

- trait `RoboticsBridge` with `publish(topic, msg)`, `subscribe(topic,
  qos, sink)`, `call(service, args)`
- typed `QoS` enum with `Reliable`, `BestEffort`, `DropOldest{depth}`,
  `DropNewest{depth}`
- built-in adapters: `StdioJsonlAdapter`, `DoraAdapter`, `Ros2Adapter`
- bounded channels everywhere. Overflow counts, never buffers.
- schema validation at ingress. Violations increment counter, drop
  message.
- new `ProfileConfig.robot.bridge` typed section with adapter choice,
  per-topic QoS, per-topic schema paths

### Allowed Files

- `crates/octos-robotics-bridge/Cargo.toml`
- `crates/octos-robotics-bridge/src/lib.rs`
- `crates/octos-robotics-bridge/src/qos.rs`
- `crates/octos-robotics-bridge/src/adapter/stdio.rs`
- `crates/octos-robotics-bridge/src/adapter/dora.rs`
- `crates/octos-robotics-bridge/src/adapter/ros2.rs`
- `crates/octos-robotics-bridge/src/schema.rs`
- `crates/octos-robotics-bridge/tests/backpressure.rs`
- `crates/octos-realtime/src/tick.rs` — consume subscriptions
- `crates/octos-cli/src/profiles.rs` — add `BridgeConfig` typed section
- `Cargo.toml` workspace member add

### Required Invariants

1. No channel in the bridge is unbounded.
2. On overflow, the configured `QoS` policy is honored. Never silent buffer
   growth. Verified by `backpressure.rs`.
3. Every subscription validates message schema at ingress. Unknown topics
   or invalid schemas are rejected with a counter bump, not a panic.
4. The bridge does not share env with child processes beyond
   `BLOCKED_ENV_VARS`-sanitized surfaces.
5. Adapter choice is a `RestartRequired` field.

### Explicit Non-Goals

1. Implementing dora-rs or ROS2 wire protocol by hand. Use established
   Rust crates per adapter. If no mature crate exists, leave the adapter
   as an explicit `Unimplemented` that returns an error, not a stub.
2. Registering the bridge as an LLM-callable tool. The bridge is for the
   fast loop, not the tool registry.
3. Landing sensor-to-context policy — that is `R06`.

### Acceptance Tests

- `should_drop_oldest_when_queue_full_with_drop_oldest_qos`
- `should_reject_message_with_invalid_schema`
- `should_survive_adapter_disconnect_without_memory_growth`
- `should_never_buffer_past_configured_depth`

### Observability

- Counter `octos_bridge_messages_in_total{topic,adapter}`
- Counter `octos_bridge_messages_dropped_total{topic,reason}`
- Gauge `octos_bridge_queue_depth{topic}`
- Counter `octos_bridge_schema_violations_total{topic}`
- Operator summary key `bridge.drops_last_minute`

### Rollback

Gate each adapter behind its own config field. Default to no adapters
enabled.

### Review Checklist

1. Is every queue bounded?
2. Does overflow behavior exactly match the declared QoS?
3. Is there a path where schema validation is skipped?
4. Is the bridge visible to the LLM tool registry? It must not be.

---

## R05 — Mission Contract

**Phase:** RB
**Tier:** T2.2
**Blocks:** R10
**Blocked by:** R02, and the existing workspace contract being green on canary

### Problem

The workspace contract asserts filesystem state at turn boundaries. Robot
missions need pre-conditions, invariants over time, and post-conditions.
Without these, a mission's terminal success cannot be validated the way a
coding task's can.

### Stable Surfaces

- existing `workspace_contract.rs` public types and semantics
- existing turn-end gate path from the harness branch

### Architectural Surface

Extension, not replacement, of `workspace_contract.rs`.

- new types `MissionContract { pre: Vec<Predicate>, invariants:
  Vec<Predicate>, post: Vec<Predicate> }`
- new predicate kinds: `HardwareReady`, `Calibrated`, `WorkspaceClear`,
  `WithinEnvelope`, `UnderForceLimit`, `UnderSpeedLimit`, `ToolParked`,
  `PowerSafe`, `BlackBoxFlushed`
- pre-conditions checked at mission start by `MissionExecutor` from `R02`
- invariants checked on every fast-loop tick by the supervisor from `R03`
- post-conditions checked at mission end by the existing turn-end gate,
  extended to recognize `MissionContract`

### Allowed Files

- `crates/octos-agent/src/workspace_contract.rs` — new `MissionContract`
  type and `MissionPredicate` enum, additive
- `crates/octos-agent/src/mission_gate.rs` — new module, mission-specific
  gate helpers
- `crates/octos-agent/tests/mission_gate.rs`
- `crates/octos-pipeline/src/mission.rs` — consume pre and post
- `crates/octos-safety/src/supervisor.rs` — consume invariants
- `crates/octos-cli/src/profiles.rs` — add `MissionContractConfig` typed
  section
- `crates/octos-cli/src/session_actor.rs` — extend existing turn-end gate

### Required Invariants

1. `MissionContract` is a strict superset of `WorkspaceContract`. No
   existing workspace-contract field changes meaning.
2. Pre-condition failure prevents the mission from starting and records a
   `PreConditionFailure` event.
3. Invariant violation during mission causes immediate abort via the
   supervisor from `R03`.
4. Post-condition failure blocks terminal success in exactly the same way
   workspace contract failure does today.
5. No new LLM call path is introduced by this contract. The LLM is told
   the verdict; it does not produce it.

### Explicit Non-Goals

1. Adding the actual force or speed thresholds for a specific robot. Those
   live in per-deployment config, not here.
2. Changing workspace contract semantics for non-mission sessions.

### Acceptance Tests

- `should_block_mission_start_when_pre_condition_fails`
- `should_abort_mission_when_invariant_violated_mid_run`
- `should_block_terminal_success_when_post_condition_fails`
- `should_leave_workspace_contract_semantics_unchanged_for_coding_sessions`

### Observability

- Counter `octos_mission_pre_failures_total{predicate}`
- Counter `octos_mission_invariant_violations_total{predicate}`
- Counter `octos_mission_post_failures_total{predicate}`
- Operator summary keys `mission.pre_failures`,
  `mission.invariant_violations`, `mission.post_failures`

### Rollback

Mission contract is opt-in by presence in the session descriptor. Absent,
turn-end gate behaves exactly as on harness branch.

### Review Checklist

1. Do coding-session tests stay green unchanged?
2. Does pre-condition failure prevent any motion from being commanded?
3. Is the invariant evaluation the supervisor's, not the LLM's?

---

## R06 — Sensor-Context Policy

**Phase:** RB
**Tier:** T2.3
**Blocks:** —
**Blocked by:** R04

### Problem

A robot generates more sensor data per second than an LLM can consume. A
naive "pass all sensors in context" approach either blows the prompt
budget or drops important data silently.

### Stable Surfaces

- existing context engineering path
- existing compaction logic in `octos-agent/src/compaction.rs`
- existing message and memory shape

### Architectural Surface

New module `crates/octos-agent/src/sensor_context.rs`.

- policy enum `SensorPolicy { Periodic { hz }, EventTriggered { predicate },
  AttentionGate { cond } }`
- trait `SensorSummarizer` with `summarize(samples) -> String`
- budget type `SensorBudget { max_tokens_per_turn }`
- injection site in the system-prompt builder, gated on budget
- config on `ProfileConfig.robot.sensor_context`

### Allowed Files

- `crates/octos-agent/src/sensor_context.rs`
- `crates/octos-agent/src/agent/mod.rs` — re-export
- `crates/octos-agent/src/agent/execution.rs` — inject at prompt build
- `crates/octos-agent/tests/sensor_context.rs`
- `crates/octos-cli/src/profiles.rs` — `SensorContextConfig` typed section

### Required Invariants

1. Sensor injection never exceeds `max_tokens_per_turn`. Verified with a
   property test that synthesizes high-rate streams.
2. Attention gates evaluate without invoking the LLM. No inference in the
   injection path.
3. Missing or stalled sensor source degrades silently without failing the
   turn.
4. Sensor summarization runs synchronously within a declared time budget.
   Over-budget summaries are truncated, not omitted.

### Explicit Non-Goals

1. Providing default summarizers for specific sensors. The framework lands
   here; per-sensor summarizers live with per-deployment config.
2. Introducing a new memory store. Use the existing one.

### Acceptance Tests

- `should_never_exceed_max_tokens_per_turn_under_high_rate_stream`
- `should_include_force_sensor_only_when_in_contact_gate_true`
- `should_degrade_silently_when_sensor_source_stalls`
- `should_truncate_over_budget_summary_not_omit`

### Observability

- Histogram `octos_sensor_context_tokens_used`
- Counter `octos_sensor_context_truncations_total`
- Counter `octos_sensor_context_gate_evaluations_total{gate,result}`
- Operator summary key `sensor_context.tokens_last_turn`

### Rollback

Off by default. Enable only when at least one sensor policy is configured.

### Review Checklist

1. Does the injection code path have zero LLM calls?
2. Is the budget a hard ceiling?
3. Does compaction continue to work unchanged for non-robot sessions?

---

## R07 — Black-Box Recorder

**Phase:** RC
**Tier:** T3.1
**Blocks:** —
**Blocked by:** R03

### Problem

After a safety event or failure, we have no tamper-evident, monotonically
timestamped, replayable record of inputs and decisions. This is required
for ISO 10218, IEC 61508, and ISO 15066 postures.

### Stable Surfaces

- existing episode store
- existing log pipeline

### Architectural Surface

New crate `crates/octos-blackbox/`.

- typed event schema covering: goal issued, state observed, supervisor
  decision, violation, tool call, LLM message, channel message
- monotonic source is a wrapped `std::time::Instant` synced once at
  startup against `SystemTime` for wall-clock correspondence
- hash chain: each record carries a `prev_hash` field; verifier replays
  and checks the chain
- storage format: length-delimited binary records, one file per session
  with rotation on size
- `fsync_data` after every record
- replay tool `octos blackbox replay <file>` that reproduces the event
  stream for a given session

### Allowed Files

- `crates/octos-blackbox/Cargo.toml`
- `crates/octos-blackbox/src/lib.rs`
- `crates/octos-blackbox/src/chain.rs`
- `crates/octos-blackbox/src/writer.rs`
- `crates/octos-blackbox/src/replay.rs`
- `crates/octos-blackbox/tests/tamper.rs`
- `crates/octos-blackbox/tests/crash_safe.rs`
- `crates/octos-cli/src/commands/blackbox.rs` — new subcommand
- `crates/octos-cli/src/main.rs` — subcommand wire-up
- `crates/octos-safety/src/supervisor.rs` — emit events
- `crates/octos-realtime/src/tick.rs` — emit events
- `crates/octos-cli/src/profiles.rs` — `BlackBoxConfig` typed section

### Required Invariants

1. Every record is hash-chained to its predecessor. Tampering detected on
   replay. Verified by `tamper.rs`.
2. A crash between two records loses at most the uncommitted record.
   Verified by `crash_safe.rs` using `fsync_data`.
3. Timestamps are monotonic within a session. Wall-clock correspondence
   is recorded once at session start.
4. The recorder does not block the fast loop. A full queue drops
   non-critical records before safety records.
5. Replay reproduces the event stream byte-for-byte identical given the
   same file.

### Explicit Non-Goals

1. Implementing a GUI or dashboard viewer. That is a follow-on.
2. Encryption at rest. That is a follow-on.
3. Replacing the episode store. The recorder is complementary.

### Acceptance Tests

- `should_detect_tamper_when_record_mutated`
- `should_lose_at_most_one_record_on_crash`
- `should_emit_monotonic_timestamps_within_session`
- `should_drop_non_critical_record_before_safety_record_under_full_queue`
- `should_replay_event_stream_byte_for_byte`

### Observability

- Counter `octos_blackbox_records_written_total{kind}`
- Counter `octos_blackbox_records_dropped_total{kind}`
- Counter `octos_blackbox_chain_verifications_total{result}`
- Operator summary key `blackbox.last_written_seq`

### Rollback

Off by default. The rest of the runtime does not observe the recorder's
absence.

### Review Checklist

1. Does every record call through `fsync_data`?
2. Is the chain verified by the replay tool before it yields records?
3. Do safety records outrank all others under pressure?

---

## R08 — HITL Authorization

**Phase:** RC
**Tier:** T3.2
**Blocks:** —
**Blocked by:** R03

### Problem

High-consequence actions need supervisory approval with a time-to-live
and default-deny on timeout. The gateway has transports; it does not have
a typed authorization primitive with safe-state fallback.

### Stable Surfaces

- existing channel bus transports (Telegram, Slack, Matrix, etc.)
- existing session identity

### Architectural Surface

New module `crates/octos-bus/src/hitl.rs`.

- types `AuthorizationRequest { action, ttl, default_on_timeout }`,
  `AuthorizationResponse { decision, approver, at }`
- default-on-timeout is restricted to `Deny` or `Safe`. `Allow` is not a
  valid default.
- dispatch path: supervisor requests authorization, HITL manager sends via
  channel bus, awaits with TTL, returns decision or default
- config on `ProfileConfig.robot.hitl`

### Allowed Files

- `crates/octos-bus/src/hitl.rs`
- `crates/octos-bus/src/lib.rs` — re-export
- `crates/octos-bus/tests/hitl_timeout.rs`
- `crates/octos-safety/src/supervisor.rs` — request authorization
- `crates/octos-cli/src/profiles.rs` — `HitlConfig` typed section

### Required Invariants

1. A request whose TTL expires without response resolves to its
   `default_on_timeout`. No exceptions.
2. A response arriving after TTL is recorded but ignored for the
   decision.
3. `default_on_timeout = Allow` is rejected at config-load time.
4. The HITL path is not accessible to the LLM as a tool.
5. Every request and response is black-boxed via `R07`.

### Explicit Non-Goals

1. Implementing UI. Reuse existing channel transports.
2. Rate limiting per approver. Follow-on.

### Acceptance Tests

- `should_resolve_to_deny_on_timeout`
- `should_ignore_response_after_ttl`
- `should_reject_allow_as_default_on_timeout_at_config_load`
- `should_black_box_request_and_response`

### Observability

- Counter `octos_hitl_requests_total{action,outcome}`
- Histogram `octos_hitl_latency_ms{outcome}`
- Operator summary key `hitl.pending`

### Rollback

Off by default. When off, supervisor paths that would request HITL treat
the absence as `Deny`.

### Review Checklist

1. Is `Allow` ever a valid default on timeout?
2. Can the LLM route its own request through HITL bypassing the
   supervisor?
3. Are both request and response black-boxed?

---

## R09 — Simulation Parity

**Phase:** RC
**Tier:** T3.3
**Blocks:** —
**Blocked by:** R04

### Problem

Robot code cannot enter CI without a deterministic simulation path that
speaks the same interface as hardware. Without parity, every test runs
only on the rig.

### Stable Surfaces

- `RoboticsBridge` trait from `R04`
- existing CI runner

### Architectural Surface

New crate `crates/octos-sim/`.

- trait implementation: `SimBridge` implementing `RoboticsBridge`
- deterministic seeding: single `u64` seed drives every stochastic
  sensor, every timing jitter, every environment perturbation
- time control: `TimeScale::Real`, `TimeScale::Accelerated(factor)`,
  `TimeScale::Stepped`
- adapter to an existing simulator via `SimBackend` enum: `Mujoco`,
  `Gazebo`, `Isaac`. Only one backend may be implemented in this contract;
  the remaining are `Unimplemented` errors, not stubs.

### Allowed Files

- `crates/octos-sim/Cargo.toml`
- `crates/octos-sim/src/lib.rs`
- `crates/octos-sim/src/backend.rs`
- `crates/octos-sim/src/seed.rs`
- `crates/octos-sim/src/time.rs`
- `crates/octos-sim/tests/determinism.rs`
- `crates/octos-sim/tests/parity.rs`
- `crates/octos-cli/src/profiles.rs` — `SimConfig` typed section

### Required Invariants

1. The same seed and the same agent program produce the same sim trace.
   Verified by `determinism.rs`.
2. The same agent program compiles and runs against `SimBridge` and the
   chosen hardware adapter without source changes. Verified by
   `parity.rs`.
3. `TimeScale::Accelerated` does not violate any invariant from `R03`.
4. Only one backend is implemented in this contract. Others return
   `Unimplemented`.

### Explicit Non-Goals

1. Shipping MuJoCo, Gazebo, or Isaac binaries. Users install themselves.
2. A visualization layer.

### Acceptance Tests

- `should_produce_identical_trace_for_identical_seed`
- `should_run_same_program_against_sim_and_hardware_without_source_change`
- `should_reject_unimplemented_backend_at_load_time_not_at_call_time`

### Observability

- Counter `octos_sim_ticks_total{backend}`
- Histogram `octos_sim_real_time_ratio{backend}`

### Rollback

Sim is opt-in by `ProfileConfig.robot.sim.enabled`. No default backend.

### Review Checklist

1. Is the seed plumbed through every stochastic source?
2. Does the parity test compile against both bridges?
3. Are unimplemented backends failing at load, not at call?

---

## R10 — Cell Orchestration

**Phase:** RD
**Tier:** T3.4
**Blocks:** —
**Blocked by:** R01, R03, R05

### Problem

A cell with two or more robots, shared tool changers, conveyors, and
dependencies cannot be coordinated by per-session primitives alone. Cell
concerns are resources, roles, priorities, and deadlock avoidance.

### Stable Surfaces

- existing session, topic, workspace identity
- existing gateway channel semantics
- every contract R01 through R09

### Architectural Surface

New crate `crates/octos-cell/`.

- types `Cell`, `Robot`, `Resource`, `Role`, `Priority`, `Claim`
- deadlock avoidance by Banker's-algorithm-style resource claim ordering
- per-cell mission contract composition: cell-level `MissionContract`
  composes from per-robot contracts
- operator surface: per-cell summary under existing operator summary

### Allowed Files

- `crates/octos-cell/Cargo.toml`
- `crates/octos-cell/src/lib.rs`
- `crates/octos-cell/src/resource.rs`
- `crates/octos-cell/src/scheduler.rs`
- `crates/octos-cell/tests/deadlock.rs`
- `crates/octos-cli/src/profiles.rs` — `CellConfig` typed section
- `crates/octos-cli/src/api/admin.rs` — operator summary extension

### Required Invariants

1. No execution order leads to deadlock among declared resources.
   Verified by `deadlock.rs` using an adversarial schedule.
2. A cell-level mission contract fails if any constituent robot's
   contract fails. Composition is conjunctive.
3. Resource claims are explicit and time-bounded. A claim past its TTL is
   released.
4. Cell operations do not change single-robot semantics.

### Explicit Non-Goals

1. Dynamic resource discovery. Cells are declared, not inferred.
2. Cross-cell coordination. Out of scope for this contract.

### Acceptance Tests

- `should_never_deadlock_under_adversarial_schedule`
- `should_fail_cell_mission_if_any_robot_contract_fails`
- `should_release_claim_past_ttl`
- `should_leave_single_robot_semantics_unchanged`

### Observability

- Counter `octos_cell_claims_total{resource,outcome}`
- Counter `octos_cell_deadlock_avoidances_total`
- Operator summary key `cell.active_claims`

### Rollback

Off by default. A single-robot deployment never instantiates a `Cell`.

### Review Checklist

1. Is the resource algorithm deterministic and bounded?
2. Are single-robot tests still green?
3. Does the cell contract compose conjunctively from per-robot
   contracts?

---

## Contract Hygiene Rules

These apply to every contract in this family.

1. A contract is immutable once a slice has opened against it. Amendments
   open a new contract or roll to the next phase.
2. A contract's allowed-files list is an allowlist, not a hint.
3. A contract's invariants must be testable. A claim with no test is not
   an invariant.
4. A contract's observability section is mandatory. Without counters and
   operator-summary keys, no slice is green.
5. A contract's rollback must be usable. "Revert the PR" is not rollback.
6. No contract may weaken `deny(unsafe_code)`, `BLOCKED_ENV_VARS`,
   `O_NOFOLLOW`, `SafePolicy`, or the existing tool sandbox.
7. No contract may add a doc that describes unimplemented behavior.
8. No contract may add a stub crate. A crate enters the workspace when it
   forwards real work.
9. No contract may add a Python port of any Rust runtime component into
   the monorepo.
10. Every contract review is gated by the Safety Gate role from
    `OCTOS_ROBOTICS_FAMILY.md`, with veto power overriding every other
    role.
