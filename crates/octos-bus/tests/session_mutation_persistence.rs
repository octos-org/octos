//! Session mutation regressions through the real gateway and persistence paths.
#![cfg(feature = "api")]
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use octos_bus::{ApiChannel, Channel, SessionHandle, SessionManager};
use octos_core::{Message, SessionKey};

const TOKEN: &str = "t0193-gateway-test-token";
const ID: &str = "web-legacy";

fn key() -> SessionKey {
    SessionKey::with_profile("_main", "api", ID)
}

struct Gateway {
    port: u16,
    task: tokio::task::JoinHandle<eyre::Result<()>>,
}
impl Drop for Gateway {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn gateway(dir: &Path) -> Gateway {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let channel = ApiChannel::new(
        port,
        Some(TOKEN.into()),
        Arc::new(AtomicBool::new(false)),
        Arc::new(tokio::sync::Mutex::new(SessionManager::open(dir).unwrap())),
        None,
    );
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    let task = tokio::spawn(async move { channel.start(tx).await });
    for _ in 0..300 {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            return Gateway { port, task };
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("gateway did not bind");
}
async fn delete(g: &Gateway) -> u16 {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .delete(format!("http://127.0.0.1:{}/sessions/{ID}", g.port))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}
async fn rename(g: &Gateway, title: &str) -> u16 {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .patch(format!("http://127.0.0.1:{}/sessions/{ID}/title", g.port))
        .bearer_auth(TOKEN)
        .header("content-type", "application/json")
        .body(serde_json::json!({ "title": title }).to_string())
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}
fn per_user(dir: &Path) -> PathBuf {
    let base = octos_bus::session::encode_path_component(key().base_key());
    dir.join("users")
        .join(base)
        .join("sessions")
        .join("default.jsonl")
}
fn lines(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .map(|s| s.lines().count())
        .unwrap_or(0)
}
async fn only_per_user_copy(dir: &Path) {
    let mut h = SessionHandle::open(dir, &key());
    h.add_message(Message::user("the only copy")).await.unwrap();
}
fn texts(messages: &[Message]) -> Vec<String> {
    messages.iter().map(|m| m.content.clone()).collect()
}

#[tokio::test]
async fn should_distinguish_truly_missing_sessions() {
    let d = tempfile::tempdir().unwrap();
    let g = gateway(d.path()).await;
    let (del, ren) = (delete(&g).await, rename(&g, "x").await);
    println!("a0 missing: delete={del} rename={ren}");
    assert_eq!((del, ren), (204, 404));
}

#[tokio::test]
async fn should_mutate_readable_sessions() {
    let d = tempfile::tempdir().unwrap();
    only_per_user_copy(d.path()).await;
    let g = gateway(d.path()).await;
    let ren = rename(&g, "ok").await;
    let del = delete(&g).await;
    println!(
        "a1 readable: rename={ren} delete={del} file_exists={}",
        per_user(d.path()).exists()
    );
    assert_eq!((ren, del), (204, 204));
    assert!(!per_user(d.path()).exists());
}

#[tokio::test]
#[cfg(unix)]
async fn should_report_permission_errors_and_allow_retry() {
    let d = tempfile::tempdir().unwrap();
    only_per_user_copy(d.path()).await;
    let f = per_user(d.path());
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::File::open(&f).is_ok() {
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
        eprintln!("privileged runner bypasses EACCES fixture");
        return;
    }
    let g = gateway(d.path()).await;
    let (del, ren) = (delete(&g).await, rename(&g, "x").await);
    let still = f.exists();
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
    let fresh = SessionManager::open(d.path()).unwrap();
    let back = fresh.load(&key()).await.map(|s| texts(&s.messages));
    println!(
        "a2 eacces: delete={del} rename={ren} file_still_there={still} after_chmod_back={back:?}"
    );
    assert_eq!((del, ren), (500, 500));
    assert!(still);
    assert_eq!(rename(&g, "retry").await, 204);
    assert_eq!(delete(&g).await, 204);
}

#[tokio::test]
async fn should_report_directory_read_errors() {
    let d = tempfile::tempdir().unwrap();
    only_per_user_copy(d.path()).await;
    let f = per_user(d.path());
    std::fs::remove_file(&f).unwrap();
    std::fs::create_dir(&f).unwrap();
    let g = gateway(d.path()).await;
    let (del, ren) = (delete(&g).await, rename(&g, "x").await);
    println!(
        "a3 directory: delete={del} rename={ren} still_there={}",
        f.exists()
    );
    assert_eq!((del, ren), (500, 500));
    assert!(f.exists());
}

#[tokio::test]
async fn should_report_invalid_utf8() {
    let d = tempfile::tempdir().unwrap();
    only_per_user_copy(d.path()).await;
    let f = per_user(d.path());
    std::fs::write(&f, [0xff_u8, 0xfe, 0x00, 0x0a, 0xff, 0x0a]).unwrap();
    let g = gateway(d.path()).await;
    let (del, ren) = (delete(&g).await, rename(&g, "x").await);
    println!(
        "a4 corrupt: delete={del} rename={ren} still_there={}",
        f.exists()
    );
    assert_eq!((del, ren), (500, 500));
    assert!(f.exists());
}

/// The actor writes through SessionHandle; the gateway's SessionManager keeps its own LRU copy.
#[tokio::test]
async fn should_keep_actor_writes_between_renames() {
    let d = tempfile::tempdir().unwrap();
    let mut actor = SessionHandle::open(d.path(), &key());
    actor.add_message(Message::user("m1")).await.unwrap();
    let g = gateway(d.path()).await;
    assert_eq!(rename(&g, "first").await, 204);
    actor.add_message(Message::user("m2")).await.unwrap();
    actor.add_message(Message::user("m3")).await.unwrap();
    let before = lines(&per_user(d.path()));
    assert_eq!(rename(&g, "second").await, 204);
    let after = lines(&per_user(d.path()));
    let fresh = SessionManager::open(d.path())
        .unwrap()
        .load(&key())
        .await
        .unwrap();
    println!(
        "b1 per_user lines before={before} after={after}; cold manager={:?} title={:?}",
        texts(&fresh.messages),
        fresh.title
    );
    assert_eq!(
        texts(&fresh.messages),
        ["m1", "m2", "m3"],
        "history written since the first rename was lost"
    );
}

/// Same, while the gateway process has no first rename: only a cached earlier read would matter.
#[tokio::test]
async fn should_keep_actor_writes_before_first_rename() {
    let d = tempfile::tempdir().unwrap();
    let mut actor = SessionHandle::open(d.path(), &key());
    actor.add_message(Message::user("m1")).await.unwrap();
    let g = gateway(d.path()).await;
    actor.add_message(Message::user("m2")).await.unwrap();
    assert_eq!(rename(&g, "only").await, 204);
    let fresh = SessionManager::open(d.path())
        .unwrap()
        .load(&key())
        .await
        .unwrap();
    println!(
        "b2 cold manager={:?} title={:?}",
        texts(&fresh.messages),
        fresh.title
    );
    assert_eq!(texts(&fresh.messages), ["m1", "m2"]);
}

#[tokio::test]
async fn should_preserve_manual_title_when_actor_rewrites_after_next_turn() {
    let d = tempfile::tempdir().unwrap();
    let mut actor = SessionHandle::open(d.path(), &key());
    actor.add_message(Message::user("first")).await.unwrap();
    let g = gateway(d.path()).await;
    assert_eq!(rename(&g, "manual").await, 204);
    actor.add_message(Message::user("next turn")).await.unwrap();
    actor.sort_by_timestamp();
    actor.rewrite().await.unwrap();
    let persisted: serde_json::Value = serde_json::from_str(
        std::fs::read_to_string(per_user(d.path()))
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(persisted["title"], "manual");
    assert_eq!(persisted["title_manual"], true);
    assert_eq!(
        texts(
            &SessionManager::open(d.path())
                .unwrap()
                .load(&key())
                .await
                .unwrap()
                .messages
        ),
        ["first", "next turn"]
    );
}

#[tokio::test]
async fn should_preserve_sealed_segments_and_control_rows_on_rename() {
    for canonical in [false, true] {
        let d = tempfile::tempdir().unwrap();
        let mut manager = SessionManager::open(d.path()).unwrap();
        let active = if canonical {
            only_per_user_copy(d.path()).await;
            per_user(d.path())
        } else {
            manager
                .add_message(&key(), Message::user("sealed history"))
                .await
                .unwrap();
            manager.session_path(&key())
        };
        let sealed_dir = active.with_extension("segments");
        std::fs::create_dir(&sealed_dir).unwrap();
        let sealed = sealed_dir.join("000001.jsonl");
        let sealed_bytes = std::fs::read(&active).unwrap();
        std::fs::write(&sealed, &sealed_bytes).unwrap();
        let mut meta: serde_json::Value = serde_json::from_str(
            std::str::from_utf8(&sealed_bytes)
                .unwrap()
                .lines()
                .next()
                .unwrap(),
        )
        .unwrap();
        meta["sealed_segments"] = 1.into();
        meta["base_seq"] = 1.into();
        meta["future_metadata"] = "preserved".into();
        let rows = format!(
            "{}\n{}\n",
            serde_json::to_string(&Message::user("rolled back")).unwrap(),
            serde_json::json!({"kind":"rollback", "num_turns":1, "at":chrono::Utc::now()})
        );
        std::fs::write(&active, format!("{meta}\n{rows}")).unwrap();
        assert!(
            manager
                .update_title_existing(&key(), "manual".into())
                .await
                .unwrap()
        );
        let after = std::fs::read_to_string(&active).unwrap();
        assert_eq!(after.split_once('\n').unwrap().1, rows);
        assert_eq!(std::fs::read(&sealed).unwrap(), sealed_bytes);
        let after_meta: serde_json::Value =
            serde_json::from_str(after.lines().next().unwrap()).unwrap();
        meta["title"] = "manual".into();
        meta["title_manual"] = true.into();
        assert_eq!(after_meta, meta);
        assert_eq!(
            SessionManager::open(d.path())
                .unwrap()
                .load_full(&key())
                .await
                .unwrap()
                .messages
                .len(),
            1
        );
    }
}

#[tokio::test]
async fn should_reject_untrusted_metadata_before_mutation_in_both_layouts() {
    for canonical in [false, true] {
        for fault in ["meta", "identity", "future"] {
            let d = tempfile::tempdir().unwrap();
            let mut manager = SessionManager::open(d.path()).unwrap();
            let active = if canonical {
                only_per_user_copy(d.path()).await;
                per_user(d.path())
            } else {
                manager
                    .add_message(&key(), Message::user("history"))
                    .await
                    .unwrap();
                manager.session_path(&key())
            };
            let original = std::fs::read_to_string(&active).unwrap();
            let damaged = match fault {
                "meta" => "{broken}\n".to_owned(),
                "identity" => original.replacen(&key().0, "other:api:foreign", 1),
                _ => original.replacen("\"schema_version\":2", "\"schema_version\":999", 1),
            };
            std::fs::write(&active, &damaged).unwrap();
            assert!(
                manager
                    .update_title_existing(&key(), "refused".into())
                    .await
                    .is_err()
            );
            assert!(manager.clear_existing(&key()).await.is_err());
            assert_eq!(std::fs::read_to_string(&active).unwrap(), damaged);
            std::fs::write(&active, &original).unwrap();
            assert!(
                manager
                    .update_title_existing(&key(), "retry".into())
                    .await
                    .unwrap()
            );
            assert!(manager.clear_existing(&key()).await.unwrap());
        }
    }
}

#[tokio::test]
async fn should_preserve_manual_title_when_stale_actor_rolls_a_segment() {
    let d = tempfile::tempdir().unwrap();
    let mut actor = SessionHandle::open(d.path(), &key());
    actor
        .add_message(Message::user("x".repeat(8 * 1024 * 1024 + 1024)))
        .await
        .unwrap();
    let mut manager = SessionManager::open(d.path()).unwrap();
    assert!(
        manager
            .update_title_existing(&key(), "manual before roll".into())
            .await
            .unwrap()
    );
    actor
        .add_message(Message::user("next segment"))
        .await
        .unwrap();
    assert!(
        per_user(d.path())
            .with_extension("segments")
            .join("000001.jsonl")
            .is_file()
    );
    let fresh = manager.load_full(&key()).await.unwrap();
    assert_eq!(fresh.messages.len(), 2);
    assert_eq!(fresh.title.as_deref(), Some("manual before roll"));
    assert!(fresh.title_manual);
}

async fn partial_corruption_remains_mutable(kind: &str) {
    let d = tempfile::tempdir().unwrap();
    only_per_user_copy(d.path()).await;
    let path = per_user(d.path());
    let original = std::fs::read(&path).unwrap();
    let split = original.iter().position(|b| *b == b'\n').unwrap() + 1;
    let flat = d.path().join("sessions").join(format!(
        "{}.jsonl",
        octos_bus::session::encode_path_component(&key().0)
    ));
    let body = match kind {
        "tail" => [original[split..].to_vec(), b"{\"role\":\"user\"".to_vec()].concat(),
        "middle" => [b"not a JSON row\n".to_vec(), original[split..].to_vec()].concat(),
        "bytes" => [original[split..].to_vec(), vec![0xff, 0xfe]].concat(),
        "flat" => {
            std::fs::create_dir_all(flat.parent().unwrap()).unwrap();
            std::fs::write(&flat, b"old garbage\n").unwrap();
            original[split..].to_vec()
        }
        _ => unreachable!(),
    };
    std::fs::write(&path, [&original[..split], &body].concat()).unwrap();
    let g = gateway(d.path()).await;
    assert_eq!(rename(&g, "kept").await, 204, "{kind}");
    let renamed = std::fs::read(&path).unwrap();
    let split = renamed.iter().position(|b| *b == b'\n').unwrap() + 1;
    assert_eq!(&renamed[split..], body);
    let meta: serde_json::Value = serde_json::from_slice(&renamed[..split]).unwrap();
    assert_eq!(meta["title"], "kept");
    if kind == "flat" {
        assert_eq!(std::fs::read(&flat).unwrap(), b"old garbage\n");
    }
    assert_eq!(delete(&g).await, 204);
    assert!(!path.exists());
    assert!(!flat.exists());
}

#[tokio::test]
async fn should_mutate_with_truncated_tail() {
    partial_corruption_remains_mutable("tail").await;
}
#[tokio::test]
async fn should_mutate_with_bad_middle_row() {
    partial_corruption_remains_mutable("middle").await;
}
#[tokio::test]
async fn should_mutate_with_non_utf8_tail() {
    partial_corruption_remains_mutable("bytes").await;
}
#[tokio::test]
async fn should_mutate_with_obsolete_flat_garbage() {
    partial_corruption_remains_mutable("flat").await;
}

#[tokio::test]
async fn should_mutate_canonical_gateway_key_without_alias_expansion() {
    let d = tempfile::tempdir().unwrap();
    only_per_user_copy(d.path()).await;
    let legacy = SessionKey::new("api", ID);
    SessionHandle::open(d.path(), &legacy)
        .add_message(Message::user("exact target"))
        .await
        .unwrap();
    let g = gateway(d.path()).await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let url = format!("http://127.0.0.1:{}/sessions/api%3A{ID}", g.port);
    assert_eq!(
        client
            .patch(format!("{url}/title"))
            .bearer_auth(TOKEN)
            .json(&serde_json::json!({"title":"exact"}))
            .send()
            .await
            .unwrap()
            .status(),
        204
    );
    let reader = SessionManager::open(d.path()).unwrap();
    assert_eq!(
        reader.load(&legacy).await.unwrap().title.as_deref(),
        Some("exact")
    );
    assert!(!reader.load(&key()).await.unwrap().title_manual);
    assert_eq!(
        client
            .delete(url)
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap()
            .status(),
        204
    );
    assert!(reader.load(&legacy).await.is_none());
    assert!(reader.load(&key()).await.is_some());
}

#[cfg(unix)]
#[tokio::test]
async fn should_mutate_internal_gateway_topic_exactly() {
    let d = tempfile::tempdir().unwrap();
    let base = key();
    let topic = SessionKey(format!("{}#child-1", base.0));
    let alias = SessionKey::with_profile("coding", "api", &topic.0);
    for target in [&base, &topic, &alias] {
        SessionHandle::open(d.path(), target)
            .add_message(Message::user("preserve exact addressing"))
            .await
            .unwrap();
    }
    let g = gateway(d.path()).await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let encoded = topic.0.replace(':', "%3A").replace('#', "%23");
    let url = format!("http://127.0.0.1:{}/sessions/{encoded}", g.port);
    let response = client
        .patch(format!("{url}/title"))
        .bearer_auth(TOKEN)
        .json(&serde_json::json!({"title":"internal topic"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 204);
    let reader = SessionManager::open(d.path()).unwrap();
    assert_eq!(
        reader.load(&topic).await.unwrap().title.as_deref(),
        Some("internal topic")
    );
    assert!(!reader.load(&base).await.unwrap().title_manual);
    assert!(!reader.load(&alias).await.unwrap().title_manual);
    assert_eq!(
        client
            .delete(&url)
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap()
            .status(),
        204
    );
    assert!(reader.load(&topic).await.is_none());
    assert!(reader.load(&base).await.is_some());
    assert!(reader.load(&alias).await.is_some());
}

#[tokio::test]
async fn deleted_session_rejects_late_writes_from_all_transcript_writers() {
    let mut violations = Vec::new();
    for writer in [
        "handle-append",
        "handle-rewrite",
        "manager-append",
        "manager-rewrite",
        "canonical",
        "title-create",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let session = key();
        let mut actor = SessionHandle::open(dir.path(), &session);
        actor
            .add_message(Message::user("must stay deleted"))
            .await
            .unwrap();
        let mut cached_manager = SessionManager::open(dir.path()).unwrap();
        cached_manager.get_or_create(&session).await;
        let mut deleter = SessionManager::open(dir.path()).unwrap();
        assert!(deleter.clear_existing(&session).await.unwrap());
        let msg = Message::assistant_with_thread(
            "late completion",
            octos_core::ThreadId::new("old-turn"),
        );
        let result = match writer {
            "handle-append" => actor.add_message(msg).await,
            "handle-rewrite" => actor.rewrite().await,
            "manager-append" => cached_manager.add_message(&session, msg).await,
            "manager-rewrite" => cached_manager.rewrite(&session).await,
            "title-create" => {
                cached_manager
                    .update_title(&session, "late title".into())
                    .await
            }
            _ => octos_bus::session::persist_message_through_canonical_path(
                dir.path(),
                &session,
                msg,
            )
            .await
            .map(|_| ()),
        };
        let resurrected = SessionHandle::session_exists(dir.path(), &session);
        eprintln!(
            "delete barrier writer={writer} rejected={} resurrected={resurrected}",
            result.is_err()
        );
        if result.is_ok() || resurrected {
            violations.push(writer);
        }
    }
    assert!(
        violations.is_empty(),
        "late writers resurrected a deleted session: {violations:?}"
    );
}

#[tokio::test]
async fn deletion_barrier_survives_reopen_and_scopes_store_and_topic() {
    let dir = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let deleted = SessionKey(format!("{}#deleted-topic", key()));
    let sibling = SessionKey(format!("{}#kept-topic", key()));
    let mut actor = SessionHandle::open(dir.path(), &deleted);
    actor.add_message(Message::user("old")).await.unwrap();
    assert!(
        SessionManager::open(dir.path())
            .unwrap()
            .clear_existing(&deleted)
            .await
            .unwrap()
    );
    drop(actor);
    let mut cold = SessionHandle::open(dir.path(), &deleted);
    assert!(
        cold.add_message(Message::assistant_with_thread(
            "late",
            octos_core::ThreadId::new("old")
        ))
        .await
        .is_err(),
        "cold writer must observe deletion"
    );
    SessionHandle::open(dir.path(), &sibling)
        .add_message(Message::user("new sibling"))
        .await
        .unwrap();
    SessionHandle::open(other.path(), &deleted)
        .add_message(Message::user("separate store"))
        .await
        .unwrap();
    assert!(!SessionHandle::session_exists(dir.path(), &deleted));
    assert!(SessionHandle::session_exists(dir.path(), &sibling));
    assert!(SessionHandle::session_exists(other.path(), &deleted));
}

#[tokio::test]
async fn fresh_user_can_reopen_without_admitting_old_deliveries() {
    let dir = tempfile::tempdir().unwrap();
    let key = key();
    let mut old = SessionHandle::open(dir.path(), &key);
    old.add_message(Message::user("old prompt")).await.unwrap();
    let old_thread = old.session().messages[0].thread_id.clone().unwrap();
    SessionManager::open(dir.path())
        .unwrap()
        .clear_existing(&key)
        .await
        .unwrap();
    let mut fresh = SessionHandle::open(dir.path(), &key);
    fresh
        .add_message(Message::user("new prompt"))
        .await
        .unwrap();
    let new_thread = fresh.session().messages[0].thread_id.clone().unwrap();
    for writer in ["old-handle", "old-delivery", "new-delivery"] {
        let msg = Message::assistant_with_thread(
            writer,
            if writer == "new-delivery" {
                octos_core::ThreadId::new(new_thread.clone())
            } else {
                octos_core::ThreadId::new(old_thread.clone())
            },
        );
        let result = if writer == "old-handle" {
            old.add_message(msg).await
        } else {
            octos_bus::session::persist_message_through_canonical_path(dir.path(), &key, msg)
                .await
                .map(|_| ())
        };
        assert_eq!(
            result.is_ok(),
            writer == "new-delivery",
            "{writer}: {result:?}"
        );
    }
    assert!(old.rewrite().await.is_err());
    let history = SessionHandle::open_full(dir.path(), &key);
    assert_eq!(
        history
            .session()
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>(),
        ["new prompt", "new-delivery"]
    );
}

#[tokio::test]
async fn interrupted_user_recovery_cannot_reopen_deleted_thread() {
    let dir = tempfile::tempdir().unwrap();
    let key = key();
    let mut original = SessionHandle::open(dir.path(), &key);
    original
        .add_message(Message::user("dispatched before deletion"))
        .await
        .unwrap();
    let old_thread = original.session().messages[0].thread_id.clone();
    SessionManager::open(dir.path())
        .unwrap()
        .clear_existing(&key)
        .await
        .unwrap();
    let mut recovery = Message::user("freshly constructed recovery of old prompt");
    recovery.thread_id = old_thread;
    assert!(
        SessionManager::open(dir.path())
            .unwrap()
            .add_message(&key, recovery)
            .await
            .is_err()
    );
    assert!(!SessionHandle::session_exists(dir.path(), &key));
    let mut new = SessionHandle::open(dir.path(), &key);
    new.add_message(Message::user("fresh user")).await.unwrap();
    assert!(
        original
            .persist_system_note_once(Message::system("late note"), "old-note")
            .await
            .is_err()
    );
    new.persist_system_note_once(Message::system("new note"), "new-note")
        .await
        .unwrap();
    assert!(
        octos_bus::session::persist_system_note_once_through_canonical_path(
            dir.path(),
            &key,
            Message::system("unowned late note"),
            "cold-note"
        )
        .await
        .is_err()
    );
    let reader = SessionHandle::open_full(dir.path(), &key);
    assert_eq!(reader.session().messages.len(), 2);
}

#[tokio::test]
async fn explicit_unthreaded_records_work_but_deleted_receipts_do_not_replay() {
    let dir = tempfile::tempdir().unwrap();
    let key = key();
    let mut store = SessionManager::open(dir.path()).unwrap();
    let mut record = Message::system("explicit record");
    record.source = Some("test".into());
    record.client_message_id = Some("old-record".into());
    store.add_message(&key, record.clone()).await.unwrap();
    store.clear_existing(&key).await.unwrap();
    assert!(store.add_message(&key, record).await.is_err());
    let mut new_record = Message::system("new explicit record");
    new_record.source = Some("test".into());
    store.add_message(&key, new_record).await.unwrap();
    assert_eq!(
        SessionHandle::open_full(dir.path(), &key)
            .session()
            .messages
            .len(),
        1
    );
}

#[tokio::test]
async fn independent_flat_writer_is_not_a_migration_leftover() {
    let dir = tempfile::tempdir().unwrap();
    let key = key();
    let mut actor = SessionHandle::open(dir.path(), &key);
    actor
        .add_message(Message::user("gateway prompt"))
        .await
        .unwrap();
    let thread = actor.session().messages[0].thread_id.clone().unwrap();
    let mut serve = SessionManager::open(dir.path()).unwrap();
    serve
        .add_message(&key, Message::user("independent serve prompt"))
        .await
        .unwrap();
    octos_bus::session::persist_message_through_canonical_path(
        dir.path(),
        &key,
        Message::assistant_with_thread("delivery", octos_core::ThreadId::new(thread)),
    )
    .await
    .unwrap();
    let reader = SessionManager::open(dir.path()).unwrap();
    let history = reader.load(&key).await.unwrap();
    assert!(
        history
            .messages
            .iter()
            .any(|m| m.content == "independent serve prompt"),
        "opening a canonical writer must not delete the independent flat writer's committed row"
    );
    assert!(
        history
            .messages
            .iter()
            .any(|m| m.content == "gateway prompt")
    );
    assert!(history.messages.iter().any(|m| m.content == "delivery"));
}
