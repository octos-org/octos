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
    let (outside, outside_dir) = (
        data.path().join("outside.md"),
        data.path().join("outside-dir"),
    );
    std::fs::write(&outside, b"secret").unwrap();
    std::fs::create_dir(&outside_dir).unwrap();
    std::fs::write(outside_dir.join("secret.md"), b"secret").unwrap();
    // A web session's workspace, and a chat's own folder (UPCR-2026-R1EFC56A3).
    let chat = SessionKey("t:api:chat".into());
    let chat_root = crate::runtime::chat_workspace::create(data.path(), "t", &chat).unwrap();
    for (session_id, workspace) in [
        ("api:demo", workspace_for_session(data.path(), "api:demo")),
        (chat.0.as_str(), chat_root.root),
    ] {
        std::fs::create_dir_all(workspace.join("directory")).unwrap();
        std::fs::write(workspace.join("own.md"), b"own").unwrap();
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut refused = vec![
            ("../secret.md", "workspace_file_invalid_path"),
            ("/etc/passwd", "workspace_file_invalid_path"),
            ("directory", "workspace_file_not_regular"),
        ];
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, workspace.join("escape.md")).unwrap();
            std::os::unix::fs::symlink(&outside_dir, workspace.join("dir-link")).unwrap();
            refused.push(("escape.md", "workspace_file_escape"));
            refused.push(("dir-link/secret.md", "workspace_file_escape"));
        }
        let read = |path: &str| {
            let params = file_read_params(session_id, path);
            read_session_workspace_file(&state, data.path(), Some(MAIN_PROFILE_ID), params)
        };
        let own = read("own.md").await.unwrap();
        assert_eq!(own.content_base64, BASE64_STANDARD.encode(b"own"));
        for (path, kind) in refused {
            let error = read(path).await.unwrap_err();
            assert_eq!(error.data.unwrap()["kind"], kind, "{session_id} {path}");
        }
    }
    assert!(!data.path().join("secret.md").exists());
    assert!(!outside_dir.join("created").exists());
}
