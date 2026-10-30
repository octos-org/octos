use super::*;
use std::sync::Barrier;

fn record(text: &str) -> Record {
    Record::new(
        "doc:race",
        RecordKind::Document,
        "test",
        Utc::now(),
        text,
        text,
    )
}

fn assert_rebuild_does_not_drop_upsert(updating: bool) {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(RecallStore::open(root.path(), RecallConfig::default()).unwrap());
    if updating {
        store
            .upsert(vec![record("oldkeyword")], vec![None])
            .unwrap();
    }
    let barrier = Arc::new(Barrier::new(2));
    let worker_store = store.clone();
    let worker_barrier = barrier.clone();
    let worker = std::thread::spawn(move || {
        worker_store
            .try_rebuild_after_snapshot(|| {
                worker_barrier.wait();
                worker_barrier.wait();
            })
            .unwrap()
    });
    barrier.wait(); // The old record/vector snapshot is already captured.
    let result = store.upsert(vec![record("newkeyword")], vec![None]);
    barrier.wait(); // Always release the worker, even when the write failed.
    let installed = worker.join().unwrap();
    result.unwrap();
    assert!(
        !installed,
        "a BM25-only mutation must invalidate the rebuild snapshot"
    );
    assert!(store.get("doc:race").unwrap().is_some());
    let filter = SearchFilter {
        limit: 10,
        ..Default::default()
    };
    assert_eq!(store.search("newkeyword", None, &filter).unwrap().len(), 1);
    assert!(
        store
            .search("oldkeyword", None, &filter)
            .unwrap()
            .is_empty()
    );
    store.rebuild().unwrap();
    assert_eq!(store.search("newkeyword", None, &filter).unwrap().len(), 1);
}

#[test]
fn should_preserve_bm25_insert_racing_with_rebuild() {
    assert_rebuild_does_not_drop_upsert(false);
}

#[test]
fn should_preserve_bm25_text_update_racing_with_rebuild() {
    assert_rebuild_does_not_drop_upsert(true);
}
