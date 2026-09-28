//! End-to-end proof for #2597 on the production paths.
//!
//! An interrupted seal (#2466's F2) leaves the real history in the segments
//! directory with no active file. `touch_user_session` (the gateway's `/new
//! <topic>` bookkeeping) then wrote a fresh meta naming `sealed_segments: 0`
//! over that state — a count the #2481 guards trust — so the next seal
//! replaced (deleted) the segment the meta does not name. The touch must
//! recover the active file exactly as the loader does instead, and only a
//! session with nothing sealed materializes empty.
//!
//! The gateway's `/new <name>` is the only production caller and its name
//! passes `validate_topic_name`, so the touched sessions here use a named
//! topic — the `{base_key}#{topic}` arm the gateway actually drives.

use std::path::Path;

use octos_bus::session::{SessionHandle, SessionManager};
use octos_core::{Message, MessageRole, SessionKey};

const OVERSIZE: usize = 8 * 1024 * 1024 + 1024; // past the 8 MiB segment size

fn message(role: MessageRole, content: &str) -> Message {
    // Assistant/Tool rows must carry a thread_id on the new-write path.
    let thread_id = match role {
        MessageRole::Assistant | MessageRole::Tool => Some("e2e-thread".to_string()),
        _ => None,
    };
    Message {
        role,
        content: content.into(),
        media: vec![],
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: None,
        client_message_id: None,
        thread_id,
        timestamp: chrono::Utc::now(),
    }
}

fn oversize() -> Message {
    message(MessageRole::Assistant, &"x".repeat(OVERSIZE))
}

fn sealed_segment_path(active: &Path, index: u32) -> std::path::PathBuf {
    active
        .with_extension("segments")
        .join(format!("{index:06}.jsonl"))
}

/// The handle writes to the per-user layout
/// (`users/<encoded base key>/sessions/<topic>.jsonl`); find the session's
/// active file there instead of re-deriving the private naming.
fn active_file(data_dir: &Path) -> std::path::PathBuf {
    let users = data_dir.join("users");
    let mut files = Vec::new();
    let mut stack = vec![users];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                if !path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().ends_with(".segments"))
                {
                    stack.push(path);
                }
            } else if path.extension().is_some_and(|ext| ext == "jsonl") {
                files.push(path);
            }
        }
    }
    assert_eq!(
        files.len(),
        1,
        "one session file under the per-user layout: {files:?}"
    );
    files.pop().unwrap()
}

/// Drive the history to the interrupted-seal crash state through the
/// production write path: `[seed, oversize]` seals as 000001 together,
/// "after the roll" starts the fresh active file, and deleting that file
/// leaves the crash's on-disk shape (rename landed, fresh file never
/// durable). Returns the data dir, the active path, and the sealed bytes.
async fn interrupted_seal_state(base: &str) -> (tempfile::TempDir, std::path::PathBuf, Vec<u8>) {
    let tmp = tempfile::tempdir().unwrap();
    let key = SessionKey(format!("{base}#research"));
    let mut handle = SessionHandle::open(tmp.path(), &key);
    handle
        .add_message_with_seq(message(MessageRole::User, "seed"))
        .await
        .unwrap();
    handle.add_message_with_seq(oversize()).await.unwrap();
    handle
        .add_message_with_seq(message(MessageRole::User, "after the roll"))
        .await
        .unwrap();
    drop(handle);

    let active = active_file(tmp.path());
    let sealed = sealed_segment_path(&active, 1);
    assert!(sealed.is_file(), "the first roll sealed a segment");
    let real_history = std::fs::read(&sealed).unwrap();
    std::fs::remove_file(&active).unwrap();
    (tmp, active, real_history)
}

