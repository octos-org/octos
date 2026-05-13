use std::path::{Component, Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

const PROFILE_HANDLE_PREFIX: &str = "pf";
const UPLOAD_HANDLE_PREFIX: &str = "up";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileHandleScope {
    ProfileRelative(PathBuf),
    TempUpload(PathBuf),
}

/// Scope of a resolved tool-argument path. Lets callers apply
/// scope-specific policy (e.g. read-only for profile files,
/// symlink-safe everywhere) on top of the unified resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolPathScope {
    /// Path resolved under the authenticated upload tmpdir
    /// (`octos-uploads`). User-uploaded attachments live here.
    UploadTmpdir,
    /// Path resolved under the per-session workspace root.
    Workspace,
    /// Path resolved under the profile root (a profile's `data_dir`).
    Profile,
}

/// Successful resolution of a tool-supplied file path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedToolPath {
    /// Absolute on-disk path. Existence is guaranteed when the scope
    /// is [`ToolPathScope::UploadTmpdir`] or [`ToolPathScope::Profile`]
    /// because their resolution always passes through `canonicalize`.
    /// For [`ToolPathScope::Workspace`] the path is canonicalized only
    /// if it points at an existing file/dir; otherwise the result is
    /// the normalised workspace-relative location, so write-style tools
    /// (`write_file`, `edit_file`) can still create new files.
    pub absolute: PathBuf,
    /// Which root the path resolved under.
    pub scope: ToolPathScope,
}

/// Errors returned by [`resolve_tool_path`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolPathError {
    /// The supplied path tried to escape its allowed root via `..`.
    Traversal,
    /// The path is absolute but does not lie inside any allowed root
    /// (workspace root, upload tmpdir, or profile root).
    OutsideAllowedRoots,
    /// The handle (`up/...` or `pf/...`) could not be decoded — bad
    /// base64, empty payload, or unknown scope prefix.
    DecodeFailed,
}

impl std::fmt::Display for ToolPathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Traversal => f.write_str("path traversal is not allowed"),
            Self::OutsideAllowedRoots => {
                f.write_str("path is outside the workspace, upload tmpdir, and profile root")
            }
            Self::DecodeFailed => f.write_str("file handle could not be decoded"),
        }
    }
}

impl std::error::Error for ToolPathError {}

pub fn temp_upload_root() -> PathBuf {
    std::env::temp_dir().join("octos-uploads")
}

pub fn encode_profile_file_handle(base_dir: &Path, path: &Path) -> Option<String> {
    let relative = path
        .strip_prefix(base_dir)
        .ok()
        .map(Path::to_path_buf)
        .or_else(|| {
            let canonical_base = std::fs::canonicalize(base_dir).ok()?;
            let canonical_path = std::fs::canonicalize(path).ok()?;
            canonical_path
                .strip_prefix(&canonical_base)
                .ok()
                .map(Path::to_path_buf)
        })?;
    let display_name = path.file_name()?.to_str()?;
    encode_scoped_handle(PROFILE_HANDLE_PREFIX, &relative, display_name)
}

pub fn encode_tmp_upload_handle(path: &Path, display_name: Option<&str>) -> Option<String> {
    let upload_root = temp_upload_root();
    let relative = path.strip_prefix(&upload_root).ok()?;
    let display_name = display_name
        .or_else(|| path.file_name().and_then(|name| name.to_str()))
        .filter(|value| !value.is_empty())
        .unwrap_or("file");
    encode_scoped_handle(UPLOAD_HANDLE_PREFIX, relative, display_name)
}

pub fn decode_file_handle(handle: &str) -> Option<FileHandleScope> {
    let mut parts = handle.splitn(3, '/');
    let prefix = parts.next()?;
    let payload = parts.next()?;
    // The third segment is a human-readable display name appended at
    // `encode_scoped_handle` time — purely decorative. LLMs frequently
    // truncate the handle to `up/<base64>` (e.g. tab-complete suggests
    // the path up to the last `/` and the trailing filename is
    // dropped). Accepting the two-segment form rescues those calls
    // because the payload alone carries the full relative path needed
    // to locate the file under `temp_upload_root` / profile root.
    let _display_name = parts.next();
    let relative = decode_relative_payload(payload)?;

    match prefix {
        PROFILE_HANDLE_PREFIX => Some(FileHandleScope::ProfileRelative(relative)),
        UPLOAD_HANDLE_PREFIX => Some(FileHandleScope::TempUpload(relative)),
        _ => None,
    }
}

