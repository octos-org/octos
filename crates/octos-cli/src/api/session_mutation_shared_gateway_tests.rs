use super::*;

#[tokio::test]
async fn session_mutation_shared_gateway_failure_never_writes_locally() {
    for canonical in [false, true] {
        for deleting in [false, true] {
            let gateway =
                GatewayStub::start(StatusCode::INTERNAL_SERVER_ERROR, Some("owner-token")).await;
            let (dir, state) = legacy_fixture(MAIN_PROFILE_ID, false).await;
            let key = if canonical {
                SessionKey::new("api", "web-legacy")
            } else {
                SessionKey::with_profile(MAIN_PROFILE_ID, "api", "web-legacy")
            };
            state
                .sessions
                .as_ref()
                .unwrap()
                .lock()
                .await
                .add_message(&key, Message::user("preserve me"))
                .await
                .unwrap();
            register_gateway_at(
                &state,
                MAIN_PROFILE_ID,
                gateway.port,
                Some("owner-token"),
                Some(dir.path().join("data").to_str().unwrap().into()),
            )
            .await;
            let request = if canonical {
                key.clone()
            } else {
                SessionKey("web-legacy".into())
            };
            let response = if deleting {
                delete(&state, &request, Some(AuthIdentity::Admin)).await
            } else {
                rename(&state, &request, Some(AuthIdentity::Admin)).await
            };
            assert_eq!(
                response.status(),
                StatusCode::INTERNAL_SERVER_ERROR,
                "canonical={canonical}, deleting={deleting}"
            );
            let fresh = octos_bus::SessionManager::open(&dir.path().join("data"))
                .unwrap()
                .load(&key)
                .await
                .unwrap();
            assert_eq!(fresh.messages.len(), 1);
            assert!(!fresh.title_manual);
            let calls = gateway.calls.lock().unwrap();
            assert_eq!(calls.len(), 1);
            assert_eq!(
                calls[0].authorization.as_deref(),
                Some("Bearer owner-token")
            );
        }
    }
}

#[tokio::test]
async fn session_mutation_shared_gateway_success_does_not_duplicate_local_write() {
    for deleting in [false, true] {
        let gateway = GatewayStub::start(StatusCode::NO_CONTENT, None).await;
        let (dir, state) = legacy_fixture(MAIN_PROFILE_ID, true).await;
        register_gateway_at(
            &state,
            MAIN_PROFILE_ID,
            gateway.port,
            None,
            Some(dir.path().join("data").to_str().unwrap().into()),
        )
        .await;
        assert_eq!(
            legacy_mutation(&state, deleting, AuthIdentity::Admin)
                .await
                .status(),
            StatusCode::NO_CONTENT
        );
        // The stub reports success but deliberately leaves the file untouched:
        // any change here would prove serve is still writing the child's store.
        let fresh = octos_bus::SessionManager::open(&dir.path().join("data"))
            .unwrap()
            .load(&SessionKey::with_profile(
                MAIN_PROFILE_ID,
                "api",
                "web-legacy",
            ))
            .await
            .unwrap();
        assert!(!fresh.title_manual);
        assert_eq!(fresh.messages.len(), 1);
    }
}

// A real HTTP request parks the child mutation while a canonical writer in
// serve attempts to append without taking SessionManager's mutex.
#[cfg(unix)]
async fn session_mutation_gated_proxy(deleting: bool, wait_for_response: bool) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let release = gate.clone();
    let (entered, mut requests) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(async move {
        let app = axum::Router::new().fallback(move || {
            let release = release.clone();
            let entered = entered.clone();
            async move {
                entered.send(()).unwrap();
                release.acquire().await.unwrap().forget();
                StatusCode::NO_CONTENT
            }
        });
        axum::serve(listener, app).await.unwrap();
    });
    let (dir, mut state) = legacy_fixture(MAIN_PROFILE_ID, true).await;
    Arc::get_mut(&mut state).unwrap().http_client =
        reqwest::Client::builder().no_proxy().build().unwrap();
    let data = dir.path().join("data");
    register_gateway_at(
        &state,
        MAIN_PROFILE_ID,
        port,
        None,
        Some(data.to_str().unwrap().into()),
    )
    .await;
    let key = SessionKey::new("api", &format!("gated-{}", uuid::Uuid::new_v4()));
    state
        .sessions
        .as_ref()
        .unwrap()
        .lock()
        .await
        .add_message(&key, Message::user("seed"))
        .await
        .unwrap();
    let target = key.clone();
    let owner = state.clone();
    let mut mutation = tokio::spawn(async move {
        if deleting {
            delete(&owner, &target, Some(AuthIdentity::Admin)).await
        } else {
            rename(&owner, &target, Some(AuthIdentity::Admin)).await
        }
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), requests.recv())
        .await
        .unwrap()
        .unwrap();
    let bounded = if wait_for_response {
        tokio::time::timeout(std::time::Duration::from_secs(5), &mut mutation)
            .await
            .ok()
            .map(Result::unwrap)
    } else {
        None
    };
    let sessions_held = state.sessions.as_ref().unwrap().try_lock().is_err();
    let writer_key = key.clone();
    let writer_data = data.clone();
    let mut writer = tokio::spawn(async move {
        octos_bus::persist_message_through_canonical_path(
            &writer_data,
            &writer_key,
            Message::assistant_with_thread(
                "must wait for the child",
                octos_core::ThreadId::new("gated-delivery"),
            ),
        )
        .await
        .unwrap()
    });
    let writer_attempt =
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut writer).await;
    let writer_held = writer_attempt.is_err();
    if let Ok(result) = writer_attempt {
        result.unwrap();
    }
    gate.add_permits(1);
    if !mutation.is_finished() {
        tokio::time::timeout(std::time::Duration::from_secs(2), mutation)
            .await
            .unwrap()
            .unwrap();
    }
    if writer_held {
        tokio::time::timeout(std::time::Duration::from_secs(2), writer)
            .await
            .unwrap()
            .unwrap();
    }
    let _guard = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        state.sessions.as_ref().unwrap().lock(),
    )
    .await
    .unwrap();
    server.abort();
    assert!(
        sessions_held,
        "serve's manager must remain locked until the child's result"
    );
    assert!(
        writer_held,
        "canonical send_file/background writers must share the mutation guard"
    );
    if wait_for_response {
        let response =
            bounded.expect("client receives a bounded response while child is still parked");
        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&body).contains("outcome is not yet known"));
    }
}

#[cfg(unix)]
#[tokio::test]
async fn session_mutation_delegation_blocks_canonical_writers_until_reply() {
    for deleting in [false, true] {
        session_mutation_gated_proxy(deleting, false).await;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn session_mutation_timeout_keeps_guards_until_late_reply() {
    for deleting in [false, true] {
        session_mutation_gated_proxy(deleting, true).await;
    }
}
