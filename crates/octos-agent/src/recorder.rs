//! Black-box flight-data recorder for post-incident analysis.
//!
//! Writes JSONL (one JSON object per line) to a file via a bounded async
//! channel. The recorder spawns a single background task that drains the
//! channel; the agent loop never blocks on disk I/O. If the channel is
//! full, `log()` drops the new entry and bumps a counter exposed via
//! [`BlackBoxRecorder::status`].
//!
//! ## Replay correctness
//!
//! Each entry carries a writer-assigned monotonic `seq` plus an
//! `elapsed_ms` derived from `Instant::elapsed` since the recorder was
//! opened. Both are monotonic; wall-clock skew or NTP jumps never reorder
//! the log. A separate `unix_ms` field is stamped for human readability
//! but is **not** the ordering key.
//!
//! ## Shutdown semantics
//!
//! Two patterns are supported:
//!
//! * **Fire-and-forget**: `drop(recorder)` closes the channel; the writer
//!   task drains pending entries and flushes on its own schedule.
//!   Suitable for happy-path teardown.
//! * **Guaranteed drain**: call [`BlackBoxRecorder::shutdown`].await for
//!   deterministic flush before the recorder goes out of scope. This is
//!   the path session/agent shutdown must use to avoid losing the tail
//!   of the flight log on crash.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::warn;

/// A single record entry written to the JSONL log.
///
/// `seq` and `elapsed_ms` are monotonic and define replay order. `unix_ms`
/// is wall-clock only and may jump under NTP correction — do not order by
/// it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordEntry {
    /// Writer-assigned monotonic sequence (never decreases).
    pub seq: u64,
    /// Milliseconds since recorder open (monotonic, from `Instant`).
    pub elapsed_ms: u64,
    /// Unix-epoch wall clock for human readability. Not used for ordering.
    pub unix_ms: u64,
    /// Event category.
    pub event: String,
    /// Payload data.
    pub data: serde_json::Value,
}

/// Snapshot of recorder health, returned by [`BlackBoxRecorder::status`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecorderStatus {
    /// True if both the channel is open and the writer task is alive
    /// (no panic, no terminal write failure observed yet).
    pub alive: bool,
    /// Cumulative count of disk write / serialize failures.
    pub write_errors: u64,
    /// Cumulative count of entries dropped because the channel was full.
    pub dropped_events: u64,
}

/// Shared internal health counters. Updated from the writer task and the
/// `log()` fast path; read via [`BlackBoxRecorder::status`].
struct Health {
    write_errors: AtomicU64,
    dropped_events: AtomicU64,
    writer_alive: AtomicBool,
}

impl Health {
    fn new() -> Self {
        Self {
            write_errors: AtomicU64::new(0),
            dropped_events: AtomicU64::new(0),
            writer_alive: AtomicBool::new(true),
        }
    }
}

/// Default channel capacity if the caller doesn't specify one.
pub const DEFAULT_BUFFER_SIZE: usize = 1024;

/// Writer flushes the BufWriter every N entries. Smaller = more durable
/// under crash; larger = less syscall overhead. The `shutdown()` path
/// always flushes regardless of how many entries are pending.
const FLUSH_EVERY_N_ENTRIES: usize = 32;

/// Async JSONL recorder with a bounded channel.
///
/// See the module docs for replay-ordering and shutdown semantics.
pub struct BlackBoxRecorder {
    /// `Some` while the recorder is live; `None` after `shutdown()` takes it.
    /// std::sync::Mutex keeps `log()` non-async; the critical section is just
    /// a `try_send` so contention is negligible at recorder rates.
    sender: std::sync::Mutex<Option<mpsc::Sender<RecordEntry>>>,
    start: Instant,
    seq: AtomicU64,
    health: Arc<Health>,
    /// Background writer task. `take()`n by `shutdown()` to await drain.
    handle: tokio::sync::Mutex<Option<JoinHandle<()>>>,
}

impl BlackBoxRecorder {
    /// Open a recorder writing to `path`. Creates parent directories and
    /// the file if missing; appends if the file already exists.
    pub async fn open(path: PathBuf, buffer_size: usize) -> eyre::Result<Self> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let (tx, mut rx) = mpsc::channel::<RecordEntry>(buffer_size);
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        let mut writer = tokio::io::BufWriter::new(file);
        let health = Arc::new(Health::new());
        let health_for_task = Arc::clone(&health);

