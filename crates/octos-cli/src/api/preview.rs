//! Symlink-safe sites-preview file serve.
//!
//! Codex BLOCKING #1 (issue #996 follow-up): the original fix in
//! `validated_build_output_dir` closed the metadata-parse traversal,
//! but `resolve_preview_asset_path` canonicalised the path and handed
//! it to `tokio::fs::read`, which follows symlinks. An attacker who
//! could swap `<project>/dist` (or any subdir) for a symlink to
//! `/tmp/escape` between the canonical-descendant check and the read
//! would escape the project dir while the validator's check still
//! passed. This module closes that TOCTOU window by re-walking every
//! ancestor of the resolved path with `symlink_metadata` and refusing
//! if any segment is a symlink, plus a final `O_NOFOLLOW` open on
//! the leaf so the swap can't happen between the walk and the read
//! either.
//!
//! Design parallel: `crates/octos-agent/src/tools/read_task_output.rs`
//! `reject_symlinked_ancestors` does the same ancestor walk for the
//! agent's `read_task_output` tool — same shape, different consumer.
//!
//! Cross-platform notes:
//! - Unix: `O_NOFOLLOW` is set on the leaf open. `symlink_metadata`
//!   is used for the ancestor walk.
//! - Windows: `O_NOFOLLOW` does not exist. The ancestor walk and a
//!   pre-open `symlink_metadata` check on the leaf provide the same
//!   guarantee at the cost of a one-line TOCTOU window — mirrors the
//!   fallback pattern in
//!   `crates/octos-agent/src/tools/mod.rs::read_no_follow`.

use std::path::{Path, PathBuf};

/// Reasons a preview serve request was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewServeError {
    /// The canonical resolution of `candidate` lies outside
    /// `project_dir`. Catches symlink targets pointing outside the
    /// scaffold.
    OutsideProject,
    /// One of the ancestor directories of `candidate` (between
    /// `project_dir` and the leaf) is itself a symlink. Catches the
    /// codex BLOCKING-#1 swap: an attacker turns `dist` into a
    /// symlink to `/tmp/escape` after validation but before serve.
    SymlinkedAncestor,
    /// The leaf path is a symlink, or the open with `O_NOFOLLOW`
    /// fails because of one. Catches the leaf-swap variant.
    SymlinkedLeaf,
    /// `std::fs::read` (or the `O_NOFOLLOW` open) failed for a
    /// reason that is NOT symlink-related — typically `NotFound` or
    /// `PermissionDenied`. The caller maps this to HTTP 404.
    NotFound,
}

impl std::fmt::Display for PreviewServeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OutsideProject => write!(f, "preview path escapes the project directory"),
            Self::SymlinkedAncestor => {
                write!(f, "preview path traverses a symlinked directory")
            }
            Self::SymlinkedLeaf => {
                write!(f, "preview path is a symlink and would follow off-tree")
            }
            Self::NotFound => write!(f, "preview asset not found"),
        }
    }
}

impl std::error::Error for PreviewServeError {}

/// Walk every ancestor of `resolved` from `project_root` (exclusive)
/// down to the leaf (exclusive) and refuse if any segment is a
/// symlink. Mirrors `reject_symlinked_ancestors` in
/// `octos-agent/src/tools/read_task_output.rs`. Both `project_root`
/// and `resolved` should be in the same canonical form — we compute
/// both the canonical and lexical roots and try-strip against both
/// to handle the macOS `/var` ↔ `/private/var` firmlink case.
fn reject_symlinked_ancestors(
    project_root: &Path,
    resolved: &Path,
) -> Result<(), PreviewServeError> {
    let canonical_root =
        std::fs::canonicalize(project_root).unwrap_or_else(|_| project_root.to_path_buf());
    let lexical_root = project_root.to_path_buf();

    let (mut current, suffix) = if let Ok(s) = resolved.strip_prefix(&lexical_root) {
        (lexical_root, s.to_path_buf())
    } else if let Ok(s) = resolved.strip_prefix(&canonical_root) {
        (canonical_root, s.to_path_buf())
    } else {
        // resolved is not under either root spelling — caller already
        // checked descendant via canonicalise, so getting here means
        // canonical forms disagree. Refuse defensively.
        return Err(PreviewServeError::OutsideProject);
    };

    let comps: Vec<_> = suffix.components().collect();
    if comps.is_empty() {
        // The resolved path IS the project root. Nothing to walk;
        // the descendant check is the gate, not us.
        return Ok(());
    }

    // Stop one short of the leaf — leaf check is done separately
    // (and on Unix by `O_NOFOLLOW` at open time).
    for comp in &comps[..comps.len().saturating_sub(1)] {
        current.push(comp.as_os_str());
        match std::fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(PreviewServeError::SymlinkedAncestor);
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Some ancestor doesn't exist — caller's open will
                // 404 in a moment. Not a security issue.
                return Ok(());
            }
            Err(_) => {
                // Any other stat failure (permission, IO) — treat
                // as not-found to keep error surface scrubbed.
                return Err(PreviewServeError::NotFound);
            }
        }
    }

    Ok(())
}

