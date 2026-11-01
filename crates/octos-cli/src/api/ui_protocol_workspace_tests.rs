use super::*;

#[tokio::test]
async fn session_list_cwd_root_should_honor_requested_profile_when_connection_is_admin() {
    // A browser paired with the admin bearer token has NO connection profile
    // (`AuthIdentity::Admin` → `None`), yet it opens sessions as `dev` by
    // passing `profile_id` to session/open. Before this test the cwd listing
    // ignored `params.profile_id`, fell back to `_main`, and — on a local
    // install with only a `dev` profile — failed with
    // `cwd_runtime_unavailable`, so the history browser never saw the
    // transcripts that session/open had written under `<cwd>/.octos/dev`.
    use octos_core::ui_protocol::SessionListParams;

    let tmp = tempfile::tempdir().unwrap();
    let state = session_list_state_with_dev_runtime(tmp.path()).await;
    let cap = ConnectionUiFeatures::stdio_defaults();
    let project = tmp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let project_canon = std::fs::canonicalize(&project).unwrap();
    let cwd = project.to_string_lossy().into_owned();

    // Admin connection + explicit `profile_id` → that profile's store.
    let requested = SessionListParams {
        workspace_id: None,
        cursor: None,
        limit: None,
        cwd: Some(cwd.clone()),
        profile_id: Some("dev".to_string()),
    };
    assert_eq!(
        resolve_session_list_cwd_root(&state, cap, None, &requested).unwrap(),
        Some(SessionListScope {
            workspace_root: project_canon.clone(),
            sessions_root: project_canon.join(".octos").join("dev"),
            profile_id: "dev".to_string(),
        }),
    );

    // Admin connection + no `profile_id` → unchanged: `_main` is not
    // registered here, so the safety gate still rejects.
    let bare = SessionListParams {
        workspace_id: None,
        cursor: None,
        limit: None,
        cwd: Some(cwd.clone()),
        profile_id: None,
    };
    assert!(resolve_session_list_cwd_root(&state, cap, None, &bare).is_err());

    // Empty `profile_id` is rejected, same as session/open.
    let empty = SessionListParams {
        workspace_id: None,
        cursor: None,
        limit: None,
        cwd: Some(cwd.clone()),
        profile_id: Some(String::new()),
    };
    assert!(resolve_session_list_cwd_root(&state, cap, None, &empty).is_err());

    // A requested profile that is not registered is still rejected — the
    // param must never open a SessionManager at a made-up namespace.
    let unknown = SessionListParams {
        workspace_id: None,
        cursor: None,
        limit: None,
        cwd: Some(cwd),
        profile_id: Some("ghost".to_string()),
    };
    assert!(resolve_session_list_cwd_root(&state, cap, None, &unknown).is_err());
}

#[tokio::test]
async fn session_list_cwd_root_honors_flag_and_capability() {
    // The `session/list` cwd gate: flag OFF ignores cwd (legacy listing);
    // flag ON honors it only with the `session.workspace_cwd.v1`
    // capability; and it runs the SAME workspace-safety gate as
    // session/open before resolving a root (proven here by the no-runtime
    // rejection — see `session_list_cwd_root_gates_path_and_namespaces_by_profile`
    // for the safe-path happy case + banned-path rejection).
    use octos_core::ui_protocol::SessionListParams;

    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("proj");
    std::fs::create_dir_all(&cwd).unwrap();

    let has_cap = ConnectionUiFeatures::stdio_defaults(); // workspace_cwd = true
    let no_cap = ConnectionUiFeatures {
        workspace_session_refs_v1: false,
        session_workspace_cwd: false,
        ..ConnectionUiFeatures::stdio_defaults()
    };
    let with_cwd = SessionListParams {
        workspace_id: None,
        cursor: None,
        limit: None,
        cwd: Some(cwd.to_string_lossy().into_owned()),
        profile_id: None,
    };
    let no_cwd = SessionListParams {
        workspace_id: None,
        cursor: None,
        limit: None,
        cwd: None,
        profile_id: None,
    };

    let state_off = {
        let mut s = AppState::empty_for_tests();
        s.session_cache = Arc::new(
            crate::runtime::SessionRuntimeCache::new(4, std::time::Duration::from_secs(60))
                .with_sessions_in_cwd(false),
        );
        s
    };
    // Flag OFF → cwd ignored even with the capability (byte-identical legacy).
    assert_eq!(
        resolve_session_list_cwd_root(&state_off, has_cap, None, &with_cwd).unwrap(),
        None,
    );

    let state_on = {
        let mut s = AppState::empty_for_tests();
        s.session_cache = Arc::new(
            crate::runtime::SessionRuntimeCache::new(4, std::time::Duration::from_secs(60))
                .with_sessions_in_cwd(true),
        );
        s
    };
    // Flag ON + no cwd → None (legacy listing).
    assert_eq!(
        resolve_session_list_cwd_root(&state_on, has_cap, None, &no_cwd).unwrap(),
        None,
    );
    // Flag ON + cwd but NO capability → typed error (mirrors session/open).
    assert!(resolve_session_list_cwd_root(&state_on, no_cap, None, &with_cwd).is_err());
    // Flag ON + capability + a valid cwd but NO registered profile runtime
    // → the workspace-safety gate (`validate_session_workspace_allowed`)
    // rejects. This proves the gate is wired BEFORE any SessionManager is
    // opened at the cwd (the P1 the codex review flagged).
    assert!(resolve_session_list_cwd_root(&state_on, has_cap, None, &with_cwd).is_err());
}

