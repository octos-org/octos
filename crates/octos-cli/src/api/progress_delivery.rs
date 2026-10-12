//! Ordered bridge from synchronous progress callbacks to an async transport.
//! Token deltas coalesce while the consumer is slow. Lifecycle/tool events
//! remain queued in order; only excess ephemeral text is shed, observably.

use serde_json::Value;
use std::collections::VecDeque;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};
use tokio::sync::{Notify, mpsc, oneshot};

const MAX_PENDING_TEXT_BYTES: usize = 4 * 1024 * 1024;
const MAX_COALESCED_TEXT_BYTES: usize = 64 * 1024;

enum Pending {
    Event(Value),
    Barrier(oneshot::Sender<()>),
}

#[derive(Default)]
struct Queue {
    entries: VecDeque<Pending>,
    text_bytes: usize,
    closed: bool,
}

struct Shared {
    queue: Mutex<Queue>,
    wake: Arc<Notify>,
    dropped: Arc<AtomicU64>,
    producers: AtomicUsize,
}

pub(super) struct ProgressDelivery(Arc<Shared>);

fn text_size(event: &Value) -> usize {
    if matches!(event["type"].as_str(), Some("token" | "reasoning_chunk")) {
        event["text"].as_str().map_or(0, str::len)
    } else {
        0
    }
}

fn coalesce(previous: &mut Value, next: &Value) -> bool {
    if text_size(previous) == 0
        || text_size(next) == 0
        || previous["type"] != next["type"]
        || previous["iteration"] != next["iteration"]
        || previous["thread_id"] != next["thread_id"]
        || text_size(previous) + text_size(next) > MAX_COALESCED_TEXT_BYTES
    {
        return false;
    }
    let appended = next["text"].as_str().unwrap();
    let joined = format!("{}{appended}", previous["text"].as_str().unwrap());
    previous["text"] = Value::String(joined);
    true
}

impl ProgressDelivery {
    pub(super) fn new(tx: mpsc::Sender<String>, dropped: Arc<AtomicU64>) -> Self {
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue::default()),
            wake: Arc::new(Notify::new()),
            dropped,
            producers: AtomicUsize::new(1),
        });
        let worker = shared.clone();
        let wake = shared.wake.clone();
        tokio::spawn(async move {
            loop {
                let notified = wake.notified();
                let shared = &worker;
                let (entry, closed) = {
                    let mut queue = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
                    let entry = queue.entries.pop_front();
                    if let Some(Pending::Event(event)) = &entry {
                        queue.text_bytes = queue.text_bytes.saturating_sub(text_size(event));
                    }
                    (entry, queue.closed)
                };
                match entry {
                    Some(Pending::Event(event)) => {
                        if tx.send(event.to_string()).await.is_err() {
                            let mut queue = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
                            queue.closed = true;
                            let count = 1 + queue
                                .entries
                                .iter()
                                .filter(|item| matches!(item, Pending::Event(_)))
                                .count();
                            shared.dropped.fetch_add(count as u64, Ordering::Relaxed);
                            queue.entries.clear();
                            tracing::warn!(
                                count,
                                "progress delivery receiver closed; pending events unavailable"
                            );
                            break;
                        }
                    }
                    Some(Pending::Barrier(done)) => {
                        let _ = done.send(());
                    }
                    None if closed => break,
                    None => {
                        notified.await;
                    }
                }
            }
        });
        Self(shared)
    }

    pub(super) fn push(&self, event: Value) {
        let mut queue = self.0.queue.lock().unwrap_or_else(|e| e.into_inner());
        let bytes = text_size(&event);
        if queue.closed
            || (bytes > 0 && queue.text_bytes.saturating_add(bytes) > MAX_PENDING_TEXT_BYTES)
        {
            let previous = self.0.dropped.fetch_add(1, Ordering::Relaxed);
            metrics::counter!("ws.send.drop.backpressure", "method" => "progress_text")
                .increment(1);
            if previous == 0 || previous.is_power_of_two() {
                tracing::warn!(
                    dropped = previous + 1,
                    closed = queue.closed,
                    "progress text overflow or closed receiver; replay-lossy counter advanced"
                );
            }
            return;
        }
        queue.text_bytes += bytes;
        if let Some(Pending::Event(last)) = queue.entries.back_mut()
            && coalesce(last, &event)
        {
            return;
        }
        queue.entries.push_back(Pending::Event(event));
        drop(queue);
        self.0.wake.notify_one();
    }

    /// Terminal result/error must not overtake queued deltas or tool completion.
    pub(super) async fn flush(&self) {
        let (done, wait) = oneshot::channel();
        {
            let mut queue = self.0.queue.lock().unwrap_or_else(|e| e.into_inner());
            if queue.closed {
                return;
            }
            queue.entries.push_back(Pending::Barrier(done));
        }
        self.0.wake.notify_one();
        let _ = wait.await;
    }
}

