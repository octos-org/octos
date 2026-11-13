// UPCR-2026-R1EFC56A3: one server-created workspace per chat. Included by
// ui_protocol_transport.rs. Folder, record and the session-runtime fence live
// in `crate::runtime::chat_workspace`; this file is the protocol side:
// `session/chat/create`, `session/list { work_mode: "chat" }` and the
// `session/open` binding.

use crate::runtime::chat_workspace::{self, ChatWorkspace};
use octos_core::ui_protocol::{ChatCreateParams, ChatCreateResult, SessionListResult};

/// The chat `session_id` is, or an error for a recorded chat whose workspace
/// is unusable. Chats always carry their profile in the key.
fn session_chat_workspace(
    state: &AppState,
    session_id: &SessionKey,
) -> std::io::Result<Option<ChatWorkspace>> {
    let Some(profile) = session_id
        .profile_id()
        .and_then(|profile_id| resolve_session_profile_runtime(state, Some(profile_id)))
    else {
        return Ok(None);
    };
    chat_workspace::lookup(&profile.data_dir, session_id)
}

/// Whether `session_id` is a chat, including one whose workspace is unusable:
/// a chat's limits apply to it either way.
fn is_chat_session(state: &AppState, session_id: &SessionKey) -> bool {
    !matches!(session_chat_workspace(state, session_id), Ok(None))
}

/// A recorded chat whose folder is missing, unreadable or a symlink is
/// refused, never opened or recreated as something else.
fn chat_workspace_unavailable(session_id: &SessionKey, error: std::io::Error) -> RpcError {
    tracing::warn!(session_key = %session_id, %error, "chat workspace unusable");
    RpcError::permission_denied("this chat's workspace is unavailable")
        .with_data(json!({ "kind": "chat_workspace_unavailable" }))
}

/// Delete `session_id`'s chat folder and record, if it is a chat. A failure
/// leaves the chat listed, so deleting the session again retries it.
pub(crate) fn remove_session_chat_workspace(state: &AppState, session_id: &SessionKey) {
    let Some(profile) = session_id
        .profile_id()
        .and_then(|profile_id| resolve_session_profile_runtime(state, Some(profile_id)))
    else {
        return;
    };
    if let Err(error) = chat_workspace::remove(&profile.data_dir, session_id) {
        tracing::warn!(session_key = %session_id, %error, "failed to remove the chat workspace");
    }
}

/// Whether `path` lies in any profile's chat folders.
fn path_in_any_chat_folder(state: &AppState, path: &Path) -> bool {
    let dynamic = dynamic_profile_runtimes()
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .values()
        .map(|runtime| runtime.data_dir.clone())
        .collect::<Vec<_>>();
    state
        .profiles
        .values()
        .map(|runtime| runtime.data_dir.clone())
        .chain(dynamic)
        .any(|data_dir| chat_workspace::is_in_chat_folder(&data_dir, path))
}

/// The workspace `session/open` uses: a chat's own folder (a different `cwd`
/// is refused), otherwise the validated request, which may not be a chat's.
fn requested_open_workspace(
    state: &AppState,
    features: ConnectionUiFeatures,
    active_profile_id: Option<&str>,
    params: &SessionOpenParams,
    session_create: bool,
) -> Result<Option<PathBuf>, RpcError> {
    let chat = session_chat_workspace(state, &params.session_id)
        .map_err(|error| chat_workspace_unavailable(&params.session_id, error))?;
    if let Some(chat) = chat {
        let cwd = params.cwd.as_deref().map(str::trim).filter(|cwd| !cwd.is_empty());
        if cwd.is_some_and(|cwd| std::fs::canonicalize(cwd).ok().as_ref() != Some(&chat.root)) {
            return Err(
                RpcError::invalid_params("a chat opens only in its own workspace").with_data(
                    json!({ "kind": "chat_workspace_mismatch", "workspace_root": chat.root }),
                ),
            );
        }
        return Ok(Some(chat.root));
    }
    let requested =
        validate_requested_session_cwd(state, features, active_profile_id, params, session_create)?;
    if requested.as_deref().is_some_and(|root| path_in_any_chat_folder(state, root)) {
        return Err(
            RpcError::permission_denied("this folder belongs to a chat")
                .with_data(json!({ "kind": "chat_workspace_reserved" })),
        );
    }
    Ok(requested)
}

async fn handle_session_chat_create(
    ws: &WsConnection,
    state: &Arc<AppState>,
    connection_profile_id: Option<&str>,
    id: String,
    params: ChatCreateParams,
) {
    match session_chat_create_result(state, connection_profile_id, params).await {
        Ok(result) => {
            let _ = send_rpc_result(ws, id, json!(result));
        }
        Err(error) => {
            let _ = send_rpc_error(ws, Some(id), error);
        }
    }
}

