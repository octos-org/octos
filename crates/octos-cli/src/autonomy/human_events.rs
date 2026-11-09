//! #2019 — the HUMAN sink over background events that today only wake the model.
//!
//! Background events already exist, are already durable, and already have
//! producers: [`super::monitor_runtime`] (#1977) turns a child probe's filtered
//! stdout lines into `External("monitor_fired")` master continuations, and
//! [`super::fleet_wake`] claims `ChildDone` / `FleetDrained` off the durable
//! fleet outbox into keeper continuations. Every one of them has exactly ONE
//! consumer: the model. A monitor can fire forty times during a self-critic
//! loop and the user sees nothing, because the only effect of an event is that
//! the master gets woken — whether any of it reaches the transcript depends on
//! whether the model volunteers it.
//!
//! This module is the SECOND consumer: the human. It is a process-global,
//! best-effort tap that producers call in addition to (never instead of) the
//! wake they already perform.
//!
//! ## Invariants
//!
//! - **Additive only.** Nothing here changes how or when the model is woken.
//!   Producers call [`emit_background_activity`] AFTER their durable wake work.
//! - **Never blocks or fails the producer.** The installed sink is expected to
//!   be non-blocking (the `api` surface installs a bounded-channel `try_send`);
//!   a missing sink, a full queue, or a poisoned lock all degrade to a dropped
//!   event plus a log line. No producer path can stall or error on this.
//! - **Routing is by owning session.** Every emitted event carries the
//!   `session_id` of the session that OWNS the emitter. Activity without one
//!   renders in whichever session happens to be focused (octos-tui#461, #466,
//!   #483); an event with a blank key is refused here rather than shipped.
//! - **Capped, with a VISIBLE drop marker.** Unbounded emission is a client
//!   DoS. The per-origin budget below caps admissions per window and emits ONE
//!   explicit marker row when it starts dropping — silent truncation reads as
//!   "nothing more happened" (#2015).
//! - **Never fed back into model context.** This stream exists so the human can
//!   see what the model was woken by. Routing it back would be a compaction and
//!   cost problem, and would make the loop observe itself. The model's view is
//!   unchanged: it still gets the continuation metadata + the monitor notes.

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::{Duration, Instant};

use octos_core::ui_protocol::BackgroundActivityEvent;

/// The installed human sink. Sync + `Send + Sync` so any producer (a watcher
/// task, the outbox consumer, a blocking-pool sweep) can call it directly.
/// Implementations MUST NOT block.
pub(crate) type BackgroundActivitySink = Arc<dyn Fn(BackgroundActivityEvent) + Send + Sync>;

/// Rolling window for the per-origin admission budget.
pub(crate) const BACKGROUND_ACTIVITY_WINDOW: Duration = Duration::from_secs(60);
/// Max events admitted per origin per [`BACKGROUND_ACTIVITY_WINDOW`]. A monitor
/// is already flood-capped at the durable seam (`max_events_per_hour`); this is
/// the independent CLIENT-side bound, so a pathological or multi-origin burst
/// can never turn the transcript into a DoS.
pub(crate) const BACKGROUND_ACTIVITY_MAX_PER_WINDOW: u32 = 40;
/// Per-event text cap. Monitor lines are already clipped upstream; this is the
/// backstop for any other origin.
pub(crate) const BACKGROUND_ACTIVITY_TEXT_CAP: usize = 2_000;
/// Bound on the budget map. There is no origin-close hook, so cap growth: a
/// reset only re-admits one extra window's worth per origin.
const MAX_TRACKED_ORIGINS: usize = 4_096;

fn sink_slot() -> &'static StdMutex<Option<BackgroundActivitySink>> {
    static SLOT: OnceLock<StdMutex<Option<BackgroundActivitySink>>> = OnceLock::new();
    SLOT.get_or_init(|| StdMutex::new(None))
}

fn budget_slot() -> &'static StdMutex<HashMap<String, OriginBudget>> {
    static SLOT: OnceLock<StdMutex<HashMap<String, OriginBudget>>> = OnceLock::new();
    SLOT.get_or_init(|| StdMutex::new(HashMap::new()))
}

/// Install the process-global human sink. Called once by the `api` surface at
/// serve boot; a second call replaces the first (the ledger is a process
/// singleton, so re-installing is idempotent in practice).
pub(crate) fn set_background_activity_sink(sink: BackgroundActivitySink) {
    let mut slot = sink_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *slot = Some(sink);
}

/// What the per-origin cap decided for one candidate event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CapDecision {
    /// Emit the event. `dropped` is the suppression backlog this event
    /// accounts for (0 when nothing was dropped).
    Admit { dropped: u64 },
    /// The budget just ran out. Emit ONE marker row instead of the event, so
    /// the user is told the stream is being truncated. `dropped` is the
    /// backlog the marker accounts for.
    Marker { dropped: u64 },
    /// Already marked this window: drop silently (the marker already said so).
    Drop,
}

/// One origin's rolling-window admission state. Shared by the
/// `background/activity` human sink and the `monitor/expired` sink's
/// per-session budget (#2498); the fields are private to this module —
/// other seams only ever pass the map back into [`cap_admit`].
#[derive(Debug)]
pub(crate) struct OriginBudget {
    window_start: Instant,
    emitted: u32,
    /// Suppressed events not yet reported to the human.
    suppressed: u64,
    /// Whether the drop marker was already emitted for the current window.
    marked: bool,
}

