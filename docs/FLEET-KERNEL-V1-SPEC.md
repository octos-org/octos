# Fleet Kernel v1 — Implementation Spec

Status: Draft (v1.1 — incorporates an adversarial design review)
Date: 2026-07-27
Author: ymote
Refines: `docs/FLEET-KERNEL-FOUNDATION-SPEC.md` (the durable-workflow design); implements roadmap step 1 + enables step 2 (goal on stateless workers).

> **Review outcome (v1.1):** the store + one-write-txn CAS core is **sound and
> first-PR-ready**. The *execution layer* (Phase 2a) is **not** a thin lift — it
> needs the corrections in §§5–6 + §9 below. The single biggest risk: guaranteeing
> the stateless worker cannot park (see §9). PR 1 (the store) is unaffected.

## Scope

The concrete, implementable layer: **one transactional store + the attempt/lease/
generation state machine + the durable plan**, and the **goal keeper on the
stateless worker** (Phase 2a). It does **not** include the interactive
session-worker (Phase 2b). Every primitive below is grounded on `origin/main`; the
only genuinely new code is a thin kernel dispatch entry + one keeper tool.

## Verdict (from grounding)

Buildable from existing in-process primitives with **one new thin piece**:

- **LIFTED verbatim:** the redb store pattern (`octos-swarm/src/persistence.rs`),
  the `CostAccountant` reserve/commit (`octos-agent/src/cost_ledger.rs`), the
  `ValidatorRunner` acceptance gate (`octos-agent/src/validators.rs`), the
  `GoalContinue` keeper-turn machinery (`spawn_global_master_continuation_drain` →
  `run_standalone_turn`), and the `goal_tool.rs` "stateless tool over the
  `default_agent_orchestrator()` singleton" durable-state hook.
- **NEW (thin construction, no new engine):** the kernel store module
  (`fleet-kernel.redb` + records + CAS ops + recovery), the `goal_dispatch` keeper
  tool, and the pre/post-turn plan render + reconcile hooks.
- **Executor:** a bounded, non-interactive one-shot — `Agent::run_task`
  (`loop_runner.rs`) — whose durability + resume come from the kernel store, not
  the worker. **Do NOT nest `Swarm::dispatch`** (it carries its own redb
  `DispatchStore` → violates "one transactional store"); lift its shape only.

## 1. The store — `fleet-kernel.redb`

Lift `octos-swarm/src/persistence.rs` structure verbatim:

- `struct FleetKernelStore { db: Arc<Database>, path: Arc<PathBuf>, io_gate: Arc<tokio::sync::Mutex<()>> }`.
- `open(dir)`: `spawn_blocking { Database::create → begin_write → open_table (creates each) → commit }`.
- **`io_gate` cancellation-safety (verbatim):** every op — **mutations AND
  decision reads/scans** (v1.1: reads must gate too, or a cancelled blocking write
  can commit *after* an ungated read already made a launch decision) — does
  `let held = io_gate.lock_owned().await` and **moves the owned guard into the
  `spawn_blocking` closure** (`let _held = held;`), so a cancelled caller future
  (e.g. a dropped keeper turn) can't leave its non-abortable blocking write
  unordered against the next read. Copy each `AccessGuard` to an owned record
  before mutating the table.
- `SCHEMA_VERSION: u32 = 1`; every record carries `schema_version`; load drops any
  row with a higher version (returns `Ok(None)`).
- **Persist-before-work:** a child record is written **before** its first dispatch,
  so a crash leaves a reconcilable row (mirrors `dispatcher.rs` `store()` before
  the round loop).

Tables (all `TableDefinition<&str, &str>`, JSON values):

| Table | Key | Value |
|---|---|---|
| `fleets` | `fleet_id` | `FleetRecord` |
| `fleet_children` | `"{fleet_id}\0{child_id}"` | `FleetChildRecord` |
| `attempts` | `"{child_id}\0{attempt_id}"` | `Attempt` |
| `plans` | `fleet_id` | `DurablePlan` |
| `decision_log` | `"{fleet_id}\0{seq:020}"` | `DecisionEntry` (append by seq) |
| `outbox` | `"{sequence:020}"` | `OutboxEvent` (claim/ack) |

### One-write-transaction CAS (native, confirmed)

