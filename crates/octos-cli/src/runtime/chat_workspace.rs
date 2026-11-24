//! One server-created workspace per chat (UPCR-2026-R1EFC56A3).
//!
//! A chat's folder and record live in one of two layouts. Profiles known when
//! the server starts keep the original `<profile data_dir>/chats/<name>/` and
//! `<profile data_dir>/chats/<name>.json`. Profiles registered while the
//! server runs use the stable root `<server data_dir>/chat-workspaces/<profile>/<name>/`;
//! that root exists before the first sandbox, so a shell command's hiding
//! rules already cover every future runtime profile. The record sits outside
//! the folder, so nothing the chat writes can change whose folder it is. Both
//! layouts use the same record shape and are looked up together. If a session
//! resolves in both, the lookup fails closed instead of choosing one.
//!
//! The folder is the chat's whole filesystem: its session runtime is scoped to
//! it with no shared zones, its tools are cut down to [`CHAT_TOOLS`], and its
//! transcript stays in the profile store rather than inside the folder. The
//! folder lives as long as the session and is removed when the session is
//! deleted.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

use chrono::{DateTime, Utc};
use octos_core::SessionKey;
use serde::{Deserialize, Serialize};

const CHATS_DIR: &str = "chats";
const CHAT_WORKSPACES_DIR: &str = "chat-workspaces";

#[derive(Default)]
struct ChatWorkspaceLayout {
    /// Stable root known before the first sandbox is built. New runtime
    /// profiles create chats here; legacy profiles keep their old root.
    server_root: Option<PathBuf>,
    /// Server-owned roots that are safe to recreate while wrapping a command.
    /// A root leaves this set as soon as it is known to hold content.
    healable_server_roots: std::collections::BTreeSet<PathBuf>,
    /// Canonical server root -> identity captured while it existed.
    server_root_identities: std::collections::HashMap<PathBuf, String>,
    /// Server roots that may hold chats and therefore require their recorded
    /// directory identity to match on every command wrap.
    populated_server_roots: std::collections::BTreeSet<PathBuf>,
    /// Canonical profile data dir -> server-level profile root chosen for it.
    /// Keeping this separate from the current server root keeps unit-test
    /// states independent and preserves a profile across unrelated reconfigures.
    profile_server_roots: std::collections::HashMap<PathBuf, PathBuf>,
    /// Canonical profile data dir -> that profile's pre-upgrade chats root.
    legacy_profiles: std::collections::HashMap<PathBuf, PathBuf>,
}

fn chat_layout() -> &'static RwLock<ChatWorkspaceLayout> {
    static LAYOUT: std::sync::OnceLock<RwLock<ChatWorkspaceLayout>> = std::sync::OnceLock::new();
    LAYOUT.get_or_init(|| RwLock::new(ChatWorkspaceLayout::default()))
}

fn layout_read() -> RwLockReadGuard<'static, ChatWorkspaceLayout> {
    chat_layout()
        .read()
        .unwrap_or_else(|error| error.into_inner())
}

fn layout_write() -> RwLockWriteGuard<'static, ChatWorkspaceLayout> {
    chat_layout()
        .write()
        .unwrap_or_else(|error| error.into_inner())
}

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

pub(crate) fn chats_root(data_dir: &Path) -> PathBuf {
    data_dir.join(CHATS_DIR)
}

fn canonical_dir(path: &Path) -> PathBuf {
    octos_core::canonicalize_lossy(path)
}

fn registered_path(path: &Path) -> PathBuf {
    path.components().collect()
}

fn root_identity(path: &Path) -> std::io::Result<String> {
    let metadata = std::fs::metadata(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(format!("{}:{}", metadata.dev(), metadata.ino()))
    }
    #[cfg(not(unix))]
    {
        Ok(format!("{:?}", metadata.created().ok()))
    }
}

fn track_populated_server_root(root: &Path) -> std::io::Result<()> {
    let root = registered_path(root);
    let current = root_identity(&root)?;
    let expected = layout_read().server_root_identities.get(&root).cloned();
    if expected.as_deref() != Some(current.as_str()) {
        return Err(std::io::Error::other(format!(
            "server chats root {} changed identity before becoming populated",
            root.display()
        )));
    }
    let mut layout = layout_write();
    layout.healable_server_roots.remove(&root);
    layout.populated_server_roots.insert(root);
    Ok(())
}