/// Pure cap core (extracted so tests can drive it with a synthetic clock — the
/// outer `static` map cannot be reset per case). Shared by the
/// `monitor/expired` sink's per-session budget (#2498).
pub(crate) fn cap_admit(
    budgets: &mut HashMap<String, OriginBudget>,
    key: &str,
    now: Instant,
    window: Duration,
    max_per_window: u32,
) -> CapDecision {
    if budgets.len() >= MAX_TRACKED_ORIGINS && !budgets.contains_key(key) {
        budgets.clear();
    }
    let budget = budgets.entry(key.to_owned()).or_insert(OriginBudget {
        window_start: now,
        emitted: 0,
        suppressed: 0,
        marked: false,
    });
    if now.duration_since(budget.window_start) >= window {
        budget.window_start = now;
        budget.emitted = 0;
        budget.marked = false;
    }
    if budget.emitted < max_per_window {
        budget.emitted = budget.emitted.saturating_add(1);
        return CapDecision::Admit {
            dropped: std::mem::take(&mut budget.suppressed),
        };
    }
    budget.suppressed = budget.suppressed.saturating_add(1);
    if budget.marked {
        return CapDecision::Drop;
    }
    budget.marked = true;
    CapDecision::Marker {
        dropped: std::mem::take(&mut budget.suppressed),
    }
}

/// The cap key. Per (session, origin) so one noisy monitor cannot starve a
/// sibling origin in the same session, and two sessions never share a budget.
fn cap_key(event: &BackgroundActivityEvent) -> String {
    format!(
        "{}\u{0}{}\u{0}{}",
        event.session_id.0, event.origin_kind, event.origin_id
    )
}

/// Emit one background event to the HUMAN sink. Best-effort in every failure
/// mode; the caller's wake path is never blocked, delayed, or failed by this.
pub(crate) fn emit_background_activity(mut event: BackgroundActivityEvent) {
    // An event without a routing key would render in whichever session the
    // client happens to have focused. Refuse it here rather than ship the bug.
    if event.session_id.0.trim().is_empty() {
        tracing::warn!(
            origin_kind = %event.origin_kind,
            origin_id = %event.origin_id,
            "background activity dropped: empty session_id (would misroute)"
        );
        return;
    }
    // Cheap early-out: with no sink installed (unfeatured build, `octos chat`,
    // unit tests) do not even touch the budget map.
    let sink = {
        let slot = sink_slot()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match slot.as_ref() {
            Some(sink) => sink.clone(),
            None => return,
        }
    };
    octos_core::truncate_utf8(&mut event.text, BACKGROUND_ACTIVITY_TEXT_CAP, " [...]");
    let decision = {
        let key = cap_key(&event);
        let mut budgets = budget_slot()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        cap_admit(
            &mut budgets,
            &key,
            Instant::now(),
            BACKGROUND_ACTIVITY_WINDOW,
            BACKGROUND_ACTIVITY_MAX_PER_WINDOW,
        )
    };
    match decision {
        CapDecision::Admit { dropped } => {
            event.dropped_count = (dropped > 0).then_some(dropped);
        }
        CapDecision::Marker { dropped } => {
            event.suppressed = true;
            event.dropped_count = Some(dropped);
            event.text = format!(
                "further events from this origin are suppressed \
                 (more than {BACKGROUND_ACTIVITY_MAX_PER_WINDOW} in \
                 {}s); the model is still being woken by every one of them",
                BACKGROUND_ACTIVITY_WINDOW.as_secs()
            );
        }
        CapDecision::Drop => {
            tracing::debug!(
                session = %event.session_id,
                origin_kind = %event.origin_kind,
                origin_id = %event.origin_id,
                "background activity suppressed by the per-origin cap"
            );
            return;
        }
    }
    sink(event);
}

/// Current wall clock in ms since the Unix epoch, for `emitted_at_ms`.
pub(crate) fn now_ms_for_activity() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or_default()
}

/// Build a background-activity event with the required routing key + origin
/// attribution already in place. Keeping construction here means no producer
/// can forget `session_id` or ship an unattributed line.
pub(crate) fn background_activity(
    session_id: &octos_core::SessionKey,
    profile_id: Option<&str>,
    origin_kind: &str,
    origin_id: &str,
    origin_label: Option<&str>,
    text: impl Into<String>,
) -> BackgroundActivityEvent {
    BackgroundActivityEvent {
        session_id: session_id.clone(),
        profile_id: profile_id.map(ToOwned::to_owned),
        origin_kind: origin_kind.to_owned(),
        origin_id: origin_id.to_owned(),
        origin_label: origin_label
            .map(str::trim)
            .filter(|label| !label.is_empty())
            .map(ToOwned::to_owned),
        text: text.into(),
        emitted_at_ms: now_ms_for_activity(),
        dropped_count: None,
        suppressed: false,
    }
}

/// Origin class for monitor event lines (#1977).
pub(crate) const ORIGIN_KIND_MONITOR: &str = "monitor";
/// Origin class for claimed fleet outbox events.
pub(crate) const ORIGIN_KIND_FLEET: &str = "fleet";
