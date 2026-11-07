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

#[tokio::test]
async fn should_rebuild_from_rows_alone_when_no_segment_meta_is_readable() {
    let tmp = TempDir::new().unwrap();
    let mut mgr = SessionManager::open(tmp.path()).unwrap();
    let key = SessionKey::new("cli", "no-meta-left");
    roll_once(&mut mgr, &key).await;
    let active = mgr.session_path(&key);
    std::fs::remove_file(&active).unwrap();
    corrupt_meta_line(&segment_path(&segments_dir(&active), 1));
    mgr.cache.pop(&key.0);

    let recovered = mgr.load_full(&key).await.unwrap();
    assert_eq!(
        recovered.next_seq(),
        2,
        "seed and the big row are still counted"
    );
    assert_eq!(
        recovered.title, None,
        "no line on disk names the session any more"
    );
    assert_eq!(contents(&recovered), vec!["seed", "<8389632 bytes>"]);
    let meta = read_session_meta(&active).unwrap();
    assert_eq!((meta.sealed_segments, meta.base_seq), (1, 2));
    assert_eq!(meta.session_key, key.0);
}

#[tokio::test]
async fn should_preserve_first_message_when_sealed_segment_has_no_meta_line() {
    let tmp = TempDir::new().unwrap();
    let mut mgr = SessionManager::open(tmp.path()).unwrap();
    let key = SessionKey::new("cli", "meta-absent");
    roll_once(&mut mgr, &key).await;
    let active = mgr.session_path(&key);
    std::fs::remove_file(&active).unwrap();
    let segment = segment_path(&segments_dir(&active), 1);
    let contents_on_disk = std::fs::read_to_string(&segment).unwrap();
    let rows_without_meta = contents_on_disk
        .split_once('\n')
        .map(|(_, rows)| rows)
        .unwrap();
    std::fs::write(&segment, rows_without_meta).unwrap();
    mgr.cache.pop(&key.0);

    let recovered = mgr.load_full(&key).await.unwrap();
    assert_eq!(contents(&recovered), vec!["seed", "<8389632 bytes>"]);
    assert_eq!(recovered.next_seq(), 2);
    assert_eq!(read_session_meta(&active).unwrap().base_seq, 2);

    mgr.cache.put(key.0.clone(), recovered);
    let seq = mgr
        .add_message_with_seq(&key, make_message(MessageRole::User, "resumed"))
        .await
        .unwrap();
    assert_eq!(seq, 2);
}

#[tokio::test]
async fn should_keep_base_seq_when_rebuilding_active_from_readable_latest_segment() {
    let tmp = TempDir::new().unwrap();
    let mut mgr = SessionManager::open(tmp.path()).unwrap();
    let key = SessionKey::new("cli", "readable-latest-meta");
    roll_twice(&mut mgr, &key).await;
    let active = mgr.session_path(&key);
    assert_eq!(
        read_session_meta(&segment_path(&segments_dir(&active), 2))
            .unwrap()
            .base_seq,
        2,
        "fixture must start recovery from a non-zero base seq"
    );
    std::fs::remove_file(&active).unwrap();
    mgr.cache.pop(&key.0);

    let recovered = mgr.load_full(&key).await.unwrap();
    assert_eq!(recovered.next_seq(), 4);
    let rebuilt_meta = read_session_meta(&active).unwrap();
    assert_eq!(rebuilt_meta.sealed_segments, 2);
    assert_eq!(rebuilt_meta.base_seq, 4);

    mgr.cache.put(key.0.clone(), recovered);
    let seq = mgr
        .add_message_with_seq(&key, make_message(MessageRole::User, "resumed"))
        .await
        .unwrap();
    assert_eq!(seq, 4);
}