pub fn resolve_scoped_file_handle(base_dir: &Path, handle: &str) -> Option<PathBuf> {
    match decode_file_handle(handle)? {
        FileHandleScope::ProfileRelative(relative) => canonicalize_under(base_dir, &relative),
        FileHandleScope::TempUpload(relative) => canonicalize_under(&temp_upload_root(), &relative),
    }
}

pub fn resolve_legacy_file_request(base_dir: &Path, raw: &str) -> Option<PathBuf> {
    let candidate = Path::new(raw);
    if candidate.is_absolute() {
        let canonical = std::fs::canonicalize(candidate).ok()?;
        let profile_root = canonical_root(base_dir);
        let upload_root = canonical_root(&temp_upload_root());
        if canonical.is_file()
            && (canonical.starts_with(&profile_root) || canonical.starts_with(&upload_root))
        {
            return Some(canonical);
        }
        return None;
    }

    let relative = safe_relative_path(raw)?;
    canonicalize_under(&temp_upload_root(), &relative)
}

pub fn resolve_upload_reference(raw: &str) -> Option<PathBuf> {
    match decode_file_handle(raw) {
        Some(FileHandleScope::TempUpload(relative)) => {
            canonicalize_under(&temp_upload_root(), &relative)
        }
        Some(FileHandleScope::ProfileRelative(_)) => None,
        None => {
            let candidate = Path::new(raw);
            if candidate.is_absolute() {
                let canonical = std::fs::canonicalize(candidate).ok()?;
                let upload_root = canonical_root(&temp_upload_root());
                if canonical.is_file() && canonical.starts_with(&upload_root) {
                    return Some(canonical);
                }
                return None;
            }

            let relative = safe_relative_path(raw)?;
            canonicalize_under(&temp_upload_root(), &relative)
        }
    }
}

