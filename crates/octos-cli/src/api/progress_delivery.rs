//! Ordered bridge from synchronous progress callbacks to an async transport.
//! Token deltas coalesce while the consumer is slow. Lifecycle/tool events
//! remain queued in order; excess ephemeral text is shed, observably, and so
//! are critical events once their queued payload exceeds its own budget.

use serde_json::Value;
use std::collections::VecDeque;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};
use tokio::sync::{Notify, mpsc, oneshot};

const MAX_PENDING_TEXT_BYTES: usize = 4 * 1024 * 1024;
const MAX_COALESCED_TEXT_BYTES: usize = 64 * 1024;
// Critical (non-text) events queue in order too, but a stalled consumer must
// not be able to grow this queue without bound: a long turn on a dead socket
// keeps emitting tool_start/tool_progress payloads whose tool arguments and
// progress messages are producer-sized. Same budget as the text side, same
// shed-and-count contract. (The consumer-facing mpsc in the transport adds a
// PROGRESS_CHANNEL_CAPACITY-slot backlog in front of this; this budget bounds
// what this module queues.)
const MAX_PENDING_EVENT_BYTES: usize = 4 * 1024 * 1024;
// flush() parks behind every queued row and the worker's channel send, so a
// consumer that stalls without disconnecting (TCP zero window, a parked
// reader, a peer gone quiet) would suspend turn termination indefinitely.
// The wait must still cover a healthy drain of this module's full budgets;
// past it everything still queued is shed and counted like any other
// backpressure loss, because the directly-sent terminal event overtakes the
// queue either way.
const FLUSH_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);

enum Pending {
    // Coalescable text, serialized at drain so merges keep folding into the
    // Value without re-serializing per merge.
    Text(Value),
    // Everything else, serialized once at push; the worker sends these exact
    // bytes, and `event_bytes` budgets their length.
    Critical { json: String },
    Barrier(oneshot::Sender<()>),
}

