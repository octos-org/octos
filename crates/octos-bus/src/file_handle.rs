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

/// Materialize turn-attached upload references into `<workspace>/uploads/` so
/// they become first-class, browsable session files (#1377). Returns the media
/// list rewritten for the model and for persistence:
///
/// - A **non-image** upload handle / tmpdir reference is COPIED into
///   `<workspace>/uploads/<name>` and rewritten to the workspace-relative
///   string `uploads/<name>`, so `read_file`/`grep`/`list_dir`/`glob` all work
///   by normal filesystem semantics — and global `up/` resolution can be
///   refused for scoped sessions (tenant isolation) without losing access.
/// - **Images** pass through UNCHANGED: they're consumed by the vision encoder
///   via `std::fs::read` on their path (never via `read_file`), so the model
///   never browses them and materializing would only risk the vision path.
///   (Image materialization is a tracked follow-up.)
/// - Anything that does NOT resolve to a staged upload — an already
///   workspace-relative path (e.g. a re-referenced prior-turn `uploads/x`) or
///   an unknown string — is passed through unchanged (idempotent across turns).
///
/// Copy is collision-safe (`uploads/<uuid>-<name>` on a name clash; never
/// overwrites), symlink-safe (refuses a symlinked `uploads/` dir or an existing
/// `uploads/<name>` entry), and atomic (temp file + rename). The source has
/// already been canonicalized under the upload tmpdir by
/// `resolve_upload_reference`, so symlink escapes on the source are excluded.
/// The original (sanitized) display name from an `up/<base64>/<display>`
/// handle, if present. `None` for the 2-segment `up/<base64>` form or a
/// non-handle string. base64 uses URL-safe alphabet (no `/`), so the display is
/// the segment after the second `/`.
fn handle_display_name(entry: &str) -> Option<String> {
    let display = entry.strip_prefix("up/")?.split_once('/')?.1;
    if display.is_empty() {
        return None;
    }
    Some(sanitize_display_name(display))
}

pub fn materialize_turn_uploads(
    workspace_root: &Path,
    tenant_id: Option<&str>,
    media: &[String],
) -> Vec<String> {
    let uploads_dir = workspace_root.join("uploads");
    media
        .iter()
        .filter_map(
            |entry| match materialize_one(&uploads_dir, tenant_id, entry) {
                MaterializeOutcome::Rewritten(path) => Some(path),
                MaterializeOutcome::Passthrough => Some(entry.clone()),
                // Foreign / cross-tenant: DROP the entry entirely. Passing the
                // original handle through would let a no-`SessionScope` session
                // (profile-qualified / cwd-hinted) read it via the legacy
                // `resolve_path` fallback, bypassing the tenant gate (codex P1).
                MaterializeOutcome::DropForeign => None,
            },
        )
        .collect()
}

enum MaterializeOutcome {
    /// Copied into the workspace; use this `uploads/<name>` path.
    Rewritten(String),
    /// Not a staged upload to rewrite (image / already-workspace / unknown /
    /// I/O refusal) — keep the original media string unchanged.
    Passthrough,
    /// A staged upload owned by ANOTHER tenant — remove it from the media set.
    DropForeign,
}

/// Copy one upload reference into `uploads_dir`. See [`MaterializeOutcome`].
/// Whether a RESOLVED upload-tmpdir path is owned by `tenant_id`.
///
/// Uploads are stored as `octos-uploads/<tenant>/<uuid>_<name>`, so the FIRST
/// path component of the resolved file (relative to the canonical upload root)
/// is the owning tenant. This is the single tenant-ownership predicate shared by
/// every upload-into-workspace path so the isolation rule cannot drift between
/// them (the serve `materialize_one` and the gateway/actor
/// `copy_media_to_workspace`):
///
/// - `tenant_id == Some(t)` → owned only if the resolved file's first component
///   under the upload root equals `t`. A file owned by ANOTHER tenant — by `up/`
///   handle, a raw absolute/relative tmpdir path, OR an image path — is NOT
///   owned. A flat legacy file (no tenant component) is NOT owned either.
/// - `tenant_id == None` (solo / CLI `octos chat`, single-tenant) → always owned.
///
/// Callers pass a path that is ALREADY resolved (e.g. via
/// [`resolve_upload_reference`]); a path outside the upload root yields `false`
/// for a multi-tenant caller, so non-upload paths must be screened separately.
/// Whether a RESOLVED path lives under the process-global upload tmpdir
/// root (`octos-uploads`). Callers use this to decide whether the tenant
/// ownership rule ([`upload_owned_by_tenant`]) even applies: a workspace or
/// profile file is NOT under the upload root and must not be subjected to
/// the upload-ownership check (it would be falsely denied).
pub fn is_under_upload_root(resolved: &Path) -> bool {
    resolved.starts_with(canonical_root(&temp_upload_root()))
}

