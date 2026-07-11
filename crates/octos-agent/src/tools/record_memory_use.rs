//! Record-memory-use tool: the model reports which memory entries actually
//! informed its answer, feeding a usage signal that consolidation uses to
//! keep useful memories alive and let dead ones age out (#1586).
//!
//! Why a tool and not a citation tail in the reply (codex's mechanism):
//! octos has no single reply choke point — channel egress is scattered
//! across ~10 channels — so a `[[mem-used: …]]` tail would risk leaking to
//! end users on any surface that missed the strip. A tool call is
//! structural, never part of user-visible content, and handled uniformly
//! by the agent loop on every surface, so the whole cross-surface strip
//! risk disappears.

use std::sync::Arc;

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use octos_memory::MemoryStore;
use serde::Deserialize;

use super::{Tool, ToolResult};

/// Records that specific memory entries were used to answer.
pub struct RecordMemoryUseTool {
    store: Arc<MemoryStore>,
}

impl RecordMemoryUseTool {
    pub fn new(store: Arc<MemoryStore>) -> Self {
        Self { store }
    }
}

#[derive(Deserialize)]
struct Input {
    ids: Vec<String>,
}

#[async_trait]
impl Tool for RecordMemoryUseTool {
    fn name(&self) -> &str {
        "record_memory_use"
    }

    fn description(&self) -> &str {
        "Report which remembered entries actually informed your answer, so \
         useful memories are kept and unused ones can age out. Pass the \
         `^m…` ids of the MEMORY.md entries and/or the memory-bank entity \
         names you relied on. Call it at most once per turn, ONLY when \
         remembered content genuinely shaped the answer — skip it otherwise."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "ids": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "MEMORY.md entry ids (e.g. '^m4k2abq') and/or bank entity names that informed the answer"
                }
            },
            "required": ["ids"]
        })
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let input: Input =
            serde_json::from_value(args.clone()).wrap_err("invalid record_memory_use input")?;

        let recorded = input.ids.iter().filter(|s| !s.trim().is_empty()).count();
        // Local date matches the (updated:) stamps consolidation compares
        // against for aging.
        let today = chrono::Local::now().date_naive();
        self.store.record_memory_use(&input.ids, today).await;

        Ok(ToolResult {
            output: format!("Recorded usage for {recorded} memory entr(y/ies)."),
            success: true,
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn should_record_ids_and_report_count() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(MemoryStore::open(dir.path()).await.unwrap());
        let tool = RecordMemoryUseTool::new(store.clone());

        let result = tool
            .execute(&serde_json::json!({ "ids": ["^m4k2abq", "octos", " "] }))
            .await
            .unwrap();
        assert!(result.success);
        // blank id is not counted
        assert!(result.output.contains("2 memory"), "{}", result.output);

        let usage = store.load_usage().await;
        assert_eq!(usage.entries["^m4k2abq"].count, 1);
        assert_eq!(usage.entries["octos"].count, 1);
        assert!(!usage.entries.contains_key(" "));
    }

    #[tokio::test]
    async fn should_accept_empty_id_list_as_noop() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(MemoryStore::open(dir.path()).await.unwrap());
        let tool = RecordMemoryUseTool::new(store.clone());

        let result = tool
            .execute(&serde_json::json!({ "ids": [] }))
            .await
            .unwrap();
        assert!(result.success);
        assert!(store.load_usage().await.entries.is_empty());
    }
}