/// Unified file-path resolver for LLM-supplied tool arguments.
///
/// Tries, in order:
///
/// 1. Decode as an `up/<base64>/<display>` or `up/<base64>` upload handle —
///    payload locates an existing file under `temp_upload_root()`.
/// 2. Decode as a `pf/<base64>/<display>` or `pf/<base64>` profile handle
///    when `profile_root` is provided — payload locates an existing file
///    under that root.
/// 3. Treat as absolute and accept when the canonicalised path lies inside
///    one of the allowed roots (upload tmpdir, workspace, or profile).
///    macOS firmlinks are handled transparently — the canonical form
///    (e.g. `/private/var/folders/...`) and the un-prefixed form
///    (`/var/folders/...`) compare equal.
/// 4. If the raw value matches an existing file under `temp_upload_root()`
///    (bare basename like `019e22…wav`, or `up/<x>` that didn't decode),
///    return it as an [`ToolPathScope::UploadTmpdir`] entry. This is the
///    leaf-name form the upload handler writes when the LLM strips
///    everything but the filename.
/// 5. Treat as workspace-relative — normalises `..`/`.` and rejects any
///    result that would land outside the workspace.
///
/// Existence is REQUIRED for scopes 1, 2, and 4 (those resolutions go
/// through `canonicalize`). For scope 5 the path is returned even if the
/// file does not yet exist — write-style tools (`write_file`, `edit_file`)
/// rely on this to create new files. Callers that need an existence check
/// must perform it themselves.
///
/// Symlink rejection is the caller's responsibility (use
/// `read_no_follow` / `write_no_follow` on the returned path). The
/// resolver only verifies that the **canonical** path lies inside an
/// allowed root, which already collapses any symlink-target reachable
/// through the root entry.
pub fn resolve_tool_path(
    workspace_root: &Path,
    profile_root: Option<&Path>,
    user_path: &str,
) -> Result<ResolvedToolPath, ToolPathError> {
    // 1) Try decoding as a scoped file handle (up/... or pf/...).
    match decode_file_handle(user_path) {
        Some(FileHandleScope::TempUpload(relative)) => {
            return canonicalize_under(&temp_upload_root(), &relative)
                .map(|absolute| ResolvedToolPath {
                    absolute,
                    scope: ToolPathScope::UploadTmpdir,
                })
                .ok_or(ToolPathError::OutsideAllowedRoots);
        }
        Some(FileHandleScope::ProfileRelative(relative)) => {
            let Some(profile_root) = profile_root else {
                // A pf/... handle was supplied but the caller doesn't
                // have a profile root to anchor it against. Surface as a
                // decode-shaped failure so callers can fall back to
                // their own legacy paths if any.
                return Err(ToolPathError::DecodeFailed);
            };
            return canonicalize_under(profile_root, &relative)
                .map(|absolute| ResolvedToolPath {
                    absolute,
                    scope: ToolPathScope::Profile,
                })
                .ok_or(ToolPathError::OutsideAllowedRoots);
        }
        None => {}
    }

    let candidate = Path::new(user_path);

    // 2) Absolute paths must lie inside an allowed root.
    if candidate.is_absolute() {
        let candidate_canon = canonicalize_lossy(candidate);
        let upload_root_canon = canonical_root(&temp_upload_root());
        if candidate_canon.starts_with(&upload_root_canon) {
            return Ok(ResolvedToolPath {
                absolute: candidate_canon,
                scope: ToolPathScope::UploadTmpdir,
            });
        }
        let workspace_canon = canonical_root(workspace_root);
        if candidate_canon.starts_with(&workspace_canon) {
            return Ok(ResolvedToolPath {
                absolute: candidate_canon,
                scope: ToolPathScope::Workspace,
            });
        }
        if let Some(profile_root) = profile_root {
            let profile_canon = canonical_root(profile_root);
            if candidate_canon.starts_with(&profile_canon) {
                return Ok(ResolvedToolPath {
                    absolute: candidate_canon,
                    scope: ToolPathScope::Profile,
                });
            }
        }
        return Err(ToolPathError::OutsideAllowedRoots);
    }

    // 3) Bare basenames / undecodable relative paths that exist under
    //    the upload tmpdir are accepted as uploads. This is the
    //    leaf-name form the upload handler writes (e.g. the LLM hands
    //    the model the filename verbatim instead of the encoded handle).
    if let Some(relative) = safe_relative_path(user_path) {
        if let Some(absolute) = canonicalize_under(&temp_upload_root(), &relative) {
            return Ok(ResolvedToolPath {
                absolute,
                scope: ToolPathScope::UploadTmpdir,
            });
        }
    }

    // 4) Otherwise, treat as workspace-relative. Reject `..` traversal.
    let joined = workspace_root.join(user_path);
    let normalised = normalize_lexical(&joined);
    let workspace_normalised = normalize_lexical(workspace_root);
    if !normalised.starts_with(&workspace_normalised) {
        return Err(ToolPathError::Traversal);
    }

    // Workspace-relative paths return their LEXICAL form on purpose:
    // file tools (`read_file`, `write_file`, `list_dir`) layer their
    // own `O_NOFOLLOW` open / symlink rejection on top of the resolved
    // path, and that gate is the only thing standing between a symlink
    // `workspace/secret -> /etc/passwd` and a successful read of
    // `/etc/passwd`. If we canonicalised here the resolver would
    // silently follow the symlink and the leaf `O_NOFOLLOW` would no
    // longer have anything to refuse — it'd see a plain file at the
    // canonical target. Keep the lexical workspace location and let the
    // tool's open-time gate police symlinks atomically.
    //
    // Upload-tmpdir / profile-root scopes (branches 1, 2, 3, 4) still
    // canonicalise via `canonicalize_under` / `canonicalize_lossy`
    // because those roots' files have already been written by the
    // server and the canonical form is required for the containment
    // check (macOS firmlinks).
    Ok(ResolvedToolPath {
        absolute: normalised,
        scope: ToolPathScope::Workspace,
    })
}

/// Lexical path normalisation: collapses `.` and `..` without touching
/// the filesystem. Mirrors `tools/mod.rs::normalize_path` — duplicated
/// here so the resolver stays self-contained.
fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) => {
                out.push(component.as_os_str());
            }
            Component::Normal(seg) => {
                out.push(seg);
            }
        }
    }
    out
}

