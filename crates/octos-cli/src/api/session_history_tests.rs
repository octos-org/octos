use super::*;

async fn fixture() -> (tempfile::TempDir, Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(crate::profiles::ProfileStore::open_unified(dir.path()).unwrap());
    let mut profiles = HashMap::new();
    for id in ["history-owner", "history-other"] {
        let profile = panel_user_profile(id);
        store.save(&profile).unwrap();
        let runtime = make_m11e_profile_with_llm_and_sandbox(
            id,
            &store.resolve_data_dir(&profile),
            Arc::new(M11EStubLlm),
            octos_agent::SandboxConfig::default(),
        )
        .await;
        profiles.insert(id.to_owned(), runtime);
    }
    (
        dir,
        Arc::new(AppState {
            profile_store: Some(store),
            profiles,
            session_cache: Arc::new(
                crate::runtime::SessionRuntimeCache::new(8, Duration::from_secs(60))
                    .with_sessions_in_cwd(true),
            ),
            ..AppState::empty_for_tests()
        }),
    )
}

#[tokio::test]
async fn session_history_should_isolate_profiles_and_paginate() {
    let (_dir, state) = fixture().await;
    for (id, runtime) in &state.profiles {
        let mut manager = octos_bus::SessionManager::open(&runtime.data_dir).unwrap();
        manager
            .add_message(
                &SessionKey("web-shared".into()),
                octos_core::Message::user(id),
            )
            .await
            .unwrap();
        // A context whose binding is unavailable must fail closed, never
        // become ordinary account history even if its transcript remains.
        manager
            .add_message(
                &SessionKey(format!("{id}:api:app#peerctx-missing.context")),
                octos_core::Message::user("host context"),
            )
            .await
            .unwrap();
    }
    let request = RpcRequest::new(
        "history",
        APPUI_METHOD_SESSION_HISTORY_LIST,
        json!({"limit":1}),
    );
    let first = session_history::list(&state, &request, None).await.unwrap();
    assert_eq!(first["total"], 2);
    assert_eq!(first["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(first["next_offset"], 1);
    let second = session_history::list(
        &state,
        &RpcRequest::new(
            "next",
            APPUI_METHOD_SESSION_HISTORY_LIST,
            json!({"limit":1,"offset":1}),
        ),
        None,
    )
    .await
    .unwrap();
    assert_ne!(first["sessions"][0]["id"], second["sessions"][0]["id"]);
    assert!(second["next_offset"].is_null());
    let own = session_history::list(&state, &request, Some("history-owner"))
        .await
        .unwrap();
    assert_eq!(own["total"], 1);
    assert_eq!(own["sessions"][0]["id"], "history-owner:api:web-shared");
    let denied = session_history::list(
        &state,
        &RpcRequest::new(
            "cross-profile",
            APPUI_METHOD_SESSION_HISTORY_LIST,
            json!({"profile_id":"history-other"}),
        ),
        Some("history-owner"),
    )
    .await
    .unwrap_err();
    assert_eq!(denied.code, rpc_error_codes::PERMISSION_DENIED);
    assert!(raw_method_is_dispatched(
        APPUI_METHOD_SESSION_HISTORY_LIST,
        false
    ));
    assert!(!session_ingress_callable_method(
        APPUI_METHOD_SESSION_HISTORY_LIST
    ));
    assert!(APPUI_EXTRA_METHODS.contains(&APPUI_METHOD_SESSION_HISTORY_LIST));
}

#[tokio::test]
async fn session_history_should_find_saved_workspaces_without_a_client_hint() {
    let (dir, state) = fixture().await;
    let mut roots = Vec::new();
    for name in ["project-a", "project-b"] {
        let root = dir.path().join(name);
        std::fs::create_dir_all(&root).unwrap();
        let root = dunce::canonicalize(root).unwrap();
        let path = crate::runtime::session::project_sessions_root(&root, "history-owner");
        let mut manager = octos_bus::SessionManager::open(&path).unwrap();
        manager
            .add_message(
                &SessionKey("history-owner:local:tui#coding".into()),
                octos_core::Message::user(name),
            )
            .await
            .unwrap();
        crate::runtime::workspace_history::remember(
            &state.profiles["history-owner"].data_dir,
            &root,
        )
        .unwrap();
        roots.push(root);
    }
    // No session runtime was opened on this server and the client knows no cwd.
    let result = session_history::list(
        &state,
        &RpcRequest::new("history", APPUI_METHOD_SESSION_HISTORY_LIST, json!({})),
        Some("history-owner"),
    )
    .await
    .unwrap();
    let rows = result["sessions"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "{result}");
    assert_eq!(rows[0]["id"], rows[1]["id"]);
    assert_ne!(rows[0]["workspace_root"], rows[1]["workspace_root"]);
    assert!(
        rows.iter()
            .all(|r| r["id"] == "history-owner:local:tui#coding")
    );
    let other = session_history::list(
        &state,
        &RpcRequest::new("other", APPUI_METHOD_SESSION_HISTORY_LIST, json!({})),
        Some("history-other"),
    )
    .await
    .unwrap();
    assert_eq!(other["total"], 0);
    assert!(other["workspaces"].as_array().unwrap().is_empty());
    // A removed project remains visibly unavailable and is never recreated.
    std::fs::remove_dir_all(&roots[0]).unwrap();
    let result = session_history::list(
        &state,
        &RpcRequest::new("history", APPUI_METHOD_SESSION_HISTORY_LIST, json!({})),
        Some("history-owner"),
    )
    .await
    .unwrap();
    assert_eq!(result["total"], 1);
    assert_eq!(
        result["unavailable_workspaces"].as_array().unwrap().len(),
        1
    );
    assert!(!roots[0].exists());
}

#[tokio::test]
async fn session_history_should_remember_existing_hints_but_not_create_empty_stores() {
    let (dir, state) = fixture().await;
    let root = dir.path().join("existing");
    std::fs::create_dir_all(&root).unwrap();
    let root = dunce::canonicalize(root).unwrap();
    let mut manager = octos_bus::SessionManager::open(
        &crate::runtime::session::project_sessions_root(&root, "history-owner"),
    )
    .unwrap();
    manager
        .add_message(
            &SessionKey("history-owner:api:chat".into()),
            octos_core::Message::user("history"),
        )
        .await
        .unwrap();
    let empty = dir.path().join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let request = RpcRequest::new(
        "hint",
        APPUI_METHOD_SESSION_HISTORY_LIST,
        json!({"workspaces":[root,empty]}),
    );
    let result = session_history::list(&state, &request, Some("history-owner"))
        .await
        .unwrap();
    assert_eq!(result["total"], 1);
    assert!(!empty.join(".octos").exists());
    assert_eq!(
        crate::runtime::workspace_history::load(&state.profiles["history-owner"].data_dir).unwrap(),
        vec![root]
    );
    state
        .profile_store
        .as_ref()
        .unwrap()
        .save(&panel_user_profile("history-unconfigured"))
        .unwrap();
    let all = session_history::list(&state, &request, None).await.unwrap();
    assert_eq!(all["total"], 1);
    assert!(
        all["unavailable_workspaces"].as_array().unwrap().is_empty(),
        "An unrelated unconfigured profile must not mark accessible paths unavailable: {all}"
    );
}
