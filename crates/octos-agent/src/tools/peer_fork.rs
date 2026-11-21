//! `peer_fork` — branch a native peer into a NEW identity (octos#2026).
//!
//! A fork inherits an EXPLICIT, narrow scope: the source's brief (the task
//! contract), its workspace binding, and the same originator (person
//! ownership never changes hands). It does NOT inherit the source's
//! in-flight work or authority — no closed marker, no parked rows, no
//! task-id binding, no result history — so the fork can never execute the
//! source's pending tasks or consume its approvals. The source is left
//! completely untouched.
//!
//! Guard rails mirror `peer_close`:
//! - Depth-1: registered only where `peer_handoff` is.
//! - Authorization: only the source peer's recorded ORIGINATOR may fork it.
//! - Host-owned app peers are refused.
//! - The tool carries no blackboard knowledge; the host callback owns the
//!   originator check, the fresh reservation and the scoped copy.

use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;

use super::{Tool, ToolResult};

/// Host callback that forks a peer. Arguments: the trimmed identifier (name
/// or slug) and an optional new name; returns `(new_slug, confirmation)` or
/// an error string.
pub type PeerForkCallback =
    Arc<dyn Fn(String, Option<String>) -> Result<(String, String), String> + Send + Sync>;

/// `peer_fork` tool. See the module docs for the fork semantics.
pub struct PeerForkTool {
    fork: PeerForkCallback,
}

impl PeerForkTool {
    /// Build the tool around the host's fork callback.
    pub fn new(fork: PeerForkCallback) -> Self {
        Self { fork }
    }
}

#[derive(Debug, Deserialize)]
struct Input {
    slug: String,
    #[serde(default)]
    name: Option<String>,
}

#[async_trait]
impl Tool for PeerForkTool {
    fn name(&self) -> &str {
        "peer_fork"
    }

    fn description(&self) -> &str {
        "Fork a peer YOU created into a NEW peer identity. The fork inherits \
         the source's brief and workspace binding under the same owner, but \
         NONE of its in-flight tasks, approvals or result history. Use it to \
         branch a fresh collaborator from an existing task contract; drive \
         the new peer with peer_send_input. Only the source peer's \
         originator may fork it."
    }

    fn tags(&self) -> &[&str] {
        &["gateway"]
    }

    fn concurrency_class(&self) -> super::ConcurrencyClass {
        // Mutating: reserves a fresh peer dir and writes its records —
        // serialized like peer_handoff / peer_close.
        super::ConcurrencyClass::Exclusive
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["slug"],
            "properties": {
                "slug": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Source peer NAME (or slug) to fork."
                },
                "name": {
                    "type": "string",
                    "description": "Optional name for the new peer (defaults to '<source>-fork')."
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
                        "invalid peer_fork arguments: {e}. \
                         Required: {{\"slug\": string, \"name\"?: string}}"
                    ),
                    success: false,
                    ..Default::default()
                });
            }
        };
        let ident = input.slug.trim();
        if ident.is_empty() {
            return Ok(ToolResult {
                output: "peer_fork requires a non-empty source peer name or slug".to_owned(),
                success: false,
                ..Default::default()
            });
        }
        match (self.fork)(ident.to_owned(), input.name) {
            Ok((new_slug, message)) => Ok(ToolResult {
                output: format!("{message}\nnew peer slug: {new_slug}"),
                success: true,
                ..Default::default()
            }),
            Err(message) => Ok(ToolResult {
                output: message,
                success: false,
                ..Default::default()
            }),
        }
    }
}
