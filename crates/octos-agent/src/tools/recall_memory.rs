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

#[async_trait]
impl Tool for RecallMemoryTool {
    fn name(&self) -> &str {
        "recall_memory"
    }

    fn description(&self) -> &str {
        "Load the full content of a memory bank entity by name. \
         Use the entity names shown in the Memory Bank section of the system prompt."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Entity name (e.g. 'octos', 'yuechen')"
                }
            },
            "required": ["name"]
        })
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let input: Input =
            serde_json::from_value(args.clone()).wrap_err("invalid recall_memory input")?;

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
}
