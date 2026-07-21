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
                    "description": "Codex patch envelope: '*** Begin Patch', then one or more '*** Add File: <path>' / '*** Delete File: <path>' / '*** Update File: <path>' sections (Update may be followed by '*** Move to: <path>' and unified-diff hunks under '@@' markers), then '*** End Patch'. Paths must be workspace-relative."
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
            })),
            ..Default::default()
        })
    }

    /// Resolve a patch-section path with the same workspace scoping the other
    /// file tools use, after rejecting absolute paths and `..` components
    /// outright (envelope paths must be workspace-relative).
    fn resolve_patch_path(&self, ctx: &ToolContext, user_path: &str) -> Result<PathBuf, String> {
        reject_unsafe_patch_path(user_path)?;
        match ctx.session_scope.as_ref() {
            Some(scope) => super::resolve_path_for_session_scope_write(scope, user_path)
                .map_err(|reason| format!("{reason}: {user_path}")),
            None => {
                super::resolve_path_with_scope(&self.base_dir, user_path, self.filesystem_scope)
                    .map_err(|_| format!("Path outside working directory: {user_path}"))
            }
        }
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
                    let resolved = self.resolve_patch_path(ctx, path).map_err(&fail)?;
                    if overlay_entry_exists(&overlay, &resolved)
                        .await
                        .map_err(&fail)?
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
                    let resolved = self.resolve_patch_path(ctx, path).map_err(&fail)?;
                    match overlay.get(&resolved) {
                        Some(Some(_)) => {}
                        Some(None) => return Err(fail("file not found".to_string())),
                        None => match disk_entry(&resolved).await.map_err(&fail)? {
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
                    let resolved = self.resolve_patch_path(ctx, path).map_err(&fail)?;
                    let current = match overlay.get(&resolved) {
                        Some(Some(content)) => content.clone(),
                        Some(None) => return Err(fail("file not found".to_string())),
                        None => match disk_entry(&resolved).await.map_err(&fail)? {
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
                    let updated = apply_codex_hunks(&current, hunks).map_err(&fail)?;
                    // A move to the source path is a plain in-place update.
                    let dest = match move_to.as_deref() {
                        Some(dest_display) => {
                            let dest_resolved =
                                self.resolve_patch_path(ctx, dest_display).map_err(&fail)?;
                            (dest_resolved != resolved)
                                .then_some((dest_resolved, dest_display.to_string()))
                        }
                        None => None,
                    };
                    match dest {
                        Some((to_resolved, to_display)) => {
                            if overlay_entry_exists(&overlay, &to_resolved)
                                .await
                                .map_err(&fail)?
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    async fn run(tool: &ApplyPatchTool, patch: &str) -> ToolResult {
        tool.execute(&json!({ "patch": patch }))
            .await
            .expect("apply_patch execute")
    }

    /// Parse an envelope whose first section is an Update and return its hunks.
    fn parse_update_hunks(patch: &str) -> Vec<UpdateHunk> {
        let mut ops = parse_patch_envelope(patch).expect("valid envelope");
        match ops.remove(0) {
            PatchOp::Update { hunks, .. } => hunks,
            other => panic!("expected Update, got {other:?}"),
        }
    }

    // -- Metadata ----------------------------------------------------------

    #[test]
    fn apply_patch_tool_is_exclusive() {
        let dir = tempfile::tempdir().unwrap();
        let tool = ApplyPatchTool::new(dir.path());
        assert_eq!(tool.concurrency_class(), ConcurrencyClass::Exclusive);
        assert_eq!(tool.name(), "apply_patch");
        assert!(tool.tags().contains(&"fs"));
        // The spec description must teach the envelope by example.
        assert!(tool.description().contains("*** Begin Patch"));
        assert!(tool.description().contains("*** End Patch"));
        assert!(tool.description().contains("*** Move to:"));
    }

    // -- Envelope parsing --------------------------------------------------

    #[test]
    fn should_parse_all_op_kinds_when_envelope_is_valid() {
        let patch = "*** Begin Patch\n\
                     *** Add File: hello.txt\n\
                     +Hello world\n\
                     *** Update File: src/app.py\n\
                     *** Move to: src/main.py\n\
                     @@ def greet():\n\
                     -print(\"Hi\")\n\
                     +print(\"Hello, world!\")\n\
                     *** Delete File: obsolete.txt\n\
                     *** End Patch\n";
        let ops = parse_patch_envelope(patch).expect("valid envelope");
        assert_eq!(ops.len(), 3);
        match &ops[0] {
            PatchOp::Add { path, content } => {
                assert_eq!(path, "hello.txt");
                assert_eq!(content, "Hello world");
            }
            other => panic!("expected Add, got {other:?}"),
        }
        match &ops[1] {
            PatchOp::Update {
                path,
                move_to,
                hunks,
            } => {
                assert_eq!(path, "src/app.py");
                assert_eq!(move_to.as_deref(), Some("src/main.py"));
                assert_eq!(hunks.len(), 1);
                assert_eq!(hunks[0].anchor.as_deref(), Some("def greet():"));
                assert_eq!(hunks[0].lines.len(), 2);
            }
            other => panic!("expected Update, got {other:?}"),
        }
        match &ops[2] {
            PatchOp::Delete { path } => assert_eq!(path, "obsolete.txt"),
            other => panic!("expected Delete, got {other:?}"),
        }
    }

    #[test]
    fn should_reject_when_begin_marker_missing() {
        let err = parse_patch_envelope("*** Add File: a.txt\n+x\n*** End Patch\n")
            .expect_err("missing Begin Patch must be rejected");
        assert!(err.contains("*** Begin Patch"), "got: {err}");
    }

    #[test]
    fn should_reject_when_end_marker_missing() {
        let err = parse_patch_envelope("*** Begin Patch\n*** Add File: a.txt\n+x\n")
            .expect_err("missing End Patch must be rejected");
        assert!(err.contains("*** End Patch"), "got: {err}");
    }

    #[test]
    fn should_reject_when_content_follows_end_marker() {
        let err = parse_patch_envelope(
            "*** Begin Patch\n*** Add File: a.txt\n+x\n*** End Patch\nleftover\n",
        )
        .expect_err("content after End Patch must be rejected");
        assert!(err.contains("End Patch"), "got: {err}");
    }

    #[test]
    fn should_reject_when_directive_unknown() {
        let err = parse_patch_envelope("*** Begin Patch\n*** Rename File: a.txt\n*** End Patch\n")
            .expect_err("unknown directive must be rejected");
        assert!(err.contains("Rename File"), "got: {err}");
    }

    #[test]
    fn should_reject_when_move_to_outside_update_section() {
        let err = parse_patch_envelope(
            "*** Begin Patch\n*** Add File: a.txt\n*** Move to: b.txt\n+x\n*** End Patch\n",
        )
        .expect_err("Move to outside Update must be rejected");
        assert!(err.contains("Move to"), "got: {err}");
    }

    #[test]
    fn should_reject_when_add_line_lacks_plus_prefix() {
        let err = parse_patch_envelope(
            "*** Begin Patch\n*** Add File: a.txt\nno prefix\n*** End Patch\n",
        )
        .expect_err("Add lines without '+' must be rejected");
        assert!(err.contains('+'), "got: {err}");
    }

    #[test]
    fn should_reject_when_patch_has_no_sections() {
        let err = parse_patch_envelope("*** Begin Patch\n*** End Patch\n")
            .expect_err("empty patch must be rejected");
        assert!(err.contains("no file sections"), "got: {err}");
    }

    #[test]
    fn should_reject_when_update_section_has_no_hunks_and_no_move() {
        let err = parse_patch_envelope("*** Begin Patch\n*** Update File: a.txt\n*** End Patch\n")
            .expect_err("Update without hunks or move must be rejected");
        assert!(err.contains("a.txt"), "got: {err}");
    }

    #[test]
    fn should_treat_numeric_hunk_header_as_plain_separator() {
        let patch = "*** Begin Patch\n\
                     *** Update File: a.txt\n\
                     @@ -1,3 +1,3 @@\n\
                     -x\n\
                     +y\n\
                     @@ -10,3 +10,3 @@ fn main()\n\
                     -a\n\
                     +b\n\
                     *** End Patch\n";
        let ops = parse_patch_envelope(patch).expect("valid envelope");
        match &ops[0] {
            PatchOp::Update { hunks, .. } => {
                assert_eq!(hunks.len(), 2);
                assert_eq!(hunks[0].anchor, None);
                assert_eq!(hunks[1].anchor.as_deref(), Some("fn main()"));
            }
            other => panic!("expected Update, got {other:?}"),
        }
    }

    #[test]
    fn should_reject_content_after_end_of_file_marker() {
        // A body line silently appended to the EOF-pinned hunk would corrupt
        // its match-at-end semantics — it must be a parse error instead.
        let err = parse_patch_envelope(
            "*** Begin Patch\n*** Update File: f\n@@\n+tail\n*** End of File\n+stray\n*** End Patch\n",
        )
        .expect_err("body line after '*** End of File' must be rejected");
        assert!(err.contains("line 6"), "got: {err}");
        assert!(err.contains("End of File"), "got: {err}");

        // Same for a new `@@` hunk: nothing can match after the end of file.
        let err = parse_patch_envelope(
            "*** Begin Patch\n*** Update File: f\n@@\n+tail\n*** End of File\n@@ more\n-x\n+y\n*** End Patch\n",
        )
        .expect_err("new hunk after '*** End of File' must be rejected");
        assert!(err.contains("End of File"), "got: {err}");
    }

    #[test]
    fn should_tolerate_blank_line_after_end_of_file_marker() {
        let hunks = parse_update_hunks(
            "*** Begin Patch\n*** Update File: f\n@@\n+tail\n*** End of File\n\n*** End Patch\n",
        );
        assert_eq!(hunks.len(), 1);
        assert!(hunks[0].at_eof);
        assert_eq!(
            hunks[0].lines.len(),
            1,
            "blank separator must not append a context line to the EOF hunk"
        );
    }

    // -- Hunk application --------------------------------------------------

    #[test]
    fn should_apply_hunks_sequentially_when_same_block_repeats() {
        let ops = parse_patch_envelope(
            "*** Begin Patch\n*** Update File: f\n@@\n-a\n+A1\n@@\n-a\n+A2\n*** End Patch\n",
        )
        .expect("valid envelope");
        let PatchOp::Update { hunks, .. } = &ops[0] else {
            panic!("expected Update");
        };
        let out = apply_codex_hunks("a\nb\na\nb\n", hunks).expect("hunks apply");
        assert_eq!(out, "A1\nb\nA2\nb\n");
    }

    #[test]
    fn should_locate_hunk_after_anchor_when_anchor_given() {
        let content = "fn one() {\n    x\n}\nfn two() {\n    x\n}\n";
        let ops = parse_patch_envelope(
            "*** Begin Patch\n*** Update File: f\n@@ fn two() {\n-    x\n+    y\n*** End Patch\n",
        )
        .expect("valid envelope");
        let PatchOp::Update { hunks, .. } = &ops[0] else {
            panic!("expected Update");
        };
        let out = apply_codex_hunks(content, hunks).expect("hunks apply");
        assert_eq!(out, "fn one() {\n    x\n}\nfn two() {\n    y\n}\n");
    }

    #[test]
    fn should_append_at_eof_when_end_of_file_marker() {
        let ops = parse_patch_envelope(
            "*** Begin Patch\n*** Update File: f\n@@\n+c\n*** End of File\n*** End Patch\n",
        )
        .expect("valid envelope");
        let PatchOp::Update { hunks, .. } = &ops[0] else {
            panic!("expected Update");
        };
        assert!(hunks[0].at_eof);
        let out = apply_codex_hunks("a\nb\n", hunks).expect("hunks apply");
        assert_eq!(out, "a\nb\nc\n");
    }

    #[test]
    fn should_error_with_hunk_number_when_context_not_found() {
        let ops = parse_patch_envelope(
            "*** Begin Patch\n*** Update File: f\n@@\n-not there\n+x\n*** End Patch\n",
        )
        .expect("valid envelope");
        let PatchOp::Update { hunks, .. } = &ops[0] else {
            panic!("expected Update");
        };
        let err = apply_codex_hunks("a\nb\n", hunks).expect_err("must fail to locate");
        assert!(err.contains("hunk 1"), "got: {err}");
        assert!(err.contains("not there"), "got: {err}");
    }

    #[test]
    fn should_reject_pure_insertion_when_no_context_and_file_nonempty() {
        // The common model-emitted shape "add an import": a bare '+' hunk
        // with no '@@' anchor and no '*** End of File'. Against a non-empty
        // file the position is ambiguous — this implementation deliberately
        // rejects it with guidance instead of guessing.
        let hunks =
            parse_update_hunks("*** Begin Patch\n*** Update File: f\n+import os\n*** End Patch\n");
        let err =
            apply_codex_hunks("a\nb\n", &hunks).expect_err("ambiguous insertion must be rejected");
        assert!(err.contains("hunk 1 has no context lines"), "got: {err}");
        assert!(err.contains("@@ <anchor>"), "got: {err}");
        assert!(err.contains("End of File"), "got: {err}");
    }

    #[test]
    fn should_insert_after_anchor_when_hunk_is_pure_insertion() {
        let hunks = parse_update_hunks(
            "*** Begin Patch\n*** Update File: f\n@@ two\n+two.5\n*** End Patch\n",
        );
        let out = apply_codex_hunks("one\ntwo\nthree\n", &hunks).expect("hunks apply");
        assert_eq!(out, "one\ntwo\ntwo.5\nthree\n");
    }

    #[test]
    fn should_insert_at_start_when_pure_insertion_targets_empty_file() {
        let hunks = parse_update_hunks(
            "*** Begin Patch\n*** Update File: f\n@@\n+first\n+second\n*** End Patch\n",
        );
        let out = apply_codex_hunks("", &hunks).expect("hunks apply");
        assert_eq!(out, "first\nsecond");
    }

    #[test]
    fn should_error_with_search_start_when_anchor_not_found() {
        let hunks = parse_update_hunks(
            "*** Begin Patch\n*** Update File: f\n@@\n-a\n+A\n@@ missing anchor\n-b\n+B\n*** End Patch\n",
        );
        let err = apply_codex_hunks("a\nb\n", &hunks).expect_err("anchor must not be found");
        assert!(err.contains("hunk 2"), "got: {err}");
        assert!(
            err.contains("context marker '@@ missing anchor' not found"),
            "got: {err}"
        );
        // Hunk 1 consumed line 1, so the anchor search starts at line 2.
        assert!(err.contains("searched from line 2"), "got: {err}");
    }

    #[test]
    fn should_match_anchor_when_indentation_drifts() {
        // The anchor in the patch lacks the file's leading indentation — the
        // fully-trimmed fallback comparison must still locate the line.
        let content = "class A:\n    def greet(self):\n        pass\n";
        let hunks = parse_update_hunks(
            "*** Begin Patch\n*** Update File: f\n@@ def greet(self):\n-        pass\n+        return 1\n*** End Patch\n",
        );
        let out = apply_codex_hunks(content, &hunks).expect("trimmed anchor fallback must match");
        assert_eq!(out, "class A:\n    def greet(self):\n        return 1\n");
    }

    // -- Tool end-to-end ---------------------------------------------------

    #[tokio::test]
    async fn apply_patch_adds_and_updates_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let tool = ApplyPatchTool::new(temp.path());
        let add = run(
            &tool,
            "*** Begin Patch\n*** Add File: demo.txt\n+hello\n+world\n*** End Patch\n",
        )
        .await;
        assert!(add.success, "{}", add.output);
        assert_eq!(
            std::fs::read_to_string(temp.path().join("demo.txt")).expect("read added"),
            "hello\nworld"
        );

        let update = run(
            &tool,
            "*** Begin Patch\n*** Update File: demo.txt\n@@\n hello\n-world\n+codex\n*** End Patch\n",
        )
        .await;
        assert!(update.success, "{}", update.output);
        assert_eq!(
            std::fs::read_to_string(temp.path().join("demo.txt")).expect("read updated"),
            "hello\ncodex"
        );
    }

    /// #972 / M14-B acceptance: `apply_patch` MUST produce a diff preview
    /// compatible with the AppUI diff flow — `structured_metadata` with
    /// `codex_tool = "apply_patch"`, a `diff_preview` array of `{ op, path }`
    /// entries, and a flat `modified_paths` list.
    #[tokio::test]
    async fn apply_patch_emits_diff_preview_structured_metadata() {
        let temp = tempfile::tempdir().expect("tempdir");
        let tool = ApplyPatchTool::new(temp.path());
        let result = run(
            &tool,
            "*** Begin Patch\n*** Add File: hello.txt\n+hello\n+world\n*** End Patch\n",
        )
        .await;
        assert!(result.success, "apply_patch must succeed on Add File");
        let meta = result
            .structured_metadata
            .as_ref()
            .expect("apply_patch must emit structured_metadata");
        assert_eq!(meta["codex_tool"], json!("apply_patch"));
        assert_eq!(meta["modified_paths"], json!(["hello.txt"]));
        let preview = meta["diff_preview"]
            .as_array()
            .expect("diff_preview must be an array");
        assert_eq!(preview.len(), 1);
        assert_eq!(preview[0]["op"], json!("add"));
        assert_eq!(preview[0]["path"], json!("hello.txt"));
        let contents =
            std::fs::read_to_string(temp.path().join("hello.txt")).expect("created file readable");
        assert!(contents.contains("hello"));
        assert!(contents.contains("world"));
    }

    #[tokio::test]
    async fn should_delete_file_when_patch_contains_delete_section() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(temp.path().join("gone.txt"), "bye\n").unwrap();
        let tool = ApplyPatchTool::new(temp.path());
        let result = run(
            &tool,
            "*** Begin Patch\n*** Delete File: gone.txt\n*** End Patch\n",
        )
        .await;
        assert!(result.success, "{}", result.output);
        assert!(!temp.path().join("gone.txt").exists());
    }

    #[tokio::test]
    async fn should_move_and_update_when_update_section_has_move_to() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            temp.path().join("app.py"),
            "def greet():\n    print(\"Hi\")\n",
        )
        .unwrap();
        let tool = ApplyPatchTool::new(temp.path());
        let result = run(
            &tool,
            "*** Begin Patch\n\
             *** Update File: app.py\n\
             *** Move to: main.py\n\
             @@ def greet():\n\
             -    print(\"Hi\")\n\
             +    print(\"Hello, world!\")\n\
             *** End Patch\n",
        )
        .await;
        assert!(result.success, "{}", result.output);
        assert!(!temp.path().join("app.py").exists(), "source must be gone");
        assert_eq!(
            std::fs::read_to_string(temp.path().join("main.py")).expect("read moved"),
            "def greet():\n    print(\"Hello, world!\")\n"
        );
        let meta = result.structured_metadata.as_ref().expect("metadata");
        assert_eq!(meta["diff_preview"][0]["op"], json!("move"));
        assert_eq!(meta["diff_preview"][0]["path"], json!("main.py"));
        assert_eq!(meta["diff_preview"][0]["from"], json!("app.py"));
        assert_eq!(meta["modified_paths"], json!(["main.py"]));
    }

    #[tokio::test]
    async fn should_rename_file_when_update_has_move_to_and_no_hunks() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(temp.path().join("old.txt"), "same body\n").unwrap();
        let tool = ApplyPatchTool::new(temp.path());
        let result = run(
            &tool,
            "*** Begin Patch\n*** Update File: old.txt\n*** Move to: new/dir/new.txt\n*** End Patch\n",
        )
        .await;
        assert!(result.success, "{}", result.output);
        assert!(!temp.path().join("old.txt").exists());
        assert_eq!(
            std::fs::read_to_string(temp.path().join("new/dir/new.txt")).expect("read moved"),
            "same body\n"
        );
    }

    #[tokio::test]
    async fn should_apply_multi_file_patch_when_sections_span_ops() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(temp.path().join("upd.txt"), "keep\nold\n").unwrap();
        std::fs::write(temp.path().join("del.txt"), "x\n").unwrap();
        std::fs::write(temp.path().join("mv.txt"), "content\n").unwrap();
        let tool = ApplyPatchTool::new(temp.path());
        let result = run(
            &tool,
            "*** Begin Patch\n\
             *** Add File: sub/new.txt\n\
             +fresh\n\
             *** Update File: upd.txt\n\
             @@\n \
             keep\n\
             -old\n\
             +new\n\
             *** Delete File: del.txt\n\
             *** Update File: mv.txt\n\
             *** Move to: moved.txt\n\
             @@\n\
             -content\n\
             +moved content\n\
             *** End Patch\n",
        )
        .await;
        assert!(result.success, "{}", result.output);
        assert_eq!(
            std::fs::read_to_string(temp.path().join("sub/new.txt")).unwrap(),
            "fresh"
        );
        assert_eq!(
            std::fs::read_to_string(temp.path().join("upd.txt")).unwrap(),
            "keep\nnew\n"
        );
        assert!(!temp.path().join("del.txt").exists());
        assert!(!temp.path().join("mv.txt").exists());
        assert_eq!(
            std::fs::read_to_string(temp.path().join("moved.txt")).unwrap(),
            "moved content\n"
        );
        let meta = result.structured_metadata.as_ref().expect("metadata");
        assert_eq!(
            meta["modified_paths"],
            json!(["sub/new.txt", "upd.txt", "del.txt", "moved.txt"])
        );
    }

    #[tokio::test]
    async fn should_apply_add_then_update_same_file_when_patch_chains_ops() {
        let temp = tempfile::tempdir().expect("tempdir");
        let tool = ApplyPatchTool::new(temp.path());
        let result = run(
            &tool,
            "*** Begin Patch\n\
             *** Add File: chain.txt\n\
             +one\n\
             +two\n\
             *** Update File: chain.txt\n\
             @@\n\
             -two\n\
             +three\n\
             *** End Patch\n",
        )
        .await;
        assert!(result.success, "{}", result.output);
        assert_eq!(
            std::fs::read_to_string(temp.path().join("chain.txt")).unwrap(),
            "one\nthree"
        );
    }

    // -- Path safety -------------------------------------------------------

    #[tokio::test]
    async fn should_reject_path_escape_when_path_contains_dotdot() {
        let temp = tempfile::tempdir().expect("tempdir");
        let tool = ApplyPatchTool::new(temp.path());
        let result = run(
            &tool,
            "*** Begin Patch\n*** Add File: ../escape.txt\n+bad\n*** End Patch\n",
        )
        .await;
        assert!(!result.success);
        assert!(result.output.contains("not allowed"), "{}", result.output);
        assert!(!temp.path().parent().unwrap().join("escape.txt").exists());
    }

    #[tokio::test]
    async fn should_reject_absolute_path_when_section_targets_absolute() {
        let temp = tempfile::tempdir().expect("tempdir");
        let tool = ApplyPatchTool::new(temp.path());
        // Even a workspace-INTERNAL absolute path is rejected: envelope paths
        // must be workspace-relative.
        let inside = temp.path().join("inside.txt");
        let patch = format!(
            "*** Begin Patch\n*** Add File: {}\n+bad\n*** End Patch\n",
            inside.display()
        );
        let result = run(&tool, &patch).await;
        assert!(!result.success);
        assert!(
            result.output.contains("absolute paths are not allowed"),
            "{}",
            result.output
        );
        assert!(!inside.exists());
    }

    #[tokio::test]
    async fn should_reject_move_destination_when_move_to_escapes() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(temp.path().join("src.txt"), "body\n").unwrap();
        let tool = ApplyPatchTool::new(temp.path());
        let result = run(
            &tool,
            "*** Begin Patch\n*** Update File: src.txt\n*** Move to: ../stolen.txt\n*** End Patch\n",
        )
        .await;
        assert!(!result.success);
        assert!(result.output.contains("not allowed"), "{}", result.output);
        // Source untouched, nothing escaped.
        assert!(temp.path().join("src.txt").exists());
        assert!(!temp.path().parent().unwrap().join("stolen.txt").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn should_reject_symlink_when_update_targets_symlink() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("outside dir");
        let target = outside.path().join("real.txt");
        std::fs::write(&target, "secret\n").unwrap();
        symlink(&target, temp.path().join("link.txt")).unwrap();
        let tool = ApplyPatchTool::new(temp.path());
        let result = run(
            &tool,
            "*** Begin Patch\n*** Update File: link.txt\n@@\n-secret\n+patched\n*** End Patch\n",
        )
        .await;
        assert!(!result.success, "{}", result.output);
        assert!(
            result.output.contains("Symlink"),
            "expected symlink rejection, got: {}",
            result.output
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "secret\n");
    }

    // -- Validation-phase atomicity ---------------------------------------

    #[tokio::test]
    async fn should_not_modify_workspace_when_validation_fails() {
        let temp = tempfile::tempdir().expect("tempdir");
        let tool = ApplyPatchTool::new(temp.path());
        // Section 1 (Add) is valid; section 2 (Update on a missing file) is
        // not. NOTHING may be applied.
        let result = run(
            &tool,
            "*** Begin Patch\n\
             *** Add File: ok.txt\n\
             +fine\n\
             *** Update File: missing.txt\n\
             @@\n\
             -a\n\
             +b\n\
             *** End Patch\n",
        )
        .await;
        assert!(!result.success);
        assert!(result.output.contains("section 2"), "{}", result.output);
        assert!(result.output.contains("missing.txt"), "{}", result.output);
        assert!(
            result.output.contains("No files were modified"),
            "{}",
            result.output
        );
        assert!(
            !temp.path().join("ok.txt").exists(),
            "validation failure must not apply earlier sections"
        );
    }

    #[tokio::test]
    async fn should_reject_when_add_target_exists() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(temp.path().join("dup.txt"), "already here\n").unwrap();
        let tool = ApplyPatchTool::new(temp.path());
        let result = run(
            &tool,
            "*** Begin Patch\n*** Add File: dup.txt\n+clobber\n*** End Patch\n",
        )
        .await;
        assert!(!result.success);
        assert!(
            result.output.contains("already exists"),
            "{}",
            result.output
        );
        assert_eq!(
            std::fs::read_to_string(temp.path().join("dup.txt")).unwrap(),
            "already here\n"
        );
    }

    #[tokio::test]
    async fn should_reject_when_delete_target_missing() {
        let temp = tempfile::tempdir().expect("tempdir");
        let tool = ApplyPatchTool::new(temp.path());
        let result = run(
            &tool,
            "*** Begin Patch\n*** Delete File: nope.txt\n*** End Patch\n",
        )
        .await;
        assert!(!result.success);
        assert!(result.output.contains("not found"), "{}", result.output);
    }

    #[tokio::test]
    async fn should_reject_when_move_destination_exists() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(temp.path().join("a.txt"), "a\n").unwrap();
        std::fs::write(temp.path().join("b.txt"), "b\n").unwrap();
        let tool = ApplyPatchTool::new(temp.path());
        let result = run(
            &tool,
            "*** Begin Patch\n*** Update File: a.txt\n*** Move to: b.txt\n*** End Patch\n",
        )
        .await;
        assert!(!result.success);
        assert!(
            result.output.contains("already exists"),
            "{}",
            result.output
        );
        assert_eq!(
            std::fs::read_to_string(temp.path().join("b.txt")).unwrap(),
            "b\n"
        );
        assert!(temp.path().join("a.txt").exists());
    }

    #[tokio::test]
    async fn should_report_failing_section_when_hunk_context_missing() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(temp.path().join("real.txt"), "actual content\n").unwrap();
        let tool = ApplyPatchTool::new(temp.path());
        let result = run(
            &tool,
            "*** Begin Patch\n*** Update File: real.txt\n@@\n-imaginary\n+x\n*** End Patch\n",
        )
        .await;
        assert!(!result.success);
        assert!(result.output.contains("section 1"), "{}", result.output);
        assert!(result.output.contains("real.txt"), "{}", result.output);
        assert!(result.output.contains("hunk 1"), "{}", result.output);
        assert_eq!(
            std::fs::read_to_string(temp.path().join("real.txt")).unwrap(),
            "actual content\n"
        );
    }

    // -- Mid-apply failure reporting --------------------------------------

    #[tokio::test]
    async fn should_report_applied_sections_when_apply_fails_midway() {
        let temp = tempfile::tempdir().expect("tempdir");
        // `blocker` is a regular FILE, so `blocker/child.txt` passes the
        // existence validation (stat fails with NotADirectory ⇒ absent) but
        // the apply-phase create_dir_all fails — a genuine mid-patch failure.
        std::fs::write(temp.path().join("blocker"), "i am a file\n").unwrap();
        let tool = ApplyPatchTool::new(temp.path());
        let result = run(
            &tool,
            "*** Begin Patch\n\
             *** Add File: ok.txt\n\
             +fine\n\
             *** Add File: blocker/child.txt\n\
             +never lands\n\
             *** End Patch\n",
        )
        .await;
        assert!(!result.success);
        assert!(
            result.output.contains("Patch failed at section 2"),
            "{}",
            result.output
        );
        assert!(
            result.output.contains("Already applied"),
            "{}",
            result.output
        );
        assert!(result.output.contains("added ok.txt"), "{}", result.output);
        // Section 1 really was applied — partial state is reported, not
        // rolled back.
        assert_eq!(
            std::fs::read_to_string(temp.path().join("ok.txt")).unwrap(),
            "fine"
        );
        let meta = result.structured_metadata.as_ref().expect("metadata");
        assert_eq!(meta["failed_section"], json!(2));
        assert_eq!(meta["modified_paths"], json!(["ok.txt"]));
        assert_eq!(meta["partial_paths"], json!([]));
    }

    #[tokio::test]
    async fn should_report_unchanged_when_first_section_fails_cleanly() {
        let temp = tempfile::tempdir().expect("tempdir");
        // `blocker` is a regular file, so validation passes (stat of
        // blocker/child.txt ⇒ absent) but the apply-phase create_dir_all
        // fails BEFORE the target file could be touched — a genuinely clean
        // first-section failure.
        std::fs::write(temp.path().join("blocker"), "i am a file\n").unwrap();
        let tool = ApplyPatchTool::new(temp.path());
        let result = run(
            &tool,
            "*** Begin Patch\n*** Add File: blocker/child.txt\n+never lands\n*** End Patch\n",
        )
        .await;
        assert!(!result.success);
        assert!(
            result.output.contains("Patch failed at section 1"),
            "{}",
            result.output
        );
        assert!(
            result
                .output
                .contains("No sections were applied; the workspace is unchanged."),
            "{}",
            result.output
        );
        assert!(result.file_modified.is_none());
        let meta = result.structured_metadata.as_ref().expect("metadata");
        assert_eq!(meta["failed_section"], json!(1));
        assert_eq!(meta["modified_paths"], json!([]));
        assert_eq!(meta["partial_paths"], json!([]));
    }

    /// Honesty on the Move partial-state shape: the destination was written
    /// but the source removal failed. The result must NOT claim an unchanged
    /// workspace, and `modified_paths` must list the created file.
    #[cfg(unix)]
    #[tokio::test]
    async fn should_report_partial_move_when_source_removal_fails() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("tempdir");
        let locked = temp.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::write(locked.join("src.txt"), "body\n").unwrap();
        // Read-only directory: validation can still read the source and the
        // apply phase can write the destination (workspace root), but
        // unlinking the source fails.
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
        if std::fs::write(locked.join("probe.tmp"), b"x").is_ok() {
            // Running as root (or an ACL overrides the mode bits) — the
            // setup cannot force the removal failure; skip.
            let _ = std::fs::remove_file(locked.join("probe.tmp"));
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
            return;
        }

        let tool = ApplyPatchTool::new(temp.path());
        let result = run(
            &tool,
            "*** Begin Patch\n*** Update File: locked/src.txt\n*** Move to: dest.txt\n*** End Patch\n",
        )
        .await;

        // Restore permissions so the tempdir can clean up.
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert!(!result.success);
        assert!(
            result.output.contains("Patch failed at section 1"),
            "{}",
            result.output
        );
        assert!(
            result
                .output
                .contains("wrote dest.txt but failed to remove locked/src.txt"),
            "{}",
            result.output
        );
        // The destination really exists — partial state, honestly reported.
        assert_eq!(
            std::fs::read_to_string(temp.path().join("dest.txt")).expect("dest was written"),
            "body\n"
        );
        assert!(
            !result.output.contains("workspace is unchanged"),
            "must not claim an unchanged workspace: {}",
            result.output
        );
        assert!(
            result
                .output
                .contains("The failed section left partial state on disk: dest.txt (written)"),
            "{}",
            result.output
        );
        assert_eq!(
            result.file_modified.as_deref(),
            Some(temp.path().join("dest.txt").as_path())
        );
        let meta = result.structured_metadata.as_ref().expect("metadata");
        assert_eq!(meta["failed_section"], json!(1));
        assert_eq!(meta["modified_paths"], json!(["dest.txt"]));
        assert_eq!(meta["partial_paths"], json!(["dest.txt"]));
    }

    // -- Modes and fallbacks ----------------------------------------------

    #[tokio::test]
    async fn should_refuse_when_file_access_read_only() {
        let temp = tempfile::tempdir().expect("tempdir");
        let tool = ApplyPatchTool::new(temp.path()).with_file_access(FileAccessMode::ReadOnly);
        let result = run(
            &tool,
            "*** Begin Patch\n*** Add File: a.txt\n+x\n*** End Patch\n",
        )
        .await;
        assert!(!result.success);
        assert!(result.output.contains("read-only"), "{}", result.output);
        assert!(!temp.path().join("a.txt").exists());
    }

    #[tokio::test]
    async fn should_apply_unified_diff_when_path_and_diff_given() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(temp.path().join("f.txt"), "line1\nline2\n").unwrap();
        let tool = ApplyPatchTool::new(temp.path());
        let result = tool
            .execute(&json!({
                "path": "f.txt",
                "diff": "@@ -1,2 +1,2 @@\n line1\n-line2\n+line2_new\n"
            }))
            .await
            .expect("apply diff");
        assert!(result.success, "{}", result.output);
        assert_eq!(
            std::fs::read_to_string(temp.path().join("f.txt")).unwrap(),
            "line1\nline2_new\n"
        );
    }

    #[tokio::test]
    async fn should_preserve_trailing_newline_when_updating() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(temp.path().join("nl.txt"), "a\nb\n").unwrap();
        let tool = ApplyPatchTool::new(temp.path());
        let result = run(
            &tool,
            "*** Begin Patch\n*** Update File: nl.txt\n@@\n a\n-b\n+B\n*** End Patch\n",
        )
        .await;
        assert!(result.success, "{}", result.output);
        assert_eq!(
            std::fs::read_to_string(temp.path().join("nl.txt")).unwrap(),
            "a\nB\n"
        );
    }

    #[tokio::test]
    async fn should_invalidate_file_state_cache_when_patch_touches_files() {
        use crate::file_state_cache::{CacheEntry, FileStateCache};
        use std::sync::Arc;
        use std::time::SystemTime;

        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(temp.path().join("upd.txt"), "old\n").unwrap();
        std::fs::write(temp.path().join("del.txt"), "x\n").unwrap();
        let upd_path = temp.path().join("upd.txt");
        let del_path = temp.path().join("del.txt");

        let cache = Arc::new(FileStateCache::new());
        for p in [&upd_path, &del_path] {
            cache.put(CacheEntry::new(
                p.clone(),
                SystemTime::now(),
                0xABCD,
                1,
                false,
                None,
            ));
        }
        assert_eq!(cache.len(), 2);

        let mut ctx = ToolContext::zero();
        ctx.file_state_cache = Some(cache.clone());
        let tool = ApplyPatchTool::new(temp.path());
        let result = tool
            .execute_with_context(
                &ctx,
                &json!({
                    "patch": "*** Begin Patch\n*** Update File: upd.txt\n@@\n-old\n+new\n*** Delete File: del.txt\n*** End Patch\n"
                }),
            )
            .await
            .expect("apply");
        assert!(result.success, "{}", result.output);
        assert!(cache.peek(&upd_path).is_none(), "update must invalidate");
        assert!(cache.peek(&del_path).is_none(), "delete must invalidate");
    }

    #[tokio::test]
    async fn should_use_scope_workspace_when_session_scope_present() {
        use octos_core::SessionScope;
        use std::sync::Arc;

        let scope_dir = tempfile::tempdir().unwrap();
        let legacy_dir = tempfile::tempdir().unwrap();
        let scope = SessionScope::solo(scope_dir.path().to_path_buf(), vec![]).unwrap();
        let tool = ApplyPatchTool::new(legacy_dir.path());
        let mut ctx = ToolContext::zero();
        ctx.session_scope = Some(Arc::new(scope));

        let result = tool
            .execute_with_context(
                &ctx,
                &json!({
                    "patch": "*** Begin Patch\n*** Add File: out.txt\n+hi\n*** End Patch\n"
                }),
            )
            .await
            .expect("apply");
        assert!(result.success, "{}", result.output);
        // File landed in scope.workspace(), NOT the legacy base_dir.
        assert!(scope_dir.path().join("out.txt").exists());
        assert!(!legacy_dir.path().join("out.txt").exists());
    }
}
