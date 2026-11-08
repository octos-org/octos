// T-0423: memory/search must keep returning ingested records after a
// session reopen, exactly as memory/load (by id) does. These tests pin the
// behaviour at the runtime-resolution seam (cold bootstrap vs cached), with
// no model and no shared instance.


fn memory_reopen_profile(tmp: &std::path::Path) -> crate::profiles::UserProfile {
    crate::profiles::UserProfile {
        id: "coding".to_string(),
        name: "Coding".to_string(),
        enabled: true,
        data_dir: Some(tmp.join("data").to_string_lossy().into_owned()),
        parent_id: None,
        public_subdomain: None,
        config: crate::profiles::ProfileConfig {
            llm: Some(crate::profiles::LlmProfileConfig {
                primary: Some(crate::profiles::LlmModelSelectionConfig {
                    family_id: Some("openai".to_string()),
                    model_id: Some("gpt-4o-mini".to_string()),
                    route: Some(crate::profiles::LlmRouteConfig {
                        route_id: None,
                        label: None,
                        base_url: None,
                        api_key_env: Some("T0423_TEST_KEY".to_string()),
                        api_type: None,
                    }),
                    ..Default::default()
                }),
                fallbacks: Vec::new(),
            }),
            env_vars: [("T0423_TEST_KEY".to_string(), "test-key".to_string())]
                .into_iter()
                .collect(),
            ..Default::default()
        },
        created_at: Utc::now(),
        updated_at: Utc::now(),
    }
}

async fn ingest_marker(runtime: &Arc<crate::runtime::ProfileRuntime>, marker: &str) -> String {
    let mut record = octos_memory::Record::new(
        format!("mem-{marker}"),
        octos_memory::RecordKind::Document,
        "t0423",
        Utc::now(),
        marker,
        format!("abstract {marker}"),
    );
    record.body = Some(format!("content {marker}"));
    runtime.recall.upsert(vec![record], Vec::new()).expect("ingest");
    runtime.recall.persist_index().expect("persist index");
    format!("mem-{marker}")
}

/// A cold re-bootstrap while the previous runtime is still alive must not
/// produce a store whose records are readable by id but invisible to search.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory_search_matches_load_after_runtime_rebootstrap() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(crate::profiles::ProfileStore::open_unified(tmp.path()).unwrap());
    store.save(&memory_reopen_profile(tmp.path())).unwrap();
    let mut state = AppState::empty_for_tests();
    state.profile_store = Some(store);
    let state = Arc::new(state);

    // First cold bootstrap + ingest + search hits.
    let first = ensure_session_profile_runtime(&state, Some("coding"))
        .await
        .unwrap()
        .expect("runtime bootstraps");
    let id = ingest_marker(&first, "reopen-marker").await;
    let filter = octos_memory::SearchFilter { kinds: vec![], sources: vec![], since: None, until: None, limit: 20 };
    let hits = first.recall.search("reopen-marker", None, &filter).unwrap();
    assert!(!hits.is_empty(), "fresh ingest must be searchable");

    // Reopen through the SAME resolver: the cached runtime is returned, so
    // search keeps working (this is the plain page-reload path).
    let cached = ensure_session_profile_runtime(&state, Some("coding"))
        .await
        .unwrap()
        .expect("cached runtime");
    assert!(Arc::ptr_eq(&first, &cached), "reopen must reuse the runtime");
    assert!(!cached.recall.search("reopen-marker", None, &filter).unwrap().is_empty());

    // Production runtime replacement: the previous runtime is retired and a
    // new one bootstrapped over the same data dir. When the storage shape is
    // unchanged the new runtime SHARES the retired store; otherwise it opens
    // fresh (possibly the degraded in-memory fallback). In every case id-load
    // and search must AGREE — the reported "load works, search empty"
    // divergence must never appear.
    let mut retired = Some(first.retire());
    let rebased = crate::runtime::ProfileRuntime::bootstrap_replacing(
        &memory_reopen_profile(tmp.path()),
        &tmp.path().join("data"),
        None,
        crate::runtime::BootstrapRole::Serve,
        None,
        None,
        None,
        &mut retired,
    )
    .await
    .expect("second bootstrap");
    let by_id = rebased.recall.get(&id).unwrap();
    let hits = rebased.recall.search("reopen-marker", None, &filter).unwrap();
    assert_eq!(
        by_id.is_some(),
        !hits.is_empty(),
        "id-load and search must agree after re-bootstrap (degraded={}): by_id={}, hits={}",
        rebased.recall.is_degraded(),
        by_id.is_some(),
        hits.len()
    );
}

