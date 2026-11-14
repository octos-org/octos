//! UPCR-2026-R1EFC56A3: one server-created workspace per chat.
use super::*;
use crate::runtime::chat_workspace;
use octos_core::ui_protocol::{
    ChatCreateParams, PermissionNetworkPolicy, PermissionProfileMode, PermissionProfileSetParams,
    methods, rpc_error_codes,
};

/// A provider no test here calls: nothing listens on its port.
fn unused_llm() -> Arc<dyn octos_llm::LlmProvider> {
    Arc::new(
        octos_llm::openai::OpenAIProvider::new("", "unused").with_base_url("http://127.0.0.1:9"),
    )
}

const PROFILE: &str = "t0596";

fn chat_key(chat: &str) -> SessionKey {
    SessionKey::with_profile(PROFILE, "api", chat)
}

async fn create_chat(state: &Arc<AppState>, session_id: SessionKey) -> Result<PathBuf, Value> {
    let params = ChatCreateParams {
        session_id,
        profile_id: PROFILE.into(),
    };
    session_chat_create_result(state, None, params)
        .await
        .map(|created| PathBuf::from(created.workspace_root))
        .map_err(|error| error.data.unwrap()["kind"].clone())
}

fn error_kind(error: RpcError) -> Value {
    error.data.unwrap()["kind"].clone()
}

