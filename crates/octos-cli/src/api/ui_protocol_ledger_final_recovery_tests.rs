use super::*;

fn mutation(ledger: &UiProtocolLedger, key: &SessionKey, path: &str) -> LedgeredUiProtocolEvent {
    ledger.append_progress(UiProgressEvent::new(
        key.clone(),
        None,
        octos_core::ui_protocol::UiProgressMetadata::file_mutation(UiFileMutationNotice::new(
            path, "modify",
        )),
    ))
}
fn config(root: &Path) -> LedgerConfig {
    let mut config = LedgerConfig::durable(root.into());
    config.retained_per_session = 3;
    config.snapshot_every_events = 0;
    config
}
fn assert_paths(ledger: &UiProtocolLedger, key: &SessionKey, count: usize) {
    let (_, head) = ledger.snapshot_with_cursor(key, None).unwrap();
    assert_eq!(
        ledger.retained_file_mutations(key, &head).unwrap().len(),
        count
    );
}
#[test]
fn ledger_final_recovery_full_ring_keeps_failed_cold_writes() {
    let temp = tempfile::tempdir().unwrap();
    let key = SessionKey("local:final-cold".into());
    let cfg = config(temp.path());
    let ledger = UiProtocolLedger::with_config(cfg.clone());
    for i in 0..9 {
        mutation(&ledger, &key, &format!("disk-{i}"));
    }
    drop(ledger);
    let root = temp.path().join("ui-protocol");
    let saved = temp.path().join("saved");
    fs::rename(&root, &saved).unwrap();
    fs::write(&root, b"blocked").unwrap();
    let ledger = UiProtocolLedger::recover(cfg).ledger;
    assert!(ledger.snapshot_with_cursor(&key, None).is_err());
    let mut expected_times = Vec::new();
    for i in 0..6 {
        expected_times.push(mutation(&ledger, &key, &format!("ram-{i}")));
    }
    fs::remove_file(&root).unwrap();
    fs::rename(&saved, &root).unwrap();
    assert_paths(&ledger, &key, 15);
    mutation(&ledger, &key, "after");
    assert_paths(&ledger, &key, 16);
    let (_, head) = ledger.snapshot_with_cursor(&key, None).unwrap();
    let refs = ledger
        .retained_hydrate_references(&key, &head, true)
        .unwrap();
    for entry in expected_times {
        let restored = refs.iter().find(|r| r.event == entry.event).unwrap();
        assert_eq!(restored.recorded_at, entry.recorded_at);
    }
}
#[test]
fn ledger_final_recovery_full_ring_keeps_failed_live_writes() {
    let temp = tempfile::tempdir().unwrap();
    let key = SessionKey("local:final-live".into());
    let ledger = UiProtocolLedger::with_config(config(temp.path()));
    mutation(&ledger, &key, "disk");
    let dir = temp
        .path()
        .join("ui-protocol")
        .join(encode_session_dir_name(&key));
    let path = list_log_files(&dir).unwrap().pop().unwrap();
    let saved = dir.join("saved");
    fs::rename(&path, &saved).unwrap();
    fs::create_dir(&path).unwrap();
    for i in 0..9 {
        mutation(&ledger, &key, &format!("ram-{i}"));
    }
    fs::remove_dir(&path).unwrap();
    fs::rename(&saved, &path).unwrap();
    assert_paths(&ledger, &key, 10);
    mutation(&ledger, &key, "after");
    assert_paths(&ledger, &key, 11);
}
#[cfg(unix)]
#[test]
fn ledger_final_recovery_dangling_log_never_resets_sequence() {
    let temp = tempfile::tempdir().unwrap();
    let key = SessionKey("local:final-dangling".into());
    let cfg = config(temp.path());
    let ledger = UiProtocolLedger::with_config(cfg.clone());
    for i in 0..9 {
        mutation(&ledger, &key, &format!("disk-{i}"));
    }
    drop(ledger);
    let dir = temp
        .path()
        .join("ui-protocol")
        .join(encode_session_dir_name(&key));
    let dangling = dir.join("ledger-dangling.log");
    std::os::unix::fs::symlink(dir.join("missing"), &dangling).unwrap();
    let ledger = UiProtocolLedger::recover(cfg.clone()).ledger;
    let error = ledger.snapshot_with_cursor(&key, None).unwrap_err();
    assert_eq!(error.data.unwrap()["reason"], "read_failed");
    mutation(&ledger, &key, "ram");
    assert!(ledger.snapshot_with_cursor(&key, None).is_err());
    fs::remove_file(dangling).unwrap();
    assert_paths(&ledger, &key, 10);
    assert!(mutation(&ledger, &key, "after").cursor.seq > 9);
    drop(ledger);
    assert_paths(&UiProtocolLedger::recover(cfg).ledger, &key, 10);
}
fn rows(
    ledger: &UiProtocolLedger,
    key: &SessionKey,
    turn: &TurnId,
    task: &octos_core::TaskId,
    agent: &str,
) {
    ledger.append_notification(UiNotification::TurnStarted(
        serde_json::from_value(json!({
        "session_id":key.0,"turn_id":turn.0,"timestamp":chrono::Utc::now()}))
        .unwrap(),
    ));
    ledger.append_notification(UiNotification::TaskUpdated(
        serde_json::from_value(json!({
        "session_id":key.0,"task_id":task,"title":"work","state":"running"}))
        .unwrap(),
    ));
    ledger.append_notification(UiNotification::AgentUpdated(
        serde_json::from_value(json!({
        "session_id":key.0,"agent":{"agent_id":agent,"session_id":key.0,"path":"root/work",
        "role":"worker","nickname":"worker","backend_kind":"native","status":"running",
        "profile_id":"dev","created_at_ms":1,"updated_at_ms":1}}))
        .unwrap(),
    ));
}
#[test]
fn ledger_final_recovery_reconciles_crashed_rows_but_keeps_live_rows() {
    let temp = tempfile::tempdir().unwrap();
    let key = SessionKey("local:final-orphans".into());
    let cfg = LedgerConfig::durable(temp.path().into());
    let ghost_turn = TurnId::new();
    let ghost_task = octos_core::TaskId::new();
    let live_turn = TurnId::new();
    let live_task = octos_core::TaskId::new();
    let ledger = UiProtocolLedger::with_config(cfg.clone());
    rows(&ledger, &key, &ghost_turn, &ghost_task, "ghost");
    drop(ledger);
    let root = temp.path().join("ui-protocol");
    let saved = temp.path().join("saved");
    fs::rename(&root, &saved).unwrap();
    fs::write(&root, b"blocked").unwrap();
    let ledger = UiProtocolLedger::recover(cfg).ledger;
    assert!(ledger.snapshot_with_cursor(&key, None).is_err());
    rows(&ledger, &key, &live_turn, &live_task, "live");
    fs::remove_file(&root).unwrap();
    fs::rename(&saved, &root).unwrap();
    let (events, head) = ledger.snapshot_with_cursor(&key, None).unwrap();
    let terminals: Vec<_> = events
        .iter()
        .filter_map(|entry| match &entry.event {
            UiProtocolLedgerEvent::Notification(UiNotification::TurnError(e)) => {
                Some(e.turn_id.0.to_string())
            }
            UiProtocolLedgerEvent::Notification(UiNotification::TaskUpdated(e))
                if e.state == TaskRuntimeState::Cancelled =>
            {
                Some(e.task_id.to_string())
            }
            UiProtocolLedgerEvent::Notification(UiNotification::AgentUpdated(e))
                if e.agent.status == "failed" =>
            {
                Some(e.agent.agent_id.clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        terminals.len(),
        3,
        "only recovered rows must become terminal: {terminals:?}"
    );
    for id in [
        ghost_turn.0.to_string(),
        ghost_task.to_string(),
        "ghost".into(),
    ] {
        assert!(terminals.contains(&id));
    }
    assert_eq!(
        ledger.snapshot_with_cursor(&key, None).unwrap().1,
        head,
        "one sweep only"
    );
}

#[test]
fn ledger_final_recovery_unpersisted_cache_limit_fails_closed() {
    let temp = tempfile::tempdir().unwrap();
    let key = SessionKey("local:final-limit".into());
    let ledger = UiProtocolLedger::with_config(config(temp.path()));
    mutation(&ledger, &key, "disk");
    let dir = temp
        .path()
        .join("ui-protocol")
        .join(encode_session_dir_name(&key));
    let path = list_log_files(&dir).unwrap().pop().unwrap();
    let saved = dir.join("saved");
    fs::rename(&path, &saved).unwrap();
    fs::create_dir(&path).unwrap();
    for i in 0..40 {
        mutation(&ledger, &key, &format!("{i}-{}", "x".repeat(256 * 1024)));
    }
    fs::remove_dir(&path).unwrap();
    fs::rename(&saved, &path).unwrap();
    for _ in 0..5 {
        ledger.append_progress(UiProgressEvent::new(
            key.clone(),
            None,
            octos_core::ui_protocol::UiProgressMetadata::new("status"),
        ));
    }
    let (_, head) = ledger.snapshot_with_cursor(&key, None).unwrap();
    let error = ledger.retained_file_mutations(&key, &head).unwrap_err();
    assert_eq!(
        error.data.as_ref().unwrap()["reason"],
        "unpersisted_reference_limit"
    );
    assert_eq!(error.data.unwrap()["retryable"], false);
    let inner = ledger.inner.lock().unwrap();
    assert!(inner.sessions[&key].unpersisted_references.bytes <= 8 * 1024 * 1024);
}

#[test]
fn ledger_final_recovery_eviction_cannot_hide_unpersisted_references() {
    for idle in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let key = SessionKey("local:final-eviction".into());
        let mut cfg = config(temp.path());
        cfg.active_session_cap = 1;
        cfg.idle_ttl = Duration::ZERO;
        let ledger = UiProtocolLedger::with_config(cfg);
        mutation(&ledger, &key, "disk");
        let dir = temp
            .path()
            .join("ui-protocol")
            .join(encode_session_dir_name(&key));
        let path = list_log_files(&dir).unwrap().pop().unwrap();
        let saved = dir.join("saved");
        fs::rename(&path, &saved).unwrap();
        fs::create_dir(&path).unwrap();
        mutation(&ledger, &key, "ram");
        fs::remove_dir(&path).unwrap();
        fs::rename(&saved, &path).unwrap();
        if idle {
            assert_eq!(ledger.sweep_idle(), 1);
        } else {
            mutation(&ledger, &SessionKey("local:other".into()), "other");
        }
        for _ in 0..2 {
            let error = ledger.snapshot_with_cursor(&key, None).unwrap_err();
            assert_eq!(
                error.data.unwrap()["reason"],
                "unpersisted_references_evicted"
            );
        }
        assert_paths(
            &ledger,
            &SessionKey("local:other".into()),
            usize::from(!idle),
        );
    }
}