redb 2.x `WriteTransaction::open_table` returns a `Table: ReadableTable`, so
read-modify-write is one txn — proven in-repo by `PersistentCostLedger::record`
(`cost_ledger.rs:357-365`, reads the index list then writes the appended list under
the same `begin_write`). Every state transition is:

```
let wtx = db.begin_write()?;
{
  let mut t = wtx.open_table(TABLE)?;
  let cur: Rec = t.get(key)?.parse();          // read
  ensure!(predicate(cur));                       // CAS predicate (status/gen/lease/revision)
  t.insert(key, next_json)?;                     // write new state
  // + budget counter + outbox row in the SAME wtx (atomic with the transition)
}
wtx.commit()?;
```

The launch-lease, generation bump, budget reservation, and outbox append are all
committed together — no cross-store window.

## 2. Records

```rust
struct FleetRecord {
  schema_version: u32, fleet_id: String,
  controller_session_key: SessionKey,          // server-resolved; how FleetEvent finds the keeper
  profile_id: String,
  budget: FleetBudget { token_budget: u64, tokens_reserved: u64, tokens_committed: u64, hard: bool }, // hard=false in v1 (soft)
  status: FleetStatus,                          // Active | Draining | Complete | Failed | Cancelled
  generation: u64,                              // membership epoch (bumped on re-plan)
  created_at_ms: u64, updated_at_ms: u64,
}
struct FleetChildRecord {
  schema_version: u32, fleet_id: String, child_id: String,   // child_id == plan task_id in v1
  worker_kind: StatelessTask,                  // v1: only this; SessionWorker is 2b
  status: ChildStatus,                         // Planned | Ready | Launching | Running | Succeeded | Failed | Cancelled
  current_attempt_id: Option<String>,
  attempts_used: u32,
  outcome: Option<AcceptanceVerdict>,          // Accepted{evidence} | Rejected{reason} | Terminated{reason} — NOT "has result"
  tokens_committed: u64,
  generation: u64,
  updated_at_ms: u64,
}
struct Attempt {
  schema_version: u32, child_id: String, attempt_id: String, // fresh Uuid per attempt
  generation: u64,                             // v1.1 FIX: immutable, stamped at launch (Complete predicate needs it)
  status: AttemptStatus,                       // Leased | Running | Done | Interrupted
  lease: Lease { owner_epoch: u64, expires_at_ms: u64 },     // owner_epoch = this daemon boot's id
  result_snapshot: Option<ChildResultSnapshot>,// LIFTS DispatchRecord.final_result (verbatim replay, never recompute)
  started_at_ms: u64, ended_at_ms: Option<u64>,
}
struct OutboxEvent { schema_version: u32, sequence: u64, event_id: String, kind: FleetEventKind, claimed_by: Option<String>, acked: bool }
```

## 3. State machine

Child: `Planned → Ready → Launching(lease) → Running(attempt) → Succeeded | Failed`
(+ `Cancelled` from any non-terminal). `Ready` = all `deps` are `Succeeded`.

- **Launch** (`Ready → Launching`): one CAS write-txn — predicate `status==Ready &&
  no live lease && budget_ok`; write `status=Launching`, mint `attempt_id`, write
  the `Attempt{Leased, lease}`, reserve tokens (`tokens_reserved += est`),
  append `outbox(ChildLaunching)`. **Then** (outside the txn) invoke the executor;
  its side effects are idempotent by `attempt_id`.
- **Running** (`Launching → Running`): CAS on the attempt (`Leased → Running`).
- **Complete** (`Running → Succeeded/Failed`): one CAS whose predicate requires
  **all four** (v1.1 — attempt-id + generation alone is insufficient to fence a
  *same-generation* retry): `child.current_attempt_id == this attempt_id`,
  `attempt.status == Running`, `attempt.generation == fleet.generation`, and the
  lease token matches. Write `AcceptanceVerdict`, `result_snapshot`,
  `tokens_committed` (reconcile reserve→actual), append `outbox(ChildDone)`. A slow
  superseded attempt that finishes after relaunch fails this predicate and its
  result is dropped.
- **Generation fences acceptance; attempt-id fences effects.** A late event from a
  superseded attempt (predicate fails on `current_attempt_id`/`generation`) is
  dropped. A re-plan bumps `fleet.generation` and stamps new/kept children.
