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
}

/// Normalize an entity name into a slug: trim, lowercase, spaces to hyphens.
fn to_slug(name: &str) -> String {
    name.trim().to_lowercase().replace(' ', "-")
}

/// Reserved names that resolve to the whole long-term MEMORY.md registry
/// rather than a memory-bank entity page (#1588 two-tier: the injected
/// registry is budget-truncated, so the model must be able to pull the
/// full thing on demand). Matched case-insensitively after trim.
fn is_registry_alias(name: &str) -> bool {
    matches!(
        name.trim().to_lowercase().as_str(),
        "memory" | "memory.md" | "registry" | "long-term memory" | "long-term-memory"
    )
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
        if is_registry_alias(&input.name) {
            let registry = self.store.read_long_term().await?;
            return Ok(ToolResult {
                output: if registry.trim().is_empty() {
                    "The long-term memory registry (MEMORY.md) is empty.".to_string()
                } else {
                    registry
                },
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
                is_registry_alias(name),
                "{name:?} should be a registry alias"
            );
        }
        for name in ["octos", "yuechen", "memories", "mem"] {
            assert!(
                !is_registry_alias(name),
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
