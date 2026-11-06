
fn dev_profile_with_data_dir(tmp: &std::path::Path) -> crate::profiles::UserProfile {
    crate::profiles::UserProfile {
        id: "dev".to_string(),
        name: "Dev".to_string(),
        enabled: true,
        data_dir: Some(tmp.join("data").to_string_lossy().into_owned()),
        parent_id: None,
        public_subdomain: None,
        config: Default::default(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
    }
}

fn session_info_for(id: &str) -> crate::api::handlers::SessionInfo {
    crate::api::handlers::SessionInfo {
        id: id.to_owned(),
        message_count: 0,
        title: Some("ref test".to_owned()),
        updated_at: None,
        last_prompt: None,
        active_turn: false,
    }
}

/// #2890 follow-up gap 1: opening by `session_ref` must subscribe the live
/// forwarder to the RESOLVED session identity, not the empty `session_id`
/// the request carried — otherwise title/status/message/terminal events on
/// the real session never reach this connection.
#[tokio::test(flavor = "current_thread")]
async fn session_ref_open_subscribes_live_events_to_the_resolved_session() {
    let tmp = tempfile::tempdir().unwrap();
    let mut state = session_list_state_with_dev_runtime(tmp.path()).await;
    let store =
        std::sync::Arc::new(crate::profiles::ProfileStore::open_unified(tmp.path()).unwrap());
    store.save(&dev_profile_with_data_dir(tmp.path())).unwrap();
    state.profile_store = Some(store);
    state.workspace_allowed_roots = vec![tmp.path().to_owned()];
    let state = Arc::new(state);

    let session = SessionKey::with_profile("dev", "api", "ref-sub");
    let record = crate::api::workspace_registry::ensure_session_ref(
        &state,
        "dev",
        &session,
        None,
        &session_info_for(&session.0),
        false,
    )
    .unwrap();

    let ledger = Arc::new(UiProtocolLedger::new(64));
    let forwarders: SharedLiveForwarders = Arc::new(TokioMutex::new(HashMap::new()));
    let contracts = UiProtocolContractStores::default();
    let (ws, mut rx) = ws_connection_for_test(64);
    let features = ConnectionUiFeatures {
        workspace_session_refs_v1: true,
        ..ConnectionUiFeatures::stdio_defaults()
    };
    let opened = handle_session_open(
        &ws,
        &state,
        &ledger,
        &contracts.approvals,
        &contracts.user_questions,
        &forwarders,
        Some("dev"),
        None,
        features,
        "open-ref".into(),
        SessionOpenParams {
            session_ref: Some(record.session_ref.clone()),
            client_commands: None,
            session_id: SessionKey(String::new()),
            topic: None,
            profile_id: None,
            cwd: None,
            sandbox: None,
            after: None,
        },
        false,
    )
    .await;
    assert!(opened, "session/open by ref must succeed");

    // Append a LIVE event on the resolved session AFTER the open completed:
    // the forwarder must push it to this connection.
    ledger.append_notification(UiNotification::ProgressUpdated(UiProgressEvent::new(
        session.clone(),
        None,
        UiProgressMetadata::new("ref-live-marker"),
    )));
    let frame = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let frame = recv_rpc_json(&mut rx).await;
            if frame.get("method").and_then(Value::as_str) == Some("progress/updated")
                && frame["params"]["session_id"].as_str() == Some(session.0.as_str())
            {
                break frame;
            }
        }
    })
    .await
    .expect("live event on the resolved session must reach the ref-opened connection");
    assert_eq!(frame["params"]["session_id"], session.0.as_str());
}

/// #2890 follow-up gap 2: an admin connection with NO `_main` profile must
/// still resolve refs owned by other profiles and list workspaces, instead
/// of aborting on the missing `_main` runtime/store.
#[test]
fn session_ref_resolution_and_workspace_list_survive_missing_main_profile() {
    let tmp = tempfile::tempdir().unwrap();
    let mut state = AppState::empty_for_tests();
    state.workspace_allowed_roots = vec![tmp.path().to_owned()];
    let store =
        std::sync::Arc::new(crate::profiles::ProfileStore::open_unified(tmp.path()).unwrap());
    store.save(&dev_profile_with_data_dir(tmp.path())).unwrap();
    state.profile_store = Some(store);
    let state = Arc::new(state);

    let session = SessionKey::with_profile("dev", "api", "ref-no-main");
    let record = crate::api::workspace_registry::ensure_session_ref(
        &state,
        "dev",
        &session,
        None,
        &session_info_for(&session.0),
        false,
    )
    .unwrap();

    // Unscoped (admin) resolution must not abort on the missing `_main`.
    let resolved = crate::api::workspace_registry::resolve_session_ref(
        &state,
        None,
        &record.session_ref,
    )
    .expect("admin without _main must resolve another profile's session_ref");
    assert_eq!(resolved.session_id, session.0);

    // workspace/list for the absent default profile degrades to empty, not an
    // error that breaks the admin's listing.
    let workspaces = crate::api::workspace_registry::list_workspaces(&state, MAIN_PROFILE_ID)
        .expect("workspace/list without _main must not error");
    assert!(workspaces.is_empty());
}

