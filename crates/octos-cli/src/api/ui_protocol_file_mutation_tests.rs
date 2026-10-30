// Included by ui_protocol_tests.rs; uses the existing transport fixtures.

#[test]
fn hydrate_file_mutations_recover_preview_references_beyond_hot_ring() {
    use crate::contracts::diff::{DiffPreviewConfig, PendingDiffPreviewStore};
    use octos_core::ui_protocol::{UiFileMutationNotice, UiProgressMetadata};
    let dir = tempfile::tempdir().unwrap();
    let mut config = LedgerConfig::durable(dir.path().into());
    config.retained_per_session = 1;
    let session = SessionKey("local:file-mutation-recovery".into());
    let ledger = UiProtocolLedger::with_config(config.clone());
    ledger.set_session_scope(&session, Some("workspace-a".into()));
    let previews =
        PendingDiffPreviewStore::with_config(DiffPreviewConfig::durable(dir.path().into()));
    let mut notice = UiFileMutationNotice::new("sum.js", "modify");
    previews.upsert_file_mutation(session.clone(), &TurnId::new(), &mut notice,
        Some("diff --git a/sum.js b/sum.js\n--- a/sum.js\n+++ b/sum.js\n@@ -1 +1 @@\n-sum + row.amount\n+sum + Number(row.amount)\n"));
    let append = |notice| {
        ledger.append_progress(UiProgressEvent::new(
            session.clone(),
            None,
            UiProgressMetadata::file_mutation(notice),
        ))
    };
    let captured = append(notice.clone());
    append(UiFileMutationNotice::new("later.js", "create"));
    assert_eq!(
        ledger
            .retained_file_mutations(&session, &captured.cursor)
            .unwrap(),
        vec![notice.clone()]
    );
    drop(ledger);
    drop(previews);

    let ledger = UiProtocolLedger::recover(config).ledger;
    ledger.set_session_scope(&session, Some("workspace-a".into()));
    assert_eq!(
        ledger
            .retained_file_mutations(&session, &captured.cursor)
            .unwrap(),
        vec![notice.clone()]
    );
    let previews =
        PendingDiffPreviewStore::recover(DiffPreviewConfig::durable(dir.path().into())).store;
    let params = DiffPreviewGetParams {
        session_id: session.clone(),
        preview_id: notice.preview_id.unwrap(),
    };
    let preview = previews.get(params.clone()).unwrap().preview;
    assert_eq!(preview.files[0].path, "sum.js");
    assert!(
        preview.files[0].hunks[0]
            .lines
            .iter()
            .any(|line| line.content == "sum + Number(row.amount)")
    );
    assert!(
        previews
            .get(DiffPreviewGetParams {
                session_id: SessionKey("local:other".into()),
                ..params
            })
            .is_err()
    );
    ledger.set_session_scope(&session, Some("workspace-b".into()));
    assert!(
        ledger
            .retained_file_mutations(&session, &captured.cursor)
            .is_err()
    );
    let (_, head) = ledger.snapshot_with_cursor(&session, None).unwrap();
    assert!(
        ledger
            .retained_file_mutations(&session, &head)
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn session_hydrate_file_mutations_honor_include_after_and_profile_scope() {
    use octos_core::ui_protocol::{UiFileMutationNotice, UiProgressMetadata};
    let temp = tempfile::tempdir().unwrap();
    let state = state_with_sessions(temp.path());
    let ledger = event_ledger(&state).await;
    let session = SessionKey("local:hydrate-file-changes".into());
    state
        .sessions
        .as_ref()
        .unwrap()
        .lock()
        .await
        .get_or_create(&session)
        .await;
    let mut notice = UiFileMutationNotice::new("sum.js", "modify");
    notice.preview_id = Some(PreviewId::new());
    let entry = ledger.append_progress(UiProgressEvent::new(
        session.clone(),
        None,
        UiProgressMetadata::file_mutation(notice.clone()),
    ));
    for (include, expected) in [
        (vec![], true),
        (vec!["messages".into()], true),
        (vec!["turns".into()], false),
    ] {
        let (ws, mut rx) = ws_connection_for_test(8);
        handle_session_hydrate(
            &ws,
            &state,
            &ledger,
            &PendingApprovalStore::default(),
            &PendingQuestionStore::default(),
            &active_turns_registry(),
            None,
            None,
            ConnectionUiFeatures::default(),
            "hydrate".into(),
            SessionHydrateParams {
                session_id: session.clone(),
                after: Some(entry.cursor.clone()),
                include,
            },
        )
        .await;
        let frame = recv_rpc_json(&mut rx).await;
        assert!(frame.get("error").is_none(), "{frame}");
        if expected {
            assert_eq!(frame["result"]["file_mutations"], json!([notice]));
        } else {
            assert!(frame["result"].get("file_mutations").is_none());
        }
    }
    let (ws, mut rx) = ws_connection_for_test(8);
    handle_session_hydrate(
        &ws,
        &state,
        &ledger,
        &PendingApprovalStore::default(),
        &PendingQuestionStore::default(),
        &active_turns_registry(),
        Some("other-profile"),
        None,
        ConnectionUiFeatures::default(),
        "forbidden".into(),
        SessionHydrateParams {
            session_id: SessionKey::with_profile("coding", "local", "private"),
            after: None,
            include: vec![],
        },
    )
    .await;
    // A profile mismatch follows the transport's existing fail-closed close path.
    assert!(matches!(rx.recv().await, Some(WsMessage::Close(Some(frame))) if frame.code == 1008));
}