#[tokio::test]
async fn session_list_cwd_root_gates_path_and_namespaces_by_profile() {
    // With a registered profile runtime: a SAFE cwd resolves to the
    // per-project, per-PROFILE store `<cwd>/.octos/<profile_id>` (so two
    // profiles sharing a cwd can't read each other's transcripts), and a
    // banned system path is rejected by the shared safety gate.
    use octos_core::ui_protocol::SessionListParams;

    let tmp = tempfile::tempdir().unwrap();
    let state = session_list_state_with_dev_runtime(tmp.path()).await;
    let cap = ConnectionUiFeatures::stdio_defaults();

    // Safe cwd → `<cwd>/.octos/dev` (profile-namespaced).
    let good = tmp.path().join("project");
    std::fs::create_dir_all(&good).unwrap();
    let good_canon = std::fs::canonicalize(&good).unwrap();
    let good_params = SessionListParams {
        workspace_id: None,
        cursor: None,
        limit: None,
        cwd: Some(good.to_string_lossy().into_owned()),
        profile_id: None,
    };
    let resolved = resolve_session_list_cwd_root(&state, cap, Some("dev"), &good_params).unwrap();
    assert_eq!(
        resolved,
        Some(SessionListScope {
            workspace_root: good_canon.clone(),
            sessions_root: crate::runtime::session::project_sessions_root(&good_canon, "dev"),
            profile_id: "dev".to_string(),
        }),
    );
    assert_eq!(
        resolved.map(|scope| scope.sessions_root),
        Some(good_canon.join(".octos").join("dev"))
    );

    // Banned system root (`/usr` is a real dir on Linux and macOS that
    // canonicalizes to `/usr`) → rejected by the safety gate.
    let banned = SessionListParams {
        workspace_id: None,
        cursor: None,
        limit: None,
        cwd: Some("/usr".to_string()),
        profile_id: Some("dev".to_string()),
    };
    assert!(resolve_session_list_cwd_root(&state, cap, Some("dev"), &banned).is_err());
}

fn workspace_features() -> ConnectionUiFeatures {
    ConnectionUiFeatures::from_requested_feature_tokens(
        [octos_core::ui_protocol::UI_PROTOCOL_FEATURE_WORKSPACE_SESSION_REFS_V1],
        true,
    )
}

fn profile_catalog_state(root: &std::path::Path, profiles: &[&str]) -> AppState {
    let mut state = AppState::empty_for_tests();
    state.workspace_allowed_roots = vec![root.to_owned()];
    let store = std::sync::Arc::new(crate::profiles::ProfileStore::open_unified(root).unwrap());
    for profile_id in profiles {
        store
            .save(&crate::profiles::UserProfile {
                id: (*profile_id).to_owned(),
                name: (*profile_id).to_owned(),
                public_subdomain: None,
                enabled: true,
                data_dir: Some(
                    root.join("profile-data")
                        .join(profile_id)
                        .to_string_lossy()
                        .to_string(),
                ),
                parent_id: None,
                config: Default::default(),
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            })
            .unwrap();
    }
    state.profile_store = Some(store);
    state.sessions = Some(Arc::new(tokio::sync::Mutex::new(
        octos_bus::SessionManager::open(root).unwrap(),
    )));
    state
}

