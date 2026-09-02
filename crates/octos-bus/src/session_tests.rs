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
    }
}

#[test]
fn test_session_get_history() {
    let mut session = Session::new(SessionKey::new("cli", "test"));
    for i in 0..10 {
        session
            .messages
            .push(make_message(MessageRole::User, &format!("msg{i}")));
    }
    let history = session.get_history(3);
    assert_eq!(history.len(), 3);
    assert_eq!(history[0].content, "msg7");
    assert_eq!(history[2].content, "msg9");
}

#[test]
fn test_session_get_history_all() {
    let mut session = Session::new(SessionKey::new("cli", "test"));
    session.messages.push(make_message(MessageRole::User, "a"));
    session.messages.push(make_message(MessageRole::User, "b"));
    let history = session.get_history(10);
    assert_eq!(history.len(), 2);
}

#[test]
fn test_sort_by_timestamp_restores_order() {
    use chrono::Duration;
    let mut session = Session::new(SessionKey::new("cli", "test"));
    let t0 = Utc::now();

    // Simulate speculative overflow: primary pre-saved at t0,
    // overflow inserted at t0+15s, primary results saved at t0+45s.
    let mut msg_a = make_message(MessageRole::User, "primary question");
    msg_a.timestamp = t0;

    let mut msg_b_user = make_message(MessageRole::User, "overflow question");
    msg_b_user.timestamp = t0 + Duration::seconds(15);

    let mut msg_b_asst = make_message(MessageRole::Assistant, "overflow answer");
    msg_b_asst.timestamp = t0 + Duration::seconds(16);

    // Primary's tool call happened at t=5s but saved later
    let mut msg_a_tool = make_message(MessageRole::Assistant, "tool_call for primary");
    msg_a_tool.timestamp = t0 + Duration::seconds(5);

    let mut msg_a_result = make_message(MessageRole::User, "tool_result");
    msg_a_result.timestamp = t0 + Duration::seconds(8);

    let mut msg_a_reply = make_message(MessageRole::Assistant, "primary answer");
    msg_a_reply.timestamp = t0 + Duration::seconds(44);

    // Insert in write order (primary pre-save, overflow, primary completion)
    session.messages.push(msg_a); // t0
    session.messages.push(msg_b_user); // t0+15
    session.messages.push(msg_b_asst); // t0+16
    session.messages.push(msg_a_tool); // t0+5 (out of order!)
    session.messages.push(msg_a_result); // t0+8 (out of order!)
    session.messages.push(msg_a_reply); // t0+44

    // Before sort: insertion order
    assert_eq!(session.messages[1].content, "overflow question");
    assert_eq!(session.messages[3].content, "tool_call for primary");

    session.sort_by_timestamp();

    // After sort: chronological order
    assert_eq!(session.messages[0].content, "primary question"); // t0
    assert_eq!(session.messages[1].content, "tool_call for primary"); // t0+5
    assert_eq!(session.messages[2].content, "tool_result"); // t0+8
    assert_eq!(session.messages[3].content, "overflow question"); // t0+15
    assert_eq!(session.messages[4].content, "overflow answer"); // t0+16
    assert_eq!(session.messages[5].content, "primary answer"); // t0+44
}

#[tokio::test]
async fn test_session_manager_create_and_retrieve() {
    let tmp = TempDir::new().unwrap();
    let mut mgr = SessionManager::open(tmp.path()).unwrap();
    let key = SessionKey::new("cli", "default");

    let session = mgr.get_or_create(&key).await;
    assert_eq!(session.messages.len(), 0);

    mgr.add_message(&key, make_message(MessageRole::User, "hello"))
        .await
        .unwrap();
    mgr.add_message(&key, make_message(MessageRole::Assistant, "hi"))
        .await
        .unwrap();

    let session = mgr.get_or_create(&key).await;
    assert_eq!(session.messages.len(), 2);
}

#[tokio::test]
async fn test_session_manager_persistence() {
    let tmp = TempDir::new().unwrap();
    let key = SessionKey::new("cli", "persist");

    // Write session
    {
        let mut mgr = SessionManager::open(tmp.path()).unwrap();
        mgr.add_message(&key, make_message(MessageRole::User, "saved"))
            .await
            .unwrap();
        mgr.add_message(&key, make_message(MessageRole::Assistant, "reply"))
            .await
            .unwrap();
    }

    // New manager should load from disk
    {
        let mut mgr = SessionManager::open(tmp.path()).unwrap();
        let session = mgr.get_or_create(&key).await;
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].content, "saved");
        assert_eq!(session.messages[1].content, "reply");
    }
}

#[tokio::test]
async fn rollback_last_n_user_turns_trims_and_survives_reload() {
    let tmp = TempDir::new().unwrap();
    let key = SessionKey::new("cli", "rollback-rt");

    {
        let mut mgr = SessionManager::open(tmp.path()).unwrap();
        // Seed 3 user turns (user + assistant each) with persisted
        // thread_ids so the drop can group them.
        for n in 1..=3 {
            let tid = format!("t{n}");
            let mut user = make_message(MessageRole::User, &format!("turn {n}"));
            user.client_message_id = Some(tid.clone());
            user.thread_id = Some(tid.clone());
            mgr.add_message(&key, user).await.unwrap();
            let mut asst = make_message(MessageRole::Assistant, &format!("reply {n}"));
            asst.thread_id = Some(tid.clone());
            mgr.add_message(&key, asst).await.unwrap();
        }

        // Roll back the last user turn: appends the marker + trims memory.
        let dropped = mgr.rollback_last_n_user_turns(&key, 1).await.unwrap();
        assert_eq!(dropped, 1);

        let session = mgr.get_or_create(&key).await;
        assert_eq!(session.messages.len(), 4, "turns 1 & 2 remain in memory");
        assert!(session.messages.iter().all(|m| m.content != "turn 3"));
        assert!(session.messages.iter().all(|m| m.content != "reply 3"));
    }

    // A fresh manager reloads from disk; the append-only marker replays the
    // trim (NOT a truncation — the dropped rows are still on disk).
    {
        let mut reload = SessionManager::open(tmp.path()).unwrap();
        let session = reload.get_or_create(&key).await;
        let contents: Vec<&String> = session.messages.iter().map(|m| &m.content).collect();
        assert_eq!(
            session.messages.len(),
            4,
            "rollback marker must survive reload; got {contents:?}"
        );
        assert!(session.messages.iter().all(|m| m.content != "turn 3"));
    }
}

/// Codex P1: in a MIXED flat + per-user layout, the rollback marker is
/// co-located with ONE file (per-user preferred) but the drop is computed
/// over the MERGED transcript. If the rolled-back turns straddle the legacy
/// flat file, applying the marker per-file would resurrect them on the next
/// merge-load. The drop must be applied POST-MERGE.
#[tokio::test]
async fn rollback_trims_across_mixed_flat_and_per_user_layout_without_resurrection() {
    let tmp = TempDir::new().unwrap();
    let mut mgr = SessionManager::open(tmp.path()).unwrap();
    let key = SessionKey::with_profile("dspfac", "api", "mixed-rollback");

    // Flat turns are older (legacy); the per-user turn is newer. The
    // rollback marker (appended at `Utc::now()`) sorts after all of them.
    let base = Utc::now() - chrono::Duration::minutes(10);
    let msg = |content: &str, tid: &str, offset: i64| {
        let role = if content.starts_with("turn") {
            MessageRole::User
        } else {
            MessageRole::Assistant
        };
        let mut m = make_message(role, content);
        m.client_message_id = Some(tid.to_string());
        m.thread_id = Some(tid.to_string());
        m.timestamp = base + chrono::Duration::milliseconds(offset);
        m
    };

    // Legacy flat file: turns 1 & 2.
    let flat_meta = serde_json::json!({
        "schema_version": 1,
        "session_key": key.0,
        "created_at": base,
        "updated_at": base + chrono::Duration::milliseconds(3),
    });
    std::fs::create_dir_all(tmp.path().join("sessions")).unwrap();
    std::fs::write(
        mgr.session_path(&key),
        format!(
            "{}\n{}\n{}\n{}\n{}\n",
            serde_json::to_string(&flat_meta).unwrap(),
            serde_json::to_string(&msg("turn 1", "t1", 0)).unwrap(),
            serde_json::to_string(&msg("reply 1", "t1", 1)).unwrap(),
            serde_json::to_string(&msg("turn 2", "t2", 2)).unwrap(),
            serde_json::to_string(&msg("reply 2", "t2", 3)).unwrap(),
        ),
    )
    .unwrap();

    // Per-user file: turn 3 only.
    let encoded_base = encode_path_component(key.base_key());
    let per_user_dir = tmp
        .path()
        .join("users")
        .join(&encoded_base)
        .join("sessions");
    std::fs::create_dir_all(&per_user_dir).unwrap();
    let per_user_meta = serde_json::json!({
        "schema_version": 1,
        "session_key": key.0,
        "created_at": base + chrono::Duration::milliseconds(4),
        "updated_at": base + chrono::Duration::milliseconds(5),
    });
    std::fs::write(
        per_user_dir.join("default.jsonl"),
        format!(
            "{}\n{}\n{}\n",
            serde_json::to_string(&per_user_meta).unwrap(),
            serde_json::to_string(&msg("turn 3", "t3", 4)).unwrap(),
            serde_json::to_string(&msg("reply 3", "t3", 5)).unwrap(),
        ),
    )
    .unwrap();

    // Merged load sees all 3 turns.
    assert_eq!(
        mgr.get_or_create(&key).await.messages.len(),
        6,
        "precondition: merged transcript = 3 turns (6 messages)"
    );

    // Roll back 2 turns: drops turn 3 (per-user) AND turn 2 (legacy flat).
    // The marker lands in the per-user file (it exists) but must cover the
    // flat turn on reload.
    let dropped = mgr.rollback_last_n_user_turns(&key, 2).await.unwrap();
    assert_eq!(dropped, 2);
    {
        let session = mgr.get_or_create(&key).await;
        assert_eq!(session.messages.len(), 2, "in-memory: only turn 1 remains");
        assert!(session.messages.iter().all(|m| m.content != "reply 2"));
    }

    // Reload from a FRESH manager (empty cache): load_from_disk re-merges
    // flat + per-user and folds the marker post-merge. Turn 2 (flat) must
    // NOT be resurrected.
    let mut reload = SessionManager::open(tmp.path()).unwrap();
    let session = reload.get_or_create(&key).await;
    let contents: Vec<&String> = session.messages.iter().map(|m| &m.content).collect();
    assert_eq!(
        session.messages.len(),
        2,
        "turn 1 only after reload; got {contents:?}"
    );
    assert!(
        session.messages.iter().all(|m| m.content != "turn 2"),
        "turn 2 (legacy flat) must not resurrect after a merge-reload: {contents:?}"
    );
    assert!(session.messages.iter().all(|m| m.content != "turn 3"));
    assert_eq!(session.messages[0].content, "turn 1");
}

/// Codex follow-up on the P1 fix: the merge dedup fingerprint must be
/// synthesis-INDEPENDENT. A legacy flat row (no `thread_id`) and its migrated
/// per-user copy (a synthesized `thread_id`) are the SAME logical message;
/// fingerprinting the raw rows kept both and DOUBLED the transcript on reload
/// for partial-migration (stale flat + per-user) sessions.
#[tokio::test]
async fn mixed_layout_dedups_migrated_rows_despite_synthesized_thread_id() {
    let tmp = TempDir::new().unwrap();
    let mut mgr = SessionManager::open(tmp.path()).unwrap();
    let key = SessionKey::with_profile("dspfac", "api", "partial-migration");

    let base = Utc::now() - chrono::Duration::minutes(5);
    // Same logical rows (identical role/content/timestamp). The flat copy has
    // NO thread_id (legacy); the per-user copy carries a synthesized thread_id
    // (migrated) — the only serialized difference between the two.
    // ids = Some((thread_id, client_message_id)) for the migrated per-user
    // copy; None for the legacy flat copy (which carries neither).
    let row = |content: &str, offset: i64, ids: Option<(&str, &str)>| {
        let role = if content.starts_with("turn") {
            MessageRole::User
        } else {
            MessageRole::Assistant
        };
        let mut m = make_message(role, content);
        m.timestamp = base + chrono::Duration::milliseconds(offset);
        let (tid, cmid) = match ids {
            Some((t, c)) => (Some(t.to_string()), Some(c.to_string())),
            None => (None, None),
        };
        m.thread_id = tid;
        m.client_message_id = cmid;
        m
    };
    let meta = |updated_ms: i64| {
        serde_json::json!({
            "schema_version": 1,
            "session_key": key.0,
            "created_at": base,
            "updated_at": base + chrono::Duration::milliseconds(updated_ms),
        })
    };

    // Legacy flat file: rows WITHOUT thread_id.
    std::fs::create_dir_all(tmp.path().join("sessions")).unwrap();
    std::fs::write(
        mgr.session_path(&key),
        format!(
            "{}\n{}\n{}\n",
            serde_json::to_string(&meta(1)).unwrap(),
            serde_json::to_string(&row("turn 1", 0, None)).unwrap(),
            serde_json::to_string(&row("reply 1", 1, None)).unwrap(),
        ),
    )
    .unwrap();

    // Per-user file: the SAME rows, but with synthesized thread_ids (migrated).
    let encoded_base = encode_path_component(key.base_key());
    let per_user_dir = tmp
        .path()
        .join("users")
        .join(&encoded_base)
        .join("sessions");
    std::fs::create_dir_all(&per_user_dir).unwrap();
    std::fs::write(
        per_user_dir.join("default.jsonl"),
        format!(
            "{}\n{}\n{}\n",
            serde_json::to_string(&meta(2)).unwrap(),
            serde_json::to_string(&row("turn 1", 0, Some(("t1", "cmid-turn1")))).unwrap(),
            serde_json::to_string(&row("reply 1", 1, Some(("t1", "cmid-reply1")))).unwrap(),
        ),
    )
    .unwrap();

    // First access on an empty cache re-merges both layouts from disk.
    let session = mgr.get_or_create(&key).await;
    let contents: Vec<&String> = session.messages.iter().map(|m| &m.content).collect();
    assert_eq!(
        session.messages.len(),
        2,
        "partial-migration session must dedup across layouts, not double: {contents:?}"
    );
    assert_eq!(
        session
            .messages
            .iter()
            .filter(|m| m.content == "turn 1")
            .count(),
        1,
        "migrated 'turn 1' must appear exactly once: {contents:?}"
    );
    // The RICHER per-user copy must win the dedup, so its canonical
    // client_message_id survives (not the flat row's None).
    let turn1 = session
        .messages
        .iter()
        .find(|m| m.content == "turn 1")
        .expect("turn 1 present");
    assert_eq!(
        turn1.client_message_id.as_deref(),
        Some("cmid-turn1"),
        "dedup must keep the canonical per-user row's client_message_id"
    );
}

