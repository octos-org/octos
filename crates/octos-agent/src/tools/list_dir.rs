//! List directory tool.

use std::path::PathBuf;

use async_trait::async_trait;
use eyre::Result;
use octos_core::PathClassification;
use serde::Deserialize;

use super::{Tool, ToolContext, ToolResult};
use crate::policy::FilesystemScope;

/// List contents of a directory.
pub struct ListDirTool {
    base_dir: PathBuf,
    filesystem_scope: FilesystemScope,
}

impl ListDirTool {
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
            filesystem_scope: FilesystemScope::Workspace,
        }
    }

    pub fn with_filesystem_scope(mut self, filesystem_scope: FilesystemScope) -> Self {
        self.filesystem_scope = filesystem_scope;
        self
    }
}

#[derive(Deserialize)]
// #1770: unknown keys are usually a typo of a real parameter; rejecting
// them (with a did-you-mean via `args::parse_tool_args`) lets the model
// self-correct instead of silently dropping its intent.
#[serde(deny_unknown_fields)]
struct Input {
    path: String,
}

#[async_trait]
impl Tool for ListDirTool {
    fn name(&self) -> &str {
        "list_dir"
    }

    fn description(&self) -> &str {
        "List the contents of a directory."
    }

    fn tags(&self) -> &[&str] {
        &["search", "code"]
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "The directory path to list (relative to working directory)"
                }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        // PR-B: legacy entry point routes through the typed path with a
        // zero-value context so out-of-band callers exercise the same
        // SessionScope-aware logic when no scope is wired.
        self.execute_with_context(&ToolContext::zero(), args).await
    }

    async fn execute_with_context(
        &self,
        ctx: &ToolContext,
        args: &serde_json::Value,
    ) -> Result<ToolResult> {
        let input: Input = super::args::parse_tool_args(self.name(), &self.input_schema(), args)?;

        // Interim mitigation (#1378, superseded by #1377): `up/<base64>/<name>`
        // is an opaque upload handle, not a browsable directory. When the input
        // targets that namespace and doesn't resolve to a real listable entry,
        // return guidance pointing the model at `read_file` instead of a
        // misleading "Directory not found" (observed to make models — incl.
        // gemini-2.5-pro on replay — give up on a file they can read). The gate
        // (shared with `glob`) redirects only for a genuine upload handle OR
        // when there's no real `up/` directory, so a repo that uses `up/` as a
        // normal folder keeps its normal behaviour.
        //
        // CRITICAL (codex round-6 P3): this substitution is applied ONLY on the
        // not-found / not-a-dir exits — NEVER on a resolver error. A traversal /
        // out-of-scope violation (`up/../secret`) must surface as the real
        // "Path outside session scope" error, not be masked as upload confusion.
        let ws_root = ctx
            .session_scope
            .as_ref()
            .map(|s| s.workspace().to_path_buf())
            .unwrap_or_else(|| self.base_dir.clone());
        let up_guidance = super::upload_namespace_redirect(&input.path, &ws_root);
        let fail = |orig: String| -> ToolResult {
            ToolResult {
                output: up_guidance.map(str::to_string).unwrap_or(orig),
                success: false,
                ..Default::default()
            }
        };

        // PR-B: when the host has threaded a `SessionScope` through
        // `ToolContext`, validate the directory against it (same
        // classification semantics as `read_file` — InWorkspace,
        // InGrantedDir, InSharedZone, and InSkillDir all permit reads).
        // No scope wired => keep the legacy `base_dir + FilesystemScope`
        // path for backward compatibility with `octos chat`.
        // Resolver errors are returned VERBATIM (no upload guidance) so policy
        // violations are reported as such.
        let target = match ctx.session_scope.as_ref() {
            Some(scope) => match super::resolve_path_for_session_scope_read(scope, &input.path) {
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

        if let Some(r) = super::reject_symlink(&target).await {
            return Ok(r);
        }

        if !target.exists() {
            return Ok(fail(format!("Error: Directory not found: {}", input.path)));
        }

        if !target.is_dir() {
            return Ok(fail(format!("Error: Not a directory: {}", input.path)));
        }

        let mut entries = match tokio::fs::read_dir(&target).await {
            Ok(entries) => entries,
            Err(e) => {
                return Ok(ToolResult {
                    output: format!("Error: {e}"),
                    success: false,
                    ..Default::default()
                });
            }
        };

        // PR-B: when a SessionScope is present, drop entries whose
        // canonicalised path classifies as `OutOfScope`. This catches
        // the case where the listed directory contains a symlink that
        // points out of every declared zone — without the filter, the
        // bare entry name would leak the symlink's existence even if
        // following it would be refused at open time.
        let scope_filter = ctx.session_scope.as_ref();

        let mut dirs = Vec::new();
        let mut files = Vec::new();

        while let Ok(Some(entry)) = entries.next_entry().await {
            let entry_path = entry.path();
            if let Some(scope) = scope_filter {
                if matches!(
                    scope.classify_canonical_path(&entry_path),
                    PathClassification::OutOfScope
                ) {
                    continue;
                }
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if entry_path.is_dir() {
                dirs.push(name);
            } else {
                files.push(name);
            }
        }

        dirs.sort();
        files.sort();

        if dirs.is_empty() && files.is_empty() {
            return Ok(ToolResult {
                output: format!("Directory {} is empty.", input.path),
                success: true,
                ..Default::default()
            });
        }

        let mut out = String::new();
        for d in &dirs {
            out.push_str(&format!("[dir]  {d}\n"));
        }
        for f in &files {
            out.push_str(&format!("[file] {f}\n"));
        }

        Ok(ToolResult {
            output: format!(
                "{} entries in {}:\n{}",
                dirs.len() + files.len(),
                input.path,
                out.trim_end()
            ),
            success: true,
            ..Default::default()
        })
    }
}