fn catalog_info(id: &str, updated_at: &str) -> crate::api::handlers::SessionInfo {
    crate::api::handlers::SessionInfo {
        id: id.to_owned(),
        message_count: 1,
        title: Some(id.to_owned()),
        updated_at: Some(updated_at.to_owned()),
        last_prompt: None,
        active_turn: false,
    }
}

#[test]
fn workspace_session_refs_require_explicit_stdio_negotiation() {
    assert!(!ConnectionUiFeatures::stdio_defaults().workspace_session_refs_v1);
    assert!(workspace_features().workspace_session_refs_v1);
}

#[test]
fn session_ingress_ref_cannot_be_widened_with_a_topic() {
    let params = SessionOpenParams {
        session_id: SessionKey(String::new()),
        session_ref: Some("sr_bound".to_owned()),
        topic: Some("other-topic".to_owned()),
        profile_id: None,
        cwd: None,
        sandbox: None,
        after: None,
        client_commands: None,
    };
    let command = UiCommand::SessionOpen(params.clone());
    assert!(
        validate_session_ingress_command_scope(&command, &SessionKey("api:bound".into())).is_err()
    );

    let command = UiCommand::SessionOpen(SessionOpenParams {
        topic: None,
        ..params
    });
    validate_session_ingress_command_scope(&command, &SessionKey("api:bound".into())).unwrap();
}

#[tokio::test]
async fn external_connection_cannot_list_session_ref_catalog() {
    let state = Arc::new(AppState::empty_for_tests());
    let (ws, mut rx) = ws_connection_for_test(4);
    handle_session_list(
        &ws,
        &state,
        &HeaderMap::new(),
        None,
        Some("main"),
        true,
        workspace_features(),
        "external-list".to_owned(),
        SessionListParams::default(),
    )
    .await;
    let response = recv_rpc_json(&mut rx).await;
    assert_eq!(
        response["error"]["data"]["kind"],
        json!("external_method_denied")
    );
}

#[tokio::test]
async fn session_create_rejects_a_profile_outside_the_connection() {
    let state = Arc::new(AppState::empty_for_tests());
    let contracts = UiProtocolContractStores::default();
    let (ws, mut rx) = ws_connection_for_test(4);
    handle_session_create(
        &ws,
        &state,
        &Arc::new(UiProtocolLedger::new(4)),
        &contracts.approvals,
        &contracts.user_questions,
        &SharedLiveForwarders::default(),
        Some("main"),
        Some("main"),
        workspace_features(),
        "create-cross-profile".to_owned(),
        octos_core::ui_protocol::SessionCreateParams {
            workspace_id: "ws_any".to_owned(),
            profile_id: Some("other".to_owned()),
        },
    )
    .await;
    let response = recv_rpc_json(&mut rx).await;
    assert_eq!(
        response["error"]["data"]["kind"],
        json!("auth_scope_violation")
    );
}

#[tokio::test]
async fn session_open_rejects_a_ref_from_another_profile() {
    let temp = tempfile::tempdir().unwrap();
    let state = Arc::new(profile_catalog_state(temp.path(), &["main", "other"]));
    let info = catalog_info("api:other", "2026-01-02T03:04:05Z");
    let reference = crate::api::workspace_registry::ensure_session_ref(
        &state,
        "other",
        &SessionKey("api:other".to_owned()),
        None,
        &info,
        false,
    )
    .unwrap()
    .session_ref;
    let error = open_session_result(
        &state,
        &UiProtocolLedger::new(4),
        &PendingApprovalStore::default(),
        &PendingQuestionStore::default(),
        ConnectionId::next(),
        Some("main"),
        None,
        workspace_features(),
        SessionOpenParams {
            session_id: SessionKey(String::new()),
            session_ref: Some(reference),
            topic: None,
            profile_id: None,
            cwd: None,
            sandbox: None,
            after: None,
            client_commands: None,
        },
    )
    .await
    .unwrap_err();
    assert_eq!(error.data.unwrap()["kind"], json!("session_not_found"));
}