        let handle = tokio::spawn(async move {
            let mut since_flush: usize = 0;
            while let Some(entry) = rx.recv().await {
                let line = match serde_json::to_string(&entry) {
                    Ok(s) => s,
                    Err(e) => {
                        record_serialize_error(&health_for_task, &e);
                        continue;
                    }
                };
                if let Err(e) = writer.write_all(line.as_bytes()).await {
                    record_io_error(&health_for_task, &e);
                    continue;
                }
                if let Err(e) = writer.write_all(b"\n").await {
                    record_io_error(&health_for_task, &e);
                    continue;
                }
                since_flush += 1;
                if since_flush >= FLUSH_EVERY_N_ENTRIES {
                    if let Err(e) = writer.flush().await {
                        record_io_error(&health_for_task, &e);
                    }
                    since_flush = 0;
                }
            }
            // Channel closed — flush whatever is buffered so the tail is
            // durable. Even if this fails we mark writer_alive=false so
            // `status()` reflects reality.
            if let Err(e) = writer.flush().await {
                record_io_error(&health_for_task, &e);
            }
            health_for_task.writer_alive.store(false, Ordering::Release);
        });

        Ok(Self {
            sender: std::sync::Mutex::new(Some(tx)),
            start: Instant::now(),
            seq: AtomicU64::new(0),
            health,
            handle: tokio::sync::Mutex::new(Some(handle)),
        })
    }

    /// Stamp a fresh entry and try to enqueue it.
    ///
    /// Returns `true` if queued, `false` if dropped (channel full OR
    /// shutdown already called). Dropped entries bump
    /// `status().dropped_events`.
    pub fn log(&self, event: &str, data: serde_json::Value) -> bool {
        let entry = RecordEntry {
            seq: self.seq.fetch_add(1, Ordering::Relaxed),
            elapsed_ms: self.start.elapsed().as_millis() as u64,
            unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or_default(),
            event: event.to_string(),
            data,
        };
        let guard = self
            .sender
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match guard.as_ref() {
            Some(sender) => match sender.try_send(entry) {
                Ok(()) => true,
                Err(_) => {
                    self.health.dropped_events.fetch_add(1, Ordering::Relaxed);
                    false
                }
            },
            None => {
                // Shutdown already taken the sender.
                self.health.dropped_events.fetch_add(1, Ordering::Relaxed);
                false
            }
        }
    }

    /// Snapshot of recorder health.
    pub fn status(&self) -> RecorderStatus {
        let channel_open = self
            .sender
            .lock()
            .map(|g| g.is_some() && g.as_ref().is_some_and(|s| !s.is_closed()))
            .unwrap_or(false);
        RecorderStatus {
            alive: channel_open && self.health.writer_alive.load(Ordering::Acquire),
            write_errors: self.health.write_errors.load(Ordering::Relaxed),
            dropped_events: self.health.dropped_events.load(Ordering::Relaxed),
        }
    }

    /// True if both the channel and the writer task are alive. Convenience
    /// wrapper over `status().alive`.
    pub fn is_active(&self) -> bool {
        self.status().alive
    }

    /// Gracefully drain and flush. Closes the channel, awaits the writer
    /// task, and returns once the JSONL file has been flushed.
    ///
    /// Idempotent — calling more than once is a no-op after the first
    /// call returns. Call this from session/agent shutdown for a
    /// deterministic flush; relying on `Drop` alone may lose the tail
    /// of the log if the tokio runtime is torn down before the writer
    /// task finishes.
    pub async fn shutdown(&self) -> eyre::Result<()> {
        // Close the channel by dropping the Sender. Once dropped, the
        // writer's `rx.recv()` returns None and the task exits.
        {
            let mut guard = self
                .sender
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard.take();
        }
        let handle = self.handle.lock().await.take();
        if let Some(h) = handle {
            // `JoinHandle::await` only fails on panic in the writer.
            h.await?;
        }
        Ok(())
    }
}

fn record_serialize_error(health: &Arc<Health>, e: &serde_json::Error) {
    let prev = health.write_errors.fetch_add(1, Ordering::Relaxed);
    if prev == 0 {
        warn!(error = %e, "BlackBoxRecorder: failed to serialize first entry (subsequent serialize errors counted but silenced; see status())");
    }
}

