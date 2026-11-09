//! Codex-style `apply_patch` tool (#1773).
//!
//! Parses the Codex patch envelope (`*** Begin Patch` … `*** End Patch`) with
//! `*** Add File:` / `*** Delete File:` / `*** Update File:` sections, an
//! optional `*** Move to:` rename inside Update sections, and unified-diff
//! hunk bodies under `@@` markers.
//!
//! # Multi-file atomicity
//!
//! The ENTIRE patch is validated against the filesystem before any write:
//! every path must resolve inside the workspace, Add targets must be absent,
//! Delete/Update targets must be regular files, and every Update hunk must
//! locate its context. Validation simulates the patch in memory (an overlay
//! of path → content), so intra-patch sequences like delete-then-add or
//! add-then-update validate correctly. Only after the whole plan checks out
//! are files written. If a write still fails mid-apply (e.g. a permissions
//! race), the result reports exactly which section failed, which sections
//! were already applied, and any partial state the failed section itself
//! left behind (e.g. a move destination that was written before the source
//! removal failed) — the output never claims an unchanged workspace when
//! the failed section may have touched a file.
//!
//! # Path safety
//!
//! Envelope paths must be workspace-relative: absolute paths and `..`
//! components are rejected outright, then every path goes through the same
//! workspace-scoping resolution the other file tools use (session-scope-aware
//! when a [`SessionScope`](octos_core::SessionScope) is threaded through the
//! [`ToolContext`]). All file I/O uses the shared `O_NOFOLLOW` helpers so
//! symlinks are rejected atomically.
//!
//! Hunk-body semantics (line classification, pattern/replacement extraction,
//! trailing-whitespace-tolerant matching) are shared with `diff_edit` — see
//! [`super::diff_edit`].

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::warn;

use super::diff_edit::{DiffLine, matches_at, pattern_lines, replacement_lines};
use super::{ConcurrencyClass, Tool, ToolContext, ToolResult};
use crate::policy::{FileAccessMode, FilesystemScope};

/// Tool applying multi-file Codex-envelope patches atomically.
pub struct ApplyPatchTool {
    base_dir: PathBuf,
    filesystem_scope: FilesystemScope,
    file_access: FileAccessMode,
}

impl ApplyPatchTool {
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
            filesystem_scope: FilesystemScope::Workspace,
            file_access: FileAccessMode::ReadWrite,
        }
    }

    pub fn with_filesystem_scope(mut self, filesystem_scope: FilesystemScope) -> Self {
        self.filesystem_scope = filesystem_scope;
        self
    }

    pub fn with_file_access(mut self, file_access: FileAccessMode) -> Self {
        self.file_access = file_access;
        self
    }
}

#[derive(Debug, Deserialize)]
struct ApplyPatchInput {
    #[serde(default)]
    patch: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    diff: Option<String>,
}

#[async_trait]
impl Tool for ApplyPatchTool {
    fn name(&self) -> &str {
        "apply_patch"
    }

    fn description(&self) -> &str {
        "Apply a multi-file patch in the Codex envelope format. Supports adding, \
         deleting, updating, and moving/renaming files in ONE atomic call — the \
         entire patch is validated before any file is written. Update hunks are \
         unified-diff bodies (' ' context, '-' remove, '+' add) under '@@' \
         markers; '@@ <text>' anchors the search at that context line. Example:\n\
         *** Begin Patch\n\
         *** Add File: docs/hello.txt\n\
         +Hello world\n\
         *** Update File: src/app.py\n\
         *** Move to: src/main.py\n\
         @@ def greet():\n\
         -    print(\"Hi\")\n\
         +    print(\"Hello, world!\")\n\
         *** Delete File: obsolete.txt\n\
         *** End Patch\n\
         Paths must be workspace-relative. Also accepts {path, diff} to apply a \
         single-file unified diff."
    }

    fn tags(&self) -> &[&str] {
        &["fs", "code"]
    }

    fn concurrency_class(&self) -> ConcurrencyClass {
        // apply_patch mutates multiple files on disk — the same race hazard
        // as write_file / diff_edit, multiplied across sections. Serialize
        // the whole batch (M8.8).
        ConcurrencyClass::Exclusive
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "patch": {
                    "type": "string",
                    "description": "Codex patch envelope; see tool description for syntax and example. Paths must be workspace-relative."
                },
                "path": {
                    "type": "string",
                    "description": "Single file path when applying a plain unified diff instead of an envelope"
                },
                "diff": {
                    "type": "string",
                    "description": "Unified diff (with @@ hunk headers) applied to path"
                }
            }
        })
    }

    async fn execute(&self, args: &Value) -> Result<ToolResult> {
        self.execute_with_context(&ToolContext::zero(), args).await
    }

    async fn execute_with_context(&self, ctx: &ToolContext, args: &Value) -> Result<ToolResult> {
        let input: ApplyPatchInput =
            serde_json::from_value(args.clone()).wrap_err("invalid apply_patch input")?;
        if !self.file_access.allows_write() {
            return Ok(ToolResult {
                output: "apply_patch is not permitted by read-only filesystem access".to_string(),
                success: false,
                ..Default::default()
            });
        }
        if let (Some(path), Some(diff)) = (input.path.as_deref(), input.diff.as_deref()) {
            return self.apply_unified_diff(ctx, path, diff).await;
        }
        let Some(patch) = input.patch.as_deref() else {
            return Ok(ToolResult {
                output: "apply_patch requires either patch or {path,diff}".to_string(),
                success: false,
                ..Default::default()
            });
        };
        self.apply_codex_patch(ctx, patch).await
    }
}

