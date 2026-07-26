//! `peer_list` — compact status index of the caller's peers.
//!
//! The companion to `peer_gather`: where `peer_gather` reads each peer's
//! full brief + latest result (the payload), `peer_list` returns a one-line
//! INDEX per peer — slug, status (running / done / closed), when it last
//! updated, turn count, and whether it has its own worktree. Use `peer_list`
//! to see WHAT peers exist and which have finished; use `peer_gather` to read
//! a specific peer's actual output.
//!
//! Like `peer_gather`, the tool is deliberately NOT registered anywhere by
//! default — construction requires the list callback, which only the
//! serve/WS turn path can provide (it owns the profile's `peers/` root). It
//! is read-only, so — like `peer_gather` — the host wires it into peer
//! sessions too: a read-only index has no recursion hazard for a depth guard
//! to contain.

use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use serde_json::{Value, json};

use super::{Tool, ToolResult};

/// Host list callback. Takes NO arguments — it always lists ALL of the
/// caller's peers. Returns the composed plain-text index (the host owns
/// formatting and caps), or a model-visible error string. Synchronous by
/// design, mirroring `PeerGatherCallback`: listing is bounded local file
/// reads under the profile's `peers/` root.
pub type PeerListCallback = Arc<dyn Fn() -> Result<String, String> + Send + Sync>;

/// `peer_list` tool. See the module docs for the index-vs-read split.
pub struct PeerListTool {
    list: PeerListCallback,
}

impl PeerListTool {
    /// Build the tool around the host's list callback. There is no
    /// callback-free constructor on purpose: without a host that owns a
    /// peer blackboard, the tool must not exist.
    pub fn new(list: PeerListCallback) -> Self {
        Self { list }
    }
}

fn failure(output: impl Into<String>) -> ToolResult {
    ToolResult {
        output: output.into(),
        success: false,
        ..Default::default()
    }
}

#[async_trait]
impl Tool for PeerListTool {
    fn name(&self) -> &str {
        "peer_list"
    }

    fn description(&self) -> &str {
        "List your peers as a compact index — ONE line per peer with its \
         status (running / done / closed), when it last updated, how many \
         turns it has run, and whether it has its own worktree. Use this to \
         see WHAT peers exist and which have finished; then use peer_gather \
         to read a specific peer's actual brief and result. Takes no \
         arguments — it always lists every peer you have staged."
    }

    fn tags(&self) -> &[&str] {
        // Same visibility surface as `peer_gather` — the index/read pairing
        // in the descriptions only works if both tools survive the same tag
        // filters.
        &["gateway"]
    }

    fn concurrency_class(&self) -> super::ConcurrencyClass {
        // Read-only: the callback only reads blackboard files, reserves
        // nothing and burns no budget — safe to run in parallel with other
        // `Safe` calls (same as `peer_gather`).
        super::ConcurrencyClass::Safe
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {}
        })
    }

    async fn execute(&self, _args: &Value) -> Result<ToolResult> {
        // No arguments: `peer_list` always lists every peer the caller has.
        match (self.list)() {
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
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    #[tokio::test]
    async fn should_invoke_callback_once_ignoring_args() {
        let calls = Arc::new(AtomicU32::new(0));
        let calls_cb = calls.clone();
        let tool = PeerListTool::new(Arc::new(move || {
            calls_cb.fetch_add(1, Ordering::SeqCst);
            Ok("peers (1):\n- ci-fix  running  updated —  turns 0\n".to_owned())
        }));

        // Extra keys are ignored — the tool takes no arguments.
        let result = tool.execute(&json!({ "ignored": true })).await.unwrap();
        assert!(result.success, "unexpected failure: {}", result.output);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "list callback invoked exactly once"
        );
    }

    #[tokio::test]
    async fn should_return_callback_text_verbatim_when_list_succeeds() {
        let text = "peers (2):\n- alpha  done  updated 123  turns 2\n\
                    - beta  running  updated —  turns 0\n";
        let tool = PeerListTool::new(Arc::new(move || Ok(text.to_owned())));

        let result = tool.execute(&json!({})).await.unwrap();
        assert!(result.success);
        assert_eq!(
            result.output, text,
            "composed index text is the tool output, unmodified"
        );
    }

    #[tokio::test]
    async fn should_surface_callback_error_as_tool_failure() {
        let tool = PeerListTool::new(Arc::new(|| {
            Err("profile dev has no bootstrapped runtime".to_owned())
        }));

        let result = tool.execute(&json!({})).await.unwrap();
        assert!(!result.success);
        assert_eq!(result.output, "profile dev has no bootstrapped runtime");
    }

    #[tokio::test]
    async fn should_declare_safe_concurrency_when_read_only() {
        let tool = PeerListTool::new(Arc::new(|| Ok(String::new())));
        assert_eq!(
            tool.concurrency_class(),
            crate::tools::ConcurrencyClass::Safe,
            "list is read-only — it must not serialize the batch like peer_handoff does"
        );
    }
}