#[derive(Default)]
struct Queue {
    entries: VecDeque<Pending>,
    text_bytes: usize,
    event_bytes: usize,
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
                    match &entry {
                        Some(Pending::Text(event)) => {
                            queue.text_bytes = queue.text_bytes.saturating_sub(text_size(event));
                        }
                        Some(Pending::Critical { json }) => {
                            queue.event_bytes = queue.event_bytes.saturating_sub(json.len());
                        }
                        Some(Pending::Barrier(_)) | None => {}
                    }
                    (entry, queue.closed)
                };
                let payload = match entry {
                    Some(Pending::Text(event)) => event.to_string(),
                    Some(Pending::Critical { json }) => json,
                    Some(Pending::Barrier(done)) => {
                        let _ = done.send(());
                        continue;
                    }
                    None if closed => break,
                    None => {
                        notified.await;
                        continue;
                    }
                };
                if tx.send(payload).await.is_err() {
                    let mut queue = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
                    queue.closed = true;
                    let count = 1 + queue
                        .entries
                        .iter()
                        .filter(|item| matches!(item, Pending::Text(_) | Pending::Critical { .. }))
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
        });
        Self(shared)
    }

    pub(super) fn push(&self, event: Value) {
        let text = text_size(&event);
        // Text is budgeted by payload length. Every other event serializes
        // once here and is budgeted by its wire size — the worker sends those
        // same bytes, so for a shed event this serialization is the whole
        // measurement cost.
        let (size, wire) = if text > 0 {
            (text, None)
        } else {
            let json = event.to_string();
            (json.len(), Some(json))
        };
        let mut queue = self.0.queue.lock().unwrap_or_else(|e| e.into_inner());
        let over_budget = if text > 0 {
            queue.text_bytes.saturating_add(size) > MAX_PENDING_TEXT_BYTES
        } else {
            queue.event_bytes.saturating_add(size) > MAX_PENDING_EVENT_BYTES
        };
        if queue.closed || over_budget {
            let previous = self.0.dropped.fetch_add(1, Ordering::Relaxed);
            let (method, reason) = if text > 0 {
                (
                    "progress_text",
                    "progress text overflow or closed receiver; replay-lossy counter advanced",
                )
            } else {
                (
                    "progress_event",
                    "progress event overflow or closed receiver; replay-lossy counter advanced",
                )
            };
            metrics::counter!("ws.send.drop.backpressure", "method" => method).increment(1);
            if previous == 0 || previous.is_power_of_two() {
                tracing::warn!(dropped = previous + 1, closed = queue.closed, message = %reason);
            }
            return;
        }
        if let Some(json) = wire {
            queue.event_bytes += size;
            queue.entries.push_back(Pending::Critical { json });
        } else {
            queue.text_bytes += size;
            if let Some(Pending::Text(last)) = queue.entries.back_mut()
                && coalesce(last, &event)
            {
                return;
            }
            queue.entries.push_back(Pending::Text(event));
        }
        drop(queue);
        self.0.wake.notify_one();
    }

    /// Terminal result/error must not overtake queued deltas or tool
    /// completion. A consumer stalled past [`FLUSH_DEADLINE`] forfeits that
    /// ordering instead of suspending turn termination indefinitely: the
    /// rows still queued are shed and counted, so the turn-boundary
    /// `replay_lossy` discloses the gap exactly like the push-path shed
    /// budgets.
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
        if tokio::time::timeout(FLUSH_DEADLINE, wait).await.is_ok() {
            return;
        }
        // Shed whatever the stalled consumer never picked up, restoring the
        // byte budgets the way the worker's pop path does. A row already
        // popped and parked inside the worker's channel send is out of reach
        // here; conservation (delivered + counted == accepted) still holds.
        let purged = {
            let mut queue = self.0.queue.lock().unwrap_or_else(|e| e.into_inner());
            let Queue {
                entries,
                text_bytes,
                event_bytes,
                ..
            } = &mut *queue;
            let mut purged = 0;
            for entry in entries.drain(..) {
                match entry {
                    Pending::Text(event) => {
                        *text_bytes = text_bytes.saturating_sub(text_size(&event));
                        purged += 1;
                    }
                    Pending::Critical { json } => {
                        *event_bytes = event_bytes.saturating_sub(json.len());
                        purged += 1;
                    }
                    Pending::Barrier(_) => {}
                }
            }
            purged
        };
        if purged > 0 {
            let previous = self.0.dropped.fetch_add(purged, Ordering::Relaxed);
            if previous == 0 || previous.is_power_of_two() {
                tracing::warn!(
                    dropped = previous + purged,
                    deadline_secs = FLUSH_DEADLINE.as_secs(),
                    "progress flush deadline expired; queued rows shed and counted"
                );
            }
        } else {
            tracing::warn!(
                deadline_secs = FLUSH_DEADLINE.as_secs(),
                "progress flush deadline expired; queue already drained"
            );
        }
        metrics::counter!(
            "ws.send.drop.backpressure",
            "method" => "progress_flush_deadline"
        )
        .increment(purged);
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
    async fn should_shed_excess_critical_events_and_count_them() {
        let (tx, mut rx) = mpsc::channel(1);
        let dropped = Arc::new(AtomicU64::new(0));
        let delivery = ProgressDelivery::new(tx, dropped.clone());
        // 64 × 128 KiB tool arguments ≈ 8 MiB of queued critical payload —
        // twice the budget. The consumer is stalled (channel(1), never read),
        // so everything lands in the queue exactly as on a dead WS client.
        let arguments = "x".repeat(128 * 1024);
        let pushed = 64;
        for i in 0..pushed {
            delivery.push(json!({
                "type": "tool_start",
                "tool": "write",
                "tool_call_id": format!("t{i}"),
                "arguments": {"body": arguments},
            }));
        }
        assert!(
            dropped.load(Ordering::Relaxed) > 0,
            "critical events beyond the byte budget must be shed"
        );
        let shed = dropped.load(Ordering::Relaxed) as usize;
        // Drain without closing, then verify the budget recovered: one
        // overflow must not wedge the queue into shedding everything that
        // follows (pins the pop-path accounting).
        let mut received = 0;
        while received + shed < pushed {
            let row: Value = serde_json::from_str(&rx.recv().await.unwrap()).unwrap();
            assert_eq!(row["type"], "tool_start");
            received += 1;
        }
        assert_eq!(
            received + shed,
            pushed,
            "shed accounting must conserve events"
        );
        delivery.push(json!({"type": "tool_end", "tool_call_id": "after"}));
        let row = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("post-overflow critical events must flow again")
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&row).unwrap()["type"],
            "tool_end"
        );
        drop(delivery);
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

    // Paused time only advances when every task is idle, so the stalled
    // worker (parked on a channel send no one will service) lets the clock
    // jump straight to the next timer — the test is instant and the large
    // outer guard fires before any real wait could.
    #[tokio::test(start_paused = true)]
    async fn should_return_from_flush_when_consumer_stalls_past_deadline() {
        let (tx, mut rx) = mpsc::channel(1);
        let dropped = Arc::new(AtomicU64::new(0));
        let delivery = ProgressDelivery::new(tx, dropped.clone());
        delivery.push(json!({"type":"tool_start","tool_call_id":"a"}));
        delivery.push(json!({"type":"tool_end","tool_call_id":"a"}));
        let flush = {
            let delivery = delivery.clone();
            tokio::spawn(async move { delivery.flush().await })
        };
        tokio::time::timeout(std::time::Duration::from_secs(600), flush)
            .await
            .expect("flush() must not suspend turn termination on a stalled consumer")
            .expect("flush task must not panic");
        // The consumer comes back after the stall: every accepted row is
        // either delivered or counted, never both, never neither.
        drop(delivery);
        let mut received = 0;
        while let Some(row) = rx.recv().await {
            assert!(row.contains("tool_"));
            received += 1;
        }
        assert_eq!(
            received as u64 + dropped.load(Ordering::Relaxed),
            2,
            "delivery and drop accounting must conserve accepted rows"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn should_count_rows_stuck_behind_expired_flush_deadline() {
        // Capacity 2 with four accepted rows: the worker fills both slots
        // (a, b) and parks on c, leaving d plus the barrier queued when the
        // deadline fires. Exactly d is purged; a/b deliver, c completes once
        // the consumer resumes.
        let (tx, mut rx) = mpsc::channel(2);
        let dropped = Arc::new(AtomicU64::new(0));
        let delivery = ProgressDelivery::new(tx, dropped.clone());
        for id in ["a", "b", "c", "d"] {
            delivery.push(json!({"type":"tool_start","tool_call_id":id}));
        }
        let flush = {
            let delivery = delivery.clone();
            tokio::spawn(async move { delivery.flush().await })
        };
        tokio::time::timeout(std::time::Duration::from_secs(600), flush)
            .await
            .expect("flush() must not suspend turn termination on a stalled consumer")
            .expect("flush task must not panic");
        assert_eq!(
            dropped.load(Ordering::Relaxed),
            1,
            "exactly the row still queued behind the parked send must be shed and counted"
        );
        drop(delivery);
        let mut received = Vec::new();
        while let Some(row) = rx.recv().await {
            let id = serde_json::from_str::<Value>(&row).unwrap()["tool_call_id"]
                .as_str()
                .unwrap()
                .to_owned();
            received.push(id);
            if received.len() == 3 {
                break;
            }
        }
        assert_eq!(received, ["a", "b", "c"]);
        assert!(rx.recv().await.is_none(), "d was shed at the deadline");
    }
}
