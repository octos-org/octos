//! CLI channel — reads stdin, writes stdout. For local testing.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use chrono::Utc;
use octos_core::{InboundMessage, OutboundMessage};
use eyre::Result;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use crate::channel::Channel;

pub struct CliChannel {
    shutdown: Arc<AtomicBool>,
}

impl CliChannel {
    pub fn new(shutdown: Arc<AtomicBool>) -> Self {
        Self { shutdown }
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

        while let Ok(Some(line)) = reader.next_line().await {
            let trimmed = line.trim().to_string();

            if trimmed.is_empty() {
                stdout.write_all(b"octos gateway> ").await?;
                stdout.flush().await?;
                continue;
            }

            if trimmed == "/quit" || trimmed == "/exit" {
                self.shutdown.store(true, Ordering::SeqCst);
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
        self.shutdown.store(true, Ordering::SeqCst);
        Ok(())
    }
}
