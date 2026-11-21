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
//! - **Queue-full drops leave a DURABLE gap, not just a log (#2021).** The
//!   bounded sink channel can fill faster than the drain appends. Previously
//!   that drop was only a `debug!` line, so a reconnecting client replayed a
//!   stream that looked complete. [`record_human_sink_drop`] appends ONE
//!   rate-limited marker row per session to the SAME durable ledger the drain
//!   uses, so the replay shows the hole.
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

/// #2021 — one session's unflushed sink drops: how many, plus the most recent
/// dropped event (its origin attribution labels the gap marker).
struct PendingSinkDrops {
    count: u64,
    template: BackgroundActivityEvent,
    /// Last time a marker for this session was successfully appended. Bounds
    /// marker writes to one per window even though the drain flushes often.
    last_flushed: Option<Instant>,
}

fn pending_sink_drops() -> &'static StdMutex<HashMap<octos_core::SessionKey, PendingSinkDrops>> {
    static PENDING: OnceLock<StdMutex<HashMap<octos_core::SessionKey, PendingSinkDrops>>> =
        OnceLock::new();
    PENDING.get_or_init(|| StdMutex::new(HashMap::new()))
}

/// #2021 — producer-side drop accounting. Runs on the emitter's thread when
/// the bounded sink queue is full: ONLY increments an in-memory counter
/// (poison-tolerant mutex, no IO, no ledger), so a producer never waits on
/// storage — even when slow storage is exactly why the queue filled. The
/// drain persists the counts via [`flush_human_sink_drops`]; a drop noted
/// before the drain's ledger exists is still accounted (startup window
/// closed), and a session whose client is gone still gets its marker once
/// the drain next flushes.
pub(crate) fn note_human_sink_drop(dropped: BackgroundActivityEvent) {
    note_sink_drop_into(pending_sink_drops(), dropped);
}

/// [`note_human_sink_drop`] against an explicit map (tests isolate their own).
fn note_sink_drop_into(
    map: &StdMutex<HashMap<octos_core::SessionKey, PendingSinkDrops>>,
    dropped: BackgroundActivityEvent,
) {
    let mut pending = map.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let entry = pending
        .entry(dropped.session_id.clone())
        .or_insert_with(|| PendingSinkDrops {
            count: 0,
            template: dropped.clone(),
            last_flushed: None,
        });
    entry.count = entry.count.saturating_add(1);
    entry.template = dropped;
}

/// Minimum spacing between durable gap markers for one session. The drain
/// flushes after every event and on a tick; the window keeps that from
/// turning into a marker-per-burst write amplification on the same storage
/// that is already slow enough to fill the queue.
const HUMAN_SINK_GAP_WINDOW: Duration = Duration::from_secs(60);

/// Drain-side persistence for [`note_human_sink_drop`]. Writes AT MOST ONE
/// gap marker per session per [`HUMAN_SINK_GAP_WINDOW`]; drops noted inside
/// the window keep accumulating and merge into the NEXT window's marker.
/// A session's count is cleared only after its append succeeded — a failed
/// append leaves the full count for a later flush, so no drop is ever lost.
/// Called from the drain task (never the producer), so the synchronous
/// ledger append cannot stall an emitter.
#[cfg(feature = "api")]
pub(crate) fn flush_human_sink_drops(
    ledger: &crate::api::ui_protocol_ledger::UiProtocolLedger,
    now: Instant,
) {
    flush_human_sink_drops_with(pending_sink_drops(), now, |marker| {
        ledger
            .try_append_notification(octos_core::ui_protocol::UiNotification::BackgroundActivity(
                marker,
            ))
            .is_ok()
    });
}

