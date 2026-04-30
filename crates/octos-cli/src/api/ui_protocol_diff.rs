use std::collections::HashMap;
use std::sync::RwLock;

use octos_core::SessionKey;
use octos_core::ui_protocol::{
    DiffPreview, DiffPreviewFile, DiffPreviewFileStatus, DiffPreviewGetParams,
    DiffPreviewGetResult, DiffPreviewGetStatus, DiffPreviewHunk, DiffPreviewLine,
    DiffPreviewLineKind, DiffPreviewSource, PreviewId, RpcError, TurnId, UiFileMutationNotice,
    file_mutation_operations, methods, rpc_error_codes,
};
use serde_json::json;

/// A pending diff-preview entry. Carries both the parsed `DiffPreview` that
/// is shipped to clients and a raw snapshot of the underlying diff bytes
/// captured *at proposal time*. Storing the snapshot in the entry closes
/// the TOCTOU between proposal and apply: subsequent `diff/preview/get`
/// calls return the proposal-time view even if the file on disk has been
/// rewritten between proposal and approval.
#[derive(Debug, Clone)]
pub(super) struct PendingDiffEntry {
    preview: DiffPreview,
    /// Raw unified diff captured at proposal time. `None` when the
    /// runtime did not surface a diff at all (e.g. tool emitted no
    /// `diff` and `materialize_file_mutation_diff` could not produce one).
    /// Used by tests today and by apply-time consistency checks once the
    /// apply path is wired in.
    #[allow(dead_code)]
    snapshot_at_proposal: Option<String>,
}

impl PendingDiffEntry {
    fn new(preview: DiffPreview, snapshot: Option<String>) -> Self {
        Self {
            preview,
            snapshot_at_proposal: snapshot,
        }
    }

    #[cfg(test)]
    pub(super) fn snapshot(&self) -> Option<&str> {
        self.snapshot_at_proposal.as_deref()
    }
}

#[derive(Default)]
pub(super) struct PendingDiffPreviewStore {
    entries: RwLock<HashMap<PreviewId, PendingDiffEntry>>,
}

impl PendingDiffPreviewStore {
    pub(super) fn get(
        &self,
        params: DiffPreviewGetParams,
    ) -> Result<DiffPreviewGetResult, RpcError> {
        let entries = self
            .entries
            .read()
            .expect("pending diff preview store poisoned");
        let Some(entry) = entries.get(&params.preview_id) else {
            return Err(diff_preview_not_found_error(&params));
        };

        if entry.preview.session_id != params.session_id {
            return Err(diff_preview_not_found_error(&params));
        }

        Ok(DiffPreviewGetResult {
            status: DiffPreviewGetStatus::Ready,
            source: DiffPreviewSource::PendingStore,
            preview: entry.preview.clone(),
        })
    }

    #[allow(dead_code)]
    pub(super) fn insert(&self, preview: DiffPreview) {
        self.insert_with_snapshot(preview, None);
    }

    pub(super) fn insert_with_snapshot(
        &self,
        preview: DiffPreview,
        snapshot_at_proposal: Option<String>,
    ) {
        let mut entries = self
            .entries
            .write()
            .expect("pending diff preview store poisoned");
        entries.insert(
            preview.preview_id.clone(),
            PendingDiffEntry::new(preview, snapshot_at_proposal),
        );
    }

    pub(super) fn upsert_file_mutation(
        &self,
        session_id: SessionKey,
        turn_id: &TurnId,
        notice: &mut UiFileMutationNotice,
        diff: Option<&str>,
    ) -> PreviewId {
        let preview_id = notice
            .preview_id
            .clone()
            .unwrap_or_else(|| preview_id_for_file_mutation(&session_id, turn_id, notice));
        notice.preview_id = Some(preview_id.clone());
        self.insert_with_snapshot(
            preview_from_file_mutation(session_id, preview_id.clone(), notice, diff),
            diff.map(ToOwned::to_owned),
        );
        preview_id
    }

    #[cfg(test)]
    pub(super) fn snapshot_for(&self, preview_id: &PreviewId) -> Option<String> {
        self.entries
            .read()
            .expect("pending diff preview store poisoned")
            .get(preview_id)
            .and_then(|entry| entry.snapshot().map(ToOwned::to_owned))
    }
}

fn preview_id_for_file_mutation(
    session_id: &SessionKey,
    turn_id: &TurnId,
    notice: &UiFileMutationNotice,
) -> PreviewId {
    let mut hash = 0xcbf2_9ce4_8422_2325_u128;
    fn feed(hash: &mut u128, bytes: &[u8]) {
        for byte in bytes {
            *hash ^= u128::from(*byte);
            *hash = hash.wrapping_mul(0x100_0000_01b3);
        }
        *hash ^= 0xff;
        *hash = hash.wrapping_mul(0x100_0000_01b3);
    }

    feed(&mut hash, session_id.0.as_bytes());
    feed(&mut hash, turn_id.0.as_bytes());
    feed(&mut hash, notice.path.as_bytes());
    feed(&mut hash, notice.operation.as_bytes());
    if let Some(tool_call_id) = &notice.tool_call_id {
        feed(&mut hash, tool_call_id.as_bytes());
    }
    PreviewId(uuid::Uuid::from_u128(hash))
}