#[tokio::test]
async fn chat_workspace_is_created_once_bound_to_its_chat_and_deleted_with_it() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (state, profile) = state_with_profile_llm(temp.path(), PROFILE, unused_llm()).await;
    let on = ConnectionUiFeatures {
        session_chat_workspace_v1: true,
        session_workspace_cwd: true,
        ..Default::default()
    };
    let (a, b, code) = (chat_key("a"), chat_key("b"), chat_key("code"));
    let create = json!({ "session_id": a, "profile_id": PROFILE });
    let request = RpcRequest::new("create-1", methods::SESSION_CHAT_CREATE, create);
    let gated = route_rpc_command(request.clone(), ConnectionUiFeatures::default());
    assert_eq!(
        gated.unwrap_err().code,
        rpc_error_codes::METHOD_NOT_SUPPORTED
    );
    assert!(matches!(
        route_rpc_command(request, on),
        Ok(UiCommand::SessionChatCreate(_))
    ));

    let root_a = create_chat(&state, a.clone()).await.expect("chat a");
    assert_eq!(create_chat(&state, a.clone()).await, Ok(root_a.clone()));
    let root_b = create_chat(&state, b.clone()).await.expect("chat b");
    assert_ne!(root_a, root_b);
    let chats = std::fs::canonicalize(profile.data_dir.join("chats")).unwrap();
    for root in [&root_a, &root_b] {
        assert_eq!(root.parent(), Some(chats.as_path()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(root).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700, "{}", root.display());
        }
    }
    let sessions = resolve_sessions_for_lookup(&state, None, None, &code)
        .await
        .unwrap();
    sessions
        .lock()
        .await
        .ensure_session_record_in_mode(&code, None)
        .await
        .unwrap();
    for (id, expected) in [
        (code.clone(), "session_exists"),
        (SessionKey(format!("{a}#topic")), "chat_session_id_invalid"),
        (
            SessionKey::with_profile("other", "api", "a"),
            "chat_session_id_invalid",
        ),
    ] {
        assert_eq!(create_chat(&state, id).await, Err(json!(expected)));
    }

    // A chat opens only in its own folder; no other session may open one.
    let open = |session_id: &SessionKey, cwd: Option<&Path>| {
        let params = SessionOpenParams {
            cwd: cwd.map(|cwd| cwd.display().to_string()),
            ..test_open_params(session_id.clone(), None)
        };
        requested_open_workspace(&state, on, Some(PROFILE), &params, false).map_err(error_kind)
    };
    assert_eq!(open(&a, None), Ok(Some(root_a.clone())));
    assert_eq!(open(&a, Some(&root_a)), Ok(Some(root_a.clone())));
    assert_eq!(
        open(&a, Some(&root_b)),
        Err(json!("chat_workspace_mismatch"))
    );
    // Nor in a chat's folder, or the data directory or `chats` folder that
    // holds them. A folder above the data directory stays open.
    let holders = [chats.clone(), profile.data_dir.clone()];
    assert!(open(&code, Some(temp.path())).is_ok());
    for cwd in [root_a.clone(), root_a.join("sub")]
        .into_iter()
        .chain(holders)
    {
        std::fs::create_dir_all(&cwd).unwrap();
        assert_eq!(
            open(&code, Some(&cwd)),
            Err(json!("chat_workspace_reserved"))
        );
    }

    // Read-only and offline by default; never the network or the host.
    let permission = effective_session_permission_state(&state, &a).selection;
    assert_eq!(permission.mode, PermissionProfileMode::ReadOnly);
    assert_eq!(permission.network, PermissionNetworkPolicy::Deny);
    let set = |mode, network| {
        permission_profile_set_result(
            &state,
            PermissionProfileSetParams {
                session_id: b.clone(),
                update: octos_core::ui_protocol::PermissionProfileUpdate {
                    mode,
                    network,
                    approval_policy: None,
                },
                runtime_mode: None,
            },
        )
        .map(|set| set.current.mode)
        .map_err(error_kind)
    };
    let bound = Err(json!("chat_permission_bound"));
    let full = Some(PermissionProfileMode::DangerFullAccess);
    assert_eq!(set(None, Some(PermissionNetworkPolicy::Allow)), bound);
    assert_eq!(set(full, None), bound);
    let write = PermissionProfileMode::WorkspaceWrite;
    assert_eq!(set(Some(write), None), Ok(write));

    let list = |features| {
        let params = SessionListParams {
            work_mode: Some(WorkMode::Chat),
            profile_id: Some(PROFILE.into()),
            ..Default::default()
        };
        let state = Arc::clone(&state);
        async move { chat_session_list(&state, None, features, &params).await }
    };
    let listed = list(on).await.unwrap();
    let rows = listed["sessions"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    for (id, root) in [(&a, &root_a), (&b, &root_b)] {
        let row = rows.iter().find(|row| row["id"] == json!(id)).unwrap();
        assert_eq!(
            (&row["workspace_root"], &row["work_mode"]),
            (&json!(root), &json!("chat"))
        );
    }
    assert_eq!(listed["work_mode"], "chat");
    let refused = list(ConnectionUiFeatures::default()).await.unwrap_err();
    assert_eq!(error_kind(refused), "feature_required");

    let delete = async |id: &SessionKey| {
        let response = crate::api::handlers::delete_session(
            axum::extract::State(Arc::clone(&state)),
            HeaderMap::new(),
            Some(axum::Extension(crate::api::router::AuthIdentity::Admin)),
            axum::extract::Path(id.to_string()),
        )
        .await;
        response.status()
    };
    let gone = |id: &SessionKey| matches!(chat_workspace::lookup(&profile.data_dir, id), Ok(None));
    // A folder that cannot be removed fails the delete and leaves the whole
    // chat, transcript included, so deleting it again retries.
    chat_workspace::FAIL_FOLDER_REMOVAL.with(|fail| fail.set(true));
    assert!(delete(&a).await.is_server_error());
    chat_workspace::FAIL_FOLDER_REMOVAL.with(|fail| fail.set(false));
    let transcript = octos_bus::SessionHandle::session_exists(&profile.data_dir, &a);
    assert!(root_a.is_dir() && !gone(&a) && transcript);
    assert!(delete(&a).await.is_success() && gone(&a));
    assert!(!root_a.exists());
    assert!(root_b.is_dir());

    // A recorded chat whose folder is gone, or is a symlink, stays a chat: it
    // is refused, never run elsewhere, keeps its limits, and still deletes.
    let elsewhere = temp.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::remove_dir_all(&root_b).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&elsewhere, &root_b).unwrap();
    let unavailable = json!("chat_workspace_unavailable");
    assert_eq!(open(&b, Some(&elsewhere)), Err(unavailable.clone()));
    assert_eq!(create_chat(&state, b.clone()).await, Err(unavailable));
    assert_eq!(set(full, None), bound);
    assert!(writable_runtime(&profile, &b, &elsewhere).await.is_err());
    assert!(delete(&b).await.is_success() && gone(&b));
    assert!(elsewhere.is_dir() && !root_b.exists());
}

