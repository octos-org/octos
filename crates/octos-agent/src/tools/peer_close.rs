//! `peer_close` — retire a running peer you created.
//!
//! The lifecycle complement to `peer_handoff`: where `peer_handoff` STAGES a
//! peer and `peer_send_input` STEERS it, `peer_close` RETIRES it. The host
//! callback marks the peer closed on the durable blackboard (a
//! `peers/<slug>/closed` marker) and evicts its live wire, so it receives no
//! further input; `peer_list` / `peer_gather` then report it closed. Whatever
//! the peer produced before the close stays readable via `peer_gather`.
//!
//! Closing STOPS the peer (#1842): the host callback interrupts the peer's
//! in-flight turn on the same path a user's stop does. That is what makes a
//! close final — a peer left running after its close could park on an
//! approval / clarifying question that `peer_list` no longer shows and
//! `peer_respond` refuses to answer, hanging until the connection tore down.
//! Close a peer when you want its work to END, not to let it finish.
//!
//! Guard rails mirror `peer_send_input`:
//! - Depth-1: registered only where `peer_handoff` is (never on peer
//!   sessions themselves).
//! - Authorization: only the peer's recorded ORIGINATOR may close it
//!   (enforced host-side by the callback).
//! - The tool carries no blackboard knowledge; the host callback owns the
//!   originator check, wire eviction, and the atomic marker write.

use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;

use super::{Tool, ToolResult};

/// Host callback that closes (retires) a staged peer. The argument is the
/// trimmed `slug`; the host authorizes (originator-only), evicts the live
/// wire, and writes the durable close marker, returning a model-visible
/// confirmation or an error string.
pub type PeerCloseCallback = Arc<dyn Fn(String) -> Result<String, String> + Send + Sync>;

/// `peer_close` tool. See the module docs for the retire semantics.
pub struct PeerCloseTool {
    close: PeerCloseCallback,
}

impl PeerCloseTool {
    /// Build the tool around the host's close callback. There is no
    /// callback-free constructor on purpose: without a host that owns a
    /// peer blackboard, the tool must not exist.
    pub fn new(close: PeerCloseCallback) -> Self {
        Self { close }
    }
}

#[derive(Debug, Deserialize)]
struct Input {
    slug: String,
}

#[async_trait]
impl Tool for PeerCloseTool {
    fn name(&self) -> &str {
        "peer_close"
    }

    fn description(&self) -> &str {
        "Close (retire) a running peer YOU created, identified by its NAME (or \
         slug), as reported by peer_handoff or peer_list. The peer is marked \
         closed and receives no further input; peer_list and peer_gather then \
         show it as closed. Only the peer's originator may close it. Closing \
         STOPS the peer: its in-flight turn is interrupted, so close it when \
         you want its work to end, not to let it finish. Whatever it already \
         produced remains readable via peer_gather."
    }

    fn tags(&self) -> &[&str] {
        &["gateway"]
    }

    fn concurrency_class(&self) -> super::ConcurrencyClass {
        // Mutating: the host callback evicts the live wire registry and writes
        // the durable close marker — keep calls serialized (mirrors
        // peer_handoff, not the read-only peer_gather / peer_list).
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
                    "description": "Peer NAME (or slug) to close, as reported by peer_handoff or peer_list."
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
                        "invalid peer_close arguments: {e}. \
                         Required: {{\"slug\": string}}"
                    ),
                    success: false,
                    ..Default::default()
                });
            }
        };

        // The arg is an IDENTIFIER (peer name or slug) — a name may legitimately
        // contain characters a slug cannot (e.g. "/"), so this only rejects an
        // empty identifier; the host callback resolves it to a real slug and
        // validates THAT (`peer_slug_is_safe`) before any path / wire op.
        let ident = input.slug.trim();

        if ident.is_empty() {
            return Ok(ToolResult {
                output: "peer_close requires a non-empty peer name (or slug)".to_string(),
                success: false,
                ..Default::default()
            });
        }

        match (self.close)(ident.to_string()) {
            Ok(text) => Ok(ToolResult {
                output: text,
                success: true,
                ..Default::default()
            }),
            Err(e) => Ok(ToolResult {
                output: format!("failed to close peer {ident}: {e}"),
                success: false,
                ..Default::default()
            }),
        }
    }
}
