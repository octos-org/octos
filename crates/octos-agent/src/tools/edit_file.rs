//! Edit file tool for making precise text replacements.

use std::path::PathBuf;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use tracing::warn;

use super::write_grant::WritePathGrant;
use super::{ConcurrencyClass, Tool, ToolContext, ToolResult};
use crate::policy::{FileAccessMode, FilesystemScope};

/// Tool for editing files via string replacement.
pub struct EditFileTool {
    /// Base directory for resolving relative paths.
    base_dir: PathBuf,
    /// Effective filesystem scope.
    filesystem_scope: FilesystemScope,
    /// Whether writes are permitted.
    file_access: FileAccessMode,
    /// #1976 — optional per-path write fence. Under `create_only` EVERY edit
    /// is refused (allowlisted paths may only be created); otherwise edits
    /// follow the allowlist. `None` = pre-#1976 behaviour.
    write_grant: Option<WritePathGrant>,
}

impl EditFileTool {
    /// Create a new edit file tool.
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
            filesystem_scope: FilesystemScope::Workspace,
            file_access: FileAccessMode::ReadWrite,
            write_grant: None,
        }
    }

    /// Set the effective filesystem scope.
    pub fn with_filesystem_scope(mut self, filesystem_scope: FilesystemScope) -> Self {
        self.filesystem_scope = filesystem_scope;
        self
    }

    /// Set the effective file access mode.
    pub fn with_file_access(mut self, file_access: FileAccessMode) -> Self {
        self.file_access = file_access;
        self
    }

    /// #1976 — bind a per-path write fence (see
    /// [`WriteFileTool::with_write_grant`](super::WriteFileTool::with_write_grant)).
    pub fn with_write_grant(mut self, write_grant: WritePathGrant) -> Self {
        self.write_grant = Some(write_grant);
        self
    }
}

