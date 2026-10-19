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

// Forking preserves the selected session history and tenant boundaries.
#[tokio::test(flavor = "current_thread")]
async fn session_fork_copies_full_history_by_default() {
    let session_id = SessionKey("local:fork-parent".into());
    // 3 turns -> 6 messages persisted on the parent.
    let (state, _tmp) = prg_state_with_persisted_turns(&session_id, 3).await;
    let (ws, mut rx) = ws_connection_for_test(8);

    handle_session_fork(
        &ws,
        &state,
        None,
        None,
        "fk1".into(),
        octos_core::ui_protocol::SessionForkParams {
            session_id: session_id.clone(),
            new_chat_id: "fork-child".into(),
            copy_messages: None,
        },
    )
    .await;

    let frame = recv_rpc_json(&mut rx).await;
    assert_eq!(frame["id"], "fk1");
    let result = &frame["result"];
    assert_eq!(result["new_session_id"], "local:fork-child");
    assert_eq!(result["parent_session_id"], session_id.to_string());
    assert_eq!(result["copied_messages"], 6);

    // The child must exist on the manager, carry parent lineage, and
    // hold the full copied history.
    let sessions = state.sessions.as_ref().expect("sessions");
    let mut guard = sessions.lock().await;
    let child_key = SessionKey("local:fork-child".into());
    assert!(
        guard.session_known(&child_key),
        "child session must persist"
    );
    let child = guard.get_or_create(&child_key).await;
    assert_eq!(child.parent_key.as_ref(), Some(&session_id));
    assert_eq!(child.messages.len(), 6);
}

#[tokio::test(flavor = "current_thread")]
async fn session_fork_copies_only_requested_tail() {
    let session_id = SessionKey("local:fork-tail".into());
    let (state, _tmp) = prg_state_with_persisted_turns(&session_id, 3).await;
    let (ws, mut rx) = ws_connection_for_test(8);

    handle_session_fork(
        &ws,
        &state,
        None,
        None,
        "fk2".into(),
        octos_core::ui_protocol::SessionForkParams {
            session_id: session_id.clone(),
            new_chat_id: "tail-child".into(),
            copy_messages: Some(2),
        },
    )
    .await;

    let frame = recv_rpc_json(&mut rx).await;
    assert_eq!(frame["result"]["copied_messages"], 2);
    let sessions = state.sessions.as_ref().expect("sessions");
    let mut guard = sessions.lock().await;
    let child = guard
        .get_or_create(&SessionKey("local:tail-child".into()))
        .await;
    assert_eq!(child.messages.len(), 2);
    // Tail = the LAST turn (user + assistant of turn 3).
    assert_eq!(child.messages[0].content, "turn 3");
    assert_eq!(child.messages[1].content, "reply 3");
}

#[tokio::test(flavor = "current_thread")]
async fn session_fork_unknown_session_errors() {
    let seeded = SessionKey("local:fork-seeded".into());
    let (state, _tmp) = prg_state_with_persisted_turns(&seeded, 1).await;
    let (ws, mut rx) = ws_connection_for_test(8);

    handle_session_fork(
        &ws,
        &state,
        None,
        None,
        "fk3".into(),
        octos_core::ui_protocol::SessionForkParams {
            session_id: SessionKey("local:never-created".into()),
            new_chat_id: "child".into(),
            copy_messages: None,
        },
    )
    .await;

    let frame = recv_rpc_json(&mut rx).await;
    assert!(
        frame.get("error").is_some(),
        "fork of an unknown session must error, not auto-create"
    );
    // Fork must NOT have created either session as a side effect.
    let sessions = state.sessions.as_ref().expect("sessions");
    let mut guard = sessions.lock().await;
    assert!(!guard.session_known(&SessionKey("local:never-created".into())));
    assert!(!guard.session_known(&SessionKey("local:child".into())));
}

#[tokio::test(flavor = "current_thread")]
async fn session_fork_refuses_existing_child_key() {
    let session_id = SessionKey("local:fork-clobber".into());
    let (state, _tmp) = prg_state_with_persisted_turns(&session_id, 2).await;
    let (ws, mut rx) = ws_connection_for_test(8);

    for id in ["fka", "fkb"] {
        handle_session_fork(
            &ws,
            &state,
            None,
            None,
            id.into(),
            octos_core::ui_protocol::SessionForkParams {
                session_id: session_id.clone(),
                new_chat_id: "same-child".into(),
                copy_messages: None,
            },
        )
        .await;
    }

    let first = recv_rpc_json(&mut rx).await;
    assert!(first.get("result").is_some(), "first fork succeeds");
    let second = recv_rpc_json(&mut rx).await;
    assert_eq!(
        second["error"]["data"]["kind"], "child_exists",
        "second fork onto the same child key must refuse, not clobber"
    );

    // The child's history must be the FIRST fork's copy, untouched.
    let sessions = state.sessions.as_ref().expect("sessions");
    let mut guard = sessions.lock().await;
    let child = guard
        .get_or_create(&SessionKey("local:same-child".into()))
        .await;
    assert_eq!(child.messages.len(), 4);
}

#[tokio::test(flavor = "current_thread")]
async fn session_fork_rejects_invalid_chat_id() {
    let session_id = SessionKey("local:fork-badname".into());
    let (state, _tmp) = prg_state_with_persisted_turns(&session_id, 1).await;
    let (ws, mut rx) = ws_connection_for_test(8);

    handle_session_fork(
        &ws,
        &state,
        None,
        None,
        "fk4".into(),
        octos_core::ui_protocol::SessionForkParams {
            session_id: session_id.clone(),
            new_chat_id: "../escape".into(),
            copy_messages: None,
        },
    )
    .await;

    let frame = recv_rpc_json(&mut rx).await;
    assert_eq!(frame["error"]["data"]["kind"], "invalid_new_chat_id");
}

