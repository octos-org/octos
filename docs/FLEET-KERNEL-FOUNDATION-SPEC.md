# Fleet Kernel — Foundation Spec (v2)

Status: Draft
Date: 2026-07-27
Author: ymote
Refines: `docs/FLEET-RUNTIME-ADR.md` (the architecture decision this spec grounds).

## Why v2

The ADR decided the architecture (one kernel; a stateless task-worker and a
durable interactive session-worker; goal as first client). Two adversarial design
passes then established that the **kernel is a durable-execution / restartable-
workflow runtime**, and that a first draft ("merge swarm's `DispatchRecord` with
`SupervisorStore`, persist the in-memory maps, persist the parked prompt") is
**not buildable as stated**. This v2 folds those corrections in and re-sequences
the work so the goal win does not wait on the hard part.

## The core principle

> **You cannot persist and "resume" a live agentic turn.** After a restart the
> parked-question oneshot and the awaiting future are gone. Recovery is not
> *snapshot-and-resume*; it is **interrupt the old attempt and start a fresh,
> checkpointed one** — which octos already does correctly today (restored children
> come back `interrupted`, not resumed; see `agent_orchestrator.rs` restore path).

Every structural correction below follows from this.

## Five invariants (the buildable shape)

1. **One physical transactional store.** Fleet/child state, attempts, budget
   reservations, prompt state, and the delivery outbox all mutate in a *single*
   redb write-transaction. Do **not** split state across two stores (redb +
   JSONL): a crash between the two desyncs them with no shared transaction. Swarm's
   `DispatchRecord` (fingerprint + finalize-replay + `io_gate` cancellation
   ordering) and `SupervisorStore` (event outbox + snapshot/replay + idempotent
   `event_id`) are **design references, not stores to compose.**
2. **Attempt-based lifecycle + interrupt-and-restart recovery.** A child runs as a
   sequence of **attempts**, each with a lease and an incarnation id. Recovery
   interrupts the prior attempt (marks it `Interrupted`) and schedules a fresh
   attempt from durable checkpoint state (the plan + ledger), never resurrecting a
   live turn's stack. Every side-effecting command, answer, outbox claim, and
   completion carries `(child_id, attempt_id)` so a replayed/duplicated action is
   idempotent.
3. **Durable prompts, not persisted oneshots** (session-worker only, Phase 2b). An
   `AwaitingInput` state persists a `Prompt { attempt_id, status, answer }`, not a
   Tokio `Sender`. Answering **atomically** consumes the current prompt and
   schedules the next checkpointed attempt. Recovery declares the old attempt
   interrupted and re-derives the wait from the durable prompt — it does not
   reconnect a dead future.
4. **Server-owned controller principal.** A fleet's controller authority is a
   *server-side* identity resolved from the durable `FleetRecord`
   (`fleet_id → controller_session_key`), not a bearer secret an LLM holds (a
   secret is either in replayable context, lost on restart, or absent for an
   autonomous continuation). Autonomous keeper turns are server-authorized by the
   continuation's provenance. Cross-user control stays blocked by profile scoping;
   the residual "same user opens the controller session" case is the user's own
   authority, by design (the stance `peer_respond` already documents).
5. **Budget enforced at the model-request boundary (or labelled soft).** A
   reservation from estimates is admission control, not a ceiling: two children
   estimated at 45 can both reserve against 100 and each spend 90. A *hard* cap
   requires enforceable maxima (provider output limits + bounded input/tool-loop)
   plus durable reservations written in the **same transaction** as launch and
   idempotent usage-commit keyed by `(child_id, attempt_id)`. Absent enforceable
   maxima, the fleet/goal budget is **soft** (stops the next dispatch, not the
   current turn) — and must be named as such.

## The kernel store

One redb-backed transactional store. Every mutation is one write-txn that
CAS-checks status/generation/revision and updates state + reservation + outbox
together.

- **`FleetRecord`** — `fleet_id`, `controller_session_key`, `profile_id`,
  `budget { token_budget, tokens_reserved, tokens_committed, usd_cap? }`,
  `status`, `generation`, `child_ids`.
- **`FleetChildRecord`** — keyed `(fleet_id, child_id)`: `worker_kind`,
  `current_attempt_id`, `status`, `attempts_used`, `idempotency_fingerprint`,
  `tokens_reserved/committed`, `outcome: AcceptanceVerdict?`, `generation`,
  and (session-worker) `workspace_path`, `brief_path`, `worktree_branch`.
- **`Attempt`** — the durable-execution unit: `attempt_id`, `child_id`, `status`
  (`Leased → Running → {AwaitingInput} → Done{outcome} | Interrupted`),
  `lease { owner_epoch, expires_at_ms }`, timestamps.
- **`Prompt`** (Phase 2b) — `{ prompt_id, attempt_id, kind, prompt, status,
  answer }`; atomic consume-and-reschedule.
- **`Reservation`** — token/USD reservation, written in the launch txn.
- **`OutboxEvent`** — `{ event_id, sequence, claimed_by?, acked }`: a *real*
  claim/ack outbox (unlike `SupervisorStore`'s replay-only `event_id` dedup),
  so a `FleetEvent → controller continuation` enqueue is exactly-once.
- **`DurablePlan`** — see below.

Fix the two `SupervisorStore` weaknesses when lifting its shape: batched `fsync`
(today appends aren't synced) and an in-memory `last_sequence` (today every append
does a full `load_state()`, O(n)).

## State machine

Child: `Planned → Launching(lease) → Running(attempt) → [AwaitingInput(prompt)]* →
Succeeded(verdict) | Failed | Cancelled`.

- **Launch lease.** `Planned → Launching` is a durable CAS in one write-txn
  (read status → predicate → write status + lease + reservation + outbox). Only
  the lease holder drives the launch. The store is **single-active-process** (redb
  takes an exclusive file lock); multi-writer needs a separate leader/fencing
  service — out of scope for v1.
- **Launch is not atomic with the external effect.** A lease-commit cannot include
  spawning the turn. Handle the three windows explicitly: crash after lease before
  launch → reconciliation reclaims the stranded lease; crash after launch before
  ack → the attempt is `Leased` with no completion → interrupted + retried, and
  the worker's effects are idempotent by `attempt_id`; cancellation after launch →
  the stale attempt's later events are fenced by generation + attempt id.
- **Generation fences *acceptance*, attempt-id fences *effects*.** Honor a child
  event only if `child.generation == fleet.generation` **and** it carries the
  current `attempt_id`. `FleetDrained` fires when the **sealed** current generation
  has every member terminal *with an `AcceptanceVerdict`* — a new spawn bumps the
  generation and re-opens drain (unlike today's supervisor groups, which reopen an
  auto-terminal group when a child is added).
- **Recovery reconciliation** (boot): `load_state` → expired `Launching` lease →
  back to `Planned`; `Running` attempt with no live task → `Interrupted` + schedule
  a new attempt; `AwaitingInput` → re-derive the wait from the durable `Prompt`.

## Worker-kinds

- **Stateless task-worker** — a bounded, non-interactive one-shot (swarm's MCP/CLI
  dispatch, or pipeline's in-process `Agent.run_task`). It **already** has a
  durable ledger and correct interrupt-and-restart recovery. It cannot park for
  input. **This is the Phase 2a worker.**
- **Durable interactive session-worker** — a long-lived peer session that can park
  on `ask_user_question`/approval and be woken. It needs the *full* protocol:
  durable prompts (invariant 3), attempt-based park/answer, and the headless
  launcher below. **This is Phase 2b.**

### Headless launcher (Phase 2b, the crux)

The headless turn runner already exists (`spawn_global_master_continuation_drain`
drives `run_standalone_turn` with a sink connection). The gap is three localized
pieces — but they need a **durable, server-authorized launch specification**, not
an imitation of `session/open`'s tail (which sits downstream of session-scoping,
profile/workspace validation, permissions epoch, sandbox selection, feature
gates): (1) a connection-free `establish_fleet_session` from that launch spec;
(2) a first-turn trigger (none exists — "model stages, client opens"); (3) a boot
rehydration pass repopulating `session_workspaces` / `peer_wire_registry` from the
durable ledger. The sink transport is **not** safe as a generic worker transport:
approval/question emits would block with no controller, and `run_standalone_turn`
treats discarded delivery as success — so a session-worker's interactive emits
must route through the durable prompt protocol and the controller, not the sink.

## Durable plan

Beside `FleetRecord`, keyed `fleet_id`, revision-fenced. The goal record has
*none* of this today (only a free-text `objective` + accounting).

- `PlanTask { task_id, title, detail, deps: [task_id], state, acceptance:
  [AcceptanceCriterion], evidence: [EvidenceRef], assigned_child? }`.
- `state`: `Pending | Ready | Assigned | Running | Blocked | Accepted | Rejected |
  Cancelled`.
- `AcceptanceCriterion { id, description, verifier: Manual | FileExists |
  CommandExit | ValidatorRef }` — **data + a verifier**, so "done" is checkable,
  not model-asserted (today `result.md` means "a turn ended," not "succeeded," and
  `complete` is model-asserted, never verified).
- `DecisionLog` — append-only `{ seq, at_ms, actor_child, kind, note }`, the
  structural fix for "progress rots as context compacts."
- **Revision fencing:** every plan mutation is a write-txn CAS on `revision`, so
  concurrent keeper turns cannot clobber the plan.

The keeper reads the plan from the store, **not its context** — `goal_get` returns
the task graph + per-task state + the remaining set.

## Sequencing (revised)

1. **Kernel v1 — the transactional store + attempt lifecycle + durable plan.**
   The store, the state machine (leases/generation/attempts/outbox), and the plan
   schema. No session-worker durability yet.
2. **Phase 2a — goal on *stateless* workers (the goal win).** A goal keeper +
   durable plan dispatching bounded sub-tasks to the already-durable stateless
   worker. Progress lives in the plan + ledger, not context — the goal-drift fix,
   using workers that already recover correctly. **Ships without the hard part.**
3. **Phase 2b — the durable interactive session-worker.** Durable prompts, the
   attempt-based park/answer protocol, and the headless launcher. For sub-tasks
   that are themselves long interactive work.
4. **Converge swarm + pipeline** onto the kernel — phased follow-on, not a
   prerequisite.

The tradeoff to name: Phase 2a sub-tasks are **bounded and non-interactive** (they
cannot ask a mid-task question). That fits decomposable goals whose tasks are
"do X, report evidence." Goals whose sub-tasks must themselves park for input
require Phase 2b.

## Hazards addressed (v2)

| Hazard | Mitigation |
|---|---|
| Split-store desync (state vs event) | one transactional store; every mutation one write-txn |
| Double-launch across restart | durable launch lease (CAS), reclaimed on expiry |
| Launch not atomic with effect | attempt-id idempotency on all effects; explicit crash-window reconciliation |
| Reparent / stale-worker | generation fences acceptance; attempt-id fences effects |
| "Resume" a dead turn | interrupt + fresh checkpointed attempt (never resurrect) |
| Parked prompt lost on restart | durable `Prompt`, atomic consume-and-reschedule (2b) |
| Budget overspend | reserve in launch txn + commit actual by attempt; hard only with enforceable maxima, else soft |
| Controller spoofing | server-owned principal + profile scoping (not a RAM secret) |
| Cancellation-orphaned write | lift swarm's `io_gate` owned-guard-into-`spawn_blocking` |
| fsync gap / O(n) append | batched fsync + in-memory `last_sequence` |

## Still hard / open

- Making a session-worker's interactive emits (approval/question) route through the
  durable prompt protocol + controller instead of the sink, without reintroducing
  the close-while-parked race (`docs`/#1842) — the Phase-2b core.
- Single-active-process is assumed; a multi-writer daemon needs a leader/fencing
  service.
- Hard token budgets depend on provider-side output caps + a bounded tool-loop
  policy; otherwise the budget is soft.

## References

- `docs/FLEET-RUNTIME-ADR.md` (the decision this refines).
- Design references: `octos-swarm` (`DispatchRecord`, `io_gate`, reserve/commit,
  finalize-replay), `SupervisorStore` (event outbox, snapshot/replay), the goal
  machinery + `MasterContinuationScheduler`, `CostAccountant`.
- Shipped foundations: `peer_respond` (#1843), awaiting-input wake (#1844).
