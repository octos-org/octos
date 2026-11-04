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

fn content_profile(id: &str) -> crate::profiles::UserProfile {
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
async fn content_preview_service_rest_and_oup_are_equivalent_and_scoped() {
    let dir = tempfile::tempdir().unwrap();
    let profile_store = Arc::new(crate::profiles::ProfileStore::open_unified(dir.path()).unwrap());
    let profile = content_profile("tenant");
    profile_store.save(&profile).unwrap();
    profile_store.save(&content_profile("other")).unwrap();
    let data_dir = profile_store.resolve_data_dir(&profile);
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::write(data_dir.join("alpha.md"), "alpha").unwrap();
    std::fs::write(data_dir.join("beta.md"), "b".repeat(1024 * 1024)).unwrap();
    // A root-outside file must not enter the profile's catalog or leak a raw path.
    std::fs::write(dir.path().join("outside.md"), "outside").unwrap();
    let state = Arc::new(AppState {
        profile_store: Some(profile_store.clone()),
        content_catalog_mgr: Some(Arc::new(
            crate::content_catalog::ContentCatalogManager::new(profile_store.clone()),
        )),
        ..AppState::empty_for_tests()
    });
    let identity = AuthIdentity::User {
        id: "tenant".into(),
        role: crate::user_store::UserRole::User,
    };
    let headers = HeaderMap::new();
    let service = crate::api::auth_handlers::ContentPreviewService::new(state.clone());

    let all = service
        .list(
            &headers,
            &identity,
            crate::content_catalog::ContentQuery::default(),
        )
        .await
        .unwrap();
    assert_eq!(all.total, 2, "outside.md must stay outside the catalog");
    assert!(all.entries.iter().all(|entry| {
        !std::path::Path::new(&entry.path).is_absolute() && entry.filename != "outside.md"
    }));
    assert_eq!(
        all.entries
            .iter()
            .find(|entry| entry.filename == "beta.md")
            .unwrap()
            .size_bytes,
        1024 * 1024
    );

    let query = crate::content_catalog::ContentQuery {
        limit: 1,
        sort: "name".into(),
        ..Default::default()
    };
    let expected = service
        .list(
            &headers,
            &identity,
            crate::content_catalog::ContentQuery {
                limit: 1,
                sort: "name".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(expected.total, 2);
    assert_eq!(expected.entries.len(), 1);
    assert_eq!(expected.entries[0].filename, "alpha.md");
    assert_eq!(
        expected.entries[0].category,
        crate::content_catalog::ContentCategory::Report
    );
    let expected_json = serde_json::to_value(&expected).unwrap();

    let axum::Json(rest) = crate::api::auth_handlers::my_content(
        State(state.clone()),
        headers.clone(),
        axum::Extension(identity.clone()),
        axum::extract::Query(query),
    )
    .await
    .unwrap();
    assert_eq!(serde_json::to_value(rest).unwrap(), expected_json);

    let (ws, mut rx) = ws_connection_for_test(16);
    handle_content_list(
        &ws,
        &state,
        &headers,
        Some(&identity),
        false,
        "content-list-service".into(),
        ContentListParams {
            filters: json!({ "limit": 1, "sort": "name" }),
        },
    )
    .await;
    let frame = recv_rpc_json(&mut rx).await;
    assert_eq!(frame["id"], json!("content-list-service"));
    assert_eq!(frame["result"]["entries"], expected_json["entries"]);
    assert_eq!(frame["result"]["total"], expected_json["total"]);

    let mut foreign = headers.clone();
    foreign.insert(
        "x-profile-id",
        axum::http::HeaderValue::from_static("other"),
    );
    let error = service
        .list(
            &foreign,
            &identity,
            crate::content_catalog::ContentQuery::default(),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        &error,
        crate::api::auth_handlers::ContentPreviewError::Forbidden
    ));
    let (status, message) = error.into_parts();
    assert_eq!(status, axum::http::StatusCode::FORBIDDEN);
    assert_eq!(message, "forbidden");
    handle_content_list(
        &ws,
        &state,
        &foreign,
        Some(&identity),
        false,
        "content-list-foreign".into(),
        ContentListParams::default(),
    )
    .await;
    let frame = recv_rpc_json(&mut rx).await;
    assert_eq!(frame["error"]["data"]["rest_status"], json!(403));
    assert_eq!(frame["error"]["data"]["detail"], json!("forbidden"));

    std::fs::remove_file(data_dir.join("alpha.md")).unwrap();
    let missing = service
        .list(
            &headers,
            &identity,
            crate::content_catalog::ContentQuery {
                search: Some("alpha".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(missing.total, 0);
    assert!(missing.entries.is_empty());
}