#[tokio::test]
async fn test_session_manager_clear() {
    let tmp = TempDir::new().unwrap();
    let key = SessionKey::new("cli", "clear-me");
    let mut mgr = SessionManager::open(tmp.path()).unwrap();

    mgr.add_message(&key, make_message(MessageRole::User, "temp"))
        .await
        .unwrap();
    assert_eq!(mgr.get_or_create(&key).await.messages.len(), 1);

    mgr.clear(&key).await.unwrap();

    // After clear, should be empty
    let session = mgr.get_or_create(&key).await;
    assert_eq!(session.messages.len(), 0);
}

#[tokio::test]
async fn test_concurrent_sessions() {
    let tmp = TempDir::new().unwrap();
    let mut mgr = SessionManager::open(tmp.path()).unwrap();

    let k1 = SessionKey::new("telegram", "chat1");
    let k2 = SessionKey::new("telegram", "chat2");

    mgr.add_message(&k1, make_message(MessageRole::User, "from chat1"))
        .await
        .unwrap();
    mgr.add_message(&k2, make_message(MessageRole::User, "from chat2"))
        .await
        .unwrap();

    assert_eq!(mgr.get_or_create(&k1).await.messages.len(), 1);
    assert_eq!(mgr.get_or_create(&k2).await.messages.len(), 1);
    assert_eq!(
        mgr.get_or_create(&k1).await.messages[0].content,
        "from chat1"
    );
    assert_eq!(
        mgr.get_or_create(&k2).await.messages[0].content,
        "from chat2"
    );
}

#[tokio::test]
async fn test_session_rewrite() {
    let tmp = TempDir::new().unwrap();
    let key = SessionKey::new("cli", "rewrite");
    let mut mgr = SessionManager::open(tmp.path()).unwrap();

    // Add 5 messages
    for i in 0..5 {
        mgr.add_message(&key, make_message(MessageRole::User, &format!("msg{i}")))
            .await
            .unwrap();
    }

    // Mutate in-memory: keep only last 2
    let session = mgr.get_or_create(&key).await;
    session.messages.drain(0..3);
    assert_eq!(session.messages.len(), 2);

    // Rewrite to disk
    mgr.rewrite(&key).await.unwrap();

    // Load fresh from disk — should have only 2 messages
    let mut mgr2 = SessionManager::open(tmp.path()).unwrap();
    let session2 = mgr2.get_or_create(&key).await;
    assert_eq!(session2.messages.len(), 2);
    assert_eq!(session2.messages[0].content, "msg3");
    assert_eq!(session2.messages[1].content, "msg4");
}

#[tokio::test]
async fn concurrent_rewrites_of_same_session_dont_collide_on_tmp_path() {
    // Regression: prior to using a unique-per-call tmp suffix, two writers
    // racing the same session file (e.g. fanout children of one parent
    // calling parent.rewrite() in the same millisecond) shared a single
    // `<file>.jsonl.tmp` path. Both `File::create` would clobber the same
    // tmp; one rename succeeded, the other got ENOENT and surfaced as a
    // failed rewrite — manifested in spawn lifecycle as `Orphaned` instead
    // of `Joined` for the unlucky child. Asserts the rewrite race no
    // longer drops state.
    let tmp = TempDir::new().unwrap();
    let key = SessionKey::new("cli", "rewrite-race");
    let mut mgr = SessionManager::open(tmp.path()).unwrap();
    for i in 0..3 {
        mgr.add_message(&key, make_message(MessageRole::User, &format!("seed{i}")))
            .await
            .unwrap();
    }
    let mgr = std::sync::Arc::new(tokio::sync::Mutex::new(mgr));

    // Spawn N concurrent rewrites of the same session. Without the unique
    // suffix, several would race on the shared `<file>.jsonl.tmp` path and
    // ~1 in N would fail with ENOENT.
    let mut handles = Vec::new();
    for _ in 0..16 {
        let mgr = mgr.clone();
        let key = key.clone();
        handles.push(tokio::spawn(async move {
            let mgr = mgr.lock().await;
            mgr.rewrite(&key).await
        }));
    }
    for h in handles {
        let result = h.await.expect("join");
        assert!(
            result.is_ok(),
            "concurrent rewrite must not lose to tmp-file collision: {result:?}"
        );
    }

    // Disk state should still be parseable.
    let mut reload = SessionManager::open(tmp.path()).unwrap();
    let session = reload.get_or_create(&key).await;
    assert_eq!(session.messages.len(), 3);
}

#[test]
fn rewrite_tmp_path_is_unique_per_call() {
    let target = std::path::PathBuf::from("/tmp/some/session.jsonl");
    let a = rewrite_tmp_path(&target);
    let b = rewrite_tmp_path(&target);
    assert_ne!(a, b, "successive calls must produce distinct tmp paths");
    assert!(
        a.to_string_lossy().contains(".tmp"),
        "tmp path keeps a .tmp suffix: {}",
        a.display()
    );
    // Suffix encodes both PID and counter so cross-process races don't
    // collide either.
    let pid = std::process::id().to_string();
    assert!(
        a.to_string_lossy().contains(&pid),
        "tmp path includes the pid for cross-process disambiguation: {}",
        a.display()
    );
}

#[tokio::test]
async fn test_fork_creates_child() {
    let tmp = TempDir::new().unwrap();
    let mut mgr = SessionManager::open(tmp.path()).unwrap();
    let parent = SessionKey::new("telegram", "chat1");

    for i in 0..5 {
        mgr.add_message(&parent, make_message(MessageRole::User, &format!("msg{i}")))
            .await
            .unwrap();
    }

    let child_key = mgr.fork(&parent, "chat1_fork", 3).await.unwrap();
    assert_eq!(child_key, SessionKey::new("telegram", "chat1_fork"));

    let child = mgr.get_or_create(&child_key).await;
    assert_eq!(child.parent_key, Some(parent.clone()));
    assert_eq!(child.messages.len(), 3);
    assert_eq!(child.messages[0].content, "msg2");
    assert_eq!(child.messages[2].content, "msg4");
}

#[tokio::test]
async fn test_fork_failed_write_leaves_no_ghost_child() {
    let tmp = TempDir::new().unwrap();
    let mut mgr = SessionManager::open(tmp.path()).unwrap();
    let parent = SessionKey::new("cli", "ghost-parent");
    mgr.add_message(&parent, make_message(MessageRole::User, "hello"))
        .await
        .unwrap();

    // Replace the sessions DIR with a regular FILE: creating the
    // child's jsonl under it fails with ENOTDIR for EVERY euid —
    // unlike a 0o555 mode, which root bypasses (codex #1613 r2).
    let sessions_dir = tmp.path().join("sessions");
    std::fs::remove_dir_all(&sessions_dir).unwrap();
    std::fs::write(&sessions_dir, b"not a directory").unwrap();

    let result = mgr.fork(&parent, "ghost-child", 1).await;
    // Restore the real directory before asserting so the retry leg
    // below can succeed.
    std::fs::remove_file(&sessions_dir).unwrap();
    std::fs::create_dir_all(&sessions_dir).unwrap();

    assert!(result.is_err(), "fork must surface the failed write");
    assert!(
        !mgr.session_known(&SessionKey::new("cli", "ghost-child")),
        "failed fork must not leave a cache-resident ghost child"
    );
    // And a retry once storage recovers succeeds.
    let retried = mgr.fork(&parent, "ghost-child", 1).await.unwrap();
    assert_eq!(retried, SessionKey::new("cli", "ghost-child"));
}

