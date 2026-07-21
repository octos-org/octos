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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_list_dir() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("subdir")).unwrap();
        std::fs::write(dir.path().join("file.txt"), "hello").unwrap();

        let tool = ListDirTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({"path": "."}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("[dir]  subdir"));
        assert!(result.output.contains("[file] file.txt"));
    }

    #[tokio::test]
    async fn test_not_found() {
        let dir = TempDir::new().unwrap();
        let tool = ListDirTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({"path": "nonexistent"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.output.contains("not found"));
    }

    // ------------------------------------------------------------------
    // PR-B: SessionScope integration tests for ListDirTool.
    // ------------------------------------------------------------------

    use octos_core::SessionScope;

    fn ctx_with_scope(scope: SessionScope) -> ToolContext {
        let mut ctx = ToolContext::zero();
        ctx.tool_id = "list-dir-with-scope".to_string();
        ctx.session_scope = Some(Arc::new(scope));
        ctx
    }

    #[tokio::test]
    async fn list_dir_inside_skill_dir_allowed() {
        // The LLM may `list_dir` inside a registered plugin skill_dir to
        // discover its layout. PR-A added `InSkillDir` to the read-side
        // classifier; PR-B threads that decision through here.
        let workspace = TempDir::new().unwrap();
        let skill = TempDir::new().unwrap();
        std::fs::create_dir(skill.path().join("styles")).unwrap();
        std::fs::write(skill.path().join("styles/a.toml"), "k=1").unwrap();
        std::fs::write(skill.path().join("styles/b.toml"), "k=2").unwrap();

        let scope = SessionScope::solo(workspace.path().to_path_buf(), vec![])
            .unwrap()
            .with_skill_read_zones(vec![skill.path().to_path_buf()])
            .unwrap();

        let tool = ListDirTool::new(workspace.path());
        let ctx = ctx_with_scope(scope);

        let target = skill.path().join("styles");
        let result = tool
            .execute_with_context(&ctx, &serde_json::json!({"path": target.to_string_lossy()}))
            .await
            .unwrap();
        assert!(result.success, "expected success, got: {}", result.output);
        assert!(
            result.output.contains("[file] a.toml") && result.output.contains("[file] b.toml"),
            "expected both .toml entries, got: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn list_dir_on_upload_handle_namespace_returns_guidance() {
        // #1378: list_dir('up') / list_dir('up/<handle>') must not return the
        // misleading "Directory not found"; it returns guidance pointing the
        // model at read_file so it stops looping into "please re-upload".
        let workspace = TempDir::new().unwrap();
        let scope = SessionScope::solo(workspace.path().to_path_buf(), vec![]).unwrap();
        let tool = ListDirTool::new(workspace.path());
        let ctx = ctx_with_scope(scope);

        for p in ["up", "up/MDE5_handle/report.md"] {
            let result = tool
                .execute_with_context(&ctx, &serde_json::json!({ "path": p }))
                .await
                .unwrap();
            assert!(!result.success, "expected non-success for {p:?}");
            assert!(
                result.output.contains("opaque upload handle")
                    && result.output.contains("read_file"),
                "expected upload guidance for {p:?}, got: {}",
                result.output
            );
            assert!(
                !result.output.contains("Directory not found"),
                "must NOT return the misleading not-found for {p:?}"
            );
        }
    }

    #[tokio::test]
    async fn list_dir_up_traversal_keeps_scope_error_not_upload_guidance() {
        // codex round-6 P3: a traversal/out-of-scope violation under the up/
        // namespace must surface as the real scope error, NOT be masked as
        // upload-handle confusion (the substitution is failure-exit-only and
        // never replaces a resolver error).
        let workspace = TempDir::new().unwrap();
        let scope = SessionScope::solo(workspace.path().to_path_buf(), vec![]).unwrap();
        let tool = ListDirTool::new(workspace.path());
        let ctx = ctx_with_scope(scope);

        let result = tool
            .execute_with_context(&ctx, &serde_json::json!({ "path": "up/../secret" }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(
            result.output.contains("outside session scope"),
            "traversal under up/ must report the scope error, got: {}",
            result.output
        );
        assert!(
            !result.output.contains("opaque upload handle"),
            "a policy violation must NOT be masked as upload guidance"
        );
    }

    #[tokio::test]
    async fn list_dir_lists_a_real_workspace_up_directory() {
        // codex #1378 P2: the guard is a FALLBACK — a repo that genuinely has
        // an `up/` directory must list normally, NOT get hijacked by upload
        // guidance.
        let workspace = TempDir::new().unwrap();
        std::fs::create_dir(workspace.path().join("up")).unwrap();
        std::fs::write(workspace.path().join("up/keep.txt"), "x").unwrap();
        let scope = SessionScope::solo(workspace.path().to_path_buf(), vec![]).unwrap();
        let tool = ListDirTool::new(workspace.path());
        let ctx = ctx_with_scope(scope);

        let result = tool
            .execute_with_context(&ctx, &serde_json::json!({ "path": "up" }))
            .await
            .unwrap();
        assert!(
            result.success,
            "a real up/ dir must list, got: {}",
            result.output
        );
        assert!(
            result.output.contains("keep.txt"),
            "expected up/ contents, got: {}",
            result.output
        );
        assert!(
            !result.output.contains("opaque upload handle"),
            "must NOT show upload guidance for a real up/ dir"
        );

        // codex round-2 P2: a normal file UNDER a real up/ dir keeps its normal
        // error, not upload guidance.
        let not_dir = tool
            .execute_with_context(&ctx, &serde_json::json!({ "path": "up/keep.txt" }))
            .await
            .unwrap();
        assert!(
            not_dir.output.contains("Not a directory"),
            "up/keep.txt under a real up/ dir must say Not a directory, got: {}",
            not_dir.output
        );
        assert!(!not_dir.output.contains("opaque upload handle"));
    }

    #[tokio::test]
    async fn list_dir_outside_workspace_and_skill_zones_refused() {
        // A dir entirely outside every declared zone classifies as
        // `OutOfScope` and must be refused.
        let workspace = TempDir::new().unwrap();
        let skill = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();

        let scope = SessionScope::solo(workspace.path().to_path_buf(), vec![])
            .unwrap()
            .with_skill_read_zones(vec![skill.path().to_path_buf()])
            .unwrap();

        let tool = ListDirTool::new(workspace.path());
        let ctx = ctx_with_scope(scope);

        let result = tool
            .execute_with_context(
                &ctx,
                &serde_json::json!({"path": outside.path().to_string_lossy()}),
            )
            .await
            .unwrap();
        assert!(!result.success);
        assert!(
            result.output.contains("outside session scope"),
            "expected scope rejection, got: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn list_dir_falls_back_to_legacy_when_no_scope() {
        // No scope on the context => pre-PR-B `base_dir`-relative path
        // still works (back-compat for `octos chat`).
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), "x").unwrap();

        let tool = ListDirTool::new(dir.path());
        let ctx = ToolContext::zero();
        assert!(ctx.session_scope.is_none());

        let result = tool
            .execute_with_context(&ctx, &serde_json::json!({"path": "."}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("[file] f.txt"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn list_dir_drops_symlinked_entries_pointing_out_of_scope() {
        // A symlink inside the skill_dir pointing to /tmp must not
        // leak the target's contents through `list_dir`. The
        // canonical-classify guard returns `OutOfScope` for the
        // symlink target so the entry is dropped.
        use std::os::unix::fs::symlink;

        let workspace = TempDir::new().unwrap();
        let skill = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        std::fs::write(outside.path().join("secret"), "exfil").unwrap();

        // Real entry inside the skill_dir (should remain).
        std::fs::write(skill.path().join("real.txt"), "ok").unwrap();
        // Symlink pointing outside every declared zone (should be dropped).
        symlink(outside.path(), skill.path().join("escape")).unwrap();

        let scope = SessionScope::solo(workspace.path().to_path_buf(), vec![])
            .unwrap()
            .with_skill_read_zones(vec![skill.path().to_path_buf()])
            .unwrap();

        let tool = ListDirTool::new(workspace.path());
        let ctx = ctx_with_scope(scope);

        let result = tool
            .execute_with_context(
                &ctx,
                &serde_json::json!({"path": skill.path().to_string_lossy()}),
            )
            .await
            .unwrap();
        assert!(result.success, "expected success, got: {}", result.output);
        assert!(
            result.output.contains("real.txt"),
            "expected real entry retained, got: {}",
            result.output
        );
        assert!(
            !result.output.contains("escape"),
            "symlinked entry pointing out-of-scope must be dropped, got: {}",
            result.output
        );
    }
}
