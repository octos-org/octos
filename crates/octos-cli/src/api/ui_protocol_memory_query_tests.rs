use super::*;

fn ws_connection_for_test(capacity: usize) -> (WsConnection, mpsc::Receiver<WsMessage>) {
    let (tx, rx) = mpsc::channel(capacity);
    (WsConnection::new(tx), rx)
}

async fn recv_rpc_json(rx: &mut mpsc::Receiver<WsMessage>) -> Value {
    match rx.recv().await {
        Some(WsMessage::Text(text)) => serde_json::from_str(&text).expect("RPC frame must be JSON"),
        other => panic!("expected a text RPC frame, got {other:?}"),
    }
}

fn memory_profile(id: &str) -> crate::profiles::UserProfile {
    crate::profiles::UserProfile {
        id: id.into(),
        name: id.into(),
        enabled: true,
        data_dir: None,
        parent_id: None,
        public_subdomain: None,
        config: crate::profiles::ProfileConfig::default(),
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    }
}

#[tokio::test]
async fn memory_query_service_rest_and_oup_are_equivalent_and_scoped() {
    let dir = tempfile::tempdir().unwrap();
    let profile_store = Arc::new(crate::profiles::ProfileStore::open_unified(dir.path()).unwrap());
    let profile = memory_profile("tenant");
    profile_store.save(&profile).unwrap();
    profile_store.save(&memory_profile("other")).unwrap();
    let data_dir = profile_store.resolve_data_dir(&profile);
    let memory = octos_memory::MemoryStore::open(&data_dir).await.unwrap();
    memory
        .write_long_term("# MEMORY\n\n- remembers things\n")
        .await
        .unwrap();
    memory
        .write_entity("fleet", "# fleet\n\nAbstract: five minis\n")
        .await
        .unwrap();
    let state = Arc::new(AppState {
        profile_store: Some(profile_store),
        ..AppState::empty_for_tests()
    });
    let identity = AuthIdentity::User {
        id: "tenant".into(),
        role: crate::user_store::UserRole::User,
    };
    let headers = HeaderMap::new();
    let service = crate::api::memory_panel::MemoryQueryService::new(state.clone());

    let expected = service.overview("tenant").await.unwrap();
    let expected_json = serde_json::to_value(&expected).unwrap();
    let axum::Json(rest) = crate::api::memory_panel::my_memory(
        State(state.clone()),
        headers.clone(),
        axum::Extension(identity.clone()),
    )
    .await
    .unwrap();
    assert_eq!(serde_json::to_value(rest).unwrap(), expected_json);

    let (ws, mut rx) = ws_connection_for_test(16);
    handle_memory_overview(
        &ws,
        &state,
        &headers,
        Some(&identity),
        false,
        "memory-overview-service".into(),
        MemoryOverviewParams::default(),
    )
    .await;
    let frame = recv_rpc_json(&mut rx).await;
    assert_eq!(
        frame["result"]["overview"]["long_term"],
        expected_json["long_term"]
    );
    assert_eq!(
        frame["result"]["overview"]["entities"],
        expected_json["entities"]
    );
    assert_eq!(
        frame["result"]["overview"]["long_term_truncated"],
        json!(false)
    );

    let expected_entity = service.entity("tenant", "fleet".into()).await.unwrap();
    let axum::Json(rest_entity) = crate::api::memory_panel::my_memory_entity(
        State(state.clone()),
        headers.clone(),
        axum::Extension(identity.clone()),
        axum::extract::Path("fleet".into()),
    )
    .await
    .unwrap();
    assert_eq!(rest_entity.content, expected_entity.content);
    handle_memory_entity(
        &ws,
        &state,
        &headers,
        Some(&identity),
        false,
        "memory-entity-service".into(),
        MemoryEntityParams {
            session_id: None,
            name: "fleet".into(),
        },
    )
    .await;
    let frame = recv_rpc_json(&mut rx).await;
    assert_eq!(frame["result"]["name"], json!("fleet"));
    assert_eq!(frame["result"]["content"], json!(expected_entity.content));
    assert_eq!(frame["result"]["content_truncated"], json!(false));

    let missing = service
        .entity("tenant", "missing".into())
        .await
        .unwrap_err();
    assert_eq!(
        missing,
        crate::api::memory_panel::MemoryQueryError::NotFound
    );
    assert_eq!(missing.status(), axum::http::StatusCode::NOT_FOUND);
    handle_memory_entity(
        &ws,
        &state,
        &headers,
        Some(&identity),
        false,
        "memory-entity-missing".into(),
        MemoryEntityParams {
            session_id: None,
            name: "missing".into(),
        },
    )
    .await;
    let frame = recv_rpc_json(&mut rx).await;
    assert_eq!(frame["error"]["data"]["rest_status"], json!(404));
    assert_eq!(frame["error"]["data"]["identifier"], json!("missing"));

    let empty_other = service.overview("other").await.unwrap();
    assert!(empty_other.long_term.is_empty());
    assert!(empty_other.entities.is_empty());
    let missing_profile = service.overview("missing-profile").await.unwrap_err();
    assert_eq!(
        missing_profile,
        crate::api::memory_panel::MemoryQueryError::NotFound
    );

    let large_text = "x".repeat(MEMORY_RPC_ENTITY_CONTENT_BUDGET + 4096);
    memory.write_entity("large", &large_text).await.unwrap();
    let large = service.entity("tenant", "large".into()).await.unwrap();
    assert_eq!(large.content.len(), large_text.len());
    handle_memory_entity(
        &ws,
        &state,
        &headers,
        Some(&identity),
        false,
        "memory-entity-large".into(),
        MemoryEntityParams {
            session_id: None,
            name: "large".into(),
        },
    )
    .await;
    let frame = recv_rpc_json(&mut rx).await;
    assert_eq!(frame["result"]["content_truncated"], json!(true));
    assert_eq!(
        frame["result"]["content_total_bytes"],
        json!(large_text.len())
    );
    assert!(frame["result"]["content"].as_str().unwrap().len() <= MEMORY_RPC_ENTITY_CONTENT_BUDGET);

    handle_memory_overview(
        &ws,
        &state,
        &headers,
        Some(&identity),
        false,
        "memory-overview-cross-profile".into(),
        MemoryOverviewParams {
            session_id: Some("other:api:web-1".into()),
        },
    )
    .await;
    let frame = recv_rpc_json(&mut rx).await;
    assert_eq!(frame["error"]["code"], json!(-32602));
    assert_eq!(frame["error"]["data"]["auth_scope_violation"], json!(true));
    assert_eq!(
        frame["error"]["data"]["expected_profile_id"],
        json!("tenant")
    );
    assert_eq!(frame["error"]["data"]["actual_profile_id"], json!("other"));
}