fn preview_from_file_mutation(
    session_id: SessionKey,
    preview_id: PreviewId,
    notice: &UiFileMutationNotice,
    diff: Option<&str>,
) -> DiffPreview {
    let files = diff
        .and_then(parse_unified_diff_preview_files)
        .filter(|files| !files.is_empty())
        .map(|files| files.into_iter().map(sanitize_preview_file).collect())
        .unwrap_or_else(|| vec![file_from_mutation_notice(notice)]);

    let safe_path = super::ui_protocol_sanitize::sanitize_display_path(&notice.path);
    DiffPreview {
        session_id,
        preview_id,
        title: Some(format!("{} {}", notice.operation, safe_path)),
        files,
    }
}

fn file_from_mutation_notice(notice: &UiFileMutationNotice) -> DiffPreviewFile {
    DiffPreviewFile {
        path: super::ui_protocol_sanitize::sanitize_display_path(&notice.path),
        old_path: None,
        status: status_from_operation(&notice.operation),
        hunks: Vec::new(),
    }
}

fn sanitize_preview_file(mut file: DiffPreviewFile) -> DiffPreviewFile {
    file.path = super::ui_protocol_sanitize::sanitize_display_path(&file.path);
    file.old_path = file
        .old_path
        .map(|path| super::ui_protocol_sanitize::sanitize_display_path(&path));
    file
}

fn status_from_operation(operation: &str) -> DiffPreviewFileStatus {
    match operation {
        file_mutation_operations::CREATE | file_mutation_operations::WRITE => {
            DiffPreviewFileStatus::Added
        }
        file_mutation_operations::DELETE => DiffPreviewFileStatus::Deleted,
        _ => DiffPreviewFileStatus::Modified,
    }
}

fn parse_unified_diff_preview_files(diff: &str) -> Option<Vec<DiffPreviewFile>> {
    let mut files = Vec::new();
    let mut current: Option<DiffPreviewFile> = None;
    let mut current_hunk: Option<DiffPreviewHunk> = None;
    let mut old_line = 0_u32;
    let mut new_line = 0_u32;

    for line in diff.lines() {
        if let Some((old_path, new_path)) = line
            .strip_prefix("diff --git ")
            .and_then(parse_diff_git_paths)
        {
            push_hunk(&mut current, &mut current_hunk);
            if let Some(file) = current.take() {
                files.push(file);
            }
            current = Some(DiffPreviewFile {
                path: new_path,
                old_path: Some(old_path),
                status: DiffPreviewFileStatus::Modified,
                hunks: Vec::new(),
            });
            continue;
        }

        let Some(file) = current.as_mut() else {
            continue;
        };

        if line.starts_with("new file mode ") {
            file.status = DiffPreviewFileStatus::Added;
        } else if line.starts_with("deleted file mode ") {
            file.status = DiffPreviewFileStatus::Deleted;
        } else if let Some(path) = line.strip_prefix("rename from ") {
            file.old_path = Some(path.to_string());
            file.status = DiffPreviewFileStatus::Renamed;
        } else if let Some(path) = line.strip_prefix("rename to ") {
            file.path = path.to_string();
            file.status = DiffPreviewFileStatus::Renamed;
        } else if line.starts_with("@@ ") {
            push_hunk(&mut current, &mut current_hunk);
            let (old_start, new_start) = parse_hunk_starts(line).unwrap_or((1, 1));
            old_line = old_start;
            new_line = new_start;
            current_hunk = Some(DiffPreviewHunk {
                header: line.to_string(),
                lines: Vec::new(),
            });
        } else if let Some(hunk) = current_hunk.as_mut() {
            if line.starts_with("--- ") || line.starts_with("+++ ") {
                continue;
            }
            let Some(first) = line.chars().next() else {
                continue;
            };
            match first {
                '+' => {
                    hunk.lines.push(DiffPreviewLine {
                        kind: DiffPreviewLineKind::Added,
                        content: line[1..].to_string(),
                        old_line: None,
                        new_line: Some(new_line),
                    });
                    new_line += 1;
                }
                '-' => {
                    hunk.lines.push(DiffPreviewLine {
                        kind: DiffPreviewLineKind::Removed,
                        content: line[1..].to_string(),
                        old_line: Some(old_line),
                        new_line: None,
                    });
                    old_line += 1;
                }
                ' ' => {
                    hunk.lines.push(DiffPreviewLine {
                        kind: DiffPreviewLineKind::Context,
                        content: line[1..].to_string(),
                        old_line: Some(old_line),
                        new_line: Some(new_line),
                    });
                    old_line += 1;
                    new_line += 1;
                }
                _ => {}
            }
        }
    }

    push_hunk(&mut current, &mut current_hunk);
    if let Some(file) = current {
        files.push(file);
    }
    Some(files)
}

