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