#[derive(Debug, Deserialize)]
// #1770: unknown keys are usually a typo of a real parameter; rejecting
// them (with a did-you-mean via `args::parse_tool_args`) lets the model
// self-correct instead of silently dropping its intent.
#[serde(deny_unknown_fields)]
struct EditFileInput {
    /// #1767: `filePath` is the industry-convention alias.
    #[serde(alias = "filePath")]
    path: String,
    #[serde(default, alias = "oldString")]
    old_string: Option<String>,
    #[serde(default, alias = "newString")]
    new_string: Option<String>,
    #[serde(default)]
    edits: Option<Vec<Replacement>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Replacement {
    #[serde(alias = "oldString")]
    old_string: String,
    #[serde(alias = "newString")]
    new_string: String,
}

#[async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn description(&self) -> &str {
        "Edit one file. For multiple replacements, use edits: [{old_string, new_string}, ...] in one call. Edits apply in order to the evolving content; all must succeed before writing. Alternatively supply a single old_string/new_string pair. Each match must be unique; exact matching is preferred, with guarded whitespace/indentation/escape fallbacks."
    }

    fn tags(&self) -> &[&str] {
        &["fs", "code"]
    }

    fn concurrency_class(&self) -> ConcurrencyClass {
        // edit_file rewrites a file in place — same race hazard as
        // write_file. Serialize the whole batch. See M8.8.
        ConcurrencyClass::Exclusive
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to edit (alias: filePath)"
                },
                "old_string": {
                    "type": "string",
                    "description": "The string to find and replace. An exact match is preferred; minor whitespace/indentation/escape differences are tolerated as a fallback. Must identify a unique location. (alias: oldString)"
                },
                "new_string": {
                    "type": "string",
                    "description": "Replacement text (alias: newString)"
                },
                "edits": {
                    "type": "array",
                    "minItems": 1,
                    "description": "Ordered replacements in the same file. Use instead of the top-level old_string/new_string pair. A failed match leaves the file unchanged.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "old_string": { "type": "string", "minLength": 1 },
                            "new_string": { "type": "string" }
                        },
                        "required": ["old_string", "new_string"],
                        "additionalProperties": false
                    }
                }
            },
            // Keep a flat, typed schema for providers that reject oneOf/not.
            // The mutually exclusive input forms are checked below.
            "required": ["path"]
        })
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        // M8.4: legacy entry point routes through the typed path with a
        // zero-value context so out-of-band callers still exercise the same
        // file-state-cache invalidation logic.
        self.execute_with_context(&ToolContext::zero(), args).await
    }

    async fn execute_with_context(
        &self,
        ctx: &ToolContext,
        args: &serde_json::Value,
    ) -> Result<ToolResult> {
        let input: EditFileInput =
            super::args::parse_tool_args(self.name(), &self.input_schema(), args)?;

        let is_batch = input.edits.is_some();
        let edits = match (input.old_string, input.new_string, input.edits) {
            (Some(old_string), Some(new_string), None) if args.get("edits").is_none() =>
                vec![Replacement { old_string, new_string }],
            (None, None, Some(edits)) if !edits.is_empty()
                && ["old_string", "new_string", "oldString", "newString"].iter().all(|key| args.get(key).is_none()) => edits,
            _ => return Err(super::ToolInputError::new(
                "edit_file expects either old_string and new_string, or a non-empty edits array; do not mix the two forms."
            ).into()),
        };

        if !self.file_access.allows_write() {
            return Ok(ToolResult {
                output: "edit_file is not permitted by read-only filesystem access".to_string(),
                success: false,
                ..Default::default()
            });
        }

        // Phase 2-C of the SessionScope migration: when the host has
        // threaded a scope through `ToolContext`, use it as the single
        // source of truth for base_dir + path classification. Same
        // write policy as `write_file` — `InWorkspace` and
        // `InGrantedDir` allowed; `InSharedZone` and `OutOfScope`
        // refused. The shared helper canonicalizes the candidate before
        // classification so ancestor symlinks can't smuggle an edit
        // out of the workspace.
        let path = match ctx.session_scope.as_ref() {
            Some(scope) => match super::resolve_path_for_session_scope_write(scope, &input.path) {
                Ok(p) => p,
                Err(reason) => {
                    return Ok(ToolResult {
                        output: format!("{reason}: {}", input.path),
                        success: false,
                        ..Default::default()
                    });
                }
            },
            None => match super::resolve_path_with_scope(
                &self.base_dir,
                &input.path,
                self.filesystem_scope,
            ) {
                Ok(p) => p,
                Err(_) => {
                    return Ok(ToolResult {
                        output: format!("Path outside working directory: {}", input.path),
                        success: false,
                        ..Default::default()
                    });
                }
            },
        };

        // Never let a file tool touch a `.git` directory: its config/hooks
        // decide what the kernel's own git invocations execute.
        if let Err(reason) = super::refuse_git_internal_path(&path) {
            return Ok(ToolResult {
                output: reason,
                success: false,
                ..Default::default()
            });
        }

        // #1976 — per-path write fence, BEFORE any file I/O. SECURITY ROUND
        // (codex): a fenced edit must read AND write through ONE confined
        // handle so an ancestor swapped between the read and the write cannot
        // redirect the edit. Under create_only every edit is refused ("created,
        // never modified"); otherwise `check_edit` returns the workspace-
        // relative path and `confined_open_rdwr` opens the leaf `O_RDWR` via a
        // component-wise `O_NOFOLLOW` `openat` walk (symlinked ancestor →
        // refused). The returned handle is reused for the write-back below, so
        // read and write bind to the same walked object. Unfenced edits keep
        // the historical decoded read + `write_no_follow`.
        let mut fenced: Option<(std::fs::File, std::path::PathBuf)> = None;
        let (content, transformed) = if let Some(grant) = &self.write_grant {
            let workspace_root = ctx
                .session_scope
                .as_ref()
                .map(|scope| scope.workspace().to_path_buf())
                .unwrap_or_else(|| self.base_dir.clone());
            let rel = match grant.check_edit(&workspace_root, &path, &input.path, self.name()) {
                Ok(rel) => rel,
                Err(denied) => {
                    return Ok(ToolResult {
                        output: denied,
                        success: false,
                        ..Default::default()
                    });
                }
            };
            match super::write_grant::confined_open_rdwr(workspace_root.clone(), rel).await {
                Ok((file, content)) => {
                    fenced = Some((file, workspace_root));
                    (content, false)
                }
                Err(e) => {
                    return Ok(ToolResult {
                        output: grant.map_confined_error(
                            &e,
                            &workspace_root,
                            &input.path,
                            self.name(),
                        ),
                        success: false,
                        ..Default::default()
                    });
                }
            }
        } else {
            // Read current content (O_NOFOLLOW atomically rejects symlinks)
            match super::read_no_follow_with_meta(&path).await {
                Ok((content, metadata)) => (content, metadata.transformed),
                Err(e) => return Ok(super::file_io_error(e, &input.path)),
            }
        };

        let previous_content = if transformed {
            None
        } else {
            super::file_mutation_previous_text(content.clone())
        };
        let mut new_content = content;
        let mut replacers = Vec::with_capacity(edits.len());
        for (index, edit) in edits.iter().enumerate() {
            match stage_replacement(&new_content, edit) {
                Ok((staged, replacer)) => {
                    new_content = staged;
                    replacers.push(replacer);
                }
                Err(reason) => {
                    return Ok(ToolResult {
                        output: if is_batch {
                            format!(
                                "Edit {} of {} failed: {reason} No changes were written.",
                                index + 1,
                                edits.len()
                            )
                        } else {
                            reason
                        },
                        success: false,
                        ..Default::default()
                    });
                }
            }
        }

        // Write back. A fenced edit rewrites the SAME confined handle opened
        // above (truncate + write from offset 0) — no re-open, so no
        // ancestor-swap window between read and write. Unfenced edits keep the
        // historical lexical `write_no_follow`.
        if let Some((file, workspace_root)) = fenced.take() {
            if let Err(e) =
                super::write_grant::confined_rewrite(file, new_content.as_bytes().to_vec()).await
            {
                // `map_confined_error` records the typed `[denied]`/[io] to the
                // sink; a rewrite failure is not create_only-relevant.
                let grant = self.write_grant.as_ref().expect("fenced implies a grant");
                return Ok(ToolResult {
                    output: grant.map_confined_error(&e, &workspace_root, &input.path, self.name()),
                    success: false,
                    ..Default::default()
                });
            }
        } else if let Err(e) = super::write_no_follow(&path, new_content.as_bytes()).await {
            return Ok(super::file_io_error(e, &input.path));
        }

        // #1976 SECURITY ROUND 2 (codex): under a per-path write fence the
        // post-write processors that re-resolve the LEXICAL path are SKIPPED —
        // a fenced edit is a leaf-file operation, not a project mutation.
        // Formatting would run an external tool on a lexical filename, and
        // `snapshot_workspace_change` lexically derives a repo root and
        // unconditionally creates dirs/`.git`/objects/commits at NON-granted
        // sibling paths (reopening the ancestor-swap window the confined edit
        // just closed). `write_grant` Some at this point means the edit went
        // through the confined path above. Cache invalidation stays (pure
        // in-memory, path-keyed).
        let fence_active = self.write_grant.is_some();

        // #1774: opt-in post-edit formatting. Runs BEFORE cache invalidation
        // and the git snapshot so both observe the final on-disk content.
        // Best-effort by contract — a formatter failure never fails the edit.
        // Never runs under a fence (see above).
        let format_note = if ctx.format_after_edit && !fence_active {
            crate::format::post_edit_format_note(&path, &new_content).await
        } else {
            None
        };

        // M8.4: invalidate any stale cache entry — the file's contents and
        // mtime just changed.
        if let Some(cache) = ctx.file_state_cache.as_ref() {
            cache.invalidate(&path);
        }

        if !fence_active {
            if let Err(error) =
                crate::workspace_git::snapshot_workspace_change(&self.base_dir, &path, "edit_file")
            {
                warn!(
                    path = %input.path,
                    error = %error,
                    "workspace git snapshot failed after edit_file"
                );
            }
        }

        // Report which replacer produced the match. The exact-match wording
        // is kept identical to the historical output for compatibility.
        let fuzzy: Vec<_> = replacers
            .iter()
            .copied()
            .filter(|name| *name != "exact")
            .collect();
        if !fuzzy.is_empty() {
            tracing::info!(replacers = ?fuzzy, path = %input.path, "edit_file fuzzy match succeeded");
        }
        let replacer_name = replacers[0];
        let output = if is_batch {
            format!(
                "Successfully edited {} ({} replacements{})",
                input.path,
                edits.len(),
                if fuzzy.is_empty() {
                    String::new()
                } else {
                    format!("; fuzzy match via {}", fuzzy.join(", "))
                }
            )
        } else if replacer_name == "exact" {
            format!("Successfully edited {}", input.path)
        } else {
            format!(
                "Successfully edited {} (fuzzy match via {replacer_name} replacer)",
                input.path
            )
        };

        Ok(ToolResult {
            output: format!("{output}{}", format_note.unwrap_or_default()),
            success: true,
            file_modified: Some(path),
            structured_metadata: Some(serde_json::json!({
                "file_created": false,
                "previous_content": previous_content,
            })),
            ..Default::default()
        })
    }
}

