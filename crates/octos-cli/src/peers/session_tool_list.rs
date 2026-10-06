//! The durable host-only kernel tool list of a host SESSION (#2605,
//! UPCR-2026-035 "Durable host session tool list").
//!
//! The host (the connection that owns the app peers, e.g. OctoSense's shell)
//! sets, for one of its own sessions that is not an app peer (typically the
//! system agent's conversation), the EXACT list of kernel tools every turn on
//! that session may keep. The list:
//!
//! - is stored on disk (`<profile data dir>/host_session_tools/<sha256 of
//!   the session key>.json`), so it survives host reconnects and kernel
//!   restarts, and applies from the next turn start;
//! - narrows every turn on the session, whoever drives it (the host, an
//!   external client, a kernel continuation), in the serve turn path and the
//!   gateway's `session_actor` path;
//! - never widens anything: it only removes tools from the roster the
//!   profile policy already produced;
//! - is set and read only through the host-only raw methods
//!   `session/tool_list/set` and `session/tool_list/get`.
//!
//! A list that exists but cannot be read fails closed: the turn keeps no
//! kernel tools at all.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use octos_agent::ToolRegistry;
use octos_core::SessionKey;
use serde::{Deserialize, Serialize};

use super::peer_io;

/// Folder under the profile data dir holding one list per session.
pub(crate) const SESSION_TOOL_LISTS_DIR: &str = "host_session_tools";

/// The most kernel tool names one list may hold.
pub(crate) const MAX_SESSION_TOOL_LIST: usize = super::host_tools::MAX_GENERIC_TOOLS;

/// One session's durable list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SessionToolList {
    /// Increments on every set (the first set is 1), clears included, so a
    /// host's `if_version` never matches a stale read.
    pub(crate) version: u64,
    /// The session the list belongs to (a check against a digest collision
    /// or a copied file).
    pub(crate) session_id: String,
    /// `None`: cleared (the session keeps its usual roster). `Some`: the
    /// exact kernel tools every turn keeps (an empty list keeps none).
    pub(crate) generic_tools: Option<Vec<String>>,
    /// Unix seconds of the last set.
    pub(crate) updated_at: u64,
}

/// What the kernel knows about a session's list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StoredSessionToolList {
    /// Never set (or the session is not eligible): no change.
    None,
    Set(SessionToolList),
    /// The file exists but cannot be read or parsed: fail closed.
    Unreadable,
}

impl StoredSessionToolList {
    /// The kernel tool names a turn may keep: `None` leaves the roster as
    /// it is, `Some(empty)` keeps nothing.
    pub(crate) fn allowed(&self) -> Option<Vec<String>> {
        match self {
            StoredSessionToolList::None => None,
            StoredSessionToolList::Set(list) => list.generic_tools.clone(),
            StoredSessionToolList::Unreadable => Some(Vec::new()),
        }
    }

    pub(crate) fn version(&self) -> u64 {
        match self {
            StoredSessionToolList::Set(list) => list.version,
            StoredSessionToolList::None | StoredSessionToolList::Unreadable => 0,
        }
    }
}

/// Whether `session` may carry a host session list: an app peer's session
/// (`peer-…`) or request context (`peerctx-…`) takes its kernel tools from
/// its peer's `peer/tools/register` `generic_tools` instead.
pub(crate) fn session_is_eligible(session: &SessionKey) -> bool {
    !session.topic().is_some_and(|topic| {
        topic.starts_with("peer-")
            || topic.starts_with(super::app_binding::PEER_CONTEXT_TOPIC_PREFIX)
    })
}

fn lists_dir(data_dir: &Path) -> PathBuf {
    data_dir.join(SESSION_TOOL_LISTS_DIR)
}

fn leaf_for(session: &SessionKey) -> String {
    format!("{}.json", super::app_binding::token_digest(&session.0))
}

