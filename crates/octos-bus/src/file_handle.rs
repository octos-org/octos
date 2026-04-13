use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

const PROFILE_HANDLE_PREFIX: &str = "pf";
const UPLOAD_HANDLE_PREFIX: &str = "up";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileHandleScope {
    ProfileRelative(PathBuf),
    TempUpload(PathBuf),
}

pub fn temp_upload_root() -> PathBuf {
    std::env::temp_dir().join("octos-uploads")
}

pub fn encode_profile_file_handle(base_dir: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(base_dir).ok()?;
    let display_name = path.file_name()?.to_str()?;
    encode_scoped_handle(PROFILE_HANDLE_PREFIX, relative, display_name)
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
    let _display_name = parts.next()?;
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
