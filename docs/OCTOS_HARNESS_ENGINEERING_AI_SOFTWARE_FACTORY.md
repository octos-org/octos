# Harness Engineering: evolution from a 灵光一闪的天才 to a stable-output AI software factory

Date: 2026-04-22  
Audience: Octos maintainers, app-skill developers, platform operators

---

## Preface: Why this book exists

Octos already proved a hard truth: a powerful model plus prompt tricks can look brilliant for demos, then fail under real traffic, long-running jobs, reloads, and multi-session concurrency.

This book captures the harness engineering lessons that turned that pattern into a factory posture:

- deterministic task lifecycle
- durable contract state
- typed progress/event ABI
- validator-gated completion
- replayable UI truth
- operator-visible failure causes

The goal is not architecture purity. The goal is reliable outcomes when the model, network, plugins, browser tabs, and user behavior are all messy.

---

## 1. LLM operating region and why control systems exist

The central engineering goal is not “make the model smarter in isolation.”  
The practical goal is to keep the LLM inside its best context and operating region for as much of the task horizon as possible.

Two analogies are useful:

- CPU cache/RAM locality: when working sets stay local and coherent, throughput is predictable; once locality is lost, performance collapses and variance rises.
- Semiconductor amplifier linear region: in the linear zone, output tracks input with acceptable distortion; outside it, clipping/saturation makes output uncontrolled.

LLMs show the same practical behavior. There is a usable deterministic/high-quality zone where:

- task constraints are explicit and recent
- tool interfaces are unambiguous
- feedback loops are short and verifiable
- state transitions are coherent across turns

Outside this zone, quality becomes unstable:

- hallucinated state and stale assumptions increase
- tool invocation quality drifts
- long-horizon coherence breaks
- confidence language diverges from actual correctness

### 1.1 Control-stack evolution in autonomous systems

Octos engineering evolved as layered outer controls that keep long-running work in that quality zone:

```text
Prompt engineering
  -> improves local single-turn behavior
Context engineering
  -> stabilizes multi-turn behavior (AGENTS.md, skills, memory, policy context)
Harness engineering
  -> stabilizes long-horizon autonomous behavior with durable runtime contracts
```

Prompt engineering alone cannot provide durable state truth.  
Context engineering improves this but still depends on probabilistic adherence.  
Harness engineering adds runtime-level guardrails and evidence, so the system can recover from model drift instead of pretending drift does not happen.

### 1.2 Context engineering as pre-harness discipline

Before harness maturity, context engineering carries most control weight:

- `AGENTS.md` and role instructions constrain planning style
- skills encode reusable, domain-specific procedures
- memory captures user/project facts to reduce repeated drift
- bounded tool policies reduce unsafe exploration

This layer is necessary. It is not sufficient for long-running, concurrent, background work.

### 1.3 Harness engineering as the outer deterministic loop

Harness adds deterministic control loops around probabilistic model output:

- recoverable, traceable workspace state
- tool-call lifecycle hooks and event sinks for observability
- verification/confirmation of outputs before terminal success
- durable task lifecycle and replay semantics
- explicit failure categories for repair and operator action

The harness does not replace model intelligence. It bounds it.

### 1.4 Key mechanisms that keep the system in-zone

1. Recoverable and traceable workspace contracts
2. Tool-call event hooks (`before_*`, `after_*`, spawn hooks) for visibility
3. Artifact and validator contracts that confirm output truth
4. Self-repair loops that update memory/skills from failures
5. Replayable status APIs so UI/operator surfaces stay aligned

This is how “good output sometimes” becomes “stable output under stress.”

### 1.5 Workflow vs free-form exploration tradeoff

Workflow-constrained execution gives stronger guarantees:

- better phase monotonicity
- better artifact predictability
- easier replay and operator diagnosis

Free-form exploration gives higher ceiling on novel problem-solving, but higher variance.

The practical strategy is hybrid:

- use workflow rails for long-running/high-cost/high-risk segments
- allow bounded free-form exploration inside explicit budgets and checks

### 1.6 Multi-agent orchestration and autonomy levels

As systems move from L2 toward L3/L4 autonomy, long-horizon stability matters more than single-turn brilliance.

- L2-style assistive behavior tolerates more manual recovery.
- L3/L4-style delegated execution requires self-monitoring, repair, and bounded autonomy.