/// TEST-DIET category 3: a chat's tools refuse `../`, absolute paths and
/// symlinks that lead out of the chat's folder.
#[tokio::test]
async fn chat_tools_cannot_leave_the_chat_folder() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (state, profile) = state_with_profile_llm(temp.path(), PROFILE, unused_llm()).await;
    let (a, b) = (chat_key("a"), chat_key("b"));
    let root_a = create_chat(&state, a.clone()).await.expect("chat a");
    let root_b = create_chat(&state, b.clone()).await.expect("chat b");
    std::fs::write(root_a.join("note.txt"), "mine").unwrap();
    std::fs::write(root_b.join("secret.txt"), "theirs").unwrap();
    std::fs::write(profile.data_dir.join("profile.txt"), "profile").unwrap();
    let research = profile.data_dir.join("research");
    std::fs::create_dir_all(&research).unwrap();
    std::fs::write(research.join("shared.txt"), "shared").unwrap();
    let b_name = root_b.file_name().unwrap().to_string_lossy().into_owned();
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut escapes = vec![
        format!("../{b_name}/secret.txt"),
        "../../profile.txt".to_owned(),
        "../../research/shared.txt".to_owned(),
        research.join("shared.txt").display().to_string(),
        root_b.join("secret.txt").display().to_string(),
        profile.data_dir.join("profile.txt").display().to_string(),
    ];
    // The symlinks into the other chat exist only on unix.
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(root_b.join("secret.txt"), root_a.join("leak.txt")).unwrap();
        std::os::unix::fs::symlink(&root_b, root_a.join("other")).unwrap();
        escapes.extend(["leak.txt".to_owned(), "other/secret.txt".to_owned()]);
    }

    // Even a write-enabled chat runs in its folder with the chat tools only,
    // and its transcript stays out of the folder.
    let runtime = writable_runtime(&profile, &a, &root_b)
        .await
        .expect("chat runtime");
    assert_eq!(
        std::fs::canonicalize(&runtime.workspace_root).unwrap(),
        root_a
    );
    assert_eq!(runtime.sessions_root, profile.data_dir);
    let names = runtime.tools.tool_names();
    assert!(names.contains(&"read_file".to_owned()), "{names:?}");
    for name in &names {
        assert!(
            chat_workspace::CHAT_TOOLS.contains(&name.as_str()),
            "{name}"
        );
    }
    let run = |name: &'static str, args: Value| run_tool(&runtime, name, args);
    let read = run("read_file", json!({ "path": "note.txt" })).await;
    assert!(read.is_ok_and(|output| output.contains("mine")));
    let write = run(
        "write_file",
        json!({ "path": "draft.txt", "content": "ok" }),
    )
    .await;
    assert!(
        write.is_ok() && root_a.join("draft.txt").is_file(),
        "{write:?}"
    );
    // Neither read_file nor write_file gets through `path`.
    let refused = async |path: &str| {
        let read = run("read_file", json!({ "path": path })).await;
        let write = run("write_file", json!({ "path": path, "content": "planted" })).await;
        read.is_err() && write.is_err()
    };
    for path in &escapes {
        assert!(refused(path).await, "{path}");
    }
    assert_eq!(
        std::fs::read_to_string(root_b.join("secret.txt")).unwrap(),
        "theirs"
    );
    assert_eq!(
        std::fs::read_to_string(profile.data_dir.join("profile.txt")).unwrap(),
        "profile"
    );

    // An ordinary session rooted above the data directory, as at a project
    // or home holding it, reads its own files but nothing in a chat folder.
    std::fs::write(temp.path().join("project.txt"), "project").unwrap();
    let code = writable_runtime(&profile, &chat_key("code"), temp.path()).await;
    let code = code.expect("code runtime");
    let project = run_tool(&code, "read_file", json!({ "path": "project.txt" })).await;
    assert!(project.is_ok_and(|output| output.contains("project")));
    let secret = root_b.join("secret.txt");
    let below = secret.strip_prefix(std::fs::canonicalize(temp.path()).unwrap());
    for path in [secret.clone(), below.unwrap().to_path_buf()] {
        let read = run_tool(&code, "read_file", json!({ "path": path })).await;
        assert!(read.is_err(), "{}", path.display());
    }
    let found = run_tool(&code, "grep", json!({ "pattern": "theirs|project" })).await;
    let found = found.unwrap();
    assert!(
        found.contains("project.txt") && !found.contains("secret"),
        "{found}"
    );

    // `send_file` as a chat turn builds it, pinned to the folder the runtime
    // was admitted with: the chat's folder, not even the shared uploads root.
    let uploads = octos_core::file_handle::temp_upload_root();
    std::fs::create_dir_all(&uploads).unwrap();
    let upload = tempfile::tempdir_in(&uploads).unwrap();
    std::fs::write(upload.path().join("up.txt"), "upload").unwrap();
    let sent_escapes = [upload.path().join("up.txt").display().to_string()];
    let (tx, mut queued) = mpsc::channel(8);
    let chat_send_file = || {
        let admitted = runtime.chat_root.clone().expect("a chat runtime");
        octos_agent::SendFileTool::with_context(tx.clone(), "api", a.0.clone())
            .with_base_dir(runtime.workspace_root.clone())
            .with_base_dir_only(admitted)
    };
    let sends = async |tool: &octos_agent::SendFileTool, path: &str| {
        let args = json!({ "file_path": path });
        let sent = octos_agent::tools::Tool::execute(tool, &args).await;
        sent.unwrap().success
    };
    let send_file = chat_send_file();
    assert!(sends(&send_file, "note.txt").await, "send_file note.txt");
    for path in escapes.iter().chain(&sent_escapes) {
        assert!(!sends(&send_file, path).await, "send_file {path}");
    }

    // An open chat whose folder is swapped for a symlink out of the profile:
    // the cached runtime is refused, the tools it built, or a turn on it
    // builds after the swap, stay pinned to the real folder, and a delivery
    // queued before the swap copies nothing from the replacement.
    #[cfg(unix)]
    {
        let outside = tempfile::tempdir().unwrap();
        for name in ["secret.txt", "note.txt"] {
            std::fs::write(outside.path().join(name), "outside").unwrap();
        }
        let (permissions, cache) = (runtime.permissions, &state.session_cache);
        let epoch = cache.session_generation(&a);
        let open = async || {
            let hint = Some(root_a.clone());
            cache
                .get_or_init_with_permissions(&profile, a.clone(), hint, permissions, epoch)
                .await
        };
        open().await.expect("cached chat runtime");
        std::fs::rename(&root_a, temp.path().join("moved")).unwrap();
        std::os::unix::fs::symlink(outside.path(), &root_a).unwrap();
        assert!(
            open().await.is_err(),
            "a cached chat with a replaced folder"
        );
        assert!(refused("secret.txt").await && refused("x.txt").await);
        assert!(!outside.path().join("x.txt").exists());
        for tool in [send_file, chat_send_file()] {
            let sent = sends(&tool, "secret.txt").await;
            assert!(!sent, "send_file after the swap");
        }
        let delivery = queued.try_recv().expect("the queued note.txt delivery");
        let reference = delivery.media[0].clone();
        let (sessions, retention) = (&runtime.sessions, profile.artifact_retention);
        persist_send_file_delivery(sessions, &profile.data_dir, &a, "t", retention, delivery).await;
        let copy =
            octos_bus::session_artifacts::delivered_copy_path(&profile.data_dir, &a, &reference);
        assert!(!copy.exists(), "a download copy of {reference}");
    }
}
