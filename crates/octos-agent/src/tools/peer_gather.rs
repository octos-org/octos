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
         (peers write results when their turns end). Use AFTER you have handed \
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

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    fn tool_with_recorder() -> (PeerGatherTool, Arc<Mutex<Vec<Option<Vec<String>>>>>) {
        let seen: Arc<Mutex<Vec<Option<Vec<String>>>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_cb = seen.clone();
        let tool = PeerGatherTool::new(Arc::new(move |slugs: Option<Vec<String>>| {
            seen_cb.lock().unwrap().push(slugs.clone());
            Ok("## peer ci-fix (done)\nBrief: Fix CI.\n\nAll green.\n".to_owned())
        }));
        (tool, seen)
    }

    #[tokio::test]
    async fn should_pass_none_to_callback_when_slugs_absent() {
        let (tool, seen) = tool_with_recorder();

        let result = tool.execute(&json!({})).await.unwrap();
        assert!(result.success, "unexpected failure: {}", result.output);

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0], None, "absent slugs = read every peer");
    }

    #[tokio::test]
    async fn should_treat_empty_slugs_as_all_peers() {
        let (tool, seen) = tool_with_recorder();

        let result = tool.execute(&json!({ "slugs": [] })).await.unwrap();
        assert!(result.success);
        let result = tool
            .execute(&json!({ "slugs": ["   ", "\n"] }))
            .await
            .unwrap();
        assert!(result.success);

        let seen = seen.lock().unwrap();
        assert_eq!(
            *seen,
            vec![None, None],
            "empty and all-blank slug lists both mean \"all peers\""
        );
    }

    #[tokio::test]
    async fn should_pass_trimmed_slugs_to_callback_when_named() {
        let (tool, seen) = tool_with_recorder();

        let result = tool
            .execute(&json!({ "slugs": ["  ci-fix ", "lens-review-2", "  "] }))
            .await
            .unwrap();
        assert!(result.success);

        let seen = seen.lock().unwrap();
        assert_eq!(
            seen[0],
            Some(vec!["ci-fix".to_owned(), "lens-review-2".to_owned()]),
            "slugs are trimmed and blanks dropped"
        );
    }

    #[tokio::test]
    async fn should_reject_and_skip_callback_when_slugs_not_an_array() {
        let calls = Arc::new(AtomicU32::new(0));
        let calls_cb = calls.clone();
        let tool = PeerGatherTool::new(Arc::new(move |_slugs| {
            calls_cb.fetch_add(1, Ordering::SeqCst);
            Err("must not run".to_owned())
        }));

        let result = tool
            .execute(&json!({ "slugs": "ci-fix" }))
            .await
            .expect("validation failure is a tool result, not an Err");
        assert!(!result.success);
        assert!(
            result.output.contains("invalid peer_gather arguments"),
            "schema hint surfaces: {}",
            result.output
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0, "callback must not fire");
    }

    #[tokio::test]
    async fn should_return_callback_text_verbatim_when_gather_succeeds() {
        let text = "## peer alpha (done)\nBrief: A.\n\ndone body\n\n## peer beta (still running)\nBrief: B.\n\n(still running — no result yet)\n";
        let tool = PeerGatherTool::new(Arc::new(move |_slugs| Ok(text.to_owned())));

        let result = tool.execute(&json!({})).await.unwrap();
        assert!(result.success);
        assert_eq!(
            result.output, text,
            "composed blackboard text is the tool output, unmodified"
        );
    }

    #[tokio::test]
    async fn should_surface_callback_error_as_tool_failure() {
        let tool = PeerGatherTool::new(Arc::new(|_slugs| {
            Err("profile dev has no bootstrapped runtime".to_owned())
        }));

        let result = tool.execute(&json!({})).await.unwrap();
        assert!(!result.success);
        assert_eq!(result.output, "profile dev has no bootstrapped runtime");
    }

    #[tokio::test]
    async fn should_declare_safe_concurrency_when_read_only() {
        let (tool, _) = tool_with_recorder();
        assert_eq!(
            tool.concurrency_class(),
            crate::tools::ConcurrencyClass::Safe,
            "gather is read-only — it must not serialize the batch like peer_handoff does"
        );
    }
}
