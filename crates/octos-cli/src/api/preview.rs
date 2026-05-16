//! Symlink-safe sites-preview file serve.
//!
//! Codex round-1 BLOCKING #1 (issue #996 follow-up): the original fix
//! in `validated_build_output_dir` closed the metadata-parse
//! traversal, but `resolve_preview_asset_path` canonicalised the path
//! and handed it to `tokio::fs::read`, which follows symlinks. An
//! attacker who could swap `<project>/dist` (or any subdir) for a
//! symlink to `/tmp/escape` between the canonical-descendant check
//! and the read would escape the project dir while the validator's
//! check still passed.
//!
//! Round-1 closed the leaf swap (`O_NOFOLLOW`) and added a
//! `symlink_metadata` ancestor walk, but that left a multi-syscall
//! TOCTOU window: an attacker could swap an ancestor between the
//! `symlink_metadata` stat and the open. Codex round-2 BLOCKING #1
//! (this module): walk every component with `rustix::fs::openat` so
//! each step is anchored to a parent fd that already passed
//! `O_NOFOLLOW`. After the walk completes, neither the leaf nor any
//! ancestor between `project_dir` and the leaf can be substituted
//! by an unobserved symlink — the open of component `n+1` happens
//! relative to the fd we opened at step `n`, so even a mid-walk
//! swap of the on-disk path can't redirect the next `openat`.
//!
//! Design parallel: `crates/octos-agent/src/tools/read_task_output.rs`
//! `reject_symlinked_ancestors` does a metadata walk for the agent's
//! `read_task_output` tool; we use the component-by-component openat
//! shape here because the preview handler is exposed to LLM-controlled
//! metadata (`build_output_dir`) AND OS-level filesystem races, while
//! `read_task_output` operates on canonical workspace paths produced
//! by the agent itself.
//!
//! Cross-platform notes:
//! - Unix (Linux + macOS): every component is opened with
//!   `O_NOFOLLOW | O_DIRECTORY` (`O_NOFOLLOW` alone on the leaf) via
//!   `rustix::fs::openat`. No metadata pre-check is needed because
//!   the open enforces the same property atomically. Workspace-wide
//!   `deny(unsafe_code)` rules out raw `libc::openat`.
//! - Windows: `O_NOFOLLOW` does not exist. The ancestor walk uses
//!   `symlink_metadata` and the leaf is opened after a pre-open
//!   `symlink_metadata` check. There is a residual multi-syscall
//!   TOCTOU window here that does not exist on Unix — see the PR
//!   body and the `#[cfg(not(unix))]` fallback below. Mirrors the
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

/// Compute the path of `candidate` relative to `project_root`. Both
/// inputs are expected to have been canonicalised by the caller —
/// because canonical roots can diverge across firmlinks (macOS
/// `/var` ↔ `/private/var`), we try-strip against both the lexical
/// and canonical spellings of `project_root` here.
fn relative_under_root(
    project_root: &Path,
    candidate: &Path,
) -> Result<PathBuf, PreviewServeError> {
    let canonical_root =
        std::fs::canonicalize(project_root).unwrap_or_else(|_| project_root.to_path_buf());
    if let Ok(rel) = candidate.strip_prefix(project_root) {
        return Ok(rel.to_path_buf());
    }
    if let Ok(rel) = candidate.strip_prefix(&canonical_root) {
        return Ok(rel.to_path_buf());
    }
    Err(PreviewServeError::OutsideProject)
}

/// Symlink-safe blocking read of a preview asset. Use this for the
/// test surface; the async wrapper [`serve_preview_no_follow`] is the
/// production entry-point. Returns the file bytes on success.
///
/// Phases:
/// 1. Canonical-descendant check of `candidate` against `project_root`.
/// 2. Strip `project_root` prefix to get the asset's relative path
///    (e.g. `dist/index.html`).
/// 3. Walk every component of the relative path with
///    [`open_no_follow_walk`] — each step opens a child fd from the
///    previous fd using `O_NOFOLLOW`, so the parent-child relationship
///    is enforced atomically per-component. Even if an attacker
///    swaps any path component for a symlink mid-walk, the swap
///    cannot be observed by the next `openat` because the next
///    `openat` operates on the fd, not the path string.
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

    // Compute the asset's relative path from the canonical root. The
    // walk below traverses it component-by-component.
    let relative = relative_under_root(&canonical_root, &canonical_candidate)?;

    open_no_follow_walk(&canonical_root, &relative)
}

