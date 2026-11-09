//! Save memory tool: write/update entity pages in the memory bank.

use std::sync::Arc;

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use octos_memory::MemoryStore;
use serde::Deserialize;

use super::{ConcurrencyClass, Tool, ToolResult};

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

    fn concurrency_class(&self) -> ConcurrencyClass {
        // save_memory writes the memory bank. A parallel recall_memory could
        // observe a half-written entity. Serialize the whole batch. See M8.8.
        ConcurrencyClass::Exclusive
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Entity name slug (e.g. 'octos', 'alice')"
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

        // Names reserved for recall_memory's registry load must not become
        // bank entities — recall_memory would resolve them to MEMORY.md and
        // the entity would be permanently unreachable (codex #1608 P2).
        if octos_memory::is_reserved_memory_name(&input.name)
            || octos_memory::is_reserved_memory_name(&slug)
        {
            return Ok(ToolResult {
                output: format!(
                    "'{slug}' is reserved (recall_memory uses it for the full registry). \
                     Choose a different entity name."
                ),
                success: false,
                ..Default::default()
            });
        }

        // Write-time threat gate (#1585): entity pages are loaded into the
        // prompt via recall_memory and the memory-bank index; refuse content
        // that reads as instruction-override / exfiltration.
        if let Some(threat) = octos_memory::guard::first_threat(&input.content) {
            return Ok(ToolResult {
                output: format!(
                    "Memory content rejected by the content guard ({threat}). \
                     Restate the information as plain facts without \
                     instruction-like or exfiltration phrasing."
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