#[tokio::test]
async fn test_eviction_keeps_max_sessions() {
    let tmp = TempDir::new().unwrap();
    let mut mgr = SessionManager::open(tmp.path())
        .unwrap()
        .with_max_sessions(3);

    // Create 5 sessions
    for i in 0..5 {
        let key = SessionKey::new("cli", &format!("s{i}"));
        mgr.add_message(&key, make_message(MessageRole::User, &format!("msg{i}")))
            .await
            .unwrap();
        // Small delay so last_accessed ordering is deterministic
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    // Should have at most 3 in memory
    assert_eq!(mgr.cache_len(), 3);
}

#[tokio::test]
async fn test_evicted_session_reloads_from_disk() {
    let tmp = TempDir::new().unwrap();
    let mut mgr = SessionManager::open(tmp.path())
        .unwrap()
        .with_max_sessions(2);

    let k0 = SessionKey::new("cli", "oldest");
    mgr.add_message(&k0, make_message(MessageRole::User, "hello from oldest"))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;

    let k1 = SessionKey::new("cli", "middle");
    mgr.add_message(&k1, make_message(MessageRole::User, "hello from middle"))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;

    let k2 = SessionKey::new("cli", "newest");
    mgr.add_message(&k2, make_message(MessageRole::User, "hello from newest"))
        .await
        .unwrap();

    // k0 should have been evicted from memory
    assert_eq!(mgr.cache_len(), 2);

    // But accessing k0 should reload it from disk
    let session = mgr.get_or_create(&k0).await;
    assert_eq!(session.messages.len(), 1);
    assert_eq!(session.messages[0].content, "hello from oldest");
}

#[test]
fn test_with_max_sessions_clamps_zero() {
    let tmp = TempDir::new().unwrap();
    let mgr = SessionManager::open(tmp.path())
        .unwrap()
        .with_max_sessions(0);
    assert_eq!(mgr.capacity(), 1);
}

/// Integration test: concurrent session processing via multiple tasks.
/// Verifies that sessions created from parallel tasks don't corrupt each other
/// and the LRU cache correctly evicts and reloads.
#[tokio::test]
async fn test_concurrent_session_processing() {
    use std::sync::Arc;
    use tokio::sync::Mutex;

    let tmp = TempDir::new().unwrap();
    let mgr = Arc::new(Mutex::new(
        SessionManager::open(tmp.path())
            .unwrap()
            .with_max_sessions(5),
    ));

    // Spawn 10 tasks that each create a session and add messages
    let mut handles = Vec::new();
    for i in 0..10 {
        let mgr = mgr.clone();
        handles.push(tokio::spawn(async move {
            let key = SessionKey::new("test", &format!("session-{i}"));
            let mut mgr = mgr.lock().await;
            mgr.add_message(
                &key,
                make_message(MessageRole::User, &format!("hello from {i}")),
            )
            .await
            .unwrap();
            mgr.add_message(
                &key,
                make_message(MessageRole::Assistant, &format!("reply to {i}")),
            )
            .await
            .unwrap();
        }));
    }

    for h in handles {
        h.await.unwrap();
    }

    // Cache should be capped at 5
    let mgr = mgr.lock().await;
    assert!(mgr.cache_len() <= 5);

    // But all 10 sessions should be loadable from disk
    drop(mgr);
    let mut fresh = SessionManager::open(tmp.path()).unwrap();
    for i in 0..10 {
        let key = SessionKey::new("test", &format!("session-{i}"));
        let session = fresh.get_or_create(&key).await;
        assert_eq!(
            session.messages.len(),
            2,
            "session-{i} should have 2 messages"
        );
        assert_eq!(session.messages[0].content, format!("hello from {i}"));
    }
}

#[tokio::test]
async fn test_fork_persists_to_disk() {
    let tmp = TempDir::new().unwrap();
    let parent = SessionKey::new("cli", "main");

    {
        let mut mgr = SessionManager::open(tmp.path()).unwrap();
        mgr.add_message(&parent, make_message(MessageRole::User, "hello"))
            .await
            .unwrap();
        mgr.fork(&parent, "branch", 1).await.unwrap();
    }

    // Reload from disk
    let mut mgr2 = SessionManager::open(tmp.path()).unwrap();
    let child_key = SessionKey::new("cli", "branch");
    let child = mgr2.get_or_create(&child_key).await;
    assert_eq!(child.parent_key, Some(parent));
    assert_eq!(child.messages.len(), 1);
    assert_eq!(child.messages[0].content, "hello");
}

#[tokio::test]
async fn test_session_handle_fork_from_parent_if_missing_copies_recent_history() {
    let tmp = TempDir::new().unwrap();
    let parent = SessionKey::new("api", "web-parent");
    let child = child_session_key(&parent, "task-123");

    {
        let mut parent_handle = SessionHandle::open(tmp.path(), &parent);
        parent_handle
            .add_message(make_message(MessageRole::User, "msg0"))
            .await
            .unwrap();
        parent_handle
            .add_message(make_message(MessageRole::Assistant, "msg1"))
            .await
            .unwrap();
        parent_handle
            .add_message(make_message(MessageRole::User, "msg2"))
            .await
            .unwrap();
    }

    SessionHandle::fork_from_parent_if_missing(tmp.path(), &parent, &child, 2)
        .await
        .unwrap();

    let child_handle = SessionHandle::open(tmp.path(), &child);
    let child_session = child_handle.session();
    assert_eq!(child_session.parent_key, Some(parent.clone()));
    assert_eq!(child_session.messages.len(), 2);
    assert_eq!(child_session.messages[0].content, "msg1");
    assert_eq!(child_session.messages[1].content, "msg2");
}

#[tokio::test]
async fn test_session_handle_fork_from_parent_if_missing_is_idempotent() {
    let tmp = TempDir::new().unwrap();
    let parent = SessionKey::new("api", "web-parent");
    let child = child_session_key(&parent, "task-123");

    {
        let mut parent_handle = SessionHandle::open(tmp.path(), &parent);
        parent_handle
            .add_message(make_message(MessageRole::User, "msg0"))
            .await
            .unwrap();
    }

    SessionHandle::fork_from_parent_if_missing(tmp.path(), &parent, &child, 1)
        .await
        .unwrap();

    {
        let mut child_handle = SessionHandle::open(tmp.path(), &child);
        child_handle
            .add_message(make_message(MessageRole::Assistant, "child-result"))
            .await
            .unwrap();
    }

    SessionHandle::fork_from_parent_if_missing(tmp.path(), &parent, &child, 1)
        .await
        .unwrap();

    let child_handle = SessionHandle::open(tmp.path(), &child);
    let child_session = child_handle.session();
    assert_eq!(child_session.messages.len(), 2);
    assert_eq!(child_session.messages[1].content, "child-result");
}

#[tokio::test]
async fn test_child_session_contract_round_trips_through_disk() {
    let tmp = TempDir::new().unwrap();
    let parent = SessionKey::new("api", "parent");
    let child = child_session_key(&parent, "task-contract");

    {
        let mut parent_handle = SessionHandle::open(tmp.path(), &parent);
        parent_handle
            .add_message(make_message(MessageRole::User, "seed"))
            .await
            .unwrap();
    }

    {
        let mut child_handle = SessionHandle::open(tmp.path(), &child);
        child_handle
            .upsert_child_contract(ChildSessionContract {
                task_id: "task-123".to_string(),
                task_label: "Research".to_string(),
                parent_session_key: parent.to_string(),
                child_session_key: child.to_string(),
                workflow_kind: Some("deep_research".to_string()),
                current_phase: Some("research".to_string()),
                terminal_state: None,
                join_state: None,
                joined_at: None,
                failure_action: None,
                error: None,
                output_files: vec![],
            })
            .await
            .unwrap();
        child_handle
            .upsert_child_contract(ChildSessionContract {
                task_id: "task-123".to_string(),
                task_label: "Research".to_string(),
                parent_session_key: parent.to_string(),
                child_session_key: child.to_string(),
                workflow_kind: Some("deep_research".to_string()),
                current_phase: Some("deliver_result".to_string()),
                terminal_state: Some(ChildSessionTerminalState::Completed),
                join_state: Some(ChildSessionJoinState::Joined),
                joined_at: Some(Utc::now()),
                failure_action: None,
                error: None,
                output_files: vec!["/tmp/report.md".to_string()],
            })
            .await
            .unwrap();
    }

    assert!(SessionHandle::session_exists(tmp.path(), &child));

    let child_handle = SessionHandle::open(tmp.path(), &child);
    let child_session = child_handle.session();
    assert_eq!(child_session.child_contracts.len(), 1);
    let contract = &child_session.child_contracts[0];
    assert_eq!(contract.task_id, "task-123");
    assert_eq!(
        contract.terminal_state,
        Some(ChildSessionTerminalState::Completed)
    );
    assert_eq!(contract.join_state, Some(ChildSessionJoinState::Joined));
    assert_eq!(contract.output_files, vec!["/tmp/report.md"]);
    assert!(contract.joined_at.is_some());
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
async fn should_assign_distinct_seqs_when_concurrent_appends_race_same_key() {
    // Seq-assignment race: `SessionHandle::add_message_with_seq` derived
    // the committed seq from its OWN in-memory mirror length, with no
    // per-key serialisation between the disk append and the len read.
    // Independent writers (direct pre-opened handles racing the canonical
    // locked path) each hold a mirror snapshotted at the same pre-state,
    // so they all compute the SAME seq for different durable rows.
    //
    // Determinism: the 8 direct handles are opened at the 1-message
    // pre-state BEFORE the race starts. Each direct future appends its
    // row and then pushes onto its own mirror — every one of those
    // mirrors has len 1 regardless of blocking-pool timing, so WITHOUT
    // the per-key lock + disk-derived seq all 8 direct writers return
    // seq 1. The duplicate is deterministic, not timing-dependent.
    let tmp = TempDir::new().unwrap();
    let key = SessionKey::new("api", "seq-race");
    {
        let mut seed = SessionHandle::open(tmp.path(), &key);
        seed.add_message(make_message(MessageRole::User, "seed"))
            .await
            .unwrap();
    }

    // 8 independent handles, each mirroring the seeded pre-state.
    let mut direct_handles: Vec<SessionHandle> = (0..8)
        .map(|_| SessionHandle::open(tmp.path(), &key))
        .collect();
    let direct_futures = direct_handles
        .iter_mut()
        .enumerate()
        .map(|(i, handle)| {
            let msg = make_message(MessageRole::User, &format!("direct-{i}"));
            async move { handle.add_message_with_seq(msg).await }
        })
        .collect::<Vec<_>>();
    // 8 canonical-path persists racing the direct handles on the same key.
    let canonical_futures = (0..8)
        .map(|i| {
            persist_message_through_canonical_path(
                tmp.path(),
                &key,
                make_message(MessageRole::User, &format!("canonical-{i}")),
            )
        })
        .collect::<Vec<_>>();

    let (direct_seqs, canonical_seqs) = tokio::join!(
        futures::future::join_all(direct_futures),
        futures::future::join_all(canonical_futures),
    );

    let mut seqs = Vec::new();
    for result in direct_seqs.into_iter().chain(canonical_seqs) {
        seqs.push(result.expect("every concurrent append must commit"));
    }

    let unique: std::collections::BTreeSet<usize> = seqs.iter().copied().collect();
    assert_eq!(
        unique.len(),
        seqs.len(),
        "concurrent appends to the same key must commit distinct seqs; got {seqs:?}"
    );
    let expected: std::collections::BTreeSet<usize> = (1..=16).collect();
    assert_eq!(
        unique, expected,
        "committed seqs must form the contiguous set 1..=16; got {seqs:?}"
    );

    // Every row must be durably visible on a reload too.
    let reloaded = SessionHandle::open(tmp.path(), &key);
    assert_eq!(reloaded.session().messages.len(), 17);
}

#[tokio::test]
async fn test_session_handle_fork_from_parent_if_missing_links_existing_child_history() {
    let tmp = TempDir::new().unwrap();
    let parent = SessionKey::new("api", "web-parent");
    let child = child_session_key(&parent, "task-linked");

    {
        let mut parent_handle = SessionHandle::open(tmp.path(), &parent);
        parent_handle
            .add_message(make_message(MessageRole::User, "parent-msg"))
            .await
            .unwrap();
    }

    {
        let mut child_handle = SessionHandle::open(tmp.path(), &child);
        child_handle
            .add_message(make_message(MessageRole::Assistant, "existing-child-msg"))
            .await
            .unwrap();
        assert_eq!(child_handle.session().parent_key, None);
    }

    SessionHandle::fork_from_parent_if_missing(tmp.path(), &parent, &child, 1)
        .await
        .unwrap();

    let child_handle = SessionHandle::open(tmp.path(), &child);
    let child_session = child_handle.session();
    assert_eq!(child_session.parent_key, Some(parent));
    assert_eq!(child_session.messages.len(), 1);
    assert_eq!(child_session.messages[0].content, "existing-child-msg");
}

#[tokio::test]
async fn test_load_rejects_oversized_file() {
    let tmp = TempDir::new().unwrap();
    let mut mgr = SessionManager::open(tmp.path()).unwrap();
    let key = SessionKey::new("cli", "huge");

    // Write a normal message so the file exists
    mgr.add_message(&key, make_message(MessageRole::User, "seed"))
        .await
        .unwrap();

    // Evict from cache so next access must load from disk
    mgr.cache.pop(&key.0);

    // Overwrite the file with junk exceeding the size limit
    let path = mgr.session_path(&key);
    let junk = "x".repeat((MAX_SESSION_FILE_SIZE as usize) + 1);
    std::fs::write(&path, junk).unwrap();

    // load_from_disk should return None for oversized file
    assert!(mgr.load_from_disk(&key).await.is_none());
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

#[tokio::test]
async fn list_top_level_sessions_with_meta_surfaces_last_user_prompt() {
    let tmp = TempDir::new().unwrap();
    let mut mgr = SessionManager::open(tmp.path()).unwrap();
    let key = SessionKey::new("cli", "last-prompt");

    // Two user turns with an assistant reply between; the MOST RECENT user
    // message ("second question") is the expected `last_prompt` preview.
    mgr.add_message(&key, make_message(MessageRole::User, "first question"))
        .await
        .unwrap();
    mgr.add_message(&key, make_message(MessageRole::Assistant, "first answer"))
        .await
        .unwrap();
    mgr.add_message(&key, make_message(MessageRole::User, "second question"))
        .await
        .unwrap();

    let rows = mgr.list_top_level_sessions_with_meta();
    let row = rows
        .iter()
        .find(|(id, ..)| id == "cli:last-prompt")
        .expect("session present");
    // 5th tuple element = last_prompt.
    assert_eq!(
        row.4.as_deref(),
        Some("second question"),
        "last_prompt must be the MOST RECENT user message, not the first"
    );
}

#[test]
fn last_user_prompt_from_jsonl_truncates_long_prompt() {
    let long = "x".repeat(250);
    let user_line = serde_json::to_string(&make_message(MessageRole::User, &long)).unwrap();
    let content = format!("{{\"session_key\":\"k\"}}\n{user_line}\n");
    let preview = last_user_prompt_from_jsonl(&content).expect("prompt present");
    assert!(
        preview.ends_with('…'),
        "truncated preview must end with an ellipsis: {preview}"
    );
    assert!(
        preview.len() <= LAST_PROMPT_PREVIEW_BYTES + '…'.len_utf8(),
        "preview must be capped near the byte budget; got {} bytes",
        preview.len()
    );
}

#[test]
fn last_user_prompt_from_jsonl_unwraps_content_part_text() {
    // codex P2: content-part user messages must surface the inner text,
    // not the raw `[{"type":"text","text":"…"}]` wrapper.
    let mut msg = make_message(MessageRole::User, "");
    msg.content = r#"[{"type":"text","text":"deploy the app"}]"#.to_string();
    let user_line = serde_json::to_string(&msg).unwrap();
    let content = format!("{{\"session_key\":\"k\"}}\n{user_line}\n");
    assert_eq!(
        last_user_prompt_from_jsonl(&content).as_deref(),
        Some("deploy the app"),
        "content-part prompt must show its text, not raw JSON"
    );
}

#[test]
fn last_user_prompt_from_jsonl_honors_rollback_markers() {
    // A /rewound session keeps the dropped rows on disk; the preview must
    // reflect the FOLDED transcript (what hydrate shows), not the raw
    // rolled-back tail (codex P2). Two user turns then a rollback of 1 →
    // the last surviving user prompt is the FIRST turn.
    let u1 = serde_json::to_string(&make_message(MessageRole::User, "first turn")).unwrap();
    let a1 = serde_json::to_string(&make_message(MessageRole::Assistant, "reply one")).unwrap();
    let u2 = serde_json::to_string(&make_message(MessageRole::User, "second turn")).unwrap();
    let a2 = serde_json::to_string(&make_message(MessageRole::Assistant, "reply two")).unwrap();
    let marker = rollback_marker_line(1).unwrap();
    let content = format!("{{\"session_key\":\"k\"}}\n{u1}\n{a1}\n{u2}\n{a2}\n{marker}\n");
    assert_eq!(
        last_user_prompt_from_jsonl(&content).as_deref(),
        Some("first turn"),
        "preview must honor the rollback marker (folded), not show the dropped 'second turn'"
    );
}

#[test]
fn last_user_prompt_from_jsonl_returns_none_without_user_message() {
    // Meta line + an assistant line only → no user message → None.
    let assistant =
        serde_json::to_string(&make_message(MessageRole::Assistant, "hi there")).unwrap();
    let content = format!("{{\"session_key\":\"k\"}}\n{assistant}\n");
    assert_eq!(last_user_prompt_from_jsonl(&content), None);
}

/// Codex P2: `SessionMeta.updated_at` is stamped once at creation and is
/// NOT rewritten on ordinary message appends, so `session/list` recency read
/// straight from meta goes stale for active chats. Appending a message must
/// advance the list recency — the surfaced timestamp has to track the file's
/// real last-write time (mtime), not the frozen meta value.
#[tokio::test]
async fn list_recency_advances_when_a_message_is_appended() {
    let tmp = TempDir::new().unwrap();
    let mut mgr = SessionManager::open(tmp.path()).unwrap();
    let key = SessionKey::new("cli", "recency-append");

    // Create the session: `meta.updated_at` is stamped here, once.
    mgr.add_message(&key, make_message(MessageRole::User, "hi"))
        .await
        .unwrap();
    let recency_before = mgr
        .list_top_level_sessions_with_meta()
        .into_iter()
        .find(|(id, ..)| id == "cli:recency-append")
        .and_then(|row| row.3)
        .expect("recency present after create");

    // Let the filesystem mtime clock advance, then append. The meta line
    // (and its `updated_at`) is left untouched; only the file mtime moves.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    mgr.add_message(&key, make_message(MessageRole::Assistant, "there"))
        .await
        .unwrap();
    let recency_after = mgr
        .list_top_level_sessions_with_meta()
        .into_iter()
        .find(|(id, ..)| id == "cli:recency-append")
        .and_then(|row| row.3)
        .expect("recency present after append");

    assert!(
        recency_after > recency_before,
        "appending a message must advance list recency; meta.updated_at is \
             frozen at creation, so mtime must drive it — before={recency_before} \
             after={recency_after}"
    );
}

/// Regression guard for the O(N) hang. With 5 000 synthetic
/// `child-*.jsonl` files on disk, `list_top_level_sessions` must stay
/// well under the 500 ms bound — the original `list_sessions`
/// `count_lines`-per-file loop blew past 30 s in the wild on a dir
/// 13× larger.
#[test]
fn list_top_level_sessions_is_fast_with_many_child_jsonls() {
    let tmp = TempDir::new().unwrap();
    let mgr = SessionManager::open(tmp.path()).unwrap();

    let user_dir = tmp.path().join("users/api%3Aweb-river/sessions");
    std::fs::create_dir_all(&user_dir).unwrap();

    // Top-level session.
    std::fs::write(
        user_dir.join("default.jsonl"),
        "{\"schema_version\":1,\"session_key\":\"api:web-river\",\
             \"created_at\":\"2024-01-01T00:00:00Z\",\
             \"updated_at\":\"2024-01-01T00:00:00Z\"}\n",
    )
    .unwrap();

    // Synthetic spawn fanout.
    const FANOUT: usize = 5_000;
    for i in 0..FANOUT {
        std::fs::write(
            user_dir.join(format!("child-task-{i:05}.jsonl")),
            "{\"schema_version\":1}\n{\"role\":\"assistant\",\"content\":\"x\"}\n",
        )
        .unwrap();
    }

    let start = std::time::Instant::now();
    let top = mgr.list_top_level_sessions();
    let elapsed = start.elapsed();

    assert_eq!(top.len(), 1, "only top-level session should surface");
    assert!(
        elapsed < std::time::Duration::from_millis(500),
        "list_top_level_sessions took {elapsed:?} for {FANOUT} child files; \
             the per-file count_lines fallback regressed",
    );
}

#[test]
fn test_short_key_no_hash_suffix() {
    let tmp = TempDir::new().unwrap();
    let mgr = SessionManager::open(tmp.path()).unwrap();

    let key = SessionKey::new("cli", "short");
    let path = mgr.session_path(&key);
    let name = path.file_stem().unwrap().to_str().unwrap();
    // Short keys should not have hash suffix (no underscore + hex)
    assert!(!name.contains('_') || name.len() < 200);
}

#[tokio::test]
async fn test_list_sessions_for_chat() {
    let tmp = TempDir::new().unwrap();
    let mut mgr = SessionManager::open(tmp.path()).unwrap();

    // Create default session + two topic sessions
    let base = SessionKey::new("telegram", "12345");
    let research = SessionKey::with_topic("telegram", "12345", "research");
    let code = SessionKey::with_topic("telegram", "12345", "code");
    // Unrelated session
    let other = SessionKey::new("telegram", "99999");

    mgr.add_message(&base, make_message(MessageRole::User, "hello default"))
        .await
        .unwrap();
    mgr.add_message(&research, make_message(MessageRole::User, "hello research"))
        .await
        .unwrap();
    mgr.add_message(&code, make_message(MessageRole::User, "hello code"))
        .await
        .unwrap();
    mgr.add_message(&other, make_message(MessageRole::User, "unrelated"))
        .await
        .unwrap();

    let entries = mgr.list_sessions_for_chat("telegram:12345");
    assert_eq!(entries.len(), 3);

    let topics: Vec<Option<String>> = entries.iter().map(|e| e.topic.clone()).collect();
    assert!(topics.contains(&None)); // default
    assert!(topics.contains(&Some("research".into())));
    assert!(topics.contains(&Some("code".into())));

    // Each has 1 message
    for e in &entries {
        assert_eq!(e.message_count, 1);
    }
}

#[tokio::test]
async fn test_session_topic_persists() {
    let tmp = TempDir::new().unwrap();
    let key = SessionKey::with_topic("telegram", "12345", "research");

    {
        let mut mgr = SessionManager::open(tmp.path()).unwrap();
        mgr.add_message(&key, make_message(MessageRole::User, "topic data"))
            .await
            .unwrap();
    }

    // Reload and verify topic
    let mut mgr = SessionManager::open(tmp.path()).unwrap();
    let session = mgr.get_or_create(&key).await;
    assert_eq!(session.topic.as_deref(), Some("research"));
}

#[tokio::test]
async fn test_update_summary() {
    let tmp = TempDir::new().unwrap();
    let key = SessionKey::new("telegram", "12345");

    let mut mgr = SessionManager::open(tmp.path()).unwrap();
    mgr.add_message(&key, make_message(MessageRole::User, "hello"))
        .await
        .unwrap();
    mgr.update_summary(&key, "A test session".into())
        .await
        .unwrap();

    // Reload and verify summary
    let mut mgr2 = SessionManager::open(tmp.path()).unwrap();
    let session = mgr2.get_or_create(&key).await;
    assert_eq!(session.summary.as_deref(), Some("A test session"));
}

#[tokio::test]
async fn should_persist_title_separately_from_summary_when_renamed() {
    let tmp = TempDir::new().unwrap();
    let key = SessionKey::new("telegram", "12345");

    let mut mgr = SessionManager::open(tmp.path()).unwrap();
    mgr.add_message(&key, make_message(MessageRole::User, "hello"))
        .await
        .unwrap();
    mgr.update_title(&key, "Custom title".into()).await.unwrap();
    mgr.update_summary(&key, "Long-form summary".into())
        .await
        .unwrap();

    // Reload from disk and verify title + summary are independent.
    let mut mgr2 = SessionManager::open(tmp.path()).unwrap();
    let session = mgr2.get_or_create(&key).await;
    assert_eq!(session.title.as_deref(), Some("Custom title"));
    assert_eq!(session.summary.as_deref(), Some("Long-form summary"));
}

#[tokio::test]
async fn should_auto_derive_title_from_first_user_message_when_unset() {
    let tmp = TempDir::new().unwrap();
    let key = SessionKey::new("telegram", "12345");

    let mut mgr = SessionManager::open(tmp.path()).unwrap();
    mgr.add_message(
        &key,
        make_message(MessageRole::User, "What is the weather today?"),
    )
    .await
    .unwrap();

    let session = mgr.get_or_create(&key).await;
    assert_eq!(
        session.title.as_deref(),
        Some("What is the weather today?"),
        "first user message should auto-populate title"
    );
}

#[tokio::test]
async fn should_not_overwrite_manual_title_when_subsequent_messages_arrive() {
    let tmp = TempDir::new().unwrap();
    let key = SessionKey::new("telegram", "12345");

    let mut mgr = SessionManager::open(tmp.path()).unwrap();
    mgr.update_title(&key, "Manual title".into()).await.unwrap();
    mgr.add_message(&key, make_message(MessageRole::User, "first user message"))
        .await
        .unwrap();
    mgr.add_message(&key, make_message(MessageRole::User, "second user message"))
        .await
        .unwrap();

    let session = mgr.get_or_create(&key).await;
    assert_eq!(
        session.title.as_deref(),
        Some("Manual title"),
        "manual title must be preserved across new messages"
    );
}

#[tokio::test]
async fn session_handle_should_auto_derive_title_from_first_user_message() {
    let tmp = TempDir::new().unwrap();
    let key = SessionKey::new("api", "fix-617b-handle-derive-test");
    let mut handle = SessionHandle::open(tmp.path(), &key);
    handle
        .add_message(make_message(
            MessageRole::User,
            "What's the weather in San Francisco?",
        ))
        .await
        .unwrap();

    // Reload from disk via SessionHandle::open (forces JSONL deserialize)
    let handle2 = SessionHandle::open(tmp.path(), &key);
    assert_eq!(
        handle2.session().title.as_deref(),
        Some("What's the weather in San Francisco?"),
        "SessionHandle::add_message_with_seq must auto-derive title"
    );
}

#[tokio::test]
async fn should_truncate_auto_derived_title_to_50_chars() {
    let tmp = TempDir::new().unwrap();
    let key = SessionKey::new("telegram", "12345");

    let mut mgr = SessionManager::open(tmp.path()).unwrap();
    let long_message = "a".repeat(200);
    mgr.add_message(&key, make_message(MessageRole::User, &long_message))
        .await
        .unwrap();

    let session = mgr.get_or_create(&key).await;
    let title = session.title.as_deref().unwrap();
    assert!(
        title.chars().count() <= 50,
        "title should be at most 50 chars, got {}",
        title.chars().count()
    );
}

#[test]
fn test_active_session_store() {
    let tmp = TempDir::new().unwrap();
    let mut store = ActiveSessionStore::open(tmp.path()).unwrap();

    // Default: no topic
    assert_eq!(store.get_active_topic("telegram:12345"), "");
    let key = store.resolve_session_key("telegram:12345");
    assert_eq!(key.0, "telegram:12345");

    // Switch to "research"
    store.switch_to("telegram:12345", "research").unwrap();
    assert_eq!(store.get_active_topic("telegram:12345"), "research");
    let key = store.resolve_session_key("telegram:12345");
    assert_eq!(key.0, "telegram:12345#research");

    // Switch to "code"
    store.switch_to("telegram:12345", "code").unwrap();
    assert_eq!(store.get_active_topic("telegram:12345"), "code");

    // Go back -> should return "research"
    let prev = store.go_back("telegram:12345").unwrap();
    assert_eq!(prev, Some("research".into()));
    assert_eq!(store.get_active_topic("telegram:12345"), "research");
}

#[test]
fn test_active_session_store_persistence() {
    let tmp = TempDir::new().unwrap();

    {
        let mut store = ActiveSessionStore::open(tmp.path()).unwrap();
        store.switch_to("telegram:12345", "research").unwrap();
    }

    // Reload
    let store = ActiveSessionStore::open(tmp.path()).unwrap();
    assert_eq!(store.get_active_topic("telegram:12345"), "research");
}

#[test]
fn test_validate_topic_name() {
    assert!(validate_topic_name("research").is_ok());
    assert!(validate_topic_name("my-code").is_ok());
    assert!(validate_topic_name("work_notes").is_ok());
    assert!(validate_topic_name("").is_err());
    assert!(validate_topic_name("a#b").is_err());
    assert!(validate_topic_name("a:b").is_err());
    assert!(validate_topic_name("a/b").is_err());
    assert!(validate_topic_name(&"x".repeat(51)).is_err());

    // Reserved: "default" is the no-topic filename in the per-user
    // layout. Allowing a user-named "default" topic would silently
    // collide with the topic-less mapping.
    assert!(validate_topic_name("default").is_err());
    assert!(validate_topic_name("DEFAULT").is_err());
    assert!(validate_topic_name("Default").is_err());
}

#[tokio::test]
async fn test_append_respects_file_size_limit() {
    let tmp = TempDir::new().unwrap();
    let mut mgr = SessionManager::open(tmp.path()).unwrap();
    let key = SessionKey::new("cli", "big");

    // Write a seed message
    mgr.add_message(&key, make_message(MessageRole::User, "seed"))
        .await
        .unwrap();

    // Manually inflate the file to just under the limit
    let path = mgr.session_path(&key);
    let padding = "x".repeat((MAX_SESSION_FILE_SIZE as usize) - 10);
    std::fs::write(&path, padding).unwrap();

    // Append should silently skip (file is at limit)
    mgr.add_message(&key, make_message(MessageRole::User, "should not append"))
        .await
        .unwrap();

    // File should not have grown significantly
    let size = std::fs::metadata(&path).unwrap().len();
    assert!(size < MAX_SESSION_FILE_SIZE + 1000);
}

#[tokio::test]
async fn test_load_rejects_future_schema_version() {
    let tmp = TempDir::new().unwrap();
    let mgr = SessionManager::open(tmp.path()).unwrap();
    let key = SessionKey::new("cli", "future");

    // Write a session file with schema version 999
    let meta = serde_json::json!({
        "schema_version": 999,
        "session_key": "cli:future",
        "created_at": "2025-01-01T00:00:00Z",
        "updated_at": "2025-01-01T00:00:00Z"
    });
    let path = mgr.session_path(&key);
    let content = format!("{}\n", serde_json::to_string(&meta).unwrap());
    std::fs::write(&path, content).unwrap();

    // Should refuse to load
    assert!(mgr.load_from_disk(&key).await.is_none());
}

#[tokio::test]
async fn test_load_from_disk_merges_flat_and_per_user_histories() {
    let tmp = TempDir::new().unwrap();
    let mgr = SessionManager::open(tmp.path()).unwrap();
    let key = SessionKey::with_profile("dspfac", "api", "slides-123");

    let older = chrono::Utc::now() - chrono::Duration::minutes(2);
    let newer = older + chrono::Duration::minutes(1);

    let flat_meta = serde_json::json!({
        "schema_version": 1,
        "session_key": key.0,
        "created_at": older,
        "updated_at": newer
    });
    std::fs::create_dir_all(tmp.path().join("sessions")).unwrap();
    std::fs::write(
        mgr.session_path(&key),
        format!(
            "{}\n{}\n",
            serde_json::to_string(&flat_meta).unwrap(),
            serde_json::to_string(&Message {
                role: MessageRole::Assistant,
                content: "artifact".into(),
                media: vec!["/tmp/file.png".into()],
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: newer,
            })
            .unwrap()
        ),
    )
    .unwrap();

    let encoded_base = encode_path_component(key.base_key());
    let per_user_dir = tmp.path().join("users").join(encoded_base).join("sessions");
    std::fs::create_dir_all(&per_user_dir).unwrap();
    let per_user_meta = serde_json::json!({
        "schema_version": 1,
        "session_key": key.0,
        "created_at": older,
        "updated_at": older
    });
    std::fs::write(
        per_user_dir.join("default.jsonl"),
        format!(
            "{}\n{}\n",
            serde_json::to_string(&per_user_meta).unwrap(),
            serde_json::to_string(&Message {
                role: MessageRole::User,
                content: "make slides".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: older,
            })
            .unwrap()
        ),
    )
    .unwrap();

    let session = mgr
        .load_from_disk(&key)
        .await
        .expect("expected merged session");
    assert_eq!(session.messages.len(), 2);
    assert_eq!(session.messages[0].content, "make slides");
    assert_eq!(session.messages[1].content, "artifact");
    assert_eq!(session.updated_at, newer);
}

#[tokio::test]
async fn test_purge_stale_sessions() {
    let tmp = TempDir::new().unwrap();
    let mut mgr = SessionManager::open(tmp.path()).unwrap();

    // Create a session
    let key = SessionKey::new("cli", "old-session");
    mgr.add_message(&key, make_message(MessageRole::User, "old"))
        .await
        .unwrap();

    // Manually backdate the session metadata to 100 days ago
    let path = mgr.session_path(&key);
    let content = std::fs::read_to_string(&path).unwrap();
    let mut lines: Vec<&str> = content.lines().collect();
    let mut meta: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    let old_date = (Utc::now() - chrono::Duration::days(100))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    meta["updated_at"] = serde_json::Value::String(old_date);
    lines[0] = &serde_json::to_string(&meta).unwrap();
    // Need to own the string for lines[0]
    let meta_str = serde_json::to_string(&meta).unwrap();
    let new_content = format!(
        "{}\n{}\n",
        meta_str,
        content.lines().skip(1).collect::<Vec<_>>().join("\n")
    );
    std::fs::write(&path, new_content).unwrap();

    // Purge sessions older than 90 days
    let removed = mgr.purge_stale(90);
    assert_eq!(removed, 1);

    // File should be gone
    assert!(!path.exists());
}

#[test]
fn test_list_user_sessions_merges_both_layouts() {
    let tmp = TempDir::new().unwrap();
    let mgr = SessionManager::open(tmp.path()).unwrap();

    let now = Utc::now();
    let older = now - chrono::Duration::hours(2);
    let old = now - chrono::Duration::hours(1);

    // --- Legacy flat layout ---
    // Default session (older timestamp — should be superseded by per-user default)
    let legacy_default_meta = serde_json::json!({
        "schema_version": 1,
        "session_key": "telegram:12345",
        "created_at": older,
        "updated_at": older
    });
    let legacy_default_path = tmp.path().join("sessions/telegram%3A12345.jsonl");
    std::fs::write(
        &legacy_default_path,
        format!(
            "{}\n{}\n",
            serde_json::to_string(&legacy_default_meta).unwrap(),
            serde_json::to_string(&make_message(MessageRole::User, "legacy default")).unwrap()
        ),
    )
    .unwrap();

    // "research" topic — only exists in legacy
    let legacy_research_meta = serde_json::json!({
        "schema_version": 1,
        "session_key": "telegram:12345#research",
        "topic": "research",
        "created_at": old,
        "updated_at": old
    });
    let legacy_research_path = tmp
        .path()
        .join("sessions/telegram%3A12345%23research.jsonl");
    std::fs::write(
        &legacy_research_path,
        format!(
            "{}\n{}\n",
            serde_json::to_string(&legacy_research_meta).unwrap(),
            serde_json::to_string(&make_message(MessageRole::User, "legacy research")).unwrap()
        ),
    )
    .unwrap();

    // --- Per-user layout ---
    let user_sessions_dir = tmp.path().join("users/telegram%3A12345/sessions");
    std::fs::create_dir_all(&user_sessions_dir).unwrap();

    // Default session (newer — should win over legacy default)
    let peruser_default_meta = serde_json::json!({
        "schema_version": 1,
        "session_key": "telegram:12345",
        "created_at": old,
        "updated_at": now
    });
    std::fs::write(
        user_sessions_dir.join("default.jsonl"),
        format!(
            "{}\n{}\n",
            serde_json::to_string(&peruser_default_meta).unwrap(),
            serde_json::to_string(&make_message(MessageRole::User, "peruser default")).unwrap()
        ),
    )
    .unwrap();

    // "coding" topic — only exists in per-user
    let peruser_coding_meta = serde_json::json!({
        "schema_version": 1,
        "session_key": "telegram:12345#coding",
        "topic": "coding",
        "created_at": old,
        "updated_at": old
    });
    std::fs::write(
        user_sessions_dir.join("coding.jsonl"),
        format!(
            "{}\n{}\n",
            serde_json::to_string(&peruser_coding_meta).unwrap(),
            serde_json::to_string(&make_message(MessageRole::User, "peruser coding")).unwrap()
        ),
    )
    .unwrap();

    // --- Call list_user_sessions and assert ---
    let entries = mgr.list_user_sessions("telegram:12345");

    // Should have 3 entries: default (per-user), research (legacy), coding (per-user)
    assert_eq!(entries.len(), 3, "expected 3 entries, got: {entries:?}");

    // Sorted by updated_at descending: default(now) > research(old) >= coding(old)
    let topics: Vec<Option<&str>> = entries.iter().map(|e| e.topic.as_deref()).collect();

    // Default session (from per-user, not legacy) should be first (newest)
    assert_eq!(
        entries[0].topic, None,
        "first entry should be default session"
    );
    assert_eq!(
        entries[0].updated_at, now,
        "default session should come from per-user layout (newer timestamp)"
    );

    // "research" and "coding" should both be present
    assert!(
        topics.contains(&Some("research")),
        "research topic should be included from legacy"
    );
    assert!(
        topics.contains(&Some("coding")),
        "coding topic should be included from per-user"
    );

    // Each entry should have 1 message
    for e in &entries {
        assert_eq!(e.message_count, 1, "each session should have 1 message");
    }
}

#[tokio::test]
async fn test_session_handle_add_message_with_seq_returns_committed_index() {
    let tmp = TempDir::new().unwrap();
    let key = SessionKey::new("api", "web-seq-test");
    let mut handle = SessionHandle::open(tmp.path(), &key);

    let first = handle
        .add_message_with_seq(make_message(MessageRole::User, "hello"))
        .await
        .unwrap();
    let second = handle
        .add_message_with_seq(make_message(MessageRole::Assistant, "world"))
        .await
        .unwrap();

    assert_eq!(first, 0);
    assert_eq!(second, 1);
    assert_eq!(handle.get_history(10).len(), 2);
}

#[tokio::test]
async fn add_message_preserves_client_message_id_through_jsonl_roundtrip() {
    let tmp = TempDir::new().unwrap();
    let key = SessionKey::new("api", "web-cmid-test");

    // First handle: persist a user message tagged with a client_message_id.
    {
        let mut handle = SessionHandle::open(tmp.path(), &key);
        let user_msg = Message::user("hi there").with_client_message_id("cmid-xyz");
        let seq = handle.add_message_with_seq(user_msg).await.unwrap();
        assert_eq!(seq, 0);
    }

    // Reopen the handle: it should reload from JSONL and the
    // client_message_id field must survive the disk round-trip.
    {
        let handle = SessionHandle::open(tmp.path(), &key);
        let history = handle.get_history(10);
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].content, "hi there");
        assert_eq!(
            history[0].client_message_id.as_deref(),
            Some("cmid-xyz"),
            "client_message_id must survive append-and-reload"
        );
    }
}

#[test]
fn test_session_handle_task_state_path_uses_sidecar_file() {
    let tmp = TempDir::new().unwrap();
    let key = SessionKey::new("api", "web-task-ledger");
    let handle = SessionHandle::open(tmp.path(), &key);

    let path = handle.task_state_path();
    assert!(path.ends_with("default.tasks.jsonl"));
    assert_eq!(
        path.parent().unwrap(),
        handle.session_path().parent().unwrap()
    );
}

#[test]
fn test_child_session_key_derivation_is_stable() {
    let parent = SessionKey::new("api", "web-task-ledger");
    let child = child_session_key(&parent, "spawn-01/alpha beta");

    assert_eq!(child.0, "api:web-task-ledger#child-spawn-01%2Falpha%20beta");
    assert_eq!(child.base_key(), "api:web-task-ledger");
    assert_eq!(child.topic(), Some("child-spawn-01%2Falpha%20beta"));
}

/// M8.6: `sanitize_loaded_messages` replaces the session's in-memory
/// transcript with the cleaned-up version and returns the report. No
/// disk state is touched until the caller rewrites.
#[test]
fn should_sanitize_loaded_messages_in_place() {
    use octos_core::ToolCall;

    let tmp = TempDir::new().unwrap();
    let key = SessionKey::new("api", "resume-test");
    let mut handle = SessionHandle::open(tmp.path(), &key);

    // Load an unresolved tool_call + a whitespace-only assistant
    // message into the handle directly.
    handle
        .session
        .messages
        .push(make_message(MessageRole::User, "hi"));
    handle.session.messages.push(Message {
        role: MessageRole::Assistant,
        content: String::new(),
        media: vec![],
        tool_calls: Some(vec![ToolCall {
            id: "unresolved-1".into(),
            name: "shell".into(),
            arguments: serde_json::json!({}),
            metadata: None,
        }]),
        tool_call_id: None,
        reasoning_content: None,
        client_message_id: None,
        thread_id: None,
        timestamp: chrono::Utc::now(),
    });
    handle.session.messages.push(Message {
        role: MessageRole::Assistant,
        content: "   ".into(),
        media: vec![],
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: None,
        client_message_id: None,
        thread_id: None,
        timestamp: chrono::Utc::now(),
    });

    let before = handle.session.messages.len();
    let (report, refs) = handle
        .sanitize_loaded_messages(None, None)
        .expect("clean outcome — no workspace root");

    assert_eq!(report.input_len, before);
    assert_eq!(report.unresolved_tool_uses_dropped, 1);
    assert_eq!(report.whitespace_only_dropped, 1);
    assert_eq!(report.output_len, 1);
    assert!(refs.is_empty());
    // Handle was mutated in place.
    assert_eq!(handle.session.messages.len(), 1);
    assert_eq!(handle.session.messages[0].content, "hi");
}

/// #2204 regression (real disk round-trip): a session whose persisted
/// transcript ends in an interrupted thinking-only assistant turn must, on a
/// COLD reload from disk, have that turn FAILED (dropped) — not resurrected
/// and resumed. Exercises the exact production path the session actor uses at
/// bootstrap: persist → `SessionHandle::open` (loads from disk) →
/// `sanitize_loaded_messages(None, ..)`.
#[tokio::test]
async fn cold_reload_fails_interrupted_thinking_only_tail() {
    let tmp = TempDir::new().unwrap();
    let key = SessionKey::new("cli", "coding");

    // Persist a completed user turn, then an interrupted thinking-only
    // assistant turn (empty content, non-empty reasoning, no tool calls) — the
    // killed reasoning spiral that used to be resurrected on the next launch.
    {
        let mut writer = SessionHandle::open(tmp.path(), &key);
        writer
            .add_message(make_message(MessageRole::User, "hi"))
            .await
            .unwrap();
        let spiral = Message {
            role: MessageRole::Assistant,
            content: String::new(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: Some(
                "... geometry topology manifold spacetime quantum mechanics ...".into(),
            ),
            client_message_id: None,
            thread_id: Some("01a05fda-turn".into()),
            timestamp: chrono::Utc::now(),
        };
        writer.add_message(spiral).await.unwrap();
    }

    // Cold reload: a fresh handle loads the transcript from disk, exactly as
    // the session actor does at bootstrap (retry_state = None).
    let mut handle = SessionHandle::open(tmp.path(), &key);
    assert_eq!(
        handle.session.messages.len(),
        2,
        "both persisted turns must load from disk"
    );

    let (report, _refs) = handle
        .sanitize_loaded_messages(None, None)
        .expect("clean outcome — no workspace root");

    assert_eq!(
        report.orphan_thinking_dropped, 1,
        "the interrupted thinking-only tail must be failed on cold reload"
    );
    assert_eq!(handle.session.messages.len(), 1);
    assert_eq!(handle.session.messages[0].content, "hi");
    assert!(
        handle
            .session
            .messages
            .last()
            .unwrap()
            .reasoning_content
            .is_none(),
        "no thinking-only turn survives to be resumed"
    );
}

/// M8.6: a missing worktree surfaces as `Err` and DOES NOT mutate the
/// session's in-memory transcript — callers can still log what was
/// loaded before deciding to refuse resume.
#[test]
fn should_preserve_messages_when_worktree_missing() {
    let tmp = TempDir::new().unwrap();
    let key = SessionKey::new("api", "resume-no-worktree");
    let mut handle = SessionHandle::open(tmp.path(), &key);

    handle
        .session
        .messages
        .push(make_message(MessageRole::User, "hi"));
    handle
        .session
        .messages
        .push(make_message(MessageRole::Assistant, "there"));

    let gone = tmp.path().join("ghost-worktree");
    let before_count = handle.session.messages.len();

    let outcome = handle.sanitize_loaded_messages(None, Some(&gone));

    match outcome {
        Err(crate::SanitizeError::WorktreeMissing { path, .. }) => {
            assert_eq!(path, gone);
        }
        other => panic!("expected WorktreeMissing, got {other:?}"),
    }
    // Transcript is preserved.
    assert_eq!(handle.session.messages.len(), before_count);
}

// ----------------------------------------------------------------------
// Item 3 of OCTOS_M8_FIX_FIRST_CHECKLIST_2026-04-24:
// worktree-missing must be a hard resume refusal. The session actor
// calls `clear_messages_for_unsafe_resume()` on Err so the in-memory
// transcript cannot be silently consumed by the first LLM call.
// ----------------------------------------------------------------------

#[test]
fn session_actor_refuses_resume_when_worktree_missing() {
    // Top-level session whose worktree was cleaned up. After the actor
    // clears the in-memory transcript, the handle must look like a
    // fresh session.
    let tmp = TempDir::new().unwrap();
    let key = SessionKey::new("api", "top-level-refusal");
    let mut handle = SessionHandle::open(tmp.path(), &key);
    handle
        .session
        .messages
        .push(make_message(MessageRole::User, "do thing"));
    handle.session.messages.push(make_message(
        MessageRole::Assistant,
        "I'll start working on it",
    ));

    // Step 1: sanitize sees a missing worktree and returns Err.
    let gone = tmp.path().join("ghost-worktree");
    let outcome = handle.sanitize_loaded_messages(None, Some(&gone));
    assert!(matches!(
        outcome,
        Err(crate::SanitizeError::WorktreeMissing { .. })
    ));

    // Step 2: session_actor responds with a hard refusal.
    assert!(
        !handle.is_child_session(),
        "test fixture is top-level (no parent_key)"
    );
    handle.clear_messages_for_unsafe_resume();
    assert_eq!(
        handle.session.messages.len(),
        0,
        "top-level worktree-missing refusal must drop the in-memory transcript"
    );
}

#[test]
fn session_actor_does_not_continue_with_unsanitized_transcript_on_worktree_missing() {
    // The legacy "warn and continue" branch left the original
    // transcript in `handle.session.messages` so the next LLM call
    // would see unresolved tool_calls / orphan thinking. Verify the
    // post-clear state is empty.
    let tmp = TempDir::new().unwrap();
    let key = SessionKey::new("api", "no-unsafe-llm-call");
    let mut handle = SessionHandle::open(tmp.path(), &key);
    // Add a transcript that previously would have been consumed unsafely:
    //   user → assistant with unresolved tool_call (no matching Tool result).
    handle
        .session
        .messages
        .push(make_message(MessageRole::User, "go"));
    handle.session.messages.push(Message {
        role: MessageRole::Assistant,
        content: String::new(),
        media: vec![],
        tool_calls: Some(vec![octos_core::ToolCall {
            id: "unresolved-1".into(),
            name: "shell".into(),
            arguments: serde_json::json!({}),
            metadata: None,
        }]),
        tool_call_id: None,
        reasoning_content: None,
        client_message_id: None,
        thread_id: None,
        timestamp: Utc::now(),
    });

    let gone = tmp.path().join("ghost-worktree");
    let outcome = handle.sanitize_loaded_messages(None, Some(&gone));
    assert!(matches!(
        outcome,
        Err(crate::SanitizeError::WorktreeMissing { .. })
    ));
    // Even though the sanitizer DID NOT mutate, the actor's hard
    // refusal must clear before any consumer reads `messages()`.
    handle.clear_messages_for_unsafe_resume();
    assert!(
        handle.session.messages.is_empty(),
        "no first LLM call must be made using the unsafe transcript"
    );
}

#[test]
fn background_child_session_marks_failed_when_worktree_missing() {
    // A child session has parent_key set. The session actor uses
    // `is_child_session()` to drive a "mark task failed" decision in
    // the supervisor (top-level decision is "drop transcript and
    // start fresh"). The state-clear is the same on both branches —
    // the difference is the operator-visible signal. We verify the
    // parent linkage flows through here so the actor can branch on it.
    let tmp = TempDir::new().unwrap();
    let parent = SessionKey::new("api", "parent-task");
    let child = SessionKey::new("api", "parent-task#child-job-01");
    let mut child_handle = SessionHandle::open(tmp.path(), &child);
    child_handle.session.parent_key = Some(parent.clone());
    child_handle
        .session
        .messages
        .push(make_message(MessageRole::User, "run"));

    assert!(
        child_handle.is_child_session(),
        "child session must report is_child_session=true"
    );

    let gone = tmp.path().join("ghost-child-worktree");
    let outcome = child_handle.sanitize_loaded_messages(None, Some(&gone));
    assert!(matches!(
        outcome,
        Err(crate::SanitizeError::WorktreeMissing { .. })
    ));
    child_handle.clear_messages_for_unsafe_resume();
    assert_eq!(
        child_handle.session.messages.len(),
        0,
        "child worktree-missing refusal must also clear the unsafe transcript"
    );
    // Parent linkage survives the clear so the supervisor can find
    // the parent on its mark-failed lookup.
    assert_eq!(child_handle.session.parent_key, Some(parent));
}

/// Helper to build the legacy-flat path for a key.
fn legacy_session_path(data_dir: &Path, key: &SessionKey) -> PathBuf {
    SessionManager::session_path_static(&data_dir.join("sessions"), key)
}

/// Helper to build the per-user session path for a key.
fn per_user_session_path(data_dir: &Path, key: &SessionKey) -> PathBuf {
    let encoded_base = encode_path_component(key.base_key());
    let topic = key.topic().unwrap_or("default");
    let encoded_topic = encode_path_component(topic);
    data_dir
        .join("users")
        .join(&encoded_base)
        .join("sessions")
        .join(format!("{encoded_topic}.jsonl"))
}

/// Helper to build the migration marker path for a key.
fn migration_marker_path_for(data_dir: &Path, key: &SessionKey) -> PathBuf {
    let encoded_base = encode_path_component(key.base_key());
    let topic = key.topic().unwrap_or("default");
    let encoded_topic = encode_path_component(topic);
    data_dir
        .join("users")
        .join(&encoded_base)
        .join("sessions")
        .join(format!(".migrated.{encoded_topic}"))
}

/// Write a minimal JSONL with one user message at `path`.
fn write_jsonl_with_one_user_message(path: &Path, key: &SessionKey, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let meta = serde_json::json!({
        "schema_version": 1,
        "session_key": key.0,
        "topic": key.topic(),
        "created_at": Utc::now(),
        "updated_at": Utc::now(),
    });
    let msg = make_message(MessageRole::User, content);
    let body = format!(
        "{}\n{}\n",
        serde_json::to_string(&meta).unwrap(),
        serde_json::to_string(&msg).unwrap()
    );
    std::fs::write(path, body).unwrap();
}

#[test]
fn migration_marker_skips_redundant_migration_when_present() {
    // Pre-condition: a per-user JSONL exists, the legacy flat file ALSO
    // exists (e.g. from a stale prior boot), and the per-key migration
    // marker is present — meaning a previous open already migrated and
    // confirmed remove. On `SessionHandle::open` we must skip the legacy
    // load AND the legacy delete entirely: the marker is the authoritative
    // signal that migration completed, so the per-user file wins and the
    // stale legacy file is left untouched (a separate operator cleanup
    // can remove it).
    let tmp = TempDir::new().unwrap();
    let key = SessionKey::new("api", "marker-skip");

    // 1) Per-user JSONL with the canonical content.
    let per_user_path = per_user_session_path(tmp.path(), &key);
    write_jsonl_with_one_user_message(&per_user_path, &key, "canonical");

    // 2) Stale legacy file with DIFFERENT content. If migration runs
    //    redundantly it would overwrite the per-user file with this
    //    legacy content — the test catches that.
    let legacy_path = legacy_session_path(tmp.path(), &key);
    write_jsonl_with_one_user_message(&legacy_path, &key, "STALE-LEGACY");

    // 3) Migration marker present.
    let marker = migration_marker_path_for(tmp.path(), &key);
    std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
    std::fs::write(&marker, b"migrated-from-flat\n").unwrap();

    // Open the handle — must skip legacy entirely.
    let handle = SessionHandle::open(tmp.path(), &key);
    let session = handle.session();

    assert_eq!(
        session.messages.len(),
        1,
        "marker present: must load per-user only, not merge legacy"
    );
    assert_eq!(
        session.messages[0].content, "canonical",
        "marker present: per-user content must win, legacy must NOT overwrite"
    );

    // Stale legacy file must remain untouched (we didn't delete it).
    assert!(
        legacy_path.exists(),
        "marker present + stale legacy: legacy file must remain (no redundant remove)"
    );
    // Marker still present.
    assert!(marker.exists(), "marker must remain after open");
}

#[test]
fn migration_retries_legacy_remove_when_marker_absent_but_per_user_exists() {
    // Pre-condition: a previous open succeeded `rewrite_blocking` (per-user
    // file written) but `remove_file(legacy)` failed (transient errno) —
    // so we ended up with both files on disk and NO marker. The next open
    // must detect this partial-migration shape and best-effort RETRY the
    // legacy removal. On success the marker is written.
    let tmp = TempDir::new().unwrap();
    let key = SessionKey::new("api", "marker-retry");

    // 1) Per-user JSONL exists — canonical state.
    let per_user_path = per_user_session_path(tmp.path(), &key);
    write_jsonl_with_one_user_message(&per_user_path, &key, "canonical");

    // 2) Legacy file ALSO exists (the failed-remove leftover).
    let legacy_path = legacy_session_path(tmp.path(), &key);
    write_jsonl_with_one_user_message(&legacy_path, &key, "legacy-leftover");

    // 3) Marker is ABSENT.
    let marker = migration_marker_path_for(tmp.path(), &key);
    assert!(
        !marker.exists(),
        "precondition: marker must not exist for this case"
    );

    // Open the handle — must retry the legacy removal.
    let _handle = SessionHandle::open(tmp.path(), &key);

    assert!(
        !legacy_path.exists(),
        "legacy file must be removed by the retry-on-open path"
    );
    assert!(
        marker.exists(),
        "marker must be written after the retry succeeds"
    );
}

// ---- M8.10 PR #1: thread_id persistence + legacy synthesis ---------------

/// Build a Message with a specific role and `client_message_id` for tests.
fn make_message_with_cmid(
    role: MessageRole,
    content: &str,
    client_message_id: Option<&str>,
) -> Message {
    let mut m = make_message(role, content);
    m.client_message_id = client_message_id.map(String::from);
    m
}

#[tokio::test]
async fn should_round_trip_thread_id() {
    // M8.10 PR #1: a freshly-arrived user message with a `client_message_id`
    // becomes the thread_id; the assistant reply inherits it; both survive
    // a JSONL save/load round-trip.
    let tmp = TempDir::new().unwrap();
    let key = SessionKey::new("api", "thread-roundtrip");

    {
        let mut handle = SessionHandle::open(tmp.path(), &key);
        handle
            .add_message(make_message_with_cmid(
                MessageRole::User,
                "ask",
                Some("cmid-alpha"),
            ))
            .await
            .unwrap();
        // PR F (M8.10): caller pre-stamps Assistant with the
        // originating turn's `thread_id`. In production this is the
        // canonical helper `persist_assistant_message` (see
        // `octos-cli/src/session_actor.rs`); the test mirrors that.
        let mut assistant = make_message(MessageRole::Assistant, "answer");
        assistant.thread_id = Some("cmid-alpha".into());
        handle.add_message(assistant).await.unwrap();
    }

    let reload = SessionHandle::open(tmp.path(), &key);
    let session = reload.session();
    assert_eq!(session.messages.len(), 2);
    assert_eq!(
        session.messages[0].thread_id.as_deref(),
        Some("cmid-alpha"),
        "user message thread_id == its client_message_id"
    );
    assert_eq!(
        session.messages[1].thread_id.as_deref(),
        Some("cmid-alpha"),
        "assistant inherits the rooting user's thread_id"
    );
}

#[tokio::test]
async fn should_synthesize_thread_id_for_legacy_record_without_field() {
    // Pre-existing JSONL written by a prior octos build that didn't know
    // about thread_id. On load the synthesizer must thread the messages
    // using the `client_message_id` hints already present (or
    // synth_{seq} when even those are missing) so `Session::threads()`
    // produces a sensible grouping.
    let tmp = TempDir::new().unwrap();
    let key = SessionKey::new("api", "legacy-synth");
    let per_user_path = per_user_session_path(tmp.path(), &key);
    std::fs::create_dir_all(per_user_path.parent().unwrap()).unwrap();

    let meta = serde_json::json!({
        "schema_version": 1,
        "session_key": key.0,
        "topic": key.topic(),
        "created_at": Utc::now(),
        "updated_at": Utc::now(),
    });
    // Legacy user with cmid (note: no thread_id field at all).
    let user_with_cmid = serde_json::json!({
        "role": "user",
        "content": "hello",
        "client_message_id": "cmid-legacy-1",
        "timestamp": Utc::now().to_rfc3339(),
    });
    // Legacy assistant — no cmid, no thread_id.
    let asst_no_cmid = serde_json::json!({
        "role": "assistant",
        "content": "hi back",
        "timestamp": Utc::now().to_rfc3339(),
    });
    let body = format!(
        "{}\n{}\n{}\n",
        serde_json::to_string(&meta).unwrap(),
        serde_json::to_string(&user_with_cmid).unwrap(),
        serde_json::to_string(&asst_no_cmid).unwrap(),
    );
    std::fs::write(&per_user_path, body).unwrap();

    let handle = SessionHandle::open(tmp.path(), &key);
    let session = handle.session();
    assert_eq!(session.messages.len(), 2);
    assert_eq!(
        session.messages[0].thread_id.as_deref(),
        Some("cmid-legacy-1"),
        "legacy user with cmid synthesizes thread_id == cmid"
    );
    assert_eq!(
        session.messages[1].thread_id.as_deref(),
        Some("cmid-legacy-1"),
        "legacy assistant inherits the current thread"
    );
}

#[tokio::test]
async fn should_synthesize_threads_from_real_legacy_session() {
    // Real-world legacy JSONL: a multi-turn transcript with a mix of
    // cmid-present and cmid-absent rows, and a tool result wedged
    // between assistant turns. The synthesizer must produce the same
    // thread grouping that the new write path would have produced.
    let tmp = TempDir::new().unwrap();
    let key = SessionKey::new("api", "legacy-multi-turn");
    let per_user_path = per_user_session_path(tmp.path(), &key);
    std::fs::create_dir_all(per_user_path.parent().unwrap()).unwrap();

    let now = Utc::now();
    let meta = serde_json::json!({
        "schema_version": 1,
        "session_key": key.0,
        "topic": key.topic(),
        "created_at": now,
        "updated_at": now,
    });
    // Turn 1: user with cmid, then assistant + tool.
    let u1 = serde_json::json!({
        "role": "user",
        "content": "first",
        "client_message_id": "cmid-1",
        "timestamp": now.to_rfc3339(),
    });
    let a1 = serde_json::json!({
        "role": "assistant",
        "content": "calling tool",
        "timestamp": (now + chrono::Duration::seconds(1)).to_rfc3339(),
    });
    let t1 = serde_json::json!({
        "role": "tool",
        "content": "tool result for turn 1",
        "tool_call_id": "tc-1",
        "timestamp": (now + chrono::Duration::seconds(2)).to_rfc3339(),
    });
    let a1b = serde_json::json!({
        "role": "assistant",
        "content": "answer to first",
        "timestamp": (now + chrono::Duration::seconds(3)).to_rfc3339(),
    });
    // Turn 2: user WITHOUT cmid (legacy: had no client_message_id at all).
    let u2 = serde_json::json!({
        "role": "user",
        "content": "second",
        "timestamp": (now + chrono::Duration::seconds(10)).to_rfc3339(),
    });
    let a2 = serde_json::json!({
        "role": "assistant",
        "content": "answer to second",
        "timestamp": (now + chrono::Duration::seconds(11)).to_rfc3339(),
    });

    let body = format!(
        "{meta_line}\n{u1}\n{a1}\n{t1}\n{a1b}\n{u2}\n{a2}\n",
        meta_line = serde_json::to_string(&meta).unwrap(),
        u1 = serde_json::to_string(&u1).unwrap(),
        a1 = serde_json::to_string(&a1).unwrap(),
        t1 = serde_json::to_string(&t1).unwrap(),
        a1b = serde_json::to_string(&a1b).unwrap(),
        u2 = serde_json::to_string(&u2).unwrap(),
        a2 = serde_json::to_string(&a2).unwrap(),
    );
    std::fs::write(&per_user_path, body).unwrap();

    let handle = SessionHandle::open(tmp.path(), &key);
    let session = handle.session();
    assert_eq!(session.messages.len(), 6);

    // First turn — all four messages share the cmid-1 thread.
    for (i, expected_role) in [
        (0, MessageRole::User),
        (1, MessageRole::Assistant),
        (2, MessageRole::Tool),
        (3, MessageRole::Assistant),
    ] {
        assert_eq!(
            session.messages[i].thread_id.as_deref(),
            Some("cmid-1"),
            "msg {i} ({expected_role:?}) must inherit cmid-1 thread"
        );
        assert_eq!(session.messages[i].role, expected_role);
    }

    // Second turn — user without cmid gets a synth_{seq}.
    let synth = session.messages[4].thread_id.clone().expect("synthesized");
    assert!(
        synth.starts_with("synth_"),
        "user without cmid must get synth_<seq> thread_id (got {synth:?})"
    );
    assert_eq!(
        session.messages[5].thread_id.as_deref(),
        Some(synth.as_str()),
        "assistant after legacy user without cmid inherits the synth thread"
    );

    // threads() groups them sensibly.
    let threads = session.threads();
    assert_eq!(threads.len(), 2, "two user-rooted threads");
    assert_eq!(threads[0].id, "cmid-1");
    assert_eq!(threads[0].user_msg.content, "first");
    assert_eq!(threads[0].responses.len(), 3);
    assert_eq!(threads[0].intra_thread_seq, 0);
    assert_eq!(threads[1].id, synth);
    assert_eq!(threads[1].user_msg.content, "second");
    assert_eq!(threads[1].responses.len(), 1);
    assert_eq!(threads[1].intra_thread_seq, 1);
}

#[test]
fn derive_thread_id_user_uses_client_message_id() {
    let user_msg = make_message_with_cmid(MessageRole::User, "hi", Some("cmid-x"));
    let id = derive_thread_id_for_legacy_load(&user_msg, &[]);
    assert_eq!(id.as_deref(), Some("cmid-x"));
}

#[test]
fn derive_thread_id_user_without_cmid_synthesizes_uuid() {
    let user_msg = make_message(MessageRole::User, "hi");
    let id = derive_thread_id_for_legacy_load(&user_msg, &[]);
    assert!(
        id.is_some(),
        "user without cmid still gets a synthesized id"
    );
    // UUIDv7 surface — at least standard hyphenated length.
    assert!(id.as_deref().unwrap().contains('-'));
}

#[test]
fn derive_thread_id_assistant_inherits_from_recent_user() {
    let mut user_msg = make_message_with_cmid(MessageRole::User, "ask", Some("cmid-q"));
    user_msg.thread_id = Some("cmid-q".into());
    let history = vec![user_msg];
    let asst = make_message(MessageRole::Assistant, "answer");
    let id = derive_thread_id_for_legacy_load(&asst, &history);
    assert_eq!(id.as_deref(), Some("cmid-q"));
}

#[test]
fn derive_thread_id_tool_inherits_from_assistant() {
    let mut user_msg = make_message_with_cmid(MessageRole::User, "ask", Some("cmid-q"));
    user_msg.thread_id = Some("cmid-q".into());
    let mut asst = make_message(MessageRole::Assistant, "tool call");
    asst.thread_id = Some("cmid-q".into());
    let history = vec![user_msg, asst];
    let tool = make_message(MessageRole::Tool, "tool result");
    let id = derive_thread_id_for_legacy_load(&tool, &history);
    assert_eq!(id.as_deref(), Some("cmid-q"));
}

#[test]
fn derive_thread_id_system_returns_none() {
    let sys = make_message(MessageRole::System, "system primer");
    let id = derive_thread_id_for_legacy_load(&sys, &[]);
    assert!(id.is_none(), "system messages aren't thread-scoped");
}

/// PR F (M8.10): the new-write derivation MUST refuse Assistant rows
/// that arrived without a caller-supplied `thread_id`. Pre-fix, this
/// silently walked history backwards and picked the most-recent user
/// — under concurrent rapid-fire writes that's the WRONG turn (a
/// sibling user has rotated the in-memory history between the
/// originating turn's user-write and its assistant-write). The
/// metric `octos_session_persist_total{outcome="rejected_unbound_assistant"}`
/// is the alerting hook for soak.
#[test]
fn derive_thread_id_for_new_write_rejects_unbound_assistant() {
    let mut user_msg = make_message_with_cmid(MessageRole::User, "ask", Some("cmid-q"));
    user_msg.thread_id = Some("cmid-q".into());
    let history = vec![user_msg];
    let asst = make_message(MessageRole::Assistant, "answer");
    let result = derive_thread_id_for_new_write(&asst, &history);
    assert!(
        result.is_err(),
        "new-write path must fail-closed for unbound Assistant; got {result:?}"
    );
}

/// PR F: the new-write derivation MUST refuse Tool rows that arrived
/// without a caller-supplied `thread_id`, for the same structural
/// reason as Assistant. Tools must inherit from the originating turn
/// (carried via `originating_thread_id` in BackgroundResultPayload),
/// not from history walking.
#[test]
fn derive_thread_id_for_new_write_rejects_unbound_tool() {
    let mut user_msg = make_message_with_cmid(MessageRole::User, "ask", Some("cmid-q"));
    user_msg.thread_id = Some("cmid-q".into());
    let history = vec![user_msg];
    let tool = make_message(MessageRole::Tool, "tool result");
    let result = derive_thread_id_for_new_write(&tool, &history);
    assert!(
        result.is_err(),
        "new-write path must fail-closed for unbound Tool; got {result:?}"
    );
}

/// PR F: User rows always synthesize/derive cleanly (cmid → thread_id).
/// System rows return `Ok(None)`. These remain non-fail-closed
/// because the rule is structurally well-defined for them.
#[test]
fn derive_thread_id_for_new_write_accepts_user_and_system() {
    let user_msg = make_message_with_cmid(MessageRole::User, "ask", Some("cmid-q"));
    let id = derive_thread_id_for_new_write(&user_msg, &[]).expect("user must succeed");
    assert_eq!(id.as_deref(), Some("cmid-q"));

    let sys = make_message(MessageRole::System, "primer");
    let id = derive_thread_id_for_new_write(&sys, &[]).expect("system must succeed");
    assert!(id.is_none());
}

/// PR F: legacy JSONL replay must continue to gap-fill via
/// `derive_thread_id_for_legacy_load` — a transcript that pre-dates
/// the PR #1 typed write path has Assistant rows without thread_id
/// and must reconstruct via history walking. The new-write
/// fail-closed split MUST NOT regress this.
#[tokio::test]
async fn add_message_with_seq_accepts_legacy_replay_via_legacy_load() {
    let tmp = TempDir::new().unwrap();
    let key = SessionKey::new("api", "legacy-replay");

    // Build a session in memory with legacy rows (no thread_id) using
    // bare struct literals so the test exercises the actual legacy
    // wire shape (Assistant/Tool rows lacking thread_id).
    fn legacy_msg(role: MessageRole, content: &str, cmid: Option<&str>) -> Message {
        Message {
            role,
            content: content.into(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: cmid.map(String::from),
            thread_id: None,
            timestamp: Utc::now(),
        }
    }
    let mut messages = vec![
        legacy_msg(MessageRole::User, "first", Some("cmid-old-1")),
        legacy_msg(MessageRole::Assistant, "first reply", None),
        legacy_msg(MessageRole::User, "second", Some("cmid-old-2")),
        legacy_msg(MessageRole::Assistant, "second reply", None),
    ];
    for m in &mut messages {
        assert!(m.thread_id.is_none(), "legacy fixture must lack thread_id");
    }
    synthesize_thread_ids(&mut messages);
    // Synthesis populates every row.
    assert_eq!(messages[0].thread_id.as_deref(), Some("cmid-old-1"));
    assert_eq!(messages[1].thread_id.as_deref(), Some("cmid-old-1"));
    assert_eq!(messages[2].thread_id.as_deref(), Some("cmid-old-2"));
    assert_eq!(messages[3].thread_id.as_deref(), Some("cmid-old-2"));

    // After legacy gap-fill, the rows can be re-persisted via the
    // new-write path because the gap-fill stamped thread_id.
    let mut handle = SessionHandle::open(tmp.path(), &key);
    for m in messages {
        handle
            .add_message(m)
            .await
            .expect("legacy-gap-filled rows must persist via new write path");
    }
}

#[tokio::test]
async fn add_message_with_seq_stamps_thread_id_on_inbound_messages() {
    // The new write path must populate thread_id on the persisted line so
    // a later reload doesn't have to fall back to synthesis.
    //
    // PR F (M8.10): the persist path is fail-closed for unbound
    // Assistant — the caller must pre-stamp `thread_id`. The test
    // mirrors production: the User row derives its thread_id from
    // its `client_message_id`; the Assistant row arrives pre-stamped
    // with the originating turn's `thread_id` (in production via
    // `Message::assistant_with_thread`).
    let tmp = TempDir::new().unwrap();
    let key = SessionKey::new("api", "stamp-on-write");

    let mut handle = SessionHandle::open(tmp.path(), &key);
    handle
        .add_message(make_message_with_cmid(
            MessageRole::User,
            "first",
            Some("cmid-write-1"),
        ))
        .await
        .unwrap();
    let mut assistant = make_message(MessageRole::Assistant, "first reply");
    assistant.thread_id = Some("cmid-write-1".into());
    handle.add_message(assistant).await.unwrap();

    // In-memory copy carries thread_id.
    assert_eq!(
        handle.session().messages[0].thread_id.as_deref(),
        Some("cmid-write-1")
    );
    assert_eq!(
        handle.session().messages[1].thread_id.as_deref(),
        Some("cmid-write-1")
    );

    // Persisted JSONL line carries it too — verified by checking the file.
    let per_user_path = per_user_session_path(tmp.path(), &key);
    let content = std::fs::read_to_string(&per_user_path).unwrap();
    assert!(
        content.contains("\"thread_id\":\"cmid-write-1\""),
        "persisted JSONL must include thread_id on the user message: {content}"
    );
}

#[test]
fn session_threads_returns_empty_for_session_without_user_messages() {
    let session = Session::new(SessionKey::new("cli", "empty"));
    assert!(session.threads().is_empty());
}

#[test]
fn session_threads_skips_system_messages() {
    let mut session = Session::new(SessionKey::new("cli", "with-system"));
    let mut sys = make_message(MessageRole::System, "primer");
    sys.thread_id = None; // explicitly None — system messages aren't thread-scoped
    session.messages.push(sys);
    let mut user = make_message_with_cmid(MessageRole::User, "ask", Some("cmid-1"));
    user.thread_id = Some("cmid-1".into());
    session.messages.push(user);

    let threads = session.threads();
    assert_eq!(threads.len(), 1);
    assert_eq!(threads[0].id, "cmid-1");
    assert_eq!(threads[0].responses.len(), 0);
}

/// Regression for codex retro-review BLOCKING #1: SessionHandle::append_to_disk
/// must return Err on size-cap rejection (was returning Ok(()), letting the
/// caller push to memory and fire message/persisted observer for a row that
/// never committed to disk — UPCR-2026-012 contract violation).
#[tokio::test]
async fn session_handle_append_returns_err_when_at_size_cap() {
    let tmp = TempDir::new().unwrap();
    let key = SessionKey::new("api", "web-cap-test");
    let mut handle = SessionHandle::open(tmp.path(), &key);

    // Pre-fill the JSONL above MAX_SESSION_FILE_SIZE so the next
    // append must refuse.
    let path = handle.session_path();
    std::fs::create_dir_all(path.parent().unwrap()).ok();
    let oversize = vec![b'a'; (MAX_SESSION_FILE_SIZE + 1) as usize];
    std::fs::write(&path, oversize).expect("pre-fill oversize jsonl");

    let msg = make_message(MessageRole::User, "after-cap");
    let result = handle.add_message_with_seq(msg).await;

    assert!(
        result.is_err(),
        "add_message_with_seq must return Err when file at size cap; got {result:?}"
    );
    // In-memory state must NOT advance on a refused append.
    assert_eq!(
        handle.get_history(10).len(),
        0,
        "no message should be in memory when disk append refused"
    );
}

/// Regression for codex retro-review BLOCKING #2: SessionManager::session_known
/// must check both legacy flat layout AND canonical per-user layout. Without
/// this dual check, after LRU eviction or daemon restart, sessions persisted
/// via ApiChannel's per-user path become un-discoverable to UPCR-2026-009 /
/// -010 / -011 handlers.
#[tokio::test]
async fn session_known_finds_canonical_per_user_path_after_restart() {
    let tmp = TempDir::new().unwrap();
    let sessions_dir = tmp.path().join("sessions");
    std::fs::create_dir_all(&sessions_dir).unwrap();

    // Simulate ApiChannel's canonical layout:
    // <data_dir>/users/<encoded-base>/sessions/<encoded-topic>.jsonl
    let users_dir = tmp.path().join("users");
    let key = SessionKey::new("api", "user42#chat-1");
    let base_key = key.base_key();
    let encoded_base = encode_path_component(base_key);
    let topic = key.topic().unwrap_or("default");
    let encoded_topic = encode_path_component(topic);
    let per_user_path = users_dir
        .join(&encoded_base)
        .join("sessions")
        .join(format!("{encoded_topic}.jsonl"));
    std::fs::create_dir_all(per_user_path.parent().unwrap()).unwrap();
    std::fs::write(&per_user_path, b"{}\n").unwrap();

    // Fresh manager (cache empty — simulates post-restart). The legacy flat
    // path does NOT exist; only the canonical per-user path exists.
    let mut mgr = SessionManager::open(tmp.path()).expect("open SessionManager");
    assert!(
        !mgr.session_path(&key).exists(),
        "precondition: flat path must NOT exist for this test"
    );
    assert!(
        mgr.session_known(&key),
        "session_known must find session via canonical per-user layout \
             when only that layout has the file (regression for UPCR-2026-009/010/011 handlers)"
    );
}

// --- list_for_analysis / export_transcript (memory-refresh sweep) ---

#[tokio::test]
async fn should_list_both_layout_files_when_session_exists_in_both() {
    let dir = tempfile::tempdir().unwrap();
    let mgr = SessionManager::open(dir.path()).unwrap();
    let key = SessionKey("tg:42#research".to_string());

    write_jsonl_with_one_user_message(&legacy_session_path(dir.path(), &key), &key, "old row");
    write_jsonl_with_one_user_message(&per_user_session_path(dir.path(), &key), &key, "new row");

    let sessions = mgr.list_for_analysis();
    assert_eq!(sessions.len(), 1);
    let s = &sessions[0];
    assert_eq!(s.key.0, "tg:42#research");
    assert_eq!(s.files.len(), 2, "both physical copies must be reported");
    assert!(s.files.iter().all(|f| f.len > 0));
    assert!(!s.internal);
}

#[tokio::test]
async fn should_reconstruct_base_key_when_per_user_default_topic() {
    let dir = tempfile::tempdir().unwrap();
    let mgr = SessionManager::open(dir.path()).unwrap();
    let key = SessionKey("discord:99".to_string());
    write_jsonl_with_one_user_message(&per_user_session_path(dir.path(), &key), &key, "hi");

    let sessions = mgr.list_for_analysis();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].key.0, "discord:99");
}

#[tokio::test]
async fn should_mark_internal_when_spawn_child_or_parented() {
    let dir = tempfile::tempdir().unwrap();
    let mgr = SessionManager::open(dir.path()).unwrap();

    for key_str in ["tg:1#spawn-worker1", "tg:1#child-abc", "tg:1#roadmap.tasks"] {
        let key = SessionKey(key_str.to_string());
        write_jsonl_with_one_user_message(&per_user_session_path(dir.path(), &key), &key, "x");
    }
    // Parented session: meta carries parent_key.
    let parented = SessionKey("tg:1#forked".to_string());
    let path = per_user_session_path(dir.path(), &parented);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let meta = serde_json::json!({
        "schema_version": 1,
        "session_key": parented.0,
        "parent_key": "tg:1",
        "created_at": chrono::Utc::now(),
        "updated_at": chrono::Utc::now(),
    });
    std::fs::write(&path, format!("{meta}\n")).unwrap();

    let sessions = mgr.list_for_analysis();
    assert_eq!(sessions.len(), 4);
    assert!(
        sessions.iter().all(|s| s.internal),
        "spawn/child/tasks/parented sessions must all be internal: {:?}",
        sessions
            .iter()
            .map(|s| (&s.key.0, s.internal))
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn should_fold_rollback_and_stay_read_only_when_exporting_transcript() {
    let dir = tempfile::tempdir().unwrap();
    let mut mgr = SessionManager::open(dir.path()).unwrap();
    let key = SessionKey("tg:7".to_string());

    mgr.add_message(&key, make_message(MessageRole::User, "keep me"))
        .await
        .unwrap();
    mgr.add_message(&key, make_message(MessageRole::Assistant, "kept reply"))
        .await
        .unwrap();
    mgr.add_message(&key, make_message(MessageRole::User, "roll me back"))
        .await
        .unwrap();
    mgr.rollback_last_n_user_turns(&key, 1).await.unwrap();

    let transcript = mgr.export_transcript(&key).await.expect("session loads");
    let texts: Vec<&str> = transcript.iter().map(|(_, m)| m.content.as_str()).collect();
    assert!(texts.contains(&"keep me"));
    assert!(
        !texts.contains(&"roll me back"),
        "rolled-back turn must be folded out: {texts:?}"
    );
    // Indices are dense positions into the folded transcript.
    for (expected, (idx, _)) in transcript.iter().enumerate() {
        assert_eq!(expected, *idx);
    }
}

#[tokio::test]
async fn should_not_migrate_legacy_file_when_exporting_transcript() {
    let dir = tempfile::tempdir().unwrap();
    let mgr = SessionManager::open(dir.path()).unwrap();
    let key = SessionKey("tg:legacy#old".to_string());
    let legacy = legacy_session_path(dir.path(), &key);
    write_jsonl_with_one_user_message(&legacy, &key, "legacy row");

    let transcript = mgr.export_transcript(&key).await.expect("loads");
    assert_eq!(transcript.len(), 1);

    assert!(
        legacy.exists(),
        "export must not delete/migrate the legacy file"
    );
    assert!(
        !dir.path().join("users").exists(),
        "export must not create the per-user tree"
    );
}
