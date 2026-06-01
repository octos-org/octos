//! CLI channel — reads stdin, writes stdout. For local testing.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use chrono::Utc;
use eyre::Result;
use octos_core::{InboundMessage, OutboundMessage};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Notify, mpsc};

use crate::channel::Channel;

pub struct CliChannel {
    shutdown: Arc<AtomicBool>,
    shutdown_notify: Arc<Notify>,
}

impl CliChannel {
    pub fn new(shutdown: Arc<AtomicBool>) -> Self {
        Self::with_shutdown_notify(shutdown, Arc::new(Notify::new()))
    }

    pub fn with_shutdown_notify(shutdown: Arc<AtomicBool>, shutdown_notify: Arc<Notify>) -> Self {
        Self {
            shutdown,
            shutdown_notify,
        }
    }

    pub fn is_exit_command(input: &str) -> bool {
        matches!(
            input.trim().to_ascii_lowercase().as_str(),
            "quit" | "exit" | "/quit" | "/exit" | ":q"
        )
    }

    fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.shutdown_notify.notify_waiters();
    }
}

#[async_trait]
impl Channel for CliChannel {
    fn name(&self) -> &str {
        "cli"
    }

    async fn start(&self, inbound_tx: mpsc::Sender<InboundMessage>) -> Result<()> {
        let stdin = tokio::io::stdin();
        let mut reader = BufReader::new(stdin).lines();
        let mut stdout = tokio::io::stdout();

        stdout.write_all(b"octos gateway> ").await?;
        stdout.flush().await?;

        loop {
            let shutdown_notified = self.shutdown_notify.notified();
            tokio::pin!(shutdown_notified);

            if self.shutdown.load(Ordering::SeqCst) {
                break;
            }

            let line = tokio::select! {
                biased;
                _ = &mut shutdown_notified => break,
                line = reader.next_line() => line,
            };
            let Some(line) = line? else {
                break;
            };
            let trimmed = line.trim().to_string();

            if trimmed.is_empty() {
                stdout.write_all(b"octos gateway> ").await?;
                stdout.flush().await?;
                continue;
            }

            if Self::is_exit_command(&trimmed) {
                self.request_shutdown();
                break;
            }

            let msg = InboundMessage {
                channel: "cli".into(),
                sender_id: "local".into(),
                chat_id: "default".into(),
                content: trimmed,
                timestamp: Utc::now(),
                media: vec![],
                metadata: serde_json::json!({}),
                message_id: None,
            };

            if inbound_tx.send(msg).await.is_err() {
                break;
            }
        }

        Ok(())
    }

    async fn send(&self, msg: &OutboundMessage) -> Result<()> {
        let mut stdout = tokio::io::stdout();
        stdout.write_all(b"\n").await?;
        stdout.write_all(msg.content.as_bytes()).await?;
        stdout.write_all(b"\n\noctos gateway> ").await?;
        stdout.flush().await?;
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.request_shutdown();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_gateway_exit_commands() {
        for cmd in ["quit", "exit", "/quit", "/exit", ":q", " QUIT "] {
            assert!(CliChannel::is_exit_command(cmd), "{cmd} should exit");
        }

        for cmd in ["", "quit now", "/sessions", "please exit"] {
            assert!(!CliChannel::is_exit_command(cmd), "{cmd} should not exit");
        }
    }
}
