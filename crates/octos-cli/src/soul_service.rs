//! Per-user soul/personality storage.
//!
//! Each user's custom personality is stored as `soul.md` (lowercase) in their
//! profile data directory. This is distinct from the shared `SOUL.md` bootstrap
//! file and takes precedence when present.

use std::io;
use std::path::{Path, PathBuf};

use octos_core::SessionKey;

const SOUL_FILENAME: &str = "soul.md";

fn soul_path(data_dir: &Path) -> PathBuf {
    data_dir.join(SOUL_FILENAME)
}

fn session_soul_dir(data_dir: &Path, session_key: &SessionKey) -> PathBuf {
    let encoded_base = octos_bus::session::encode_path_component(session_key.base_key());
    data_dir.join("users").join(encoded_base)
}

/// Read the per-user soul file, returning trimmed content or `None`.
pub fn read_soul(data_dir: &Path) -> Option<String> {
    let path = soul_path(data_dir);
    match std::fs::read_to_string(&path) {
        Ok(content) if !content.trim().is_empty() => Some(content.trim().to_string()),
        _ => None,
    }
}

/// Write (or overwrite) the per-user soul file.
pub fn write_soul(data_dir: &Path, content: &str) -> io::Result<()> {
    std::fs::create_dir_all(data_dir)?;
    let path = soul_path(data_dir);
    std::fs::write(&path, content.trim())
}

/// Remove the per-user soul file, reverting to the shared default.
pub fn remove_soul(data_dir: &Path) -> io::Result<()> {
    let path = soul_path(data_dir);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Read the soul override for one gateway user/chat within a profile.
pub fn read_soul_for_session(data_dir: &Path, session_key: &SessionKey) -> Option<String> {
    read_soul(&session_soul_dir(data_dir, session_key))
}

/// Write the soul override for one gateway user/chat within a profile.
pub fn write_soul_for_session(
    data_dir: &Path,
    session_key: &SessionKey,
    content: &str,
) -> io::Result<()> {
    write_soul(&session_soul_dir(data_dir, session_key), content)
}

/// Remove the soul override for one gateway user/chat within a profile.
pub fn remove_soul_for_session(data_dir: &Path, session_key: &SessionKey) -> io::Result<()> {
    remove_soul(&session_soul_dir(data_dir, session_key))
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

    #[test]
    fn should_isolate_soul_by_session_base_key() {
        let tmp = tempfile::tempdir().unwrap();
        let chat_a = SessionKey::with_profile("dspfac", "telegram", "100");
        let chat_b = SessionKey::with_profile("dspfac", "telegram", "200");

        write_soul_for_session(tmp.path(), &chat_a, "coding helper").unwrap();
        write_soul_for_session(tmp.path(), &chat_b, "writing tutor").unwrap();

        assert_eq!(
            read_soul_for_session(tmp.path(), &chat_a).unwrap(),
            "coding helper"
        );
        assert_eq!(
            read_soul_for_session(tmp.path(), &chat_b).unwrap(),
            "writing tutor"
        );
    }

    #[test]
    fn should_share_soul_across_topics_for_same_chat() {
        let tmp = tempfile::tempdir().unwrap();
        let base = SessionKey::with_profile("dspfac", "telegram", "100");
        let topic = SessionKey::with_profile_topic("dspfac", "telegram", "100", "research");

        write_soul_for_session(tmp.path(), &base, "topic independent").unwrap();

        assert_eq!(
            read_soul_for_session(tmp.path(), &topic).unwrap(),
            "topic independent"
        );
    }
}
