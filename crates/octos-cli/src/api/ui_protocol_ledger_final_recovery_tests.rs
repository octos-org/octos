use super::*;

fn event(key: &SessionKey, path: &str) -> UiProgressEvent {
    UiProgressEvent::new(
        key.clone(),
        None,
        octos_core::ui_protocol::UiProgressMetadata::file_mutation(UiFileMutationNotice::new(
            path, "modify",
        )),
    )
}
fn mutation(ledger: &UiProtocolLedger, key: &SessionKey, path: &str) -> LedgeredUiProtocolEvent {
    ledger
        .try_append_progress_from(event(key, path), ConnectionId::next())
        .unwrap()
}
fn config(root: &Path) -> LedgerConfig {
    let mut cfg = LedgerConfig::durable(root.into());
    cfg.retained_per_session = 3;
    cfg.snapshot_every_events = 0;
    cfg
}
fn assert_paths(ledger: &UiProtocolLedger, key: &SessionKey, count: usize) {
    let (_, head) = ledger.snapshot_with_cursor(key, None).unwrap();
    assert_eq!(
        ledger.retained_file_mutations(key, &head).unwrap().len(),
        count
    );
}
fn directory(root: &Path, key: &SessionKey) -> PathBuf {
    root.join("ui-protocol").join(encode_session_dir_name(key))
}
#[test]
fn ledger_final_recovery_cold_read_failure_never_admits_ram_only_events() {
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
    let ledger = UiProtocolLedger::recover(cfg.clone()).ledger;
    let mut live = ledger.subscribe(&key);
    for _ in 0..6 {
        assert!(ledger.snapshot_with_cursor(&key, None).is_err());
        assert!(
            ledger
                .try_append_progress_from(event(&key, "rejected"), ConnectionId::next())
                .is_err()
        );
        assert!(!ledger.has_session_in_memory_for_test(&key));
        assert!(live.try_recv().is_err());
    }
    fs::remove_file(&root).unwrap();
    fs::rename(&saved, &root).unwrap();
    assert_paths(&ledger, &key, 9);
    assert_eq!(mutation(&ledger, &key, "after-repair").cursor.seq, 10);
    drop(ledger);
    assert_paths(&UiProtocolLedger::recover(cfg).ledger, &key, 10);
}
#[test]
fn ledger_final_recovery_write_failure_stays_latched_after_repair() {
    let temp = tempfile::tempdir().unwrap();
    let key = SessionKey("local:final-write".into());
    let cfg = config(temp.path());
    let ledger = UiProtocolLedger::with_config(cfg.clone());
    mutation(&ledger, &key, "disk");
    let dir = directory(temp.path(), &key);
    let path = list_log_files(&dir).unwrap().pop().unwrap();
    let original = fs::read(&path).unwrap();
    let saved = dir.join("saved");
    fs::rename(&path, &saved).unwrap();
    fs::create_dir(&path).unwrap();
    let mut live = ledger.subscribe(&key);
    for _ in 0..40 {
        assert!(
            ledger
                .try_append_progress_from(
                    event(&key, &"x".repeat(256 * 1024)),
                    ConnectionId::next()
                )
                .is_err()
        );
    }
    fs::remove_dir(&path).unwrap();
    fs::rename(&saved, &path).unwrap();
    for _ in 0..2 {
        assert!(ledger.snapshot_with_cursor(&key, None).is_err());
        assert!(
            ledger
                .try_append_progress_from(
                    event(&key, "after-repair-rejected"),
                    ConnectionId::next()
                )
                .is_err()
        );
    }
    assert!(live.try_recv().is_err());
    assert_eq!(fs::read(&path).unwrap(), original);
    let inner = ledger.inner.lock().unwrap();
    assert_eq!(inner.sessions[&key].next_seq, 1);
    assert_eq!(inner.sessions[&key].entries.len(), 1);
    drop(inner);
    drop(ledger);
    let ledger = UiProtocolLedger::recover(cfg).ledger;
    assert_paths(&ledger, &key, 1);
    assert_eq!(mutation(&ledger, &key, "after-restart").cursor.seq, 2);
}
#[test]
fn ledger_final_recovery_eviction_cannot_clear_ambiguous_write_latch() {
    for idle in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let key = SessionKey("local:final-eviction".into());
        let mut cfg = config(temp.path());
        cfg.active_session_cap = 1;
        cfg.idle_ttl = Duration::ZERO;
        let ledger = UiProtocolLedger::with_config(cfg.clone());
        mutation(&ledger, &key, "disk");
        let dir = directory(temp.path(), &key);
        let mut live = ledger.subscribe(&key);
        let fault = inject_ledger_write_failure_at(&dir, "after-write", true);
        assert!(
            ledger
                .try_append_progress_from(event(&key, "ambiguous-disk"), ConnectionId::next())
                .is_err()
        );
        drop(fault);
        assert!(live.try_recv().is_err());
        if idle {
            assert_eq!(ledger.sweep_idle(), 1);
        } else {
            mutation(&ledger, &SessionKey("local:other".into()), "other");
        }
        assert!(!ledger.has_session_in_memory_for_test(&key));
        for _ in 0..2 {
            assert!(
                ledger.snapshot_with_cursor(&key, None).is_err(),
                "eviction cannot clear a write latch"
            );
            assert!(
                ledger
                    .try_append_progress_from(event(&key, "rejected"), ConnectionId::next())
                    .is_err()
            );
        }
        let ledger = UiProtocolLedger::recover(cfg).ledger;
        assert_paths(&ledger, &key, 2);
        assert_eq!(mutation(&ledger, &key, "after-restart").cursor.seq, 3);
    }
}
#[test]
fn ledger_final_recovery_malformed_latch_prevents_tail_repair_until_restart() {
    let temp = tempfile::tempdir().unwrap();
    let key = SessionKey("local:final-malformed".into());
    let cfg = config(temp.path());
    let ledger = UiProtocolLedger::with_config(cfg.clone());
    mutation(&ledger, &key, "disk");
    drop(ledger);
    let path = list_log_files(&directory(temp.path(), &key))
        .unwrap()
        .pop()
        .unwrap();
    let mut bytes = fs::read(&path).unwrap();
    bytes.extend_from_slice(b"{malformed\n");
    fs::write(&path, &bytes).unwrap();
    let ledger = UiProtocolLedger::recover(cfg.clone()).ledger;
    assert!(ledger.snapshot_with_cursor(&key, None).is_err());
    bytes.pop(); // A latched malformed record now looks like a repairable tail.
    fs::write(&path, &bytes).unwrap();
    for _ in 0..2 {
        assert!(ledger.snapshot_with_cursor(&key, None).is_err());
        assert!(
            ledger
                .try_append_progress_from(event(&key, "rejected"), ConnectionId::next())
                .is_err()
        );
        assert_eq!(
            fs::read(&path).unwrap(),
            bytes,
            "same-process retry must not truncate latched history"
        );
    }
    drop(ledger);
    let ledger = UiProtocolLedger::recover(cfg).ledger;
    assert_paths(&ledger, &key, 1);
    assert_eq!(mutation(&ledger, &key, "after-restart").cursor.seq, 2);
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
    let dir = directory(temp.path(), &key);
    let dangling = dir.join("ledger-dangling.log");
    std::os::unix::fs::symlink(dir.join("missing"), &dangling).unwrap();
    let ledger = UiProtocolLedger::recover(cfg.clone()).ledger;
    assert!(ledger.snapshot_with_cursor(&key, None).is_err());
    assert!(
        ledger
            .try_append_progress_from(event(&key, "rejected"), ConnectionId::next())
            .is_err()
    );
    fs::remove_file(dangling).unwrap();
    assert_paths(&ledger, &key, 9);
    assert_eq!(mutation(&ledger, &key, "after").cursor.seq, 10);
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
fn ledger_final_recovery_reconciles_crashed_rows_before_admitting_live_rows() {
    let temp = tempfile::tempdir().unwrap();
    let key = SessionKey("local:final-orphans".into());
    let cfg = LedgerConfig::durable(temp.path().into());
    let ghost_turn = TurnId::new();
    let ghost_task = octos_core::TaskId::new();
    let ledger = UiProtocolLedger::with_config(cfg.clone());
    rows(&ledger, &key, &ghost_turn, &ghost_task, "ghost");
    drop(ledger);
    let root = temp.path().join("ui-protocol");
    let saved = temp.path().join("saved");
    fs::rename(&root, &saved).unwrap();
    fs::write(&root, b"blocked").unwrap();
    let ledger = UiProtocolLedger::recover(cfg).ledger;
    assert!(ledger.snapshot_with_cursor(&key, None).is_err());
    assert!(
        ledger
            .try_append_progress_from(event(&key, "rejected"), ConnectionId::next())
            .is_err()
    );
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
    assert_eq!(terminals.len(), 3);
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
        "one orphan sweep"
    );
    rows(
        &ledger,
        &key,
        &TurnId::new(),
        &octos_core::TaskId::new(),
        "live",
    );
    let (events, after) = ledger.snapshot_with_cursor(&key, Some(&head)).unwrap();
    assert_eq!(events.len(), 3, "live rows must not be swept");
    assert_eq!(after.seq, head.seq + 3);
}
