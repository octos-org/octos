use super::*;

fn visibility_features() -> ConnectionUiFeatures {
    ConnectionUiFeatures {
        session_workspace_cwd: true,
        ..ConnectionUiFeatures::default()
    }
}

async fn open_visibility_session(
    state: &Arc<AppState>,
    session_id: &SessionKey,
    workspace: Option<&std::path::Path>,
) -> SessionOpenOutcome {
    open_session_result(
        state,
        &UiProtocolLedger::new(16),
        &PendingApprovalStore::default(),
        &PendingQuestionStore::default(),
        ConnectionId::next(),
        Some("dev"),
        Some("dev"),
        visibility_features(),
        SessionOpenParams {
            session_id: session_id.clone(),
            topic: None,
            profile_id: Some("dev".to_owned()),
            cwd: workspace.map(|root| root.to_string_lossy().into_owned()),
            sandbox: None,
            after: None,
            client_commands: None,
            session_ref: None,
        },
    )
    .await
    .expect("session/open succeeds")
}

async fn list_visibility_sessions(
    state: &Arc<AppState>,
    workspace: Option<&std::path::Path>,
) -> serde_json::Value {
    let (ws, mut rx) = ws_connection_for_test(8);
    handle_session_list(
        &ws,
        state,
        &HeaderMap::new(),
        None,
        Some("dev"),
        false,
        visibility_features(),
        "visibility-list".to_owned(),
        SessionListParams {
            workspace_id: None,
            cursor: None,
            limit: None,
            cwd: workspace.map(|root| root.to_string_lossy().into_owned()),
            profile_id: Some("dev".to_owned()),
        },
    )
    .await;
    let response = recv_rpc_json(&mut rx).await;
    assert_eq!(response["error"], Value::Null, "{response}");
    response["result"].clone()
}