- **`FleetDrained`** fires when the **sealed** current generation has every member
  child terminal *with an `AcceptanceVerdict`* — a new spawn bumps the generation
  and re-opens drain (unlike today's supervisor groups).

### Recovery reconciliation (boot)

Each daemon boot has a fresh `owner_epoch`. Scan `fleet_children` for
non-`Cancelled` fleets:

- `Launching`/`Running` attempt whose `lease.owner_epoch != current` **or**
  `lease.expires_at_ms < now` → **one atomic CAS**: mark `Attempt=Interrupted`,
  **clear `child.current_attempt_id`**, child back to `Ready` (a fresh attempt will
  relaunch). **Never resurrect the old attempt.** v1 relies on **boot-only**
  reconciliation — a true process crash kills the old Rust future, so no double-run.
  (Caveat: if *active* lease expiry is ever added, or a task spawns an external
  child process that outlives the daemon, the CAS can drop the stale *result* but
  cannot fence its *filesystem/external side effects* — see §9 task-kinds.)
- `Succeeded/Failed` with a `result_snapshot` → leave (finalize-replay).
- Re-derive due keeper continuations from child status, not from a persisted queue
  (a lost enqueue self-heals; enqueue is idempotent by dedupe key).

## 4. The durable plan

```rust
struct DurablePlan { schema_version, fleet_id, revision: u64, objective: String, tasks: Vec<PlanTask> }
struct PlanTask {                               // the SPEC only — live state derives from the child
  task_id: String, title: String, detail: String,
  deps: Vec<String>,                            // task_ids that must be Succeeded first (child.deps is the store's re-synced copy)
  acceptance: Vec<AcceptanceCriterion>,
}
// PR-2 reconciliation: PlanTask.state and PlanTask.evidence were removed — one source of truth per fact.
// Live state = FleetChildRecord.status (+ current_attempt_id); outcome + evidence live with
// FleetChildRecord.outcome (an AcceptanceVerdict). Because child_id == task_id there is no assigned_child.
// The ergonomic Fleet::view() (PR 2) JOINs the spec back with the child's state for rendering.
struct AcceptanceCriterion { id: String, description: String,
  verifier: Manual | FileExists{path} | CommandExit{cmd, code} | ValidatorRef{id} }  // data + a verifier — "done" is checkable
struct EvidenceRef { kind, locator: String, sha256: String, captured_at_ms: u64 }
struct DecisionEntry { seq: u64, at_ms: u64, actor: String, kind, note: String }     // append-only
```

Every plan mutation is a write-txn CAS on `revision` (read `revision N` → write
`N+1` only if unchanged), so concurrent keeper turns can't clobber it.

## 5. The keeper loop (Phase 2a) — event-driven, parallel

The keeper **launches** ready tasks and is **woken on completions**; it does not
run sub-tasks synchronously (that would serialize the fleet and defeat the point).
It reuses the **already-shipped** wake machinery: `FleetEvent::ChildCompleted`
enqueues a keeper continuation on the fleet's controller exactly as
`peer_awaiting_input` / `peer_fleet_synthesis` wake the master today — generalized
from "the peer's originator" to "the fleet's controller" (foundation-spec Draft 2).

A keeper turn (`GoalContinue`), three hooks:

1. **Pre-turn (host, no LLM):** `master_continuation_prompt` renders the durable
   plan — the task graph, each task's current state/verdict (already updated by the
   background dispatches), and the newly-`Ready` set — where it renders `objective`
   today.
2. **In-turn (LLM via tools):** the keeper calls `goal_get` (the task graph) and,
   for each `Ready` task, **`goal_dispatch(task_id)`** — which **launches and
   returns immediately** — plus `goal_update` for plan mutations. The turn ends
   after launching; it does **not** block on sub-tasks. The keeper reasons over
   durable state, never its own accumulated context.
3. **Woken on `ChildCompleted`:** each finished dispatch emits the event → a keeper
   continuation fires → the next turn's pre-turn render shows the new verdicts and
   next-`Ready` tasks → the keeper launches those. Completion = all tasks
   `Accepted` → `FleetDrained` → synthesis turn.

