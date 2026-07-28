# ADR — Fleet runtime: one kernel, two worker-kinds, three topologies

Status: Proposed
Date: 2026-07-27
Author: ymote
Related: `docs/FLEET-KERNEL-FOUNDATION-SPEC.md` (the grounded design this decision drives); `peer_respond` (#1843), awaiting-input wake (#1844); supersedes the informal "peers-as-substrate" sketch.

## Context

The **goal** feature keeps a durable objective but pursues it inside *one agent's
conversation*. Compaction is lossy, so the goal's **execution state** — the plan,
decisions, what's done vs. what remains — erodes as tasks prolong. The objective
survives (a 300-char, `active`-only system-prompt pin via `#1697`); the *progress*
rots. That is the concrete "the goal vanishes as tasks prolong" failure.

Separately, octos already has **three** would-be orchestration stacks that do not
know about each other:

- **peers** — durable, interactive agent *sessions* (`SessionKey`,
  `run_standalone_turn`, blackboard, `MasterContinuationScheduler`);
- **`octos-swarm`** — idempotent batch *dispatch* to external one-shot agents
  (`DispatchRecord` in redb, parallel/sequential/pipeline/fanout topologies);
- **`octos-pipeline`** — a DOT-graph *DAG* workflow engine (checkpoint/resume,
  per-node model selection, one bounded runtime fan-out).

Rebasing goal onto peers naïvely would add a **fourth** incompatible ledger /
budget / retry / lifecycle model. This ADR answers: **is there one runtime
underneath these, and where is its boundary?**

Two adversarial design reviews and two code-grounded capability maps (on
`origin/main`) inform it.

## Decision

Build **one durable fleet kernel** — the child-state ledger, budget reservation,
status/retry state machine, policy-gate, and validators — shared by all fleet
orchestration. Keep **two distinct worker-kinds** on it:

- a **stateless task-worker** (onto which `octos-swarm` and `octos-pipeline`
  converge), and
- a **durable, interactive session-worker** (peers — parkable, scheduler-woken).

Express **topologies as planners** over the kernel: batch (swarm), DAG (pipeline),
and dynamic fleet (goal/peers). **Do not** force a parking session and a one-shot
batch call into a single "worker" abstraction — that seam is the fault line.
**Goal** becomes the first app: a keeper that decomposes an objective onto a
durable session fleet and reads progress from the kernel ledger, not its own
context.

This is option **C** below.

## Evidence — what converges, where the seam tears

Grounded in the code on `origin/main`. The kernel is **already ~80% shared**; the
**worker model** is the one thing that genuinely differs.

| Layer | octos-swarm | octos-pipeline | peers (+ goal) | Verdict |
|---|---|---|---|---|
| **Worker model** | stateless external one-shot (`claude -p`/MCP, `external_unmanaged`) | stateless in-process one-shot (`Agent.run_task`) | **durable, interactive** session — parks mid-turn | **fault line** |
| **Human-in-the-loop** | none in-band | defined but **dormant/unwired** (`ir.rs:94`) | **first-class** park + scheduler wake | **fault line** |
| **Durable ledger** | `DispatchRecord` (redb): per-child status, attempts, idempotency fingerprint, resumable | `Checkpoint` (JSON): per-node outcome + resume | blackboard + `SupervisorStore` | **converges** (same idea ×3) |
| **Budget** | reserve/commit on shared `CostAccountant` | reserve/commit on the **same** `CostAccountant` | per-goal token budget | **converges** (USD already shared) |
| **Gate + validators** | shared `DispatchPolicy` + M4.3 validators | (shares `CostAccountant`) | — | **converges** (already literally shared) |
| **Status / retry** | 3-state + retry rounds | 4-state + per-node retries | running/done/closed/awaiting_input | **converges** |
| **Decomposition** | static batch | mostly static DAG (one bounded fan-out) | **dynamic** spawn/close mid-flight | differs |

Neither swarm nor pipeline can carry an interactive worker today: swarm never
parks; pipeline's HITL is defined but not wired. So a durable-interactive
session-worker is a **new** worker-kind that maps onto peers, not a re-skin of the
other two.

## Architecture

```
Topologies (planners / apps)   Batch (swarm) · DAG (pipeline) · Dynamic fleet (goal/peers)
            │  emit work onto
Worker-kinds (kept distinct)   Stateless task-worker  |  Durable interactive session-worker
            │  run & record on                          (swarm+pipeline)   (peers · needs headless runner)
Kernel (unify — mostly shared) Durable child-state ledger · Budget · Status/retry · Policy-gate · Validators
```

The kernel's ledger is **lifted from swarm's `DispatchRecord`** — a keyed,
per-child, idempotent, resumable record that is precisely what a fleet needs and
what peers lack today. The session-worker adds what swarm's model never had: a
live, parkable turn that can await input and be woken by the continuation
scheduler.

## Options considered

- **A — one runtime, one worker (rejected).** Every worker (batch, node, session)
  is one pluggable kind. Forced fit: a durable, parkable, scheduler-woken session
  is categorically not a one-shot dispatch; collapsing them spreads
  interactive-worker complexity everywhere.
- **B — fleet beside swarm + pipeline (rejected).** A new peer-native runtime with
  its own ledger/budget/lifecycle. Fastest to a goal win, but it is exactly the
  **fourth incompatible silo**, and it rebuilds the durable ledger swarm already
  has.
- **C — shared kernel, distinct workers (chosen).** Unify only what genuinely
  should be one — ledger, budget, status/retry, gate, validators (much already
  is). Keep the two worker models distinct. Kills the silo risk at the layer where
  it hurts, without pretending a parking session and a batch call are the same
  thing.

## Consequences

**Unlocks**

- Goal **reuses swarm's ledger** instead of inventing a fourth — the durable,
  idempotent, resumable per-child record is the exact thing prior review flagged
  as missing from peers.
- Goal's progress **leaves the compactable context** and lives in the kernel
  ledger — the "goal drifts" fix, made structural.
- Goal **doesn't wait on the swarm/pipeline convergence.** Goal drives the
  kernel's creation; swarm and pipeline adopt it afterward.
- swarm + pipeline **converge cheaply** — they already share budget, gate, and
  validators; only the ledger needs lifting.

**Costs & risks**

- **The headless session runner is a new component.** Peers today only *stage*; a
  WS client opens them. A goal has no client — so a server-side launcher/recovery
  for session-workers is the real first build, not a factoring.
- **A durable plan is required.** The goal record has no task graph; without one,
  every keeper cycle re-derives "what's left" from lossy context. And `result.md`
  means "a turn ended," not "succeeded" — the plan must track real acceptance, not
  file presence.
- **#1842-grade concurrency.** The fleet state machine (launch leases, generation
  membership, idempotent event outbox, recovery reconciliation) is the hard part —
  treat it with the rigor the close-while-parked race taught us.
