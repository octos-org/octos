//! Recall memory tool: load full entity pages from the memory bank.

use std::sync::Arc;

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use octos_memory::MemoryStore;
use serde::Deserialize;

use super::{Tool, ToolResult};

/// Tool that loads full entity pages from the memory bank.
pub struct RecallMemoryTool {
    store: Arc<MemoryStore>,
}

impl RecallMemoryTool {
    pub fn new(store: Arc<MemoryStore>) -> Self {
        Self { store }
    }
}

#[derive(Deserialize)]
struct Input {
    name: String,
    /// Optional 0-based page for the paged registry load (`name="MEMORY"`
    /// when the registry exceeds the tool-output limit). Ignored for bank
    /// entities.
    #[serde(default)]
    page: Option<usize>,
}

/// Normalize an entity name into a slug: trim, lowercase, spaces to hyphens.
fn to_slug(name: &str) -> String {
    name.trim().to_lowercase().replace(' ', "-")
}

/// Split the registry into byte-safe, line-aligned pages each just under
/// the tool-output limit, and render the requested page with a truthful
/// pager marker. The generic tool truncation (`truncate_head_tail`, 0.7)
/// would otherwise SILENTLY drop the middle of a large `MEMORY.md`,
/// recreating the unrecoverable-tail problem this tool exists to solve —
/// and a one-shot cap left the tail unreachable, over-promising a
/// targeted retrieval the schema never offered (codex #1608 P2). Paging
/// makes the whole registry reachable page by page.
fn render_registry_page(registry: &str, page: usize) -> String {
    // Headroom covers the pager marker appended after slicing (~75 bytes)
    // with generous slack, so every rendered page stays STRICTLY under the
    // outer truncate_head_tail limit — a too-tight margin let a full page +
    // marker + disclosure exceed it and re-trigger the silent middle-loss
    // (codex #1608 round-3 P1).
    let limit = octos_core::tool_output_limit("recall_memory");
    let budget = limit.saturating_sub(512).max(1);

    // Build page ranges at newline boundaries (always char-safe), each
    // <= budget bytes. A pathological newline-free run is cut at a char
    // boundary so slicing can never panic mid-codepoint.
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut start = 0usize;
    while start < registry.len() {
        let mut end = (start + budget).min(registry.len());
        while end > start && !registry.is_char_boundary(end) {
            end -= 1;
        }
        if end < registry.len() {
            if let Some(nl) = registry[start..end].rfind('\n') {
                end = start + nl + 1;
            }
        }
        if end == start {
            // Single codepoint wider than the budget: advance one char.
            end = registry[start..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| start + i)
                .unwrap_or(registry.len());
        }
        ranges.push((start, end));
        start = end;
    }

    let total = ranges.len().max(1);
    let page = page.min(total - 1);
    let (s0, e0) = ranges.get(page).copied().unwrap_or((0, registry.len()));
    let mut out = registry[s0..e0].to_string();
    if total > 1 {
        let more = if page + 1 < total {
            format!(
                " — more: call recall_memory(name=\"MEMORY\", page={})",
                page + 1
            )
        } else {
            String::new()
        };
        out.push_str(&format!(
            "\n\n_[registry page {}/{}{}]_",
            page + 1,
            total,
            more
        ));
    }
    out
}

#[async_trait]
impl Tool for RecallMemoryTool {
    fn name(&self) -> &str {
        "recall_memory"
    }

