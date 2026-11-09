// Included by the transport test module; actual hydrate dispatch, no model.
include!("ui_protocol_hydrate_recovery_tests.rs");
async fn hydrate_budget_frame(
    state: &Arc<AppState>,
    ledger: &Arc<UiProtocolLedger>,
    session: &SessionKey,
    after: Option<UiCursor>,
) -> Value {
    let (ws, mut rx) = ws_connection_for_test(8);
    handle_session_hydrate(
        &ws,
        state,
        ledger,
        &PendingApprovalStore::default(),
        &PendingQuestionStore::default(),
        &active_turns_registry(),
        None,
        None,
        ConnectionUiFeatures::default(),
        "budget-probe".into(),
        SessionHydrateParams {
            session_id: session.clone(),
            after,
            include: vec![],
        },
    )
    .await;
    recv_rpc_json(&mut rx).await
}

async fn hydrate_budget_fixture(
    cap: usize,
    count: usize,
    path_bytes: usize,
) -> (
    tempfile::TempDir,
    Arc<AppState>,
    Arc<UiProtocolLedger>,
    SessionKey,
    UiCursor,
) {
    use octos_core::ui_protocol::{UiFileMutationNotice, UiProgressMetadata};
    let temp = tempfile::tempdir().unwrap();
    let state = state_with_sessions(temp.path());
    let mut config = LedgerConfig::durable(temp.path().into());
    config.retained_per_session = cap;
    config.snapshot_every_events = 0;
    let ledger = Arc::new(UiProtocolLedger::with_config(config));
    let session = SessionKey("local:hydrate-budget".into());
    state
        .sessions
        .as_ref()
        .unwrap()
        .lock()
        .await
        .get_or_create(&session)
        .await;
    let mut head = None;
    for i in 0..count {
        let notice = UiFileMutationNotice::new(format!("{i}{}", "x".repeat(path_bytes)), "modify");
        head = Some(
            ledger
                .append_progress(UiProgressEvent::new(
                    session.clone(),
                    None,
                    UiProgressMetadata::file_mutation(notice),
                ))
                .cursor,
        );
    }
    (temp, state, ledger, session, head.unwrap())
}

fn hydrate_budget_log(temp: &std::path::Path) -> std::path::PathBuf {
    let dir = std::fs::read_dir(temp.join("ui-protocol"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|e| e == "log"))
        .unwrap()
}

fn assert_hydrate_budget_error(frame: &Value, reason: &str) {
    assert!(
        frame.get("result").is_none(),
        "must not commit a partial snapshot: {frame}"
    );
    assert_eq!(
        frame["error"]["data"]["kind"], "hydrate_references_unavailable",
        "{frame}"
    );
    assert_eq!(frame["error"]["data"]["reason"], reason, "{frame}");
}

// Replacing an open log pathname has platform-specific sharing semantics.
#[cfg(unix)]
#[tokio::test]
async fn hydrate_budget_read_failures_do_not_replace_changes_and_retry_recovers() {
    for fault in ["directory", "missing", "json", "utf8", "version"] {
        let (temp, state, ledger, session, head) = hydrate_budget_fixture(1, 3, 8).await;
        let path = hydrate_budget_log(temp.path());
        let original = std::fs::read(&path).unwrap();
        match fault {
            "directory" => {
                std::fs::remove_file(&path).unwrap();
                std::fs::create_dir(&path).unwrap();
            }
            "missing" => std::fs::remove_file(&path).unwrap(),
            "json" => std::fs::write(&path, b"{broken\n").unwrap(),
            "utf8" => std::fs::write(&path, b"\xff\n").unwrap(),
            "version" => std::fs::write(
                &path,
                String::from_utf8(original.clone())
                    .unwrap()
                    .replace("\"v\":1", "\"v\":999"),
            )
            .unwrap(),
            _ => unreachable!(),
        }
        let frame = hydrate_budget_frame(&state, &ledger, &session, Some(head.clone())).await;
        assert_hydrate_budget_error(
            &frame,
            if matches!(fault, "directory" | "missing") {
                "read_failed"
            } else {
                "invalid_record"
            },
        );
        if fault == "directory" {
            std::fs::remove_dir(&path).unwrap();
        }
        std::fs::write(&path, original).unwrap();
        let frame = hydrate_budget_frame(&state, &ledger, &session, Some(head)).await;
        assert_eq!(
            frame["result"]["file_mutations"].as_array().unwrap().len(),
            3
        );
    }
}

