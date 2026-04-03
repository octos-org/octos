//! Per-user soul/personality storage.
//!
//! Each user's custom personality is stored as `soul.md` (lowercase) in their
//! profile data directory. This is distinct from the shared `SOUL.md` bootstrap
//! file and takes precedence when present.

use std::io;
use std::path::Path;

const SOUL_FILENAME: &str = "soul.md";

/// Read the per-user soul file, returning trimmed content or `None`.
pub fn read_soul(data_dir: &Path) -> Option<String> {
    let path = data_dir.join(SOUL_FILENAME);
    match std::fs::read_to_string(&path) {
        Ok(content) if !content.trim().is_empty() => Some(content.trim().to_string()),
        _ => None,
    }
}

/// Write (or overwrite) the per-user soul file.
pub fn write_soul(data_dir: &Path, content: &str) -> io::Result<()> {
    let path = data_dir.join(SOUL_FILENAME);
    std::fs::write(&path, content.trim())
}

/// Remove the per-user soul file, reverting to the shared default.
pub fn remove_soul(data_dir: &Path) -> io::Result<()> {
    let path = data_dir.join(SOUL_FILENAME);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_return_none_when_no_soul_file() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(read_soul(tmp.path()).is_none());
    }

    #[test]
    fn should_roundtrip_write_and_read() {
        let tmp = tempfile::tempdir().unwrap();
        write_soul(tmp.path(), "  你是一个温柔的助手  ").unwrap();
        assert_eq!(read_soul(tmp.path()).unwrap(), "你是一个温柔的助手");
    }

    #[test]
    fn should_return_none_for_empty_content() {
        let tmp = tempfile::tempdir().unwrap();
        write_soul(tmp.path(), "   ").unwrap();
        assert!(read_soul(tmp.path()).is_none());
    }

    #[test]
    fn should_remove_soul_file() {
        let tmp = tempfile::tempdir().unwrap();
        write_soul(tmp.path(), "test").unwrap();
        assert!(read_soul(tmp.path()).is_some());
        remove_soul(tmp.path()).unwrap();
        assert!(read_soul(tmp.path()).is_none());
    }

    #[test]
    fn should_not_error_removing_nonexistent_soul() {
        let tmp = tempfile::tempdir().unwrap();
        remove_soul(tmp.path()).unwrap();
    }
}
