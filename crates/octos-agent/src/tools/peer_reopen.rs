//! `peer_reopen` — resume a CLOSED native peer you created (octos#2026).
//!
//! The lifecycle complement to `peer_close`: closing retires a peer but keeps
//! its durable directory (briefs, dated result versions, parked rows,
//! originator, workspace binding). Reopening clears ONLY the close marker
//! under the originator's authority, so the SAME peer/session identity
//! becomes addressable again with its unfinished work preserved and no
//! replay of already-committed results. A peer that is not closed is
//! refused (the close marker is the single lifecycle authority, so
//! crash/restart and the live view always agree).
//!
//! Guard rails mirror `peer_close`:
//! - Depth-1: registered only where `peer_handoff` is (never on peer
//!   sessions themselves).
//! - Authorization: only the peer's recorded ORIGINATOR may reopen it.
//! - Host-owned app peers are refused: their app owns the lifetime.
//! - The tool carries no blackboard knowledge; the host callback owns the
//!   originator check and the marker removal.

use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;

use super::{Tool, ToolResult};

/// Host callback that reopens a closed peer. The argument is the trimmed
/// identifier (peer name or slug); the host authorizes (originator-only) and
/// clears the durable close marker, returning a confirmation or an error.
pub type PeerReopenCallback = Arc<dyn Fn(String) -> Result<String, String> + Send + Sync>;

/// `peer_reopen` tool. See the module docs for the reopen semantics.
pub struct PeerReopenTool {
    reopen: PeerReopenCallback,
}

impl PeerReopenTool {
    /// Build the tool around the host's reopen callback.
    pub fn new(reopen: PeerReopenCallback) -> Self {
        Self { reopen }
    }
}

#[derive(Debug, Deserialize)]
struct Input {
    slug: String,
}

#[async_trait]
impl Tool for PeerReopenTool {
    fn name(&self) -> &str {
        "peer_reopen"
    }

    fn description(&self) -> &str {
        "Reopen a CLOSED peer YOU created, identified by its NAME (or slug), \
         resuming it with its briefs, results and unfinished work preserved. \
         Only the peer's originator may reopen it, and only when it is \
         currently closed. The peer's identity (session topic) is unchanged; \
         already-committed results are NOT replayed — drive the peer forward \
         with peer_send_input."
    }

    fn tags(&self) -> &[&str] {
        &["gateway"]
    }

    fn concurrency_class(&self) -> super::ConcurrencyClass {
        // Mutating: clears the durable close marker — serialized like
        // peer_close.
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
                    "description": "Peer NAME (or slug) to reopen, as reported by peer_list (closed peers)."
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
                        "invalid peer_reopen arguments: {e}. Required: {{\"slug\": string}}"
                    ),
                    success: false,
                    ..Default::default()
                });
            }
        };
        let ident = input.slug.trim();
        if ident.is_empty() {
            return Ok(ToolResult {
                output: "peer_reopen requires a non-empty peer name or slug".to_owned(),
                success: false,
                ..Default::default()
            });
        }
        match (self.reopen)(ident.to_owned()) {
            Ok(message) => Ok(ToolResult {
                output: message,
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
