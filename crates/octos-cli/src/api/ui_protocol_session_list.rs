#[allow(clippy::too_many_arguments)]
async fn handle_session_list(
    ws: &WsConnection,
    state: &Arc<AppState>,
    headers: &HeaderMap,
    identity: Option<&AuthIdentity>,
    connection_profile_id: Option<&str>,
    connection_is_external: bool,
    features: ConnectionUiFeatures,
    id: String,
    params: SessionListParams,
) {
    if connection_is_external && features.workspace_session_refs_v1 {
        let _ = send_rpc_error(
            ws,
            Some(id),
            RpcError::permission_denied(
                "session/list SessionRef catalogs are not available to external clients",
            )
            .with_data(json!({ "kind": "external_method_denied" })),
        );
        return;
    }
    if features.workspace_session_refs_v1 {
        return handle_session_ref_list(
            ws,
            state,
            headers,
            identity,
            connection_profile_id,
            id,
            params,
        )
        .await;
    }
    if params.workspace_id.is_some() || params.cursor.is_some() || params.limit.is_some() {
        let _ = send_rpc_error(
            ws,
            Some(id),
            RpcError::invalid_params(
                "workspace_id, cursor and limit require feature workspace.session_refs.v1",
            )
            .with_data(json!({
                "kind": "feature_required",
                "feature": octos_core::ui_protocol::UI_PROTOCOL_FEATURE_WORKSPACE_SESSION_REFS_V1,
            })),
        );
        return;
    }
    // Per-project (`appui.sessions_in_cwd`) scoping. When the client sends a
    // `cwd` AND the server flag is on AND the connection negotiated
    // `session.workspace_cwd.v1`, the listing is scoped to `<cwd>/.octos`.
    // Absent cwd / flag off → `None` → byte-identical legacy listing.
    let cwd_scope =
        match resolve_session_list_cwd_root(state, features, connection_profile_id, &params) {
            Ok(scope) => scope,
            Err(error) => {
                let _ = send_rpc_error(ws, Some(id), error);
                return;
            }
        };
    // When the listing is project-scoped, the effective profile is the one
    // the store was resolved for (which may come from `params.profile_id` on
    // an admin connection), so `active_turn` is stamped against the same
    // wire keys session/open registers under.
    let connection_profile_id = cwd_scope
        .as_ref()
        .map(|scope| scope.profile_id.as_str())
        .or(connection_profile_id);
    // Per-session busy state. Read from the PROCESS-global registry, not this
    // connection's `connection_turns`, so the flag is honest about a session
    // another client — the TUI next to this browser tab — is mid-turn in but
    // this connection never opened. Taken as one snapshot BEFORE the listing
    // so no sessions lock is ever held while the registry lock is.
    let busy_sessions = active_turn_sessions(&active_turns_registry()).await;
    let result = super::handlers::SessionQueryService::new(state.clone())
        .list(
            headers,
            identity,
            connection_profile_id,
            cwd_scope.as_ref().map(|scope| scope.sessions_root.clone()),
            &busy_sessions,
        )
        .await;
    let method = octos_core::ui_protocol::methods::SESSION_LIST;
    // Collection endpoint — no addressable session id. Treat any
    // (unexpected) 404 as a generic resource-not-found rather than
    // an `UNKNOWN_SESSION` miss.
    let context = RestResourceContext::resource("session", "");
    match result {
        Ok(sessions) => {
            if let Some(scope) = cwd_scope.as_ref() {
                remember_listed_session_workspaces(&sessions, scope);
            }
            send_aux_rpc_result(
                ws,
                id,
                method,
                session_list_result_value(sessions, cwd_scope.as_ref()),
            );
        }
        Err(error) => {
            let _ = send_rpc_error(
                ws,
                Some(id),
                session_query_error_to_rpc(method, &error, &context),
            );
        }
    }
}

