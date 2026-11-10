//! Session mutation regressions through the real persistence paths.
#![cfg(feature = "api")]
use std::path::{Path, PathBuf};

use octos_bus::{SessionHandle, SessionManager};
use octos_core::{Message, SessionKey};

const ID: &str = "web-legacy";

fn key() -> SessionKey {
    SessionKey::with_profile("_main", "api", ID)
}

fn per_user(dir: &Path) -> PathBuf {
    let base = octos_bus::session::encode_path_component(key().base_key());
    dir.join("users")
        .join(base)
        .join("sessions")
        .join("default.jsonl")
}
async fn only_per_user_copy(dir: &Path) {
    let mut h = SessionHandle::open(dir, &key());
    h.add_message(Message::user("the only copy")).await.unwrap();
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