/// Stage one edit without touching disk. Batch callers commit only after
/// every replacement has passed the existing unique-match and span guards.
fn stage_replacement(
    content: &str,
    edit: &Replacement,
) -> std::result::Result<(String, &'static str), String> {
    if edit.old_string.is_empty() {
        return Err("old_string must not be empty".to_string());
    }

    // #1771: cascading replacer chain — exact match first, then
    // increasingly whitespace/indentation/escape-tolerant fallbacks.
    let (range, replacer_name) = match super::replacer::find_replacement(content, &edit.old_string)
    {
        super::replacer::ChainOutcome::Match { range, replacer } => (range, replacer),
        super::replacer::ChainOutcome::Ambiguous { count, replacer } => {
            return Err(format!(
                "Found {count} occurrences of the string (via {replacer} replacer). Please provide more context to make the match unique.",
            ));
        }
        super::replacer::ChainOutcome::NoMatch => {
            return Err(format!(
                "String not found in file. No exact match, and no fuzzy match via the line-trimmed, whitespace-normalized, indentation-flexible, escape-normalized or block-anchor replacers.\n\nSearched for:\n```\n{}\n```",
                edit.old_string
            ));
        }
    };

    // When the match came from the escape_normalized replacer, BOTH call
    // strings carry the same double-escaping pathology — interpret
    // new_string (and the guard's needle) with the same unescape rules
    // that made old_string match. Splicing new_string verbatim would
    // write literal `\n` text into the file as code, and guarding
    // against the still-escaped old_string (1 physical line) would
    // falsely reject every legitimate multi-line match (#1771 review).
    let (guard_needle, splice_new) = if replacer_name == "escape_normalized" {
        (
            super::replacer::unescape_find(&edit.old_string),
            super::replacer::unescape_find(&edit.new_string),
        )
    } else {
        (edit.old_string.clone(), edit.new_string.clone())
    };

    // Safety guard: a fuzzy matcher must never silently swallow far more
    // of the file than the old_string described.
    let matched_text = &content[range.clone()];
    if super::replacer::is_disproportionate_match(matched_text, &guard_needle) {
        return Err(format!(
            "Fuzzy match rejected as disproportionate: the {replacer_name} replacer matched {} lines / {} bytes for an old_string of {} lines / {} bytes. Provide more context so the match is precise.",
            matched_text.lines().count(),
            matched_text.len(),
            guard_needle.lines().count(),
            guard_needle.len(),
        ));
    }

    // Perform replacement by byte range — the fuzzy-matched span may
    // occur elsewhere as a plain substring, so a string replacen could
    // hit the wrong occurrence.
    let mut new_content = String::with_capacity(content.len() - range.len() + splice_new.len());
    new_content.push_str(&content[..range.start]);
    new_content.push_str(&splice_new);
    new_content.push_str(&content[range.end..]);

    Ok((new_content, replacer_name))
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // #1976: per-path write-grant enforcement.
    // -----------------------------------------------------------------------

    fn fenced_tool(dir: &std::path::Path, patterns: &[&str], create_only: bool) -> EditFileTool {
        let owned: Vec<String> = patterns.iter().map(|s| s.to_string()).collect();
        EditFileTool::new(dir).with_write_grant(
            crate::tools::write_grant::WritePathGrant::new(&owned, create_only)
                .expect("test grant compiles"),
        )
    }

    #[tokio::test]
    async fn write_grant_create_only_refuses_edit_even_on_allowlisted_path() {
        // Acceptance (#1976): under create_only, edit_file is refused on ANY
        // path — allowlisted or not ("created, never modified").
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("exemplar.card"), "v1\n").unwrap();
        let tool = fenced_tool(dir.path(), &["exemplar.card"], true);

        let result = tool
            .execute(&serde_json::json!({
                "path": "exemplar.card",
                "old_string": "v1",
                "new_string": "v2",
            }))
            .await
            .unwrap();
        assert!(!result.success, "create_only must refuse edits");
        assert!(
            result
                .output
                .contains(crate::tools::write_grant::DENIED_MARKER),
            "typed refusal: {}",
            result.output
        );
        assert!(result.output.contains("create-only"), "{}", result.output);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("exemplar.card")).unwrap(),
            "v1\n",
            "refused edit must leave the file untouched"
        );
    }

    #[tokio::test]
    async fn write_grant_edit_follows_allowlist_without_create_only() {
        // Without create_only: edits inside the allowlist pass, outside are
        // refused with the typed `[denied]` class.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("exemplar.card"), "v1\n").unwrap();
        std::fs::write(dir.path().join("app.md"), "keep\n").unwrap();
        let tool = fenced_tool(dir.path(), &["exemplar.card"], false);

        let ok = tool
            .execute(&serde_json::json!({
                "path": "exemplar.card",
                "old_string": "v1",
                "new_string": "v2",
            }))
            .await
            .unwrap();
        assert!(ok.success, "allowlisted edit must pass: {}", ok.output);

        let denied = tool
            .execute(&serde_json::json!({
                "path": "app.md",
                "old_string": "keep",
                "new_string": "gone",
            }))
            .await
            .unwrap();
        assert!(!denied.success);
        assert!(
            denied
                .output
                .contains(crate::tools::write_grant::DENIED_MARKER),
            "typed refusal: {}",
            denied.output
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("app.md")).unwrap(),
            "keep\n",
            "refused edit must leave the file untouched"
        );
    }
}