/// Walk `relative` component-by-component starting from
/// `project_dir`, using `openat(O_NOFOLLOW)` on Unix so each
/// intermediate fd is anchored to the previous fd. Returns the bytes
/// of the final leaf file.
///
/// The walk's atomicity is what makes this resistant to a mid-walk
/// ancestor swap. The previous round-1 implementation called
/// `symlink_metadata(path)` on each ancestor and then `OpenOptions::open`
/// on the leaf — between those two syscalls, an attacker could swap
/// the on-disk path for a symlink. By contrast, an `openat` call
/// using a parent fd does not consult the path-name resolution
/// table for the parent — the parent fd is already an open handle
/// to the previously-validated directory, so even if the on-disk
/// name now points elsewhere the open relative to the fd still
/// reaches the right inode.
#[cfg(unix)]
fn open_no_follow_walk(project_dir: &Path, relative: &Path) -> Result<Vec<u8>, PreviewServeError> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;

    use rustix::fs::{Mode, OFlags, openat};

    // Open the project root with O_NOFOLLOW | O_DIRECTORY. If the
    // project_dir itself is a symlink we want that to fail here —
    // the caller has already canonicalised, so a symlink here would
    // be a TOCTOU substitution we want to refuse.
    let root_file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(project_dir)
        .map_err(map_open_error)?;
    let mut current: std::os::fd::OwnedFd = root_file.into();

    let comps: Vec<_> = relative.components().collect();
    if comps.is_empty() {
        // The candidate IS the project root. That's `OutsideProject`
        // from the caller's perspective — preview must serve a file
        // under the root, not the root itself.
        return Err(PreviewServeError::OutsideProject);
    }

    // All but the last component must be directories. The last
    // component is the leaf file (we open it without O_DIRECTORY).
    let leaf_idx = comps.len() - 1;
    for (idx, comp) in comps.iter().enumerate() {
        let name = match comp {
            std::path::Component::Normal(s) => s,
            // Any non-Normal component (`.`, `..`, root) was already
            // ruled out by the upstream validation; refusing here is
            // belt-and-braces.
            _ => return Err(PreviewServeError::OutsideProject),
        };

        let flags = if idx == leaf_idx {
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC
        } else {
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::DIRECTORY | OFlags::CLOEXEC
        };

        // `rustix::fs::openat` is a safe wrapper around the
        // `openat(2)` syscall. The new fd is anchored to `current`
        // (the parent fd), not to the path string — so a mid-walk
        // swap of any on-disk name leaves the resolution chain
        // intact: the next openat consults the fd, never re-walks
        // the path.
        let next = openat(&current, *name, flags, Mode::empty())
            .map_err(|err| map_rustix_error(err, idx == leaf_idx))?;
        current = next;
    }

    // `current` is now the leaf fd, opened with O_NOFOLLOW. Std
    // provides a safe `File::from(OwnedFd)` conversion, so no
    // raw-fd or `unsafe` is needed here (workspace lint denies
    // `unsafe_code`).
    let mut file: std::fs::File = current.into();
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|_| PreviewServeError::NotFound)?;
    Ok(bytes)
}

#[cfg(unix)]
fn map_open_error(e: std::io::Error) -> PreviewServeError {
    // `ELOOP` is the kernel's signal that the open hit a symlink it
    // refused to follow because of `O_NOFOLLOW`. Every other failure
    // (NotFound, PermissionDenied, IO) collapses to `NotFound` to
    // keep the error surface uninformative — the caller maps this
    // to HTTP 404.
    if e.raw_os_error() == Some(libc::ELOOP) {
        PreviewServeError::SymlinkedAncestor
    } else {
        PreviewServeError::NotFound
    }
}

#[cfg(unix)]
fn map_rustix_error(err: rustix::io::Errno, is_leaf: bool) -> PreviewServeError {
    if err == rustix::io::Errno::LOOP {
        if is_leaf {
            PreviewServeError::SymlinkedLeaf
        } else {
            PreviewServeError::SymlinkedAncestor
        }
    } else if err == rustix::io::Errno::NOTDIR {
        // A non-final component was not a directory — either the
        // ancestor walk hit a file (impossible if the input was a
        // canonical descendant of `project_dir`) or the path no
        // longer resolves the way it did at canonicalise time. Both
        // are NotFound from the caller's perspective.
        PreviewServeError::NotFound
    } else {
        PreviewServeError::NotFound
    }
}

#[cfg(not(unix))]
fn open_no_follow_walk(project_dir: &Path, relative: &Path) -> Result<Vec<u8>, PreviewServeError> {
    // Non-Unix fallback. `O_NOFOLLOW` and `openat` are not available;
    // we use the round-1 ancestor walk + pre-open `symlink_metadata`
    // check on the leaf. There is a multi-syscall TOCTOU window here
    // — documented in the module docstring and the PR body. Tracking
    // a Windows fix is out of scope for #996 round-2.
    let mut current = project_dir.to_path_buf();
    let comps: Vec<_> = relative.components().collect();
    if comps.is_empty() {
        return Err(PreviewServeError::OutsideProject);
    }
    let leaf_idx = comps.len() - 1;
    for (idx, comp) in comps.iter().enumerate() {
        let name = match comp {
            std::path::Component::Normal(s) => s,
            _ => return Err(PreviewServeError::OutsideProject),
        };
        current.push(name);
        match std::fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(if idx == leaf_idx {
                    PreviewServeError::SymlinkedLeaf
                } else {
                    PreviewServeError::SymlinkedAncestor
                });
            }
            Ok(_) => {}
            Err(_) => return Err(PreviewServeError::NotFound),
        }
    }
    std::fs::read(&current).map_err(|_| PreviewServeError::NotFound)
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
