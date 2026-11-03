// Restart faults exercise the real hydrate handler, including its seq-zero path.
fn hydrate_recovery_ledger(path: &std::path::Path) -> Arc<UiProtocolLedger> {
    let mut config = LedgerConfig::durable(path.into());
    config.snapshot_every_events = 0;
    UiProtocolLedger::recover(config).ledger
}

async fn hydrate_recovery_history() -> (tempfile::TempDir, Arc<AppState>, SessionKey) {
    let (temp, state, ledger, session, _) = hydrate_budget_fixture(4096, 3, 8).await;
    for _ in 0..4200 {
        ledger.append_progress(UiProgressEvent::new(
            session.clone(),
            None,
            UiProgressMetadata::new("status"),
        ));
    }
    drop(ledger);
    (temp, state, session)
}

#[tokio::test]
async fn hydrate_recovery_truncates_only_unterminated_latest_tail_before_append() {
    use std::io::Write;
    for tail in [b"{\"v\":1,".to_vec(), vec![b'x'; 20_000], b"\xff".to_vec()] {
        let (temp, state, session) = hydrate_recovery_history().await;
        let path = hydrate_budget_log(temp.path());
        let complete = std::fs::read(&path).unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(&tail)
            .unwrap();
        let ledger = hydrate_recovery_ledger(temp.path());
        let frame = hydrate_budget_frame(&state, &ledger, &session, None).await;
        assert_eq!(
            frame["result"]["file_mutations"].as_array().map(Vec::len),
            Some(3),
            "{frame}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), complete);
        ledger.append_progress(UiProgressEvent::new(
            session.clone(),
            None,
            UiProgressMetadata::file_mutation(UiFileMutationNotice::new("after-repair", "modify")),
        ));
        let frame = hydrate_budget_frame(&state, &ledger, &session, None).await;
        assert_eq!(
            frame["result"]["file_mutations"].as_array().map(Vec::len),
            Some(4),
            "{frame}"
        );
        drop(ledger);
        let ledger = hydrate_recovery_ledger(temp.path());
        let frame = hydrate_budget_frame(&state, &ledger, &session, None).await;
        assert_eq!(
            frame["result"]["file_mutations"].as_array().map(Vec::len),
            Some(4),
            "{frame}"
        );
    }
}

#[tokio::test]
async fn hydrate_recovery_does_not_repair_middle_or_sealed_corruption() {
    use std::io::Write;
    for sealed in [false, true] {
        let (temp, state, session) = hydrate_recovery_history().await;
        let path = hydrate_budget_log(temp.path());
        let bad = if sealed {
            b"{unfinished".as_slice()
        } else {
            b"{unfinished\n".as_slice()
        };
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(bad)
            .unwrap();
        if sealed {
            let last = std::fs::read(&path)
                .unwrap()
                .split(|b| *b == b'\n')
                .find(|line| !line.is_empty())
                .unwrap()
                .to_vec();
            let mut next = last;
            next.push(b'\n');
            std::fs::write(path.parent().unwrap().join("ledger-zzzz.log"), next).unwrap();
        }
        let before = std::fs::read(&path).unwrap();
        let ledger = hydrate_recovery_ledger(temp.path());
        let frame = hydrate_budget_frame(&state, &ledger, &session, None).await;
        assert_hydrate_budget_error(&frame, "invalid_record");
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn hydrate_recovery_read_failure_retries_without_restart() {
    use std::os::unix::fs::PermissionsExt;
    let (temp, state, session) = hydrate_recovery_history().await;
    let path = hydrate_budget_log(temp.path());
    let original = std::fs::read(&path).unwrap();
    let ledger = hydrate_recovery_ledger(temp.path());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o0)).unwrap();
    for _ in 0..2 {
        let frame = hydrate_budget_frame(&state, &ledger, &session, None).await;
        assert_hydrate_budget_error(&frame, "read_failed");
        assert_eq!(frame["error"]["data"]["retryable"], true);
    }
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let frame = hydrate_budget_frame(&state, &ledger, &session, None).await;
    assert_eq!(
        frame["result"]["file_mutations"].as_array().map(Vec::len),
        Some(3),
        "{frame}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), original);
    assert!(!ledger.index_failed_for_test(&session));
}

#[cfg(unix)]
#[tokio::test]
async fn hydrate_recovery_failed_tail_truncation_is_retryable() {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    let (temp, state, session) = hydrate_recovery_history().await;
    let path = hydrate_budget_log(temp.path());
    let complete = std::fs::read(&path).unwrap();
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"{torn")
        .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();
    let ledger = hydrate_recovery_ledger(temp.path());
    let frame = hydrate_budget_frame(&state, &ledger, &session, None).await;
    assert_hydrate_budget_error(&frame, "read_failed");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let frame = hydrate_budget_frame(&state, &ledger, &session, None).await;
    assert_eq!(
        frame["result"]["file_mutations"].as_array().map(Vec::len),
        Some(3),
        "{frame}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), complete);
}

#[cfg(unix)]
#[tokio::test]
async fn hydrate_live_recovery_directory_errors_retry_after_boot() {
    use std::os::unix::fs::PermissionsExt;
    for root in [false, true] {
        for mode in [0o000, 0o644] {
            let (temp, state, ledger, session, _) = hydrate_budget_fixture(4096, 3, 8).await;
            drop(ledger);
            let log = hydrate_budget_log(temp.path());
            let dir = if root {
                log.parent().unwrap().parent().unwrap()
            } else {
                log.parent().unwrap()
            };
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode)).unwrap();
            let ledger = hydrate_recovery_ledger(temp.path());
            let failed = hydrate_budget_frame(&state, &ledger, &session, None).await;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert_hydrate_budget_error(&failed, "read_failed");
            let restored = hydrate_budget_frame(&state, &ledger, &session, None).await;
            assert_eq!(
                restored["result"]["file_mutations"]
                    .as_array()
                    .map(Vec::len),
                Some(3),
                "{restored}"
            );
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn hydrate_live_recovery_tail_repaired_before_turn_without_hydrate() {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    let (temp, state, session) = hydrate_recovery_history().await;
    let path = hydrate_budget_log(temp.path());
    let original = std::fs::read(&path).unwrap();
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"{torn")
        .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();
    let ledger = hydrate_recovery_ledger(temp.path());
    let failed = hydrate_budget_frame(&state, &ledger, &session, None).await;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_hydrate_budget_error(&failed, "read_failed");
    ledger.append_progress(UiProgressEvent::new(
        session.clone(),
        None,
        UiProgressMetadata::file_mutation(UiFileMutationNotice::new("after-repair", "modify")),
    ));
    for current in [ledger, hydrate_recovery_ledger(temp.path())] {
        let frame = hydrate_budget_frame(&state, &current, &session, None).await;
        assert_eq!(
            frame["result"]["file_mutations"].as_array().map(Vec::len),
            Some(4),
            "{frame}"
        );
    }
    let bytes = std::fs::read(&path).unwrap();
    assert!(bytes.starts_with(&original));
    for line in bytes.split(|b| *b == b'\n').filter(|line| !line.is_empty()) {
        serde_json::from_slice::<serde_json::Value>(line).unwrap();
    }
}

#[cfg(unix)]
#[tokio::test]
async fn hydrate_live_recovery_merges_older_disk_with_fault_period_ram() {
    use std::os::unix::fs::PermissionsExt;
    for during in [1, 12] {
        let (temp, state, ledger, session, _) = hydrate_budget_fixture(4096, 3, 8).await;
        drop(ledger);
        let log = hydrate_budget_log(temp.path());
        let dir = log.parent().unwrap();
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o000)).unwrap();
        let ledger = hydrate_recovery_ledger(temp.path());
        let failed = hydrate_budget_frame(&state, &ledger, &session, None).await;
        for i in 0..during {
            ledger.append_progress(UiProgressEvent::new(
                session.clone(),
                None,
                UiProgressMetadata::file_mutation(UiFileMutationNotice::new(
                    format!("ram-{i}"),
                    "modify",
                )),
            ));
        }
        let still_failed = hydrate_budget_frame(&state, &ledger, &session, None).await;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_hydrate_budget_error(&failed, "read_failed");
        assert_hydrate_budget_error(&still_failed, "read_failed");
        for _ in 0..2 {
            let frame = hydrate_budget_frame(&state, &ledger, &session, None).await;
            assert_eq!(
                frame["result"]["file_mutations"].as_array().map(Vec::len),
                Some(3 + during),
                "{frame}"
            );
        }
        ledger.append_progress(UiProgressEvent::new(
            session.clone(),
            None,
            UiProgressMetadata::file_mutation(UiFileMutationNotice::new(
                "after-recovery",
                "modify",
            )),
        ));
        let frame = hydrate_budget_frame(&state, &ledger, &session, None).await;
        assert_eq!(
            frame["result"]["file_mutations"].as_array().map(Vec::len),
            Some(4 + during),
            "{frame}"
        );
        // Failed writes are still explicitly RAM-only; old durable history must survive restart.
        let ledger = hydrate_recovery_ledger(temp.path());
        let frame = hydrate_budget_frame(&state, &ledger, &session, None).await;
        assert_eq!(
            frame["result"]["file_mutations"].as_array().map(Vec::len),
            Some(4),
            "{frame}"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn hydrate_live_recovery_evicted_post_boot_session_read_error() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let state = state_with_sessions(temp.path());
    let mut config = LedgerConfig::durable(temp.path().into());
    config.active_session_cap = 1;
    config.snapshot_every_events = 0;
    let ledger = Arc::new(UiProtocolLedger::with_config(config));
    let a = SessionKey("local:evicted-a".into());
    state
        .sessions
        .as_ref()
        .unwrap()
        .lock()
        .await
        .get_or_create(&a)
        .await;
    for i in 0..3 {
        ledger.append_progress(UiProgressEvent::new(
            a.clone(),
            None,
            UiProgressMetadata::file_mutation(UiFileMutationNotice::new(format!("f{i}"), "modify")),
        ));
    }
    let log = hydrate_budget_log(temp.path());
    ledger.append_progress(UiProgressEvent::new(
        SessionKey("local:evicted-b".into()),
        None,
        UiProgressMetadata::new("status"),
    ));
    std::fs::set_permissions(&log, std::fs::Permissions::from_mode(0o000)).unwrap();
    let failed = hydrate_budget_frame(&state, &ledger, &a, None).await;
    std::fs::set_permissions(&log, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_hydrate_budget_error(&failed, "read_failed");
    let frame = hydrate_budget_frame(&state, &ledger, &a, None).await;
    assert_eq!(
        frame["result"]["file_mutations"].as_array().map(Vec::len),
        Some(3),
        "{frame}"
    );
}

#[tokio::test]
async fn hydrate_live_recovery_small_corrupt_history_is_not_complete_ram() {
    for (replace, bad) in [
        (false, b"{bad\n".as_slice()),
        (false, b"\xff\n".as_slice()),
        (true, b"{bad\n".as_slice()),
    ] {
        let (temp, state, ledger, session, _) = hydrate_budget_fixture(4096, 3, 8).await;
        drop(ledger);
        let log = hydrate_budget_log(temp.path());
        let mut bytes = if replace {
            Vec::new()
        } else {
            std::fs::read(&log).unwrap()
        };
        bytes.extend_from_slice(bad);
        std::fs::write(&log, &bytes).unwrap();
        let ledger = hydrate_recovery_ledger(temp.path());
        let frame = hydrate_budget_frame(&state, &ledger, &session, None).await;
        assert_hydrate_budget_error(&frame, "invalid_record");
        assert_eq!(std::fs::read(log).unwrap(), bytes);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn hydrate_live_recovery_unreadable_other_identity_is_isolated() {
    use std::os::unix::fs::PermissionsExt;
    let (temp, state, ledger, session, _) = hydrate_budget_fixture(4096, 3, 8).await;
    let other = SessionKey(format!("local:{}", "unreadable".repeat(20)));
    ledger.append_progress(UiProgressEvent::new(
        other.clone(),
        None,
        UiProgressMetadata::new("status"),
    ));
    state
        .sessions
        .as_ref()
        .unwrap()
        .lock()
        .await
        .get_or_create(&other)
        .await;
    drop(ledger);
    let dir = std::fs::read_dir(temp.path().join("ui-protocol"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("sha256-")
        })
        .unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000)).unwrap();
    let ledger = hydrate_recovery_ledger(temp.path());
    let good = hydrate_budget_frame(&state, &ledger, &session, None).await;
    let failed = hydrate_budget_frame(&state, &ledger, &other, None).await;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        good["result"]["file_mutations"].as_array().map(Vec::len),
        Some(3),
        "{good}"
    );
    assert_hydrate_budget_error(&failed, "read_failed");
    let repaired = hydrate_budget_frame(&state, &ledger, &other, None).await;
    assert!(repaired.get("result").is_some(), "{repaired}");
}
