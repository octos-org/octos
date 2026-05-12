//! Black-box flight-data recorder for post-incident analysis.
//!
//! Writes JSONL (one JSON object per line) to a file via a bounded async channel.
//! If the channel is full, new entries are dropped (never blocks the agent loop).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

/// A single record entry written to the JSONL log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordEntry {
    /// Monotonic timestamp in milliseconds.
    pub timestamp_ms: u64,
    /// Event category.
    pub event: String,
    /// Payload data.
    pub data: serde_json::Value,
}

impl RecordEntry {
    pub fn new(event: impl Into<String>, data: serde_json::Value) -> Self {
        let timestamp_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        Self {
            timestamp_ms,
            event: event.into(),
            data,
        }
    }
}

/// Async JSONL recorder with a bounded channel.
///
/// The recorder spawns a background task that reads from a channel and writes
/// each entry as a single JSON line to the output file. If the channel is full,
/// `record()` drops the entry silently (non-blocking for the agent loop).
pub struct BlackBoxRecorder {
    sender: mpsc::Sender<RecordEntry>,
}

impl BlackBoxRecorder {
    /// Create a new recorder writing to the given path.
    ///
    /// `buffer_size` controls the bounded channel capacity (default: 1024).
    pub async fn new(path: PathBuf, buffer_size: usize) -> eyre::Result<Self> {
        let (tx, mut rx) = mpsc::channel::<RecordEntry>(buffer_size);

        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        let mut writer = tokio::io::BufWriter::new(file);

        tokio::spawn(async move {
            while let Some(entry) = rx.recv().await {
                if let Ok(line) = serde_json::to_string(&entry) {
                    let _ = writer.write_all(line.as_bytes()).await;
                    let _ = writer.write_all(b"\n").await;
                    let _ = writer.flush().await;
                }
            }
        });

        Ok(Self { sender: tx })
    }

    /// Record an entry. Returns true if queued, false if dropped (channel full).
    pub fn record(&self, entry: RecordEntry) -> bool {
        self.sender.try_send(entry).is_ok()
    }

    /// Record a simple event with JSON data.
    pub fn log(&self, event: &str, data: serde_json::Value) -> bool {
        self.record(RecordEntry::new(event, data))
    }

    /// Check if the recorder is still connected (background task alive).
    pub fn is_active(&self) -> bool {
        !self.sender.is_closed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[tokio::test]
    async fn should_write_jsonl_entries() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();

        let recorder = BlackBoxRecorder::new(path.clone(), 1024).await.unwrap();

        recorder.log(
            "llm_call",
            serde_json::json!({"model": "test", "tokens": 100}),
        );
        recorder.log("tool_call", serde_json::json!({"tool": "read_file"}));
        recorder.log("safety_check", serde_json::json!({"tier": "observe"}));

        // Drop to close the sender, causing the background task to drain and finish.
        drop(recorder);

        // Give the background task time to flush.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let contents = tokio::fs::read_to_string(&path).await.unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 3, "expected 3 JSONL lines, got: {lines:?}");

        for line in &lines {
            let entry: serde_json::Value = serde_json::from_str(line).unwrap();
            assert!(entry.get("timestamp_ms").is_some());
            assert!(entry.get("event").is_some());
            assert!(entry.get("data").is_some());
        }
    }

    #[tokio::test]
    async fn should_drop_on_full_channel() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();

        // Buffer of 1 — almost all rapid-fire sends will be dropped.
        let recorder = BlackBoxRecorder::new(path, 1).await.unwrap();

        // Send many entries without awaiting; some must be dropped, none must panic.
        for i in 0..100 {
            recorder.log("event", serde_json::json!({"i": i}));
        }

        // No panic means the test passes.
        drop(recorder);
    }

    #[tokio::test]
    async fn should_flush_on_drop() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();

        let recorder = BlackBoxRecorder::new(path.clone(), 1024).await.unwrap();
        recorder.log("startup", serde_json::json!({"status": "ok"}));
        drop(recorder);

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let meta = tokio::fs::metadata(&path).await.unwrap();
        assert!(meta.len() > 0, "file should not be empty after drop");
    }

    #[tokio::test]
    async fn should_be_active_initially() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();

        let recorder = BlackBoxRecorder::new(path, 1024).await.unwrap();
        assert!(
            recorder.is_active(),
            "recorder should be active after creation"
        );
    }
}