/// A fresh on-disk open after the previous store is dropped must rebuild the
/// search index from the persisted records (not just the id lookup).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory_search_rebuilds_index_from_persistent_records() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let config = octos_memory::RecallConfig::default();
    let id = {
        let store = octos_memory::RecallStore::open(&data_dir, config.clone()).unwrap();
        let mut record = octos_memory::Record::new(
            "mem-persist",
            octos_memory::RecordKind::Document,
            "t0423",
            Utc::now(),
            "persist",
            "abstract persist",
        );
        record.body = Some("content persist".to_string());
        store.upsert(vec![record], Vec::new()).unwrap();
        store.persist_index().unwrap();
        assert!(!store.search("persist", None, &octos_memory::SearchFilter { kinds: vec![], sources: vec![], since: None, until: None, limit: 20 }).unwrap().is_empty());
        "mem-persist".to_string()
        // store dropped here: the redb lock is released.
    };
    let reopened = octos_memory::RecallStore::open(&data_dir, config).unwrap();
    assert!(reopened.get(&id).unwrap().is_some(), "id-load must survive reopen");
    assert!(
        !reopened.search("persist", None, &octos_memory::SearchFilter { kinds: vec![], sources: vec![], since: None, until: None, limit: 20 }).unwrap().is_empty(),
        "search must rebuild from persisted records after reopen"
    );
}

/// Reopening with a DIFFERENT (or not-yet-ready) embedder must not empty the
/// search index: vectors from another embedder are ignored, but every record
/// stays searchable BM25-only and readable by id.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory_search_survives_embedder_change_on_reopen() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let config_a = octos_memory::RecallConfig {
        embedder_id: "provider/model-a".to_string(),
        ..Default::default()
    };
    let dim = config_a.dimension;
    {
        let store = octos_memory::RecallStore::open(&data_dir, config_a).unwrap();
        let mut record = octos_memory::Record::new(
            "mem-embed",
            octos_memory::RecordKind::Document,
            "t0423",
            Utc::now(),
            "embed",
            "abstract embed",
        );
        record.body = Some("content embed".to_string());
        store.upsert(vec![record], vec![Some(vec![0.1; dim])]).unwrap();
        store.persist_index().unwrap();
    }
    // Reopen with a different embedder id (e.g. a deferred model that has not
    // resolved to its real id yet): the stored vector is foreign.
    let config_b = octos_memory::RecallConfig {
        embedder_id: "provider/model-b".to_string(),
        ..Default::default()
    };
    let reopened = octos_memory::RecallStore::open(&data_dir, config_b).unwrap();
    let filter = octos_memory::SearchFilter {
        kinds: vec![], sources: vec![], since: None, until: None, limit: 20,
    };
    assert!(reopened.get("mem-embed").unwrap().is_some(), "id-load must survive");
    assert!(
        !reopened.search("embed", None, &filter).unwrap().is_empty(),
        "search must still find the record BM25-only after an embedder change"
    );
}

/// A record without a stored vector (e.g. its embedding backfill was skipped)
/// must still be returned by a HYBRID search that supplies a query vector:
/// the BM25 component covers vector-less records while the backfill retries.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory_search_returns_vectorless_records_with_query_vector() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let config = octos_memory::RecallConfig {
        embedder_id: "provider/model-a".to_string(),
        ..Default::default()
    };
    let dim = config.dimension;
    let store = octos_memory::RecallStore::open(&data_dir, config).unwrap();
    let mut record = octos_memory::Record::new(
        "mem-novec",
        octos_memory::RecordKind::Document,
        "t0423",
        Utc::now(),
        "novec",
        "abstract novec",
    );
    record.body = Some("content novec".to_string());
    // No vector stored for this record (as when its backfill is deferred).
    store.upsert(vec![record], Vec::new()).unwrap();
    let filter = octos_memory::SearchFilter {
        kinds: vec![], sources: vec![], since: None, until: None, limit: 20,
    };
    let hits = store.search("novec", Some(&vec![0.1; dim]), &filter).unwrap();
    assert!(
        hits.iter().any(|hit| hit.id == "mem-novec"),
        "a query vector must not hide vector-less records: {hits:?}"
    );
}
