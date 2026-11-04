use super::*;

#[test]
fn raw_session_open_binding_enforces_profile_scope() {
    let session = SessionKey("api:web-raw-session-scope-regression".to_owned());
    session_open_profiles().record(session.clone(), "coding");

    assert_eq!(
        validate_session_scope(&session, None, None).unwrap(),
        Some("coding".to_owned())
    );
    assert_eq!(
        validate_session_scope(&session, None, Some("coding")).unwrap(),
        Some("coding".to_owned())
    );
    let error = validate_session_scope(&session, None, Some("other")).unwrap_err();
    assert_eq!(
        error.data.as_ref().unwrap()["auth_scope_violation"],
        json!(true)
    );

    // Unscoped admin retains the existing explicit rebind behavior.
    assert_eq!(
        validate_session_scope(&session, Some("rebind-second"), None).unwrap(),
        Some("rebind-second".to_owned())
    );
}

#[tokio::test]
async fn session_delete_clears_in_memory_side_metadata() {
    let data_dir = tempfile::tempdir().unwrap();
    let sessions = Arc::new(tokio::sync::Mutex::new(
        octos_bus::SessionManager::open(data_dir.path()).unwrap(),
    ));
    let state = Arc::new(AppState {
        sessions: Some(sessions),
        ..AppState::empty_for_tests()
    });
    let session = SessionKey("web-delete-side-metadata-v3".to_owned());
    session_open_profiles().record(session.clone(), "raw-owner");
    session_workspaces().set(
        "raw-owner",
        session.clone(),
        data_dir.path().join("owned-workspace"),
    );
    session_workspaces().set(
        "other-profile",
        session.clone(),
        data_dir.path().join("other-workspace"),
    );
    session_permission_profiles().set(session.clone(), Default::default(), None);
    update_session_context_status(&session, json!({ "cached": true }));
    set_session_compaction_llm(&session, true);

    let response = crate::api::handlers::delete_session(
        axum::extract::State(state),
        HeaderMap::new(),
        None,
        axum::extract::Path(session.0.clone()),
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::NO_CONTENT);

    assert!(
        session_workspaces()
            .snapshot("raw-owner", &session)
            .is_none(),
        "delete clears the owning profile's workspace binding"
    );
    assert!(
        session_workspaces()
            .snapshot("other-profile", &session)
            .is_some(),
        "delete must not clear another profile's workspace binding"
    );
    assert!(
        session_permission_profiles()
            .get_state_explicit(&session)
            .is_none()
    );
    assert!(session_context_statuses().get(&session).is_none());
    assert!(session_open_profiles().lookup(&session).is_none());
    assert!(session_compaction_llm_overrides().get(&session).is_none());
}
