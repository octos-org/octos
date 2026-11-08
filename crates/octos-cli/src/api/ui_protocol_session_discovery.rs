async fn ensure_session_discovery_record(
    sessions: &Arc<tokio::sync::Mutex<octos_bus::SessionManager>>,
    session_id: &SessionKey,
) -> Result<(), RpcError> {
    sessions
        .lock()
        .await
        .ensure_session_record(session_id)
        .await
        .map_err(|error| {
            if error.chain().any(|cause| cause.to_string() == "session was deleted") {
                RpcError::unknown_session(session_id.0.clone())
            } else {
                runtime_unavailable_error(format!(
                    "failed to persist session discovery record: {error}"
                ))
            }
        })
}

async fn deleted_session_state(
    state: &Arc<AppState>,
    connection_profile_id: Option<&str>,
    routed_profile_id: Option<&str>,
    session_id: &SessionKey,
) -> Result<bool, RpcError> {
    let Some(sessions) = resolve_sessions_for_lookup(
        state,
        connection_profile_id,
        routed_profile_id,
        session_id,
    )
    .await
    else {
        return Ok(false);
    };
    sessions
        .lock()
        .await
        .session_was_deleted(session_id)
        .await
        .map_err(|error| {
            runtime_unavailable_error(format!("failed to read session discovery state: {error}"))
        })
}

async fn note_first_turn_discovery(
    state: &Arc<AppState>,
    connection_profile_id: Option<&str>,
    routed_profile_id: Option<&str>,
    session_id: &SessionKey,
    prompt: &str,
) -> Option<RpcError> {
    let sessions =
        resolve_sessions_for_lookup(state, connection_profile_id, routed_profile_id, session_id)
            .await?;
    sessions
        .lock()
        .await
        .note_session_prompt(session_id, prompt)
        .await
        .err()
        .map(|error| {
            runtime_unavailable_error(format!(
                "failed to persist first-turn discovery metadata: {error}"
            ))
        })
}