#[tokio::test]
async fn hydrate_budget_bounds_disk_even_with_no_additional_mutations() {
    let (temp, state, ledger, session, head) = hydrate_budget_fixture(1, 3, 8).await;
    // set_len needs GENERIC_WRITE on Windows; an append handle maps to
    // FILE_APPEND_DATA only, so open write-only for the truncate.
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(hydrate_budget_log(temp.path()))
        .unwrap();
    file.set_len(file.metadata().unwrap().len() + 65 * 1024 * 1024)
        .unwrap();
    assert_hydrate_budget_error(
        &hydrate_budget_frame(&state, &ledger, &session, Some(head)).await,
        "scan_bytes_limit",
    );
}

#[tokio::test]
async fn hydrate_budget_at_count_limit_is_complete_and_ordered() {
    let (_temp, state, ledger, session, head) = hydrate_budget_fixture(8, 1024, 8).await;
    let frame = hydrate_budget_frame(&state, &ledger, &session, Some(head.clone())).await;
    let notices = frame["result"]["file_mutations"].as_array().unwrap();
    assert_eq!(notices.len(), 1024);
    assert_eq!(notices.first().unwrap()["path"], "0xxxxxxxx");
    assert_eq!(notices.last().unwrap()["path"], "1023xxxxxxxx");
    assert_eq!(frame["result"]["cursor"], json!(head));
}

#[cfg(unix)]
#[tokio::test]
async fn hydrate_budget_permission_error_is_visible_and_repairable() {
    use std::os::unix::fs::PermissionsExt;
    let (temp, state, ledger, session, head) = hydrate_budget_fixture(1, 3, 8).await;
    let path = hydrate_budget_log(temp.path());
    let permissions = std::fs::metadata(&path).unwrap().permissions();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
    let frame = hydrate_budget_frame(&state, &ledger, &session, Some(head.clone())).await;
    std::fs::set_permissions(&path, permissions).unwrap();
    assert_hydrate_budget_error(&frame, "read_failed");
    assert_eq!(frame["error"]["data"]["retryable"], true);
    let frame = hydrate_budget_frame(&state, &ledger, &session, Some(head)).await;
    assert_eq!(
        frame["result"]["file_mutations"].as_array().unwrap().len(),
        3
    );
}

#[tokio::test]
#[ignore = "manual hydrate size/latency measurement, no timing assertion"]
async fn measure_hydrate_retained_mutation_history() {
    use octos_core::ui_protocol::{UiFileMutationNotice, UiProgressMetadata};
    for count in [1_000, 20_000, 80_000] {
        let temp = tempfile::tempdir().unwrap();
        let state = state_with_sessions(temp.path());
        let mut config = LedgerConfig::durable(temp.path().into());
        config.retained_per_session = 8;
        config.snapshot_every_events = 0;
        let ledger = Arc::new(UiProtocolLedger::with_config(config));
        let session = SessionKey("local:hydrate-budget-benchmark".into());
        state
            .sessions
            .as_ref()
            .unwrap()
            .lock()
            .await
            .get_or_create(&session)
            .await;
        let mut head = None;
        for i in 0..count {
            let notice =
                UiFileMutationNotice::new(format!("src/{}/{i}.rs", "a".repeat(180)), "modify");
            head = Some(
                ledger
                    .append_progress(UiProgressEvent::new(
                        session.clone(),
                        None,
                        UiProgressMetadata::file_mutation(notice),
                    ))
                    .cursor,
            );
        }
        for attempt in 0..3 {
            let started = std::time::Instant::now();
            let frame = hydrate_budget_frame(&state, &ledger, &session, head.clone()).await;
            eprintln!(
                "HYDRATE_BENCH {}",
                json!({"count":count,"attempt":attempt,
                "elapsed_ms":started.elapsed().as_secs_f64()*1000.0,
                "frame_bytes":serde_json::to_vec(&frame).unwrap().len(),
                "returned_mutations":frame["result"]["file_mutations"].as_array().map(Vec::len),
                "error":frame.get("error"), "disk_bytes":ledger.metrics().bytes_on_disk})
            );
        }
    }
}

#[tokio::test]
async fn hydrate_budget_unshrinkable_changes_fail_without_partial_result() {
    use octos_core::ui_protocol::UiFileMutationNotice;
    let (_temp, state, ledger, session, _head) = hydrate_budget_fixture(4, 1, 8).await;
    let baseline = hydrate_budget_frame(&state, &ledger, &session, None).await;
    let mut result: SessionHydrateResult =
        serde_json::from_value(baseline["result"].clone()).unwrap();
    result.file_mutations = Some(vec![UiFileMutationNotice::new(
        "\\".repeat(MAX_TEXT_FRAME_BYTES),
        "modify",
    )]);
    let (ws, mut rx) = ws_connection_for_test(8);
    let id = "escaped-\"\\\n-id";
    send_hydrate_result(&ws, id.into(), result);
    let frame = recv_rpc_json(&mut rx).await;
    assert_eq!(frame["id"], id);
    assert_hydrate_budget_error(&frame, "response_bytes_limit");
}
