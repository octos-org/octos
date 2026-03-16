//! Save memory tool: write/update entity pages in the memory bank.

use std::sync::Arc;

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use octos_memory::MemoryStore;
use serde::Deserialize;

use super::{Tool, ToolResult};

/// Tool that saves or updates entity pages in the memory bank.
pub struct SaveMemoryTool {
    store: Arc<MemoryStore>,
}

impl SaveMemoryTool {
    pub fn new(store: Arc<MemoryStore>) -> Self {
        Self { store }
    }
}

#[derive(Deserialize)]
struct Input {
    name: String,
    content: String,
}

/// Normalize an entity name into a slug: trim, lowercase, spaces to hyphens.
fn to_slug(name: &str) -> String {
    name.trim().to_lowercase().replace(' ', "-")
}

/// Check whether a slug is valid: non-empty and only alphanumeric, hyphen, or underscore.
fn is_valid_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
}

#[async_trait]
impl Tool for SaveMemoryTool {
    fn name(&self) -> &str {
        "save_memory"
    }

    fn description(&self) -> &str {
        "Save or update a memory bank entity. Always start content with a heading \
         (# Name) followed by a one-line plain-text summary — this becomes the abstract \
         shown in the memory bank index. Example: '# Daniu\\nUBC CS student in Vancouver, \
         likes music and movies.\\n\\n## Details\\n...'. \
         IMPORTANT: When updating an existing entity, first use `recall_memory` to \
         load the current content, then MERGE new information into it before saving. \
         Never discard existing facts — add to or update them."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Entity name slug (e.g. 'octos', 'yuechen')"
                },
                "content": {
                    "type": "string",
                    "description": "Full markdown content for the entity page"
                }
            },
            "required": ["name", "content"]
        })
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let input: Input =
            serde_json::from_value(args.clone()).wrap_err("invalid save_memory input")?;

        let slug = to_slug(&input.name);

        if !is_valid_slug(&slug) {
            return Ok(ToolResult {
                output: format!(
                    "Invalid entity name: '{slug}'. Use alphanumeric chars and hyphens."
                ),
                success: false,
                ..Default::default()
            });
        }

        // Read existing content before overwriting, so we can warn about lost info
        let existing = self.store.read_entity(&slug).await.unwrap_or(None);

        self.store.write_entity(&slug, &input.content).await?;

        let output = if let Some(old) = existing {
            format!(
                "Memory entity '{slug}' updated. Previous content was:\n\
                 ---\n{old}\n---\n\
                 If important information was lost, save again with merged content."
            )
        } else {
            format!("Memory entity '{slug}' created.")
        };

        Ok(ToolResult {
            output,
            success: true,
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- to_slug ---

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
    fn slug_whitespace_only() {
        assert_eq!(to_slug("   "), "");
    }

    // --- is_valid_slug ---

    #[test]
    fn valid_slug_alphanumeric() {
        assert!(is_valid_slug("octos"));
        assert!(is_valid_slug("my_project"));
        assert!(is_valid_slug("abc123"));
    }

    #[test]
    fn invalid_slug_empty() {
        assert!(!is_valid_slug(""));
    }

    #[test]
    fn invalid_slug_special_chars() {
        assert!(!is_valid_slug("foo/bar"));
        assert!(!is_valid_slug("hello world"));
        assert!(!is_valid_slug("a.b"));
        assert!(!is_valid_slug("name@host"));
    }

    #[test]
    fn valid_slug_underscore_and_hyphen() {
        assert!(is_valid_slug("a-b_c"));
        assert!(is_valid_slug("_leading"));
        assert!(is_valid_slug("-leading"));
    }

    // --- Input deserialization ---

    #[test]
    fn input_deserialization_valid() {
        let val = serde_json::json!({"name": "test", "content": "# Test\nSome content"});
        let input: Input = serde_json::from_value(val).unwrap();
        assert_eq!(input.name, "test");
        assert_eq!(input.content, "# Test\nSome content");
    }

    #[test]
    fn input_deserialization_missing_content() {
        let val = serde_json::json!({"name": "test"});
        assert!(serde_json::from_value::<Input>(val).is_err());
    }

    #[test]
    fn input_deserialization_missing_name() {
        let val = serde_json::json!({"content": "stuff"});
        assert!(serde_json::from_value::<Input>(val).is_err());
    }

    // --- Tool metadata ---

    #[test]
    fn tool_metadata() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let store = rt.block_on(async {
            let dir = tempfile::tempdir().unwrap();
            Arc::new(MemoryStore::open(dir.path()).await.unwrap())
        });
        let tool = SaveMemoryTool::new(store);

        assert_eq!(tool.name(), "save_memory");
        assert!(tool.description().contains("Save or update"));

        let schema = tool.input_schema();
        let required = schema["required"].as_array().unwrap();
        let required_names: Vec<&str> = required.iter().map(|v| v.as_str().unwrap()).collect();
        assert!(required_names.contains(&"name"));
        assert!(required_names.contains(&"content"));

        let props = schema["properties"].as_object().unwrap();
        assert!(props.contains_key("name"));
        assert!(props.contains_key("content"));
    }

    // --- Slug + validation integration ---

    #[test]
    fn slug_with_special_chars_rejected() {
        // Simulate what execute does: slugify then validate
        let slug = to_slug("foo/bar");
        assert!(!is_valid_slug(&slug));
    }

    #[test]
    fn slug_from_spaces_becomes_valid() {
        let slug = to_slug("My Project");
        assert!(is_valid_slug(&slug));
        assert_eq!(slug, "my-project");
    }

    #[test]
    fn slug_from_whitespace_only_is_invalid() {
        let slug = to_slug("   ");
        assert!(!is_valid_slug(&slug));
    }
}