impl ApplyPatchTool {
    /// Legacy `{path, diff}` form — delegates to the unified-diff editor.
    async fn apply_unified_diff(
        &self,
        ctx: &ToolContext,
        path: &str,
        diff: &str,
    ) -> Result<ToolResult> {
        let tool = super::DiffEditTool::new(&self.base_dir)
            .with_filesystem_scope(self.filesystem_scope)
            .with_file_access(self.file_access);
        tool.execute_with_context(ctx, &json!({ "path": path, "diff": diff }))
            .await
    }

    async fn apply_codex_patch(&self, ctx: &ToolContext, patch: &str) -> Result<ToolResult> {
        let ops = match parse_patch_envelope(patch) {
            Ok(ops) => ops,
            Err(e) => {
                return Ok(ToolResult {
                    output: format!(
                        "Failed to parse patch: {e}\n\nExpected envelope:\n\
                         *** Begin Patch\n\
                         *** Update File: <path>\n\
                         @@ <context marker>\n\
                         <space><context line>\n\
                         -<removed line>\n\
                         +<added line>\n\
                         *** End Patch\n\
                         (other sections: '*** Add File: <path>' with '+' lines, \
                         '*** Delete File: <path>', '*** Move to: <path>' after Update File)"
                    ),
                    success: false,
                    ..Default::default()
                });
            }
        };

        // Phase 1 — validate the WHOLE patch (paths, existence, hunks) before
        // touching the filesystem.
        let plan = match self.plan_sections(ctx, &ops).await {
            Ok(plan) => plan,
            Err(e) => {
                return Ok(ToolResult {
                    output: format!("Patch validation failed — {e}. No files were modified."),
                    success: false,
                    ..Default::default()
                });
            }
        };

        // Phase 2 — apply. Failures here are unexpected (the plan validated),
        // but if one happens we report exactly what was already applied and
        // any partial state the failed section itself left on disk.
        let (applied, failure) = self.apply_planned(ctx, &plan).await;

        let snapshot_seed = applied
            .first()
            .and_then(|op| op.touched.first())
            .or_else(|| {
                failure
                    .as_ref()
                    .and_then(|f| f.partial.first().map(|p| &p.resolved))
            });
        if let Some(first) = snapshot_seed
            && let Err(error) = crate::workspace_git::snapshot_workspace_change(
                &self.base_dir,
                first,
                "apply_patch",
            )
        {
            warn!(
                error = %error,
                "workspace git snapshot failed after apply_patch"
            );
        }

        let previews: Vec<Value> = applied.iter().map(AppliedOp::preview_entry).collect();
        let mut modified_paths: Vec<String> = applied.iter().map(|op| op.display.clone()).collect();
        let file_modified = applied.first().and_then(|op| op.touched.first().cloned());

        if let Some(failure) = failure {
            // Honesty invariant: never claim an untouched workspace when the
            // failed section may have modified a file (a move that wrote its
            // destination, or a truncate-in-place write that failed partway).
            let partial_paths: Vec<String> =
                failure.partial.iter().map(|p| p.display.clone()).collect();
            for path in &partial_paths {
                if !modified_paths.contains(path) {
                    modified_paths.push(path.clone());
                }
            }
            let file_modified =
                file_modified.or_else(|| failure.partial.first().map(|p| p.resolved.clone()));

            let mut output = format!(
                "Patch failed at section {} ({}): {}\n",
                failure.section_no, failure.label, failure.error
            );
            if applied.is_empty() && failure.partial.is_empty() {
                output.push_str("No sections were applied; the workspace is unchanged.");
            } else {
                let mut state = Vec::new();
                if !applied.is_empty() {
                    let summaries: Vec<String> = applied.iter().map(AppliedOp::summary).collect();
                    state.push(format!(
                        "Already applied before the failure: {}.",
                        summaries.join(", ")
                    ));
                }
                if !failure.partial.is_empty() {
                    let notes: Vec<String> = failure
                        .partial
                        .iter()
                        .map(|p| format!("{} ({})", p.display, p.note))
                        .collect();
                    state.push(format!(
                        "The failed section left partial state on disk: {}.",
                        notes.join(", ")
                    ));
                }
                output.push_str(&format!(
                    "{}\nRemaining sections were NOT applied — the workspace is \
                     partially patched; re-read the affected files before retrying.",
                    state.join(" ")
                ));
            }
            return Ok(ToolResult {
                output,
                success: false,
                file_modified,
                structured_metadata: Some(json!({
                    "codex_tool": "apply_patch",
                    "diff_preview": previews,
                    "modified_paths": modified_paths,
                    "file_created": applied.first().map(|op| op.created),
                    "previous_content": applied.first().and_then(|op| op.previous_content.clone()),
                    "partial_paths": partial_paths,
                    "failed_section": failure.section_no,
                })),
                ..Default::default()
            });
        }

        let listed: Vec<String> = applied.iter().map(AppliedOp::display_for_output).collect();
        Ok(ToolResult {
            output: format!("Applied patch to {}", listed.join(", ")),
            success: true,
            file_modified,
            // #972 / M14-B — structured diff preview event consumed by the
            // AppUI diff flow. `codex_tool = "apply_patch"` matches the
            // model-visible tool name so the client routing stays uniform
            // with `update_plan` / `request_user_input`.
            structured_metadata: Some(json!({
                "codex_tool": "apply_patch",
                "diff_preview": previews,
                "modified_paths": modified_paths,
                "file_created": applied.first().map(|op| op.created),
                "previous_content": applied.first().and_then(|op| op.previous_content.clone()),
            })),
            ..Default::default()
        })
    }

