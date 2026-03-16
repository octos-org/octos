//! Message tool for cross-channel messaging.

use async_trait::async_trait;
use octos_core::OutboundMessage;
use eyre::{Result, WrapErr};
use serde::Deserialize;
use tokio::sync::mpsc;

use super::{Tool, ToolResult};

/// Tool that sends messages to channels from the agent.
pub struct MessageTool {
    out_tx: mpsc::Sender<OutboundMessage>,
    default_channel: std::sync::Mutex<String>,
    default_chat_id: std::sync::Mutex<String>,
}

impl MessageTool {
    pub fn new(out_tx: mpsc::Sender<OutboundMessage>) -> Self {
        Self {
            out_tx,
            default_channel: std::sync::Mutex::new(String::new()),
            default_chat_id: std::sync::Mutex::new(String::new()),
        }
    }

    /// Create a new MessageTool with context pre-set (for per-session instances).
    pub fn with_context(
        out_tx: mpsc::Sender<OutboundMessage>,
        channel: impl Into<String>,
        chat_id: impl Into<String>,
    ) -> Self {
        Self {
            out_tx,
            default_channel: std::sync::Mutex::new(channel.into()),
            default_chat_id: std::sync::Mutex::new(chat_id.into()),
        }
    }

    /// Update the default channel/chat_id context (called per inbound message).
    /// WARNING: This mutates shared state. When using a shared Arc<MessageTool> across
    /// concurrent sessions, a race condition exists between set_context() and tool
    /// execution. Prefer with_context() for per-session instances when possible.
    pub fn set_context(&self, channel: &str, chat_id: &str) {
        *self
            .default_channel
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = channel.to_string();
        *self
            .default_chat_id
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = chat_id.to_string();
    }
}

#[derive(Deserialize)]
struct Input {
    content: String,
    #[serde(default)]
    channel: Option<String>,
    #[serde(default)]
    chat_id: Option<String>,
    #[serde(default)]
    metadata: Option<serde_json::Value>,
}

#[async_trait]
impl Tool for MessageTool {
    fn name(&self) -> &str {
        "message"
    }

    fn description(&self) -> &str {
        "Send a message to a channel. If channel/chat_id are omitted, sends to the current conversation."
    }

    fn tags(&self) -> &[&str] {
        &["gateway"]
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "content": {
                    "type": "string",
                    "description": "The message content to send"
                },
                "channel": {
                    "type": "string",
                    "description": "Target channel (e.g. 'telegram', 'slack'). Defaults to current."
                },
                "chat_id": {
                    "type": "string",
                    "description": "Target chat/user ID. Defaults to current."
                },
                "metadata": {
                    "type": "object",
                    "description": "Optional metadata. Use 'inline_keyboard' for Telegram button menus: {\"inline_keyboard\": [[{\"text\": \"Label\", \"callback_data\": \"value\"}]]}. Each inner array is a row of buttons."
                }
            },
            "required": ["content"]
        })
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let input: Input =
            serde_json::from_value(args.clone()).wrap_err("invalid message tool input")?;

        let channel = input.channel.unwrap_or_else(|| {
            self.default_channel
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        });
        let chat_id = input.chat_id.unwrap_or_else(|| {
            self.default_chat_id
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        });

        if channel.is_empty() || chat_id.is_empty() {
            return Ok(ToolResult {
                output: "Error: No target channel/chat specified.".into(),
                success: false,
                ..Default::default()
            });
        }

        // Suppress low-value "thinking/processing" acknowledgment messages
        let normalized = input.content.trim().to_lowercase();
        if is_thinking_filler(&normalized) {
            return Ok(ToolResult {
                output: "Skipped: don't send thinking/processing messages.".into(),
                success: true,
                ..Default::default()
            });
        }

        let msg = OutboundMessage {
            channel: channel.clone(),
            chat_id: chat_id.clone(),
            content: input.content,
            reply_to: None,
            media: vec![],
            metadata: input.metadata.unwrap_or_else(|| serde_json::json!({})),
        };

        self.out_tx
            .send(msg)
            .await
            .map_err(|e| eyre::eyre!("failed to send message: {e}"))?;

        Ok(ToolResult {
            output: format!("Message sent to {channel}:{chat_id}"),
            success: true,
            ..Default::default()
        })
    }
}

/// Check if a message is just a low-value "thinking/processing" filler.
fn is_thinking_filler(s: &str) -> bool {
    const FILLER_PATTERNS: &[&str] = &[
        "thinking",
        "thinking...",
        "thinking…",
        "let me think",
        "processing",
        "processing...",
        "processing…",
        "working on it",
        "请稍等",
        "稍等",
        "思考中",
        "正在思考",
        "正在处理",
        "让我想想",
        "让我看看",
    ];
    let clean = s.trim_matches(|c: char| !c.is_alphanumeric() && c != '.' && c != '…');
    FILLER_PATTERNS.contains(&clean)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_send_with_explicit_target() {
        let (tx, mut rx) = mpsc::channel(16);
        let tool = MessageTool::new(tx);

        let result = tool
            .execute(&serde_json::json!({
                "content": "hello",
                "channel": "telegram",
                "chat_id": "123"
            }))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("telegram:123"));

        let msg = rx.recv().await.unwrap();
        assert_eq!(msg.content, "hello");
        assert_eq!(msg.channel, "telegram");
        assert_eq!(msg.chat_id, "123");
    }

    #[tokio::test]
    async fn test_send_with_defaults() {
        let (tx, mut rx) = mpsc::channel(16);
        let tool = MessageTool::new(tx);
        tool.set_context("discord", "456");

        let result = tool
            .execute(&serde_json::json!({"content": "hi there"}))
            .await
            .unwrap();

        assert!(result.success);
        let msg = rx.recv().await.unwrap();
        assert_eq!(msg.channel, "discord");
        assert_eq!(msg.chat_id, "456");
    }

    #[tokio::test]
    async fn test_with_context_routes_correctly() {
        let (tx, mut rx) = mpsc::channel(16);
        let tool = MessageTool::with_context(tx, "telegram", "ctx-chat");

        let result = tool
            .execute(&serde_json::json!({"content": "via with_context"}))
            .await
            .unwrap();

        assert!(result.success);
        let msg = rx.recv().await.unwrap();
        assert_eq!(msg.channel, "telegram");
        assert_eq!(msg.chat_id, "ctx-chat");
        assert_eq!(msg.content, "via with_context");
    }

    #[tokio::test]
    async fn test_two_tools_different_context() {
        let (tx1, mut rx1) = mpsc::channel(16);
        let (tx2, mut rx2) = mpsc::channel(16);
        let tool_a = MessageTool::with_context(tx1, "telegram", "alice");
        let tool_b = MessageTool::with_context(tx2, "telegram", "bob");

        tool_a
            .execute(&serde_json::json!({"content": "for alice"}))
            .await
            .unwrap();
        tool_b
            .execute(&serde_json::json!({"content": "for bob"}))
            .await
            .unwrap();

        let msg_a = rx1.recv().await.unwrap();
        let msg_b = rx2.recv().await.unwrap();
        assert_eq!(msg_a.chat_id, "alice");
        assert_eq!(msg_b.chat_id, "bob");
    }

    #[tokio::test]
    async fn test_missing_target() {
        let (tx, _rx) = mpsc::channel(16);
        let tool = MessageTool::new(tx);

        let result = tool
            .execute(&serde_json::json!({"content": "orphan"}))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.output.contains("No target"));
    }
}