/// Symlink-safe blocking read of a preview asset. Use this for the
/// test surface; the async wrapper [`serve_preview_no_follow`] is
/// the production entry-point. Returns the file bytes on success.
///
/// Phases:
/// 1. Canonical-descendant check of `candidate` against `project_root`.
/// 2. Ancestor walk: every directory between `project_root` and the
///    leaf must NOT be a symlink (re-checks the canonical-descendant
///    guarantee under TOCTOU).
/// 3. Leaf open with `O_NOFOLLOW` (Unix) or a pre-open
///    `symlink_metadata` check (non-Unix).
pub fn serve_preview_no_follow_blocking(
    project_root: &Path,
    candidate: &Path,
) -> Result<Vec<u8>, PreviewServeError> {
    let canonical_root = std::fs::canonicalize(project_root).map_err(|_| {
        // Project root vanished — caller should have already checked
        // this; refuse rather than serve.
        PreviewServeError::NotFound
    })?;
    let canonical_candidate = std::fs::canonicalize(candidate).map_err(|_| {
        // The path the agent wants to serve doesn't resolve. Could
        // be a missing file OR a symlink-loop. Either way, refuse —
        // we don't want to expose the difference.
        PreviewServeError::NotFound
    })?;

    if canonical_candidate == canonical_root || !canonical_candidate.starts_with(&canonical_root) {
        return Err(PreviewServeError::OutsideProject);
    }

    // Ancestor walk on the canonical path. The previous validation
    // step canonicalised, so by definition no ancestor here is a
    // symlink at this exact instant — but a malicious build step
    // could swap one in between then and now. The walk runs again
    // here to keep the TOCTOU window down to one syscall.
    reject_symlinked_ancestors(&canonical_root, &canonical_candidate)?;

    read_leaf_no_follow(&canonical_candidate)
}

#[cfg(unix)]
fn read_leaf_no_follow(path: &Path) -> Result<Vec<u8>, PreviewServeError> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;

    let mut opts = std::fs::OpenOptions::new();
    opts.read(true).custom_flags(libc::O_NOFOLLOW);
    let mut file = match opts.open(path) {
        Ok(f) => f,
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => {
            return Err(PreviewServeError::SymlinkedLeaf);
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(PreviewServeError::NotFound);
        }
        Err(_) => return Err(PreviewServeError::NotFound),
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|_| PreviewServeError::NotFound)?;
    Ok(bytes)
}

#[cfg(not(unix))]
fn read_leaf_no_follow(path: &Path) -> Result<Vec<u8>, PreviewServeError> {
    // Non-Unix fallback: stat the leaf before the read. There is a
    // one-syscall TOCTOU window here that does not exist on Unix —
    // mirrors `read_no_follow` in `octos-agent/src/tools/mod.rs`.
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(PreviewServeError::SymlinkedLeaf);
        }
        Ok(_) => {}
        Err(_) => return Err(PreviewServeError::NotFound),
    }
    std::fs::read(path).map_err(|_| PreviewServeError::NotFound)
}

