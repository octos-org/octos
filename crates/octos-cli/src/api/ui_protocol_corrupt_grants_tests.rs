// T-0309: work-secret grants-file corruption must not strip ordinary browser
// turns of their existing capability, while the secret path stays fail-closed.
// Cross-platform (the ingress_lifetime_tests module is Unix-only).
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

/// Abort the isolated test server on drop.
struct TestServer(tokio::task::JoinHandle<()>);
impl Drop for TestServer {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn corrupt_grants_state(
    dir: &std::path::Path,
    profile: &str,
) -> (Arc<AppState>, Arc<crate::runtime::ProfileRuntime>) {
    let runtime = make_m11e_profile_with_llm_and_sandbox(
        profile,
        &dir.join("profile-data"),
        Arc::new(M11EStubLlm),
        octos_agent::SandboxConfig::default(),
    )
    .await;
    let mut state = AppState::empty_for_tests();
    state.profiles.insert(profile.to_owned(), runtime.clone());
    state.work_secret_store = Arc::new(
        octos_agent::bridge::work_secret::WorkSecretGrantStore::new(dir),
    );
    state.session_cache = Arc::new(
        crate::runtime::SessionRuntimeCache::new(4, Duration::from_secs(60))
            .with_sessions_in_cwd(true),
    );
    (Arc::new(state), runtime)
}

#[tokio::test(flavor = "current_thread")]
async fn corrupt_grants_file_preserves_browser_turns_and_background_work() {
    let dir = tempfile::tempdir().unwrap();
    let profile = format!("corrupt-grants-{}", uuid::Uuid::new_v4());
    let session = SessionKey::with_profile(&profile, "api", "corrupt-grants");
    let (state, _runtime) = corrupt_grants_state(dir.path(), &profile).await;
    let contracts = contract_stores();
    // Seed legitimate background work owned by a prior grant generation.
    let parent_turn = TurnId::new();
    let supervisor = Arc::new(octos_agent::TaskSupervisor::new());
    let _work = contracts
        .background_work
        .register(&session, &profile, &parent_turn, &supervisor);
    let task = supervisor.register("spawn", "child", Some(&session.0));
    supervisor.mark_running(&task);
    // Corrupt the grants file (invalid JSON).
    std::fs::write(dir.path().join("work_secrets.json"), "{ not valid json").unwrap();

    // Drive an ordinary browser turn/start (no ingress scope).
    let active_turns = active_turns_registry();
    let connection_turns: SharedConnectionTurns = Arc::new(TokioMutex::new(HashMap::new()));
    let ledger = event_ledger(&state).await;
    let (ws, _rx) = ws_connection_for_test(64);
    let turn = TurnId::new();
    let admitted = handle_turn_start(
        &ws,
        &state,
        &ledger,
        &contracts,
        &active_turns,
        &connection_turns,
        Some(&profile),
        None,
        ConnectionUiFeatures::stdio_defaults(),
        "turn".into(),
        TurnStartParams {
            session_id: session.clone(),
            turn_id: turn.clone(),
            input: vec![InputItem::Text {
                text: "hello".into(),
            }],
            media: Vec::new(),
            topic: None,
            rewrite_for: None,
            reasoning_effort: None,
            tool_context: None,
            live_video: false,
            origin: None,
        },
    )
    .await;
    assert!(admitted, "browser turn must be admitted despite a corrupt grants file");
    // The pre-existing background work was NOT retired/cancelled by treating
    // the unreadable file as a revocation.
    assert_eq!(
        supervisor.get_task(&task).unwrap().status,
        octos_agent::TaskStatus::Running,
        "a corrupt grants file must not cancel legitimate background work"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn corrupt_grants_file_rejects_ingress_bind_over_real_ws() {
    let dir = tempfile::tempdir().unwrap();
    let profile = format!("corrupt-ingress-{}", uuid::Uuid::new_v4());
    let session = SessionKey::with_profile(&profile, "api", "corrupt-ingress");
    let (state, _runtime) = corrupt_grants_state(dir.path(), &profile).await;
    let token = uuid::Uuid::new_v4().to_string();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    state
        .work_secret_store
        .issue(
            &session.0,
            &token,
            format!("http://{addr}"),
            chrono::Duration::minutes(5),
            Some(profile.clone()),
        )
        .unwrap();
    // Corrupt the grants file AFTER issuing the token.
    std::fs::write(dir.path().join("work_secrets.json"), "{ not valid json").unwrap();
    let app = crate::api::build_router(state.clone());
    let _server = TestServer(tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    }));
    let mut request = format!("ws://{addr}/v1/session_ingress/ws/{}", session.0)
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    // The route validates the work-secret before upgrading; a corrupt grants
    // file fails that validation closed, so the upgrade itself is rejected.
    let error = tokio_tungstenite::connect_async(request)
        .await
        .expect_err("ingress upgrade must be rejected on a corrupt grants file");
    assert!(
        error.to_string().contains("401"),
        "expected an auth rejection, got {error}"
    );
}
