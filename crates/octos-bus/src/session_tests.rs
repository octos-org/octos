use super::*;

use octos_core::MessageRole;
use tempfile::TempDir;

fn make_message(role: MessageRole, content: &str) -> Message {
    // PR F (M8.10): pre-stamp Assistant/Tool messages with a synthetic
    // thread_id so they pass the new-write fail-closed check. Tests
    // that exercise the legacy untagged path use bare struct literals
    // or the dedicated helpers directly. Production code uses the
    // typed `Message::assistant_with_thread`/`tool_with_thread`
    // constructors and the canonical `persist_assistant_message`
    // helper, both of which already supply thread_id.
    let thread_id = match role {
        MessageRole::Assistant | MessageRole::Tool => Some("test-thread-default".to_string()),
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
        timestamp: Utc::now(),
        source: None,
        participant: None,
    }
}

#[tokio::test]
async fn concurrent_canonical_contract_upserts_lose_no_updates() {
    // The production fanout race: N children terminate together and each
    // stamps its terminal contract into the SHARED parent session. A
    // contract write is a whole-file read-modify-write (open snapshots
    // the file, rewrite renames over it), so writers that read the same
    // pre-state erase each other — the canonical path must hold the
    // per-key persist lock across open→mutate→rewrite.
    //
    // On the single-threaded test runtime every future runs its
    // synchronous open() before any rewrite lands, so WITHOUT the lock
    // all 16 read the empty pre-state and exactly one contract survives
    // — the loss is deterministic, not timing-dependent.
    let tmp = TempDir::new().unwrap();
    let parent = SessionKey::new("api", "fanout-parent");
    {
        let mut parent_handle = SessionHandle::open(tmp.path(), &parent);
        parent_handle
            .add_message(make_message(MessageRole::User, "seed"))
            .await
            .unwrap();
    }

    let make_contract = |i: usize| ChildSessionContract {
        task_id: format!("task-{i}"),
        task_label: format!("worker {i}"),
        parent_session_key: parent.to_string(),
        child_session_key: child_session_key(&parent, &format!("task-{i}")).to_string(),
        workflow_kind: Some("deep_research".to_string()),
        current_phase: Some("deliver_result".to_string()),
        terminal_state: Some(ChildSessionTerminalState::Completed),
        join_state: Some(ChildSessionJoinState::Joined),
        joined_at: Some(Utc::now()),
        failure_action: None,
        error: None,
        output_files: vec![],
    };

    let futures = (0..16)
        .map(|i| {
            upsert_child_contract_through_canonical_path(tmp.path(), &parent, make_contract(i))
        })
        .collect::<Vec<_>>();
    for result in futures::future::join_all(futures).await {
        result.expect("every contract upsert must commit");
    }

    let reloaded = SessionHandle::open(tmp.path(), &parent);
    assert_eq!(
        reloaded.session().child_contracts.len(),
        16,
        "every concurrently-upserted contract must survive on disk (lost-update race)"
    );
    // The seeded message must survive the rewrites too.
    assert_eq!(reloaded.session().messages.len(), 1);
}

#[tokio::test]
async fn should_surface_active_read_error_instead_of_committing_past_tail() {
    use std::io::Write;

    let tmp = TempDir::new().unwrap();
    let key = SessionKey::new("api", "invalid-utf8-active-watermark");
    let mut handle = SessionHandle::open(tmp.path(), &key);
    handle
        .add_message_with_seq(make_message(MessageRole::User, "seed"))
        .await
        .unwrap();
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(handle.session_path())
        .unwrap();
    file.write_all(&[0xff, b'\n']).unwrap();
    file.sync_all().unwrap();
    drop(file);

    let error = handle
        .add_message_with_seq(make_message(MessageRole::User, "must not commit"))
        .await
        .expect_err("an unreadable active row must not be treated as the end of history");
    assert!(error.to_string().contains("committed seq"), "{error}");
    assert!(
        !String::from_utf8_lossy(&std::fs::read(handle.session_path()).unwrap())
            .contains("must not commit")
    );
}

/// A row big enough that the active file passes the segment size, so the
/// NEXT append seals it. Rolling is decided per append from the file's size,
/// which keeps these tests free of process-global knobs.
fn oversize_row() -> Message {
    make_message(
        MessageRole::Assistant,
        &"x".repeat(SESSION_SEGMENT_BYTES_DEFAULT as usize + 1024),
    )
}