fn overlaps_path(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

fn profile_folder_name(profile_id: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = format!("{:x}", Sha256::digest(profile_id.as_bytes()));
    digest[..32].to_owned()
}

fn server_root() -> Option<PathBuf> {
    layout_read().server_root.clone()
}

fn server_profile_root(profile_id: &str) -> Option<PathBuf> {
    server_root().map(|root| root.join(profile_folder_name(profile_id)))
}

fn server_profile_roots(data_dir: &Path, profile_id: &str) -> Vec<PathBuf> {
    let profile_dir = canonical_dir(data_dir);
    let mut roots = Vec::new();
    if let Some(root) = layout_read().profile_server_roots.get(&profile_dir) {
        roots.push(root.clone());
    }
    if let Some(root) = server_profile_root(profile_id)
        && !roots.contains(&root)
    {
        roots.push(root);
    }
    roots
}

/// Validate the one server-level root. A profile data directory may contain
/// the root (standard installs often use the state home itself); the real
/// conflict is with that profile's legacy chats root. A workspace may likewise
/// be an ancestor, but never at or below the root.
fn validate_server_root(
    root: &Path,
    profile_data_dirs: &[PathBuf],
    workspace_roots: &[PathBuf],
) -> std::io::Result<()> {
    for profile_dir in profile_data_dirs {
        let legacy_root = canonical_dir(&chats_root(profile_dir));
        if overlaps_path(root, &legacy_root) {
            return Err(std::io::Error::other(format!(
                "server chats root {} overlaps legacy profile chats root {}",
                root.display(),
                legacy_root.display()
            )));
        }
    }
    for workspace in workspace_roots {
        let workspace = canonical_dir(workspace);
        if workspace.starts_with(root) {
            return Err(std::io::Error::other(format!(
                "allowed workspace {} is inside server chats root {}",
                workspace.display(),
                root.display()
            )));
        }
    }
    Ok(())
}

/// Move the pre-release #3326 root into the release root. Both locations on
/// the same startup mean we cannot prove a merge is safe; refuse instead of
/// silently choosing one.
fn migrate_server_root(
    data_dir: &Path,
    profile_data_dirs: &[PathBuf],
    root: &Path,
) -> std::io::Result<()> {
    let old_raw = data_dir.join(CHATS_DIR);
    let new_raw = data_dir.join(CHAT_WORKSPACES_DIR);
    let old_meta = match std::fs::symlink_metadata(&old_raw) {
        Ok(meta) => Some(meta),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    if old_meta
        .as_ref()
        .is_some_and(|meta| meta.file_type().is_symlink())
    {
        return Err(std::io::Error::other(format!(
            "old server chats root {} is a symlink; refusing to move its target",
            old_raw.display()
        )));
    }
    if let Ok(meta) = std::fs::symlink_metadata(&new_raw)
        && meta.file_type().is_symlink()
    {
        return Err(std::io::Error::other(format!(
            "server chats root {} is a symlink; refusing to write through it",
            new_raw.display()
        )));
    }
    let old = canonical_dir(&old_raw);
    if old == root {
        return Ok(());
    }
    if profile_data_dirs
        .iter()
        .any(|profile| canonical_dir(&chats_root(profile)) == old)
    {
        // The old path is this profile's legacy root, not the pre-release
        // server root. It may hold pre-upgrade chats and must stay put.
        return Ok(());
    }
    if profile_data_dirs
        .iter()
        .any(|profile| canonical_dir(profile).starts_with(&old))
    {
        return Err(std::io::Error::other(format!(
            "old server chats root {} contains a configured profile data directory; refusing to move the profile",
            old_raw.display()
        )));
    }
    let Some(old_meta) = old_meta else {
        return Ok(());
    };
    if !old_meta.is_dir() {
        return Err(std::io::Error::other(format!(
            "old server chats root {} is not a directory",
            old_raw.display()
        )));
    }
    let old_empty = std::fs::read_dir(&old)?.next().is_none();
    match std::fs::symlink_metadata(&new_raw) {
        Ok(meta) if meta.is_dir() => {
            if std::fs::read_dir(&new_raw)?.next().is_some() && !old_empty {
                return Err(std::io::Error::other(format!(
                    "cannot move old server chats root {} to {}: both directories contain data",
                    old_raw.display(),
                    new_raw.display()
                )));
            }
            if old_empty {
                std::fs::remove_dir(&old_raw)?;
                return Ok(());
            }
            std::fs::remove_dir(&new_raw)?;
        }
        Ok(_) => {
            return Err(std::io::Error::other(format!(
                "server chats root {} is not a directory",
                new_raw.display()
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    std::fs::rename(&old_raw, &new_raw).map_err(|error| {
        std::io::Error::other(format!(
            "cannot move old server chats root {} to {}: {error}",
            old_raw.display(),
            new_raw.display()
        ))
    })
}

/// Create and reserve the one server-level root. Its stable address is known
/// before any sandbox is built, so even a profile created later is already
/// hidden from every command started by this process.
pub(crate) fn configure_server_root(
    data_dir: &Path,
    profile_data_dirs: &[PathBuf],
    workspace_roots: &[PathBuf],
) -> std::io::Result<PathBuf> {
    let root = registered_path(&canonical_dir(&data_dir.join(CHAT_WORKSPACES_DIR)));
    validate_server_root(&root, profile_data_dirs, workspace_roots)?;
    migrate_server_root(data_dir, profile_data_dirs, &root)?;
    private_dir(&root, true)?;
    let identity = root_identity(&root)?;
    let is_empty = root.read_dir()?.next().is_none();
    reserved_folders()
        .add(root.clone())
        .map_err(std::io::Error::other)?;
    let mut layout = layout_write();
    layout.server_root = Some(root.clone());
    layout.server_root_identities.insert(root.clone(), identity);
    if is_empty {
        layout.healable_server_roots.insert(root.clone());
    }
    drop(layout);
    if !is_empty {
        track_populated_server_root(&root)?;
    }
    Ok(root)
}

/// A workspace inside the fixed server-level chats root is invalid. A larger
/// ancestor workspace (commonly `$HOME` for a standard install) is valid: the
/// root itself is already in the process-wide hidden set before any command
/// starts, matching the pre-existing per-profile chat hiding.
#[cfg(feature = "api")]
pub(crate) fn workspace_inside_server_root(path: &Path) -> bool {
    let Some(root) = server_root() else {
        return false;
    };
    canonical_dir(path).starts_with(canonical_dir(&root))
}

/// Stop hiding a deleted legacy profile's chats root. Runtime-created
/// profiles share the stable server root, which remains registered for the
/// other profiles.
#[cfg(feature = "api")]
pub(crate) fn unregister_profile(data_dir: &Path) {
    let profile_dir = canonical_dir(data_dir);
    let mut layout = layout_write();
    layout.profile_server_roots.remove(&profile_dir);
    if layout.legacy_profiles.remove(&profile_dir).is_some() {
        drop(layout);
        reserved_folders().remove(&chats_root(&profile_dir));
    }
}

/// Let the shared hidden-folder list repair the server-owned Chat root at
/// command wrap time. A legacy root is never healed from inside a sandboxed
/// command: it may have been renamed by an earlier command, so its absence
/// cannot prove that its contents are gone.
pub(crate) fn install_missing_root_healer() {
    reserved_folders().on_missing_folder(Arc::new(move |folder, command_roots| {
        heal_missing_folder(folder, command_roots)
    }));
    reserved_folders().on_existing_folder(Arc::new(|folder, resolved| {
        verify_existing_folder(folder, resolved)
    }));
}

fn verify_existing_folder(folder: &Path, resolved: &Path) -> Result<(), String> {
    // The registered path is the lookup key; `resolved` only describes the
    // object currently found there.
    let folder = registered_path(folder);
    if std::fs::symlink_metadata(&folder)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err(format!(
            "hidden chats path {} is a symlink replacement",
            folder.display()
        ));
    }
    let populated = layout_read().populated_server_roots.contains(&folder);
    if !populated {
        return Ok(());
    }
    let expected = layout_read().server_root_identities.get(&folder).cloned();
    let current = root_identity(resolved).map_err(|error| {
        format!(
            "server chats root {} cannot be identified: {error}",
            folder.display()
        )
    })?;
    if expected.as_deref() != Some(current.as_str()) {
        return Err(format!(
            "server chats root {} was replaced instead of restored",
            folder.display()
        ));
    }
    Ok(())
}

fn heal_missing_folder(folder: &Path, command_roots: &[PathBuf]) -> Result<bool, String> {
    // The registered path is the lookup key; normalizing strips a trailing
    // separator without resolving a symlink replacement.
    let folder = registered_path(folder);
    if layout_read().healable_server_roots.contains(&folder) {
        if command_roots
            .iter()
            .any(|root| overlaps_path(&folder, &canonical_dir(root)))
        {
            return Err(format!(
                "server chats root {} overlaps a workspace/read root available to this command",
                folder.display()
            ));
        }
        private_dir(&folder, true).map_err(|error| {
            format!(
                "server chats root {} is missing and could not be recreated: {error}",
                folder.display()
            )
        })?;
        let identity = root_identity(&folder).map_err(|error| {
            format!(
                "server chats root {} was recreated but its identity could not be read: {error}",
                folder.display()
            )
        })?;
        layout_write()
            .server_root_identities
            .insert(folder.clone(), identity);
        tracing::warn!(path = %folder.display(), "recreated missing server chats root");
        return Ok(true);
    }
    Err(format!(
        "hidden chats root {} is missing. Refusing to run the command; restore the directory, or have an operator verify its contents are deleted and unregister it with the repair flow.",
        folder.display()
    ))
}

#[cfg(all(test, feature = "api"))]
static HIDDEN_FIXTURE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(all(test, feature = "api"))]
pub(crate) struct HiddenFixtureGuard {
    root: PathBuf,
    // Layout, reserved folders and root identities are process-global. One
    // fixture at a time prevents another async test from populating or moving
    // this fixture's root between wrapping and execution.
    _lock: std::sync::MutexGuard<'static, ()>,
}

#[cfg(all(test, feature = "api"))]
pub(crate) fn hidden_fixture_guard(root: &Path) -> HiddenFixtureGuard {
    HiddenFixtureGuard {
        root: canonical_dir(root),
        _lock: HIDDEN_FIXTURE_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner()),
    }
}

#[cfg(all(test, feature = "api"))]
pub(crate) fn tempdir_with_hidden_guard() -> (tempfile::TempDir, HiddenFixtureGuard) {
    let temp = tempfile::tempdir().expect("tempdir");
    let guard = hidden_fixture_guard(temp.path());
    (temp, guard)
}

#[cfg(all(test, feature = "api"))]
impl Drop for HiddenFixtureGuard {
    fn drop(&mut self) {
        let mut layout = layout_write();
        layout
            .legacy_profiles
            .retain(|data_dir, _| !data_dir.starts_with(&self.root));
        layout
            .profile_server_roots
            .retain(|data_dir, _| !data_dir.starts_with(&self.root));
        layout
            .healable_server_roots
            .retain(|root| !root.starts_with(&self.root));
        layout
            .server_root_identities
            .retain(|root, _| !root.starts_with(&self.root));
        layout
            .populated_server_roots
            .retain(|root| !root.starts_with(&self.root));
        if layout
            .server_root
            .as_ref()
            .is_some_and(|root| root.starts_with(&self.root))
        {
            layout.server_root = None;
        }
        drop(layout);
        let reserved = reserved_folders();
        for folder in reserved.folders() {
            if canonical_dir(&folder).starts_with(&self.root) {
                reserved.remove(&folder);
            }
        }
    }
}

/// A runtime-created profile must never move the server-level chats root into
/// its own data directory, or its data directory into that root. This is the
/// runtime half of the startup validation in `octos serve`.
pub(crate) fn ensure_profile_data_dir_safe(data_dir: &Path) -> std::io::Result<()> {
    let Some(root) = server_root() else {
        return Ok(());
    };
    let profile_dir = canonical_dir(data_dir);
    if overlaps_path(&root, &profile_dir) {
        return Err(std::io::Error::other(format!(
            "profile data directory {} overlaps server chats root {}",
            profile_dir.display(),
            root.display()
        )));
    }
    Ok(())
}

/// A chat's folder and record name, the same for every spelling of a
/// session id that reaches the chat's transcript: the session store files the
/// `default` topic as the bare id, and storage that folds case (as most macOS
/// and Windows volumes do) makes ids that differ only in ASCII case one
/// transcript. Where storage tells case apart, those stay separate sessions.
/// [`lookup`] refuses every spelling but the chat's own. Normalization follows
/// the profile's transcript store (`data_dir`), even when the chat folder is
/// under the server-level root on another volume.
fn folder_name(data_dir: &Path, session_id: &SessionKey) -> String {
    use sha2::{Digest, Sha256};
    let base = session_id.base_key();
    let base = if folds_case(data_dir) {
        base.to_ascii_lowercase()
    } else {
        base.to_owned()
    };
    let digest = format!("{:x}", Sha256::digest(base.as_bytes()));
    digest[..32].to_owned()
}

/// Whether the profile's transcript storage folds ASCII case. `sessions` is
/// the authoritative runtime store directory; `chats` is the fallback for a
/// legacy-only data directory that has not initialized sessions yet.
fn folds_case(data_dir: &Path) -> bool {
    for (lower, upper) in [
        (data_dir.join("sessions"), data_dir.join("SESSIONS")),
        (
            chats_root(data_dir),
            data_dir.join(CHATS_DIR.to_ascii_uppercase()),
        ),
    ] {
        if let (Ok(lower), Ok(upper)) = (std::fs::metadata(lower), std::fs::metadata(upper))
            && same_entry(&lower, &upper)
        {
            return true;
        }
    }
    false
}

#[cfg(unix)]
fn same_entry(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    (a.dev(), a.ino()) == (b.dev(), b.ino())
}

/// Both spellings resolve: only storage that folds case has both here.
#[cfg(not(unix))]
fn same_entry(_: &std::fs::Metadata, _: &std::fs::Metadata) -> bool {
    true
}

/// Whether `session_id` may be a spelling of a chat's transcript: no topic,
/// or the default one. Any other topic is a transcript of its own.
fn may_be_chat(session_id: &SessionKey) -> bool {
    session_id
        .topic()
        .is_none_or(|topic| topic.eq_ignore_ascii_case("default"))
}

fn record_candidates(data_dir: &Path, session_id: &SessionKey) -> Vec<(PathBuf, PathBuf)> {
    let mut roots = vec![chats_root(data_dir)];
    if let Some(profile_id) = session_id.profile_id() {
        let server_roots = server_profile_roots(data_dir, profile_id)
            .into_iter()
            .filter(|root| !roots.contains(root))
            .collect::<Vec<_>>();
        roots.extend(server_roots);
    }
    roots
        .into_iter()
        .map(|root| {
            let name = folder_name(data_dir, session_id);
            (root.clone(), root.join(format!("{name}.json")))
        })
        .collect()
}

/// The chat `session_id` owns under `data_dir`. `Ok(None)`: it has no chat
/// record. An error: it has one whose binding cannot be used (the record names
/// another session, as for another spelling of the chat's id, or the folder is
/// missing, unreadable or a symlink). A recorded chat stays a chat, so every
/// caller refuses on the error instead of running the session as an ordinary
/// one. Legacy and server roots are both checked; records in both fail closed.
pub(crate) fn lookup(
    data_dir: &Path,
    session_id: &SessionKey,
) -> std::io::Result<Option<ChatWorkspace>> {
    let Some((record, root)) = read_record(data_dir, session_id)? else {
        return Ok(None);
    };
    let folder = root.join(folder_name(data_dir, session_id));
    if !std::fs::symlink_metadata(&folder)?.is_dir() {
        return Err(unusable("its folder is not a directory"));
    }
    Ok(Some(ChatWorkspace {
        session_id: session_id.clone(),
        profile_id: record.profile_id,
        root: std::fs::canonicalize(&folder)?,
    }))
}

/// `session_id`'s chat record and its root, if it has one; one naming another
/// session is an error. A session with a topic of its own has none. Finding a
/// record in both roots refuses rather than choosing one.
fn read_record(
    data_dir: &Path,
    session_id: &SessionKey,
) -> std::io::Result<Option<(ChatRecord, PathBuf)>> {
    if !may_be_chat(session_id) {
        return Ok(None);
    }
    let mut found = None;
    for (root, path) in record_candidates(data_dir, session_id) {
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if found.is_some() {
            return Err(unusable("its record exists in multiple chat roots"));
        }
        let record: ChatRecord = serde_json::from_slice(&bytes)?;
        if record.session_id != session_id.0 {
            return Err(unusable("its record names another session"));
        }
        found = Some((record, root));
    }
    Ok(found)
}

fn unusable(why: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("chat workspace unusable: {why}"),
    )
}

#[cfg(feature = "api")]
fn creation_root(data_dir: &Path, profile_id: &str) -> std::io::Result<PathBuf> {
    let profile_dir = canonical_dir(data_dir);
    if layout_read().legacy_profiles.contains_key(&profile_dir) {
        return Ok(chats_root(data_dir));
    }
    if let Some(root) = layout_read().profile_server_roots.get(&profile_dir) {
        return Ok(root.clone());
    }
    if let Some(root) = server_profile_root(profile_id) {
        let server_root = server_root().expect("server_profile_root implies server_root");
        // Once a profile can create a chat here, disappearance no longer proves
        // the root was empty. Validate and pin the populated identity before
        // caching this profile's mapping.
        track_populated_server_root(&server_root)?;
        layout_write()
            .profile_server_roots
            .insert(profile_dir, root.clone());
        return Ok(root);
    }
    if cfg!(test) {
        // Existing inline fixtures exercise the legacy layout without a serve
        // startup. Production servers always configure the root or fail to
        // start, so this branch is not a runtime escape hatch.
        return Ok(chats_root(data_dir));
    }
    Err(std::io::Error::other(
        "server chats root is not configured for a runtime-created profile",
    ))
}

/// Create the chat's folder and record, or return the existing ones for the
/// same session (a retried create is harmless). Existing profiles registered
/// at server startup stay in their legacy root; profiles registered later use
/// the stable server root.
#[cfg(feature = "api")]
pub(crate) fn create(
    data_dir: &Path,
    profile_id: &str,
    session_id: &SessionKey,
) -> std::io::Result<ChatWorkspace> {
    if let Some(existing) = lookup(data_dir, session_id)? {
        return Ok(existing);
    }
    let root = creation_root(data_dir, profile_id)?;
    if root == chats_root(data_dir) {
        // No chat exists that other sessions are not kept out of.
        reserve_profile(data_dir)?;
    } else {
        private_dir(&root, true)?;
    }
    let name = folder_name(data_dir, session_id);
    private_dir(&root.join(&name), false)?;
    let record = serde_json::to_vec(&ChatRecord {
        session_id: session_id.0.clone(),
        profile_id: profile_id.to_owned(),
        created_at: Utc::now(),
    })?;
    let path = root.join(format!("{name}.json"));
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
    let Some((_record, root)) = read_record(data_dir, session_id)? else {
        return Ok(());
    };
    if lookup(data_dir, session_id).is_ok() {
        return Ok(());
    }
    let folder = root.join(folder_name(data_dir, session_id));
    match std::fs::symlink_metadata(&folder) {
        Ok(meta) if !meta.is_dir() => std::fs::remove_file(&folder)?,
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    private_dir(&folder, false)
}

/// A real directory only the server user can enter. A leftover folder from an
/// interrupted create is reused; a symlink in its place is refused.
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

/// Every chat recorded for `profile_id`, across its legacy and server roots.
/// A duplicate or unusable record is skipped here; direct lookup refuses it.
#[cfg(feature = "api")]
pub(crate) fn list(data_dir: &Path, profile_id: &str) -> Vec<ChatWorkspace> {
    let mut roots = vec![chats_root(data_dir)];
    let server_roots = server_profile_roots(data_dir, profile_id)
        .into_iter()
        .filter(|root| !roots.contains(root))
        .collect::<Vec<_>>();
    roots.extend(server_roots);
    roots
        .into_iter()
        .flat_map(|root| {
            let entries = match std::fs::read_dir(root) {
                Ok(entries) => entries,
                Err(_) => return Vec::new(),
            };
            entries
                .flatten()
                .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
                .filter_map(|entry| {
                    let record: ChatRecord =
                        serde_json::from_slice(&std::fs::read(entry.path()).ok()?).ok()?;
                    Some(record.session_id)
                })
                .collect::<Vec<_>>()
        })
        .filter_map(|session_id| lookup(data_dir, &SessionKey(session_id)).ok()?)
        .collect()
}

/// Whether a workspace at `path` would reach into the chat folders under
/// `data_dir`: it lies in one, or it holds them from inside the data
/// directory (the data directory itself, or its `chats` folder). A folder
/// above the data directory is not refused: the data directory may sit in a
/// project (`--data-dir`, `OCTOS_HOME`) or in the user's home, and refusing
/// those would refuse the project or the home itself.
#[cfg(feature = "api")]
pub(crate) fn overlaps_chat_folders(data_dir: &Path, path: &Path) -> bool {
    let data_dir = std::fs::canonicalize(data_dir).unwrap_or_else(|_| data_dir.to_path_buf());
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let holds = path.starts_with(&data_dir) && chats_root(&data_dir).starts_with(&path);
    let server_holds =
        server_root().is_some_and(|root| overlaps_path(&canonical_dir(&root), &path));
    holds || server_holds || is_in_chat_folder(&data_dir, &path)
}

/// The chats folders of the profiles this process serves, shared by every
/// scope and `send_file` of a session that is not a chat, so a profile that
/// starts later still keeps its chats from sessions already running.
pub(crate) fn reserved_folders() -> octos_core::HiddenFolders {
    static RESERVED: std::sync::OnceLock<octos_core::HiddenFolders> = std::sync::OnceLock::new();
    RESERVED.get_or_init(Default::default).clone()
}

/// Reserve a profile's chats folder as the profile starts. A profile already
/// known when the server starts is legacy and keeps its old root; a profile
/// registered later reserves the stable server root instead, so the folder
/// that will hold its future chats existed before every sandbox did.
pub(crate) fn register_profile(data_dir: &Path) {
    let profile_dir = canonical_dir(data_dir);
    let result = if layout_read().legacy_profiles.contains_key(&profile_dir) {
        reserve_profile(data_dir)
    } else if let Some(root) = server_root() {
        reserved_folders()
            .add(root.clone())
            .map_err(std::io::Error::other)
            .and_then(|()| private_dir(&root, true))
    } else {
        // A non-server process has no server-level root; preserve the old
        // per-profile layout there. `octos serve` always configures one.
        reserve_profile(data_dir)
    };
    if let Err(error) = result {
        tracing::warn!(%error, "cannot reserve this profile's chats");
    }
}

/// A profile that existed when the server started keeps its pre-upgrade
/// chats root — but only when that root holds real pre-upgrade data. A
/// profile installed after the server-level root shipped has an empty or
/// absent legacy root; marking it legacy would split its chats across roots
/// after the first restart (T-0670). Runtime-created profiles are
/// deliberately not marked here.
pub(crate) fn register_existing_profile(data_dir: &Path) -> std::io::Result<()> {
    let profile_dir = canonical_dir(data_dir);
    if !has_pre_upgrade_chat_data(&profile_dir)? {
        return Ok(());
    }
    reserve_profile(&profile_dir)?;
    layout_write()
        .legacy_profiles
        .insert(profile_dir.clone(), chats_root(&profile_dir));
    Ok(())
}

/// Whether the profile's legacy chats root holds something a pre-upgrade
/// server must have written: any entry at all. An empty directory left
/// behind by an older startup, or no directory, proves nothing. A symlink or
/// a non-directory entry counts as evidence so the root stays reserved and
/// reads of it fail closed instead of silently rerouting new chats.
fn has_pre_upgrade_chat_data(profile_dir: &Path) -> std::io::Result<bool> {
    let root = chats_root(profile_dir);
    let metadata = match std::fs::symlink_metadata(&root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if !metadata.is_dir() {
        return Ok(true);
    }
    Ok(std::fs::read_dir(&root)?.next().is_some())
}

/// [`register_profile`], by the folder the data directory resolves to: a
/// custom one may be spelled with `..` or through a symlinked parent. The
/// folder is made now, so a sandbox can hide it from a shell that started
/// before the profile's first chat.
fn reserve_profile(data_dir: &Path) -> std::io::Result<()> {
    let root = chats_root(&canonical_dir(data_dir));
    reserved_folders()
        .add(root.clone())
        .map_err(std::io::Error::other)?;
    private_dir(&root, true)
}

/// The chats folders a session that is not a chat keeps out of: its own
/// profile's under `data_dir` and those of every profile this process
/// serves, since one workspace (a project or home above them) may hold
/// several profiles' data directories. Each as named and canonical.
#[cfg(feature = "api")]
pub(crate) fn reserved_roots(data_dir: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for root in std::iter::once(chats_root(data_dir)).chain(reserved_folders().folders()) {
        roots.extend(std::fs::canonicalize(&root));
        roots.push(root);
    }
    roots.sort();
    roots.dedup();
    roots
}

/// Whether `path` is in a reserved chats folder ([`reserved_roots`]) of any
/// of `data_dirs`: a chat's folder or files, or a chat's record.
#[cfg(feature = "api")]
pub(crate) fn is_reserved<'a>(data_dirs: impl IntoIterator<Item = &'a Path>, path: &Path) -> bool {
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    data_dirs
        .into_iter()
        .flat_map(reserved_roots)
        .any(|root| path.starts_with(root))
}

/// The chats folders that `session_id`'s workspace file reads and listings
/// skip ([`reserved_roots`]); none for a chat, which reads only its own
/// folder.
#[cfg(feature = "api")]
pub(crate) fn hidden_from(data_dir: &Path, session_id: &SessionKey) -> Vec<PathBuf> {
    if !matches!(lookup(data_dir, session_id), Ok(None)) {
        return Vec::new();
    }
    reserved_roots(data_dir)
}

/// Whether `path` is inside one of the legacy chat folders under `data_dir`,
/// or inside the server-level root.
pub(crate) fn is_in_chat_folder(data_dir: &Path, path: &Path) -> bool {
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mut roots = vec![chats_root(data_dir)];
    roots.extend(server_root());
    roots.into_iter().any(|root| {
        let Ok(root) = std::fs::canonicalize(root) else {
            return false;
        };
        path.strip_prefix(&root)
            .is_ok_and(|rest| rest.components().next().is_some())
    })
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
    let Some((_record, root)) = read_record(data_dir, session_id)? else {
        return Ok(());
    };
    #[cfg(test)]
    if FAIL_FOLDER_REMOVAL.with(std::cell::Cell::get) {
        return Err(std::io::Error::other("injected folder removal failure"));
    }
    let folder = root.join(folder_name(data_dir, session_id));
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
    // Never another session's: a topic of its own has none, and another
    // spelling of a chat's id leaves the chat's. A record too damaged to
    // name its session still goes. Duplicate records fail closed instead of
    // choosing one.
    if !may_be_chat(session_id) {
        return Ok(());
    }
    let candidates = record_candidates(data_dir, session_id);
    let mut present = 0;
    for (_root, path) in &candidates {
        let named = std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<ChatRecord>(&bytes).ok());
        if named.is_some_and(|record| record.session_id != session_id.0) {
            return Err(unusable("its record names another session"));
        }
        if path.try_exists()? {
            present += 1;
        }
    }
    if present > 1 {
        return Err(unusable("its record exists in multiple chat roots"));
    }
    for (_root, path) in candidates {
        match std::fs::remove_file(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            result => result?,
        }
    }
    Ok(())
}

/// Cut a registry down to the compiled-in [`CHAT_TOOLS`]: a plugin or MCP tool
/// that takes one of their names is dropped too.
pub(crate) fn confine_chat_tools(tools: &mut octos_agent::ToolRegistry) {
    tools.retain_builtin(|name| CHAT_TOOLS.contains(&name));
}

#[cfg(all(test, feature = "api"))]
mod server_root_tests {
    use super::*;

    static LAYOUT_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// A chat folder and record written the way a pre-upgrade server left
    /// them, so startup registration sees real legacy data (T-0670).
    fn plant_legacy_chat(data_dir: &Path, profile_id: &str, session_id: &SessionKey) -> PathBuf {
        let root = chats_root(data_dir);
        let name = folder_name(data_dir, session_id);
        let folder = root.join(&name);
        private_dir(&folder, true).unwrap();
        std::fs::write(
            root.join(format!("{name}.json")),
            serde_json::to_vec(&ChatRecord {
                session_id: session_id.0.clone(),
                profile_id: profile_id.to_owned(),
                created_at: Utc::now(),
            })
            .unwrap(),
        )
        .unwrap();
        folder
    }

    #[tokio::test]
    async fn should_route_a_fresh_profile_to_the_server_root_after_every_restart() {
        let _layout = LAYOUT_TEST_LOCK.lock().await;
        let temp = tempfile::tempdir().unwrap();
        let profile_dir = temp.path().join("profiles/fresh/data");
        std::fs::create_dir_all(&profile_dir).unwrap();
        let profile_dir = canonical_dir(&profile_dir);
        let mut roots = Vec::new();
        for boot in 0..3 {
            let hidden = hidden_fixture_guard(temp.path());
            let server_root =
                configure_server_root(temp.path(), std::slice::from_ref(&profile_dir), &[])
                    .unwrap();
            // Registration at every startup must not mark the fresh profile
            // legacy: it has no pre-upgrade chat data (T-0670).
            register_existing_profile(&profile_dir).unwrap();
            assert!(
                !layout_read().legacy_profiles.contains_key(&profile_dir),
                "boot {boot} marked a fresh profile legacy"
            );
            let chat = create(
                &profile_dir,
                "fresh",
                &SessionKey::with_profile("fresh", "api", &format!("chat-{boot}")),
            )
            .unwrap();
            assert!(chat.root.starts_with(&server_root), "{chat:?}");
            roots.push(chat.root);
            drop(hidden);
        }
        // A fresh profile never gains a legacy root with content.
        let legacy_root = chats_root(&profile_dir);
        assert!(
            std::fs::read_dir(&legacy_root)
                .map(|mut entries| entries.next().is_none())
                .unwrap_or(true),
            "{} must stay empty",
            legacy_root.display()
        );
    }

    #[tokio::test]
    async fn should_not_treat_an_empty_legacy_dir_as_pre_upgrade_data() {
        let _layout = LAYOUT_TEST_LOCK.lock().await;
        let (temp, _hidden) = tempdir_with_hidden_guard();
        let profile_dir = temp.path().join("profiles/empty/data");
        // An older startup created this directory for every profile; it is not
        // evidence of pre-upgrade chats.
        std::fs::create_dir_all(chats_root(&profile_dir)).unwrap();
        let server_root =
            configure_server_root(temp.path(), std::slice::from_ref(&profile_dir), &[]).unwrap();
        register_existing_profile(&profile_dir).unwrap();
        let chat = create(
            &profile_dir,
            "empty",
            &SessionKey::with_profile("empty", "api", "chat"),
        )
        .unwrap();
        assert!(chat.root.starts_with(&server_root), "{chat:?}");
    }

    #[test]
    fn should_refuse_server_root_overlapping_profile_or_workspace() {
        let (temp, hidden) = tempdir_with_hidden_guard();
        let server = temp.path().join("server");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let profile_in_server_root = server.join(CHAT_WORKSPACES_DIR);
        assert!(
            configure_server_root(
                &server,
                std::slice::from_ref(&profile_in_server_root),
                std::slice::from_ref(&workspace)
            )
            .is_err()
        );
        let home = temp.path().join("home");
        let server_root = canonical_dir(&home.join("state").join(CHAT_WORKSPACES_DIR));
        validate_server_root(&server_root, &[], std::slice::from_ref(&home))
            .expect("standard install home workspace may contain the hidden root");
        assert!(
            validate_server_root(&server_root, &[], std::slice::from_ref(&server_root)).is_err()
        );
        assert!(
            validate_server_root(
                &server_root,
                &[],
                std::slice::from_ref(&server_root.join("workspace"))
            )
            .is_err()
        );
        let profile_file = temp.path().join("profile-file");
        let failed_root = chats_root(&profile_file);
        std::fs::create_dir_all(&profile_file).unwrap();
        std::fs::write(&failed_root, b"not a directory").unwrap();
        let failed_root = canonical_dir(&failed_root);
        assert!(register_existing_profile(&profile_file).is_err());
        assert!(reserved_folders().folders().contains(&failed_root));
        drop(hidden);
        assert!(!reserved_folders().folders().contains(&failed_root));
    }

    async fn run_hidden_command(root: &Path, command: &str) -> Option<std::process::Output> {
        // Other tests share this process-global list. Drop only stale fixture
        // roots outside this test's TempDir; a missing root under this fixture
        // is the case under test and must reach the resolver.
        let reserved = reserved_folders();
        for folder in reserved.folders() {
            if !folder.exists() && !folder.starts_with(root) {
                reserved.remove(&folder);
            }
        }
        for mode in [
            octos_agent::SandboxMode::Bwrap,
            octos_agent::SandboxMode::Macos,
        ] {
            let sandbox = octos_agent::create_sandbox(&octos_agent::SandboxConfig {
                mode,
                hidden_folders: Some(reserved_folders()),
                ..octos_agent::SandboxConfig::default()
            });
            if sandbox.refusal().is_some() {
                println!("{} unavailable on this host", sandbox.backend_name());
                continue;
            }
            return sandbox.wrap_command(command, root).output().await.ok();
        }
        None
    }

    #[tokio::test]
    async fn should_recreate_missing_server_root_before_command() {
        let _layout = LAYOUT_TEST_LOCK.lock().await;
        let (temp, _hidden) = tempdir_with_hidden_guard();
        let server_root = configure_server_root(temp.path(), &[], &[]).unwrap();
        install_missing_root_healer();
        let work = temp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::remove_dir_all(&server_root).unwrap();
        assert!(
            reserved_folders()
                .resolve_missing_folder(&server_root, std::slice::from_ref(&work))
                .unwrap()
        );
        let marker = work.join("ran");
        let Some(output) =
            run_hidden_command(&work, &format!("printf healed > \"{}\"", marker.display())).await
        else {
            return;
        };
        assert!(
            output.status.success(),
            "status={:?} stdout={} stderr={}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(server_root.is_dir());
        let profile_dir = temp.path().join("profiles/new/data");
        std::fs::create_dir_all(&profile_dir).unwrap();
        let chat = create(
            &profile_dir,
            "new",
            &SessionKey::with_profile("new", "api", "healed"),
        )
        .unwrap();
        assert!(chat.root.starts_with(&server_root));
        std::fs::write(chat.root.join("secret.txt"), "server-secret").unwrap();
        let output = run_hidden_command(
            &work,
            &format!("cat \"{}\"", chat.root.join("secret.txt").display()),
        )
        .await
        .unwrap();
        assert!(!output.status.success());

        let moved_root = temp.path().join("renamed-healed-chats");
        let moved_secret = moved_root
            .join(chat.root.strip_prefix(&server_root).unwrap())
            .join("secret.txt");
        std::fs::rename(&server_root, &moved_root).unwrap();
        reserved_folders()
            .resolve_missing_folder(&server_root, std::slice::from_ref(&work))
            .expect_err("a populated healed root must not be replaced by a fresh empty one");
        assert!(moved_secret.is_file());
        assert!(reserved_folders().folders().contains(&server_root));

        std::fs::rename(&moved_root, &server_root).unwrap();
        reserved_folders()
            .resolve_existing_folder(&server_root, &server_root)
            .unwrap();
    }

    #[tokio::test]
    async fn should_refuse_server_root_inside_command_roots() {
        let _layout = LAYOUT_TEST_LOCK.lock().await;
        let (temp, _hidden) = tempdir_with_hidden_guard();
        let server_root = configure_server_root(temp.path(), &[], &[]).unwrap();
        install_missing_root_healer();
        std::fs::remove_dir_all(&server_root).unwrap();
        let error = reserved_folders()
            .resolve_missing_folder(&server_root, &[server_root.join("code-workspace")])
            .expect_err("a command root inside the server root must not self-heal");
        assert!(
            error.contains(server_root.to_string_lossy().as_ref()),
            "{error}"
        );
        assert!(error.contains("overlaps a workspace/read root"), "{error}");
        assert!(!server_root.exists());
    }

    #[tokio::test]
    async fn should_refuse_renamed_server_root_inside_command_roots() {
        let _layout = LAYOUT_TEST_LOCK.lock().await;
        let (temp, _hidden) = tempdir_with_hidden_guard();
        let profile_dir = temp.path().join("profiles/live/data");
        std::fs::create_dir_all(&profile_dir).unwrap();
        let server_root =
            configure_server_root(temp.path(), std::slice::from_ref(&profile_dir), &[]).unwrap();
        install_missing_root_healer();
        let chat = create(
            &profile_dir,
            "live",
            &SessionKey::with_profile("live", "api", "server"),
        )
        .unwrap();
        let secret = chat.root.join("secret.txt");
        std::fs::write(&secret, "server-secret").unwrap();
        let moved_root = temp.path().join("renamed-server-chats");
        let moved_secret = moved_root
            .join(chat.root.strip_prefix(&server_root).unwrap())
            .join("secret.txt");
        std::fs::rename(&server_root, &moved_root).unwrap();
        let unrelated = temp.path().join("unrelated-work");
        std::fs::create_dir_all(&unrelated).unwrap();

        let first = reserved_folders()
            .resolve_missing_folder(&server_root, std::slice::from_ref(&unrelated))
            .expect_err("an unrelated workspace must not recreate a populated server root");
        let second = reserved_folders()
            .resolve_missing_folder(&server_root, &[temp.path().to_path_buf()])
            .expect_err("the ancestor workspace stays blocked on the renamed root");

        assert!(
            first.contains(server_root.to_string_lossy().as_ref()),
            "{first}"
        );
        assert!(
            second.contains(server_root.to_string_lossy().as_ref()),
            "{second}"
        );
        assert!(!server_root.exists());
        assert!(moved_secret.is_file());
        assert!(reserved_folders().folders().contains(&server_root));

        std::fs::create_dir_all(&server_root).unwrap();
        let other_profile = temp.path().join("profiles/other/data");
        std::fs::create_dir_all(&other_profile).unwrap();
        let identity_error = create(
            &other_profile,
            "other",
            &SessionKey::with_profile("other", "api", "replacement"),
        )
        .expect_err("a replacement directory must not be blessed for another profile");
        assert!(
            identity_error.to_string().contains("changed identity"),
            "{identity_error}"
        );
        assert!(
            !layout_read()
                .profile_server_roots
                .contains_key(&canonical_dir(&other_profile))
        );

        let command_marker = unrelated.join("ran");
        if let Some(output) = run_hidden_command(
            &unrelated,
            &format!("printf ran > \"{}\"", command_marker.display()),
        )
        .await
        {
            assert!(
                !output.status.success(),
                "status={:?} stdout={} stderr={}",
                output.status.code(),
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::fs::remove_dir_all(&server_root).unwrap();

        let server_path: PathBuf = server_root.components().collect();
        #[cfg(unix)]
        {
            assert!(moved_root.is_dir());
            assert!(server_path.parent().unwrap().is_dir());
            std::fs::create_dir_all(server_path.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(&moved_root, &server_path).unwrap_or_else(|error| {
                panic!(
                    "symlink {} -> {} failed: {error}",
                    moved_root.display(),
                    server_path.display()
                )
            });
            if let Some(output) = run_hidden_command(
                &unrelated,
                &format!("printf ran > \"{}\"", command_marker.display()),
            )
            .await
            {
                assert!(
                    !output.status.success(),
                    "status={:?} stdout={} stderr={}",
                    output.status.code(),
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            std::fs::remove_file(&server_path).unwrap();
        }

        std::fs::rename(&moved_root, &server_path).unwrap();
        reserved_folders()
            .resolve_existing_folder(&server_path, &server_path)
            .unwrap();
        assert!(reserved_folders().folders().contains(&server_root));
        if let Some(output) = run_hidden_command(
            &unrelated,
            &format!("printf ran > \"{}\"", command_marker.display()),
        )
        .await
        {
            assert!(
                output.status.success(),
                "status={:?} stdout={} stderr={}",
                output.status.code(),
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[tokio::test]
    async fn should_refuse_renamed_legacy_root_without_exposing_it() {
        let _layout = LAYOUT_TEST_LOCK.lock().await;
        let (temp, _hidden) = tempdir_with_hidden_guard();
        let profile_dir = temp.path().join("profiles/live/data");
        std::fs::create_dir_all(&profile_dir).unwrap();
        let old_id = SessionKey::with_profile("live", "api", "old");
        plant_legacy_chat(&profile_dir, "live", &old_id);
        configure_server_root(temp.path(), std::slice::from_ref(&profile_dir), &[]).unwrap();
        register_existing_profile(&profile_dir).unwrap();
        install_missing_root_healer();
        let old_chat = create(&profile_dir, "live", &old_id).unwrap();
        std::fs::write(old_chat.root.join("secret.txt"), "old-secret").unwrap();
        let legacy_root = canonical_dir(&chats_root(&profile_dir));
        let moved_root = temp.path().join("renamed-chats");
        let moved_secret = moved_root
            .join(old_chat.root.strip_prefix(&legacy_root).unwrap())
            .join("secret.txt");
        std::fs::rename(&legacy_root, &moved_root).unwrap();
        let error = reserved_folders()
            .resolve_missing_folder(&legacy_root, &[temp.path().to_path_buf()])
            .expect_err("a renamed legacy root stays fail-closed");
        assert!(
            error.contains(legacy_root.to_string_lossy().as_ref()),
            "{error}"
        );
        assert!(error.contains("restore the directory"), "{error}");
        assert!(reserved_folders().folders().contains(&legacy_root));
        let Some(output) =
            run_hidden_command(temp.path(), &format!("cat \"{}\"", moved_secret.display())).await
        else {
            return;
        };
        assert!(
            !output.status.success(),
            "status={:?} stdout={} stderr={}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(moved_secret.exists());
    }

    #[tokio::test]
    async fn should_allow_main_profile_data_dir_equal_state_home() {
        let _guard = LAYOUT_TEST_LOCK.lock().await;
        let temp = tempfile::tempdir().unwrap();
        let legacy = temp.path().join(CHATS_DIR).join("old-chat.txt");
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, "legacy").unwrap();
        let root = configure_server_root(
            temp.path(),
            std::slice::from_ref(&temp.path().to_path_buf()),
            std::slice::from_ref(&temp.path().to_path_buf()),
        )
        .expect("state-home profile data dir is valid");
        assert_eq!(root, canonical_dir(&temp.path().join(CHAT_WORKSPACES_DIR)));
        register_existing_profile(temp.path()).expect("legacy root for the same directory");
        assert_ne!(root, canonical_dir(&chats_root(temp.path())));
        assert_eq!(std::fs::read_to_string(&legacy).unwrap(), "legacy");
    }

    #[tokio::test]
    async fn should_move_pre_release_server_chats_root_on_startup() {
        let _guard = LAYOUT_TEST_LOCK.lock().await;
        let temp = tempfile::tempdir().unwrap();
        let old = temp.path().join(CHATS_DIR).join("profile").join("chat");
        let new = temp.path().join(CHAT_WORKSPACES_DIR).join("profile/chat");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("note.txt"), "kept").unwrap();
        let root = configure_server_root(temp.path(), &[], &[]).expect("migrate old root");
        assert_eq!(root, canonical_dir(&temp.path().join(CHAT_WORKSPACES_DIR)));
        assert!(!temp.path().join(CHATS_DIR).exists());
        assert_eq!(
            std::fs::read_to_string(new.join("note.txt")).unwrap(),
            "kept"
        );
    }

    #[tokio::test]
    async fn should_refuse_moving_server_chats_root_containing_profile() {
        let _guard = LAYOUT_TEST_LOCK.lock().await;
        let temp = tempfile::tempdir().unwrap();
        let profile_dir = temp.path().join(CHATS_DIR).join("profiles/custom/data");
        std::fs::create_dir_all(&profile_dir).unwrap();
        std::fs::write(profile_dir.join("profile.json"), "{}").unwrap();
        let old = temp.path().join(CHATS_DIR).join("server-chat.txt");
        std::fs::write(&old, "server").unwrap();
        let error = configure_server_root(temp.path(), std::slice::from_ref(&profile_dir), &[])
            .expect_err("migration must not move a configured profile");
        assert!(
            error
                .to_string()
                .contains("contains a configured profile data directory"),
            "{error}"
        );
        assert!(profile_dir.join("profile.json").exists());
        assert!(old.exists());
        assert!(!temp.path().join(CHAT_WORKSPACES_DIR).exists());
    }

    #[tokio::test]
    async fn should_refuse_moving_server_chats_root_over_existing_data() {
        let _guard = LAYOUT_TEST_LOCK.lock().await;
        let temp = tempfile::tempdir().unwrap();
        let old = temp.path().join(CHATS_DIR).join("old.txt");
        let new = temp.path().join(CHAT_WORKSPACES_DIR).join("new.txt");
        std::fs::create_dir_all(old.parent().unwrap()).unwrap();
        std::fs::create_dir_all(new.parent().unwrap()).unwrap();
        std::fs::write(&old, "old").unwrap();
        std::fs::write(&new, "new").unwrap();
        let error = configure_server_root(temp.path(), &[], &[]).expect_err("both roots have data");
        assert!(
            error.to_string().contains("both directories contain data"),
            "{error}"
        );
        assert!(old.exists() && new.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn should_refuse_moving_server_chats_root_through_symlinks() {
        let _guard = LAYOUT_TEST_LOCK.lock().await;
        let source_case = tempfile::tempdir().unwrap();
        let source_target = tempfile::tempdir().unwrap();
        std::fs::write(source_target.path().join("kept.txt"), "kept").unwrap();
        std::os::unix::fs::symlink(source_target.path(), source_case.path().join(CHATS_DIR))
            .unwrap();
        let error = configure_server_root(source_case.path(), &[], &[])
            .expect_err("symlink source is not moved");
        assert!(error.to_string().contains("is a symlink"), "{error}");
        assert!(
            source_case
                .path()
                .join(CHATS_DIR)
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(source_target.path().join("kept.txt").exists());
        assert!(!source_case.path().join(CHAT_WORKSPACES_DIR).exists());

        let destination_case = tempfile::tempdir().unwrap();
        let destination_target = tempfile::tempdir().unwrap();
        let old = destination_case.path().join(CHATS_DIR).join("old.txt");
        std::fs::create_dir_all(old.parent().unwrap()).unwrap();
        std::fs::write(&old, "old").unwrap();
        std::os::unix::fs::symlink(
            destination_target.path(),
            destination_case.path().join(CHAT_WORKSPACES_DIR),
        )
        .unwrap();
        let error = configure_server_root(destination_case.path(), &[], &[])
            .expect_err("symlink destination is not written through");
        assert!(error.to_string().contains("is a symlink"), "{error}");
        assert!(old.exists());
        assert!(
            destination_case
                .path()
                .join(CHAT_WORKSPACES_DIR)
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[tokio::test]
    async fn should_unreserve_deleted_legacy_profile_root() {
        let _layout = LAYOUT_TEST_LOCK.lock().await;
        let temp = tempfile::tempdir().unwrap();
        let root_dir = temp.path().to_path_buf();
        std::mem::forget(temp);
        let state_home = root_dir.join("state");
        let profile = root_dir.join("profiles/deleted/data");
        std::fs::create_dir_all(&profile).unwrap();
        plant_legacy_chat(
            &profile,
            "deleted",
            &SessionKey::with_profile("deleted", "api", "old"),
        );
        configure_server_root(&state_home, std::slice::from_ref(&profile), &[])
            .expect("configure server root");
        register_existing_profile(&profile).expect("legacy profile root");
        let legacy_root = canonical_dir(&chats_root(&profile));
        let reserved = reserved_folders();
        assert!(reserved.folders().contains(&legacy_root));

        std::fs::remove_dir_all(&profile).unwrap();
        unregister_profile(&profile);
        assert!(!reserved.folders().contains(&legacy_root));
    }

    async fn new_profile_command_stays_fenced(mode: octos_agent::SandboxMode) {
        let _guard = LAYOUT_TEST_LOCK.lock().await;
        let temp = tempfile::tempdir().unwrap();
        // The layout registry and hidden-folder list are process-global. Keep
        // this root alive for the test process so a concurrently running test
        // never follows a deleted registered path.
        let root_dir = temp.path().to_path_buf();
        std::mem::forget(temp);
        let state_home = root_dir.join("state");
        let runtime_data = root_dir.join("runtime");
        let old_profile = root_dir.join("profiles/old/data");
        std::fs::create_dir_all(&old_profile).unwrap();
        // The command's cwd deliberately contains the shared state root and
        // the separate runtime profile root, matching a standard `$HOME`
        // workspace; the fixed root itself is what must be hidden.
        let server_root =
            configure_server_root(&state_home, std::slice::from_ref(&old_profile), &[])
                .expect("configure server root");
        assert_eq!(
            server_root,
            canonical_dir(&state_home.join(CHAT_WORKSPACES_DIR))
        );
        assert!(!workspace_inside_server_root(&root_dir));
        assert!(workspace_inside_server_root(&server_root));
        assert!(overlaps_chat_folders(&old_profile, &root_dir));
        assert!(ensure_profile_data_dir_safe(&server_root).is_err());
        let profile_store =
            crate::profiles::ProfileStore::open(&state_home, &runtime_data).expect("profile store");
        let bad_profile = crate::profiles::UserProfile {
            id: "bad".to_owned(),
            name: "Bad".to_owned(),
            public_subdomain: None,
            enabled: true,
            data_dir: Some(server_root.display().to_string()),
            parent_id: None,
            config: crate::profiles::ProfileConfig::default(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        assert!(profile_store.save(&bad_profile).is_err());
        let old_id = SessionKey::with_profile("old", "api", "old-chat");
        plant_legacy_chat(&old_profile, "old", &old_id);
        register_existing_profile(&old_profile).expect("legacy profile root");

        let old_chat = create(&old_profile, "old", &old_id).expect("legacy chat");
        assert!(
            old_chat
                .root
                .starts_with(canonical_dir(&chats_root(&old_profile)))
        );

        let new_record = crate::profiles::UserProfile {
            id: "new".to_owned(),
            name: "New".to_owned(),
            public_subdomain: None,
            enabled: true,
            data_dir: None,
            parent_id: None,
            config: crate::profiles::ProfileConfig::default(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let new_profile = profile_store.resolve_data_dir(&new_record);
        let new_id = SessionKey::with_profile("new", "api", "late-chat");
        let new_root = server_profile_root("new").expect("server profile root");
        let name = folder_name(&new_profile, &new_id);
        let secret = new_root.join(&name).join("secret.txt");
        let marker = root_dir.join("command-started");
        let command = if cfg!(windows) {
            // NoSandbox on Windows must refuse before this ever runs; success
            // would prove the static hidden-root fence was missing.
            "exit 0".to_owned()
        } else {
            format!(
                "printf started > \"{}\"; i=0; while [ $i -lt 100 ]; do \
                 cat \"{}\" && exit 0; i=$((i+1)); sleep 0.02; done; exit 2",
                marker.display(),
                secret.display()
            )
        };
        // Other tests may have dropped their TempDirs after registering
        // fixture roots in this process-global list. Remove stale fixture
        // entries here; the roots under test are created by production calls.
        let reserved = reserved_folders();
        for root in reserved.folders().into_iter().filter(|root| !root.exists()) {
            reserved.remove(&root);
        }
        let sandbox = octos_agent::create_sandbox(&octos_agent::SandboxConfig {
            mode,
            hidden_folders: Some(reserved_folders()),
            ..octos_agent::SandboxConfig::default()
        });
        if sandbox.refusal().is_some() {
            println!("{} unavailable on this host", sandbox.backend_name());
            return;
        }
        let mut command = sandbox.wrap_command(&command, &root_dir);
        command
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let child = command.spawn().expect("spawn long command");
        if !sandbox.is_noop() {
            for _ in 0..100 {
                if marker.exists() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            assert!(marker.exists(), "sandbox command did not start");
        }
        profile_store.save(&new_record).expect("runtime profile");
        register_profile(&new_profile);
        assert!(!chats_root(&new_profile).exists());
        let new_chat = create(&new_profile, "new", &new_id).expect("new chat");
        assert!(new_chat.root.starts_with(canonical_dir(&new_root)));
        std::fs::write(new_chat.root.join("secret.txt"), "late-chat-secret").unwrap();
        let output = child.wait_with_output().await.expect("wait for command");
        let said = String::from_utf8_lossy(&[output.stdout, output.stderr].concat()).into_owned();
        assert!(!said.contains("late-chat-secret"), "{said}");
        assert!(!output.status.success(), "{said}");
        if sandbox.is_noop() {
            assert!(said.contains("cannot hide chat folders"), "{said}");
        }
        remove(&new_profile, &new_id).expect("remove server-root chat");
        assert!(!new_chat.root.exists());
        let new_chat = create(&new_profile, "new", &new_id).expect("recreate chat");
        assert!(new_chat.root.starts_with(canonical_dir(&new_root)));

        #[cfg(unix)]
        {
            // Simulate the transcript store's case-folding behavior. A
            // case-only spelling of the same transcript must be refused, not
            // normalized by the server-root filesystem.
            let sessions = new_profile.join("sessions");
            std::fs::create_dir_all(&sessions).unwrap();
            // #3326 (macOS APFS, BH3GEI 11-23): the SIMULATION is only needed
            // on a case-SENSITIVE filesystem. On a case-folding filesystem
            // `sessions` and `SESSIONS` already name the same directory entry,
            // so symlink(SESSIONS -> sessions) fails EEXIST. Probe the host FS
            // once (write a lower-case file, read it back by its upper-case
            // name); when it already folds case, skip the symlink — the FS is
            // the case-fold, and folds_case()/lookup() assertions below run
            // exactly as on a case-sensitive host.
            let probe = new_profile.join(".octos-casefold-probe");
            std::fs::write(&probe, b"probe").unwrap();
            let fs_folds_case = new_profile.join(".OCTOS-CASEFOLD-PROBE").exists();
            std::fs::remove_file(&probe).ok();
            if !fs_folds_case {
                std::os::unix::fs::symlink(&sessions, new_profile.join("SESSIONS")).unwrap();
            }
            assert!(folds_case(&new_profile));
            let variant = SessionKey::with_profile("new", "api", "LATE-CHAT");
            let error = lookup(&new_profile, &variant).expect_err("case variant is not the chat");
            assert!(
                error.to_string().contains("names another session"),
                "{error}"
            );
        }

        // Reverse-mutation guard: a same-session record in the legacy root
        // makes lookup fail closed; implementation must never pick one root.
        let legacy_root = chats_root(&new_profile);
        private_dir(&legacy_root, true).unwrap();
        private_dir(
            legacy_root
                .join(folder_name(&new_profile, &new_id))
                .as_path(),
            false,
        )
        .unwrap();
        std::fs::write(
            legacy_root.join(format!("{}.json", folder_name(&new_profile, &new_id))),
            serde_json::to_vec(&ChatRecord {
                session_id: new_id.0.clone(),
                profile_id: "new".to_owned(),
                created_at: Utc::now(),
            })
            .unwrap(),
        )
        .unwrap();
        let error = lookup(&new_profile, &new_id).expect_err("duplicate roots fail closed");
        assert!(error.to_string().contains("multiple chat roots"), "{error}");
        assert!(create(&new_profile, "new", &new_id).is_err());
        assert!(remove_folder(&new_profile, &new_id).is_err());
        assert!(remove_record(&new_profile, &new_id).is_err());
        assert!(
            list(&new_profile, "new")
                .iter()
                .all(|chat| chat.session_id != new_id)
        );
    }

    #[tokio::test]
    async fn should_refuse_no_sandbox_command_started_before_runtime_profile() {
        new_profile_command_stays_fenced(octos_agent::SandboxMode::None).await;
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn should_hide_new_profile_chat_from_bwrap_command() {
        new_profile_command_stays_fenced(octos_agent::SandboxMode::Bwrap).await;
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn should_hide_new_profile_chat_from_macos_command() {
        new_profile_command_stays_fenced(octos_agent::SandboxMode::Macos).await;
    }
}