### The `goal_dispatch` tool (the one new piece) — launch-and-return

Shape mirrors `GoalUpdateTool` (`goal_tool.rs`): stateless, resolves session from
`ctx.parent_session_key`, reaches durable state via `default_agent_orchestrator()`
(which owns the `FleetKernelStore`). It **launches** an attempt and returns; the run
happens in a background task that records its own outcome and wakes the keeper —
so N ready tasks run in parallel:

```
goal_dispatch(task_id):                                     // SYNCHRONOUS part (fast)
  1. CAS-launch the child (§3 Launch) in one write-txn → attempt_id   // durable BEFORE work; refuses a double-launch
  2. CostAccountant::reserve(fleet_id, projected_usd)? → handle       // cost_ledger.rs:687 (admission; soft)
  3. tokio::spawn(run_attempt(fleet_id, task_id, attempt_id, handle)) // background; parallel across tasks
  4. return { dispatched: task_id, attempt_id }                       // keeper turn continues / ends

run_attempt(...):                                           // BACKGROUND task
  a. build a bounded, NON-INTERACTIVE Task from the plan task (brief + acceptance)
  b. Agent::run_task(&task) -> TaskResult                             // loop_runner.rs:2058, in-process one-shot
  c. run acceptance: ValidatorRunner::run_all(...) → required_gate_passed()  // validators.rs:513/267
  d. CAS-Complete (§3): write result_snapshot + AcceptanceVerdict + tokens; handle.commit(actual)
  e. emit FleetEvent::ChildCompleted → enqueue keeper continuation   // the shipped wake path, generalized
```

Idempotency + recovery: step 1's CAS refuses a second launch of an
already-`Launching`/`Running` task; a crash before (d) leaves a `Leased` attempt
whose lease expires → recovery marks it `Interrupted`, child → `Ready`, a fresh
attempt relaunches (never resurrecting the old run). **Side-effect caveat:**
`Agent::run_task` does real work (file writes, commands), so a crash-relaunch is
*at-least-once* per task — bounded sub-tasks should be written idempotently, and
the acceptance verifier is the guard that a partial/duplicated run is not marked
`Accepted`.

## 6. Budget (soft in v1, durable in the fleet txn)

v1.1 correction: do **not** split the budget across the fleet store and the
in-memory `CostAccountant`. **Reserve and settle the fleet budget *inside* the
same fleet write-transaction as the launch/complete CAS** — `FleetBudget` is the
single durable representation (pick one unit: a token cap, or USD-with-model-
pricing). The launch CAS predicate includes `tokens_reserved + tokens_committed +
projected ≤ token_budget`; on failure the child is **not** left `Launching` (the
prior draft reserved *after* launch, creating a `Launching` child with no worker —
fixed; this restores the "no cross-store window" claim). `Complete` settles
reserve→actual in the same txn.

Treat `CostAccountant` as **attribution/telemetry only** — its reservations are
in-memory (lost on restart), USD-projected, always-allow without a policy, and
compare with strict `>` where v1 wants `≥`. The v1 budget is **soft**
(`FleetBudget.hard=false`): admission control that rejects the *next* dispatch; a
single `Agent::run_task` can still overshoot its estimate. A **hard** cap needs
enforceable per-request maxima (provider output limits + a bounded tool-loop) and
stays a roadmap/open item.

## 6b. Executor safety, fanout & event routing (v1.1 — the execution layer)

The store is a lift; **this layer is genuinely new** and is where Phase 2a's real
work lives.

- **Closed task-worker registry (THE crux).** `Agent::run_task` is *not*
  intrinsically non-interactive — the built-in registry includes
  `ask_user_question` and command approvals that **block** on a requester. Do not
  rely on `tokio::spawn` accidentally dropping task-locals. The executor must be
  handed a **curated, audited NATIVE allowlist** that *cannot* park: the
  replay-safe work tools only (read / write / shell / search), `ApprovalPolicy::
  Never` for command tools, **no** question/input, peer, spawn/delegate, or
  plugin/MCP/session tools, plus a finite outer deadline. Only a native allowlist
  is auditable — the open `Tool` trait cannot prove an arbitrary plugin does not
  await a human internally. **This registry is what makes Phase 2a stateless.**
