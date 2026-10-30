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