    /// Resolve a patch-section path with the same workspace scoping the other
    /// file tools use, after rejecting absolute paths and `..` components
    /// outright (envelope paths must be workspace-relative).
    fn resolve_patch_path(&self, ctx: &ToolContext, user_path: &str) -> Result<PathBuf, String> {
        reject_unsafe_patch_path(user_path)?;
        let resolved = match ctx.session_scope.as_ref() {
            Some(scope) => super::resolve_path_for_session_scope_write(scope, user_path)
                .map_err(|reason| format!("{reason}: {user_path}")),
            None => {
                super::resolve_path_with_scope(&self.base_dir, user_path, self.filesystem_scope)
                    .map_err(|_| format!("Path outside working directory: {user_path}"))
            }
        }?;
        // No section may add, update, move into/out of or delete anything
        // inside a `.git` directory.
        super::refuse_git_internal_path(&resolved)?;
        Ok(resolved)
    }

    /// Phase 1: validate every section against the filesystem (through an
    /// in-memory overlay so intra-patch sequences compose) and produce the
    /// concrete write plan. No filesystem mutation happens here.
    async fn plan_sections(
        &self,
        ctx: &ToolContext,
        ops: &[PatchOp],
    ) -> Result<Vec<PlannedSection>, String> {
        // Simulated post-patch state per resolved path: `Some(content)` for a
        // file this patch creates/rewrites, `None` for a file it removes.
        // Paths not in the overlay defer to the on-disk state.
        let mut overlay: HashMap<PathBuf, Option<String>> = HashMap::new();
        let mut plan = Vec::new();

        for (i, op) in ops.iter().enumerate() {
            let section_no = i + 1;
            let label = op.label();
            let fail = |detail: String| format!("section {section_no} ({label}): {detail}");

            let change = match op {
                PatchOp::Add { path, content } => {
                    let resolved = self.resolve_patch_path(ctx, path).map_err(fail)?;
                    if overlay_entry_exists(&overlay, &resolved)
                        .await
                        .map_err(fail)?
                    {
                        return Err(fail(
                            "file already exists (Add File requires the path to be absent)"
                                .to_string(),
                        ));
                    }
                    overlay.insert(resolved.clone(), Some(content.clone()));
                    PlannedChange::Write {
                        path: resolved,
                        display: path.clone(),
                        content: content.clone(),
                        op: "add",
                    }
                }
                PatchOp::Delete { path } => {
                    let resolved = self.resolve_patch_path(ctx, path).map_err(fail)?;
                    match overlay.get(&resolved) {
                        Some(Some(_)) => {}
                        Some(None) => return Err(fail("file not found".to_string())),
                        None => match disk_entry(&resolved).await.map_err(fail)? {
                            DiskEntry::File => {}
                            DiskEntry::Absent => return Err(fail("file not found".to_string())),
                            DiskEntry::Symlink => {
                                return Err(fail("Symlinks are not allowed".to_string()));
                            }
                            DiskEntry::Other => {
                                return Err(fail("not a regular file".to_string()));
                            }
                        },
                    }
                    overlay.insert(resolved.clone(), None);
                    PlannedChange::Remove {
                        path: resolved,
                        display: path.clone(),
                    }
                }
                PatchOp::Update {
                    path,
                    move_to,
                    hunks,
                } => {
                    let resolved = self.resolve_patch_path(ctx, path).map_err(fail)?;
                    let current = match overlay.get(&resolved) {
                        Some(Some(content)) => content.clone(),
                        Some(None) => return Err(fail("file not found".to_string())),
                        None => match disk_entry(&resolved).await.map_err(fail)? {
                            DiskEntry::File => super::read_no_follow(&resolved)
                                .await
                                .map_err(|e| fail(format!("failed to read file: {e}")))?,
                            DiskEntry::Absent => return Err(fail("file not found".to_string())),
                            DiskEntry::Symlink => {
                                return Err(fail("Symlinks are not allowed".to_string()));
                            }
                            DiskEntry::Other => {
                                return Err(fail("not a regular file".to_string()));
                            }
                        },
                    };
                    let updated = apply_codex_hunks(&current, hunks).map_err(fail)?;
                    // A move to the source path is a plain in-place update.
                    let dest = match move_to.as_deref() {
                        Some(dest_display) => {
                            let dest_resolved =
                                self.resolve_patch_path(ctx, dest_display).map_err(fail)?;
                            (dest_resolved != resolved)
                                .then_some((dest_resolved, dest_display.to_string()))
                        }
                        None => None,
                    };
                    match dest {
                        Some((to_resolved, to_display)) => {
                            if overlay_entry_exists(&overlay, &to_resolved)
                                .await
                                .map_err(fail)?
                            {
                                return Err(fail(format!(
                                    "move destination already exists: {to_display}"
                                )));
                            }
                            overlay.insert(resolved.clone(), None);
                            overlay.insert(to_resolved.clone(), Some(updated.clone()));
                            PlannedChange::Move {
                                from: resolved,
                                from_display: path.clone(),
                                to: to_resolved,
                                to_display,
                                content: updated,
                            }
                        }
                        None => {
                            overlay.insert(resolved.clone(), Some(updated.clone()));
                            PlannedChange::Write {
                                path: resolved,
                                display: path.clone(),
                                content: updated,
                                op: "update",
                            }
                        }
                    }
                }
            };
            plan.push(PlannedSection {
                section_no,
                label,
                change,
            });
        }
        Ok(plan)
    }