/// Async wrapper around [`serve_preview_no_follow_blocking`].
/// Offloads the blocking read to `tokio::task::spawn_blocking` so
/// the handler doesn't park the runtime on a large asset.
pub async fn serve_preview_no_follow(
    project_root: PathBuf,
    candidate: PathBuf,
) -> Result<Vec<u8>, PreviewServeError> {
    tokio::task::spawn_blocking(move || serve_preview_no_follow_blocking(&project_root, &candidate))
        .await
        .unwrap_or(Err(PreviewServeError::NotFound))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serves_regular_file_inside_project() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path();
        let dist = project.join("dist");
        std::fs::create_dir_all(&dist).unwrap();
        std::fs::write(dist.join("index.html"), b"<html>ok</html>").unwrap();

        let served = serve_preview_no_follow_blocking(project, &dist.join("index.html"))
            .expect("regular file must serve");
        assert_eq!(served, b"<html>ok</html>");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_leaf_symlink() {
        use std::os::unix::fs::symlink;
        let tmp_project = tempfile::tempdir().unwrap();
        let tmp_outside = tempfile::tempdir().unwrap();
        let dist = tmp_project.path().join("dist");
        std::fs::create_dir_all(&dist).unwrap();
        std::fs::write(tmp_outside.path().join("secret"), b"PRIVATE").unwrap();
        let leaf = dist.join("malicious.html");
        symlink(tmp_outside.path().join("secret"), &leaf).unwrap();

        let result = serve_preview_no_follow_blocking(tmp_project.path(), &leaf);
        assert!(
            matches!(
                result,
                Err(PreviewServeError::SymlinkedLeaf | PreviewServeError::OutsideProject)
            ),
            "leaf symlink must be rejected, got: {result:?}",
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_ancestor_symlink() {
        use std::os::unix::fs::symlink;

        // Build a layout where `<project>/dist/sub/leaf.html` exists
        // legitimately, then symlink-swap an interior directory
        // (`sub`) for a directory outside the project.
        let tmp_project = tempfile::tempdir().unwrap();
        let tmp_outside = tempfile::tempdir().unwrap();
        let dist = tmp_project.path().join("dist");
        std::fs::create_dir_all(&dist).unwrap();

        let evil = tmp_outside.path().join("evil");
        std::fs::create_dir_all(&evil).unwrap();
        std::fs::write(evil.join("leaf.html"), b"<html>evil</html>").unwrap();

        let sub = dist.join("sub");
        symlink(&evil, &sub).unwrap();

        let candidate = dist.join("sub").join("leaf.html");
        let result = serve_preview_no_follow_blocking(tmp_project.path(), &candidate);
        assert!(
            matches!(
                result,
                Err(PreviewServeError::SymlinkedAncestor | PreviewServeError::OutsideProject)
            ),
            "ancestor symlink must be rejected, got: {result:?}",
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_swapped_after_validation() {
        use std::os::unix::fs::symlink;

        // Reproduces the BLOCKING #1 scenario end-to-end on this
        // module alone: build a real dist/, take canonical paths
        // (mimicking the validator's output), then symlink-swap
        // dist/ for an outside dir before calling serve.
        let tmp_project = tempfile::tempdir().unwrap();
        let tmp_outside = tempfile::tempdir().unwrap();
        let dist = tmp_project.path().join("dist");
        std::fs::create_dir_all(&dist).unwrap();
        std::fs::write(dist.join("index.html"), b"<html>real</html>").unwrap();

        // Take the validated path (canonical) NOW, before the swap.
        let validated = std::fs::canonicalize(&dist).unwrap();
        let leaf = validated.join("index.html");

        // Swap.
        std::fs::write(tmp_outside.path().join("index.html"), b"SECRET").unwrap();
        std::fs::remove_dir_all(&dist).unwrap();
        symlink(tmp_outside.path(), &dist).unwrap();

        // Serve must refuse — the canonical form of `leaf` now
        // points outside `project_root`, OR an ancestor is now a
        // symlink. Either gate is acceptable.
        let result = serve_preview_no_follow_blocking(tmp_project.path(), &leaf);
        assert!(
            matches!(
                result,
                Err(PreviewServeError::SymlinkedAncestor
                    | PreviewServeError::OutsideProject
                    | PreviewServeError::NotFound)
            ),
            "post-swap serve must refuse, got: {result:?}",
        );
    }
}
