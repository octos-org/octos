//! Fleet-kernel outbox consumer → keeper wake (PR 4a).
//!
//! A background consumer that claims durable [`octos_fleet`] outbox events
//! (`ChildDone` / `FleetDrained`) and turns each into a **keeper
//! continuation** on the fleet's controller session — reusing the same
//! pull-model wake machinery (`MasterContinuationScheduler`) that
//! `GoalContinue` and `peer_fleet_synthesis` already ride.
//!
//! ## Shape: testable core + thin loop
//!
//! [`drain_fleet_outbox_once`] is a pure, singleton-free core: it takes the
//! store, a `now` clock (refreshed per claim), a per-tick batch cap, and a
//! `commit_wake` callback, so tests drive it against a fresh
//! [`MasterContinuationScheduler`] + a tempdir store.
//! [`spawn_fleet_outbox_consumer`] is the process loop; it calls
//! [`crate::autonomy::agent_orchestrator::InProcessAgentOrchestrator::drain_fleet_outbox`],
//! which supplies a `commit_wake` closure that locks the runtime state **only**
//! for the synchronous enqueue+persist — the async store I/O never runs under
//! the `std::sync::Mutex` guard.
//!
//! ## Durability (never lose a wake)
//!
//! The commit callback routes the wake through the SAME durable-persist path
//! the peer/goal wakes use, and the core acks an outbox event **only** once its
//! wake is [`WakeCommit::Durable`] (persisted, or a duplicate of an
//! already-recorded occurrence). A wake that is only in-memory (no supervisor
//! store) or whose persist failed is [`WakeCommit::NotDurable`]: the event is
//! left claimed and redelivers after the lease lapses, so a crash between the
//! in-memory enqueue and the controller draining it can never drop the wake.
//!
//! ## Dormant-but-correct (PR 4a scope)
//!
//! Nothing writes live fleet events until a later PR, so this consumer is inert
//! in production today; it is proven with synthetic events in the unit tests
//! below. It wakes a keeper whose workspace is already loaded (an interactive /
//! connected goal session); headless rehydration is PR 4b.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use octos_core::SessionKey;
use octos_fleet::{AckOutcome, Fleet, FleetEventKind, FleetKernelStore, FleetStatus};
use tokio::time::MissedTickBehavior;

use super::agent_orchestrator::{
    FLEET_KEEPER_EXTERNAL_KIND, FLEET_KEEPER_GROUP, FLEET_KEEPER_META_FLEET_ID,
    FLEET_KEEPER_META_OBJECTIVE, FLEET_KEEPER_META_READY, FLEET_KEEPER_META_TASK_LINES,
    FLEET_KEEPER_META_WORKSPACE_HAS_RUNTIME_HINT, FLEET_KEEPER_META_WORKSPACE_ROOT,
    InProcessAgentOrchestrator, default_agent_orchestrator,
};
use super::master_continuation_scheduler::{MasterContinuationReason, MasterContinuationRequest};

/// Consumer id stamped onto claims this loop takes (`claimed_by`).
pub(crate) const FLEET_WAKE_CONSUMER: &str = "fleet-wake";
/// Claim lease TTL. A crashed consumer's claim becomes reclaimable after this
/// window (matches the outbox's `recently_claimed_external` occurrence window).
const FLEET_WAKE_TTL_MS: u64 = 30_000;
/// Poll interval for the background loop.
const FLEET_WAKE_INTERVAL_SECS: u64 = 3;
/// Max events acked per drain tick. Bounds a single tick so a large or
/// continuous outbox cannot monopolise the consumer or build an unbounded
/// continuation backlog; the remainder is picked up on the next tick.
pub(crate) const FLEET_WAKE_MAX_BATCH: usize = 64;

/// Whether a keeper wake was durably committed — the ack gate for
/// [`drain_fleet_outbox_once`]. The `commit_wake` callback returns this so the
/// core acks an outbox event ONLY once its wake is durable; a `NotDurable` wake
/// is left claimed to redeliver after the lease lapses (never silently lost).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WakeCommit {
    /// Persisted to the durable continuation store (or a duplicate of an
    /// already-recorded occurrence). Safe to ack the outbox event.
    Durable,
    /// Enqueued only in-memory (no supervisor store) or the persist failed. Do
    /// NOT ack — leave the event for redelivery.
    NotDurable,
}

