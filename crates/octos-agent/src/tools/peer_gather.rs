//! `peer_gather` — model-readable peer blackboard (#1801 v3 fan-in).
//!
//! The read half of the peer loop: `peer_handoff` fans work OUT into
//! sovereign peer sessions, `peer_gather` fans their results back IN. The
//! tool reads the durable blackboard (each peer's brief + latest result
//! file) through an injected host callback and hands the model one composed
//! plain-text view; peers that have not finished a turn yet are listed as
//! still running.
//!
//! Like `peer_handoff`, the tool is deliberately NOT registered anywhere by
//! default — construction requires the gather callback, which only the
//! serve/WS turn path can provide (it owns the profile's `peers/` root).
//! Unlike `peer_handoff`, the host wires it into peer sessions too: reading
//! sibling results is blackboard collaboration, and a read-only tool has no
//! recursion hazard for a depth guard to contain.

use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Tool, ToolResult};

/// Host gather callback. `None` reads every staged peer; `Some(slugs)`
/// narrows to those peers. Returns the composed plain-text blackboard view
/// (the host owns formatting and caps), or a model-visible error string.
/// Synchronous by design, mirroring `PeerHandoffCallback`: gathering is
/// bounded local file reads under the profile's `peers/` root.
pub type PeerGatherCallback =
    Arc<dyn Fn(Option<Vec<String>>) -> Result<String, String> + Send + Sync>;

/// `peer_gather` tool. See the module docs for the fan-out/fan-in split.
pub struct PeerGatherTool {
    gather: PeerGatherCallback,
}

impl PeerGatherTool {
    /// Build the tool around the host's gather callback. There is no
    /// callback-free constructor on purpose: without a host that owns a
    /// peer blackboard, the tool must not exist.
    pub fn new(gather: PeerGatherCallback) -> Self {
        Self { gather }
    }
}

#[derive(Debug, Deserialize)]
struct Input {
    #[serde(default)]
    slugs: Option<Vec<String>>,
}

fn failure(output: impl Into<String>) -> ToolResult {
    ToolResult {
        output: output.into(),
        success: false,
        ..Default::default()
    }
}

#[async_trait]
impl Tool for PeerGatherTool {
    fn name(&self) -> &str {
        "peer_gather"
    }

    fn description(&self) -> &str {
        "Read the peer blackboard: each peer's task brief and its latest result \
         (peers write results when their turns end). Also reads automatic workspace peers by workspace agent_id. Use AFTER you have handed \
         work off with peer_handoff — typically later in the conversation, or \
         when the user asks what the peers found — to collect and synthesize \
         their results. Peers with no result yet are listed as still running; \
         do not busy-wait on them, continue other work and gather again later. \
         Pass slugs to read specific peers; omit to read all."
    }

    fn tags(&self) -> &[&str] {
        // Same visibility surface as `peer_handoff` — the fan-out/fan-in
        // pairing in the descriptions only works if both tools survive the
        // same tag filters.
        &["gateway"]
    }

    fn concurrency_class(&self) -> super::ConcurrencyClass {
        // Read-only: the callback only reads blackboard files, reserves
        // nothing and burns no budget — safe to run in parallel with other
        // `Safe` calls (contrast `peer_handoff`, which is `Exclusive`).
        super::ConcurrencyClass::Safe
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "slugs": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Slugs of specific peers to read (as reported by peer_handoff). Omit to read every staged peer."
                }
            }
        })
    }

    async fn execute(&self, args: &Value) -> Result<ToolResult> {
        let input: Input = match serde_json::from_value(args.clone()) {
            Ok(input) => input,
            Err(err) => {
                return Ok(failure(format!(
                    "invalid peer_gather arguments: {err}. Optional: {{\"slugs\": [string]}} \
                     — omit to read every staged peer."
                )));
            }
        };
        // Trim entries and drop blanks; an empty (or all-blank) list reads
        // every peer, same as omitting `slugs` — a model passing `[]` means
        // "all", not "none".
        let slugs = input.slugs.and_then(|slugs| {
            let slugs: Vec<String> = slugs
                .iter()
                .map(|slug| slug.trim())
                .filter(|slug| !slug.is_empty())
                .map(ToOwned::to_owned)
                .collect();
            (!slugs.is_empty()).then_some(slugs)
        });
        match (self.gather)(slugs) {
            Ok(text) => Ok(ToolResult {
                output: text,
                success: true,
                ..Default::default()
            }),
            Err(err) => Ok(failure(err)),
        }
    }
}