    /// Phase 2: execute the validated plan in section order. Returns the
    /// successfully applied operations plus the failure (if any) that stopped
    /// the patch.
    async fn apply_planned(
        &self,
        ctx: &ToolContext,
        plan: &[PlannedSection],
    ) -> (Vec<AppliedOp>, Option<ApplyFailure>) {
        let mut applied = Vec::new();
        for section in plan {
            // Invalidate every candidate path regardless of outcome — a
            // failed move may still have written its destination.
            let candidates: Vec<PathBuf> = match &section.change {
                PlannedChange::Write { path, .. } | PlannedChange::Remove { path, .. } => {
                    vec![path.clone()]
                }
                PlannedChange::Move { from, to, .. } => vec![from.clone(), to.clone()],
            };

            let (created, previous_content) = match &section.change {
                PlannedChange::Write { path, op, .. } => (
                    *op == "add",
                    if *op == "add" {
                        None
                    } else {
                        super::read_file_mutation_previous_text(path).await
                    },
                ),
                PlannedChange::Remove { path, .. } => {
                    (false, super::read_file_mutation_previous_text(path).await)
                }
                PlannedChange::Move { from, .. } => {
                    (false, super::read_file_mutation_previous_text(from).await)
                }
            };

            let outcome: Result<AppliedOp, SectionFailure> =
                match &section.change {
                    PlannedChange::Write {
                        path,
                        display,
                        content,
                        op,
                    } => write_with_parents(path, content)
                        .await
                        .map(|()| AppliedOp {
                            op,
                            created,
                            previous_content: previous_content.clone(),
                            display: display.clone(),
                            from_display: None,
                            touched: vec![path.clone()],
                        })
                        .map_err(|failure| SectionFailure {
                            partial: write_failure_partial(&failure, display, path),
                            error: failure.into_message(),
                        }),
                    PlannedChange::Remove { path, display } => tokio::fs::remove_file(path)
                        .await
                        .map_err(|e| SectionFailure {
                            error: format!("failed to delete file: {e}"),
                            // A failed unlink leaves the file as it was.
                            partial: Vec::new(),
                        })
                        .map(|()| AppliedOp {
                            op: "delete",
                            created,
                            previous_content: previous_content.clone(),
                            display: display.clone(),
                            from_display: None,
                            touched: vec![path.clone()],
                        }),
                    PlannedChange::Move {
                        from,
                        from_display,
                        to,
                        to_display,
                        content,
                    } => {
                        async {
                            write_with_parents(to, content).await.map_err(|failure| {
                                SectionFailure {
                                    partial: write_failure_partial(&failure, to_display, to),
                                    error: failure.into_message(),
                                }
                            })?;
                            tokio::fs::remove_file(from).await.map_err(|e| SectionFailure {
                            error: format!(
                                "wrote {to_display} but failed to remove {from_display}: {e}"
                            ),
                            // The destination fully exists even though the
                            // section did not complete — report it so the
                            // result never claims an unchanged workspace.
                            partial: vec![PartialPath {
                                display: to_display.clone(),
                                resolved: to.clone(),
                                note: PARTIAL_WRITTEN,
                            }],
                        })?;
                            Ok(AppliedOp {
                                op: "move",
                                created,
                                previous_content: previous_content.clone(),
                                display: to_display.clone(),
                                from_display: Some(from_display.clone()),
                                touched: vec![from.clone(), to.clone()],
                            })
                        }
                        .await
                    }
                };

            if let Some(cache) = ctx.file_state_cache.as_ref() {
                for path in &candidates {
                    cache.invalidate(path);
                }
            }

            match outcome {
                Ok(op) => applied.push(op),
                Err(SectionFailure { error, partial }) => {
                    return (
                        applied,
                        Some(ApplyFailure {
                            section_no: section.section_no,
                            label: section.label.clone(),
                            error,
                            partial,
                        }),
                    );
                }
            }
        }
        (applied, None)
    }
}

