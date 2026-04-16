//! Loop activity tracking for idle-timeout enforcement.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::progress::{ProgressEvent, ProgressReporter};

/// Default time without any progress before the loop is considered wedged.
///
/// This is intentionally distinct from the wall-clock timeout. The loop may
/// run for longer overall, but if it stops making observable progress for this
/// long, we treat it as idle.
pub(crate) const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 300;

#[derive(Debug)]
pub(crate) struct LoopActivityState {
    last_activity_at: Mutex<Instant>,
}

impl LoopActivityState {
    pub(crate) fn new(started_at: Instant) -> Self {
        Self {
            last_activity_at: Mutex::new(started_at),
        }
    }

    pub(crate) fn mark_activity(&self) {
        *self
            .last_activity_at
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Instant::now();
    }

    pub(crate) fn idle_elapsed(&self) -> Duration {
        let last_activity_at = *self
            .last_activity_at
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        Instant::now().saturating_duration_since(last_activity_at)
    }

    pub(crate) fn has_timed_out(&self, limit: Duration) -> bool {
        self.idle_elapsed() >= limit
    }

    #[cfg(test)]
    pub(crate) fn set_last_activity_at(&self, instant: Instant) {
        *self
            .last_activity_at
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = instant;
    }
}

/// Reporter wrapper that records loop activity before delegating progress
/// events to the actual reporter.
#[derive(Clone)]
pub(crate) struct ActivityTrackingReporter {
    activity: Arc<LoopActivityState>,
    delegate: Arc<dyn ProgressReporter>,
}

impl ActivityTrackingReporter {
    pub(crate) fn new(
        activity: Arc<LoopActivityState>,
        delegate: Arc<dyn ProgressReporter>,
    ) -> Self {
        Self { activity, delegate }
    }
}

impl ProgressReporter for ActivityTrackingReporter {
    fn report(&self, event: ProgressEvent) {
        self.activity.mark_activity();
        self.delegate.report(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingReporter {
        calls: AtomicUsize,
    }

    impl CountingReporter {
        fn new() -> Self {
            Self {
                calls: AtomicUsize::new(0),
            }
        }
    }

    impl ProgressReporter for CountingReporter {
        fn report(&self, _event: ProgressEvent) {
            self.calls.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn active_progress_resets_idle_timeout() {
        let activity = Arc::new(LoopActivityState::new(Instant::now()));
        activity.set_last_activity_at(
            Instant::now() - Duration::from_secs(DEFAULT_IDLE_TIMEOUT_SECS + 10),
        );
        assert!(activity.has_timed_out(Duration::from_secs(DEFAULT_IDLE_TIMEOUT_SECS)));

        let delegate = Arc::new(CountingReporter::new());
        let reporter = ActivityTrackingReporter::new(activity.clone(), delegate.clone());
        reporter.report(ProgressEvent::Thinking { iteration: 1 });

        assert_eq!(delegate.calls.load(Ordering::SeqCst), 1);
        assert!(!activity.has_timed_out(Duration::from_secs(DEFAULT_IDLE_TIMEOUT_SECS)));
    }

    #[test]
    fn wedged_no_progress_trips_idle_timeout() {
        let activity = LoopActivityState::new(Instant::now());
        activity.set_last_activity_at(
            Instant::now() - Duration::from_secs(DEFAULT_IDLE_TIMEOUT_SECS + 1),
        );

        assert!(activity.has_timed_out(Duration::from_secs(DEFAULT_IDLE_TIMEOUT_SECS)));
    }
}
