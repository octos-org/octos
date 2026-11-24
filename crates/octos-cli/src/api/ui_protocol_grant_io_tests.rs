//! Grant-file storage latency must not stall unrelated admission work.
use super::*;

#[tokio::test(flavor = "current_thread")]
async fn slow_grant_read_does_not_hold_admission_lock_across_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let store = octos_agent::bridge::work_secret::WorkSecretGrantStore::new(dir.path());
    let slow_session = SessionKey("local:grant-slow".into());
    let fast_session = SessionKey("local:grant-fast".into());
    let slow_grant = store
        .issue(
            &slow_session.0,
            "slow-token",
            "http://127.0.0.1:0",
            chrono::Duration::minutes(5),
            None,
        )
        .unwrap();
    let fast_grant = store
        .issue(
            &fast_session.0,
            "fast-token",
            "http://127.0.0.1:0",
            chrono::Duration::minutes(5),
            None,
        )
        .unwrap();
    let body = std::fs::read_to_string(store.path()).unwrap();
    let writer = spawn_delayed_grant_snapshot(store.path(), body.clone(), body);

    let mut state = AppState::empty_for_tests();
    state.work_secret_store = Arc::new(store);
    let state = Arc::new(state);
    let contracts = Arc::new(UiProtocolContractStores::default());
    let ledger = Arc::new(UiProtocolLedger::new(16));
    let active = active_turns_registry();
    let started = std::time::Instant::now();
    let slow_scope = SessionIngressScope {
        session_id: slow_session,
        token: "slow-token".into(),
        grant_created_at: slow_grant.created_at,
    };
    let fast_scope = SessionIngressScope {
        session_id: fast_session,
        token: "fast-token".into(),
        grant_created_at: fast_grant.created_at,
    };
    let slow = bind_ingress_generation(
        &state,
        &contracts,
        &ledger,
        &active,
        &slow_scope,
        MAIN_PROFILE_ID,
        MAIN_PROFILE_ID,
    );
    let fast_path = state.work_secret_store.path().to_path_buf();
    let fast = async {
        while !fast_path.is_file() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let result = bind_ingress_generation(
            &state,
            &contracts,
            &ledger,
            &active,
            &fast_scope,
            MAIN_PROFILE_ID,
            MAIN_PROFILE_ID,
        )
        .await;
        (result, started.elapsed())
    };
    let (slow, (fast, fast_elapsed)) = tokio::join!(slow, fast);
    writer.join().unwrap();
    slow.expect("slow bind completes after its real delay");
    fast.expect("fast bind reads the replacement snapshot");
    assert!(
        fast_elapsed < Duration::from_secs(1),
        "unrelated admission waited for the slow grant read: {fast_elapsed:?}"
    );
    assert!(
        started.elapsed() >= Duration::from_millis(5300),
        "the slow request must not report an early success"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn stale_grant_snapshot_cannot_retire_replacement_generation() {
    let dir = tempfile::tempdir().unwrap();
    let store = octos_agent::bridge::work_secret::WorkSecretGrantStore::new(dir.path());
    let session = SessionKey("local:grant-revision-order".into());
    let old_grant = store
        .issue(
            &session.0,
            "old-token",
            "http://127.0.0.1:0",
            chrono::Duration::minutes(5),
            None,
        )
        .unwrap();
    let old_body = std::fs::read_to_string(store.path()).unwrap();
    let new_grant = store
        .issue(
            &session.0,
            "new-token",
            "http://127.0.0.1:0",
            chrono::Duration::minutes(5),
            None,
        )
        .unwrap();
    let new_body = std::fs::read_to_string(store.path()).unwrap();
    let writer = spawn_delayed_grant_snapshot(store.path(), old_body, new_body);

    let mut state = AppState::empty_for_tests();
    state.work_secret_store = Arc::new(store);
    let state = Arc::new(state);
    let contracts = Arc::new(UiProtocolContractStores::default());
    let ledger = Arc::new(UiProtocolLedger::new(16));
    let active = active_turns_registry();
    let old_scope = SessionIngressScope {
        session_id: session.clone(),
        token: "old-token".into(),
        grant_created_at: old_grant.created_at,
    };
    let new_scope = SessionIngressScope {
        session_id: session.clone(),
        token: "new-token".into(),
        grant_created_at: new_grant.created_at,
    };
    let started = std::time::Instant::now();
    let slow = async {
        let result = bind_ingress_generation(
            &state,
            &contracts,
            &ledger,
            &active,
            &old_scope,
            MAIN_PROFILE_ID,
            MAIN_PROFILE_ID,
        )
        .await;
        (result, started.elapsed())
    };
    let fast_path = state.work_secret_store.path().to_path_buf();
    let fast = async {
        while !fast_path.is_file() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let result = bind_ingress_generation(
            &state,
            &contracts,
            &ledger,
            &active,
            &new_scope,
            MAIN_PROFILE_ID,
            MAIN_PROFILE_ID,
        )
        .await;
        (result, started.elapsed())
    };
    let ((slow, slow_elapsed), (fast, fast_elapsed)) = tokio::join!(slow, fast);
    writer.join().unwrap();
    slow.expect("old snapshot remains valid at its own observation instant");
    let (replacement_generation, _lease) =
        fast.expect("replacement bind follows the older observation");
    assert!(slow_elapsed >= Duration::from_millis(5300));
    assert!(
        fast_elapsed >= slow_elapsed,
        "replacement observed before the older snapshot: slow={slow_elapsed:?}, fast={fast_elapsed:?}"
    );
    assert!(
        contracts
            .background_work
            .admit(
                &session,
                MAIN_PROFILE_ID,
                &TurnId::new(),
                Some(replacement_generation),
            )
            .is_some(),
        "a late old snapshot retired the replacement generation"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn revoke_after_snapshot_cannot_admit_revoked_generation() {
    let dir = tempfile::tempdir().unwrap();
    let store = octos_agent::bridge::work_secret::WorkSecretGrantStore::new(dir.path());
    let session = SessionKey("local:grant-admission-fence".into());
    let grant = store
        .issue(
            &session.0,
            "fence-token",
            "http://127.0.0.1:0",
            chrono::Duration::minutes(5),
            None,
        )
        .unwrap();
    let mut state = AppState::empty_for_tests();
    state.work_secret_store = Arc::new(store.clone());
    let state = Arc::new(state);
    let contracts = Arc::new(UiProtocolContractStores::default());
    let ledger = Arc::new(UiProtocolLedger::new(16));
    let active = active_turns_registry();
    let scope = SessionIngressScope {
        session_id: session.clone(),
        token: "fence-token".into(),
        grant_created_at: grant.created_at,
    };
    let (entered, release) = install_ingress_bind_admission_gate(&session);
    let bind = tokio::spawn({
        let state = state.clone();
        let contracts = contracts.clone();
        let ledger = ledger.clone();
        let active = active.clone();
        let scope = scope.clone();
        async move {
            bind_ingress_generation(
                &state,
                &contracts,
                &ledger,
                &active,
                &scope,
                MAIN_PROFILE_ID,
                MAIN_PROFILE_ID,
            )
            .await
        }
    });
    entered.acquire().await.unwrap().forget();
    let lock_probe = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(store.path().with_extension("lock"))
        .unwrap();
    let contention = fs2::FileExt::try_lock_exclusive(&lock_probe)
        .expect_err("the snapshot fence must cover the post-read admission gate");
    assert_eq!(contention.kind(), fs2::lock_contended_error().kind());
    release.add_permits(1);
    let bound = bind.await.unwrap();
    let (_generation, lease) =
        bound.expect("shared snapshot fence serializes admission before revoke");
    drop(lease);
    // Test-only timing, not a production gap: the fence is dropped
    // synchronously in the bind future, but task-frame teardown (closing
    // the lock file) can lag the JoinHandle on a current-thread runtime —
    // intermittent WouldBlock on ARM64 (#3326). Poll with a deadline.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while let Err(error) = fs2::FileExt::try_lock_exclusive(&lock_probe) {
        assert!(
            std::time::Instant::now() < deadline,
            "bind must release its snapshot fence after admission: {error}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    fs2::FileExt::unlock(&lock_probe).unwrap();
    let revoke_store = octos_agent::bridge::work_secret::WorkSecretGrantStore::new(dir.path());
    assert!(revoke_store.revoke_token("fence-token").unwrap());
    assert!(matches!(
        store.validate(&session.0, "fence-token"),
        Err(octos_agent::bridge::work_secret::WorkSecretValidationError::Revoked)
    ));
}

fn spawn_delayed_grant_snapshot(
    path: &Path,
    delayed_body: String,
    normal_body: String,
) -> std::thread::JoinHandle<()> {
    use std::io::Write;

    std::fs::remove_file(path).unwrap();
    assert!(
        std::process::Command::new("mkfifo")
            .arg(path)
            .status()
            .unwrap()
            .success()
    );
    let path = path.to_path_buf();
    std::thread::spawn(move || {
        let mut fifo = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        let normal = path.with_extension("json.normal");
        std::fs::write(&normal, &normal_body).unwrap();
        std::fs::rename(&normal, &path).unwrap();
        std::thread::sleep(Duration::from_millis(5300));
        fifo.write_all(delayed_body.as_bytes()).unwrap();
    })
}
