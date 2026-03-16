//! Publish-subscribe event bus for agent lifecycle events.
//!
//! Wraps `ProgressEvent` in a multi-subscriber dispatch pattern.
//! Backward-compatible: `ProgressReporter` implementations can be
//! registered as subscribers via `EventBus::add_reporter()`.

use std::sync::Arc;

use crate::progress::{ProgressEvent, ProgressReporter};

/// Subscriber trait for receiving agent events.
pub trait EventSubscriber: Send + Sync {
    /// Called when an event is published.
    fn on_event(&self, event: &ProgressEvent);

    /// Optional filter — return false to skip events you don't care about.
    fn accepts(&self, _event: &ProgressEvent) -> bool {
        true
    }
}

/// Multi-subscriber event bus that dispatches `ProgressEvent`s.
///
/// Note: `EventBus` is `Send` but not `Sync`. For use across async tasks,
/// wrap in `Arc<Mutex<EventBus>>` or configure subscribers before sharing
/// an immutable reference (subscribers are `Arc<dyn EventSubscriber>`).
pub struct EventBus {
    subscribers: Vec<Arc<dyn EventSubscriber>>,
}

impl EventBus {
    pub fn new() -> Self {
        Self {
            subscribers: Vec::new(),
        }
    }

    /// Add a subscriber.
    pub fn subscribe(&mut self, subscriber: Arc<dyn EventSubscriber>) {
        self.subscribers.push(subscriber);
    }

    /// Bridge: wrap an existing `ProgressReporter` as a subscriber.
    pub fn add_reporter(&mut self, reporter: Arc<dyn ProgressReporter>) {
        self.subscribers.push(Arc::new(ReporterBridge(reporter)));
    }

    /// Publish an event to all subscribers.
    pub fn publish(&self, event: &ProgressEvent) {
        for sub in &self.subscribers {
            if sub.accepts(event) {
                sub.on_event(event);
            }
        }
    }

    /// Number of registered subscribers.
    pub fn subscriber_count(&self) -> usize {
        self.subscribers.len()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

/// Implement ProgressReporter so EventBus can be used as a drop-in replacement.
impl ProgressReporter for EventBus {
    fn report(&self, event: ProgressEvent) {
        self.publish(&event);
    }
}

/// Bridge adapter: wraps a `ProgressReporter` as an `EventSubscriber`.
struct ReporterBridge(Arc<dyn ProgressReporter>);

impl EventSubscriber for ReporterBridge {
    fn on_event(&self, event: &ProgressEvent) {
        self.0.report(event.clone());
    }
}

/// A subscriber that filters events by a predicate.
pub struct FilteredSubscriber<F> {
    inner: Arc<dyn EventSubscriber>,
    filter: F,
}

impl<F: Fn(&ProgressEvent) -> bool + Send + Sync> FilteredSubscriber<F> {
    pub fn new(inner: Arc<dyn EventSubscriber>, filter: F) -> Self {
        Self { inner, filter }
    }
}

impl<F: Fn(&ProgressEvent) -> bool + Send + Sync> EventSubscriber for FilteredSubscriber<F> {
    fn on_event(&self, event: &ProgressEvent) {
        self.inner.on_event(event);
    }

    fn accepts(&self, event: &ProgressEvent) -> bool {
        (self.filter)(event)
    }
}

/// A subscriber that collects events (for testing).
pub struct CollectingSubscriber {
    events: std::sync::Mutex<Vec<ProgressEvent>>,
}

impl CollectingSubscriber {
    pub fn new() -> Self {
        Self {
            events: std::sync::Mutex::new(Vec::new()),
        }
    }

    pub fn events(&self) -> Vec<ProgressEvent> {
        self.events.lock().unwrap().clone()
    }

    pub fn count(&self) -> usize {
        self.events.lock().unwrap().len()
    }
}

impl Default for CollectingSubscriber {
    fn default() -> Self {
        Self::new()
    }
}

impl EventSubscriber for CollectingSubscriber {
    fn on_event(&self, event: &ProgressEvent) {
        self.events.lock().unwrap().push(event.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::SilentReporter;
    use std::time::Duration;

    #[test]
    fn should_dispatch_to_subscribers() {
        let collector = Arc::new(CollectingSubscriber::new());
        let mut bus = EventBus::new();
        bus.subscribe(collector.clone());

        bus.publish(&ProgressEvent::TaskStarted {
            task_id: "t1".into(),
        });
        bus.publish(&ProgressEvent::Thinking { iteration: 1 });

        assert_eq!(collector.count(), 2);
    }

    #[test]
    fn should_bridge_progress_reporter() {
        let mut bus = EventBus::new();
        bus.add_reporter(Arc::new(SilentReporter));
        // Should not panic
        bus.publish(&ProgressEvent::TaskStarted {
            task_id: "t1".into(),
        });
        assert_eq!(bus.subscriber_count(), 1);
    }

    #[test]
    fn should_filter_events() {
        let collector = Arc::new(CollectingSubscriber::new());
        let filtered = Arc::new(FilteredSubscriber::new(collector.clone(), |e| {
            matches!(e, ProgressEvent::ToolCompleted { .. })
        }));

        let mut bus = EventBus::new();
        bus.subscribe(filtered);

        bus.publish(&ProgressEvent::TaskStarted {
            task_id: "t1".into(),
        });
        bus.publish(&ProgressEvent::ToolCompleted {
            name: "shell".into(),
            tool_id: "c1".into(),
            success: true,
            output_preview: "ok".into(),
            duration: Duration::from_millis(100),
        });

        assert_eq!(collector.count(), 1); // Only ToolCompleted
    }

    #[test]
    fn should_work_as_progress_reporter() {
        let collector = Arc::new(CollectingSubscriber::new());
        let mut bus = EventBus::new();
        bus.subscribe(collector.clone());

        let reporter: &dyn ProgressReporter = &bus;
        reporter.report(ProgressEvent::Thinking { iteration: 1 });

        assert_eq!(collector.count(), 1);
    }

    #[test]
    fn should_handle_empty_bus() {
        let bus = EventBus::new();
        // Should not panic with no subscribers
        bus.publish(&ProgressEvent::Thinking { iteration: 1 });
        assert_eq!(bus.subscriber_count(), 0);
    }

    #[test]
    fn should_support_multiple_subscribers() {
        let c1 = Arc::new(CollectingSubscriber::new());
        let c2 = Arc::new(CollectingSubscriber::new());
        let mut bus = EventBus::new();
        bus.subscribe(c1.clone());
        bus.subscribe(c2.clone());

        bus.publish(&ProgressEvent::Thinking { iteration: 1 });

        assert_eq!(c1.count(), 1);
        assert_eq!(c2.count(), 1);
    }
}
