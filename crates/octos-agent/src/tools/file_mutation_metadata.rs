//! Bounded process-local metadata used to render non-git file diffs.

use std::path::Path;

const FILE_MUTATION_PREVIEW_MAX_BYTES: usize = 256 * 1024;

/// Keep only bounded UTF-8 text for a non-git preview. Binary and oversized
/// files deliberately omit the previous-content side-channel.
pub fn file_mutation_previous_text(content: String) -> Option<String> {
    (content.len() <= FILE_MUTATION_PREVIEW_MAX_BYTES && !content.contains('\0')).then_some(content)
}

/// Capture an overwrite/delete baseline before mutation without following a
/// symlink. The caller uses this only as preview metadata, never as tool input.
pub async fn read_file_mutation_previous_text(path: &Path) -> Option<String> {
    let metadata = tokio::fs::metadata(path).await.ok()?;
    if !metadata.is_file() || metadata.len() > FILE_MUTATION_PREVIEW_MAX_BYTES as u64 {
        return None;
    }
    file_mutation_previous_text(super::read_no_follow(path).await.ok()?)
}