/// Kind of directory entry currently on disk at a path.
enum DiskEntry {
    Absent,
    File,
    Symlink,
    Other,
}

/// Stat a path without following symlinks. `NotFound` / `NotADirectory`
/// (an ancestor component is a regular file) both classify as absent.
async fn disk_entry(path: &Path) -> Result<DiskEntry, String> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(meta) if meta.is_symlink() => Ok(DiskEntry::Symlink),
        Ok(meta) if meta.is_file() => Ok(DiskEntry::File),
        Ok(_) => Ok(DiskEntry::Other),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            Ok(DiskEntry::Absent)
        }
        Err(e) => Err(format!("failed to stat path: {e}")),
    }
}

/// Whether any entry (file, directory, or symlink) currently occupies `path`,
/// consulting the intra-patch overlay first.
async fn overlay_entry_exists(
    overlay: &HashMap<PathBuf, Option<String>>,
    path: &Path,
) -> Result<bool, String> {
    if let Some(state) = overlay.get(path) {
        return Ok(state.is_some());
    }
    Ok(!matches!(disk_entry(path).await?, DiskEntry::Absent))
}

/// Failure from [`write_with_parents`], distinguishing whether the target
/// file itself may have been touched (drives partial-state reporting).
enum WriteFailure {
    /// Parent-directory creation failed — the target file was NOT touched.
    Parents(String),
    /// The `O_NOFOLLOW` write failed. The writer opens with truncate, so the
    /// target may have been created, truncated, or partially written before
    /// the error.
    Write(String),
}

impl WriteFailure {
    fn into_message(self) -> String {
        match self {
            WriteFailure::Parents(message) | WriteFailure::Write(message) => message,
        }
    }
}

/// Partial-state entries for a failed [`write_with_parents`] call: a failed
/// parent mkdir touched nothing, but a failed write may have left the target
/// created, truncated, or partially written.
fn write_failure_partial(failure: &WriteFailure, display: &str, path: &Path) -> Vec<PartialPath> {
    match failure {
        WriteFailure::Parents(_) => Vec::new(),
        WriteFailure::Write(_) => vec![PartialPath {
            display: display.to_string(),
            resolved: path.to_path_buf(),
            note: PARTIAL_UNKNOWN,
        }],
    }
}

/// Create parent directories and write `content` through the shared
/// `O_NOFOLLOW` writer.
async fn write_with_parents(path: &Path, content: &str) -> Result<(), WriteFailure> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|e| {
            WriteFailure::Parents(format!("failed to create parent directories: {e}"))
        })?;
    }
    super::write_no_follow(path, content.as_bytes())
        .await
        .map_err(|e| WriteFailure::Write(format!("failed to write file: {e}")))
}

// ---------------------------------------------------------------------------
// Parsed representation
// ---------------------------------------------------------------------------

/// One file section of a parsed patch envelope.
#[derive(Debug)]
enum PatchOp {
    Add {
        path: String,
        content: String,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        move_to: Option<String>,
        hunks: Vec<UpdateHunk>,
    },
}

impl PatchOp {
    /// Section label used in model-facing error messages.
    fn label(&self) -> String {
        match self {
            PatchOp::Add { path, .. } => format!("Add File: {path}"),
            PatchOp::Delete { path } => format!("Delete File: {path}"),
            PatchOp::Update {
                path,
                move_to: Some(dest),
                ..
            } => format!("Update File: {path} -> {dest}"),
            PatchOp::Update { path, .. } => format!("Update File: {path}"),
        }
    }
}

/// One hunk of an Update section.
#[derive(Debug, Default)]
struct UpdateHunk {
    /// Optional `@@ <anchor>` context marker: a file line the matcher must
    /// locate (at or after the current cursor) before searching for the hunk
    /// body.
    anchor: Option<String>,
    /// Classified body lines (shared [`DiffLine`] representation).
    lines: Vec<DiffLine>,
    /// `*** End of File`: this hunk must match at the very end of the file.
    at_eof: bool,
}

/// A validated, ready-to-apply change for one patch section.
struct PlannedSection {
    section_no: usize,
    label: String,
    change: PlannedChange,
}

enum PlannedChange {
    /// Create or overwrite `path` with `content` (`op` is "add" or "update").
    Write {
        path: PathBuf,
        display: String,
        content: String,
        op: &'static str,
    },
    /// Remove the file at `path`.
    Remove { path: PathBuf, display: String },
    /// Write updated `content` to `to`, then remove `from` (Update + Move).
    Move {
        from: PathBuf,
        from_display: String,
        to: PathBuf,
        to_display: String,
        content: String,
    },
}