/// F3: after a rollback that reaches into a sealed segment, the seq a handle
/// reads back from disk must reflect the rows the marker removed there.
#[tokio::test]
async fn should_read_back_the_right_seq_after_a_rollback_into_a_sealed_segment() {
    let tmp = TempDir::new().unwrap();
    let mut mgr = SessionManager::open(tmp.path()).unwrap();
    let key = SessionKey::new("cli", "undo-seq");
    for content in [
        "seed".to_owned(),
        "u".repeat(SESSION_SEGMENT_BYTES_DEFAULT as usize + 1024),
        "after the roll".to_owned(),
    ] {
        mgr.add_message(&key, make_message(MessageRole::User, &content))
            .await
            .unwrap();
    }
    assert_eq!(mgr.rollback_last_n_user_turns(&key, 2).await.unwrap(), 2);
    let seq = mgr
        .add_message_with_seq(&key, make_message(MessageRole::User, "again"))
        .await
        .unwrap();
    assert_eq!(seq, 1, "manager: seed is 0, the two dropped turns are gone");

    // A handle's read-back folds the active file, where the marker sits;
    // the marker's debt spills into the sealed segment, so the active file
    // alone would over-count by the sealed row it removed.
    let mut handle = SessionHandle::open(tmp.path(), &key);
    assert_eq!(handle.session().next_seq(), 2);
    let seq = handle
        .add_message_with_seq(make_message(MessageRole::User, "handle"))
        .await
        .unwrap();
    assert_eq!(seq, 2);
    mgr.cache.pop(&key.0);
    let full = mgr.load_full(&key).await.unwrap();
    assert_eq!(contents(&full), vec!["seed", "again", "handle"]);
    assert_eq!(full.next_seq(), 3);
}

/// F5: the listing previews the last prompt from the file's tail, and falls
/// back to the whole file only when the tail has no user row.
#[test]
fn should_preview_the_last_prompt_from_the_file_tail() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("s.jsonl");
    let meta = serde_json::json!({
        "schema_version": 1, "session_key": "cli:x",
        "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:00:00Z"
    });
    let row = |role: &str, content: &str| {
        serde_json::json!({"role": role, "content": content, "timestamp": "2026-01-01T00:00:00Z"})
            .to_string()
    };
    let blob = "b".repeat(600 * 1024);

    // Prompt at the end of a big file: found in the tail.
    let lines = [
        meta.to_string(),
        row("user", "first question"),
        row("assistant", &blob),
        row("user", "last question"),
    ];
    std::fs::write(&path, lines.join("\n") + "\n").unwrap();
    assert_eq!(
        last_user_prompt_from_file(&path).as_deref(),
        Some("last question")
    );

    // Prompt buried before a big blob: the tail has no user row, so the
    // whole file is read.
    let lines = [
        meta.to_string(),
        row("user", "buried question"),
        row("assistant", &blob),
    ];
    std::fs::write(&path, lines.join("\n") + "\n").unwrap();
    assert_eq!(
        last_user_prompt_from_file(&path).as_deref(),
        Some("buried question")
    );

    // A rollback in the tail that drops the tail's only user row: the
    // fold over the tail yields nothing, and the full read honours it.
    let lines = [
        meta.to_string(),
        row("user", "kept question"),
        row("assistant", &blob),
        row("user", "undone question"),
        rollback_marker_line(1).unwrap(),
    ];
    std::fs::write(&path, lines.join("\n") + "\n").unwrap();
    assert_eq!(
        last_user_prompt_from_file(&path).as_deref(),
        Some("kept question")
    );
}

#[test]
fn test_truncated_session_keys_no_collision() {
    let tmp = TempDir::new().unwrap();
    let mgr = SessionManager::open(tmp.path()).unwrap();

    // Create two keys that share the same 200-char prefix but differ after
    let prefix = "a".repeat(200);
    let key1 = SessionKey(format!("{prefix}_suffix1"));
    let key2 = SessionKey(format!("{prefix}_suffix2"));

    let path1 = mgr.session_path(&key1);
    let path2 = mgr.session_path(&key2);
    assert_ne!(
        path1, path2,
        "truncated keys with different suffixes must produce different paths"
    );
}

