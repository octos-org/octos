//! Record-only retries must also see live rows outside the resident window.
use super::*;

#[tokio::test]
async fn append_retry_beyond_the_loaded_window_returns_original_receipt() {
    let dir = tempfile::tempdir().unwrap();
    let key = SessionKey::new("api", "app-replay-window");
    let mut first = SessionManager::open(dir.path()).unwrap();
    let mut original = Message::user("Original learner question");
    original.client_message_id = Some("app-question-1".into());
    assert_eq!(
        first
            .add_message_once_with_seq(&key, original.clone())
            .await
            .unwrap(),
        (0, true)
    );
    let mut filler = Message::user("x".repeat(SESSION_LOAD_BUDGET_DEFAULT as usize + 1024));
    filler.client_message_id = Some("filler".into());
    first.add_message_with_seq(&key, filler).await.unwrap();
    first
        .add_message_with_seq(&key, Message::user("new active segment"))
        .await
        .unwrap();
    drop(first);

    let mut reopened = SessionManager::open(dir.path()).unwrap();
    let window = reopened
        .load_from_disk_with_budget(&key, 64 * 1024)
        .await
        .unwrap();
    assert!(window.is_partial());
    assert_eq!(window.base_seq, 2);
    assert!(
        window
            .messages
            .iter()
            .all(|m| m.client_message_id.as_deref() != Some("app-question-1"))
    );
    reopened.cache.put(key.0.clone(), window);

    // The lost reply is retried after reopening with a bounded history load.
    // The retry must find the old row without replacing the bounded cache.
    assert_eq!(
        reopened
            .add_message_once_with_receipt(&key, original)
            .await
            .unwrap(),
        (0, false, Some("app-question-1".into()))
    );
    assert!(reopened.cache.peek(&key.0).unwrap().is_partial());
    let full = reopened.load_full(&key).await.unwrap();
    assert_eq!(
        full.messages
            .iter()
            .filter(|m| m.client_message_id.as_deref() == Some("app-question-1"))
            .count(),
        1
    );
}

#[tokio::test]
async fn receipt_preserves_original_binding_even_when_retry_changes_role() {
    let dir = tempfile::tempdir().unwrap();
    let key = SessionKey::new("api", "receipt-binding");
    let mut manager = SessionManager::open(dir.path()).unwrap();
    let mut note = Message::system("Original unbound system record");
    note.client_message_id = Some("shared-producer-id".into());
    assert_eq!(
        manager
            .add_message_once_with_receipt(&key, note)
            .await
            .unwrap(),
        (0, true, None)
    );
    let mut retry = Message::user("Different producer retrying the same id");
    retry.client_message_id = Some("shared-producer-id".into());
    assert_eq!(
        manager
            .add_message_once_with_receipt(&key, retry)
            .await
            .unwrap(),
        (0, false, None)
    );
    assert_eq!(manager.load_full(&key).await.unwrap().messages.len(), 1);
}

#[tokio::test]
async fn record_retry_observes_other_canonical_producers_and_never_uses_a_stale_seq() {
    let dir = tempfile::tempdir().unwrap();
    let key = SessionKey::new("api", "receipt-producers");
    let mut manager = SessionManager::open(dir.path()).unwrap();
    manager
        .add_message(&key, Message::user("Warm the manager cache"))
        .await
        .unwrap();
    let mut external = Message::user("Written by another canonical producer");
    external.client_message_id = Some("external-cmid".into());
    assert_eq!(
        persist_message_through_canonical_path(dir.path(), &key, external.clone())
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        manager
            .add_message_once_with_seq(&key, external)
            .await
            .unwrap(),
        (1, false)
    );
    let mut fresh = Message::user("A new record after that external commit");
    fresh.client_message_id = Some("new-cmid".into());
    assert_eq!(
        manager
            .add_message_once_with_seq(&key, fresh)
            .await
            .unwrap(),
        (2, true)
    );
    assert_eq!(manager.load_full(&key).await.unwrap().messages.len(), 3);
}

#[tokio::test]
async fn independent_managers_serializing_the_same_record_commit_once() {
    let dir = tempfile::tempdir().unwrap();
    let key = SessionKey::new("api", "receipt-concurrent");
    let mut one = SessionManager::open(dir.path()).unwrap();
    let mut two = SessionManager::open(dir.path()).unwrap();
    // Both mirrors exist before either writer commits.
    one.get_or_create(&key).await;
    two.get_or_create(&key).await;
    let mut message = Message::user("The same externally observed question");
    message.client_message_id = Some("same-id".into());
    let (first, second) = tokio::join!(
        one.add_message_once_with_seq(&key, message.clone()),
        two.add_message_once_with_seq(&key, message)
    );
    let outcomes = [first.unwrap(), second.unwrap()];
    assert_eq!(outcomes.iter().filter(|(_, appended)| *appended).count(), 1);
    assert!(outcomes.iter().all(|(seq, _)| *seq == 0));
    assert_eq!(one.load_full(&key).await.unwrap().messages.len(), 1);
}
