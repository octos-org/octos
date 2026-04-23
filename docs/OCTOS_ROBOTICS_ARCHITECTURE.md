# Octos Robotics Architecture

See also:

- [OCTOS_ROBOTICS_FAMILY.md](./OCTOS_ROBOTICS_FAMILY.md)
- [OCTOS_ROBOTICS_CONTRACTS.md](./OCTOS_ROBOTICS_CONTRACTS.md)
- [OCTOS_HARNESS_MIGRATION_GUARDRAILS.md](./OCTOS_HARNESS_MIGRATION_GUARDRAILS.md)
- [OCTOS_HARNESS_ENGINEERING.md](./OCTOS_HARNESS_ENGINEERING.md)

## Purpose

This document defines the architectural target for supporting robotics use cases
on top of the existing Octos harness layer.

It answers two questions:

1. What in the current runtime must change — not bolt on — to run a robot with
   the same trust level the harness already gives a coding session?
2. What looks like a robotics feature but is actually a type declaration, a
   manifest field, or a new example, and therefore does not belong in this
   architecture document?

The document is deliberately shorter than the full robotics wishlist. Its job
is to name the load-bearing shifts, rank them by impact, and rule out
non-architectural proposals that have previously been pitched as architecture.

## Working Definition

For Octos, robotics support means:

- a two-cadence runtime — a deterministic fast loop owning hardware, and the
  existing LLM-driven slow loop owning task-level planning
- a safety supervisor that is not an LLM participant
- typed, backpressure-aware bridges to existing robotics stacks
- the harness contract language extended from filesystem state to temporal
  hardware state

This is narrower than "robotics platform" and broader than "add a ROS tool."

## Why A New Architecture Note At All

Robotics is not just another app vertical like slides or sites. A coding
session that stalls for 30 seconds is annoying. A motion command that stalls
for 30 seconds is a mechanical failure, a safety event, or both.

The existing harness layer assumes:

- tools can be slow
- retries are safe
- state lives in the filesystem
- the only decision-maker is the LLM

Every one of those assumptions breaks on a physical robot. This document
records what we change to fix that, and what we refuse to change so the rest of
Octos keeps working.

## Stable Surfaces — Do Not Rebuild

These surfaces are stable for the robotics program, per
[OCTOS_HARNESS_MIGRATION_GUARDRAILS.md](./OCTOS_HARNESS_MIGRATION_GUARDRAILS.md).

They may be extended. They must not be replaced.

- account / profile / sub-account model
- session / topic / workspace model
- existing agent loop and tool registry
- gateway / channel / SSE / background task transport
- memory store / episode store
- pipeline engine (`octos-pipeline`)
- workspace contract (`crates/octos-agent/src/workspace_contract.rs`)
- typed profile config (`crates/octos-cli/src/profiles.rs`)

Anything that claims to be a robotics feature but requires replacing one of
these is a rewrite, not a robotics addition.

## Architectural Targets

Ranked by how much they change the shape of the platform.

### Tier 1 — Reshape The Loop

These change the core topology. Nothing in Tier 2 or Tier 3 is stable without
them.

**T1.1 Control-plane / data-plane split.**
The agent loop runs at LLM cadence (seconds, variable). Hardware runs at
control cadence (1-100Hz, bounded). They cannot share a thread or a tokio
task. The fast loop becomes a peer process with its own state machine. The
agent loop posts goals and reads state. It never executes motion directly.

Consequence: a blocked LLM call can no longer block a motion command or an
e-stop handler.

**T1.2 Pipeline-as-substrate.**
For robot tasks, the pipeline engine becomes the execution substrate. The
agent populates or edits a pipeline graph once, the pipeline executes
deterministically, and the agent re-enters only on exception or replanning
need. The agent stops being the per-step driver.

Consequence: the LLM's role becomes planner and repairer, not per-tick
controller. This is the single largest product shift.

**T1.3 Safety as a separate trust domain.**
Safety is not an LLM participant. It is deterministic code owning formal
invariants — workspace bounds, force and torque limits, speed clamps, e-stop
propagation. It runs in the fast loop. The slow loop cannot veto it.

Consequence: `Tool::required_safety_tier()` style trait declarations are only
useful if enforcement lives in the agent loop and in the fast-loop
supervisor. Declaration alone is not safety.

### Tier 2 — Big Additions Inside The Current Shape

**T2.1 Typed robotics bridge with backpressure.**
Bridges to ROS2 and dora-rs must be schema-validated, rate-limited, and carry
QoS semantics. The bridge drops or samples on overflow rather than buffering
unbounded. MCP-over-stdio is retained for tool-shaped calls but is not the
sensor transport.

**T2.2 Mission contract.**
The current workspace contract asserts filesystem state at turn boundaries.
The mission contract extends it to temporal hardware state:

- pre-conditions — hardware ready, calibrated, workspace clear
- invariants — stay in envelope, under force limit, within speed clamp
- post-conditions — tool parked, power safe, black box flushed

Same contract grammar, same gate point, extended predicates. This is the
natural continuation of the harness branch.

**T2.3 Sensor-to-context policy.**
A robot generates more context per second than any model can consume. The
architecture includes a policy layer deciding what enters the prompt:
periodic summaries, event-triggered interrupts, attention gates such as
"include force and torque only during contact." Sensor injection is a budget
problem, not a "pass everything" problem.

