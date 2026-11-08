use super::*;

#[tokio::test]
async fn session_mutation_listed_cold_session_uses_its_project_store() {
    let tmp = tempfile::tempdir().unwrap();
    let state = Arc::new(session_list_state_with_dev_runtime(tmp.path()).await);
    let workspace = tmp.path().join("project");
    std::fs::create_dir_all(&workspace).unwrap();
    let scope = SessionListScope {
        workspace_root: workspace.clone(),
        sessions_root: crate::runtime::session::project_sessions_root(&workspace, "dev"),
        profile_id: "dev".to_owned(),
    };
    let key = SessionKey::with_profile("dev", "api", &format!("cold-{}", uuid::Uuid::new_v4()));
    let mut project = octos_bus::SessionManager::open(&scope.sessions_root).unwrap();
    project
        .add_message(&key, octos_core::Message::user("project history"))
        .await
        .unwrap();
    let rows = crate::api::handlers::list_profile_sessions(
        &scope.sessions_root,
        &Default::default(),
        "dev",
    );
    assert_eq!(rows.len(), 1);
    assert!(session_workspaces().get("dev", &key).is_none());
    remember_listed_session_workspaces(&serde_json::to_value(rows).unwrap(), &scope);

    let response = crate::api::handlers::update_session_title(
        axum::extract::State(state.clone()),
        HeaderMap::new(),
        Some(axum::Extension(AuthIdentity::Admin)),
        axum::extract::Path(key.0.clone()),
        axum::Json(crate::api::handlers::UpdateTitleRequest {
            title: "renamed without opening".to_owned(),
        }),
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::NO_CONTENT);
    assert_eq!(
        project.load(&key).await.unwrap().title.as_deref(),
        Some("renamed without opening")
    );
    let response = crate::api::handlers::delete_session(
        axum::extract::State(state),
        HeaderMap::new(),
        Some(axum::Extension(AuthIdentity::Admin)),
        axum::extract::Path(key.0.clone()),
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::NO_CONTENT);
    assert!(project.load(&key).await.is_none());
}

fn snapshot_session_files(
    root: &std::path::Path,
) -> std::collections::BTreeMap<std::path::PathBuf, Vec<u8>> {
    fn visit(
        root: &std::path::Path,
        dir: &std::path::Path,
        out: &mut std::collections::BTreeMap<std::path::PathBuf, Vec<u8>>,
    ) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.map(|entry| entry.unwrap()) {
            let path = entry.path();
            if path.is_dir() {
                visit(root, &path, out);
            } else if path.is_file() {
                out.insert(
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    std::fs::read(&path).unwrap(),
                );
            }
        }
    }

    let mut out = std::collections::BTreeMap::new();
    visit(root, root, &mut out);
    out
}

async fn recv_rpc_id(
    rx: &mut tokio::sync::mpsc::Receiver<axum::extract::ws::Message>,
    id: &str,
) -> serde_json::Value {
    loop {
        let frame = recv_rpc_json(rx).await;
        if frame["id"] == json!(id) {
            return frame;
        }
    }
}

/// Deleting a session must not make an already-active turn uninterruptible:
/// the handler checks the live registry before applying the idle not-found
/// mapping (UPCR-2026-RF6720565).
#[tokio::test]
async fn deleted_session_active_turn_interrupt_remains_captured() {
    let tmp = tempfile::tempdir().unwrap();
    let key = SessionKey::new("api", "deleted-active-turn");
    let mut sessions = octos_bus::SessionManager::open(tmp.path()).unwrap();
    sessions
        .add_message(&key, octos_core::Message::user("start"))
        .await
        .unwrap();
    sessions.clear_existing(&key).await.unwrap();
    let before = snapshot_session_files(tmp.path());

    let active_turns: SharedActiveTurns = Arc::new(TokioMutex::new(HashMap::new()));
    let turn_id = TurnId::new();
    let abort = tokio::spawn(async {}).abort_handle();
    active_turns
        .lock()
        .await
        .insert(key.clone(), test_active_turn(turn_id.clone(), abort));
    let outcome = decide_interrupt(
        &active_turns,
        &TurnInterruptParams {
            session_id: key,
            turn_id,
        },
    )
    .await;
    assert!(matches!(outcome, InterruptOutcome::Captured { .. }));
    assert_eq!(snapshot_session_files(tmp.path()), before);
}