#[tokio::test]
async fn touch_over_recoverable_seal_state_does_not_rearm_segment_replacement() {
    let base = "cli:e2e-touch";
    let (tmp, active, real_history) = interrupted_seal_state(base).await;
    let mgr = SessionManager::open(tmp.path()).unwrap();
    let key = SessionKey(format!("{base}#research"));
    let sealed = sealed_segment_path(&active, 1);

    // `/new research` touches the session before any load — the window the
    // issue describes (gateway_dispatcher.rs:184).
    mgr.touch_user_session(base, "research");

    // The touch must not write a meta that names zero segments while the
    // sealed history sits beside the path: it recovers the active file the
    // same way the loader does, keeping the session's topic identity.
    let meta_line = std::fs::read_to_string(&active).unwrap();
    let meta = meta_line.split_once('\n').unwrap().0;
    assert!(
        meta.contains("\"sealed_segments\":1"),
        "the touch materialized the recovered active file: {meta}"
    );
    assert!(
        meta.contains("\"topic\":\"research\""),
        "the recovered meta keeps the topic identity: {meta}"
    );

    // The recovered session loads with its real history, and the next seal
    // rolls BEHIND it instead of replacing it.
    let loaded = mgr.load_full(&key).await.unwrap();
    assert_eq!(
        loaded.messages.len(),
        2,
        "the sealed segment loads after the touch"
    );
    assert_eq!(loaded.messages[0].content, "seed");

    let mut handle = SessionHandle::open(tmp.path(), &key);
    handle.add_message_with_seq(oversize()).await.unwrap();
    handle
        .add_message_with_seq(message(MessageRole::User, "later"))
        .await
        .unwrap();
    drop(handle);

    assert!(
        std::fs::read(&sealed).unwrap() == real_history,
        "the sealed history survives the post-touch seal byte for byte"
    );
    assert!(
        sealed_segment_path(&active, 2).is_file(),
        "the post-touch seal rolled into the next slot"
    );
    let loaded = mgr.load_full(&key).await.unwrap();
    assert_eq!(loaded.messages.len(), 4, "all four visible rows load");
    assert_eq!(loaded.messages[0].content, "seed");
    assert_eq!(loaded.messages[3].content, "later");
    let listed = mgr.list_top_level_sessions();
    assert!(
        listed.iter().any(|(id, _)| id == &key.0),
        "the touched topic session is discoverable: {listed:?}"
    );
}

#[tokio::test]
async fn touch_still_materializes_a_fresh_empty_session() {
    let tmp = tempfile::tempdir().unwrap();
    let mgr = SessionManager::open(tmp.path()).unwrap();
    let key = SessionKey::new("cli", "e2e-fresh");

    // No history at all: the touch's own purpose — an empty, discoverable
    // session file — must be unchanged.
    mgr.touch_user_session(key.base_key(), "");

    let active = active_file(tmp.path());
    let meta_line = std::fs::read_to_string(&active).unwrap();
    let meta = meta_line.split_once('\n').unwrap().0;
    assert!(
        meta.contains("\"sealed_segments\":0"),
        "a fresh session starts at zero sealed segments: {meta}"
    );
    let listed = mgr.list_top_level_sessions();
    assert!(
        listed
            .iter()
            .any(|(id, _)| id == &key.0 || id.starts_with(&format!("{}#", key.0))),
        "the touched session is discoverable: {listed:?}"
    );
}

#[tokio::test]
async fn touch_over_noncontiguous_segments_keeps_the_fresh_zero_meta() {
    let tmp = tempfile::tempdir().unwrap();
    let mgr = SessionManager::open(tmp.path()).unwrap();
    let base = "cli:e2e-gap";
    let key = SessionKey(format!("{base}#research"));

    // A lone 000002 with no 000001 is the loader's residue shape: the
    // contiguous count is zero, so a fresh meta naming zero is truthful and
    // the next seal rolls into 000001 without touching the strayed file.
    // This pins the contiguity contract the recover-or-fresh decision rides
    // on — counting non-contiguous files here would flip the touch into
    // recovering from a directory the loader never loads.
    let active = tmp
        .path()
        .join("users")
        .join(octos_bus::session::encode_path_component(base))
        .join("sessions")
        .join("research.jsonl");
    std::fs::create_dir_all(active.with_extension("segments")).unwrap();
    std::fs::write(sealed_segment_path(&active, 2), "{\"row\":true}\n").unwrap();

    mgr.touch_user_session(base, "research");

    let meta_line = std::fs::read_to_string(&active).unwrap();
    let meta = meta_line.split_once('\n').unwrap().0;
    assert!(
        meta.contains("\"sealed_segments\":0"),
        "a noncontiguous segments dir leaves the fresh zero meta in place: {meta}"
    );
    let _loaded = mgr.load_full(&key).await.unwrap();
    assert!(
        sealed_segment_path(&active, 2).is_file(),
        "the stray segment stays on disk"
    );
}

/// The recovery-write failure path: when the recovered active file cannot be
/// written, the touch leaves the state for the loader instead of falling
/// back to the zeroed meta the deletion vector needs.
#[cfg(unix)]
#[tokio::test]
async fn touch_with_unwritable_sessions_dir_leaves_the_state_for_the_loader() {
    use std::os::unix::fs::PermissionsExt;

    let base = "cli:e2e-ro";
    let (tmp, active, _real_history) = interrupted_seal_state(base).await;
    let mgr = SessionManager::open(tmp.path()).unwrap();
    let sessions_dir = active.parent().unwrap().to_path_buf();

    std::fs::set_permissions(&sessions_dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    mgr.touch_user_session(base, "research");
    std::fs::set_permissions(&sessions_dir, std::fs::Permissions::from_mode(0o755)).unwrap();

    assert!(
        !active.exists(),
        "the failed recovery does not fall back to the zeroed meta"
    );
    assert!(
        sealed_segment_path(&active, 1).is_file(),
        "the sealed history is untouched"
    );
}