/// A successfully applied section, for reporting and cache invalidation.
struct AppliedOp {
    op: &'static str,
    created: bool,
    previous_content: Option<String>,
    /// Display path (destination path for moves).
    display: String,
    /// Move source display path.
    from_display: Option<String>,
    /// Resolved paths touched on disk.
    touched: Vec<PathBuf>,
}

impl AppliedOp {
    fn summary(&self) -> String {
        match self.op {
            "add" => format!("added {}", self.display),
            "update" => format!("updated {}", self.display),
            "delete" => format!("deleted {}", self.display),
            "move" => format!(
                "moved {} -> {}",
                self.from_display.as_deref().unwrap_or("?"),
                self.display
            ),
            other => format!("{other} {}", self.display),
        }
    }

    fn display_for_output(&self) -> String {
        match self.from_display.as_deref() {
            Some(from) => format!("{from} -> {}", self.display),
            None => self.display.clone(),
        }
    }

    fn preview_entry(&self) -> Value {
        match self.from_display.as_deref() {
            Some(from) => json!({ "op": self.op, "path": self.display, "from": from }),
            None => json!({ "op": self.op, "path": self.display }),
        }
    }
}

/// State note for a [`PartialPath`]: the content was fully written (a move
/// destination whose source removal then failed).
const PARTIAL_WRITTEN: &str = "written";
/// State note for a [`PartialPath`]: the truncate-in-place write failed
/// partway, so the on-disk state is unknown.
const PARTIAL_UNKNOWN: &str = "may have been created, truncated, or partially written";

/// A path the FAILED section may already have modified on disk before the
/// failure. Reported so the result never claims an untouched workspace.
struct PartialPath {
    /// Workspace-relative display path.
    display: String,
    /// Resolved on-disk path.
    resolved: PathBuf,
    /// Honest state description ([`PARTIAL_WRITTEN`] / [`PARTIAL_UNKNOWN`]).
    note: &'static str,
}

/// Failure of one section during the apply phase.
struct SectionFailure {
    error: String,
    /// Paths the failed section may have modified before failing.
    partial: Vec<PartialPath>,
}

struct ApplyFailure {
    section_no: usize,
    label: String,
    error: String,
    /// Paths the failed section may have modified before failing.
    partial: Vec<PartialPath>,
}

// ---------------------------------------------------------------------------
// Path safety
// ---------------------------------------------------------------------------