#[test]
fn test_decode_filename() {
    assert_eq!(
        SessionManager::decode_filename("feishu%3Aoc_abc123"),
        "feishu:oc_abc123"
    );
    assert_eq!(
        SessionManager::decode_filename("cli%3Adefault"),
        "cli:default"
    );
    assert_eq!(SessionManager::decode_filename("plain-name"), "plain-name");
    // Double-byte UTF-8 round-trip
    assert_eq!(
        SessionManager::decode_filename("hello%E4%B8%96%E7%95%8C"),
        "hello\u{4e16}\u{754c}" // hello世界
    );
}

#[tokio::test]
async fn test_list_sessions_returns_decoded_keys() {
    let tmp = TempDir::new().unwrap();
    let mut mgr = SessionManager::open(tmp.path()).unwrap();
    let key = SessionKey::new("feishu", "oc_abc123");

    mgr.add_message(&key, make_message(MessageRole::User, "hello"))
        .await
        .unwrap();

    let sessions = mgr.list_sessions();
    assert_eq!(sessions.len(), 1);
    // Should return decoded key, not percent-encoded filename
    assert_eq!(sessions[0].0, "feishu:oc_abc123");
}

/// Issue #607 §D: `/api/sessions` hung 30 s+ on a user dir with 65 535
/// `child-*.jsonl` siblings because the listing iterated every JSONL.
/// `list_top_level_sessions` must skip `child-*` and `*.tasks` files at
/// the directory walk so the cost stays O(top-level sessions).
#[tokio::test]
async fn list_top_level_sessions_skips_child_jsonl() {
    let tmp = TempDir::new().unwrap();
    let mut mgr = SessionManager::open(tmp.path()).unwrap();
    let parent = SessionKey::new("api", "web-parent");

    // 1 top-level session.
    mgr.add_message(&parent, make_message(MessageRole::User, "parent"))
        .await
        .unwrap();

    // 100 child sessions written via the same canonical handle code path
    // production uses (so the test exercises real filename encoding).
    for i in 0..100 {
        let child = child_session_key(&parent, &format!("task-{i:03}"));
        mgr.add_message(&child, make_message(MessageRole::Assistant, "child"))
            .await
            .unwrap();
    }

    // The all-inclusive walk reflects every jsonl on disk (1 parent +
    // 100 children).
    let all = mgr.list_sessions();
    assert_eq!(all.len(), 101, "internal walk should include children");

    // The user-facing listing must surface only the top-level session.
    let top = mgr.list_top_level_sessions();
    assert_eq!(
        top.len(),
        1,
        "list_top_level_sessions must skip child-* fanouts; got {top:?}"
    );
    assert_eq!(top[0].0, "api:web-parent");
}

/// Sidecar `*.tasks.jsonl` ledgers (e.g. `default.tasks.jsonl`) are an
/// internal runtime detail and must never appear in the user-facing
/// listing.
#[test]
fn list_top_level_sessions_skips_tasks_sidecar_jsonl() {
    let tmp = TempDir::new().unwrap();
    let mgr = SessionManager::open(tmp.path()).unwrap();

    // Construct a per-user dir directly so we can drop in both a
    // top-level `default.jsonl` and an internal `default.tasks.jsonl`
    // without having to drive the task-ledger writers in this unit.
    let user_dir = tmp.path().join("users/api%3Aweb-tasks/sessions");
    std::fs::create_dir_all(&user_dir).unwrap();

    let meta = serde_json::json!({
        "schema_version": 1,
        "session_key": "api:web-tasks",
        "created_at": Utc::now(),
        "updated_at": Utc::now(),
    });
    std::fs::write(
        user_dir.join("default.jsonl"),
        format!("{}\n", serde_json::to_string(&meta).unwrap()),
    )
    .unwrap();
    // Sidecar — must be ignored.
    std::fs::write(
        user_dir.join("default.tasks.jsonl"),
        "{\"task_id\":\"t-1\",\"state\":\"queued\"}\n",
    )
    .unwrap();

    let top = mgr.list_top_level_sessions();
    let ids: Vec<&str> = top.iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(ids, vec!["api:web-tasks"], "got {top:?}");
}