Multi-agent orchestration helps by decomposing behavior:

- planner/coordinator agents manage phase intent
- specialist agents execute narrow tool domains
- verifier/operator agents enforce contracts and escalation

Harness is the substrate that makes this decomposition reliable over time.

---

## 2. The maturity gap: genius moments vs factory output

The “灵光一闪的天才” mode has these traits:

- success is prompt-dependent and non-repeatable
- status comes from chat text, not durable runtime state
- artifacts are guessed from file names or heuristics
- long jobs can silently fail but still look “done”
- session switching causes state bleed and UI confusion

A software factory mode has opposite traits:

- contract-defined outputs and validators
- lifecycle is persisted and monotonic (`queued -> running -> verifying -> ready|failed`)
- progress is evented, typed, replayable, and scoped
- UI is a projection of backend truth, not independent truth
- every incident has operator-grade evidence

Harness is the shift mechanism between those two modes.

---

## 3. Failure classes Octos hit (and why they were expensive)

### 3.1 Background task progress bugs

Observed class:

- child task running correctly
- parent session only shows initial status or stale status
- users assume hang or retry, creating duplicate work and confusion

Root causes:

- progress emitted as freeform stderr text
- no typed event sink contract
- no durable bridge from child progress into parent task state

Cost:

- false support escalations
- duplicate child sessions
- “it works locally” but not in live canary confidence collapse

### 3.2 `run_pipeline` / `deep-search` status drift

Observed class:

- deep research run is active, but status bubble and task API diverge
- API may say running while bubble appears frozen or mismatched phase

Root causes:

- multiple status channels with weak reconciliation
- no single canonical phase ladder enforced across UI/API/replay

Fix direction (M4.1A contract):

- `octos.harness.event.v1` progress schema
- runtime sink -> `TaskStatusChanged` -> `/api/sessions/:id/tasks` + SSE
- UI replay consumes same backend event truth

### 3.3 Session switching / status bubble contamination

Observed class:

- switch to sibling session and see progress/status from another session
- user cannot trust what belongs to current conversation

Root causes:

- topic/session scoping not enforced end-to-end
- replay and active stream mixed without strict session ownership checks

Fix direction:

- hard session/topic scoping in task status events
- replay filters + bleed checks in live gates
- explicit “no cross-session progress bleed” acceptance criteria

### 3.4 Artifact contract enforcement gaps

Observed class:

- background job completes but wrong file is delivered
- task marked done with missing/invalid primary artifact

Root causes:

- filename heuristics instead of declared artifact truth
- completion not blocked by validator failure

Fix direction:

- policy-owned `primary` artifact
- `BeforeSpawnVerify` blocking contract
- completion gating tied to validator outcomes

### 3.5 Validator runner incompleteness

Observed class:

- validators exist but outputs are too coarse for operators/devs
- insufficient typed evidence for debugging and replay

Required posture:

- typed per-validator outcome
- duration, reason category, replayable evidence path
- clear timeout and failure taxonomy

### 3.6 Operator blind spots

Observed class:

- counters exist but no compact operator narrative
- hard to answer “why this task failed” quickly

Fix direction:

- operator summary and dashboard fed by canonical task/harness state
- no dashboard-only hidden logic

---

## 4. Core harness architecture (current practical model)

```text
Child tool/skill (Rust/Python/Node/shell)
        |
        | emits octos.harness.event.v1 (progress/error/info)
        v
Event sink (OCTOS_EVENT_SINK transport: file/socket/stdio)
        |
        v
Runtime consumer + validation
        |
        +--> Task supervisor durable state (task_status, lifecycle_state, detail)
        |
        +--> TaskStatusChanged events
        |
        +--> Session event stream replay
        v
API (/sessions/:id/tasks + /events/stream)
        |
        v
UI status bubble / header / reload replay
```

Key rule: the UI is downstream, never authority.

### 4.1 Lifecycle model

Public state machine:

- `queued`
- `running`
- `verifying`
- `ready`
- `failed`

Internal fine-grained states may exist, but product surfaces must not depend on unstable internal labels.

### 4.2 Contract layers