fn record_io_error(health: &Arc<Health>, e: &std::io::Error) {
    let prev = health.write_errors.fetch_add(1, Ordering::Relaxed);
    if prev == 0 {
        warn!(error = %e, "BlackBoxRecorder: writer hit first I/O error (subsequent I/O errors counted but silenced; see status())");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[tokio::test]
    async fn should_write_jsonl_entries_in_seq_order() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();

        let recorder = BlackBoxRecorder::open(path.clone(), 1024).await.unwrap();
        recorder.log("llm_call", serde_json::json!({"model": "test"}));
        recorder.log("tool_call", serde_json::json!({"tool": "read_file"}));
        recorder.log("safety_check", serde_json::json!({"tier": "observe"}));

        recorder.shutdown().await.unwrap();

        let contents = tokio::fs::read_to_string(&path).await.unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 3, "expected 3 JSONL lines, got: {lines:?}");

        let mut last_seq: i64 = -1;
        let mut last_elapsed: u64 = 0;
        for (i, line) in lines.iter().enumerate() {
            let entry: RecordEntry = serde_json::from_str(line).unwrap();
            assert!(
                (entry.seq as i64) > last_seq,
                "seq must be strictly increasing; line {i} got {} after {last_seq}",
                entry.seq
            );
            assert!(
                entry.elapsed_ms >= last_elapsed,
                "elapsed_ms must be non-decreasing; line {i} got {} after {last_elapsed}",
                entry.elapsed_ms
            );
            assert!(entry.unix_ms > 0, "unix_ms should be stamped");
            assert!(!entry.event.is_empty());
            last_seq = entry.seq as i64;
            last_elapsed = entry.elapsed_ms;
        }
    }

    #[tokio::test]
    async fn should_count_dropped_when_full() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();

        // Capacity of 1 — most rapid-fire sends must overflow.
        let recorder = BlackBoxRecorder::open(path, 1).await.unwrap();

        for i in 0..1000 {
            recorder.log("event", serde_json::json!({"i": i}));
        }

        let status = recorder.status();
        assert!(
            status.dropped_events > 0,
            "expected some drops with capacity 1 and 1000 rapid sends; got {status:?}"
        );

        recorder.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_flushes_deterministically() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();

        let recorder = BlackBoxRecorder::open(path.clone(), 1024).await.unwrap();
        recorder.log("startup", serde_json::json!({"status": "ok"}));

        // shutdown awaits the writer — no sleep needed.
        recorder.shutdown().await.unwrap();

        let contents = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(
            !contents.is_empty(),
            "shutdown should have flushed the entry"
        );
        let line = contents.lines().next().expect("at least one line");
        let entry: RecordEntry = serde_json::from_str(line).unwrap();
        assert_eq!(entry.event, "startup");
        assert_eq!(entry.seq, 0);
    }

    #[tokio::test]
    async fn shutdown_is_idempotent() {
        let tmp = NamedTempFile::new().unwrap();
        let recorder = BlackBoxRecorder::open(tmp.path().to_path_buf(), 1024)
            .await
            .unwrap();
        recorder.shutdown().await.unwrap();
        // Second call is a no-op, must not error or panic.
        recorder.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn log_after_shutdown_counts_as_dropped() {
        let tmp = NamedTempFile::new().unwrap();
        let recorder = BlackBoxRecorder::open(tmp.path().to_path_buf(), 1024)
            .await
            .unwrap();
        recorder.shutdown().await.unwrap();
        assert!(!recorder.log("after", serde_json::json!({})));
        assert!(recorder.status().dropped_events >= 1);
    }

    #[tokio::test]
    async fn should_be_active_initially() {
        let tmp = NamedTempFile::new().unwrap();
        let recorder = BlackBoxRecorder::open(tmp.path().to_path_buf(), 1024)
            .await
            .unwrap();
        assert!(recorder.is_active());
        let status = recorder.status();
        assert!(status.alive);
        assert_eq!(status.write_errors, 0);
        assert_eq!(status.dropped_events, 0);
        recorder.shutdown().await.unwrap();
        assert!(!recorder.is_active());
    }

    #[tokio::test]
    async fn open_creates_parent_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a/b/c/flight.jsonl");
        let recorder = BlackBoxRecorder::open(path.clone(), 1024).await.unwrap();
        recorder.log("hello", serde_json::json!({}));
        recorder.shutdown().await.unwrap();
        assert!(path.exists());
    }
}