#[test]
fn fork_reservations_scope_by_sessions_dir() {
    // codex #1613 r2: identical child keys in DIFFERENT profiles'
    // sessions dirs name different files — they must not exclude
    // each other. Same dir + same key must.
    let child = SessionKey("local:contested".into());
    let dir_a = std::path::Path::new("/tmp/profile-a/sessions");
    let dir_b = std::path::Path::new("/tmp/profile-b/sessions");

    let a = ForkReservation::try_acquire(dir_a, &child).expect("dir A reserves");
    let b = ForkReservation::try_acquire(dir_b, &child);
    assert!(b.is_some(), "different sessions dir must not collide");
    assert!(
        ForkReservation::try_acquire(dir_a, &child).is_none(),
        "same dir + same child key must exclude"
    );
    drop(a);
    assert!(
        ForkReservation::try_acquire(dir_a, &child).is_some(),
        "released reservation must be reacquirable"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn session_fork_raw_spa_parent_yields_raw_child() {
    // codex #1613 P1: a raw SPA handle ("web-123") must NOT be
    // treated as a channel — the child is a raw handle too, not
    // "web-123:kid" (excluded from the raw REST fallbacks).
    let session_id = SessionKey("web-123".into());
    let (state, _tmp) = prg_state_with_persisted_turns(&session_id, 1).await;
    let (ws, mut rx) = ws_connection_for_test(8);

    handle_session_fork(
        &ws,
        &state,
        None,
        None,
        "fk-raw".into(),
        octos_core::ui_protocol::SessionForkParams {
            session_id: session_id.clone(),
            new_chat_id: "web-456".into(),
            copy_messages: None,
        },
    )
    .await;

    let frame = recv_rpc_json(&mut rx).await;
    assert_eq!(frame["result"]["new_session_id"], "web-456");
    let sessions = state.sessions.as_ref().expect("sessions");
    let mut guard = sessions.lock().await;
    assert!(guard.session_known(&SessionKey("web-456".into())));
    assert!(
        !guard.session_known(&SessionKey("web-123:web-456".into())),
        "the naive channel-split key must not exist"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn session_fork_concurrent_same_child_one_wins() {
    // codex #1613 P2: two forks from DIFFERENT parents racing to
    // the same child key — exactly one may win; the loser must not
    // silently overwrite the winner's copied history.
    let parent_a = SessionKey("local:race-a".into());
    let parent_b = SessionKey("local:race-b".into());
    let (state, _tmp) = prg_state_with_persisted_turns(&parent_a, 1).await;
    {
        // Seed the second parent on the same manager.
        let sessions = state.sessions.as_ref().expect("sessions");
        let mut guard = sessions.lock().await;
        let now = Utc::now();
        let msg = Message {
            role: MessageRole::User,
            content: "b says".into(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: now,
            source: None,
        };
        guard.add_message(&parent_b, msg).await.expect("seed b");
    }
    let (ws, mut rx) = ws_connection_for_test(8);

    let fork = |parent: SessionKey, id: &str| {
        let ws = &ws;
        let state = &state;
        let id = id.to_string();
        async move {
            handle_session_fork(
                ws,
                state,
                None,
                None,
                id,
                octos_core::ui_protocol::SessionForkParams {
                    session_id: parent,
                    new_chat_id: "contested".into(),
                    copy_messages: None,
                },
            )
            .await;
        }
    };
    tokio::join!(
        fork(parent_a.clone(), "fk-a"),
        fork(parent_b.clone(), "fk-b")
    );

    let first = recv_rpc_json(&mut rx).await;
    let second = recv_rpc_json(&mut rx).await;
    let oks = [&first, &second]
        .iter()
        .filter(|f| f.get("result").is_some())
        .count();
    assert_eq!(oks, 1, "exactly one fork wins: {first} / {second}");
    let loser = if first.get("error").is_some() {
        &first
    } else {
        &second
    };
    assert_eq!(loser["error"]["data"]["kind"], "child_exists");
}

#[tokio::test(flavor = "current_thread")]
async fn session_fork_enforces_connection_scope() {
    // A profile-PREFIXED session id names another tenant's scope
    // outright; a connection bound to "tenant-b" must be refused.
    // (Un-prefixed ids are accepted by SPA convention — isolation for
    // those comes from per-profile runtime resolution.)
    let session_id = SessionKey("tenant-a:local:fork-scope".into());
    let (state, _tmp) = prg_state_with_persisted_turns(&session_id, 1).await;
    let (ws, mut rx) = ws_connection_for_test(8);

    handle_session_fork(
        &ws,
        &state,
        Some("tenant-b"),
        None,
        "fk5".into(),
        octos_core::ui_protocol::SessionForkParams {
            session_id: session_id.clone(),
            new_chat_id: "stolen".into(),
            copy_messages: None,
        },
    )
    .await;

    // Scope violations close 1008-first (see
    // send_scope_error_closes_with_1008_on_authenticated_mismatch).
    let first = rx.recv().await.expect("close frame");
    match first {
        axum::extract::ws::Message::Close(Some(frame)) => {
            assert_eq!(frame.code, 1008);
        }
        other => panic!("expected 1008 close on cross-scope fork, got {other:?}"),
    }
    // And the fork must NOT have happened.
    let sessions = state.sessions.as_ref().expect("sessions");
    let mut guard = sessions.lock().await;
    assert!(!guard.session_known(&SessionKey("tenant-a:local:stolen".into())));
    assert!(!guard.session_known(&SessionKey("local:stolen".into())));
}
