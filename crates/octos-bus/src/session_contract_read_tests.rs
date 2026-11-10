use super::*;

fn corrupt(bytes: &[u8], fault: &str) -> Vec<u8> {
    let text = std::str::from_utf8(bytes).unwrap();
    let (header, rows) = text.split_once('\n').unwrap();
    match fault {
        "meta" => format!("{{broken meta}}\n{rows}").into_bytes(),
        "schema" => {
            let mut meta: serde_json::Value = serde_json::from_str(header).unwrap();
            meta["schema_version"] = (CURRENT_SESSION_SCHEMA + 1).into();
            format!("{meta}\n{rows}").into_bytes()
        }
        "utf8" => [header.as_bytes(), b"\n\xff\n", rows.as_bytes()].concat(),
        "row" => format!("{header}\n{{\"role\":\"unknown\"}}\n{rows}").into_bytes(),
        "middle" => {
            format!("{header}\n{{\"role\":\"assistant\",\"content\":\"cut\n{rows}").into_bytes()
        }
        "empty" => vec![],
        _ => panic!("unknown fault"),
    }
}

fn contract(parent: &SessionKey) -> ChildSessionContract {
    ChildSessionContract {
        task_id: "strict-read".into(),
        task_label: "preserve transcript".into(),
        parent_session_key: parent.to_string(),
        child_session_key: child_session_key(parent, "strict-read").to_string(),
        workflow_kind: None,
        current_phase: None,
        terminal_state: None,
        join_state: None,
        joined_at: None,
        failure_action: None,
        error: None,
        output_files: vec![],
    }
}

#[tokio::test]
async fn canonical_contract_rejects_corrupt_transcripts_and_retries() {
    for fault in ["meta", "schema", "utf8", "row", "middle", "empty"] {
        let tmp = tempfile::tempdir().unwrap();
        let key = SessionKey::new("api", &format!("strict-{fault}"));
        let mut handle = SessionHandle::open(tmp.path(), &key);
        handle
            .add_message(Message::user("keep history"))
            .await
            .unwrap();
        let path = handle.session_path();
        let original = std::fs::read(&path).unwrap();
        let damaged = corrupt(&original, fault);
        std::fs::write(&path, &damaged).unwrap();
        let result =
            upsert_child_contract_through_canonical_path(tmp.path(), &key, contract(&key)).await;
        assert!(result.is_err(), "{fault}: {result:?}");
        assert_eq!(std::fs::read(&path).unwrap(), damaged, "{fault}");
        std::fs::write(&path, original).unwrap();
        upsert_child_contract_through_canonical_path(tmp.path(), &key, contract(&key))
            .await
            .unwrap();
        let reopened = SessionHandle::open_full(tmp.path(), &key);
        assert_eq!(reopened.session.messages.len(), 1);
        assert_eq!(reopened.session.messages[0].content, "keep history");
        assert_eq!(reopened.session.child_contracts.len(), 1);
    }
}