async fn handle_session_ref_list(
    ws: &WsConnection,
    state: &Arc<AppState>,
    headers: &HeaderMap,
    identity: Option<&AuthIdentity>,
    connection_profile_id: Option<&str>,
    id: String,
    params: SessionListParams,
) {
    let profile_id = connection_profile_id.unwrap_or(MAIN_PROFILE_ID);
    let busy_sessions = active_turn_sessions(&active_turns_registry()).await;
    let result = super::handlers::SessionQueryService::new(state.clone())
        .list(headers, identity, connection_profile_id, None, &busy_sessions)
        .await;
    let method = octos_core::ui_protocol::methods::SESSION_LIST;
    let context = RestResourceContext::resource("session", "");
    let global = match result {
        Ok(value) => match serde_json::from_value::<Vec<super::handlers::SessionInfo>>(value) {
            Ok(sessions) => sessions,
            Err(error) => {
                let _ = send_rpc_error(
                    ws,
                    Some(id),
                    RpcError::internal_error(format!("failed to decode session catalog: {error}")),
                );
                return;
            }
        },
        Err(error) => {
            let _ = send_rpc_error(
                ws,
                Some(id),
                session_query_error_to_rpc(method, &error, &context),
            );
            return;
        }
    };
    let catalog_state = Arc::clone(state);
    let catalog_profile_id = profile_id.to_owned();
    let result = match tokio::task::spawn_blocking(move || {
        super::workspace_protocol::session_ref_catalog(
            &catalog_state,
            &catalog_profile_id,
            global,
            busy_sessions,
            params,
        )
    })
    .await
    {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => {
            let _ = send_rpc_error(ws, Some(id), error);
            return;
        }
        Err(error) => {
            let _ = send_rpc_error(
                ws,
                Some(id),
                RpcError::internal_error(format!("session catalog task failed: {error}")),
            );
            return;
        }
    };
    let _ = send_rpc_result(
        ws,
        id,
        serde_json::to_value(result).unwrap_or_else(|_| json!({})),
    );
}

#[allow(clippy::too_many_arguments)]
async fn handle_session_create(
    ws: &WsConnection,
    state: &Arc<AppState>,
    ledger: &Arc<UiProtocolLedger>,
    approvals: &PendingApprovalStore,
    questions: &PendingQuestionStore,
    live_forwarders: &SharedLiveForwarders,
    connection_profile_id: Option<&str>,
    pinned_profile_id: Option<&str>,
    features: ConnectionUiFeatures,
    id: String,
    params: octos_core::ui_protocol::SessionCreateParams,
) -> bool {
    let params = match super::workspace_registry::session_open_params_for_create(
        state,
        connection_profile_id,
        &params,
    ) {
        Ok(params) => params,
        Err(error) => {
            send_scope_error(ws, id, error);
            return false;
        }
    };
    handle_session_open_with_mode(
        ws,
        state,
        ledger,
        approvals,
        questions,
        live_forwarders,
        connection_profile_id,
        pinned_profile_id,
        features,
        id,
        params,
        false,
        true,
    )
    .await
}