async fn session_chat_create_result(
    state: &Arc<AppState>,
    connection_profile_id: Option<&str>,
    params: ChatCreateParams,
) -> Result<ChatCreateResult, RpcError> {
    let ChatCreateParams {
        session_id,
        profile_id,
    } = params;
    // The chat is one of this profile's API sessions: `<profile_id>:api:<chat>`.
    if session_id.profile_id() != Some(profile_id.as_str())
        || session_id.channel() != "api"
        || session_id.topic().is_some()
    {
        return Err(RpcError::invalid_params(
            "session_id must be an api session key of profile_id, without a topic",
        )
        .with_data(json!({ "kind": "chat_session_id_invalid" })));
    }
    validate_session_scope(&session_id, Some(&profile_id), connection_profile_id)?;
    ensure_known_profile(state, &profile_id)?;
    ensure_session_profile_runtime(state, Some(&profile_id)).await?;
    let profile = resolve_session_profile_runtime(state, Some(&profile_id))
        .ok_or_else(|| runtime_unavailable_error("profile runtime unavailable"))?;
    let sessions = resolve_sessions_for_lookup(state, connection_profile_id, None, &session_id)
        .await
        .ok_or_else(|| runtime_unavailable_error("Sessions not available"))?;
    let already_chat = chat_workspace::lookup(&profile.data_dir, &session_id)
        .map_err(|error| chat_workspace_unavailable(&session_id, error))?
        .is_some();
    // An existing session keeps whatever workspace it had.
    if !already_chat && sessions.lock().await.session_known(&session_id) {
        return Err(
            RpcError::invalid_params("a session with this id already exists")
                .with_data(json!({ "kind": "session_exists" })),
        );
    }
    let data_dir = profile.data_dir.clone();
    let (create_id, create_profile) = (session_id.clone(), profile_id.clone());
    let chat = tokio::task::spawn_blocking(move || {
        chat_workspace::create(&data_dir, &create_profile, &create_id)
    })
    .await
    .map_err(|error| runtime_unavailable_error(format!("chat workspace task failed: {error}")))?
    .map_err(|error| runtime_unavailable_error(format!("could not create the chat workspace: {error}")))?;
    let stored = sessions
        .lock()
        .await
        .ensure_session_record_in_mode(&session_id, Some(WorkMode::Chat.as_str()))
        .await
        .map_err(|error| {
            runtime_unavailable_error(format!("failed to persist the chat session: {error}"))
        })?;
    if WorkMode::from_stored(stored.as_deref()) != WorkMode::Chat {
        // A concurrent open created it as a Code session first: it keeps its
        // own workspace, so the folder made above is not its.
        if let Err(error) = chat_workspace::remove(&profile.data_dir, &session_id) {
            tracing::warn!(session_key = %session_id, %error, "failed to remove an unused chat workspace");
        }
        return Err(RpcError::invalid_params("this session was not created as a chat")
            .with_data(json!({ "kind": "work_mode_mismatch", "work_mode": WorkMode::Code })));
    }
    Ok(ChatCreateResult {
        session_id,
        profile_id,
        workspace_root: chat.root.to_string_lossy().into_owned(),
        work_mode: WorkMode::Chat,
    })
}

/// `session/list { work_mode: "chat", profile_id }`: the profile's chats, each
/// with its own `workspace_root`, newest first.
async fn chat_session_list(
    state: &Arc<AppState>,
    connection_profile_id: Option<&str>,
    features: ConnectionUiFeatures,
    params: &SessionListParams,
) -> Result<Value, RpcError> {
    if !features.session_chat_workspace_v1 {
        return Err(RpcError::invalid_params(
            "session/list work_mode requires feature session.chat_workspace.v1",
        )
        .with_data(json!({
            "kind": "feature_required",
            "feature": octos_core::ui_protocol::UI_PROTOCOL_FEATURE_SESSION_CHAT_WORKSPACE_V1,
        })));
    }
    if params.work_mode != Some(WorkMode::Chat)
        || params.cwd.is_some()
        || params.workspace_id.is_some()
        || params.cursor.is_some()
    {
        return Err(RpcError::invalid_params(
            "session/list work_mode lists a profile's chats: work_mode \"chat\" with profile_id only",
        ));
    }
    let profile_id = match (connection_profile_id, params.profile_id.as_deref()) {
        (Some(connection), Some(requested)) if requested != connection => {
            return Err(authenticated_scope_mismatch_error(
                "profile_id is outside the authenticated profile",
                connection,
                Some(requested),
            ));
        }
        (Some(connection), _) => connection,
        (None, requested) => requested.unwrap_or(MAIN_PROFILE_ID),
    }
    .to_owned();
    ensure_known_profile(state, &profile_id)?;
    let profile = resolve_session_profile_runtime(state, Some(&profile_id))
        .ok_or_else(|| runtime_unavailable_error("profile runtime unavailable"))?;
    let busy = active_turn_sessions(&active_turns_registry()).await;
    let (data_dir, listed_profile) = (profile.data_dir.clone(), profile_id.clone());
    let sessions = tokio::task::spawn_blocking(move || {
        let listed = super::handlers::list_profile_sessions(&data_dir, &busy, &listed_profile);
        let mut rows = chat_workspace::list(&data_dir)
            .into_iter()
            .filter(|chat| chat.profile_id == listed_profile)
            .map(|chat| {
                let info = listed.iter().find(|row| {
                    row.id == chat.session_id.0
                        || SessionKey::with_profile(&listed_profile, "api", &row.id)
                            == chat.session_id
                });
                let mut row = json!(super::handlers::SessionInfo {
                    id: chat.session_id.0.clone(),
                    message_count: info.map_or(0, |info| info.message_count),
                    title: info.and_then(|info| info.title.clone()),
                    updated_at: info.and_then(|info| info.updated_at.clone()),
                    last_prompt: info.and_then(|info| info.last_prompt.clone()),
                    active_turn: busy.contains(&chat.session_id),
                    work_mode: Some(WorkMode::Chat),
                });
                row["workspace_root"] = json!(chat.root);
                row
            })
            .collect::<Vec<_>>();
        rows.sort_by(|a, b| b["updated_at"].as_str().cmp(&a["updated_at"].as_str()));
        rows
    })
    .await
    .map_err(|error| runtime_unavailable_error(format!("chat listing task failed: {error}")))?;
    Ok(json!(SessionListResult {
        sessions: json!(sessions),
        profile_id: Some(profile_id),
        work_mode: Some(WorkMode::Chat),
        workspace_root: None,
        next_cursor: None,
        scope: None,
    }))
}
