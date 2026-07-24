//! `peer_send_input` — master→peer cross-session TurnStart injection (#436).
//!
//! When the master's LLM needs to give a follow-up instruction to a RUNNING
//! peer session, this tool pushes an InboundMessage into the peer's
//! session-actor inbox via a server-side channel. No TUI round-trip required.
//!
//! The host callback (wired during turn construction in the serve/WS path)
//! holds a reference to the global peer inbox registry; the tool itself
//! carries no IPC knowledge beyond calling that callback.
//!
//! Guard rails:
//! - Depth-1: the tool is never registered on peer sessions themselves (same
//!   guard as `peer_handoff`).
//! - Max message size: PEER_SEND_INPUT_MAX_BYTES (64 KB).
//! - Returns a clear error when the peer session is not running.

use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;

use super::{Tool, ToolResult};

/// Hard cap on the message payload (bytes).
pub const PEER_SEND_INPUT_MAX_BYTES: usize = 64 * 1024;

/// Facts the host callback needs to locate the peer session.
#[derive(Debug, Clone)]
pub struct PeerSendInputRequest {
    /// The peer directory slug (as reported by peer_handoff / peer_list).
    pub slug: String,
    /// The message to inject as a new turn into the peer session.
    pub message: String,
}

/// Host callback that delivers a message to a running peer session's inbox.
pub type PeerSendInputCallback =
    Arc<dyn Fn(PeerSendInputRequest) -> Result<(), String> + Send + Sync>;

/// `peer_send_input` tool. See the module docs for the cross-session channel.
pub struct PeerSendInputTool {
    send_input: PeerSendInputCallback,
}

impl PeerSendInputTool {
    pub fn new(send_input: PeerSendInputCallback) -> Self {
        Self { send_input }
    }
}

#[derive(Debug, Deserialize)]
struct Input {
    slug: String,
    message: String,
}

#[async_trait]
impl Tool for PeerSendInputTool {
    fn name(&self) -> &str {
        "peer_send_input"
    }

    fn description(&self) -> &str {
        "Send a follow-up message to a RUNNING peer session identified by slug \
         (as reported by peer_handoff). The peer receives it as its next turn. \
         Use when a deployed peer needs steering, additional context, or a \
         correction — but only when the peer was staged earlier in THIS \
         conversation or the user confirms the slug. The peer MUST be running \
         (the user opened the staged session); if it has completed or is idle \
         this will fail with an error."
    }

    fn tags(&self) -> &[&str] {
        &["gateway"]
    }

    fn concurrency_class(&self) -> super::ConcurrencyClass {
        super::ConcurrencyClass::Safe
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["slug", "message"],
            "properties": {
                "slug": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Slug of the peer session to send to (as reported by peer_handoff or peer_list)."
                },
                "message": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": PEER_SEND_INPUT_MAX_BYTES,
                    "description": "The message to inject as the peer's next turn. Include all needed context — the peer cannot see this conversation."
                }
            }
        })
    }

    async fn execute(&self, args: &Value) -> Result<ToolResult> {
        let input: Input = match serde_json::from_value(args.clone()) {
            Ok(i) => i,
            Err(e) => {
                return Ok(ToolResult {
                    output: format!(
                        "invalid peer_send_input arguments: {e}. \
                         Required: {{\"slug\": string, \"message\": string}}"
                    ),
                    success: false,
                    ..Default::default()
                });
            }
        };

        let slug = input.slug.trim();
        let message = input.message.trim();

        if slug.is_empty() {
            return Ok(ToolResult {
                output: "peer_send_input requires a non-empty slug".to_string(),
                success: false,
                ..Default::default()
            });
        }

        if message.is_empty() {
            return Ok(ToolResult {
                output: "peer_send_input requires a non-empty message".to_string(),
                success: false,
                ..Default::default()
            });
        }

        if message.len() > PEER_SEND_INPUT_MAX_BYTES {
            return Ok(ToolResult {
                output: format!(
                    "message exceeds {PEER_SEND_INPUT_MAX_BYTES} bytes — \
                     keep the directive concise, include context in the workspace"
                ),
                success: false,
                ..Default::default()
            });
        }

        let request = PeerSendInputRequest {
            slug: slug.to_string(),
            message: message.to_string(),
        };

        match (self.send_input)(request) {
            Ok(()) => Ok(ToolResult {
                output: format!(
                    "message sent to peer {slug} — \
                     the peer will process it as its next turn"
                ),
                success: true,
                ..Default::default()
            }),
            Err(e) => Ok(ToolResult {
                output: format!("failed to send to peer {slug}: {e}"),
                success: false,
                ..Default::default()
            }),
        }
    }
}
