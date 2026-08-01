//! `peer_close` — gracefully retire a running peer you created.
//!
//! The lifecycle complement to `peer_handoff`: where `peer_handoff` STAGES a
//! peer and `peer_send_input` STEERS it, `peer_close` RETIRES it. The host
//! callback marks the peer closed on the durable blackboard (a
//! `peers/<slug>/closed` marker) and evicts its live wire, so it receives no
//! further input; `peer_list` / `peer_gather` then report it closed. Closing
//! is GRACEFUL — the peer finishes any in-flight turn; this does NOT abort a
//! running turn — and its result files stay readable via `peer_gather`.
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

/// `peer_close` tool. See the module docs for the graceful-retire semantics.
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
         show it as closed. Only the peer's originator may close it. GRACEFUL — \
         the peer finishes any in-flight turn; this does NOT abort a running \
         turn. Its result files remain readable via peer_gather."
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

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    fn tool_with_recorder() -> (PeerCloseTool, Arc<Mutex<Vec<String>>>) {
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_cb = seen.clone();
        let tool = PeerCloseTool::new(Arc::new(move |slug: String| {
            seen_cb.lock().unwrap().push(slug.clone());
            Ok(format!(
                "peer '{slug}' closed — it will receive no further input"
            ))
        }));
        (tool, seen)
    }

    #[tokio::test]
    async fn should_reject_missing_slug_without_calling_callback() {
        let calls = Arc::new(AtomicU32::new(0));
        let calls_cb = calls.clone();
        let tool = PeerCloseTool::new(Arc::new(move |_slug| {
            calls_cb.fetch_add(1, Ordering::SeqCst);
            Err("must not run".to_owned())
        }));

        let result = tool
            .execute(&json!({}))
            .await
            .expect("validation failure is a tool result, not an Err");
        assert!(!result.success);
        assert!(
            result.output.contains("invalid peer_close arguments"),
            "schema hint surfaces: {}",
            result.output
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0, "callback must not fire");
    }

    #[tokio::test]
    async fn should_reject_blank_slug_without_calling_callback() {
        let (tool, seen) = tool_with_recorder();

        let result = tool.execute(&json!({ "slug": "   " })).await.unwrap();
        assert!(!result.success);
        assert!(
            result.output.contains("non-empty"),
            "blank-identifier hint surfaces: {}",
            result.output
        );
        assert!(
            seen.lock().unwrap().is_empty(),
            "callback must not fire on a blank identifier"
        );
    }

    #[tokio::test]
    async fn should_trim_slug_before_callback() {
        let (tool, seen) = tool_with_recorder();

        let result = tool.execute(&json!({ "slug": "  ci-fix " })).await.unwrap();
        assert!(result.success, "unexpected failure: {}", result.output);

        let seen = seen.lock().unwrap();
        assert_eq!(
            seen.as_slice(),
            &["ci-fix".to_owned()],
            "slug is trimmed before the callback"
        );
    }

    #[tokio::test]
    async fn should_return_callback_text_verbatim_when_close_succeeds() {
        let tool = PeerCloseTool::new(Arc::new(|_slug| {
            Ok("peer 'ci-fix' closed — it will receive no further input".to_owned())
        }));

        let result = tool.execute(&json!({ "slug": "ci-fix" })).await.unwrap();
        assert!(result.success);
        assert_eq!(
            result.output, "peer 'ci-fix' closed — it will receive no further input",
            "callback confirmation is the tool output, unmodified"
        );
    }

    #[tokio::test]
    async fn should_surface_callback_error_as_tool_failure() {
        let tool = PeerCloseTool::new(Arc::new(|_slug| {
            Err("not the owner of peer session 'ci-fix'".to_owned())
        }));

        let result = tool.execute(&json!({ "slug": "ci-fix" })).await.unwrap();
        assert!(!result.success);
        assert!(
            result
                .output
                .contains("not the owner of peer session 'ci-fix'"),
            "callback error surfaces: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn should_pass_slashed_identifier_through_to_callback() {
        // The arg is an IDENTIFIER (peer name or slug); a NAME may contain a
        // "/", so the tool must NOT reject it — the host resolves it to a real
        // slug and validates that. Only empties are rejected tool-side.
        let (tool, seen) = tool_with_recorder();

        let result = tool.execute(&json!({ "slug": "Team A/B" })).await.unwrap();
        assert!(result.success, "unexpected failure: {}", result.output);
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            &["Team A/B".to_owned()],
            "a slash-bearing identifier is passed through to the host to resolve"
        );
    }

    #[tokio::test]
    async fn should_declare_exclusive_concurrency() {
        let (tool, _) = tool_with_recorder();
        assert_eq!(
            tool.concurrency_class(),
            crate::tools::ConcurrencyClass::Exclusive,
            "peer_close mutates the wire registry + filesystem — it must serialize like peer_handoff"
        );
    }
}
