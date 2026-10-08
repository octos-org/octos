//! End-to-end proof for #2481 on the production write path.
//!
//! The serve session actor persists turns through
//! [`SessionHandle::add_message_with_seq`], so the legacy-rewrite scenario is
//! driven through that exact entry: a rolled session whose meta line a
//! pre-segments build rewrote (erasing `sealed_segments` while the segment
//! files stay on disk), then appends that attempt a seal into the occupied
//! slot. The seal must refuse to destroy the segment; a session whose meta
//! does name its count must keep sealing as before.

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
        source: None,
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

#[tokio::test]
async fn refused_seal_preserves_sealed_history_on_the_handle_write_path() {
    let tmp = tempfile::tempdir().unwrap();
    let mgr = SessionManager::open(tmp.path()).unwrap();
    let key = SessionKey::new("cli", "e2e-legacy");

    // Roll once through the handle: [seed, oversize] seals as 000001 and
    // "after the roll" starts the fresh active file.
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

    // The legacy rewrite: the same rows, but a meta line of the
    // pre-segments shape — no `sealed_segments` key. No current build can
    // write this, which is the point: it is what an old binary in a shared
    // data directory leaves behind.
    let rows = std::fs::read_to_string(&active).unwrap();
    let body = rows.split_once('\n').unwrap().1.to_owned();
    let legacy_meta = format!(
        "{{\"schema_version\":1,\"session_key\":\"{}\",\"created_at\":\"2026-01-01T00:00:00Z\",\"updated_at\":\"2026-01-01T00:00:00Z\"}}",
        key.0
    );
    std::fs::write(&active, format!("{legacy_meta}\n{body}")).unwrap();

    // The erased count orphans the sealed rows on load.
    let orphaned = mgr.load_full(&key).await.unwrap();
    assert_eq!(orphaned.messages.len(), 1, "only the active row loads");
    assert_eq!(orphaned.messages[0].content, "after the roll");

    // Appends through the handle (the actor's entry) now attempt a seal
    // into the slot the real history occupies.
    let mut handle = SessionHandle::open(tmp.path(), &key);
    handle.add_message_with_seq(oversize()).await.unwrap();
    let seq = handle
        .add_message_with_seq(message(MessageRole::User, "later"))
        .await
        .unwrap();
    drop(handle);

    assert!(
        std::fs::read(&sealed).unwrap() == real_history,
        "the sealed history survives the refused seal byte for byte"
    );
    assert!(
        !sealed_segment_path(&active, 2).is_file(),
        "the refused seal does not chain a segment behind the history it would have destroyed"
    );
    let active_rows = std::fs::read_to_string(&active).unwrap();
    assert!(
        active_rows.contains("\"later\""),
        "the refused roll keeps the row in the active file"
    );
    assert_eq!(seq, 2, "appends continue across the refused seal");

    // The preserved history stays on disk for reconciliation.
    let still = mgr.load_full(&key).await.unwrap();
    let on_disk: Vec<usize> = vec![1]
        .into_iter()
        .filter(|i| sealed_segment_path(&active, *i as u32).is_file())
        .collect();
    assert_eq!(on_disk, vec![1], "the orphaned segment is still on disk");
    assert_eq!(still.messages.len(), 3, "active rows keep loading");
}

#[tokio::test]
async fn a_named_count_keeps_sealing_through_the_handle() {
    let tmp = tempfile::tempdir().unwrap();
    let mgr = SessionManager::open(tmp.path()).unwrap();
    let key = SessionKey::new("cli", "e2e-named");

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
    // The next oversize row rides in the active file; the one after it
    // seals again — the guard must not have discouraged ordinary rolling.
    handle.add_message_with_seq(oversize()).await.unwrap();
    handle
        .add_message_with_seq(message(MessageRole::User, "third"))
        .await
        .unwrap();
    drop(handle);

    let active = active_file(tmp.path());
    assert!(
        sealed_segment_path(&active, 1).is_file() && sealed_segment_path(&active, 2).is_file(),
        "both rolls sealed"
    );
    let meta_line = std::fs::read_to_string(&active).unwrap();
    let meta = meta_line.split_once('\n').unwrap().0;
    assert!(
        meta.contains("\"sealed_segments\":2"),
        "the active meta names both sealed segments: {meta}"
    );
    let meta = mgr.load_full(&key).await.unwrap();
    assert_eq!(
        meta.messages.len(),
        5,
        "both sealed segments load with the active row"
    );
    assert_eq!(meta.messages[0].content, "seed");
    assert_eq!(meta.messages[4].content, "third");
}