/// Read `session`'s list from the profile data dir `data_dir`.
pub(crate) fn read_session_tool_list(
    data_dir: &Path,
    session: &SessionKey,
) -> StoredSessionToolList {
    if !session_is_eligible(session) {
        return StoredSessionToolList::None;
    }
    let dir = lists_dir(data_dir);
    match std::fs::symlink_metadata(&dir) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return StoredSessionToolList::None;
        }
        // A folder that exists but is not a plain directory (a symlink, a
        // file): something tampered with it, so fail closed.
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
            return StoredSessionToolList::Unreadable;
        }
        Err(_) => return StoredSessionToolList::Unreadable,
        Ok(_) => {}
    }
    match peer_io::read_peer_control_file(
        &dir,
        &leaf_for(session),
        peer_io::PEER_FILE_READ_CAP_SMALL,
    ) {
        Ok(None) => StoredSessionToolList::None,
        Ok(Some(body)) => match serde_json::from_str::<SessionToolList>(&body) {
            Ok(list) if list.session_id == session.0 => StoredSessionToolList::Set(list),
            _ => StoredSessionToolList::Unreadable,
        },
        Err(_) => StoredSessionToolList::Unreadable,
    }
}

/// Validate and normalize a list: kernel tool names (no `.`), at most
/// [`MAX_SESSION_TOOL_LIST`], duplicates dropped, order kept.
pub(crate) fn normalize_tool_list(names: Vec<String>) -> Result<Vec<String>, String> {
    if names.len() > MAX_SESSION_TOOL_LIST {
        return Err(format!(
            "{} kernel tools (max {MAX_SESSION_TOOL_LIST})",
            names.len()
        ));
    }
    let mut out: Vec<String> = Vec::with_capacity(names.len());
    for name in names {
        super::host_tools::validate_generic_name(&name)?;
        if !out.contains(&name) {
            out.push(name);
        }
    }
    Ok(out)
}

/// Why a set was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SetSessionToolListError {
    /// `if_version` did not match; the current version.
    VersionConflict(u64),
    /// The session is an app peer's (or a request context's).
    NotEligible,
    /// Writing failed.
    Io(String),
}

fn set_lock(data_dir: &Path, session: &SessionKey) -> Arc<Mutex<()>> {
    static LOCKS: LazyLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    let key = format!("{}\u{0}{}", data_dir.display(), session.0);
    let mut locks = LOCKS.lock().unwrap_or_else(|p| p.into_inner());
    // Bounded: a lock nobody holds is dropped before the map grows large.
    if locks.len() > 1024 {
        locks.retain(|_, lock| Arc::strong_count(lock) > 1);
    }
    locks.entry(key).or_default().clone()
}

/// Set (or with `None` clear) `session`'s list durably. Returns
/// `(previous_version, stored)`. Sets of one session are serialized.
pub(crate) fn set_session_tool_list(
    data_dir: &Path,
    session: &SessionKey,
    generic_tools: Option<Vec<String>>,
    if_version: Option<u64>,
) -> Result<(u64, SessionToolList), SetSessionToolListError> {
    if !session_is_eligible(session) {
        return Err(SetSessionToolListError::NotEligible);
    }
    let lock = set_lock(data_dir, session);
    let _guard = lock.lock().unwrap_or_else(|p| p.into_inner());
    let current = read_session_tool_list(data_dir, session).version();
    if let Some(expected) = if_version {
        if expected != current {
            return Err(SetSessionToolListError::VersionConflict(current));
        }
    }
    let dir = lists_dir(data_dir);
    std::fs::create_dir_all(&dir)
        .map_err(|err| SetSessionToolListError::Io(format!("{}: {err}", dir.display())))?;
    let updated_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    let list = SessionToolList {
        version: current + 1,
        session_id: session.0.clone(),
        generic_tools,
        updated_at,
    };
    let body =
        serde_json::to_string(&list).map_err(|err| SetSessionToolListError::Io(err.to_string()))?;
    peer_io::write_peer_file_durable(&dir, &leaf_for(session), &body).map_err(|err| {
        SetSessionToolListError::Io(format!("failed to record the tool list: {err}"))
    })?;
    Ok((current, list))
}