/// Drive `mgr`'s session past the segment size: seed, one oversize row, then
/// one small row that lands in a fresh active file. Returns the committed
/// seqs in order.
async fn roll_once(mgr: &mut SessionManager, key: &SessionKey) -> Vec<usize> {
    let mut seqs = Vec::new();
    for message in [
        make_message(MessageRole::User, "seed"),
        oversize_row(),
        make_message(MessageRole::User, "after the roll"),
    ] {
        seqs.push(mgr.add_message_with_seq(key, message).await.unwrap());
    }
    seqs
}

fn contents(session: &Session) -> Vec<String> {
    session
        .messages
        .iter()
        .map(|m| {
            if m.content.len() > 32 {
                format!("<{} bytes>", m.content.len())
            } else {
                m.content.clone()
            }
        })
        .collect()
}

/// Two sealed segments: `roll_once` seals [seed, big] as 000001, then a
/// second oversize row makes the next append seal [after the roll, big] as
/// 000002, leaving `third` in the active file.
async fn roll_twice(mgr: &mut SessionManager, key: &SessionKey) {
    roll_once(mgr, key).await;
    mgr.add_message(key, oversize_row()).await.unwrap();
    mgr.add_message(key, make_message(MessageRole::User, "third"))
        .await
        .unwrap();
}

/// Overwrite a file's first line (the meta) with garbage, keeping the rows.
fn corrupt_meta_line(path: &std::path::Path) {
    let content = std::fs::read_to_string(path).unwrap();
    let rows = content.split_once('\n').map(|(_, rest)| rest).unwrap_or("");
    std::fs::write(path, format!("{{\"schema_version\":1,\"broken\n{rows}")).unwrap();
}

#[tokio::test]
async fn should_rebuild_after_a_seal_when_the_newest_segment_meta_is_unreadable() {
    let tmp = TempDir::new().unwrap();
    let mut mgr = SessionManager::open(tmp.path()).unwrap();
    let key = SessionKey::new("cli", "double-crash");
    roll_twice(&mut mgr, &key).await;
    let active = mgr.session_path(&key);
    // Crash one: the fresh active file never made it. Crash two: the newest
    // sealed segment lost its first line.
    std::fs::remove_file(&active).unwrap();
    corrupt_meta_line(&segment_path(&segments_dir(&active), 2));
    mgr.cache.pop(&key.0);

    let recovered = mgr.load_full(&key).await.unwrap();
    assert_eq!(recovered.sealed_segments, 2);
    assert_eq!(
        recovered.next_seq(),
        4,
        "rows behind the bad line are counted; the chain does not restart at 0"
    );
    assert_eq!(
        recovered.title.as_deref(),
        Some("seed"),
        "identity from 000001"
    );
    assert_eq!(read_session_meta(&active).unwrap().base_seq, 4);
    assert_eq!(
        contents(&recovered),
        vec![
            "seed",
            "<8389632 bytes>",
            "after the roll",
            "<8389632 bytes>"
        ]
    );

    mgr.cache.put(key.0.clone(), recovered);
    let seq = mgr
        .add_message_with_seq(&key, make_message(MessageRole::User, "resumed"))
        .await
        .unwrap();
    assert_eq!(seq, 4);
}

// T-0216: a broken flat `sessions` branch (plain file, file link, dangling
// or loop link) must neither redirect users discovery to a link target's
// parent nor swallow the store's own legal users rows, and listing stays
// read-only (no directory is created in place of the broken branch).
#[tokio::test]
async fn broken_flat_sessions_branch_keeps_store_users_without_redirect_or_mkdir() {
    for kind in ["file", "file-link", "dangling-link", "loop-link"] {
        if kind != "file" && cfg!(windows) {
            // File/dir symlinks need privilege on Windows; the plain-file
            // case below is the cross-platform equivalent of the same guard.
            continue;
        }
        let tmp = TempDir::new().unwrap();
        let store = tmp.path().join("store");
        let foreign = tmp.path().join("foreign");
        for (root, key, title) in [
            (&store, "alice", "Own session"),
            (&foreign, "mallory", "Foreign session"),
        ] {
            std::fs::create_dir_all(root).unwrap();
            let mut writer = SessionManager::open(root).unwrap();
            let key = SessionKey(key.into());
            writer
                .add_message(&key, Message::user("hello"))
                .await
                .unwrap();
            writer.update_title(&key, title.into()).await.unwrap();
            // Move the flat session into the users/<key>/sessions layout and
            // drop the flat branch, so only the users branch remains.
            let user_dir = root.join("users").join(key.0.clone()).join("sessions");
            std::fs::create_dir_all(&user_dir).unwrap();
            std::fs::rename(
                root.join("sessions").join(format!("{}.jsonl", key.0)),
                user_dir.join("default.jsonl"),
            )
            .unwrap();
            std::fs::remove_dir(root.join("sessions")).unwrap();
        }
        // Plant the broken flat branch on the store.
        match kind {
            "file" => std::fs::write(store.join("sessions"), "not a directory").unwrap(),
            #[cfg(unix)]
            "file-link" => {
                // Link to a plain FILE under the foreign store: canonicalizing
                // it would derive users from the foreign parent (the bug).
                let target = foreign.join("notes.jsonl");
                std::fs::write(&target, "{}\n").unwrap();
                std::os::unix::fs::symlink(&target, store.join("sessions")).unwrap();
            }
            #[cfg(unix)]
            "dangling-link" => {
                std::os::unix::fs::symlink(tmp.path().join("missing"), store.join("sessions"))
                    .unwrap();
            }
            #[cfg(unix)]
            "loop-link" => {
                std::os::unix::fs::symlink(store.join("sessions"), store.join("sessions")).unwrap();
            }
            other => panic!("unknown kind {other}"),
        }

        let rows = SessionManager::list_top_level_sessions_with_meta_at(&store);
        let keys: Vec<&str> = rows.iter().map(|row| row.0.as_str()).collect();
        assert_eq!(keys, ["alice"], "{kind}: own users sibling must survive");
        assert!(
            !keys.contains(&"mallory"),
            "{kind}: foreign users must not be discovered via the broken flat branch"
        );
        // Read-only: the broken branch is not replaced by a created directory.
        let meta = std::fs::symlink_metadata(store.join("sessions")).unwrap();
        assert!(
            !meta.is_dir(),
            "{kind}: listing must not mkdir the flat branch"
        );
        assert!(
            !foreign.join("sessions").is_dir(),
            "{kind}: foreign tree untouched"
        );
    }
}