/// Canonicalize as much of `path` as currently exists on disk; fall back
/// to lexical normalisation for the non-existent tail. macOS firmlinks
/// (`/var/folders/...` vs `/private/var/folders/...`) collapse through
/// `canonicalize`; the syntactic fallback is only used when nothing on
/// the path exists.
///
/// CRITICAL: `..` components are collapsed BEFORE the
/// walk-parents-until-existing loop. Without this pre-normalisation an
/// input like `/workspace/missing/../../secret.txt` would walk back to
/// `/workspace` (the closest existing ancestor) and re-attach the
/// original suffix verbatim, producing `/workspace/missing/../../secret.txt`
/// which then satisfies `starts_with("/workspace")` even though the
/// path actually escapes to `/secret.txt`. Lexically collapsing `..`
/// up front makes the containment check honest (codex review round 4
/// P2, 2026-05-13).
fn canonicalize_lossy(path: &Path) -> PathBuf {
    // Step 1: lexical normalisation — collapses `..` and `.` without
    // touching the filesystem. After this step the path has no `..`
    // components so `starts_with(allowed_root)` is an honest
    // containment check.
    let normalised = normalize_lexical(path);
    if let Ok(canon) = std::fs::canonicalize(&normalised) {
        return canon;
    }
    // Step 2: walk parents to find the longest existing prefix and
    // re-attach the remainder. The remainder cannot contain `..` (it
    // was already collapsed in step 1) so the result is a real
    // would-be on-disk location, not a traversal expression.
    let mut existing: &Path = &normalised;
    let mut suffix = PathBuf::new();
    while let Some(parent) = existing.parent() {
        if let Some(name) = existing.file_name() {
            let mut next_suffix = PathBuf::from(name);
            next_suffix.push(&suffix);
            suffix = next_suffix;
        }
        existing = parent;
        if let Ok(canon) = std::fs::canonicalize(existing) {
            return canon.join(suffix);
        }
        if existing.as_os_str().is_empty() {
            break;
        }
    }
    normalised
}

fn encode_scoped_handle(prefix: &str, relative: &Path, display_name: &str) -> Option<String> {
    let relative = normalize_relative_path(relative)?;
    let payload = URL_SAFE_NO_PAD.encode(relative.as_bytes());
    let display_name = sanitize_display_name(display_name);
    Some(format!("{prefix}/{payload}/{display_name}"))
}

fn decode_relative_payload(payload: &str) -> Option<PathBuf> {
    let decoded = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let relative = String::from_utf8(decoded).ok()?;
    safe_relative_path(&relative)
}

fn normalize_relative_path(path: &Path) -> Option<String> {
    let mut normalized = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::Normal(segment) => normalized.push(segment.to_string_lossy()),
            std::path::Component::CurDir => {}
            _ => return None,
        }
    }
    if normalized.is_empty() {
        None
    } else {
        Some(normalized.join("/"))
    }
}

fn safe_relative_path(raw: &str) -> Option<PathBuf> {
    let normalized = raw.trim().replace('\\', "/");
    let trimmed = normalized.trim_matches('/');
    if trimmed.is_empty() {
        return None;
    }

    let mut relative = PathBuf::new();
    for component in Path::new(trimmed).components() {
        match component {
            std::path::Component::Normal(segment) => relative.push(segment),
            std::path::Component::CurDir => {}
            _ => return None,
        }
    }
    Some(relative)
}

fn canonicalize_under(root: &Path, relative: &Path) -> Option<PathBuf> {
    let canonical = std::fs::canonicalize(root.join(relative)).ok()?;
    let canonical_root = canonical_root(root);
    if canonical.is_file() && canonical.starts_with(&canonical_root) {
        Some(canonical)
    } else {
        None
    }
}

fn canonical_root(root: &Path) -> PathBuf {
    std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf())
}

fn sanitize_display_name(name: &str) -> String {
    let cleaned = name
        .replace(['/', '\\', '\0', '\r', '\n'], "_")
        .trim()
        .to_string();
    if cleaned.is_empty() {
        "file".to_string()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_handle_round_trips() {
        let base = std::path::Path::new("/tmp/octos-data/profile");
        let file = base.join("slides/demo/output/deck.pptx");

        let handle = encode_profile_file_handle(base, &file).expect("handle");
        let decoded = decode_file_handle(&handle).expect("decoded");

        assert_eq!(
            decoded,
            FileHandleScope::ProfileRelative(PathBuf::from("slides/demo/output/deck.pptx"))
        );
        assert!(handle.ends_with("/deck.pptx"));
    }

    #[test]
    fn legacy_absolute_request_is_scoped() {
        let base = tempfile::tempdir().unwrap();
        let allowed = base.path().join("workspace").join("ok.txt");
        std::fs::create_dir_all(allowed.parent().unwrap()).unwrap();
        std::fs::write(&allowed, b"ok").unwrap();

        let outside_root = tempfile::tempdir().unwrap();
        let denied = outside_root.path().join("secret.txt");
        std::fs::write(&denied, b"nope").unwrap();

        assert!(resolve_legacy_file_request(base.path(), &allowed.to_string_lossy()).is_some());
        assert!(resolve_legacy_file_request(base.path(), &denied.to_string_lossy()).is_none());
    }
}
