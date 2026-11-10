use super::*;

fn workspace_for_session(data: &std::path::Path, session_id: &str) -> std::path::PathBuf {
    data.join("users")
        .join(octos_bus::session::encode_path_component(session_id))
        .join("workspace")
}

fn file_read_params(session_id: &str, path: &str) -> SessionFilesReadParams {
    SessionFilesReadParams {
        session_id: session_id.into(),
        path: path.into(),
        offset: None,
        length: None,
    }
}

#[tokio::test]
async fn session_file_read_rejects_traversal_directories_and_symlinks() {
    let data = tempfile::tempdir().unwrap();
    let state = AppState::empty_for_tests();
    let workspace = workspace_for_session(data.path(), "api:demo");
    std::fs::create_dir_all(&workspace).unwrap();
    for path in ["../secret.md", "/etc/passwd"] {
        let error = read_session_workspace_file(
            &state,
            data.path(),
            Some(MAIN_PROFILE_ID),
            file_read_params("api:demo", path),
        )
        .await
        .unwrap_err();
        assert_eq!(
            error.data.as_ref().unwrap()["kind"],
            "workspace_file_invalid_path"
        );
    }
    assert!(!data.path().join("secret.md").exists());

    std::fs::create_dir(workspace.join("directory")).unwrap();
    let error = read_session_workspace_file(
        &state,
        data.path(),
        Some(MAIN_PROFILE_ID),
        file_read_params("api:demo", "directory"),
    )
    .await
    .unwrap_err();
    assert_eq!(
        error.data.as_ref().unwrap()["kind"],
        "workspace_file_not_regular"
    );

    #[cfg(unix)]
    {
        let outside = data.path().join("outside.md");
        std::fs::write(&outside, b"secret").unwrap();
        std::os::unix::fs::symlink(&outside, workspace.join("escape.md")).unwrap();
        let error = read_session_workspace_file(
            &state,
            data.path(),
            Some(MAIN_PROFILE_ID),
            file_read_params("api:demo", "escape.md"),
        )
        .await
        .unwrap_err();
        assert_eq!(
            error.data.as_ref().unwrap()["kind"],
            "workspace_file_escape"
        );

        let outside_dir = data.path().join("outside-dir");
        std::fs::create_dir(&outside_dir).unwrap();
        std::fs::write(outside_dir.join("secret.md"), b"secret").unwrap();
        std::os::unix::fs::symlink(&outside_dir, workspace.join("dir-link")).unwrap();
        let error = read_session_workspace_file(
            &state,
            data.path(),
            Some(MAIN_PROFILE_ID),
            file_read_params("api:demo", "dir-link/secret.md"),
        )
        .await
        .unwrap_err();
        assert_eq!(
            error.data.as_ref().unwrap()["kind"],
            "workspace_file_escape"
        );
        assert!(!outside_dir.join("created").exists());
    }
}
