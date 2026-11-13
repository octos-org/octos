//! One server-created workspace per chat (UPCR-2026-R1EFC56A3).
//!
//! A chat's folder is `<profile data_dir>/chats/<name>/` and its record is
//! `<profile data_dir>/chats/<name>.json`, naming the session that owns the
//! folder. The record sits outside the folder, so nothing the chat writes can
//! change whose folder it is. `<name>` is derived from the session id, so a
//! lookup is a path computation, never a scan, and is cheap enough for every
//! session bootstrap and permission read to ask "is this session a chat?".
//!
//! The folder is the chat's whole filesystem: its session runtime is scoped to
//! it with no shared zones, its tools are cut down to [`CHAT_TOOLS`], and its
//! transcript stays in the profile store rather than inside the folder. The
//! folder lives as long as the session and is removed when the session is
//! deleted.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use octos_core::SessionKey;
use serde::{Deserialize, Serialize};

const CHATS_DIR: &str = "chats";

/// The tools a chat keeps. Every one of them reads and writes only through
/// the session scope (or, for `send_file`, only from the chat's folder); no
/// shell, process, network, delegation or plugin tool is on the list, and a
/// tool added later stays out until it is listed here.
pub(crate) const CHAT_TOOLS: &[&str] = &[
    "read_file",
    "list_dir",
    "glob",
    "grep",
    "write_file",
    "edit_file",
    "diff_edit",
    "apply_patch",
    "send_file",
    "ask_user_question",
    "memory_search",
    "memory_load",
    "memory_note",
    "save_memory",
    "recall",
    "recall_memory",
    "record_memory_use",
    "tool_search",
    "discover_tools",
    "run_tool",
];

#[derive(Serialize, Deserialize)]
struct ChatRecord {
    session_id: String,
    profile_id: String,
    created_at: DateTime<Utc>,
}

/// A chat and the folder it owns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChatWorkspace {
    pub session_id: SessionKey,
    pub profile_id: String,
    /// Canonical path of the chat's folder.
    pub root: PathBuf,
}

fn chats_root(data_dir: &Path) -> PathBuf {
    data_dir.join(CHATS_DIR)
}

fn folder_name(session_id: &SessionKey) -> String {
    use sha2::{Digest, Sha256};
    let digest = format!("{:x}", Sha256::digest(session_id.0.as_bytes()));
    digest[..32].to_owned()
}

fn record_path(data_dir: &Path, session_id: &SessionKey) -> PathBuf {
    chats_root(data_dir).join(format!("{}.json", folder_name(session_id)))
}

/// The chat `session_id` owns under `data_dir`. `Ok(None)`: it has no chat
/// record. An error: it has one whose binding cannot be used (the record names
/// another session, or the folder is missing, unreadable or a symlink). A
/// recorded chat stays a chat, so every caller refuses on the error instead
/// of running the session as an ordinary one.
pub(crate) fn lookup(
    data_dir: &Path,
    session_id: &SessionKey,
) -> std::io::Result<Option<ChatWorkspace>> {
    let Some(record) = read_record(data_dir, session_id)? else {
        return Ok(None);
    };
    let folder = chats_root(data_dir).join(folder_name(session_id));
    if !std::fs::symlink_metadata(&folder)?.is_dir() {
        return Err(unusable("its folder is not a directory"));
    }
    Ok(Some(ChatWorkspace {
        session_id: session_id.clone(),
        profile_id: record.profile_id,
        root: std::fs::canonicalize(&folder)?,
    }))
}

/// `session_id`'s chat record, if it has one; one naming another session is
/// an error.
fn read_record(data_dir: &Path, session_id: &SessionKey) -> std::io::Result<Option<ChatRecord>> {
    let bytes = match std::fs::read(record_path(data_dir, session_id)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let record: ChatRecord = serde_json::from_slice(&bytes)?;
    if record.session_id != session_id.0 {
        return Err(unusable("its record names another session"));
    }
    Ok(Some(record))
}

fn unusable(why: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("chat workspace unusable: {why}"),
    )
}