impl Clone for ProgressDelivery {
    fn clone(&self) -> Self {
        self.0.producers.fetch_add(1, Ordering::Relaxed);
        Self(self.0.clone())
    }
}

impl Drop for ProgressDelivery {
    fn drop(&mut self) {
        if self.0.producers.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.0
                .queue
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .closed = true;
            self.0.wake.notify_one();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn should_drain_critical_events_when_last_producer_drops() {
        let (tx, mut rx) = mpsc::channel(1);
        let delivery = ProgressDelivery::new(tx, Arc::new(AtomicU64::new(0)));
        delivery.push(json!({"type": "tool_start"}));
        delivery.push(json!({"type": "tool_end"}));
        drop(delivery);
        assert!(rx.recv().await.unwrap().contains("tool_start"));
        assert!(rx.recv().await.unwrap().contains("tool_end"));
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn should_preserve_critical_events_and_text_order_under_backpressure() {
        let (tx, mut rx) = mpsc::channel(1);
        let dropped = Arc::new(AtomicU64::new(0));
        let delivery = ProgressDelivery::new(tx, dropped.clone());
        delivery.push(json!({"type":"tool_start","tool_call_id":"one"}));
        for _ in 0..10_000 {
            delivery.push(json!({"type":"token","text":"x","iteration":1}));
        }
        delivery.push(json!({"type":"tool_end","tool_call_id":"one"}));
        delivery.push(json!({"type":"token","text":"next","iteration":2}));
        let finish = delivery.clone();
        let flushed = tokio::spawn(async move {
            finish.flush().await;
        });
        let mut rows = vec![];
        loop {
            let row: Value = serde_json::from_str(&rx.recv().await.unwrap()).unwrap();
            let last = row["text"] == "next";
            rows.push(row);
            if last {
                break;
            }
        }
        flushed.await.unwrap();
        assert_eq!(rows.first().unwrap()["type"], "tool_start");
        assert_eq!(rows[rows.len() - 2]["type"], "tool_end");
        assert_eq!(
            rows.iter()
                .filter(|r| r["iteration"] == 1)
                .map(|r| r["text"].as_str().unwrap().len())
                .sum::<usize>(),
            10_000
        );
        assert_eq!(dropped.load(Ordering::Relaxed), 0);
        assert!(rows.len() < 100, "deltas should coalesce");
    }

    #[tokio::test]
    async fn should_shed_only_excess_text_and_keep_tool_completion() {
        let (tx, mut rx) = mpsc::channel(1);
        let dropped = Arc::new(AtomicU64::new(0));
        let delivery = ProgressDelivery::new(tx, dropped.clone());
        delivery.push(
            json!({"type":"token","text":"x".repeat(MAX_PENDING_TEXT_BYTES+1),"iteration":1}),
        );
        delivery.push(json!({"type":"tool_end","tool_call_id":"one"}));
        assert_eq!(dropped.load(Ordering::Relaxed), 1);
        let row: Value = serde_json::from_str(&rx.recv().await.unwrap()).unwrap();
        assert_eq!(row["type"], "tool_end");
        delivery.flush().await;
    }

    #[tokio::test]
    async fn should_finish_flush_when_receiver_closes() {
        let (tx, rx) = mpsc::channel(1);
        let delivery = ProgressDelivery::new(tx, Arc::new(AtomicU64::new(0)));
        drop(rx);
        delivery.push(json!({"type":"tool_end"}));
        tokio::time::timeout(std::time::Duration::from_secs(1), delivery.flush())
            .await
            .unwrap();
    }
}
