//! Heartbeat service that periodically checks HEARTBEAT.md for tasks.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::Utc;
use octos_core::InboundMessage;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

/// Default heartbeat interval: 30 minutes.
pub const DEFAULT_INTERVAL_SECS: u64 = 1800;

const HEARTBEAT_PROMPT: &str = "\
Read HEARTBEAT.md in your workspace (if it exists).\n\
Follow any instructions or tasks listed there.\n\
If nothing needs attention, reply with just: HEARTBEAT_OK";

/// Service that periodically reads HEARTBEAT.md and sends its content to the agent.
pub struct HeartbeatService {
    workspace_dir: PathBuf,
    inbound_tx: mpsc::Sender<InboundMessage>,
    interval_secs: u64,
    running: AtomicBool,
    timer_handle: tokio::sync::Mutex<Option<JoinHandle<()>>>,
}

impl HeartbeatService {
    pub fn new(
        workspace_dir: impl AsRef<Path>,
        inbound_tx: mpsc::Sender<InboundMessage>,
        interval_secs: u64,
    ) -> Self {
        Self {
            workspace_dir: workspace_dir.as_ref().to_path_buf(),
            inbound_tx,
            interval_secs,
            running: AtomicBool::new(false),
            timer_handle: tokio::sync::Mutex::new(None),
        }
    }

    /// Start the heartbeat loop.
    pub fn start(self: &Arc<Self>) {
        self.running.store(true, Ordering::Relaxed);
        let this = Arc::clone(self);

        let handle = tokio::spawn(async move {
            info!(
                interval_secs = this.interval_secs,
                "heartbeat service started"
            );
            while this.running.load(Ordering::Relaxed) {
                tokio::time::sleep(std::time::Duration::from_secs(this.interval_secs)).await;
                if !this.running.load(Ordering::Relaxed) {
                    break;
                }
                this.tick().await;
            }
        });

        let this2 = Arc::clone(self);
        tokio::spawn(async move {
            *this2.timer_handle.lock().await = Some(handle);
        });
    }

    /// Stop the heartbeat loop.
    pub async fn stop(&self) {
        self.running.store(false, Ordering::Relaxed);
        let mut handle = self.timer_handle.lock().await;
        if let Some(h) = handle.take() {
            h.abort();
        }
        info!("heartbeat service stopped");
    }

    /// Single heartbeat tick: read HEARTBEAT.md, send if non-empty.
    async fn tick(&self) {
        let path = self.workspace_dir.join("HEARTBEAT.md");
        let content = match tokio::fs::read_to_string(&path).await {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                warn!("failed to read HEARTBEAT.md: {e}");
                return;
            }
        };

        if is_heartbeat_empty(&content) {
            debug!("HEARTBEAT.md is empty, skipping");
            return;
        }

        let msg = InboundMessage {
            channel: "system".into(),
            sender_id: "heartbeat".into(),
            chat_id: "heartbeat".into(),
            content: format!("{HEARTBEAT_PROMPT}\n\n---\n\n{content}"),
            timestamp: Utc::now(),
            media: vec![],
            metadata: serde_json::json!({
                "deliver_to_channel": "cli",
                "deliver_to_chat_id": "heartbeat",
            }),
            message_id: None,
        };

        if let Err(e) = self.inbound_tx.send(msg).await {
            warn!("failed to send heartbeat message: {e}");
        }
    }
}

/// Check if HEARTBEAT.md content has no actionable items.
/// Empty means: blank lines, markdown headers, HTML comments, empty checkboxes only.
pub fn is_heartbeat_empty(content: &str) -> bool {
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with('#') {
            continue;
        }
        if trimmed.starts_with("<!--") {
            continue;
        }
        // Empty checkboxes: - [ ] or * [ ] (with optional whitespace after)
        if (trimmed.starts_with("- [ ]") || trimmed.starts_with("* [ ]")) && trimmed.len() <= 6 {
            continue;
        }
        // Checked checkboxes: - [x] or * [x]
        if (trimmed.starts_with("- [x]") || trimmed.starts_with("* [x]")) && trimmed.len() <= 6 {
            continue;
        }
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_heartbeat_empty_blank() {
        assert!(is_heartbeat_empty(""));
        assert!(is_heartbeat_empty("  \n\n  \n"));
    }

    #[test]
    fn test_is_heartbeat_empty_headers_only() {
        assert!(is_heartbeat_empty("# Heartbeat\n\n## Tasks\n"));
    }

    #[test]
    fn test_is_heartbeat_empty_comments_only() {
        assert!(is_heartbeat_empty("<!-- nothing here -->\n# Title\n"));
    }

    #[test]
    fn test_is_heartbeat_empty_checkboxes_only() {
        assert!(is_heartbeat_empty("- [ ]\n* [ ]\n- [x]\n"));
    }

    #[test]
    fn test_is_heartbeat_not_empty() {
        assert!(!is_heartbeat_empty("- [ ] Check the logs"));
        assert!(!is_heartbeat_empty("# Tasks\nDo something\n"));
        assert!(!is_heartbeat_empty("Run a backup"));
    }

    #[tokio::test]
    async fn test_heartbeat_sends_on_content() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("HEARTBEAT.md"), "- [ ] Check logs\n")
            .await
            .unwrap();

        let (tx, mut rx) = mpsc::channel(16);
        let service = Arc::new(HeartbeatService::new(dir.path(), tx, 3600));

        service.tick().await;

        let msg = rx.try_recv().unwrap();
        assert_eq!(msg.channel, "system");
        assert_eq!(msg.sender_id, "heartbeat");
        assert!(msg.content.contains("Check logs"));
    }

    #[tokio::test]
    async fn test_heartbeat_skips_empty() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("HEARTBEAT.md"), "# Heartbeat\n\n")
            .await
            .unwrap();

        let (tx, mut rx) = mpsc::channel(16);
        let service = Arc::new(HeartbeatService::new(dir.path(), tx, 3600));

        service.tick().await;

        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_heartbeat_skips_missing_file() {
        let dir = tempfile::tempdir().unwrap();

        let (tx, mut rx) = mpsc::channel(16);
        let service = Arc::new(HeartbeatService::new(dir.path(), tx, 3600));

        service.tick().await;

        assert!(rx.try_recv().is_err());
    }
}