```text
Layer A: Capability manifest  (what app can do)
Layer B: Workspace policy     (artifact + validator + spawn contract)
Layer C: Runtime result model (lifecycle + task state + delivery evidence)
```

### 4.3 Why typed events matter

Without typed events, progress is “chat-like hints.”  
With typed events, progress is a data product that can be validated, replayed, and audited.

---

## 5. Non-Rust bridge: mandatory for platform reality

Octos is Rust-first runtime, but skill ecosystem is multi-language.

If progress/event contracts work only in Rust:

- third-party integration speed collapses
- contract compliance becomes Rust-gatekept
- operator data quality becomes inconsistent

Therefore M4.1A’s non-Rust bridge is essential, not optional polish:

- Python emitter helper
- Node emitter helper
- CLI fallback emitter
- no-op semantics when sink absent
- runtime-side validation remains authoritative

Principle: language diversity at emit edge, strict normalization at consume edge.

---

## 6. UI replay is not a frontend feature; it is a reliability feature

A reliable status bubble must survive:

- browser reload
- reconnect
- session switch away and back
- partial stream interruption

If replay is missing or inconsistent, users see “randomness,” even when backend is correct.

Design rule:

- replay path and live path must share canonical event/task schema
- no second interpretation layer in UI
- gate tests must assert parity between task API and SSE replay

---

## 7. Operator dashboard and mini fleet live testing

### 7.1 Dashboard role

The dashboard is not cosmetic monitoring. It is the daily control plane for:

- lifecycle anomalies
- phase stagnation
- artifact/validator failures
- retries/timeouts/orphaned children
- cross-session bleed signals

### 7.2 Mini fleet as release truth

Unit/integration tests are necessary but insufficient.  
Harness regressions are often temporal and multi-surface, only visible in live canary behavior.

Live gate posture:

- run scripted validation on at least two mini-fleet hosts
- enforce explicit diagnostic kinds (not generic “failed”)
- block merge when invariants break

Canonical examples include:

- missing required phase
- non-monotonic phase sequence
- lifecycle regression
- duplicate research sessions
- cross-session progress bleed

---

## 8. Agent swarm orchestration: how harness scales teams, not just code

Harness is also a coordination system for multi-agent and multi-owner delivery.

### 8.1 Work decomposition pattern

```text
Program Manager
  -> defines milestone contract + acceptance invariants
Runtime/Harness Owner
  -> owns ABI + sink + supervisor durability
Skill Owner (e.g., deep-search)
  -> owns event emission + workflow mapping
UI/API Owner
  -> owns replay parity + scoped projection
Release/Operator Owner
  -> owns live gate + diagnostics + rollback policy
```

Without this split, teams merge partial truths that pass local tests but fail system truth.

### 8.2 Anti-chaos rule

No workstream may redefine contract semantics unilaterally after gates are active.  
Any change to schema, lifecycle semantics, or replay behavior must update:

- fixture
- script gate
- e2e spec
- docs

in one PR slice.

---

## 9. Principles and anti-patterns

### 9.1 Principles

1. Persist contract state outside prompt/chat context.
2. Prefer explicit artifact ownership over heuristic delivery.
3. Treat completion as a validator-gated terminal contract.
4. Use one canonical lifecycle ladder across API/UI/operator surfaces.
5. Enforce session/topic scoping as a hard safety boundary.
6. Make progress replayable and diagnosable, not just visible in real-time.
7. Keep transports replaceable; keep event schema stable and versioned.
8. Tie every release claim to live gate evidence, not anecdotal runs.

### 9.2 Anti-patterns to ban

1. Prompt-only contracts (“the agent should remember to send the file”).
2. UI-derived truth (“bubble says done, so task is done”).
3. Filename guessing for primary artifact selection.
4. Best-effort validators that do not block terminal success.
5. Freeform stderr parsing as status protocol.
6. Unscoped replay streams that can leak sibling session state.
7. Dashboard metrics disconnected from runtime canonical data.
8. “One more refactor” during release slices without user-visible invariant gains.

---

## 10. Workstream to milestone mapping (pragmatic roadmap)

This mapping aligns with current M4 direction and remaining platformization gaps.

### M4.1A — Parent-visible structured progress ABI

- typed `octos.harness.event.v1`
- sink + consumer + task_status bridge
- deep-search migration from stderr status
- UI/API replay parity
- mini fleet live gate
- non-Rust emitters