/// Pre-rendered fleet snapshot stuffed into the keeper continuation's
/// metadata. Rendering does the (async) plan reads here so the SYNC prompt
/// renderer ([`crate::autonomy::agent_orchestrator::render_fleet_keeper_prompt`])
/// only formats these strings — no I/O on the render path.
#[derive(Debug, Default, Clone)]
pub(crate) struct FleetKeeperSnapshot {
    /// The plan objective (author-provided → rendered as untrusted).
    pub(crate) objective: String,
    /// One line per task: `- <task_id>: <title> [<status>]<verdict?>`.
    pub(crate) task_lines: String,
    /// Comma-separated ids of tasks ready to dispatch now.
    pub(crate) ready: String,
}

/// Read the fleet's plan graph + ready set and format them into a
/// [`FleetKeeperSnapshot`]. Best-effort: a partial read degrades to empty
/// fields rather than wedging the outbox (the fleet already resolved, so the
/// wake is still worth enqueuing). Only fixed verdict tags are emitted (never a
/// raw verdict reason), so the sole untrusted values are the objective + task
/// titles + task ids — all XML-escaped by the renderer.
async fn render_fleet_snapshot(
    store: &FleetKernelStore,
    fleet_id: &str,
    now_ms: u64,
) -> FleetKeeperSnapshot {
    let fleet = Fleet::bind(Arc::new(store.clone()), fleet_id.to_string());
    let view = fleet.view().await.ok();
    let ready = fleet.ready_tasks(now_ms).await.unwrap_or_default();

    let objective = view
        .as_ref()
        .map(|v| v.objective.clone())
        .unwrap_or_default();
    let task_lines = view
        .as_ref()
        .map(|v| {
            v.tasks
                .iter()
                .map(|t| {
                    let verdict = match &t.verdict {
                        Some(octos_fleet::AcceptanceVerdict::Accepted { .. }) => " → accepted",
                        Some(octos_fleet::AcceptanceVerdict::Rejected { .. }) => " → rejected",
                        Some(octos_fleet::AcceptanceVerdict::Terminated { .. }) => " → terminated",
                        None => "",
                    };
                    format!("- {}: {} [{:?}]{}", t.task_id, t.title, t.status, verdict)
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();

    FleetKeeperSnapshot {
        objective,
        task_lines,
        ready: ready.join(", "),
    }
}

/// The stable per-occurrence dedupe key for a keeper wake. It embeds the outbox
/// **`sequence`** — a unique, monotonic `u64` the store guarantees — as the
/// occurrence identity. A numeric terminal component is injective (the last
/// `/` unambiguously splits controller from sequence), so distinct occurrences
/// never collide the way a slash-concatenated `fleet_id`/`event_id` could
/// (`append_event` does not enforce unique `event_id`s); a genuine redelivery
/// of the same sequence correctly collapses within the 30s claim window.
fn fleet_keeper_dedupe_key(controller: &SessionKey, sequence: u64) -> String {
    format!("external/{FLEET_KEEPER_EXTERNAL_KIND}/{controller}/{sequence}")
}

/// Build the keeper wake continuation request for one outbox occurrence.
/// Mirrors `enqueue_peer_fleet_synthesis_continuation`, but pure (no
/// singleton) so the drain core stays testable — the caller applies + persists
/// it via the `commit_wake` callback.
pub(crate) fn fleet_keeper_continuation_request(
    controller: &SessionKey,
    profile_id: &str,
    fleet_id: &str,
    sequence: u64,
    snap: &FleetKeeperSnapshot,
    controller_workspace_root: Option<&str>,
    controller_workspace_has_runtime_hint: Option<bool>,
) -> MasterContinuationRequest {
    let mut req = MasterContinuationRequest::new(
        FLEET_KEEPER_GROUP,
        controller.to_string(),
        profile_id.to_string(),
        MasterContinuationReason::External(FLEET_KEEPER_EXTERNAL_KIND.to_owned()),
        SystemTime::now(),
    )
    .with_metadata(FLEET_KEEPER_META_FLEET_ID, fleet_id.to_owned())
    .with_metadata(FLEET_KEEPER_META_OBJECTIVE, snap.objective.clone())
    .with_metadata(FLEET_KEEPER_META_TASK_LINES, snap.task_lines.clone())
    .with_metadata(FLEET_KEEPER_META_READY, snap.ready.clone())
    .with_dedupe_key(fleet_keeper_dedupe_key(controller, sequence));
    // PR 4b: carry the persisted controller workspace root and its provenance
    // so the global drain can re-seed `session_workspaces()` for a HEADLESS
    // keeper without turning a derived Tier-3 root into a transcript-relocation
    // hint. Omit the root when the fleet persisted none; omit provenance only
    // for legacy/unknown records, which re-seed fail-safe without a runtime
    // hint. (The dedupe key is already built, so metadata does not perturb
    // per-occurrence de-duplication.)
    if let Some(root) = controller_workspace_root {
        req = req.with_metadata(FLEET_KEEPER_META_WORKSPACE_ROOT, root.to_owned());
    }
    if let Some(has_runtime_hint) = controller_workspace_has_runtime_hint {
        req = req.with_metadata(
            FLEET_KEEPER_META_WORKSPACE_HAS_RUNTIME_HINT,
            has_runtime_hint.to_string(),
        );
    }
    req
}

/// #2019 — the human-facing label for a fleet origin: the plan objective when
/// the snapshot read succeeded, else `None` (the client falls back to the
/// fleet id). Never a raw verdict reason — the snapshot only carries the
/// objective, task lines, and ready ids.
fn snapshot_label(snap: &FleetKeeperSnapshot) -> Option<&str> {
    let objective = snap.objective.trim();
    (!objective.is_empty()).then_some(objective)
}

/// #2019 — the human-facing one-liner for a claimed fleet outbox event. Says
/// WHAT happened and what is dispatchable now; the model's keeper prompt
/// (which carries the full task table) is unchanged and unrelated.
fn fleet_activity_text(kind: FleetEventKind, snap: &FleetKeeperSnapshot) -> String {
    let what = match kind {
        FleetEventKind::ChildDone => "a child task finished",
        FleetEventKind::FleetDrained => "the fleet drained (no runnable work left)",
        _ => "fleet event",
    };
    let ready = snap.ready.trim();
    if ready.is_empty() {
        what.to_owned()
    } else {
        format!("{what}; ready now: {ready}")
    }
}

/// Drain the fleet outbox once: claim up to `max_batch` currently-claimable
/// events, wake the controller for each `ChildDone` / `FleetDrained`, and ack
/// **only after the wake is durably committed**.
///
/// **Testable core.** `now` is refreshed per claim (a long drain must not reuse
/// one stale clock for lease math), `max_batch` bounds the per-tick work, and
/// `commit_wake` is the only side-channel to the scheduler — tests pass a
/// closure over a fresh [`MasterContinuationScheduler`]; the process loop
/// passes one that enqueues + durably persists under the singleton lock.
///
/// Order is claim → resolve controller → pre-render → **commit_wake → (durable
/// only) ack**. A `NotDurable` commit stops the tick and leaves the event for
/// redelivery, so a crash between the in-memory enqueue and delivery cannot
/// lose the wake. A `StaleClaim` (our lease was reclaimed) also breaks — the
/// new owner drives. Non-wake events and vanished fleets ack directly (nothing
/// to persist, no wedge). Returns the number of events acked.
pub(crate) async fn drain_fleet_outbox_once<N, F>(
    store: &FleetKernelStore,
    mut now: N,
    max_batch: usize,
    mut commit_wake: F,
) -> eyre::Result<usize>
where
    N: FnMut() -> u64,
    F: FnMut(MasterContinuationRequest) -> WakeCommit,
{
    let mut processed = 0usize;
    for _ in 0..max_batch {
        let now_ms = now();
        let Some(ev) = store
            .claim_next(FLEET_WAKE_CONSUMER, now_ms, FLEET_WAKE_TTL_MS)
            .await?
        else {
            break;
        };

        let durable = if matches!(
            ev.kind,
            FleetEventKind::ChildDone | FleetEventKind::FleetDrained
        ) {
            match store.get_fleet(&ev.fleet_id).await? {
                // #1973 fix-round — a CANCELLED fleet's `ChildDone` (a
                // completion/escalation that raced `goal_clear` in before the
                // store's terminal-fleet fence) has no keeper to wake: the
                // goal that owned it is gone, and a wake would re-animate a
                // dead controller with stale metadata. Ack without a wake.
                // Scoped to `Cancelled` only — a `Complete`/`Failed` fleet's
                // late events still wake the keeper, which legitimately
                // self-detects completion from them.
                Some(rec) if rec.status == FleetStatus::Cancelled => {
                    tracing::debug!(
                        sequence = ev.sequence,
                        fleet_id = %ev.fleet_id,
                        "fleet outbox: dropping wake for a cancelled fleet (goal cleared)"
                    );
                    true
                }
                Some(rec) => {
                    let snap = render_fleet_snapshot(store, &ev.fleet_id, now_ms).await;
                    let req = fleet_keeper_continuation_request(
                        &rec.controller_session_key,
                        &rec.profile_id,
                        &ev.fleet_id,
                        ev.sequence,
                        &snap,
                        rec.controller_workspace_root.as_deref(),
                        rec.controller_workspace_has_runtime_hint,
                    );
                    let durable = matches!(commit_wake(req), WakeCommit::Durable);
                    // #2019 — SECOND consumer: the human. The keeper wake above
                    // is untouched; this is a purely additive, best-effort tap
                    // so the user can SEE the fleet event that woke the
                    // controller. Emitted only once the wake is DURABLE, so a
                    // redelivered (not-yet-acked) event does not double-report.
                    // Routed on the CONTROLLER's session — the session that
                    // owns the keeper — and attributed to the fleet.
                    if durable {
                        crate::autonomy::human_events::emit_background_activity(
                            crate::autonomy::human_events::background_activity(
                                &rec.controller_session_key,
                                Some(rec.profile_id.as_str()),
                                crate::autonomy::human_events::ORIGIN_KIND_FLEET,
                                &ev.fleet_id,
                                snapshot_label(&snap),
                                fleet_activity_text(ev.kind, &snap),
                            ),
                        );
                    }
                    durable
                }
                // A vanished fleet has no wake to persist; ack so the outbox
                // advances rather than wedging on a missing record.
                None => true,
            }
        } else {
            // Non-terminal lifecycle events carry no keeper wake; just ack.
            true
        };

        if !durable {
            // The wake could not be made durable (no supervisor store, or a
            // persist failure). Do NOT ack: leave the event claimed so it
            // redelivers after the lease lapses rather than losing the wake.
            tracing::warn!(
                sequence = ev.sequence,
                fleet_id = %ev.fleet_id,
                "fleet keeper wake not durably persisted; leaving event for redelivery"
            );
            break;
        }

        let token = ev.claim_token.as_deref().unwrap_or_default();
        match store.ack(ev.sequence, FLEET_WAKE_CONSUMER, token).await? {
            AckOutcome::Acked => processed += 1,
            // Our lease was reclaimed by another owner; stop and let it drive.
            AckOutcome::StaleClaim => break,
        }
    }
    Ok(processed)
}

/// Spawn the background fleet outbox consumer loop over `store`. Mirrors
/// `spawn_global_master_continuation_drain`: an interval loop that drains
/// against the process orchestrator singleton's scheduler each tick.
pub(crate) fn spawn_fleet_outbox_consumer(store: FleetKernelStore) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(FLEET_WAKE_INTERVAL_SECS));
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        // First tick fires immediately; skip it so we don't race serve boot.
        tick.tick().await;
        tracing::info!(
            interval_secs = FLEET_WAKE_INTERVAL_SECS,
            "fleet outbox consumer loop started"
        );
        loop {
            tick.tick().await;
            let orch = default_agent_orchestrator();
            match orch.drain_fleet_outbox(&store).await {
                Ok(n) if n > 0 => tracing::debug!(events = n, "fleet outbox: drained events"),
                Ok(_) => {}
                Err(error) => tracing::warn!(%error, "fleet outbox drain failed"),
            }
        }
    });
}