#[test]
fn list_top_level_sessions_with_meta_surfaces_updated_at() {
    use chrono::TimeZone;
    let tmp = TempDir::new().unwrap();
    let mgr = SessionManager::open(tmp.path()).unwrap();

    let user_dir = tmp.path().join("users/cli%3Ameta-recency/sessions");
    std::fs::create_dir_all(&user_dir).unwrap();

    let updated = Utc.with_ymd_and_hms(2026, 6, 30, 8, 15, 0).unwrap();
    let meta = serde_json::json!({
        "schema_version": 1,
        "session_key": "cli:meta-recency",
        "title": "Recency Chat",
        "created_at": Utc.with_ymd_and_hms(2026, 6, 30, 8, 0, 0).unwrap(),
        "updated_at": updated,
    });
    std::fs::write(
        user_dir.join("default.jsonl"),
        format!("{}\n", serde_json::to_string(&meta).unwrap()),
    )
    .unwrap();

    let rows = mgr.list_top_level_sessions_with_meta();
    let row = rows
        .iter()
        .find(|(id, ..)| id == "cli:meta-recency")
        .expect("session present");
    assert_eq!(row.2.as_deref(), Some("Recency Chat"));
    // Recency is `max(meta.updated_at, file mtime)` (codex P2). The file was
    // just written, so its mtime dominates the older meta timestamp; the
    // surfaced recency is therefore at least the meta value.
    let recency = row.3.expect("updated_at present");
    assert!(
        recency >= updated,
        "recency must be at least the meta.updated_at; got {recency}"
    );
}

#[test]
fn read_only_listing_does_not_activate_missing_store() {
    let tmp = TempDir::new().unwrap();
    let store = tmp.path().join("project/.octos/reader");
    assert!(SessionManager::list_top_level_sessions_with_meta_at(&store).is_empty());
    assert!(!tmp.path().join("project").exists());
    assert!(SessionManager::list_top_level_sessions_with_meta_at(tmp.path()).is_empty());
    assert!(std::fs::read_dir(tmp.path()).unwrap().next().is_none());

    // A real writer still bootstraps the store without a prior list.
    let writer = SessionManager::open(&store).unwrap();
    assert!(store.join("sessions").is_dir());
    assert!(writer.list_top_level_sessions_with_meta().is_empty());
}

#[tokio::test]
async fn read_only_listing_preserves_flat_and_users_only_metadata() {
    for users_only in [false, true] {
        let tmp = TempDir::new().unwrap();
        let key = SessionKey("web-read-only".into());
        let mut writer = SessionManager::open(tmp.path()).unwrap();
        writer
            .add_message(&key, Message::user("first prompt"))
            .await
            .unwrap();
        writer
            .add_message(&key, Message::user("latest prompt"))
            .await
            .unwrap();
        let mut reply = Message::assistant("reply");
        reply.thread_id = Some("read-only-turn".into());
        writer.add_message(&key, reply).await.unwrap();
        writer
            .update_title(&key, "History title".into())
            .await
            .unwrap();
        let expected = writer.list_top_level_sessions_with_meta();
        assert_eq!(expected.len(), 1);
        assert_eq!(expected[0].1, 4);
        assert_eq!(expected[0].2.as_deref(), Some("History title"));
        assert_eq!(expected[0].4.as_deref(), Some("latest prompt"));
        assert!(expected[0].3.is_some());

        if users_only {
            let dir = tmp.path().join("users/web-read-only/sessions");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::rename(
                tmp.path().join("sessions/web-read-only.jsonl"),
                dir.join("default.jsonl"),
            )
            .unwrap();
            std::fs::remove_dir(tmp.path().join("sessions")).unwrap();
        }
        // Both the first scan and the cached scan preserve count/title/recency/preview.
        for _ in 0..2 {
            assert_eq!(
                SessionManager::list_top_level_sessions_with_meta_at(tmp.path()),
                expected
            );
            assert_eq!(tmp.path().join("sessions").exists(), !users_only);
        }
    }
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
