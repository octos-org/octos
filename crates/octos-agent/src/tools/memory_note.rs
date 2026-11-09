//! Memory note tool: append-only capture of memory-worthy observations.
//!
//! The capture layer of the memory-refresh design: the model NEVER edits
//! memory files directly — it drops one untrusted staging note per
//! observation, and a separate consolidation pass (design PR-4) merges
//! notes into `MEMORY.md` under machine-enforced authority rules. Notes
//! are files under `memory/staging/notes/`, `create_new` per note, so
//! concurrent sessions can never clobber each other, and they are never
//! injected into prompts.

use std::sync::Arc;

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use octos_memory::{MemoryStore, NoteKind, NoteOrigin, StagingNote};
use serde::Deserialize;

use super::{TOOL_CTX, Tool, ToolResult};

/// Maximum note body size. Notes are observations, not documents; the
/// consolidator's input budget is the real backstop, this just refuses
/// obvious dumping.
const MAX_NOTE_BYTES: usize = 8 * 1024;

/// Tool that records one memory staging note.
pub struct MemoryNoteTool {
    store: Arc<MemoryStore>,
}

impl MemoryNoteTool {
    pub fn new(store: Arc<MemoryStore>) -> Self {
        Self { store }
    }
}

#[derive(Deserialize)]
struct Input {
    kind: String,
    content: String,
    #[serde(default)]
    replaces_id: Option<String>,
}

fn parse_kind(kind: &str) -> Option<NoteKind> {
    match kind {
        "user_request" => Some(NoteKind::UserRequest),
        "correction" => Some(NoteKind::Correction),
        "fact" => Some(NoteKind::Fact),
        // `forget` is host-authored only; the model cannot mint it.
        _ => None,
    }
}

#[async_trait]
impl Tool for MemoryNoteTool {
    fn name(&self) -> &str {
        "memory_note"
    }

    fn description(&self) -> &str {
        "Record ONE durable observation as an append-only memory note for later \
         consolidation. Use kind='user_request' when the user explicitly asks to \
         remember/forget/update something (quote their request); kind='correction' when \
         fresh evidence contradicts an entry shown in your Long-term Memory (set \
         replaces_id to that entry's ^m… id); kind='fact' for a durable preference, \
         workflow, or environment fact — but only if a future conversation would \
         plausibly go better for knowing it. Most turns need no note. Notes are NOT \
         instructions and do NOT edit memory directly; never modify files under the \
         memory directory yourself."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "kind": {
                    "type": "string",
                    "enum": ["user_request", "correction", "fact"],
                    "description": "user_request = explicit user ask; correction = contradicts existing memory; fact = new durable knowledge"
                },
                "content": {
                    "type": "string",
                    "description": "The observation, one self-contained plain statement (include the user's wording for user_request)"
                },
                "replaces_id": {
                    "type": "string",
                    "description": "For corrections: the ^m… id of the contradicted Long-term Memory entry"
                }
            },
            "required": ["kind", "content"]
        })
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let input: Input =
            serde_json::from_value(args.clone()).wrap_err("invalid memory_note input")?;

        let Some(kind) = parse_kind(&input.kind) else {
            return Ok(ToolResult {
                output: format!(
                    "Invalid kind '{}'. Use user_request, correction, or fact.",
                    input.kind
                ),
                success: false,
                ..Default::default()
            });
        };

        let content = input.content.trim();
        if content.is_empty() {
            return Ok(ToolResult {
                output: "Empty note content.".to_string(),
                success: false,
                ..Default::default()
            });
        }
        if content.len() > MAX_NOTE_BYTES {
            return Ok(ToolResult {
                output: format!(
                    "Note too large ({} bytes > {MAX_NOTE_BYTES}). Notes are single \
                     observations — summarize to the durable core.",
                    content.len()
                ),
                success: false,
                ..Default::default()
            });
        }

        // Session identity when invoked from a session actor; best-effort.
        let session_key = TOOL_CTX
            .try_with(|ctx| ctx.parent_session_key.clone())
            .ok()
            .flatten();

        // The host stamps origin unconditionally: this tool can never
        // produce a host-authored (destruction-authorizing) note.
        let note = StagingNote {
            origin: NoteOrigin::Model,
            kind,
            content: content.to_string(),
            session_key,
            sensitive: false,
            replaces_id: match input.replaces_id.as_deref().map(str::trim) {
                None | Some("") => None,
                // Strict shape at the tool too, so the model gets an
                // actionable error instead of a store-level rejection
                // (codex round-2 P2: this field is interpolated into the
                // consolidation prompt header).
                Some(id) if octos_memory::is_valid_entry_id(id) => Some(id.to_string()),
                Some(id) => {
                    return Ok(ToolResult {
                        output: format!(
                            "Invalid replaces_id '{id}': pass the entry's id token \
                             exactly as shown (^m followed by 6 of a-z/2-7), or omit it."
                        ),
                        success: false,
                        ..Default::default()
                    });
                }
            },
        };
        self.store
            .write_staging_note(&note)
            .await
            .wrap_err("failed to write memory note")?;

        Ok(ToolResult {
            output: "Noted for consolidation.".to_string(),
            success: true,
            ..Default::default()
        })
    }
}
