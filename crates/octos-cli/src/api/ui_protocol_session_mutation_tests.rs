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