#[tokio::test]
async fn session_ref_dispatch_paginates_by_key_without_rewriting_unchanged_rows() {
    let temp = tempfile::tempdir().unwrap();
    let state = Arc::new(profile_catalog_state(temp.path(), &["main"]));
    for (id, updated_at) in [
        ("api:newest", "2026-01-04T00:00:00Z"),
        ("api:middle", "2026-01-03T00:00:00Z"),
        ("api:oldest", "2026-01-02T00:00:00Z"),
    ] {
        crate::api::workspace_registry::ensure_session_ref(
            &state,
            "main",
            &SessionKey(id.to_owned()),
            None,
            &catalog_info(id, updated_at),
            false,
        )
        .unwrap();
    }
    let (ws, mut rx) = ws_connection_for_test(8);
    handle_session_ref_list(
        &ws,
        &state,
        &HeaderMap::new(),
        None,
        Some("main"),
        "page-1".to_owned(),
        SessionListParams {
            limit: Some(2),
            ..SessionListParams::default()
        },
    )
    .await;
    let first = recv_rpc_json(&mut rx).await;
    assert_eq!(first["error"], Value::Null, "{first}");
    assert_eq!(first["result"]["sessions"].as_array().unwrap().len(), 2);
    assert_eq!(
        first["result"]["sessions"][0]["session_id"],
        json!("api:newest")
    );
    assert_eq!(
        first["result"]["sessions"][1]["session_id"],
        json!("api:middle")
    );
    let cursor = first["result"]["next_cursor"].as_str().unwrap().to_owned();

    crate::api::workspace_registry::ensure_session_ref(
        &state,
        "main",
        &SessionKey("api:inserted".to_owned()),
        None,
        &catalog_info("api:inserted", "2026-01-03T12:00:00Z"),
        false,
    )
    .unwrap();
    let catalog_path = temp.path().join("profile-data/main/session-catalog.json");
    let before = std::fs::read(&catalog_path).unwrap();
    handle_session_ref_list(
        &ws,
        &state,
        &HeaderMap::new(),
        None,
        Some("main"),
        "page-2".to_owned(),
        SessionListParams {
            cursor: Some(cursor),
            limit: Some(2),
            ..SessionListParams::default()
        },
    )
    .await;
    let second = recv_rpc_json(&mut rx).await;
    assert_eq!(second["result"]["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(
        second["result"]["sessions"][0]["session_id"],
        json!("api:oldest")
    );
    assert_eq!(second["result"]["next_cursor"], Value::Null);
    assert_eq!(std::fs::read(catalog_path).unwrap(), before);
}

#[tokio::test]
async fn session_create_dispatch_returns_a_resolvable_ref() {
    let temp = tempfile::tempdir().unwrap();
    let (mut state, _runtime) = state_with_profile(temp.path(), "main").await;
    Arc::get_mut(&mut state).unwrap().workspace_allowed_roots = vec![temp.path().to_owned()];
    let workspace_root = temp.path().join("workspace");
    std::fs::create_dir(&workspace_root).unwrap();
    let workspace = crate::api::workspace_registry::register_workspace(
        &state,
        "main",
        workspace_root.to_str().unwrap(),
        None,
    )
    .unwrap();
    let contracts = UiProtocolContractStores::default();
    let (ws, mut rx) = ws_connection_for_test(16);
    assert!(
        handle_session_create(
            &ws,
            &state,
            &Arc::new(UiProtocolLedger::new(16)),
            &contracts.approvals,
            &contracts.user_questions,
            &SharedLiveForwarders::default(),
            Some("main"),
            Some("main"),
            workspace_features(),
            "create-success".to_owned(),
            octos_core::ui_protocol::SessionCreateParams {
                workspace_id: workspace.workspace_id,
                profile_id: Some("main".to_owned()),
            },
        )
        .await
    );
    let response = recv_rpc_json(&mut rx).await;
    assert_eq!(response["id"], json!("create-success"));
    assert_eq!(response["result"]["created"], json!(true));
    let reference = response["result"]["session_ref"].as_str().unwrap();
    let resolved =
        crate::api::workspace_registry::resolve_session_ref(&state, Some("main"), reference)
            .unwrap();
    assert_eq!(
        resolved.workspace.unwrap().canonical_path,
        workspace.canonical_path
    );
}