/// T-0461 / UPCR-2026-RF6720565 / selftest cleanup.json: deleting a session is
/// final for that incarnation. Later open/status/interrupt return typed
/// not-found without recreating discovery files; a foreign profile is
/// independent.
#[tokio::test]
async fn deleted_session_rpc_is_not_found_without_reviving_discovery() {
    let tmp = tempfile::tempdir().unwrap();
    let mut state = session_list_state_with_dev_runtime(tmp.path()).await;
    let other = crate::profiles::UserProfile {
        id: "other".into(),
        name: "Other".into(),
        enabled: true,
        data_dir: None,
        parent_id: None,
        public_subdomain: None,
        config: crate::profiles::ProfileConfig {
            llm: Some(crate::profiles::LlmProfileConfig {
                primary: Some(crate::profiles::LlmModelSelectionConfig {
                    family_id: Some("openai".into()),
                    model_id: Some("gpt-4o-mini".into()),
                    route: Some(crate::profiles::LlmRouteConfig {
                        route_id: None,
                        label: None,
                        base_url: None,
                        api_key_env: Some("SESS_CWD_TEST_KEY".into()),
                        api_type: None,
                    }),
                    ..Default::default()
                }),
                fallbacks: Vec::new(),
            }),
            env_vars: [("SESS_CWD_TEST_KEY".into(), "test-key".into())]
                .into_iter()
                .collect(),
            ..Default::default()
        },
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    let other_data = tmp.path().join("other-data");
    std::fs::create_dir_all(&other_data).unwrap();
    let other_runtime = crate::runtime::ProfileRuntime::bootstrap(
        &other,
        &other_data,
        None,
        crate::runtime::BootstrapRole::Serve,
    )
    .await
    .unwrap();
    state.profiles.insert("other".into(), other_runtime);
    let state = Arc::new(state);

    let key = SessionKey::with_profile("dev", "api", "deleted-cleanup");
    let foreign_key = SessionKey::with_profile("other", "api", "deleted-cleanup");
    let (ws, mut rx) = ws_connection_for_test(32);
    let ledger = Arc::new(UiProtocolLedger::new(16));
    let approvals = PendingApprovalStore::default();
    let questions = PendingQuestionStore::default();
    let forwarders: SharedLiveForwarders = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    let features = ConnectionUiFeatures::default();
    let open_params = SessionOpenParams {
        session_ref: None,
        client_commands: None,
        session_id: key.clone(),
        topic: None,
        profile_id: Some("dev".into()),
        cwd: None,
        sandbox: None,
        after: None,
    };
    assert!(
        handle_session_open(
            &ws,
            &state,
            &ledger,
            &approvals,
            &questions,
            &forwarders,
            Some("dev"),
            None,
            features,
            "open-before-delete".into(),
            open_params.clone(),
            false,
        )
        .await
    );
    assert!(recv_rpc_id(&mut rx, "open-before-delete").await["error"].is_null());

    handle_session_delete(
        &ws,
        &state,
        &HeaderMap::new(),
        Some(&AuthIdentity::Admin),
        &forwarders,
        &Arc::new(UiProtocolLedger::new(256)),
        "delete".into(),
        SessionDeleteParams {
            session_id: key.0.clone(),
        },
    )
    .await;
    let delete_frame = recv_rpc_id(&mut rx, "delete").await;
    assert!(
        delete_frame["error"].is_null(),
        "delete failed: {delete_frame}"
    );

    let sessions = resolve_sessions_for_lookup(&state, Some("dev"), None, &key)
        .await
        .unwrap();
    let sessions_root = sessions.lock().await.sessions_dir().to_path_buf();
    let before = snapshot_session_files(&sessions_root);
    assert!(sessions.lock().await.load(&key).await.is_none());

    let status = raw_session_status_result(
        &state,
        &RpcRequest::<Value>::new(
            "status-deleted",
            APPUI_METHOD_SESSION_STATUS_READ,
            json!({ "session_id": key.0, "profile_id": "dev" }),
        ),
        features,
        Some("dev"),
    )
    .await;
    let status_kind = match &status {
        Ok(_) => "ok".to_owned(),
        Err(error) => error.data.as_ref().unwrap()["kind"]
            .as_str()
            .unwrap()
            .to_owned(),
    };

    let active_turns: SharedActiveTurns = Arc::new(TokioMutex::new(HashMap::new()));
    let contracts = Arc::new(UiProtocolContractStores::default());
    handle_turn_interrupt(
        &ws,
        &state,
        &ledger,
        &active_turns,
        &contracts,
        Some("dev"),
        "interrupt-deleted".into(),
        TurnInterruptParams {
            session_id: key.clone(),
            turn_id: TurnId::new(),
        },
    )
    .await;
    let interrupt_frame = recv_rpc_id(&mut rx, "interrupt-deleted").await;
    let interrupt_kind = interrupt_frame["error"]["data"]["kind"]
        .as_str()
        .unwrap_or("ok")
        .to_owned();

    assert!(
        !handle_session_open(
            &ws,
            &state,
            &ledger,
            &approvals,
            &questions,
            &forwarders,
            Some("dev"),
            None,
            features,
            "open-deleted".into(),
            open_params,
            false,
        )
        .await
    );
    let reopen_frame = recv_rpc_id(&mut rx, "open-deleted").await;
    let reopen_kind = reopen_frame["error"]["data"]["kind"]
        .as_str()
        .unwrap_or("ok")
        .to_owned();

    let foreign_status = raw_session_status_result(
        &state,
        &RpcRequest::<Value>::new(
            "status-foreign",
            APPUI_METHOD_SESSION_STATUS_READ,
            json!({ "session_id": foreign_key.0, "profile_id": "other" }),
        ),
        features,
        Some("other"),
    )
    .await;
    let after = snapshot_session_files(&sessions_root);
    let record_revived = sessions.lock().await.load(&key).await.is_some();
    let observed = json!({
        "status": status_kind,
        "interrupt": interrupt_kind,
        "reopen": reopen_kind,
        "discovery_tree_unchanged": before == after,
        "record_revived": record_revived,
        "foreign_profile_unaffected": foreign_status.is_ok(),
    });
    assert_eq!(
        observed,
        json!({
            "status": "unknown_session",
            "interrupt": "unknown_session",
            "reopen": "unknown_session",
            "discovery_tree_unchanged": true,
            "record_revived": false,
            "foreign_profile_unaffected": true,
        })
    );
}

#[test]
fn session_mutation_listing_cannot_overwrite_live_workspace_or_bind_foreign_keys() {
    let suffix = uuid::Uuid::new_v4();
    let key = SessionKey::with_profile("dev", "api", &format!("live-{suffix}"));
    let live = std::path::PathBuf::from("/live");
    session_workspaces().set("dev", key.clone(), live.clone());
    let scope = SessionListScope {
        workspace_root: std::path::PathBuf::from("/listed"),
        sessions_root: std::path::PathBuf::from("/listed/.octos/dev"),
        profile_id: "dev".to_owned(),
    };
    let foreign = SessionKey::with_profile("other", "api", &format!("foreign-{suffix}"));
    let channel = SessionKey::with_profile("dev", "telegram", &format!("channel-{suffix}"));
    let topic = SessionKey::with_profile_topic("dev", "api", &format!("topic-{suffix}"), "review");
    remember_listed_session_workspaces(
        &json!([
            {"id": key.0}, {"id": foreign.0}, {"id": channel.0},
            {"id": format!("topic-{suffix}#review")}, {"id": ""}
        ]),
        &scope,
    );
    assert_eq!(session_workspaces().get("dev", &key), Some(live));
    assert!(session_workspaces().get("dev", &foreign).is_none());
    assert!(session_workspaces().get("other", &foreign).is_none());
    assert!(session_workspaces().get("dev", &channel).is_none());
    assert_eq!(
        session_workspaces().runtime_hint("dev", &topic),
        Some(scope.workspace_root)
    );
}