#[tokio::test]
async fn strict_contract_load_preserves_legacy_and_sealed_history() {
    for fault in ["meta", "schema", "utf8", "row", "middle", "empty"] {
        for legacy in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let key = SessionKey::new("api", &format!("strict-layout-{fault}-{legacy}"));
            let mut handle = SessionHandle::open(tmp.path(), &key);
            handle
                .add_message(Message::user("old history"))
                .await
                .unwrap();
            let active = handle.session_path();
            let original = std::fs::read(&active).unwrap();
            let source = if legacy {
                let path = SessionManager::session_path_static(&tmp.path().join("sessions"), &key);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::rename(&active, &path).unwrap();
                path
            } else {
                let dir = segments_dir(&active);
                std::fs::create_dir_all(&dir).unwrap();
                let path = segment_path(&dir, 1);
                std::fs::write(&path, &original).unwrap();
                let text = std::str::from_utf8(&original).unwrap();
                let mut meta: serde_json::Value =
                    serde_json::from_str(text.lines().next().unwrap()).unwrap();
                meta["sealed_segments"] = 1.into();
                meta["base_seq"] = 1.into();
                std::fs::write(&active, format!("{meta}\n")).unwrap();
                path
            };
            let damaged = corrupt(&original, fault);
            std::fs::write(&source, &damaged).unwrap();
            let before_active = std::fs::read(&active);
            let result =
                upsert_child_contract_through_canonical_path(tmp.path(), &key, contract(&key))
                    .await;
            assert!(result.is_err(), "{fault}, legacy={legacy}: {result:?}");
            assert_eq!(std::fs::read(&source).unwrap(), damaged);
            if legacy {
                assert!(!active.exists());
            } else {
                assert_eq!(std::fs::read(&active).unwrap(), before_active.unwrap());
            }
            std::fs::write(&source, original).unwrap();
            upsert_child_contract_through_canonical_path(tmp.path(), &key, contract(&key))
                .await
                .unwrap();
            let reopened = SessionHandle::try_open(tmp.path(), &key).unwrap();
            assert_eq!(reopened.session.messages.len(), 1);
            assert_eq!(reopened.session.messages[0].content, "old history");
            assert_eq!(reopened.session.child_contracts.len(), 1);
        }
    }
}

#[tokio::test]
async fn strict_contract_load_recovers_interrupted_seal_and_replays_rollback() {
    let tmp = tempfile::tempdir().unwrap();
    let key = SessionKey::new("api", "strict-interrupted-seal");
    let mut handle = SessionHandle::open(tmp.path(), &key);
    handle
        .add_message(Message::user("keep older turn"))
        .await
        .unwrap();
    handle
        .add_message(Message::user("rolled back turn"))
        .await
        .unwrap();
    let active = handle.session_path();
    let dir = segments_dir(&active);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::rename(&active, segment_path(&dir, 1)).unwrap();
    upsert_child_contract_through_canonical_path(tmp.path(), &key, contract(&key))
        .await
        .unwrap();
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&active)
        .unwrap();
    use std::io::Write;
    writeln!(file, "{}", rollback_marker_line(1).unwrap()).unwrap();
    upsert_child_contract_through_canonical_path(tmp.path(), &key, contract(&key))
        .await
        .unwrap();
    let reopened = SessionHandle::try_open(tmp.path(), &key).unwrap();
    assert_eq!(reopened.session.messages.len(), 1);
    assert_eq!(reopened.session.messages[0].content, "keep older turn");
    assert_eq!(reopened.session.child_contracts.len(), 1);
}

#[tokio::test]
async fn strict_tail_receipt_does_not_excuse_other_corruption() {
    use std::io::Write;
    for fault in ["middle", "receipt", "utf8", "complete-json"] {
        let tmp = tempfile::tempdir().unwrap();
        let key = SessionKey::new("api", &format!("seal-negative-{fault}"));
        let mut handle = SessionHandle::open(tmp.path(), &key);
        handle.add_message(Message::user("keep")).await.unwrap();
        let path = handle.session_path();
        let tail = b"{\"role\":\"assistant\",\"content\":\"cut";
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(tail)
            .unwrap();
        handle.add_message(Message::user("next")).await.unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let damaged = match fault {
            "middle" => text
                .replacen("\n", "\n{\"content\":\"bad\n", 1)
                .into_bytes(),
            "receipt" => text.replace("\"bytes\":", "\"wrong_bytes\":").into_bytes(),
            "utf8" => [text.as_bytes(), b"\xff"].concat(),
            "complete-json" => [text.as_bytes(), b"{\"role\":\"unknown\"}"].concat(),
            _ => unreachable!(),
        };
        std::fs::write(&path, &damaged).unwrap();
        assert!(
            upsert_child_contract_through_canonical_path(tmp.path(), &key, contract(&key))
                .await
                .is_err(),
            "{fault}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), damaged, "{fault}");
    }
}