#[tokio::test(flavor = "current_thread")]
async fn first_open_and_prompt_are_visible_before_terminal_without_duplicates() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("project");
    std::fs::create_dir(&workspace).unwrap();
    let (mut state, _profile_runtime) = state_with_profile(temp.path(), "dev").await;
    Arc::get_mut(&mut state).unwrap().session_cache = Arc::new(
        crate::runtime::SessionRuntimeCache::new(8, std::time::Duration::from_secs(60))
            .with_sessions_in_cwd(true),
    );
    let session_id = SessionKey::with_profile("dev", "api", "first-approval");

    open_visibility_session(&state, &session_id, Some(&workspace)).await;
    let initial = list_visibility_sessions(&state, Some(&workspace)).await;
    assert_eq!(initial["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(initial["sessions"][0]["id"], json!(session_id.0));
    assert_eq!(initial["sessions"][0]["active_turn"], json!(false));
    assert_eq!(initial["profile_id"], json!("dev"));
    assert_eq!(
        std::fs::canonicalize(initial["workspace_root"].as_str().unwrap()).unwrap(),
        std::fs::canonicalize(&workspace).unwrap()
    );
    let global = list_visibility_sessions(&state, None).await;
    assert_eq!(
        global["sessions"].as_array().unwrap().len(),
        0,
        "a cwd-owned record must not leak into the legacy global list in either full or bare id form"
    );
    let other_workspace = temp.path().join("other-project");
    std::fs::create_dir(&other_workspace).unwrap();
    let other = list_visibility_sessions(&state, Some(&other_workspace)).await;
    assert_eq!(other["sessions"].as_array().unwrap().len(), 0);

    let runtime = state
        .session_cache
        .get_or_init(
            &state.profiles["dev"],
            session_id.clone(),
            Some(workspace.clone()),
        )
        .await
        .unwrap();
    let transcript_path = {
        let sessions = runtime.sessions.lock().await;
        sessions
            .data_dir()
            .join("users")
            .join(octos_bus::session::encode_path_component(
                session_id.base_key(),
            ))
            .join("sessions")
            .join("default.jsonl")
    };
    let before_reopen = std::fs::read(&transcript_path).unwrap();
    open_visibility_session(&state, &session_id, Some(&workspace)).await;
    assert_eq!(
        std::fs::read(&transcript_path).unwrap(),
        before_reopen,
        "reopen must not rewrite or duplicate the metadata record"
    );

    let prompt = pre_stamp_turn_thread_id(
        Message::user("First approval pending"),
        "first-approval-turn",
    );
    runtime
        .sessions
        .lock()
        .await
        .add_message_with_seq(&session_id, prompt)
        .await
        .unwrap();
    let registry = active_turns_registry();
    let (entry, _) = synthetic_active_turn(&TurnId::new(), true);
    registry.lock().await.insert(session_id.clone(), entry);
    let running = list_visibility_sessions(&state, Some(&workspace)).await;
    assert_eq!(running["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(
        running["sessions"][0]["title"],
        json!("First approval pending")
    );
    assert!(running["sessions"][0]["updated_at"].is_string());
    assert_eq!(running["sessions"][0]["active_turn"], json!(true));

    let response = pre_stamp_turn_thread_id(Message::assistant("done"), "first-approval-turn");
    runtime
        .sessions
        .lock()
        .await
        .add_message_with_seq(&session_id, response)
        .await
        .unwrap();
    registry.lock().await.remove(&session_id);
    let completed = list_visibility_sessions(&state, Some(&workspace)).await;
    assert_eq!(completed["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(completed["sessions"][0]["active_turn"], json!(false));

    runtime
        .sessions
        .lock()
        .await
        .clear(&session_id)
        .await
        .unwrap();
    let deleted = list_visibility_sessions(&state, Some(&workspace)).await;
    assert_eq!(deleted["sessions"].as_array().unwrap().len(), 0);
    assert!(!transcript_path.exists());
}

#[tokio::test(flavor = "current_thread")]
async fn first_open_global_record_uses_the_profile_store() {
    let temp = tempfile::tempdir().unwrap();
    let (state, _profile_runtime) = state_with_profile(temp.path(), "dev").await;
    let session_id = SessionKey::with_profile("dev", "api", "first-global-open");

    open_visibility_session(&state, &session_id, None).await;
    let runtime = state
        .session_cache
        .get_or_init(&state.profiles["dev"], session_id.clone(), None)
        .await
        .unwrap();
    let sessions = runtime.sessions.lock().await;
    let rows = sessions.list_top_level_sessions_with_meta();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, session_id.0);
    assert_eq!(rows[0].1, 1);
}

#[tokio::test(flavor = "current_thread")]
async fn rejected_first_open_does_not_leave_a_visible_record() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("project");
    std::fs::create_dir(&workspace).unwrap();
    let (mut state, _profile_runtime) = state_with_profile(temp.path(), "dev").await;
    Arc::get_mut(&mut state).unwrap().session_cache = Arc::new(
        crate::runtime::SessionRuntimeCache::new(8, std::time::Duration::from_secs(60))
            .with_sessions_in_cwd(true),
    );
    let session_id = SessionKey::with_profile("dev", "api", "bad-first-cursor");

    let error = open_session_result(
        &state,
        &UiProtocolLedger::new(16),
        &PendingApprovalStore::default(),
        &PendingQuestionStore::default(),
        ConnectionId::next(),
        Some("dev"),
        Some("dev"),
        visibility_features(),
        SessionOpenParams {
            session_id: session_id.clone(),
            topic: None,
            profile_id: Some("dev".to_owned()),
            cwd: Some(workspace.to_string_lossy().into_owned()),
            sandbox: None,
            after: Some(UiCursor {
                stream: "dev:api:another-stream".to_owned(),
                seq: 0,
            }),
            client_commands: None,
            session_ref: None,
        },
    )
    .await
    .expect_err("a foreign replay cursor rejects the open");
    assert_eq!(
        error.code,
        octos_core::ui_protocol::rpc_error_codes::CURSOR_INVALID
    );

    let listed = list_visibility_sessions(&state, Some(&workspace)).await;
    assert_eq!(listed["sessions"].as_array().unwrap().len(), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn forged_profile_open_cannot_create_a_cross_profile_record() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("project");
    std::fs::create_dir(&workspace).unwrap();
    let (mut state, _profile_runtime) = state_with_profile(temp.path(), "dev").await;
    Arc::get_mut(&mut state).unwrap().session_cache = Arc::new(
        crate::runtime::SessionRuntimeCache::new(8, std::time::Duration::from_secs(60))
            .with_sessions_in_cwd(true),
    );
    let session_id = SessionKey::with_profile("other", "api", "acl-escape");

    let error = open_session_result(
        &state,
        &UiProtocolLedger::new(16),
        &PendingApprovalStore::default(),
        &PendingQuestionStore::default(),
        ConnectionId::next(),
        Some("dev"),
        Some("dev"),
        visibility_features(),
        SessionOpenParams {
            session_id,
            topic: None,
            profile_id: Some("other".to_owned()),
            cwd: Some(workspace.to_string_lossy().into_owned()),
            sandbox: None,
            after: None,
            client_commands: None,
            session_ref: None,
        },
    )
    .await
    .expect_err("a dev connection cannot open an other-profile session");
    assert!(is_auth_scope_violation(&error), "{error:?}");
    let listed = list_visibility_sessions(&state, Some(&workspace)).await;
    assert_eq!(listed["sessions"].as_array().unwrap().len(), 0);
    let global = list_visibility_sessions(&state, None).await;
    assert_eq!(global["sessions"].as_array().unwrap().len(), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn reopen_after_external_delete_does_not_restore_cached_context() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("project");
    std::fs::create_dir(&workspace).unwrap();
    let (mut state, _profile_runtime) = state_with_profile(temp.path(), "dev").await;
    Arc::get_mut(&mut state).unwrap().session_cache = Arc::new(
        crate::runtime::SessionRuntimeCache::new(8, std::time::Duration::from_secs(60))
            .with_sessions_in_cwd(true),
    );
    let session_id = SessionKey::with_profile("dev", "api", "deleted-open-context");
    let runtime = state
        .session_cache
        .get_or_init(
            &state.profiles["dev"],
            session_id.clone(),
            Some(workspace.clone()),
        )
        .await
        .unwrap();
    let data_dir = {
        let mut sessions = runtime.sessions.lock().await;
        sessions
            .add_message(&session_id, Message::user("deleted context secret"))
            .await
            .unwrap();
        sessions.data_dir()
    };
    let mut canonical = octos_bus::SessionHandle::open(&data_dir, &session_id);
    canonical.clear().await.unwrap();
    assert_eq!(
        runtime
            .sessions
            .lock()
            .await
            .get_or_create(&session_id)
            .await
            .messages
            .len(),
        1,
        "the owning manager still holds the stale mirror used by this regression"
    );

    let outcome = open_session_result(
        &state,
        &UiProtocolLedger::new(16),
        &PendingApprovalStore::default(),
        &PendingQuestionStore::default(),
        ConnectionId::next(),
        Some("dev"),
        Some("dev"),
        ConnectionUiFeatures {
            context_lifecycle_v1: true,
            ..visibility_features()
        },
        SessionOpenParams {
            session_id: session_id.clone(),
            topic: None,
            profile_id: Some("dev".to_owned()),
            cwd: Some(workspace.to_string_lossy().into_owned()),
            sandbox: None,
            after: None,
            client_commands: None,
            session_ref: None,
        },
    )
    .await
    .expect("deleted session can be reopened as new");
    let context = outcome.result.opened.context_state.unwrap();
    assert_eq!(context.item_count, 0);
    assert!(
        context.token_estimate <= 1,
        "an empty context carries only its fixed accounting overhead"
    );
    assert!(
        runtime
            .sessions
            .lock()
            .await
            .get_or_create(&session_id)
            .await
            .messages
            .is_empty()
    );
    let listed = list_visibility_sessions(&state, Some(&workspace)).await;
    assert_eq!(listed["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(listed["sessions"][0]["title"], Value::Null);
}