pub fn upload_owned_by_tenant(resolved: &Path, tenant_id: Option<&str>) -> bool {
    let Some(tenant) = tenant_id else {
        return true; // single-tenant: ownership check does not apply
    };
    let Ok(rel) = resolved.strip_prefix(canonical_root(&temp_upload_root())) else {
        return false; // outside the upload root entirely
    };
    let mut comps = rel.components();
    let first_is_tenant =
        matches!(comps.next(), Some(Component::Normal(s)) if s.to_str() == Some(tenant));
    // Require at least one MORE component after the tenant directory. A flat
    // file resolved at `octos-uploads/<tenant>` (the legacy flat `/upload`
    // path can create a file named exactly like the tenant) has the tenant as
    // its ONLY component — that is NOT a `<tenant>/<file>` layout and must not
    // be treated as owned (codex P2). `<tenant>/<file>` → owned.
    first_is_tenant && comps.next().is_some()
}

fn materialize_one(uploads_dir: &Path, tenant_id: Option<&str>, entry: &str) -> MaterializeOutcome {
    let Some(src) = resolve_upload_reference(entry) else {
        return MaterializeOutcome::Passthrough; // not a staged upload (workspace path / external / unknown)
    };

    // #1377 tenant isolation: in a multi-tenant session only keep a file owned by
    // THIS tenant; a file owned by another tenant — whether referenced by `up/`
    // handle, a raw absolute/relative tmpdir path, OR an image path (which would
    // otherwise reach the vision encoder via std::fs::read) — is DROPPED. Checking
    // the RESOLVED path covers ALL of these (codex round-6 P1.3 + round-7 image
    // P1). Solo sessions (`tenant_id == None`, CLI `octos chat`) skip the check.
    if !upload_owned_by_tenant(&src, tenant_id) {
        return MaterializeOutcome::DropForeign;
    }

    // Owned image: keep it on the vision path (the encoder reads it directly via
    // std::fs::read) — don't copy into `uploads/`. Rewrite to the resolved
    // ABSOLUTE path so the encoder can read it (a bare `up/` handle isn't a real
    // file path). A non-tmpdir image (workspace/external) already returned
    // Passthrough above via `resolve_upload_reference` == None.
    if crate::media::is_image(entry) {
        return match src.to_str() {
            Some(abs) => MaterializeOutcome::Rewritten(abs.to_string()),
            None => MaterializeOutcome::Passthrough,
        };
    }

    // From here the file is OWNED by this tenant (or solo) — an I/O refusal
    // returns Passthrough (keep the original handle; never re-expose a foreign
    // file, which was already DropForeign'd above).
    // Never write through a symlinked `uploads/` directory.
    if let Ok(meta) = std::fs::symlink_metadata(uploads_dir) {
        if meta.file_type().is_symlink() {
            return MaterializeOutcome::Passthrough;
        }
    }
    if std::fs::create_dir_all(uploads_dir).is_err() {
        return MaterializeOutcome::Passthrough;
    }

    // Prefer the original display name carried in the `up/<base64>/<display>`
    // handle — the upload endpoint stores the file on disk as `<uuid>_<name>`,
    // so `src.file_name()` would leak the uuid into the workspace path the model
    // sees. Fall back to the source basename for non-handle references.
    let base = handle_display_name(entry).unwrap_or_else(|| {
        sanitize_display_name(src.file_name().and_then(|n| n.to_str()).unwrap_or("file"))
    });
    // Stage the bytes in a private temp file, then publish via `hard_link`,
    // which is atomic AND fails (rather than replacing, the way `rename` does)
    // if the destination already exists. This makes concurrent turns
    // materializing the same display name safe: at most one wins each name; the
    // loser observes `AlreadyExists` and falls back to a uuid-prefixed name. A
    // bare existence pre-check + `rename` would let two turns both pick
    // `uploads/<name>` and clobber each other (codex #1377 P2).
    let tmp = uploads_dir.join(format!(".{}.{base}.tmp", uuid::Uuid::now_v7()));
    if std::fs::copy(&src, &tmp).is_err() {
        return MaterializeOutcome::Passthrough;
    }
    let published = {
        let first = uploads_dir.join(&base);
        match std::fs::hard_link(&tmp, &first) {
            Ok(()) => Some(base.clone()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let unique = format!("{}-{base}", uuid::Uuid::now_v7());
                std::fs::hard_link(&tmp, uploads_dir.join(&unique))
                    .ok()
                    .map(|()| unique)
            }
            Err(_) => None,
        }
    };
    let _ = std::fs::remove_file(&tmp);
    match published {
        Some(name) => MaterializeOutcome::Rewritten(format!("uploads/{name}")),
        None => MaterializeOutcome::Passthrough,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Create a real file under the upload tmpdir (flat, legacy layout) and
    /// return its `up/` handle.
    fn stage_upload(name: &str, body: &[u8]) -> (PathBuf, String) {
        let root = temp_upload_root();
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join(format!("{}_{name}", uuid::Uuid::now_v7()));
        std::fs::write(&path, body).unwrap();
        let handle = encode_tmp_upload_handle(&path, Some(name)).expect("encode handle");
        (path, handle)
    }

    /// Create a real file under the per-tenant upload layout
    /// (`octos-uploads/<tenant>/<uuid>_<name>`) and return its `up/` handle
    /// (whose decoded relative path therefore begins with `<tenant>`).
    fn stage_tenant_upload(tenant: &str, name: &str, body: &[u8]) -> (PathBuf, String) {
        let dir = temp_upload_root().join(tenant);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{}_{name}", uuid::Uuid::now_v7()));
        std::fs::write(&path, body).unwrap();
        let handle = encode_tmp_upload_handle(&path, Some(name)).expect("encode handle");
        (path, handle)
    }

    #[test]
    fn materialize_matching_tenant_materializes_mismatch_and_flat_dropped() {
        let ws = tempfile::tempdir().unwrap();
        let (_a, h_a) = stage_tenant_upload("tenant-a", "owned.md", b"mine");
        let (_b, h_b) = stage_tenant_upload("tenant-b", "foreign.md", b"theirs");
        let (_f, h_flat) = stage_upload("legacy.md", b"flat");

        // Session belongs to tenant-a.
        let out = materialize_turn_uploads(ws.path(), Some("tenant-a"), &[h_a, h_b, h_flat]);

        // tenant-a's own upload → materialized; the others (another tenant + a
        // flat legacy handle) → DROPPED entirely (removed from the media set,
        // not passed through — so a no-scope session can't reach them via the
        // legacy resolver), and never copied into the workspace.
        assert_eq!(out, vec!["uploads/owned.md".to_string()]);
        assert!(ws.path().join("uploads/owned.md").is_file());
        assert!(!ws.path().join("uploads/foreign.md").exists());
        assert!(!ws.path().join("uploads/legacy.md").exists());
    }

    #[test]
    fn materialize_rejects_raw_tmpdir_path_bypass() {
        // codex round-5 P1.3: a client can submit a raw absolute path under
        // another tenant's upload dir (NOT an up/ handle, so decode returns
        // None). The check on the RESOLVED path must still drop it.
        let ws = tempfile::tempdir().unwrap();
        let (abs_src, _handle) = stage_tenant_upload("tenant-b", "secret.md", b"theirs");
        let raw_abs = abs_src.to_string_lossy().into_owned();

        let out =
            materialize_turn_uploads(ws.path(), Some("tenant-a"), std::slice::from_ref(&raw_abs));
        assert!(
            out.is_empty(),
            "raw cross-tenant tmpdir path must be DROPPED, got: {out:?}"
        );
        assert!(!ws.path().join("uploads/secret.md").exists());

        // The owner (tenant-b) CAN materialize the same raw path. (A raw path
        // carries no `up/` display name, so the workspace copy keeps the on-disk
        // `<uuid>_secret.md` basename — that's fine; the point is it materializes.)
        let ws_b = tempfile::tempdir().unwrap();
        let out_b = materialize_turn_uploads(ws_b.path(), Some("tenant-b"), &[raw_abs]);
        assert!(
            out_b[0].starts_with("uploads/") && out_b[0].ends_with("secret.md"),
            "owner's raw path should materialize, got: {}",
            out_b[0]
        );
        assert!(ws_b.path().join(&out_b[0]).is_file());
    }

    #[test]
    fn is_under_upload_root_distinguishes_upload_from_other_paths() {
        let root = canonical_root(&temp_upload_root());
        // Upload-root paths (any tenant / flat) are "under" the root — the
        // download gate then applies the ownership check to these.
        assert!(is_under_upload_root(&root.join("dspfac/uuid_doc.md")));
        assert!(is_under_upload_root(&root.join("flat.md")));
        // Workspace / profile / external paths are NOT under the upload root,
        // so the download gate leaves them alone (no false denial).
        assert!(!is_under_upload_root(std::path::Path::new(
            "/some/profile/data/users/web-1/workspace/out.pptx"
        )));
        assert!(!is_under_upload_root(std::path::Path::new("/etc/passwd")));
    }

    #[test]
    fn upload_owned_by_tenant_keys_off_first_component_under_root() {
        // The shared predicate used by BOTH the serve materializer and the
        // gateway/actor `copy_media_to_workspace`. Ownership = the resolved
        // file's first path component (under the canonical upload root)
        // equals the tenant.
        let root = canonical_root(&temp_upload_root());
        let owned = root.join("dspfac").join("uuid_doc.md");
        let foreign = root.join("acme").join("uuid_doc.md");
        let flat = root.join("legacy_no_tenant.md");
        // Flat file named EXACTLY like the tenant (legacy flat `/upload`): the
        // tenant is its only component, so it must NOT be owned (codex P2).
        let flat_named_as_tenant = root.join("dspfac");
        let outside = PathBuf::from("/etc/passwd");

        // Multi-tenant: only `<tenant>/<file>` is owned.
        assert!(upload_owned_by_tenant(&owned, Some("dspfac")));
        assert!(!upload_owned_by_tenant(&foreign, Some("dspfac")));
        // Flat files (no tenant subdir) and paths outside the upload root are
        // NOT owned — all dropped by the materializer / copy path.
        assert!(!upload_owned_by_tenant(&flat, Some("dspfac")));
        assert!(!upload_owned_by_tenant(
            &flat_named_as_tenant,
            Some("dspfac")
        ));
        assert!(!upload_owned_by_tenant(&outside, Some("dspfac")));

        // Solo (tenant_id == None, CLI `octos chat`): ownership check skipped.
        assert!(upload_owned_by_tenant(&foreign, None));
        assert!(upload_owned_by_tenant(&outside, None));
    }

    #[test]
    fn materialize_solo_session_ignores_tenant_check() {
        // tenant_id == None (CLI `octos chat`): no tenant gate — a flat handle
        // still materializes (back-compat).
        let ws = tempfile::tempdir().unwrap();
        let (_f, h_flat) = stage_upload("notes.md", b"solo");
        let out = materialize_turn_uploads(ws.path(), None, std::slice::from_ref(&h_flat));
        assert_eq!(out[0], "uploads/notes.md");
        assert!(ws.path().join("uploads/notes.md").is_file());
    }

    #[test]
    fn materialize_copies_non_image_upload_into_workspace_uploads() {
        let ws = tempfile::tempdir().unwrap();
        let (src, handle) = stage_upload("report.md", b"# strategy\n");

        let out = materialize_turn_uploads(ws.path(), None, &[handle]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], "uploads/report.md", "rewritten to workspace path");
        let dest = ws.path().join("uploads/report.md");
        assert!(dest.is_file(), "file copied into <workspace>/uploads/");
        assert_eq!(std::fs::read(&dest).unwrap(), b"# strategy\n");
        let _ = std::fs::remove_file(&src);
    }

    #[test]
    fn materialize_keeps_owned_image_on_vision_path_as_absolute() {
        let ws = tempfile::tempdir().unwrap();
        let (src, handle) = stage_upload("photo.png", b"\x89PNG\r\n");
        // Solo (no tenant): image is NOT copied into uploads/, but is rewritten
        // to its resolved ABSOLUTE path so the vision encoder can `std::fs::read`
        // it (a bare `up/` handle isn't a real file path).
        let out = materialize_turn_uploads(ws.path(), None, std::slice::from_ref(&handle));
        assert_eq!(
            out[0],
            std::fs::canonicalize(&src).unwrap().to_string_lossy(),
            "owned image → resolved absolute path"
        );
        assert!(
            !ws.path().join("uploads").exists(),
            "no uploads/ dir created for an image-only turn"
        );
        let _ = std::fs::remove_file(&src);
    }

    #[test]
    fn materialize_drops_foreign_image() {
        // codex round-7 P1: a raw image path under ANOTHER tenant's upload dir
        // must be DROPPED, not passed through to the vision encoder.
        let ws = tempfile::tempdir().unwrap();
        let (src, _h) = stage_tenant_upload("tenant-b", "secret.png", b"\x89PNG\r\n");
        let raw_abs = src.to_string_lossy().into_owned();
        let out = materialize_turn_uploads(ws.path(), Some("tenant-a"), &[raw_abs]);
        assert!(
            out.is_empty(),
            "foreign image must be dropped, got: {out:?}"
        );
        let _ = std::fs::remove_file(&src);
    }

    #[test]
    fn materialize_passes_through_non_upload_paths() {
        let ws = tempfile::tempdir().unwrap();
        // Already a workspace-relative path (e.g. a re-referenced prior turn) —
        // resolve_upload_reference returns None → passthrough (idempotent).
        let out = materialize_turn_uploads(
            ws.path(),
            None,
            &["uploads/report.md".to_string(), "notes.txt".to_string()],
        );
        assert_eq!(out, vec!["uploads/report.md", "notes.txt"]);
    }

    #[test]
    fn materialize_never_overwrites_on_name_collision() {
        let ws = tempfile::tempdir().unwrap();
        let (s1, h1) = stage_upload("dup.md", b"first");
        let (s2, h2) = stage_upload("dup.md", b"second");
        let out = materialize_turn_uploads(ws.path(), None, &[h1, h2]);
        assert_eq!(out[0], "uploads/dup.md");
        assert_ne!(out[1], "uploads/dup.md", "collision gets a unique suffix");
        assert!(out[1].starts_with("uploads/") && out[1].ends_with("-dup.md"));
        // Both files exist with their own content (no clobber).
        assert_eq!(std::fs::read(ws.path().join(&out[0])).unwrap(), b"first");
        assert_eq!(std::fs::read(ws.path().join(&out[1])).unwrap(), b"second");
        let _ = std::fs::remove_file(&s1);
        let _ = std::fs::remove_file(&s2);
    }

    #[cfg(unix)]
    #[test]
    fn materialize_refuses_symlinked_uploads_dir() {
        let ws = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        // `uploads` is a symlink pointing outside the workspace.
        std::os::unix::fs::symlink(elsewhere.path(), ws.path().join("uploads")).unwrap();
        let (src, handle) = stage_upload("x.md", b"data");
        let out = materialize_turn_uploads(ws.path(), None, std::slice::from_ref(&handle));
        assert_eq!(
            out[0], handle,
            "must NOT write through a symlinked uploads/ dir"
        );
        assert!(
            !elsewhere.path().join("x.md").exists(),
            "no file written through the symlink"
        );
        let _ = std::fs::remove_file(&src);
    }

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
