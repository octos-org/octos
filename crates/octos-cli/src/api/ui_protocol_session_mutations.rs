async fn handle_session_title_set(
    ws: &WsConnection,
    state: &Arc<AppState>,
    headers: &HeaderMap,
    identity: Option<&AuthIdentity>,
    id: String,
    params: SessionTitleSetParams,
) {
    let method = octos_core::ui_protocol::methods::SESSION_TITLE_SET;
    let trimmed = params.title.trim();
    if trimmed.is_empty() {
        let _ = send_rpc_error(
            ws,
            Some(id),
            RpcError::invalid_params(format!("{method}: title must not be empty")),
        );
        return;
    }
    if trimmed.chars().count() > SESSION_TITLE_SET_MAX_CHARS {
        let _ = send_rpc_error(
            ws,
            Some(id),
            RpcError::invalid_params(format!(
                "{method}: title must be at most {SESSION_TITLE_SET_MAX_CHARS} chars"
            )),
        );
        return;
    }
    let session_id_str = params.session_id.clone();
    let result = super::handlers::SessionQueryService::new(state.clone())
        .title(headers, identity, &params.session_id, trimmed)
        .await;
    match result {
        Ok(()) => {
            send_aux_rpc_result(
                ws,
                id,
                method,
                json!({
                    "session_id": session_id_str,
                    "title": trimmed,
                }),
            );
        }
        Err(error) => {
            let context = RestResourceContext::session(session_id_str);
            let _ = send_rpc_error(
                ws,
                Some(id),
                rest_status_to_rpc_error(method, error.status(), error.title_detail(), &context),
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_session_delete(
    ws: &WsConnection,
    state: &Arc<AppState>,
    headers: &HeaderMap,
    identity: Option<&AuthIdentity>,
    live_forwarders: &SharedLiveForwarders,
    ledger: &Arc<UiProtocolLedger>,
    id: String,
    params: SessionDeleteParams,
) {
    let method = octos_core::ui_protocol::methods::SESSION_DELETE;
    let session_id_str = params.session_id.clone();
    let response = super::handlers::delete_session(
        State(state.clone()),
        headers.clone(),
        identity.cloned().map(axum::Extension),
        axum_path(params.session_id),
    )
    .await;
    let status = response.status();
    if status.is_success() {
        // R-05: the deleted session's live forwarder (if this connection
        // opened one) is now pumping a session that no longer exists —
        // retire it and free the broadcast subscriber slot instead of
        // leaving the task parked on a dead channel until reconnect.
        // session/open may have qualified the key with a topic suffix
        // (`session_key_with_optional_topic`); a delete addressed at the
        // bare id must still find that forwarder.
        let (forwarder, forwarder_key) = {
            let mut map = live_forwarders.lock().await;
            let bare = SessionKey(session_id_str.clone());
            let key = if map.contains_key(&bare) {
                bare
            } else {
                map.keys()
                    .find(|key| key.base_key() == session_id_str)
                    .cloned()
                    .unwrap_or(bare)
            };
            (map.remove(&key), key)
        };
        if let Some(forwarder) = forwarder {
            forwarder.abort();
            let _ = forwarder.await;
        }
        ledger.prune_subscriber_if_idle(&forwarder_key);
        send_aux_rpc_result(ws, id, method, json!({}));
    } else {
        let detail = axum::body::to_bytes(response.into_body(), AUX_REST_TO_WS_MAX_BODY_BYTES)
            .await
            .ok()
            .and_then(|bytes| String::from_utf8(bytes.to_vec()).ok());
        let context = RestResourceContext::session(session_id_str);
        let _ = send_rpc_error(
            ws,
            Some(id),
            rest_status_to_rpc_error(method, status, detail, &context),
        );
    }
}