    fn description(&self) -> &str {
        "Load full memory detail on demand. Pass a memory-bank entity name \
         (as shown in the Memory Bank section) for its page, or \"MEMORY\" \
         for the complete long-term registry when the injected memory is a \
         budget-truncated summary and you need an entry that isn't shown."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Entity name (e.g. 'octos', 'yuechen'), or 'MEMORY' for the full long-term registry"
                },
                "page": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "0-based page for a large 'MEMORY' registry load; follow the 'page=N' hint in the page marker. Ignored for entity names."
                }
            },
            "required": ["name"]
        })
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let input: Input =
            serde_json::from_value(args.clone()).wrap_err("invalid recall_memory input")?;

        // Tier-2 registry load: the injected long-term memory is capped to a
        // token budget, so "MEMORY" (and aliases) returns the full MEMORY.md.
        if octos_memory::is_reserved_memory_name(&input.name) {
            // Prepend any PRE-UPGRADE bank entity whose name is now reserved:
            // new writes under these names are refused, but legacy files
            // would otherwise be shadowed by the alias and unreadable. Folding
            // their content into the paged text makes it reachable, size-safe
            // (the pager bounds it), and case/space-variant-aware since
            // list_entities returns the actual on-disk stems (codex #1608 P2).
            let mut full = String::new();
            if let Ok(entities) = self.store.list_entities().await {
                for (name, _) in entities
                    .iter()
                    .filter(|(n, _)| octos_memory::is_reserved_memory_name(n))
                {
                    if let Ok(Some(content)) = self.store.read_entity(name).await {
                        full.push_str(&format!(
                            "## Legacy bank entity \"{name}\" (shadowed by the registry \
                             alias; rename via save_memory to address it directly)\n{content}\n\n"
                        ));
                    }
                }
            }
            full.push_str(&self.store.read_long_term().await?);

            let output = if full.trim().is_empty() {
                "The long-term memory registry (MEMORY.md) is empty.".to_string()
            } else {
                render_registry_page(&full, input.page.unwrap_or(0))
            };
            return Ok(ToolResult {
                output,
                success: true,
                ..Default::default()
            });
        }

        let slug = to_slug(&input.name);

        match self.store.read_entity(&slug).await? {
            Some(content) => Ok(ToolResult {
                output: content,
                success: true,
                ..Default::default()
            }),
            None => {
                let entities = self.store.list_entities().await.unwrap_or_default();
                let available: Vec<_> = entities.iter().map(|(n, _)| n.as_str()).collect();
                Ok(ToolResult {
                    output: format!(
                        "Entity '{}' not found. Available: {}",
                        slug,
                        if available.is_empty() {
                            "(none)".to_string()
                        } else {
                            available.join(", ")
                        }
                    ),
                    success: false,
                    ..Default::default()
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_lowercase_and_trim() {
        assert_eq!(to_slug("  Octos  "), "octos");
    }

    #[test]
    fn slug_spaces_to_hyphens() {
        assert_eq!(to_slug("foo bar baz"), "foo-bar-baz");
    }

    #[test]
    fn slug_already_normalized() {
        assert_eq!(to_slug("octos"), "octos");
    }

    #[test]
    fn slug_empty_input() {
        assert_eq!(to_slug(""), "");
        assert_eq!(to_slug("   "), "");
    }

    #[test]
    fn slug_mixed_case_with_hyphens() {
        assert_eq!(to_slug("My Project"), "my-project");
    }

    #[test]
    fn input_deserialization_valid() {
        let val = serde_json::json!({"name": "octos"});
        let input: Input = serde_json::from_value(val).unwrap();
        assert_eq!(input.name, "octos");
    }

    #[test]
    fn input_deserialization_missing_name() {
        let val = serde_json::json!({});
        assert!(serde_json::from_value::<Input>(val).is_err());
    }

    #[test]
    fn schema_has_required_name() {
        // Construct a temporary store just to test metadata
        let rt = tokio::runtime::Runtime::new().unwrap();
        let store = rt.block_on(async {
            let dir = tempfile::tempdir().unwrap();
            Arc::new(MemoryStore::open(dir.path()).await.unwrap())
        });
        let tool = RecallMemoryTool::new(store);

        assert_eq!(tool.name(), "recall_memory");

        let schema = tool.input_schema();
        let required = schema["required"].as_array().unwrap();
        assert_eq!(required.len(), 1);
        assert_eq!(required[0], "name");

        let props = schema["properties"].as_object().unwrap();
        assert!(props.contains_key("name"));
        assert_eq!(props["name"]["type"], "string");
    }

    #[test]
    fn registry_aliases_match_case_and_form_insensitively() {
        for name in [
            "MEMORY",
            "memory",
            " Memory.md ",
            "registry",
            "long-term memory",
        ] {
            assert!(
                octos_memory::is_reserved_memory_name(name),
                "{name:?} should be a registry alias"
            );
        }
        for name in ["octos", "yuechen", "memories", "mem"] {
            assert!(
                !octos_memory::is_reserved_memory_name(name),
                "{name:?} is a bank entity, not the registry"
            );
        }
    }

    #[tokio::test]
    async fn should_load_full_registry_when_name_is_memory() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(MemoryStore::open(dir.path()).await.unwrap());
        store
            .write_long_term("Fact one. ^maaaaaa\nFact two. ^mbbbbbb")
            .await
            .unwrap();
        let tool = RecallMemoryTool::new(store);

        let result = tool
            .execute(&serde_json::json!({ "name": "MEMORY" }))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("Fact one."));
        assert!(result.output.contains("Fact two."));
    }

    #[tokio::test]
    async fn should_page_registry_when_it_exceeds_tool_limit() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(MemoryStore::open(dir.path()).await.unwrap());
        let limit = octos_core::tool_output_limit("recall_memory");
        // Distinct per-line content so pages are verifiably different.
        let big: String = (0..(limit / 20))
            .map(|i| format!("Fact number {i} recorded for the record. ^maaaaaa\n"))
            .collect();
        store.write_long_term(&big).await.unwrap();
        let tool = RecallMemoryTool::new(store.clone());

        let p0 = tool
            .execute(&serde_json::json!({ "name": "MEMORY" }))
            .await
            .unwrap();
        assert!(p0.success);
        assert!(
            p0.output.len() <= limit,
            "page must stay under the tool cap so the outer truncation is a no-op"
        );
        assert!(
            p0.output.contains("registry page 1/"),
            "pager marker present"
        );
        assert!(p0.output.contains("page=1"), "points at the next page");

        let p1 = tool
            .execute(&serde_json::json!({ "name": "MEMORY", "page": 1 }))
            .await
            .unwrap();
        assert!(p1.success);
        assert_ne!(
            p0.output, p1.output,
            "page 1 must return different entries than page 0 (tail reachable)"
        );
    }

    #[tokio::test]
    async fn should_not_panic_on_multibyte_registry_at_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(MemoryStore::open(dir.path()).await.unwrap());
        let limit = octos_core::tool_output_limit("recall_memory");
        // CJK is 3 bytes/char — a naive byte slice at `budget` would land
        // mid-codepoint and panic.
        let big = "记忆条目：这是一条中文记录。^maaaaaa\n".repeat(limit / 30);
        store.write_long_term(&big).await.unwrap();
        let tool = RecallMemoryTool::new(store);

        let result = tool
            .execute(&serde_json::json!({ "name": "MEMORY" }))
            .await
            .unwrap();
        assert!(result.success, "multibyte registry must not error");
        assert!(result.output.len() <= limit);
    }

    #[tokio::test]
    async fn should_disclose_legacy_entity_shadowed_by_registry_alias() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(MemoryStore::open(dir.path()).await.unwrap());
        store
            .write_long_term("A registry fact. ^maaaaaa")
            .await
            .unwrap();
        // Simulate a pre-upgrade entity literally named "memory" (writes are
        // refused now) by seeding the bank file directly on disk. Layout:
        // <data_dir>/memory/bank/entities/<name>.md.
        let bank = dir.path().join("memory").join("bank").join("entities");
        std::fs::create_dir_all(&bank).unwrap();
        std::fs::write(bank.join("memory.md"), "# memory\nlegacy page body").unwrap();
        let tool = RecallMemoryTool::new(store);

        let result = tool
            .execute(&serde_json::json!({ "name": "MEMORY" }))
            .await
            .unwrap();
        // The legacy entity's CONTENT is folded in and reachable, not noted.
        assert!(
            result.output.contains("Legacy bank entity"),
            "{}",
            result.output
        );
        assert!(
            result.output.contains("legacy page body"),
            "{}",
            result.output
        );
        assert!(
            result.output.contains("A registry fact."),
            "{}",
            result.output
        );
    }

    #[tokio::test]
    async fn should_report_empty_registry_gracefully() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(MemoryStore::open(dir.path()).await.unwrap());
        let tool = RecallMemoryTool::new(store);

        let result = tool
            .execute(&serde_json::json!({ "name": "registry" }))
            .await
            .unwrap();
        assert!(result.success, "empty registry is not an error");
        assert!(result.output.contains("empty"));
    }
}
