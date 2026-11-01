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