/// Serve turn path: narrow one turn's registry of `session` to its list.
/// Call it after the profile policy and before any host-routed app tool is
/// added (the list names kernel tools only). Returns whether a list applied.
pub(crate) fn retain_session_tool_list(
    registry: &mut ToolRegistry,
    data_dir: &Path,
    session: &SessionKey,
) -> bool {
    match read_session_tool_list(data_dir, session).allowed() {
        None => false,
        Some(allowed) => {
            registry.retain(|name| allowed.iter().any(|a| a == name));
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(raw: &str) -> SessionKey {
        SessionKey(raw.to_owned())
    }

    #[test]
    fn should_read_none_when_no_list_was_ever_set() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            read_session_tool_list(dir.path(), &key("_main:api:host#system")),
            StoredSessionToolList::None
        );
    }

    #[test]
    fn should_bump_the_version_on_every_set_including_a_clear() {
        let dir = tempfile::tempdir().unwrap();
        let session = key("_main:api:host#system");
        let (prev, first) =
            set_session_tool_list(dir.path(), &session, Some(vec!["read_file".into()]), None)
                .unwrap();
        assert_eq!((prev, first.version), (0, 1));
        let (prev, cleared) = set_session_tool_list(dir.path(), &session, None, Some(1)).unwrap();
        assert_eq!((prev, cleared.version), (1, 2));
        assert_eq!(
            read_session_tool_list(dir.path(), &session).allowed(),
            None,
            "a cleared list leaves the roster alone"
        );
        assert_eq!(
            set_session_tool_list(dir.path(), &session, Some(vec![]), Some(1)),
            Err(SetSessionToolListError::VersionConflict(2))
        );
    }

    #[test]
    fn should_refuse_a_peer_or_context_session_when_setting() {
        let dir = tempfile::tempdir().unwrap();
        for raw in ["_main:api:host#peer-news", "_main:api:host#peerctx-news.c1"] {
            assert_eq!(
                set_session_tool_list(dir.path(), &key(raw), Some(vec![]), None),
                Err(SetSessionToolListError::NotEligible)
            );
        }
    }

    #[test]
    fn should_fail_closed_when_the_stored_list_is_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let session = key("_main:api:host#system");
        set_session_tool_list(dir.path(), &session, Some(vec!["grep".into()]), None).unwrap();
        let path = dir
            .path()
            .join(SESSION_TOOL_LISTS_DIR)
            .join(leaf_for(&session));
        std::fs::write(&path, "{not json").unwrap();
        let stored = read_session_tool_list(dir.path(), &session);
        assert_eq!(stored, StoredSessionToolList::Unreadable);
        assert_eq!(stored.allowed(), Some(Vec::new()));
    }

    #[test]
    fn should_fail_closed_when_a_list_file_names_another_session() {
        let dir = tempfile::tempdir().unwrap();
        let a = key("_main:api:host#system");
        let b = key("_main:api:host#other");
        set_session_tool_list(dir.path(), &a, Some(vec!["grep".into()]), None).unwrap();
        let lists = dir.path().join(SESSION_TOOL_LISTS_DIR);
        std::fs::copy(lists.join(leaf_for(&a)), lists.join(leaf_for(&b))).unwrap();
        assert_eq!(
            read_session_tool_list(dir.path(), &b),
            StoredSessionToolList::Unreadable
        );
    }

    #[test]
    fn should_refuse_dotted_or_too_many_names_when_normalizing() {
        assert!(normalize_tool_list(vec!["news.list".into()]).is_err());
        assert!(normalize_tool_list(vec!["".into()]).is_err());
        let many: Vec<String> = (0..=MAX_SESSION_TOOL_LIST)
            .map(|i| format!("t{i}"))
            .collect();
        assert!(normalize_tool_list(many).is_err());
        assert_eq!(
            normalize_tool_list(vec!["grep".into(), "grep".into(), "glob".into()]).unwrap(),
            vec!["grep".to_owned(), "glob".to_owned()]
        );
    }
}