### Tier 3 — Necessary But Mechanical

**T3.1 Black-box recorder.**
Monotonic timestamps, hash-chained tamper evidence, structured event log,
replay tooling. Written for ISO 10218, IEC 61508, and ISO 15066 postures. A
JSONL writer is not this.

**T3.2 Human-in-the-loop authorization.**
Typed authorization requests with time-to-live and default-deny on timeout,
transported over the existing channel bus. The gateway already has the
transports; the missing piece is the typed primitive with safe-state
fallback.

**T3.3 Simulation parity.**
The same agent code drives MuJoCo, Gazebo, or Isaac that drives hardware,
with deterministic seeding and time acceleration. Sim-first is the only way
robot code gets into CI at all.

**T3.4 Multi-robot cell orchestration.**
Resources, roles, priorities, deadlock avoidance. Deferred until a one-robot
path is complete end to end.

## What Is Not Architecture

These are proposals frequently pitched as architecture. They are not. They
are type declarations, manifest fields, or examples.

- A trait method declaring a required safety tier. Declaration is trivial;
  enforcement is the architecture. Ship declaration only after the enforcer
  lands.
- A manifest field declaring a hardware lifecycle. The architecture is the
  sandboxed executor and the gate that validates before running. Without
  the executor and gate, the field is an unsandboxed shell escape.
- New hook event variants. Hooks are a transport. A new event without a
  runtime firing site is dead code.
- A Python port of the Rust runtime. That is a distribution choice, not an
  architectural improvement. If it drifts, it becomes a safety-critical
  duplicate.
- A root-level architecture document describing features that do not exist.
  Design proposals live under `docs/design/` and are marked aspirational
  until wired.
- A new crate whose entry point is a placeholder. Until the crate forwards
  real work to a real runtime, it misleads tool registration.

Each of these has a legitimate place in its own phase. None of them belong
in the first wave of robotics architecture work.

## Required Invariants

The robotics program must preserve all of the following. Any change that
breaks one of these is outside scope and is rejected.

1. Existing sessions, topics, and workspaces remain valid.
2. Existing tools remain callable through the same registry.
3. Existing workspace contracts keep their current semantics — mission
   contract is a superset, not a replacement.
4. Existing profile config sections remain valid — robot config is a new
   typed section, not a restructure of existing ones.
5. The agent loop continues to be the only place that dispatches LLM tool
   calls. No second loop emerges.
6. Safety enforcement lives in runtime code, never in a prompt.
7. The LLM cannot override a fast-loop safety veto.
8. No robotics feature introduces a new execution path that bypasses the
   sandbox, the tool policy, or `BLOCKED_ENV_VARS`.
9. No robotics feature weakens `deny(unsafe_code)` at the workspace level.
10. No robotics doc describes behavior the code does not yet exhibit.

## Explicit Non-Goals

- Replacing the existing agent loop with a state machine. The loop stays;
  the fast loop is added as a peer.
- Inventing a new robotics DSL before the typed config sections and
  mission-contract grammar are in place.
- Landing safety-tier trait declarations without enforcement.
- Landing new `HookEvent` variants without firing sites in the loop.
- Vendoring a Python reimplementation of any octos runtime component into
  the monorepo.
- Adding a stub bridge crate to the workspace. Bridges land only when they
  forward real traffic.
- Shipping a root-level `architect.md`. Design proposals belong under
  `docs/design/`.

## Relationship To The Harness Layer

Robotics sits on top of the harness layer. It does not replace it.

- Mission contract extends workspace contract.
- Robot config is a new section on `ProfileConfig`, shaped like `llm`,
  `search`, `deep_crawl`, and `apps.slides`.
- Safety tier composes with the existing `ToolPolicy`, not alongside it.
- Hardware lifecycle composes with the existing sandbox and `SafePolicy`,
  not alongside them.
- Mission execution reuses the existing `octos-pipeline` engine.
- Sensor injection reuses the existing context engineering path.

The harness branch is the prerequisite. The robotics family does not start
until `release/2026-04-17-harness-gate-local` lands.

## Success Criteria

The robotics architecture is considered complete when all of the following
hold on the public canary:

- a one-robot sim runs end-to-end through the pipeline, driven by the agent
  loop, with the safety supervisor enforcing a documented invariant set
- a motion-blocking LLM call cannot stall the fast loop
- a safety violation transitions the fast loop to Safe state without slow
  loop cooperation
- a mission contract failure blocks terminal success exactly the way a
  workspace contract failure does today
- the black-box recorder produces a replay that reproduces the incident
- operator surface includes robot-specific counters alongside the existing
  runtime summary

The program is judged a failure if any of these instead appear:

- a second session, workspace, or tool execution model
- a safety path that is implemented in prompt text
- a sensor stream buffered unbounded in Rust memory
- a robotics-only branch inside core runtime code
- a new crate whose `execute` returns a placeholder

## Persistence

This note is the canonical architecture record for the robotics program.

The program plan — phases, roles, release contracts, PM driver rubric — is
in [OCTOS_ROBOTICS_FAMILY.md](./OCTOS_ROBOTICS_FAMILY.md).

The per-issue contracts — R01 through R10 — are in
[OCTOS_ROBOTICS_CONTRACTS.md](./OCTOS_ROBOTICS_CONTRACTS.md).