fn parse_diff_git_paths(rest: &str) -> Option<(String, String)> {
    let (old_path, new_path) = rest.split_once(' ')?;
    Some((strip_diff_prefix(old_path), strip_diff_prefix(new_path)))
}

fn strip_diff_prefix(path: &str) -> String {
    path.strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path)
        .to_string()
}

fn parse_hunk_starts(header: &str) -> Option<(u32, u32)> {
    let mut parts = header.split_whitespace();
    parts.next()?;
    let old = parts.next()?.trim_start_matches('-');
    let new = parts.next()?.trim_start_matches('+');
    Some((parse_range_start(old)?, parse_range_start(new)?))
}

fn parse_range_start(range: &str) -> Option<u32> {
    range.split(',').next()?.parse().ok()
}

fn push_hunk(file: &mut Option<DiffPreviewFile>, hunk: &mut Option<DiffPreviewHunk>) {
    if let (Some(file), Some(hunk)) = (file.as_mut(), hunk.take()) {
        file.hunks.push(hunk);
    }
}

fn diff_preview_not_found_error(params: &DiffPreviewGetParams) -> RpcError {
    RpcError::new(
        rpc_error_codes::UNKNOWN_PREVIEW_ID,
        "diff/preview/get target was not found for this session",
    )
    .with_data(json!({
        "kind": "unknown_preview",
        "method": methods::DIFF_PREVIEW_GET,
        "session_id": params.session_id,
        "preview_id": params.preview_id,
        "legacy_kind": "diff_preview_not_found",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use octos_core::ui_protocol::{
        DiffPreviewFile, DiffPreviewFileStatus, DiffPreviewHunk, DiffPreviewLine,
        DiffPreviewLineKind, TurnId, UiFileMutationNotice,
    };

    #[test]
    fn known_diff_preview_returns_stored_preview() {
        let store = PendingDiffPreviewStore::default();
        let session_id = SessionKey("local:test".into());
        let preview_id = PreviewId::new();
        store.insert(DiffPreview {
            session_id: session_id.clone(),
            preview_id: preview_id.clone(),
            title: Some("preview".into()),
            files: vec![DiffPreviewFile {
                path: "src/lib.rs".into(),
                old_path: None,
                status: DiffPreviewFileStatus::Modified,
                hunks: vec![DiffPreviewHunk {
                    header: "@@ -1 +1 @@".into(),
                    lines: vec![DiffPreviewLine {
                        kind: DiffPreviewLineKind::Added,
                        content: "new line".into(),
                        old_line: None,
                        new_line: Some(1),
                    }],
                }],
            }],
        });

        let result = store
            .get(DiffPreviewGetParams {
                session_id,
                preview_id,
            })
            .expect("preview should exist");

        assert_eq!(result.status, DiffPreviewGetStatus::Ready);
        assert_eq!(result.source, DiffPreviewSource::PendingStore);
        assert_eq!(result.preview.files[0].path, "src/lib.rs");
    }

    #[test]
    fn file_mutation_produces_deterministic_preview_from_diff() {
        let store = PendingDiffPreviewStore::default();
        let session_id = SessionKey("local:test".into());
        let turn_id = TurnId::new();
        let mut notice = UiFileMutationNotice::new("src/lib.rs", file_mutation_operations::MODIFY);
        notice.tool_call_id = Some("tool-1".into());

        let diff = "\
diff --git a/src/lib.rs b/src/lib.rs
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,2 +1,2 @@
 fn main() {
-    old();
+    new();
 }
";

        let preview_id =
            store.upsert_file_mutation(session_id.clone(), &turn_id, &mut notice, Some(diff));
        let repeated = store.upsert_file_mutation(
            session_id.clone(),
            &turn_id,
            &mut UiFileMutationNotice {
                preview_id: None,
                ..notice.clone()
            },
            Some(diff),
        );

        assert_eq!(repeated, preview_id);
        assert_eq!(notice.preview_id, Some(preview_id.clone()));

        let result = store
            .get(DiffPreviewGetParams {
                session_id,
                preview_id,
            })
            .expect("preview should be produced from mutation");

        assert_eq!(result.source, DiffPreviewSource::PendingStore);
        assert_eq!(result.preview.files[0].path, "src/lib.rs");
        assert_eq!(
            result.preview.files[0].status,
            DiffPreviewFileStatus::Modified
        );
        assert_eq!(
            result.preview.files[0].hunks[0].lines[1].kind,
            DiffPreviewLineKind::Removed
        );
        assert_eq!(
            result.preview.files[0].hunks[0].lines[2].kind,
            DiffPreviewLineKind::Added
        );
    }

    #[test]
    fn missing_diff_preview_is_typed_not_found() {
        let store = PendingDiffPreviewStore::default();
        let error = store
            .get(DiffPreviewGetParams {
                session_id: SessionKey("local:test".into()),
                preview_id: PreviewId::new(),
            })
            .expect_err("missing preview should fail");

        assert_eq!(error.code, rpc_error_codes::UNKNOWN_PREVIEW_ID);
        assert_eq!(
            error.data.as_ref().and_then(|data| data.get("kind")),
            Some(&json!("unknown_preview"))
        );
    }
}