/// Resolve an optional `session/list` `cwd` into the per-project, per-profile
/// session-store root (`<cwd>/.octos/<profile_id>`), applying the same gates
/// the write path uses so list and open agree on where a project's sessions
/// live:
///
/// 1. **Flag** — when `appui.sessions_in_cwd` is off, the `cwd` is ignored
///    entirely (`Ok(None)` → legacy per-profile/global listing). This keeps a
///    flag-off server byte-identical: nothing was ever written under
///    `<cwd>/.octos`, so scoping to it would only ever return an empty list.
/// 2. **Capability** — a `cwd` under the flag requires the connection to have
///    negotiated `session.workspace_cwd.v1` (mirrors `session/open`'s cwd
///    gate). A client that sends `cwd` without it gets a typed error rather
///    than a silently-global list.
/// 3. **Workspace safety** — the SAME [`validate_session_workspace_allowed`]
///    gate `session/open` runs: the cwd must canonicalize to a directory AND
///    must not be rooted under a banned system path. Without this a client
///    could point the listing at `/etc` (or any service-writable path) and
///    `SessionManager::open` would CREATE `<cwd>/.octos/…` there and enumerate
///    it. On rejection we surface the typed error (consistent with
///    `session/open`) rather than silently degrading.
/// 4. **Profile scope** — `params.profile_id` follows the session/open rules:
///    a user connection may only restate its own profile (anything else is an
///    `auth_scope_violation`), an admin/token connection may name the profile
///    it opens sessions under, and an unregistered profile is rejected by the
///    same runtime gate as above.
/// 5. **Profile namespace** — the store root is
///    `<cwd>/.octos/<profile_id>` (via [`project_sessions_root`]), matching
///    the write path so two profiles that share a project cwd never read each
///    other's transcripts.
///
/// A trimmed-empty or absent `cwd` is always `Ok(None)` (legacy listing).
fn resolve_session_list_cwd_root(
    state: &AppState,
    features: ConnectionUiFeatures,
    connection_profile_id: Option<&str>,
    params: &SessionListParams,
) -> Result<Option<SessionListScope>, RpcError> {
    let Some(cwd) = params
        .cwd
        .as_deref()
        .map(str::trim)
        .filter(|cwd| !cwd.is_empty())
    else {
        return Ok(None);
    };
    // Flag off → the store was never relocated; ignore the cwd (legacy).
    if !state.session_cache.sessions_in_cwd() {
        return Ok(None);
    }
    if !features.session_workspace_cwd {
        return Err(RpcError::invalid_params(
            "session/list cwd requires feature session.workspace_cwd.v1",
        )
        .with_data(json!({
            "kind": "feature_required",
            "feature": UI_PROTOCOL_FEATURE_SESSION_WORKSPACE_CWD_V1,
        })));
    }
    // Profile precedence mirrors session/open (`validate_session_scope`): a
    // user connection is frozen to its own profile and may only restate it;
    // an admin/token connection (no connection profile) may name the profile
    // it opens sessions under, so the listing reads the SAME
    // `<cwd>/.octos/<profile>` store those sessions were written to.
    let requested_profile_id = params.profile_id.as_deref();
    if requested_profile_id.is_some_and(str::is_empty) {
        return Err(RpcError::invalid_params("profile_id cannot be empty"));
    }
    let active_profile_id = match (connection_profile_id, requested_profile_id) {
        (Some(connection), Some(requested)) if requested != connection => {
            return Err(authenticated_scope_mismatch_error(
                "profile_id is outside the authenticated profile",
                connection,
                Some(requested),
            ));
        }
        (Some(connection), _) => Some(connection),
        (None, requested) => requested,
    };
    let workspace_root = canonical_existing_dir(cwd)?;
    // SAME safety gate as session/open — reject banned system roots (and the
    // missing-profile-runtime case) BEFORE opening a SessionManager that would
    // otherwise materialize `<cwd>/.octos` at an arbitrary path.
    validate_session_workspace_allowed(state, active_profile_id, &workspace_root)?;
    // Namespace by the SAME profile the write path uses so the listing reads
    // exactly the project store session/open writes for this scope.
    let profile_id = resolve_session_profile_runtime(state, active_profile_id)
        .map(|runtime| runtime.profile_id.clone())
        .unwrap_or_else(|| active_profile_id.unwrap_or(MAIN_PROFILE_ID).to_string());
    Ok(Some(SessionListScope {
        sessions_root: crate::runtime::session::project_sessions_root(&workspace_root, &profile_id),
        workspace_root,
        profile_id,
    }))
}

/// A `session/list` that was scoped to one project store: the canonical
/// workspace root, the `<root>/.octos/<profile>` store it read, and that
/// profile.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionListScope {
    workspace_root: PathBuf,
    sessions_root: PathBuf,
    profile_id: String,
}

/// A sidebar mutation need not open the listed session first. Preserve the
/// location attested by an authorized, cwd-scoped listing across that gap,
/// including after a server restart. An explicit live open always wins.
fn remember_listed_session_workspaces(sessions: &Value, scope: &SessionListScope) {
    let Some(rows) = sessions.as_array() else {
        return;
    };
    for row in rows {
        let Some(id) = row.get("id").and_then(Value::as_str) else {
            continue;
        };
        let parsed = SessionKey(id.to_owned());
        if parsed.base_key().is_empty() {
            continue;
        }
        let key = if !parsed.base_key().contains(':') {
            SessionKey::with_profile(&scope.profile_id, "api", id)
        } else if parsed.profile_id() == Some(scope.profile_id.as_str())
            && parsed.channel() == "api"
        {
            parsed
        } else {
            // Listing a foreign/legacy channel row must not create a new
            // cross-profile or cross-channel mutation binding.
            continue;
        };
        session_workspaces().set_if_absent(
            &scope.profile_id,
            key,
            scope.workspace_root.clone(),
            Some(scope.workspace_root.clone()),
        );
    }
}

/// The `session/list` result body. A scoped listing attests the root and
/// profile it read (see `SessionListResult::workspace_root`); a legacy
/// listing stays the byte-identical `{ sessions }` it always was, which is
/// how a client tells the two apart for the same `{cwd}` request.
fn session_list_result_value(sessions: Value, scope: Option<&SessionListScope>) -> Value {
    serde_json::to_value(octos_core::ui_protocol::SessionListResult {
        sessions,
        workspace_root: scope.map(|scope| scope.workspace_root.to_string_lossy().into_owned()),
        profile_id: scope.map(|scope| scope.profile_id.clone()),
        next_cursor: None,
        scope: None,
    })
    .unwrap_or_else(|_| json!({}))
}
