/// Non-git fallback driven by authoritative writer metadata. We never infer a
/// create from the post-write filesystem: overwrites and deletes require the
/// bounded previous text captured before the mutation.
fn render_non_git_file_mutation_diff(
    notice: &UiFileMutationNotice,
    workspace_root: &Path,
    absolute_path: &Path,
) -> Option<String> {
    // Tool-reported paths are not authority to read outside the session. Resolve
    // both sides, including symlinked workspace roots (e.g. macOS /tmp).
    let canonical_root = workspace_root.canonicalize().ok()?;
    let current = match absolute_path.canonicalize() {
        Ok(canonical_path) => {
            canonical_path.strip_prefix(&canonical_root).ok()?;
            // A binary, oversized or unreadable existing file is not a deletion.
            Some(read_diff_preview_text(&canonical_path)?)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // A deleted leaf cannot be canonicalized. Require its real parent
            // to remain inside the workspace and reject dangling symlinks.
            if absolute_path.symlink_metadata().err()?.kind() != std::io::ErrorKind::NotFound {
                return None;
            }
            absolute_path
                .parent()?
                .canonicalize()
                .ok()?
                .strip_prefix(&canonical_root)
                .ok()?;
            None
        }
        Err(_) => return None,
    };
    match (notice.created, notice.previous_content.as_deref(), current) {
        (Some(true), _, Some(current)) => render_no_index_text_diff("", &current, &notice.path),
        (_, Some(previous), Some(current)) => {
            render_no_index_text_diff(previous, &current, &notice.path)
        }
        (_, Some(previous), None) => render_no_index_text_diff(previous, "", &notice.path),
        _ => None,
    }
}

fn read_diff_preview_text(path: &Path) -> Option<String> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_DIFF_PREVIEW_BYTES as u64 {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    if bytes.contains(&0) {
        return None;
    }
    String::from_utf8(bytes).ok()
}

fn render_no_index_text_diff(previous: &str, current: &str, display_path: &str) -> Option<String> {
    // Keep the mutation's sanitized path-only preview for control characters;
    // interpolating them into unified headers can forge files or hunks.
    if display_path.chars().any(char::is_control) {
        return None;
    }
    if previous == current
        || previous.len() > MAX_DIFF_PREVIEW_BYTES
        || current.len() > MAX_DIFF_PREVIEW_BYTES
        || previous.contains('\0')
        || current.contains('\0')
    {
        return None;
    }
    let dir = tempfile::tempdir().ok()?;
    let before = dir.path().join("before");
    let after = dir.path().join("after");
    std::fs::write(&before, previous).ok()?;
    std::fs::write(&after, current).ok()?;
    let output = Command::new("git")
        .arg("diff")
        .arg("--no-index")
        .arg("--")
        .arg(&before)
        .arg(&after)
        .output()
        .ok()?;
    let diff = String::from_utf8(output.stdout).ok()?;
    // Git's C-style quoted paths keep spaces (including those in an absolute
    // workspace root) separate from the old/new header delimiter.
    let quoted_path = display_path.replace('\\', "\\\\").replace('"', "\\\"");
    let mut normalized = String::with_capacity(diff.len());
    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            normalized.push_str(&format!(
                "diff --git \"a/{quoted_path}\" \"b/{quoted_path}\""
            ));
        } else if line.starts_with("--- ") {
            normalized.push_str(&format!("--- \"a/{quoted_path}\""));
        } else if line.starts_with("+++ ") {
            normalized.push_str(&format!("+++ \"b/{quoted_path}\""));
        } else {
            normalized.push_str(line);
        }
        normalized.push('\n');
    }
    finish_diff_preview(normalized.into_bytes())
}
