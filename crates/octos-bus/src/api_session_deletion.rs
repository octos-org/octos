/// DELETE /sessions/:id — delete a session.
async fn handle_delete_session(
    State(state): State<ApiState>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    if let Some(rejection) = unauthorized_without_bearer(&state, &headers) {
        return rejection;
    }

    // Finish the scoped mutation even if the HTTP caller disconnects. In
    // particular, its in-progress admission fence must never be left behind.
    match tokio::spawn(async move { delete_session_inner(state, id).await }).await {
        Ok(response) => response,
        Err(error) => {
            tracing::error!(%error, "session deletion task failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not delete session",
            )
                .into_response()
        }
    }
}

async fn delete_session_inner(state: ApiState, id: String) -> Response {
    let requested = api_session_mutation_candidates(state.profile_id.as_deref(), &id);
    let chat = requested[0].chat_id();
    let admission = state.pending.lock().await.admission(chat);
    let _deletion = admission.deletion.lock().await;
    // Wait for admitted actor opens/enqueues, then publish a precise revocation
    // scope. Drop the dispatch lock before filesystem work.
    admission.state.write().await.deleting = requested.clone();
    let active_keys: Vec<_> = state
        .pending
        .lock()
        .await
        .session_keys(state.profile_id.as_deref())
        .into_iter()
        .filter(|owner| {
            requested
                .iter()
                .any(|key| session_delete_matches(key, owner))
        })
        .collect();
    let mut sess = state.sessions.lock().await;
    let mut candidates = match sess.mutation_scope_keys(&requested).await {
        Ok(keys) => keys,
        Err(error) => {
            drop(sess);
            admission.state.write().await.deleting.clear();
            tracing::error!(%error, "enumerate session deletion scope failed");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not enumerate session topics",
            )
                .into_response();
        }
    };
    candidates.extend(active_keys.iter().cloned());
    candidates.sort_by(|a, b| a.0.cmp(&b.0));
    candidates.dedup();
    let mut deleted = false;
    for candidate in &candidates {
        match sess.clear_existing(candidate).await {
            Ok(existed) => deleted |= existed,
            Err(error) => {
                drop(sess);
                admission.state.write().await.deleting.clear();
                tracing::error!(session_key = %candidate, %error, "delete session from gateway store failed");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "could not delete session",
                )
                    .into_response();
            }
        }
    }
    drop(sess);
    let mut admission_state = admission.state.write().await;
    admission_state.record_deletion(&requested);
    admission_state.deleting.clear();
    state
        .pending
        .lock()
        .await
        .remove_sessions(state.profile_id.as_deref(), &requested);
    if (deleted || !active_keys.is_empty())
        && let Some(ref cb) = state.on_session_deleted
    {
        cb(&id);
        for key in candidates {
            if key.0 != id && !requested.contains(&key) {
                cb(&key.0);
            }
        }
    }
    // No session found — still return 204 (idempotent delete).
    StatusCode::NO_CONTENT.into_response()
}