- **The worker-kind boundary must be enforced.** The owner must be an opaque,
  server-checked capability, or an LLM could steer another fleet's workers;
  depth-1 still applies unless an escalation model is added.
- **Out of scope:** non-decomposable deep goals (they want durable single-agent
  working-memory, not a fleet) and structured-plan surfacing.

## Roadmap

Refined by `docs/FLEET-KERNEL-FOUNDATION-SPEC.md`, which — after two adversarial
design passes — establishes that the kernel is a **durable-execution /
restartable-workflow runtime** (recovery *interrupts and restarts* attempts; it
does not resume a live turn), and splits the goal win from the hard part:

1. **Kernel v1.** One transactional store + the attempt-based state machine
   (launch leases, generation fencing, idempotent outbox) + the durable plan
   schema. Design-first, #1842-grade rigor, before code.
2. **Goal on *stateless* workers — the goal win.** A goal keeper + durable plan
   dispatching bounded sub-tasks to the already-durable stateless worker. Progress
   lives in the plan + ledger, not context — the goal-drift fix, using workers
   that already recover correctly. **Ships without the interactive-session-worker
   durability problem.**
3. **Durable interactive session-worker.** Durable prompts + attempt-based
   park/answer + the headless launcher — for sub-tasks that are themselves long
   interactive work.
4. **Converge swarm + pipeline** onto the kernel; keep their topologies as
   planners. The "3 → fewer silos" payoff — a phased follow-on, **not** a
   prerequisite for the goal win.

## Open questions

- The exact kernel ledger schema — how much of `DispatchRecord` lifts cleanly vs.
  needs generalizing for a session-worker.
- The `FleetRecord` / owner-capability model —
  `fleet_id → controller_session_key + profile + budget`, resolved server-side.
- The durable plan schema — task IDs, dependencies, acceptance criteria, decision
  log, evidence, revision/fencing.
- Budget unification — real-token metering across USD-reserving swarm/pipeline and
  the token-budgeted goal.

## References

- Shipped foundations: `peer_respond` (#1843), awaiting-input wake (#1844).
- Related systems: `octos-swarm` (`DispatchRecord`, topologies), `octos-pipeline`
  (DAG, checkpoints), goal machinery (`AutonomyGoalRecord`,
  `MasterContinuationScheduler`).
- Prior ADRs in this repo: `docs/M9-LEDGER-DURABILITY-ADR.md`,
  `docs/M11-PROFILE-SESSION-RUNTIME-ADR.md`.