/// The flush loop, with the durable append injectable for tests. Attempts
/// EVERY eligible session at most once per pass (alan0x review): a session
/// whose append keeps failing is restored for the next pass and must not
/// starve the healthy sessions behind it.
fn flush_human_sink_drops_with(
    map: &StdMutex<HashMap<octos_core::SessionKey, PendingSinkDrops>>,
    now: Instant,
    mut append: impl FnMut(BackgroundActivityEvent) -> bool,
) {
    let mut attempted = std::collections::HashSet::new();
    loop {
        // Pick ONE eligible session not yet tried this pass, append WITHOUT
        // holding the map lock, and only then settle its count. The
        // last-flushed stamp stays behind so the window is honored even if
        // the append fails.
        let candidate = {
            let pending = map.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            pending.iter().find_map(|(session, entry)| {
                let window_open = entry
                    .last_flushed
                    .is_none_or(|last| now.duration_since(last) >= HUMAN_SINK_GAP_WINDOW);
                (entry.count > 0 && window_open && !attempted.contains(session))
                    .then(|| (session.clone(), entry.count, entry.template.clone()))
            })
        };
        let Some((session, count, template)) = candidate else {
            return;
        };
        attempted.insert(session.clone());
        // Mark the count as in-flight by zeroing it under the lock, so a
        // concurrent note starts a NEW count instead of double-counting.
        let mut pending = map.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(entry) = pending.get_mut(&session) {
            entry.count = entry.count.saturating_sub(count);
        }
        drop(pending);
        let mut marker = template.clone();
        marker.suppressed = true;
        marker.dropped_count = Some(count);
        marker.text = "background activity was dropped because the human event \
                       queue was full; the replay above has a gap here"
            .to_string();
        let appended = append(marker);
        let mut pending = map.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if appended {
            if let Some(entry) = pending.get_mut(&session) {
                entry.last_flushed = Some(now);
            }
            // Continue with any OTHER eligible session.
            continue;
        }
        // Restore the un-appended count (merged with drops noted while we
        // were appending) so a later pass retries with the full total, and
        // KEEP GOING: other sessions must still record their gaps this pass.
        let entry = pending.entry(session).or_insert_with(|| PendingSinkDrops {
            count: 0,
            template: template.clone(),
            last_flushed: None,
        });
        entry.count = entry.count.saturating_add(count);
    }
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

#[cfg(all(test, feature = "api"))]
mod tests {
    use super::*;
    use crate::api::ui_protocol_ledger::UiProtocolLedger;
    use octos_core::ui_protocol::UiNotification;

    fn activity(session: &str, origin: &str, text: &str) -> BackgroundActivityEvent {
        background_activity(
            &octos_core::SessionKey(session.to_owned()),
            Some("dev"),
            "monitor",
            origin,
            None,
            text,
        )
    }

    fn from_start(session: &octos_core::SessionKey) -> octos_core::ui_protocol::UiCursor {
        octos_core::ui_protocol::UiCursor {
            stream: session.0.clone(),
            seq: 0,
        }
    }

    fn replayed_activity(
        ledger: &UiProtocolLedger,
        session: &octos_core::SessionKey,
    ) -> Vec<BackgroundActivityEvent> {
        ledger
            .replay_after(session, Some(&from_start(session)))
            .expect("replay succeeds")
            .into_iter()
            .filter_map(|event| match event.event {
                crate::api::ui_protocol_ledger::UiProtocolLedgerEvent::Notification(
                    UiNotification::BackgroundActivity(activity),
                ) => Some(activity),
                _ => None,
            })
            .collect()
    }

    /// #2021 / alan0x review — the producer notes drops with NO ledger access
    /// (a full queue caused by slow storage must not stall the emitter on the
    /// same storage), drops noted while the writer is unavailable accumulate,
    /// and once the writer resumes the flush persists ONE marker with the
    /// full count. Marker writes are bounded per window: a second burst
    /// inside the window appends NOTHING, and its drops merge into the next
    /// window's marker (lead-core rework: no marker-per-burst write
    /// amplification on already-slow storage).
    #[test]
    fn should_buffer_drops_while_writer_is_down_and_flush_once_per_window_on_recovery() {
        let key = octos_core::SessionKey("web:tester#sink-gap-buffered".to_owned());
        let map = StdMutex::new(HashMap::new());
        let now = Instant::now();
        // Writer "down": the producer only touches the in-memory counter and
        // returns immediately — no ledger exists at this point at all.
        for _ in 0..3 {
            note_sink_drop_into(&map, activity("web:tester#sink-gap-buffered", "m1", "lost"));
        }
        // Writer "resumes": the drain flushes; one durable gap row appears.
        let ledger = UiProtocolLedger::new(64);
        flush_human_sink_drops_with(&map, now, |marker| {
            ledger
                .try_append_notification(
                    octos_core::ui_protocol::UiNotification::BackgroundActivity(marker),
                )
                .is_ok()
        });
        let events = replayed_activity(&ledger, &key);
        assert_eq!(events.len(), 1, "one marker for the burst: {events:?}");
        assert!(events[0].suppressed);
        assert_eq!(
            events[0].dropped_count,
            Some(3),
            "all buffered drops accounted"
        );
        assert_eq!(events[0].origin_id, "m1");
        assert!(events[0].text.contains("queue was full"));
        // Counts were cleared by the successful append: no duplicate marker.
        flush_human_sink_drops_with(&map, now, |marker| {
            ledger
                .try_append_notification(
                    octos_core::ui_protocol::UiNotification::BackgroundActivity(marker),
                )
                .is_ok()
        });
        assert_eq!(replayed_activity(&ledger, &key).len(), 1);

        // A second burst INSIDE the window: no new marker (bounded writes).
        note_sink_drop_into(&map, activity("web:tester#sink-gap-buffered", "m1", "lost"));
        note_sink_drop_into(&map, activity("web:tester#sink-gap-buffered", "m1", "lost"));
        flush_human_sink_drops_with(&map, now + Duration::from_secs(5), |marker| {
            ledger
                .try_append_notification(
                    octos_core::ui_protocol::UiNotification::BackgroundActivity(marker),
                )
                .is_ok()
        });
        assert_eq!(
            replayed_activity(&ledger, &key).len(),
            1,
            "window-bounded: the second burst appends nothing yet"
        );
        // After the window the buffered drops merge into the NEXT marker.
        flush_human_sink_drops_with(
            &map,
            now + HUMAN_SINK_GAP_WINDOW + Duration::from_secs(1),
            |marker| {
                ledger
                    .try_append_notification(
                        octos_core::ui_protocol::UiNotification::BackgroundActivity(marker),
                    )
                    .is_ok()
            },
        );
        let events = replayed_activity(&ledger, &key);
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[1].dropped_count,
            Some(2),
            "window-inner drops merge forward"
        );
        // And nothing further without new drops.
        flush_human_sink_drops_with(&map, now + 2 * HUMAN_SINK_GAP_WINDOW, |marker| {
            ledger
                .try_append_notification(
                    octos_core::ui_protocol::UiNotification::BackgroundActivity(marker),
                )
                .is_ok()
        });
        assert_eq!(replayed_activity(&ledger, &key).len(), 2);
    }

    /// alan0x review — one session with a persistently failing append must
    /// not starve the others: each eligible session is attempted once per
    /// flush, failed counts are retained for the next pass, and the healthy
    /// session's gap is recorded on the SAME pass.
    #[test]
    fn should_not_starve_healthy_sessions_when_one_session_append_fails() {
        let good = octos_core::SessionKey("web:tester#gap-good".to_owned());
        let bad = octos_core::SessionKey("web:tester#gap-bad".to_owned());
        let map = StdMutex::new(HashMap::new());
        let now = Instant::now();
        note_sink_drop_into(&map, activity("web:tester#gap-good", "m-good", "lost"));
        note_sink_drop_into(&map, activity("web:tester#gap-bad", "m-bad", "lost"));

        let attempts = std::sync::Arc::new(StdMutex::new(Vec::new()));
        let attempts_in = attempts.clone();
        let bad_key = bad.clone();
        let good_key = good.clone();
        flush_human_sink_drops_with(&map, now, move |marker| {
            attempts_in
                .lock()
                .unwrap()
                .push(marker.session_id.0.clone());
            marker.session_id != bad_key
        });
        {
            let attempts = attempts.lock().unwrap();
            assert_eq!(
                attempts.len(),
                2,
                "each session attempted once: {attempts:?}"
            );
            assert!(
                attempts.contains(&good_key.0),
                "the healthy session records its gap on the same pass"
            );
            assert!(attempts.contains(&bad.0));
        }
        // The failed session's count is retained and retried on the next
        // pass; the healthy one does not append again inside its window.
        let attempts = std::sync::Arc::new(StdMutex::new(Vec::new()));
        let attempts_in = attempts.clone();
        let bad_key = bad.clone();
        flush_human_sink_drops_with(
            &map,
            now + HUMAN_SINK_GAP_WINDOW + Duration::from_secs(1),
            move |marker| {
                attempts_in
                    .lock()
                    .unwrap()
                    .push(marker.session_id.0.clone());
                marker.session_id != bad_key
            },
        );
        assert_eq!(*attempts.lock().unwrap(), vec![bad.0.clone()]);
    }

    /// #2021 — agent wake and UI stream are two READ-ONLY projections of one
    /// authoritative source: replaying the same ledger twice (the reconnect
    /// path) yields identical order, cursors, and payloads, and a replay from
    /// the head is empty rather than re-delivering old events.
    #[test]
    fn should_project_one_ordered_stream_to_both_consumers_without_duplicates() {
        let ledger = UiProtocolLedger::new(64);
        let map = StdMutex::new(HashMap::new());
        let key = octos_core::SessionKey("web:tester#dual-projection".to_owned());
        ledger.append_notification(UiNotification::BackgroundActivity(activity(
            "web:tester#dual-projection",
            "m1",
            "first",
        )));
        note_sink_drop_into(&map, activity("web:tester#dual-projection", "m1", "lost"));
        flush_human_sink_drops_with(&map, Instant::now(), |marker| {
            ledger
                .try_append_notification(
                    octos_core::ui_protocol::UiNotification::BackgroundActivity(marker),
                )
                .is_ok()
        });
        ledger.append_notification(UiNotification::BackgroundActivity(activity(
            "web:tester#dual-projection",
            "m1",
            "second",
        )));

        let (agent_view, agent_head) = ledger
            .replay_after_with_head(&key, Some(&from_start(&key)))
            .unwrap();
        let (ui_view, ui_head) = ledger
            .replay_after_with_head(&key, Some(&from_start(&key)))
            .unwrap();
        assert_eq!(agent_view, ui_view, "both projections read one source");
        assert_eq!(agent_head, ui_head, "identical cursor head");
        assert_eq!(agent_view.len(), 3);

        let cursor = octos_core::ui_protocol::UiCursor {
            stream: key.0.clone(),
            seq: agent_head,
        };
        assert!(ledger.replay_after(&key, Some(&cursor)).unwrap().is_empty());

        ledger.append_notification(UiNotification::BackgroundActivity(activity(
            "web:tester#dual-projection",
            "m1",
            "third",
        )));
        let delta = ledger.replay_after(&key, Some(&cursor)).unwrap();
        assert_eq!(delta.len(), 1, "only the post-cursor event replays");
    }
}
