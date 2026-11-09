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
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn count(&self) -> usize {
        self.events.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

impl Default for CollectingSubscriber {
    fn default() -> Self {
        Self::new()
    }
}

impl EventSubscriber for CollectingSubscriber {
    fn on_event(&self, event: &ProgressEvent) {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(event.clone());
    }
}
