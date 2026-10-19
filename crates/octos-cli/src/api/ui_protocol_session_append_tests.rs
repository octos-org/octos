use super::*;

// ===== UPCR-2026-042 `session/append_message` handler =====

/// Disk-backed state with one persisted user row so assistant records have a
/// thread to derive from. Mirrors `prg_state_with_persisted_turns`'s shape.
async fn append_state_with_user_row(session_id: &SessionKey) -> (Arc<AppState>, tempfile::TempDir) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let manager = octos_bus::SessionManager::open(tmp.path()).expect("session manager open");
    let manager = Arc::new(tokio::sync::Mutex::new(manager));
    {
        let mut guard = manager.lock().await;
        let user = Message {
            role: MessageRole::User,
            content: "what is the capital?".into(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: Some("cmid-user-1".into()),
            thread_id: Some("cmid-user-1".into()),
            timestamp: Utc::now(),
            source: None,
        };
        guard
            .add_message(session_id, user)
            .await
            .expect("persist user");
    }
    let state = Arc::new(AppState {
        sessions: Some(manager),
        ..AppState::empty_for_tests()
    });
    (state, tmp)
}

#[tokio::test(flavor = "current_thread")]
async fn session_append_message_persists_records_without_a_turn() {
    let session_id = SessionKey("local:append-1".into());
    let (state, _tmp) = append_state_with_user_row(&session_id).await;
    let (ws, mut rx) = ws_connection_for_test(8);

    handle_session_append_message(
        &ws,
        &state,
        None,
        None,
        "am1".into(),
        SessionAppendMessageParams {
            session_id: session_id.clone(),
            role: "assistant".into(),
            content: "The capital is Kyoto.".into(),
            media: vec!["uploads/map.png".into()],
            source: "external_record:whiteboard".into(),
            client_message_id: Some("cmid-answer-1".into()),
            thread_id: None,
        },
    )
    .await;

    let frame = recv_rpc_json(&mut rx).await;
    assert_eq!(frame["id"], "am1");
    let result = &frame["result"];
    assert_eq!(result["session_id"], session_id.to_string());
    assert_eq!(result["seq"], 1);
    // The assistant record derived its thread from the session's user row.
    assert_eq!(result["thread_id"], "cmid-user-1");

    // The row is durable with its provenance tag — a fresh manager reads it
    // back from the JSONL.
    let sessions = state.sessions.as_ref().unwrap();
    let guard = sessions.lock().await;
    let session = guard.load(&session_id).await.expect("session");
    assert_eq!(session.messages.len(), 2);
    let assistant = session.messages.last().unwrap();
    assert_eq!(assistant.content, "The capital is Kyoto.");
    assert_eq!(
        assistant.source.as_deref(),
        Some("external_record:whiteboard")
    );
    assert_eq!(assistant.thread_id.as_deref(), Some("cmid-user-1"));
}

#[tokio::test(flavor = "current_thread")]
async fn session_append_message_retry_returns_existing_seq_without_a_second_row() {
    let session_id = SessionKey("local:append-retry".into());
    let (state, _tmp) = append_state_with_user_row(&session_id).await;

    let params = |content: &str| SessionAppendMessageParams {
        session_id: session_id.clone(),
        role: "assistant".into(),
        content: content.into(),
        media: vec![],
        source: "external_record:whiteboard".into(),
        client_message_id: Some("cmid-answer-1".into()),
        thread_id: None,
    };

    let (ws, mut rx) = ws_connection_for_test(8);
    handle_session_append_message(&ws, &state, None, None, "am-a".into(), params("first")).await;
    let first = recv_rpc_json(&mut rx).await;
    assert_eq!(first["result"]["seq"], 1);

    let (ws, mut rx) = ws_connection_for_test(8);
    handle_session_append_message(&ws, &state, None, None, "am-b".into(), params("retried")).await;
    let retry = recv_rpc_json(&mut rx).await;
    assert_eq!(retry["result"]["seq"], 1, "retry returns the original seq");

    let sessions = state.sessions.as_ref().unwrap();
    let guard = sessions.lock().await;
    let session = guard.load(&session_id).await.expect("session");
    assert_eq!(session.messages.len(), 2, "retry must not double-append");
    assert_eq!(session.messages.last().unwrap().content, "first");
}