/// The stable, boot-namespaced dedupe key for a fleet's boot-resume keeper wake.
///
/// Deliberately NOT the outbox consumer's sequence-based
/// [`fleet_keeper_dedupe_key`]: the literal `boot-resume` terminal can never
/// numerically collide a real outbox `sequence` (the consumer's key ends in a
/// `u64`), and the key is stable per fleet across boots — so a boot wake that
/// was persisted on a prior boot but not yet drained collapses with the fresh
/// one instead of double-queuing.
///
/// The `fleet_id` is IN the key (not just the controller): controller↔fleet is
/// NOT 1:1 — `goal_clear` removes a goal WITHOUT terminalizing its fleet, so a
/// cleared-then-replanned controller can transiently own two `Active` fleets
/// with `Ready` children. A controller-only key would make their two boot
/// wakes collide and silently discard one; keying on `fleet_id` keeps distinct
/// fleets distinct (belt-and-suspenders alongside the orphan guard in
/// [`enqueue_fleet_boot_resume_wakes`], which normally wakes only the bound
/// fleet).
fn fleet_boot_resume_dedupe_key(controller: &SessionKey, fleet_id: &str) -> String {
    format!("external/{FLEET_KEEPER_EXTERNAL_KIND}/{controller}/boot-resume/{fleet_id}")
}

