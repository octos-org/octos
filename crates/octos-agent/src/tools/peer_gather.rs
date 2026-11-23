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
    probe: Option<PeerGatherCallback>,
}

impl PeerGatherTool {
    /// Build the tool around the host's gather callback. There is no
    /// callback-free constructor on purpose: without a host that owns a
    /// peer blackboard, the tool must not exist.
    pub fn new(gather: PeerGatherCallback) -> Self {
        Self {
            gather,
            probe: None,
        }
    }
    /// Probe is read-only and does not mark any result consumed. Only the final
    /// gather acknowledges exactly the result delivered to the model.
    pub fn with_wait_probe(gather: PeerGatherCallback, probe: PeerGatherCallback) -> Self {
        Self {
            gather,
            probe: Some(probe),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    #[serde(default)]
    wait_ms: u64,
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
         Pass slugs to read specific peers; omit to read all. Optional wait_ms (0..60000) waits for a blackboard change; timeout does not mean completion."
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
                "wait_ms": {"type":"integer","minimum":0,"maximum":60000,"description":"Wait for a change to the selected blackboard; cancellation interrupts the wait. Timeout is not completion."},
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
        if input.wait_ms > 60_000 {
            return Ok(failure("wait_ms must be at most 60000"));
        }
        if input.wait_ms > 0 && self.probe.is_none() {
            return Ok(failure(
                "This host does not support waiting; call peer_gather without wait_ms.",
            ));
        }
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
        if input.wait_ms > 0 {
            let probe = self.probe.as_ref().unwrap();
            let initial = match probe(slugs.clone()) {
                Ok(v) => v,
                Err(e) => return Ok(failure(e)),
            };
            let end = tokio::time::Instant::now() + std::time::Duration::from_millis(input.wait_ms);
            loop {
                let now = tokio::time::Instant::now();
                if now >= end {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(250).min(end - now)).await;
                match probe(slugs.clone()) {
                    Ok(v) if v != initial => break,
                    Ok(_) => {}
                    Err(e) => return Ok(failure(e)),
                }
            }
        }
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

#[cfg(test)]
mod wait_tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    #[tokio::test]
    async fn wait_probes_changes_but_only_acknowledges_delivered_results() {
        let probes = Arc::new(AtomicU32::new(0));
        let p = probes.clone();
        let acks = Arc::new(AtomicU32::new(0));
        let a = acks.clone();
        let tool = PeerGatherTool::with_wait_probe(
            Arc::new(move |_| {
                a.fetch_add(1, Ordering::SeqCst);
                Ok("published checkpoint".into())
            }),
            Arc::new(move |_| Ok(p.fetch_add(1, Ordering::SeqCst).to_string())),
        );
        let result = tool.execute(&json!({"wait_ms":5000})).await.unwrap();
        assert!(result.success);
        assert_eq!(result.output, "published checkpoint");
        assert_eq!(acks.load(Ordering::SeqCst), 1);
        assert_eq!(probes.load(Ordering::SeqCst), 2);
        assert!(
            !tool
                .execute(&json!({"wait_ms":60001}))
                .await
                .unwrap()
                .success
        );
    }
    #[tokio::test]
    async fn cancelled_wait_does_not_acknowledge_unseen_results() {
        let calls = Arc::new(AtomicU32::new(0));
        let c = calls.clone();
        let tool = PeerGatherTool::with_wait_probe(
            Arc::new(move |_| {
                c.fetch_add(1, Ordering::SeqCst);
                Ok("unseen".into())
            }),
            Arc::new(|_| Ok("pending".into())),
        );
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(20),
                tool.execute(&json!({"wait_ms":5000}))
            )
            .await
            .is_err()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            tool.execute(&json!({"wait_ms":10})).await.unwrap().output,
            "unseen"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
