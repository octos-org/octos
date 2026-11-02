use super::*;
use std::sync::{Arc, Mutex};

#[tokio::test]
async fn independent_handles_keep_global_sequences_across_repeated_rollover() {
    let root = tempfile::tempdir().unwrap();
    let key = SessionKey::new("api", "independent-rollover");
    let observed = Arc::new(Mutex::new(Vec::new()));
    let sink = observed.clone();
    let observer: MessageCommitObserver = Arc::new(move |_, _, seq| {
        sink.lock().unwrap().push(seq);
    });
    set_scoped_message_commit_observer(root.path(), &observer);
    // Open both before either writes, as actors and canonical writers do.
    let mut a = SessionHandle::open(root.path(), &key);
    let mut b = SessionHandle::open(root.path(), &key);
    let large = || Message::user("x".repeat(session_segment_bytes() as usize + 1024));
    assert_eq!(b.add_message_with_seq(large()).await.unwrap(), 0);
    assert_eq!(
        a.add_message_with_seq(Message::user("first roll"))
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        b.add_message_with_seq(Message::user("interleaved"))
            .await
            .unwrap(),
        2
    );
    assert_eq!(b.add_message_with_seq(large()).await.unwrap(), 3);
    assert_eq!(
        a.add_message_with_seq(Message::user("second roll"))
            .await
            .unwrap(),
        4
    );
    assert_eq!(*observed.lock().unwrap(), vec![0, 1, 2, 3, 4]);

    let full = SessionHandle::open_full(root.path(), &key);
    assert_eq!(full.get_history(10).len(), 5);
    assert_eq!(full.session().next_seq(), 5);
    let window = SessionHandle::open_with_budget(root.path(), &key, 64 * 1024);
    assert_eq!(window.get_history(10).len(), 1);
    assert!(window.session().is_partial());
    assert_eq!(window.session().next_seq(), 5);
    assert_eq!(read_session_meta(&a.session_path()).unwrap().base_seq, 4);
}