### M4.2 — Developer contract productization

- stable docs for manifest/policy/task fields
- starter app templates across artifact classes
- contract examples with failure modes

### M4.3 — Typed validator runner

- declarative validators with typed outcomes
- timeout taxonomy and failure categories
- replayable evidence and operator-facing provenance

### M4.4 — Compatibility gate for third-party apps/skills

- install from git
- run harness flow
- validate artifacts
- reload and verify persistence
- uninstall and verify no state bleed

### M4.5 — Operator dashboard maturation

- lifecycle + phase + validator + artifact surfaces
- retry/timeout/orphan diagnostics
- compact incident-ready views backed by canonical task summary

### M4.6 — Explicit ABI versioning and migration policy

- schema versions for events/hooks/policy/task fields
- compatibility tests and deprecation windows
- breaking-change protocol before external adoption

---

## 11. Harness is necessary but not sufficient

Harness solves orchestration correctness, not full product quality.

A high-quality coding agent still requires:

- strong model selection and routing strategy
- tool safety boundaries (sandbox, auth, privilege gates)
- robust prompt/policy alignment for coding workflows
- test generation/repair discipline
- repository-aware edit quality and review loops
- latency/cost governance

Think of harness as the runtime truth layer.  
Without it, quality cannot be trusted.  
With it alone, quality still depends on model + tool + product discipline.

---

## 12. Actionable checklists

### 12.1 Maintainer pre-merge checklist (harness-affecting PRs)

- [ ] Change maps to explicit user-visible invariant.
- [ ] Contract state is durable across crash/reload/compaction.
- [ ] Lifecycle transitions are monotonic and tested.
- [ ] Artifact selection uses declared policy, not heuristic fallback as primary.
- [ ] Validator failures block terminal success where required.
- [ ] Session/topic scope checks prevent cross-session contamination.
- [ ] API and SSE replay expose identical task truth.
- [ ] Operator summary/dash signals include new failure mode.
- [ ] Mini fleet gate passed on at least two hosts.
- [ ] Docs + fixture + scripts + e2e updated in same slice when contract changed.

### 12.2 Third-party app/skill developer checklist

- [ ] Declare a single canonical `primary` artifact.
- [ ] Define validators for existence, size, and domain constraints.
- [ ] Use stable lifecycle/task API fields only.
- [ ] Emit structured progress through `OCTOS_EVENT_SINK` when available.
- [ ] Keep stderr for diagnostics, not contract status.
- [ ] Ensure hooks are idempotent (`before_spawn_verify` especially).
- [ ] Test reload/session-switch behavior for your workflow.
- [ ] Verify failure paths produce operator-readable evidence.

### 12.3 Incident response checklist (status drift / contamination class)

- [ ] Compare `/api/sessions/:id/tasks` snapshot vs UI bubble/header.
- [ ] Inspect SSE stream for missing or out-of-scope `task_status` events.
- [ ] Confirm session/topic tags on replay and live events.
- [ ] Check duplicate deep-research/run_pipeline tasks in final snapshot.
- [ ] Validate phase order monotonicity and progress range.
- [ ] Capture diagnostic JSON and curl hint before patching.
- [ ] Re-run mini fleet gate after fix; do not accept local-only validation.

---

## 13. Reference architecture snippets

### 13.1 Task truth flow

```text
tool emits progress -> runtime validates event -> supervisor persists state
-> TaskStatusChanged -> API/SSE -> UI replay/status bubble
```

Any direct UI mutation outside this chain is a reliability risk.

### 13.2 Contract enforcement flow

```text
spawn success
  -> resolve candidate outputs
  -> before_spawn_verify (allow/modify/deny)
  -> validator runner
  -> mark ready or failed
  -> persist delivery evidence
```

Marking terminal success before this flow completes is a contract violation.

---

## 14. Closing: engineering stance for next phases

Treat harness work as product reliability engineering, not abstract framework building.

The winning pattern for Octos has been:

1. identify concrete user-facing failure classes
2. encode explicit contract invariants
3. bind runtime/UI/operator surfaces to one truth model
4. prove on mini fleet live gates before merge
5. only then expand abstraction and external platform surface

That is the path from “looks smart today” to “ships correct every day.”