- **Fanout — durable Ready queue + permits, not raw spawn.** `goal_dispatch` marks
  the child `Ready`/`Launching` **durably** (the store *is* the queue); a bounded
  server-side worker pool runs Ready attempts under **per-fleet + global
  concurrency permits** (a semaphore), each with a **fresh owned `Agent` +
  registry** per attempt. Raw unbounded `tokio::spawn` from a tool body has no
  bound (`CostAccountant` is not a task-count semaphore; the 200-child cap lives
  only in `TaskSupervisor` registration, which a raw spawn bypasses).
- **FleetEvent outbox consumer + fleet-aware wake.** The peer wake does **not**
  free-generalize — it rejects ordinary (non-peer) sessions as `NotPeer` (there is
  a test). New required pieces: an **outbox consumer** that reads `OutboxEvent` →
  resolves `fleet_id → controller_session_key` → enqueues a keeper continuation
  (fleet-keyed, dedupe-by-`event_id`), and a **fleet-aware prompt renderer**
  (unknown `External(_)` events get only a generic prompt today). Enrich
  `OutboxEvent` to `{ sequence, event_id, fleet_id, child_id, attempt_id, kind,
  payload, claimed_by, claim_expires_at, acked }` — the prior draft's shape can't
  route to the controller or reclaim a crashed claim.
- **Keeper headless rehydration.** For a completion event to wake the keeper after
  a restart, the **keeper's own** session workspace must be re-established in
  memory (the global drain refuses a continuation whose workspace isn't loaded).
  The keeper (a goal session) needs the same server-side session establishment a
  session-worker would — a prerequisite, not free.
- **Task kinds — at-least-once safety.** Acceptance validators check
  *postconditions*; they cannot undo a half-written workspace or prove a
  deploy/charge/email/`git push` happened exactly once. v1 therefore restricts
  tasks to **replay-safe local work** (atomic/staged publish + fully automated
  validators) — which the closed allowlist enforces by *excluding* remote-mutating
  tools. Drop `Manual` acceptance from v1 (no headless mapping). Remote mutation
  would need a stable logical **effect-idempotency key** across retries (not the
  per-attempt `attempt_id`) — out of scope for v1.

## 7. First code PRs (Phase 2a)

1. **`FleetKernelStore`** — `fleet-kernel.redb`, records, the one-write-txn CAS ops
   (launch-lease / generation / revision, **budget-in-txn**), recovery
   reconciliation. Standalone module, unit-testable with a tempdir redb, **no
   LLM**. *Load-bearing, review-validated — the safe first PR.*
2. **Durable plan + `goal_get`/`goal_update`** returning/mutating the task graph
   (revision-fenced), replacing the free-text-only goal surface.
3. **Closed task-worker + bounded pool** (§6b) — the audited native-allowlist
   registry, a fresh `Agent` per attempt, per-fleet/global permits, the
   `run_attempt` record + acceptance-`ValidatorRunner` path. *The risk-carrying
   core; land it before wiring the keeper.*
4. **FleetEvent outbox consumer + fleet-aware wake** (§6b) + keeper headless
   rehydration.
5. **`goal_dispatch` tool + keeper hooks** — pre-turn plan render, the launch-and-
   return tool, wired into `GoalContinue`; the goal keeper now runs a fleet.

PR 1 is fully unit-testable with no LLM and is the recommended start; PR 3 (the
closed registry) is where the "stateless" guarantee is won.

## 8. Limitations (v1, named)

- **Budget is soft** until enforceable per-request maxima land.
- **Single-active-process** — redb's exclusive file lock; multi-writer needs a
  leader/fencing service (out of scope).
- **Stateless workers only** — bounded, non-interactive; a sub-task cannot itself
  park for input. Interactive sub-tasks are Phase 2b.

## References

- Grounding: `octos-swarm/src/{persistence,dispatcher,result}.rs`,
  `octos-agent/src/{cost_ledger,validators,agent/loop_runner}.rs`,
  `octos-cli/src/api/{agent_orchestrator,ui_protocol}.rs`,
  `octos-cli/src/goal_tool.rs`.
- `docs/FLEET-KERNEL-FOUNDATION-SPEC.md`, `docs/FLEET-RUNTIME-ADR.md`.