#[tokio::test(flavor = "current_thread")]
async fn session_append_message_retry_echoes_the_existing_row_binding() {
    let session_id = SessionKey("local:append-retry-thread".into());
    let (state, _tmp) = append_state_with_user_row(&session_id).await;

    // A system record is legitimately unbound.
    let (ws, mut rx) = ws_connection_for_test(8);
    handle_session_append_message(
        &ws,
        &state,
        None,
        None,
        "am-s".into(),
        SessionAppendMessageParams {
            session_id: session_id.clone(),
            role: "system".into(),
            content: "imported note".into(),
            media: vec![],
            source: "external_record:import".into(),
            client_message_id: Some("review-same-id".into()),
            thread_id: None,
        },
    )
    .await;
    let frame = recv_rpc_json(&mut rx).await;
    assert_eq!(frame["result"]["seq"], 1);
    assert_eq!(
        frame["result"]["thread_id"],
        serde_json::Value::Null,
        "{frame}"
    );

    // Retrying the same cmid under a role that would root its own thread
    // still hits the durable row — and must echo ITS binding (absent), not
    // the retry payload's freshly stamped one.
    let (ws, mut rx) = ws_connection_for_test(8);
    handle_session_append_message(
        &ws,
        &state,
        None,
        None,
        "am-r".into(),
        SessionAppendMessageParams {
            session_id: session_id.clone(),
            role: "user".into(),
            content: "retried as user".into(),
            media: vec![],
            source: "external_record:import".into(),
            client_message_id: Some("review-same-id".into()),
            thread_id: None,
        },
    )
    .await;
    let frame = recv_rpc_json(&mut rx).await;
    assert_eq!(frame["result"]["seq"], 1, "retry returns the original seq");
    assert_eq!(
        frame["result"]["thread_id"],
        serde_json::Value::Null,
        "a found row with no binding stays unbound: {frame}"
    );

    let sessions = state.sessions.as_ref().unwrap();
    let guard = sessions.lock().await;
    let session = guard.load(&session_id).await.expect("session");
    assert_eq!(session.messages.len(), 2, "retry must not double-append");
    let system_row = session
        .messages
        .iter()
        .find(|row| row.client_message_id.as_deref() == Some("review-same-id"))
        .expect("durable system row");
    assert_eq!(system_row.role, MessageRole::System);
    assert_eq!(
        system_row.thread_id, None,
        "the retry must not mint a thread onto the durable row"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn session_append_message_user_record_roots_its_own_thread() {
    let session_id = SessionKey("local:append-user".into());
    let (state, _tmp) = append_state_with_user_row(&session_id).await;
    let (ws, mut rx) = ws_connection_for_test(8);

    handle_session_append_message(
        &ws,
        &state,
        None,
        None,
        "am2".into(),
        SessionAppendMessageParams {
            session_id: session_id.clone(),
            role: "user".into(),
            content: "and the old one?".into(),
            media: vec![],
            source: "external_record:whiteboard".into(),
            client_message_id: Some("cmid-user-2".into()),
            thread_id: None,
        },
    )
    .await;

    let frame = recv_rpc_json(&mut rx).await;
    assert_eq!(frame["result"]["seq"], 1);
    assert_eq!(frame["result"]["thread_id"], "cmid-user-2");

    let sessions = state.sessions.as_ref().unwrap();
    let guard = sessions.lock().await;
    let session = guard.load(&session_id).await.expect("session");
    assert_eq!(
        session.messages.last().unwrap().thread_id.as_deref(),
        Some("cmid-user-2")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn session_append_message_rejects_invalid_params_with_typed_kinds() {
    let session_id = SessionKey("local:append-invalid".into());
    let (state, _tmp) = append_state_with_user_row(&session_id).await;

    let base = SessionAppendMessageParams {
        session_id: session_id.clone(),
        role: "assistant".into(),
        content: "ok".into(),
        media: vec![],
        source: "external_record:test".into(),
        client_message_id: None,
        thread_id: None,
    };

    let cases: Vec<(&str, SessionAppendMessageParams)> = vec![
        (
            "invalid_session_id",
            SessionAppendMessageParams {
                session_id: SessionKey("   ".into()),
                ..base.clone()
            },
        ),
        (
            "invalid_role",
            SessionAppendMessageParams {
                role: "tool".into(),
                ..base.clone()
            },
        ),
        (
            "invalid_content",
            SessionAppendMessageParams {
                content: "   ".into(),
                ..base.clone()
            },
        ),
        (
            "invalid_source",
            SessionAppendMessageParams {
                source: "  ".into(),
                ..base.clone()
            },
        ),
        (
            "invalid_thread_id",
            SessionAppendMessageParams {
                role: "user".into(),
                thread_id: Some("t1".into()),
                ..base.clone()
            },
        ),
        (
            "invalid_media",
            SessionAppendMessageParams {
                media: vec!["  ".into()],
                ..base
            },
        ),
    ];
    for (kind, params) in cases {
        let (ws, mut rx) = ws_connection_for_test(8);
        handle_session_append_message(&ws, &state, None, None, "amx".into(), params).await;
        let frame = recv_rpc_json(&mut rx).await;
        assert_eq!(frame["error"]["code"], -32602, "case {kind}: {frame}");
        assert_eq!(frame["error"]["data"]["kind"], kind, "case {kind}: {frame}");
    }

    // Assistant record into a session whose only row is... a user row — the
    // derivation succeeds, so exercise the unbound-thread refusal with a
    // session that has NO user row.
    let fresh = SessionKey("local:append-empty".into());
    let tmp2 = tempfile::tempdir().expect("tempdir");
    let manager = octos_bus::SessionManager::open(tmp2.path()).expect("open");
    let state2 = Arc::new(AppState {
        sessions: Some(Arc::new(tokio::sync::Mutex::new(manager))),
        ..AppState::empty_for_tests()
    });
    let (ws, mut rx) = ws_connection_for_test(8);
    handle_session_append_message(
        &ws,
        &state2,
        None,
        None,
        "am3".into(),
        SessionAppendMessageParams {
            session_id: fresh.clone(),
            role: "assistant".into(),
            content: "orphan answer".into(),
            media: vec![],
            source: "external_record:test".into(),
            client_message_id: None,
            thread_id: None,
        },
    )
    .await;
    let frame = recv_rpc_json(&mut rx).await;
    assert_eq!(frame["error"]["data"]["kind"], "unbound_thread", "{frame}");
    // ...and an explicit thread_id rescues it (caller-supplied binding wins).
    let (ws, mut rx) = ws_connection_for_test(8);
    handle_session_append_message(
        &ws,
        &state2,
        None,
        None,
        "am4".into(),
        SessionAppendMessageParams {
            session_id: fresh.clone(),
            role: "assistant".into(),
            content: "orphan answer".into(),
            media: vec![],
            source: "external_record:test".into(),
            client_message_id: None,
            thread_id: Some("imported-thread".into()),
        },
    )
    .await;
    let frame = recv_rpc_json(&mut rx).await;
    assert_eq!(frame["result"]["seq"], 0, "{frame}");
    assert_eq!(frame["result"]["thread_id"], "imported-thread");
}

// ===== UPCR-2026-042 `session/append_message` context recording =====

/// An out-of-band record merges into the session's context view wherever it
/// exists: the live manager directly, otherwise the durable snapshot
/// (loaded, merged, persisted). The row carries the `external_record` source
/// kind and its committed seq either way.
#[test]
fn external_record_row_merges_into_live_and_snapshot_context_views() {
    let session_id = SessionKey::new("api", "context-external-record");
    let history = vec![test_message(MessageRole::User, "what is the capital?")];
    let dir = tempfile::tempdir().unwrap();

    let mut record = test_message(MessageRole::Assistant, "Kyoto.");
    record.source = Some("external_record:whiteboard".to_owned());

    // Live branch.
    let live = Arc::new(StdMutex::new(ContextManager::from_session_history(
        session_id.to_string(),
        None,
        &history,
    )));
    let registration = register_appui_session_context_manager(&session_id, &live);
    record_appui_context_manager_external_record(dir.path(), &session_id, &record, 1);
    let merged = live
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone();
    let item = merged
        .ledger_items()
        .iter()
        .find(|item| {
            matches!(
                &item.kind,
                crate::context_manager::TranscriptItemKind::AssistantFinal { content }
                    if content == "Kyoto."
            )
        })
        .expect("external record merged into the live manager");
    assert_eq!(
        item.source_ref
            .as_ref()
            .and_then(|source| source.source_seq),
        Some(1)
    );
    assert_eq!(
        item.source_ref
            .as_ref()
            .map(|source| source.source_event_kind.as_str()),
        Some("external_record")
    );
    drop(registration);
    // Release the test's own strong reference too — the registry holds the
    // manager as a Weak, so `live` here would still upgrade and route the
    // second record into the LIVE branch instead of the snapshot one.
    drop(live);

    // Snapshot branch: no live manager, but the live-branch call persisted a
    // durable snapshot — a second record must load-merge-persist it.
    let mut second = test_message(MessageRole::Assistant, "importer note");
    second.source = Some("external_record:import".to_owned());
    record_appui_context_manager_external_record(dir.path(), &session_id, &second, 2);
    let reloaded = load_context_manager_snapshot(dir.path(), &session_id.to_string())
        .unwrap()
        .expect("snapshot survived the snapshot-branch merge");
    assert!(
        reloaded.ledger_items().iter().any(|item| {
            matches!(
                &item.kind,
                crate::context_manager::TranscriptItemKind::AssistantFinal { content }
                    if content == "Kyoto."
            )
        }),
        "the first record is still in the reloaded snapshot"
    );
    assert!(
        reloaded.ledger_items().iter().any(|item| {
            matches!(
                &item.kind,
                crate::context_manager::TranscriptItemKind::AssistantFinal { content }
                    if content == "importer note"
            )
        }),
        "the snapshot-branch record landed in the durable snapshot"
    );
}

/// UPCR-2026-042 review: an external record committed while a turn is
/// running must not enter that turn's later model requests, must survive the
/// per-iteration canonical copy-back the in-flight bridge performs, and must
/// surface to the next turn's prompt.
#[test]
fn external_record_committed_mid_turn_defers_to_the_next_turn_prompt() {
    let session_id = SessionKey::new("api", "context-external-midturn");
    let history = vec![
        test_message(MessageRole::User, "old request"),
        test_message(MessageRole::Assistant, "old answer"),
    ];
    let manager = Arc::new(StdMutex::new(ContextManager::from_session_history(
        session_id.to_string(),
        None,
        &history,
    )));
    // Production registers the session runtime's canonical manager, so the
    // external-record handler routes into THIS Arc via the live branch.
    let registration = register_appui_session_context_manager(&session_id, &manager);
    let dir = tempfile::tempdir().unwrap();
    let request = |phase: PromptContextPhase, iteration: u32| PromptContextRequest {
        phase,
        iteration,
        provider_name: "test".to_string(),
        model_id: "large-context".to_string(),
        context_window: 16_000,
    };

    // Turn 1: the in-flight bridge builds its first prompt.
    let bridge = AppUiPromptContextBridge::new(
        session_id.clone(),
        dir.path().to_path_buf(),
        manager.clone(),
        false,
    );
    let mut prompt = vec![test_message(MessageRole::System, "runtime system")];
    prompt.extend(history.clone());
    prompt.push(test_message(MessageRole::User, "current request"));
    bridge
        .prepare_prompt(request(PromptContextPhase::TurnStart, 1), &mut prompt)
        .expect("turn start");

    // Mid-turn append, committed into the live canonical manager.
    let mut side_note = test_message(MessageRole::User, "mid-turn side note");
    side_note.source = Some("external_record:import".to_owned());
    record_appui_context_manager_external_record(dir.path(), &session_id, &side_note, 2);

    // A later iteration of the SAME turn must not pick the row up.
    bridge
        .prepare_prompt(request(PromptContextPhase::Iteration, 2), &mut prompt)
        .expect("iteration");
    assert!(
        !prompt
            .iter()
            .any(|message| message.content == "mid-turn side note"),
        "the in-flight turn must not see an externally appended row"
    );

    // ...and the copy-back must not drop the row from canonical either.
    assert!(
        manager
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .ledger_items()
            .iter()
            .any(|item| matches!(
                &item.kind,
                crate::context_manager::TranscriptItemKind::UserInput { content, .. }
                    if content == "mid-turn side note"
            )),
        "the deferred row must survive the in-flight copy-back"
    );

    drop(bridge);
    // Turn 2 is a fresh per-turn bridge over the same canonical manager.
    let next_turn = AppUiPromptContextBridge::new(
        session_id.clone(),
        dir.path().to_path_buf(),
        manager.clone(),
        false,
    );
    let mut prompt = vec![test_message(MessageRole::System, "runtime system")];
    prompt.extend(history);
    prompt.push(test_message(MessageRole::User, "next request"));
    next_turn
        .prepare_prompt(request(PromptContextPhase::TurnStart, 1), &mut prompt)
        .expect("turn 2 start");
    assert!(
        prompt.iter().any(|message| {
            message.role == MessageRole::User && message.content == "mid-turn side note"
        }),
        "the deferred row must surface to the next turn's prompt"
    );
    drop(registration);
}
