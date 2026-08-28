//! Recall tool (#2131): re-materialize a tool output that compaction replaced
//! with a placeholder, so the model can retrieve an evicted read/command
//! result WITHOUT re-executing it — the full bytes still live, content-
//! addressed, in the session's context ledger.

use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;

use super::{Tool, ToolResult};

/// A read-back handle over the session's content-addressed tool-output ledger.
///
/// Defined here (in octos-agent) so the tool has no dependency on the
/// octos-cli `ContextManager` that implements it; the session bootstrap injects
/// a concrete impl the same way `RecallMemoryTool` takes an `Arc<MemoryStore>`.
pub trait ToolOutputLedger: Send + Sync {
    /// Return the recorded output for a `tool_call_id` — the full raw bytes
    /// when they were spilled to the ledger, else the model-visible content.
    /// `None` when nothing is known for that id.
    fn fetch(&self, tool_call_id: &str) -> Option<String>;
}

/// Tool that restores an evicted tool output by its `tool_call_id`.
pub struct RecallTool {
    ledger: Arc<dyn ToolOutputLedger>,
}

impl RecallTool {
    pub fn new(ledger: Arc<dyn ToolOutputLedger>) -> Self {
        Self { ledger }
    }
}

#[derive(Deserialize)]
struct Input {
    /// The `tool_call_id` shown on the evicted placeholder.
    tool_call_id: String,
    /// 0-based page when the recalled output exceeds the tool-output limit.
    #[serde(default)]
    page: Option<usize>,
}

/// Split `content` into byte-safe, newline-aligned pages each strictly under
/// the recall tool-output limit and render the requested one with a truthful
/// pager marker. Without paging the generic `truncate_head_tail` would SILENTLY
/// drop the middle of a large recalled file — recreating the unrecoverable-tail
/// problem recall exists to solve (mirrors `recall_memory`'s pager).
fn render_page(content: &str, page: usize) -> String {
    let limit = octos_core::tool_output_limit("recall");
    let budget = limit.saturating_sub(512).max(1);

    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut start = 0usize;
    while start < content.len() {
        let mut end = (start + budget).min(content.len());
        while end > start && !content.is_char_boundary(end) {
            end -= 1;
        }
        if end < content.len()
            && let Some(nl) = content[start..end].rfind('\n')
        {
            end = start + nl + 1;
        }
        if end == start {
            // A single codepoint wider than the budget: advance one char so
            // slicing never panics mid-codepoint and the loop always makes
            // progress.
            end = content[start..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| start + i)
                .unwrap_or(content.len());
        }
        ranges.push((start, end));
        start = end;
    }
    if ranges.is_empty() {
        return String::new();
    }

    let total = ranges.len();
    let page = page.min(total - 1);
    let (s, e) = ranges[page];
    let body = &content[s..e];
    if total == 1 {
        body.to_string()
    } else if page + 1 < total {
        // More pages follow — name the concrete next call.
        format!(
            "{body}\n[recall page {}/{} — call recall(tool_call_id=…, page={}) for the next page]",
            page + 1,
            total,
            page + 1
        )
    } else {
        // Final page: no next call to invite.
        format!("{body}\n[recall page {}/{} — last page]", page + 1, total)
    }
}

#[async_trait]
impl Tool for RecallTool {
    fn name(&self) -> &str {
        "recall"
    }

    fn description(&self) -> &str {
        "Restore a tool output that compaction replaced with a placeholder, by \
         its tool_call_id (shown on the placeholder). Returns the exact recorded \
         output — no re-execution — so you do not have to re-read a file or re-run \
         a command whose result was evicted. Pass page=N for a large output."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "tool_call_id": {
                    "type": "string",
                    "description": "The tool_call_id from the evicted placeholder."
                },
                "page": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "0-based page for outputs larger than the tool-output limit."
                }
            },
            "required": ["tool_call_id"]
        })
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let input: Input = serde_json::from_value(args.clone())?;
        match self.ledger.fetch(&input.tool_call_id) {
            Some(content) => Ok(ToolResult {
                output: render_page(&content, input.page.unwrap_or(0)),
                success: true,
                ..Default::default()
            }),
            None => Ok(ToolResult {
                output: format!(
                    "recall: no recorded output for tool_call_id {:?}. It may have \
                     been produced before the ledger existed, or never spilled.",
                    input.tool_call_id
                ),
                success: false,
                ..Default::default()
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct MapLedger(HashMap<String, String>);
    impl ToolOutputLedger for MapLedger {
        fn fetch(&self, id: &str) -> Option<String> {
            self.0.get(id).cloned()
        }
    }

    fn tool(map: &[(&str, &str)]) -> RecallTool {
        RecallTool::new(Arc::new(MapLedger(
            map.iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )))
    }

    #[tokio::test]
    async fn recalls_the_recorded_output_by_call_id() {
        let t = tool(&[("call_7", "the full train_gpt2.c contents")]);
        let r = t
            .execute(&serde_json::json!({"tool_call_id": "call_7"}))
            .await
            .unwrap();
        assert!(r.success);
        assert_eq!(r.output, "the full train_gpt2.c contents");
    }

    #[tokio::test]
    async fn unknown_id_fails_cleanly() {
        let t = tool(&[("call_7", "x")]);
        let r = t
            .execute(&serde_json::json!({"tool_call_id": "nope"}))
            .await
            .unwrap();
        assert!(!r.success);
        assert!(r.output.contains("no recorded output"));
    }

    #[tokio::test]
    async fn large_output_is_paged_not_silently_truncated() {
        // A payload several times the recall tool-output limit must be reachable
        // page by page (no silent middle-loss).
        let limit = octos_core::tool_output_limit("recall");
        let big: String = (0..(limit / 10 + 500))
            .map(|i| format!("line {i}\n"))
            .collect();
        let t = tool(&[("call_big", big.as_str())]);
        let p0 = t
            .execute(&serde_json::json!({"tool_call_id": "call_big", "page": 0}))
            .await
            .unwrap();
        assert!(p0.success);
        assert!(p0.output.len() <= limit, "each page stays under the budget");
        assert!(p0.output.contains("recall page 1/"), "pager marker present");
        // A later page returns different content (the tail is reachable).
        let p1 = t
            .execute(&serde_json::json!({"tool_call_id": "call_big", "page": 1}))
            .await
            .unwrap();
        assert_ne!(
            p0.output, p1.output,
            "page 2 is different content, not a re-truncation"
        );
        assert!(
            p0.output.contains("for the next page"),
            "non-final pages invite the next"
        );
        // The final page (a high index clamps to the last) must NOT invite a
        // non-existent next page (#2131 review item 3).
        let last = t
            .execute(&serde_json::json!({"tool_call_id": "call_big", "page": 9_999}))
            .await
            .unwrap();
        assert!(last.output.contains("last page"), "{}", last.output);
        assert!(
            !last.output.contains("for the next page"),
            "{}",
            last.output
        );
    }
}