/// Reject absolute paths and `..` components before any resolution. Envelope
/// paths must be plain workspace-relative paths.
fn reject_unsafe_patch_path(user_path: &str) -> Result<(), String> {
    let path = Path::new(user_path);
    if path.is_absolute() {
        return Err(format!(
            "absolute paths are not allowed in apply_patch (use workspace-relative paths): {user_path}"
        ));
    }
    for component in path.components() {
        match component {
            Component::ParentDir => {
                return Err(format!(
                    "'..' path components are not allowed in apply_patch: {user_path}"
                ));
            }
            Component::Prefix(_) | Component::RootDir => {
                return Err(format!(
                    "absolute paths are not allowed in apply_patch (use workspace-relative paths): {user_path}"
                ));
            }
            Component::CurDir | Component::Normal(_) => {}
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Envelope parsing
// ---------------------------------------------------------------------------

/// Parse a Codex patch envelope into file operations. Returns a model-facing
/// error message on any malformed input.
fn parse_patch_envelope(input: &str) -> Result<Vec<PatchOp>, String> {
    /// In-flight section being accumulated by the parser.
    enum Section {
        None,
        Add {
            path: String,
            lines: Vec<String>,
        },
        Update {
            path: String,
            move_to: Option<String>,
            hunks: Vec<UpdateHunk>,
        },
    }

    fn finalize(section: &mut Section, ops: &mut Vec<PatchOp>) -> Result<(), String> {
        match std::mem::replace(section, Section::None) {
            Section::None => Ok(()),
            Section::Add { path, lines } => {
                ops.push(PatchOp::Add {
                    path,
                    content: lines.join("\n"),
                });
                Ok(())
            }
            Section::Update {
                path,
                move_to,
                hunks,
            } => {
                // A hunk-less Update is only meaningful as a pure rename.
                if hunks.iter().all(|h| h.lines.is_empty()) && move_to.is_none() {
                    return Err(format!(
                        "'*** Update File: {path}' section has no hunks (and no '*** Move to:')"
                    ));
                }
                ops.push(PatchOp::Update {
                    path,
                    move_to,
                    hunks,
                });
                Ok(())
            }
        }
    }

    fn directive_path(rest: &str, directive: &str, line_no: usize) -> Result<String, String> {
        let path = rest.trim();
        if path.is_empty() {
            return Err(format!("line {line_no}: missing path after '{directive}'"));
        }
        Ok(path.to_string())
    }

    // Tolerate CRLF input and blank lines around the envelope.
    let mut lines = input
        .lines()
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .enumerate();

    let begin = lines.find(|(_, l)| !l.trim().is_empty());
    match begin {
        Some((_, "*** Begin Patch")) => {}
        _ => return Err("patch must start with '*** Begin Patch'".to_string()),
    }

    let mut ops: Vec<PatchOp> = Vec::new();
    let mut section = Section::None;
    let mut saw_end = false;

    for (idx, line) in lines {
        let line_no = idx + 1;
        if saw_end {
            if line.trim().is_empty() {
                continue;
            }
            return Err(format!("line {line_no}: content after '*** End Patch'"));
        }
        if line == "*** End Patch" {
            finalize(&mut section, &mut ops)?;
            saw_end = true;
        } else if let Some(rest) = line.strip_prefix("*** Add File: ") {
            finalize(&mut section, &mut ops)?;
            section = Section::Add {
                path: directive_path(rest, "*** Add File:", line_no)?,
                lines: Vec::new(),
            };
        } else if let Some(rest) = line.strip_prefix("*** Delete File: ") {
            finalize(&mut section, &mut ops)?;
            ops.push(PatchOp::Delete {
                path: directive_path(rest, "*** Delete File:", line_no)?,
            });
        } else if let Some(rest) = line.strip_prefix("*** Update File: ") {
            finalize(&mut section, &mut ops)?;
            section = Section::Update {
                path: directive_path(rest, "*** Update File:", line_no)?,
                move_to: None,
                hunks: Vec::new(),
            };
        } else if let Some(rest) = line.strip_prefix("*** Move to: ") {
            match &mut section {
                Section::Update {
                    move_to: move_to @ None,
                    hunks,
                    ..
                } if hunks.is_empty() => {
                    *move_to = Some(directive_path(rest, "*** Move to:", line_no)?);
                }
                Section::Update { move_to: None, .. } => {
                    return Err(format!(
                        "line {line_no}: '*** Move to:' must appear directly after \
                         '*** Update File:' (before any hunks)"
                    ));
                }
                Section::Update { .. } => {
                    return Err(format!(
                        "line {line_no}: duplicate '*** Move to:' in one Update section"
                    ));
                }
                _ => {
                    return Err(format!(
                        "line {line_no}: '*** Move to:' is only valid inside an \
                         '*** Update File:' section"
                    ));
                }
            }
        } else if line == "*** End of File" {
            match &mut section {
                Section::Update { hunks, .. } if !hunks.is_empty() => {
                    if let Some(hunk) = hunks.last_mut() {
                        hunk.at_eof = true;
                    }
                }
                _ => {
                    return Err(format!(
                        "line {line_no}: '*** End of File' is only valid after a hunk \
                         in an '*** Update File:' section"
                    ));
                }
            }
        } else if line.starts_with("***") {
            return Err(format!(
                "line {line_no}: unrecognized or malformed directive: {line} \
                 (expected '*** Add File: <path>', '*** Delete File: <path>', \
                 '*** Update File: <path>', '*** Move to: <path>', or '*** End Patch')"
            ));
        } else {
            match &mut section {
                Section::None => {
                    if line.trim().is_empty() {
                        continue;
                    }
                    return Err(format!(
                        "line {line_no}: expected a '*** ' directive, got: {line}"
                    ));
                }
                Section::Add { lines, .. } => {
                    if let Some(rest) = line.strip_prefix('+') {
                        lines.push(rest.to_string());
                    } else if line.is_empty() {
                        // Tolerate a bare empty line as an empty content line
                        // (editors commonly strip the '+' from blank lines).
                        lines.push(String::new());
                    } else {
                        return Err(format!(
                            "line {line_no}: lines in an '*** Add File:' section must \
                             start with '+'"
                        ));
                    }
                }
                Section::Update { hunks, .. } => {
                    // `*** End of File` pins the last hunk to the end of the
                    // file and closes the section's hunks — silently
                    // appending later body lines to it would corrupt its
                    // match semantics. Only blank separator lines and new
                    // '*** ' directives may follow.
                    if hunks.last().is_some_and(|h| h.at_eof) {
                        if line.trim().is_empty() {
                            continue;
                        }
                        return Err(format!(
                            "line {line_no}: content after '*** End of File' in an \
                             '*** Update File:' section ('*** End of File' closes the \
                             section's hunks; only a new '*** ' directive may follow)"
                        ));
                    }
                    if let Some(rest) = line.strip_prefix("@@") {
                        hunks.push(UpdateHunk {
                            anchor: normalize_hunk_anchor(rest),
                            ..UpdateHunk::default()
                        });
                        continue;
                    }
                    // Body line before any explicit `@@` opens an implicit hunk.
                    if hunks.is_empty() {
                        hunks.push(UpdateHunk::default());
                    }
                    let hunk = hunks.last_mut().expect("hunk just ensured");
                    if let Some(rest) = line.strip_prefix('+') {
                        hunk.lines.push(DiffLine::Add(rest.to_string()));
                    } else if let Some(rest) = line.strip_prefix('-') {
                        hunk.lines.push(DiffLine::Remove(rest.to_string()));
                    } else if let Some(rest) = line.strip_prefix(' ') {
                        hunk.lines.push(DiffLine::Context(rest.to_string()));
                    } else if line.is_empty() {
                        // Tolerate a bare empty line as an empty context line.
                        hunk.lines.push(DiffLine::Context(String::new()));
                    } else if line.starts_with('\\') {
                        // "\ No newline at end of file" — standard diff noise.
                    } else {
                        return Err(format!(
                            "line {line_no}: lines in an '*** Update File:' hunk must \
                             start with '+', '-', or ' ' (context)"
                        ));
                    }
                }
            }
        }
    }

    if !saw_end {
        return Err("patch is missing '*** End Patch'".to_string());
    }
    if ops.is_empty() {
        return Err("patch contains no file sections".to_string());
    }
    Ok(ops)
}

/// Normalize the text after an `@@` marker into an optional context anchor.
///
/// Codex-format markers are `@@` (plain separator) or `@@ <anchor text>`.
/// Classic unified headers (`@@ -1,3 +1,3 @@ [anchor]`) are tolerated: the
/// numeric range pair is stripped and only the trailing anchor text (if any)
/// is kept, because the sequential matcher does not use line numbers.
fn normalize_hunk_anchor(rest: &str) -> Option<String> {
    let rest = rest.trim();
    if rest.is_empty() {
        return None;
    }
    if rest.starts_with('-')
        && let Some(pos) = rest.rfind("@@")
    {
        let anchor = rest[pos + 2..].trim();
        return (!anchor.is_empty()).then(|| anchor.to_string());
    }
    Some(rest.to_string())
}

// ---------------------------------------------------------------------------
// Hunk application (sequential, Codex semantics)
// ---------------------------------------------------------------------------

/// Apply Update-section hunks to `content` sequentially: each hunk is
/// located at or after the position where the previous hunk ended, using the
/// shared trailing-whitespace-tolerant matcher. Returns the patched content
/// or a model-facing error naming the failing hunk.
fn apply_codex_hunks(content: &str, hunks: &[UpdateHunk]) -> Result<String, String> {
    let had_trailing_newline = content.ends_with('\n');
    let mut lines: Vec<String> = content.lines().map(String::from).collect();
    let mut cursor = 0usize;

    for (i, hunk) in hunks.iter().enumerate() {
        let hunk_no = i + 1;

        if let Some(anchor) = hunk.anchor.as_deref() {
            // Locate the `@@ <anchor>` context line at/after the cursor; the
            // hunk body is then searched after it. Falls back to a
            // fully-trimmed comparison for indentation drift.
            let anchor_pos = (cursor..lines.len())
                .find(|&idx| lines[idx].trim_end() == anchor.trim_end())
                .or_else(|| (cursor..lines.len()).find(|&idx| lines[idx].trim() == anchor.trim()));
            match anchor_pos {
                Some(pos) => cursor = pos + 1,
                None => {
                    return Err(format!(
                        "hunk {hunk_no}: context marker '@@ {anchor}' not found in the \
                         file (searched from line {})",
                        cursor + 1
                    ));
                }
            }
        }

        let pattern = pattern_lines(&hunk.lines);
        let replacement = replacement_lines(&hunk.lines);

        if pattern.is_empty() {
            // Pure insertion — only legal when the position is unambiguous.
            let insert_at = if hunk.at_eof {
                lines.len()
            } else if hunk.anchor.is_some() {
                cursor
            } else if lines.is_empty() {
                0
            } else {
                return Err(format!(
                    "hunk {hunk_no} has no context lines; add ' ' context lines, an \
                     '@@ <anchor>' marker, or '*** End of File'"
                ));
            };
            let added = replacement.len();
            lines.splice(insert_at..insert_at, replacement);
            cursor = insert_at + added;
            continue;
        }

        let match_pos = if hunk.at_eof {
            // The pattern must sit exactly at the end of the file.
            lines
                .len()
                .checked_sub(pattern.len())
                .filter(|&pos| pos >= cursor && matches_at(&lines, &pattern, pos))
        } else {
            find_block_from(&lines, &pattern, cursor)
        };
        let Some(match_pos) = match_pos else {
            let preview: Vec<&str> = pattern.iter().take(3).copied().collect();
            return Err(format!(
                "hunk {hunk_no}: could not find the context/removed lines in the file \
                 (searched from line {}). Expected block starting with: {preview:?}",
                cursor + 1
            ));
        };
        let added = replacement.len();
        lines.splice(match_pos..match_pos + pattern.len(), replacement);
        cursor = match_pos + added;
    }

    let mut out = lines.join("\n");
    if had_trailing_newline {
        out.push('\n');
    }
    Ok(out)
}

/// Find the first position `>= from` where `pattern` matches `lines`.
fn find_block_from(lines: &[String], pattern: &[&str], from: usize) -> Option<usize> {
    if pattern.is_empty() || pattern.len() > lines.len() {
        return None;
    }
    (from..=lines.len() - pattern.len()).find(|&pos| matches_at(lines, pattern, pos))
}