/// #2890 follow-up gap 3: `session/delete` must invalidate the session_ref —
/// resolving or opening by the old ref afterwards fails with the existing
/// not_found semantics instead of resurrecting the deleted session.
#[tokio::test(flavor = "current_thread")]
async fn session_delete_invalidates_session_ref_resolution() {
    let tmp = tempfile::tempdir().unwrap();
    let mut state = session_list_state_with_dev_runtime(tmp.path()).await;
    let store =
        std::sync::Arc::new(crate::profiles::ProfileStore::open_unified(tmp.path()).unwrap());
    store.save(&dev_profile_with_data_dir(tmp.path())).unwrap();
    state.profile_store = Some(store);
    state.sessions = Some(Arc::new(TokioMutex::new(
        octos_bus::SessionManager::open(&tmp.path().join("data")).unwrap(),
    )));
    state.workspace_allowed_roots = vec![tmp.path().to_owned()];
    let state = Arc::new(state);

    // Production-shaped delete: the session was opened under `dev`, so the
    // delete is routed through the dev profile header.
    let session = SessionKey::with_profile("dev", "api", "ref-del");
    let record = crate::api::workspace_registry::ensure_session_ref(
        &state,
        "dev",
        &session,
        None,
        &session_info_for(&session.0),
        false,
    )
    .unwrap();

    let mut headers = HeaderMap::new();
    headers.insert("x-profile-id", "dev".parse().unwrap());
    let response = crate::api::handlers::delete_session(
        axum::extract::State(state.clone()),
        headers,
        None,
        axum::extract::Path("ref-del".to_owned()),
    )
    .await;
    assert!(response.status().is_success(), "delete must succeed: {response:?}");

    let error = crate::api::workspace_registry::resolve_session_ref(
        &state,
        Some("dev"),
        &record.session_ref,
    )
    .expect_err("deleted session's ref must no longer resolve");
    assert_eq!(error.data.unwrap()["kind"], "session_not_found");
}

/// Scoped delete: removing a session_ref for one profile must not unbind a
/// same-named session in another profile (no silent cross-tenant unbinding).
#[test]
fn remove_session_refs_is_scoped_to_its_profile() {
    let tmp = tempfile::tempdir().unwrap();
    let mut state = AppState::empty_for_tests();
    state.workspace_allowed_roots = vec![tmp.path().to_owned()];
    let store =
        std::sync::Arc::new(crate::profiles::ProfileStore::open_unified(tmp.path()).unwrap());
    store.save(&dev_profile_with_data_dir(tmp.path())).unwrap();
    let second_profile = crate::profiles::UserProfile {
        id: "dev2".to_string(),
        // A separate data dir per profile (the production layout): each has
        // its own registry file and its own ref for the same-named session.
        data_dir: Some(tmp.path().join("data2").to_string_lossy().into_owned()),
        ..dev_profile_with_data_dir(tmp.path())
    };
    store.save(&second_profile).unwrap();
    state.profile_store = Some(store);
    let state = Arc::new(state);

    let session = SessionKey::new("api", "shared-id");
    let dev_ref = crate::api::workspace_registry::ensure_session_ref(
        &state,
        "dev",
        &session,
        None,
        &session_info_for(&session.0),
        false,
    )
    .unwrap();
    let main_ref = crate::api::workspace_registry::ensure_session_ref(
        &state,
        "dev2",
        &session,
        None,
        &session_info_for(&session.0),
        false,
    )
    .unwrap();

    crate::api::workspace_registry::remove_session_refs(&state, "dev", &session.0).unwrap();
    assert!(
        crate::api::workspace_registry::resolve_session_ref(&state, Some("dev"), &dev_ref.session_ref)
            .is_err(),
        "dev's ref must be gone after its session was deleted"
    );
    assert!(
        crate::api::workspace_registry::resolve_session_ref(&state, Some("dev2"), &main_ref.session_ref)
            .is_ok(),
        "the other profile's same-named session must keep its ref"
    );
}