/// Boot-resume — "a fleet survives an octos restart". After the boot reconcile
/// flips a restart-interrupted fleet's in-flight children back to `Ready`,
/// NOTHING re-dispatches them: reconcile emits no outbox event, so the outbox
/// consumer (the only other keeper-wake driver) never fires and the fleet stalls
/// forever. This closes that gap — for every live fleet with a launchable child
/// ([`FleetKernelStore::fleets_with_ready_children`]) it enqueues ONE keeper
/// wake, reusing the SAME continuation + PR-4b workspace seed the outbox
/// consumer builds, under a stable boot-namespaced dedupe key. The global
/// master-continuation drain (already running, ~5s poll) picks the wakes up on
/// its next tick → PR-4b reseed pre-pass → `run_standalone_turn` → the keeper's
/// `goal_dispatch` re-launches the ready set.
///
/// - **Orphan guard** — only a fleet STILL bound to its controller's current
///   goal is woken. `goal_clear` removes a goal without terminalizing its
///   fleet, and a re-plan rebinds the controller to a new fleet; in both cases
///   the keeper's `goal_dispatch` resolves ONLY the current goal's fleet, so a
///   superseded fleet has no keeper to drive it — waking it is useless and
///   would surface stale metadata. Such fleets are skipped + logged.
/// - **Double-wake** (a fleet the outbox consumer also wakes) is harmless: the
///   `launch_child` CAS + `resolve_and_collect_ready` + the per-session
///   active-turn guard admit at most one no-op keeper turn, never a
///   double-dispatch.
/// - **Not solo-gated** (matches the outbox consumer, which wakes on
///   `ChildDone` regardless of `--solo`): this recovers already-committed
///   in-flight work, not a fresh autonomous goal.
/// - **Rootless fleet** (`controller_workspace_root == None`): the wake carries
///   no workspace root, so the seed pre-pass drops it
///   ([`InProcessAgentOrchestrator::pending_fleet_keeper_seeds`]) — the same
///   not-headlessly-rehydratable limitation as the consumer path, not a
///   regression.
///
/// Returns the number of fleets for which a wake was DURABLY (or, with no
/// supervisor store, at least in-memory) enqueued — a persistence failure that
/// rolls the wake back is NOT counted (see below).
pub(crate) async fn enqueue_fleet_boot_resume_wakes(
    store: &FleetKernelStore,
    orchestrator: &InProcessAgentOrchestrator,
    now_ms: u64,
) -> eyre::Result<usize> {
    let fleets = store.fleets_with_ready_children(now_ms).await?;
    // With a supervisor store, a `NotDurable` commit can ONLY be a persistence
    // rollback (a failed persist cancels the in-memory enqueue); without one it
    // is the benign in-memory-only path. Captured once — it does not change
    // mid-pass.
    let has_store = orchestrator.has_supervisor_store();
    let mut enqueued = 0usize;
    for rec in fleets {
        // Orphan guard: skip a fleet no longer bound to its controller's current
        // goal (cleared or re-planned). No keeper will resolve it, so a wake is
        // wasted and carries stale metadata.
        let bound = orchestrator.goal_bound_fleet_id(&rec.controller_session_key);
        if bound.as_deref() != Some(rec.fleet_id.as_str()) {
            tracing::warn!(
                fleet_id = %rec.fleet_id,
                controller = %rec.controller_session_key,
                bound = ?bound,
                "fleet boot-resume: skipping an orphaned fleet not bound to its controller's \
                 current goal (goal cleared or re-planned); no keeper will drive it"
            );
            continue;
        }

        let snap = render_fleet_snapshot(store, &rec.fleet_id, now_ms).await;
        // Reuse the consumer's request builder — it writes the PR-4b workspace
        // seed when a root is present — then OVERRIDE its sequence-based dedupe
        // key with the stable, fleet-scoped boot key. `sequence` is unused here
        // (the key it would produce is replaced below), so pass 0.
        let req = fleet_keeper_continuation_request(
            &rec.controller_session_key,
            &rec.profile_id,
            &rec.fleet_id,
            0,
            &snap,
            rec.controller_workspace_root.as_deref(),
            rec.controller_workspace_has_runtime_hint,
        )
        .with_dedupe_key(fleet_boot_resume_dedupe_key(
            &rec.controller_session_key,
            &rec.fleet_id,
        ));
        // #1973 fix D — cloned up front so a failed persist can stash the
        // request for its one bounded retry (the commit consumes `req`).
        let retry_req = req.clone();
        match orchestrator.commit_fleet_keeper_wake(req) {
            // Persisted to the durable store — survives a further restart.
            WakeCommit::Durable => enqueued += 1,
            // No supervisor store → the wake is in-memory only: it still drives
            // THIS boot's drain (all boot-resume needs), it just won't survive
            // another restart. Benign — count it.
            WakeCommit::NotDurable if !has_store => enqueued += 1,
            // A supervisor store IS configured yet the commit is NotDurable —
            // the ONLY cause is a persistence error that ROLLED BACK the
            // in-memory enqueue. There is now NO wake, and reconcile emitted no
            // outbox event either. Surface it honestly; do NOT count it as a
            // success (never report false progress that masks a stall).
            // #1973 fix D — stash the request for ONE bounded retry on the next
            // fleet-outbox drain tick (~3s), so a transient persist failure no
            // longer strands the fleet for the whole boot. If the retry fails
            // too, the wake is dropped for this boot (the retry path warns).
            WakeCommit::NotDurable => {
                orchestrator.stash_fleet_wake_retry(retry_req);
                tracing::warn!(
                    fleet_id = %rec.fleet_id,
                    controller = %rec.controller_session_key,
                    "fleet boot-resume wake FAILED to persist and was rolled back; stashed for \
                     ONE retry on the next drain tick — if that also fails, this fleet will NOT \
                     auto-resume this boot (it stays Ready with no keeper wake until the next \
                     fleet event or a restart)"
                );
            }
        }
    }
    if enqueued > 0 {
        tracing::info!(
            fleets = enqueued,
            "fleet boot-resume: enqueued keeper wakes for restart-stranded fleets"
        );
    }
    Ok(enqueued)
}