// The listing row cache validates each file by its (len, modified) pair
// (#2392), so every mutation shape below must surface on the very next
// listing — a stale row served here would be a wrong message count, title,
// or preview on the dashboard's polling path.

// ----------------------------------------------------------------------
// Item 3 of OCTOS_M8_FIX_FIRST_CHECKLIST_2026-04-24:
// worktree-missing must be a hard resume refusal. The session actor
// calls `clear_messages_for_unsafe_resume()` on Err so the in-memory
// transcript cannot be silently consumed by the first LLM call.
// ----------------------------------------------------------------------

// ---- M8.10 PR #1: thread_id persistence + legacy synthesis ---------------

// --- list_for_analysis / export_transcript (memory-refresh sweep) ---

// ─────────────────────────────────────────────────────────────────────────
// evening 2026-09-10: persist_system_note_once_through_canonical_path —
// idempotency, concurrency, strict-read fail-closed, and durable-row
// identity (original timestamp/content preserved).
// ─────────────────────────────────────────────────────────────────────────

/// Issue #2006: torn bytes that are actually a COMPLETE row missing only
/// their terminator are preserved, never truncated — once sealed, the read
/// path recovers the row (mirrors the supervisor store's
/// `append_seals_a_complete_row_missing_its_trailing_newline`).
#[tokio::test]
async fn torn_tail_complete_row_is_recovered() {
    let tmp = TempDir::new().unwrap();
    let key = SessionKey::new("cli", "torn-tail-complete-row");
    let mut mgr = SessionManager::open(tmp.path()).unwrap();
    mgr.add_message(&key, make_message(MessageRole::User, "complete row"))
        .await
        .unwrap();

    // Simulate the exact crash point: the row's bytes landed but its
    // terminator did not — drop the trailing newline.
    let path = mgr.session_path(&key);
    let len = std::fs::metadata(&path).unwrap().len();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(len - 1)
        .unwrap();

    mgr.add_message(&key, make_message(MessageRole::User, "after torn"))
        .await
        .unwrap();

    let mut reload = SessionManager::open(tmp.path()).unwrap();
    let session = reload.get_or_create(&key).await;
    let contents: Vec<&str> = session
        .messages
        .iter()
        .map(|m| m.content.as_str())
        .collect();
    assert_eq!(
        contents,
        ["complete row", "after torn"],
        "a complete row that lost only its terminator must be recovered"
    );
}

// ----- UPCR-2026-042 `session/append_message` primitives -----

#[test]
fn should_not_split_a_live_persist_lock_while_pruning() {
    let key = SessionKey::new("api", "persist-lock-live-writer");
    let first = persist_lock_for(&key);
    let guard = first.try_lock().expect("initial lock");
    for index in 0..512 {
        drop(persist_lock_for(&SessionKey::new(
            "api",
            &format!("persist-lock-concurrent-{index}"),
        )));
    }
    let same = persist_lock_for(&key);
    assert!(std::sync::Arc::ptr_eq(&first, &same));
    assert!(same.try_lock().is_err());
    drop(guard);
}