/// Create the chat's folder and record, or return the existing ones for the
/// same session (a retried create is harmless).
#[cfg(feature = "api")]
pub(crate) fn create(
    data_dir: &Path,
    profile_id: &str,
    session_id: &SessionKey,
) -> std::io::Result<ChatWorkspace> {
    if let Some(existing) = lookup(data_dir, session_id)? {
        return Ok(existing);
    }
    let root = chats_root(data_dir);
    private_dir(&root, true)?;
    private_dir(&root.join(folder_name(session_id)), false)?;
    let record = serde_json::to_vec(&ChatRecord {
        session_id: session_id.0.clone(),
        profile_id: profile_id.to_owned(),
        created_at: Utc::now(),
    })?;
    let path = record_path(data_dir, session_id);
    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    let written = (|| {
        use std::io::Write;
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(&record)?;
        file.sync_all()?;
        std::fs::rename(&tmp, &path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written?;
    lookup(data_dir, session_id)?.ok_or_else(|| std::io::Error::other("chat folder vanished"))
}

/// Give a chat whose folder is missing, or has a symlink in its place, a
/// fresh, empty, private folder again, so it can be opened to be deleted. The
/// symlink is removed, never followed. A usable chat, or a session that is
/// not a chat, is left alone.
#[cfg(feature = "api")]
pub(crate) fn restore_folder(data_dir: &Path, session_id: &SessionKey) -> std::io::Result<()> {
    if read_record(data_dir, session_id)?.is_none() || lookup(data_dir, session_id).is_ok() {
        return Ok(());
    }
    let folder = chats_root(data_dir).join(folder_name(session_id));
    match std::fs::symlink_metadata(&folder) {
        Ok(meta) if !meta.is_dir() => std::fs::remove_file(&folder)?,
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    private_dir(&folder, false)
}

/// A real directory only the server user can enter. A leftover folder from an
/// interrupted create is reused; a symlink in its place is refused.
#[cfg(feature = "api")]
fn private_dir(path: &Path, recursive: bool) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(recursive);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    match builder.create(path) {
        Err(error) if error.kind() != std::io::ErrorKind::AlreadyExists => return Err(error),
        _ => {}
    }
    if std::fs::symlink_metadata(path)?.is_dir() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "{} is not a directory",
            path.display()
        )))
    }
}

/// Every chat recorded under `data_dir`.
#[cfg(feature = "api")]
pub(crate) fn list(data_dir: &Path) -> Vec<ChatWorkspace> {
    let Ok(entries) = std::fs::read_dir(chats_root(data_dir)) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .filter_map(|entry| {
            let record: ChatRecord =
                serde_json::from_slice(&std::fs::read(entry.path()).ok()?).ok()?;
            lookup(data_dir, &SessionKey(record.session_id)).ok()?
        })
        .collect()
}

/// Whether a workspace at `path` would reach into the chat folders under
/// `data_dir`: it lies in one, or it holds them (their parent folder, or any
/// folder above it).
#[cfg(feature = "api")]
pub(crate) fn overlaps_chat_folders(data_dir: &Path, path: &Path) -> bool {
    let data_dir = std::fs::canonicalize(data_dir).unwrap_or_else(|_| data_dir.to_path_buf());
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    chats_root(&data_dir).starts_with(&path) || is_in_chat_folder(&data_dir, &path)
}

/// Whether `path` is (inside) one of the chat folders under `data_dir`.
pub(crate) fn is_in_chat_folder(data_dir: &Path, path: &Path) -> bool {
    let Ok(root) = std::fs::canonicalize(chats_root(data_dir)) else {
        return false;
    };
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    path.strip_prefix(&root)
        .is_ok_and(|rest| rest.components().next().is_some())
}

/// Delete the chat's folder, then its record.
#[cfg(feature = "api")]
pub(crate) fn remove(data_dir: &Path, session_id: &SessionKey) -> std::io::Result<()> {
    remove_folder(data_dir, session_id)?;
    remove_record(data_dir, session_id)
}

#[cfg(all(test, feature = "api"))]
thread_local! {
    /// Makes [`remove_folder`] fail on this thread, as a read-only mount would.
    pub(crate) static FAIL_FOLDER_REMOVAL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Delete the chat's folder and keep its record, so the session stays a chat
/// (an unusable one) until [`remove_record`]. Not a chat: nothing to do. A
/// folder already gone is fine, and a symlink in its place is removed, never
/// followed.
#[cfg(feature = "api")]
pub(crate) fn remove_folder(data_dir: &Path, session_id: &SessionKey) -> std::io::Result<()> {
    if read_record(data_dir, session_id)?.is_none() {
        return Ok(());
    }
    #[cfg(test)]
    if FAIL_FOLDER_REMOVAL.with(std::cell::Cell::get) {
        return Err(std::io::Error::other("injected folder removal failure"));
    }
    let folder = chats_root(data_dir).join(folder_name(session_id));
    match std::fs::symlink_metadata(&folder) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(&folder),
        Ok(_) => std::fs::remove_file(&folder),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Delete the chat's record, if it has one: the last step of deleting a chat.
#[cfg(feature = "api")]
pub(crate) fn remove_record(data_dir: &Path, session_id: &SessionKey) -> std::io::Result<()> {
    match std::fs::remove_file(record_path(data_dir, session_id)) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

/// Cut a registry down to the compiled-in [`CHAT_TOOLS`]: a plugin or MCP tool
/// that takes one of their names is dropped too.
pub(crate) fn confine_chat_tools(tools: &mut octos_agent::ToolRegistry) {
    tools.retain_builtin(|name| CHAT_TOOLS.contains(&name));
}
