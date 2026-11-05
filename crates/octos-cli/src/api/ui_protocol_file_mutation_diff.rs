fn materialize_file_mutation_diff(
    notice: &UiFileMutationNotice,
    workspace_root: Option<&Path>,
) -> Option<String> {
    let path = PathBuf::from(&notice.path);
    let absolute_path = if path.is_absolute() {
        path
    } else if let Some(workspace_root) = workspace_root {
        workspace_root.join(path)
    } else {
        std::env::current_dir().ok()?.join(path)
    };
    let Some(git_root) = find_git_root_for_path(&absolute_path) else {
        let root = workspace_root
            .map(Path::to_path_buf)
            .or_else(|| std::env::current_dir().ok())?;
        return render_non_git_file_mutation_diff(notice, &root, &absolute_path);
    };
    let relative_path = absolute_path.strip_prefix(&git_root).ok()?;

    // Primary: working-tree vs index for a TRACKED file — the common edit case.
    let output = Command::new("git")
        .arg("-C")
        .arg(&git_root)
        .arg("diff")
        .arg("--")
        .arg(relative_path)
        .output()
        .ok()?;
    // A non-zero exit is a genuine git error (a corrupt/overridden index, or a
    // path git rejects) — NOT "the file is untracked". Give up rather than
    // mis-render a tracked file as wholly-added through the fallback below.
    if !output.status.success() {
        return None;
    }
    if let Some(diff) = finish_diff_preview(output.stdout) {
        return Some(diff);
    }

    // Fallback: a brand-new file the agent just created is UNTRACKED, so the
    // plain `git diff` above reports nothing and the preview would render an
    // empty "ready" box with no hunks. Render the whole file as additions.
    render_untracked_file_as_additions(&git_root, relative_path, &absolute_path)
}

include!("ui_protocol_non_git_diff.rs");

/// Trim, cap, and drop-if-empty a raw `git diff` stdout into a preview body.
/// `None` means "no renderable diff".
fn finish_diff_preview(stdout: Vec<u8>) -> Option<String> {
    let diff = String::from_utf8(stdout).ok()?;
    let diff = truncate_utf8(diff.trim_end().to_owned(), MAX_DIFF_PREVIEW_BYTES);
    (!diff.is_empty()).then_some(diff)
}

/// Render a newly-created UNTRACKED file as an all-additions diff via
/// `git diff --no-index /dev/null <file>` (read-only — it never touches the
/// index). `--no-index` exits 1 when the two paths differ (the normal case),
/// so a renderable diff is judged by non-empty output rather than the exit
/// code. `/dev/null` is Unix-only; on other platforms the file keeps today's
/// behavior (empty preview, no regression).
#[cfg(unix)]
fn render_untracked_file_as_additions(
    git_root: &Path,
    relative_path: &Path,
    absolute_path: &Path,
) -> Option<String> {
    if !absolute_path.is_file() || !file_is_untracked(git_root, relative_path) {
        return None;
    }
    // Containment guard: this fallback reads the file directly, so a notice
    // path that escapes the repo — a `..` component, or a symlinked directory
    // that the lexical `strip_prefix` accepted — must NOT be read. Resolve BOTH
    // sides (so a symlinked repo root, e.g. macOS `/tmp` -> `/private/tmp`,
    // still matches) and derive the repo-relative path from the *canonical*
    // file. `strip_prefix` succeeds only when the file lives below the real git
    // root, so it doubles as the containment check.
    let canonical_root = git_root.canonicalize().ok()?;
    let canonical_file = absolute_path.canonicalize().ok()?;
    let safe_relative = canonical_file.strip_prefix(&canonical_root).ok()?;
    // Diff the SYMLINK-FREE canonical path (relative to the canonical root) so
    // git does not re-follow a symlink we already resolved, and the preview
    // still shows the tidy repo-relative path.
    //
    // Residual TOCTOU (accepted): a concurrent process could swap one of the
    // now-real parent directories for an outside-pointing symlink between this
    // check and git's open. Closing that fully needs `openat2(RESOLVE_BENEATH)`
    // / a capability-`Dir`, which this `deny(unsafe_code)` crate can't express
    // without a carve-out; in the multi-tenant deployment the sandbox's
    // per-tenant FS isolation bounds that race, and the static `..`/symlink
    // vectors (the reachable ones) are closed above.
    let output = Command::new("git")
        .arg("-C")
        .arg(&canonical_root)
        .arg("diff")
        .arg("--no-index")
        .arg("--")
        .arg("/dev/null")
        .arg(safe_relative)
        .output()
        .ok()?;
    finish_diff_preview(output.stdout)
}

#[cfg(not(unix))]
fn render_untracked_file_as_additions(
    _git_root: &Path,
    _relative_path: &Path,
    _absolute_path: &Path,
) -> Option<String> {
    None
}

/// True when git is not tracking `relative_path` (newly created, never
/// `git add`ed). `git ls-files --error-unmatch` exits 0 when the path IS
/// tracked, 1 when it is not, and 128 on a genuine git error. Treat ONLY the
/// explicit unmatched code (1) as untracked, so a repo-level failure is never
/// mistaken for "new file" (which would wrongly render it as wholly-added).
#[cfg(unix)]
fn file_is_untracked(git_root: &Path, relative_path: &Path) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(git_root)
        .arg("ls-files")
        .arg("--error-unmatch")
        .arg("--")
        .arg(relative_path)
        .output()
        .map(|output| output.status.code() == Some(1))
        .unwrap_or(false)
}

fn find_git_root_for_path(path: &Path) -> Option<PathBuf> {
    let start = if path.is_dir() { path } else { path.parent()? };
    start
        .ancestors()
        .find(|ancestor| ancestor.join(".git").exists())
        .map(Path::to_path_buf)
}
