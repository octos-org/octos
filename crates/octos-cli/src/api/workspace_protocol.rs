//! Workspace/SessionRef OUP handlers kept out of the large transport module.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use octos_core::ui_protocol::{
    CatalogBootstrapParams, CatalogBootstrapResult, CatalogBootstrapSummary, RpcError,
    SessionListParams, SessionListResult,
};
use octos_core::{MAIN_PROFILE_ID, SessionKey};
use serde_json::json;

use super::AppState;
use super::handlers::SessionInfo;
use super::ui_protocol_transport::{WsConnection, send_rpc_error, send_rpc_result};

pub async fn handle_workspace_list(
    ws: &WsConnection,
    state: &Arc<AppState>,
    connection_profile_id: Option<&str>,
    id: String,
) {
    let profile_id = connection_profile_id.unwrap_or(MAIN_PROFILE_ID);
    match super::workspace_registry::list_workspaces(state, profile_id) {
        Ok(workspaces) => {
            let result = octos_core::ui_protocol::WorkspaceListResult { workspaces };
            let _ = send_rpc_result(
                ws,
                id,
                serde_json::to_value(result).unwrap_or_else(|_| json!({})),
            );
        }
        Err(error) => {
            let _ = send_rpc_error(ws, Some(id), error);
        }
    }
}

pub async fn handle_workspace_register(
    ws: &WsConnection,
    state: &Arc<AppState>,
    connection_profile_id: Option<&str>,
    id: String,
    params: octos_core::ui_protocol::WorkspaceRegisterParams,
) {
    let profile_id = connection_profile_id.unwrap_or(MAIN_PROFILE_ID);
    match super::workspace_registry::register_workspace(
        state,
        profile_id,
        &params.path,
        params.display_name,
    ) {
        Ok(workspace) => {
            let result = octos_core::ui_protocol::WorkspaceRegisterResult { workspace };
            let _ = send_rpc_result(
                ws,
                id,
                serde_json::to_value(result).unwrap_or_else(|_| json!({})),
            );
        }
        Err(error) => {
            let _ = send_rpc_error(ws, Some(id), error);
        }
    }
}

pub async fn handle_workspace_get(
    ws: &WsConnection,
    state: &Arc<AppState>,
    connection_profile_id: Option<&str>,
    id: String,
    params: octos_core::ui_protocol::WorkspaceGetParams,
) {
    let profile_id = connection_profile_id.unwrap_or(MAIN_PROFILE_ID);
    match super::workspace_registry::get_workspace(state, profile_id, &params.workspace_id) {
        Ok(workspace) => {
            let result = octos_core::ui_protocol::WorkspaceGetResult { workspace };
            let _ = send_rpc_result(
                ws,
                id,
                serde_json::to_value(result).unwrap_or_else(|_| json!({})),
            );
        }
        Err(error) => {
            let _ = send_rpc_error(ws, Some(id), error);
        }
    }
}

/// Resolve the server-authorized profile projection before touching any
/// catalog. A requested profile can narrow this scope but can never widen it.
fn catalog_profile_ids(
    state: &AppState,
    identity: Option<&super::router::AuthIdentity>,
    connection_profile_id: Option<&str>,
    routed_profile_id: Option<&str>,
    requested_profile_id: Option<&str>,
) -> Result<Vec<String>, RpcError> {
    let effective_scope = routed_profile_id.or(connection_profile_id);
    let authorized = |profile_id: &str| {
        identity.is_none_or(|identity| {
            super::auth_handlers::is_authorized_for_profile(state, identity, profile_id)
        })
    };
    let denied = || {
        RpcError::permission_denied("profile_id is outside the authenticated catalog scope")
            .with_data(json!({ "kind": "auth_scope_violation" }))
    };

    if requested_profile_id.is_some_and(|requested| requested.trim().is_empty()) {
        return Err(RpcError::invalid_params("profile_id must not be empty"));
    }
    if let Some(scope) = effective_scope {
        if !authorized(scope) || requested_profile_id.is_some_and(|requested| requested != scope) {
            return Err(denied());
        }
        // Keep the returned refs resolvable by session/open on this same
        // connection. A parent account's ability to administer a sub-account
        // does not silently widen an existing profile-scoped WS connection.
        return Ok(vec![scope.to_owned()]);
    }
    if let Some(requested) = requested_profile_id {
        if !authorized(requested) {
            return Err(denied());
        }
        return Ok(vec![requested.to_owned()]);
    }
    if let Some(super::router::AuthIdentity::User { id, .. }) = identity {
        return Ok(vec![id.clone()]);
    }
    // Only an unscoped admin (or an explicitly no-auth local server) reaches
    // the all-profile catalog. In particular, admin never falls back to an
    // implicit `_main`-only bootstrap.
    Ok(super::workspace_registry::scoped_profile_ids(state, None))
}

/// Serve the explicit post-auth bootstrap RPC. Auth responses stay small;
/// catalog I/O and keyset pagination run on the blocking pool.
#[allow(clippy::too_many_arguments)]
pub async fn handle_catalog_bootstrap(
    ws: &WsConnection,
    state: &Arc<AppState>,
    identity: Option<&super::router::AuthIdentity>,
    connection_profile_id: Option<&str>,
    routed_profile_id: Option<&str>,
    connection_is_external: bool,
    id: String,
    busy_sessions: HashSet<SessionKey>,
    params: CatalogBootstrapParams,
) {
    if connection_is_external {
        let _ = send_rpc_error(
            ws,
            Some(id),
            RpcError::permission_denied("catalog/bootstrap is not available to external clients")
                .with_data(json!({ "kind": "external_method_denied" })),
        );
        return;
    }
    let profile_ids = match catalog_profile_ids(
        state,
        identity,
        connection_profile_id,
        routed_profile_id,
        params.profile_id.as_deref(),
    ) {
        Ok(profiles) => profiles,
        Err(error) => {
            let _ = send_rpc_error(ws, Some(id), error);
            return;
        }
    };
    let mut global = HashMap::new();
    for profile_id in &profile_ids {
        // The selected profile is an already-authorized server scope, not a
        // caller-provided filesystem hint. Empty headers prevent a Host route
        // from overriding that per-profile selection while reusing the exact
        // legacy/global discovery query used by session/list.
        let result = super::handlers::SessionQueryService::new(state.clone())
            .list(
                &axum::http::HeaderMap::new(),
                identity,
                Some(profile_id),
                None,
                &busy_sessions,
            )
            .await;
        let value = match result {
            Ok(value) => value,
            Err(error) => {
                let detail = error
                    .detail()
                    .unwrap_or_else(|| "session query failed".to_owned());
                let _ = send_rpc_error(
                    ws,
                    Some(id),
                    RpcError::internal_error(format!(
                        "failed to query the authorized session catalog ({}): {detail}",
                        error.status()
                    )),
                );
                return;
            }
        };
        match serde_json::from_value::<Vec<SessionInfo>>(value) {
            Ok(sessions) => {
                global.insert(profile_id.clone(), sessions);
            }
            Err(error) => {
                let _ = send_rpc_error(
                    ws,
                    Some(id),
                    RpcError::internal_error(format!("failed to decode session catalog: {error}")),
                );
                return;
            }
        }
    }
    let state = state.clone();
    let result = tokio::task::spawn_blocking(move || {
        catalog_bootstrap(&state, &profile_ids, global, &busy_sessions, params)
    })
    .await;
    match result {
        Ok(Ok(result)) => match serde_json::to_value(result) {
            Ok(value) => {
                let _ = send_rpc_result(ws, id, value);
            }
            Err(error) => {
                let _ = send_rpc_error(
                    ws,
                    Some(id),
                    RpcError::internal_error(format!(
                        "failed to encode catalog bootstrap: {error}"
                    )),
                );
            }
        },
        Ok(Err(error)) => {
            let _ = send_rpc_error(ws, Some(id), error);
        }
        Err(error) => {
            let _ = send_rpc_error(
                ws,
                Some(id),
                RpcError::internal_error(format!("catalog bootstrap worker failed: {error}")),
            );
        }
    }
}

fn session_ref_records(
    state: &AppState,
    profile_id: &str,
    global: Vec<SessionInfo>,
    busy_sessions: &HashSet<SessionKey>,
    workspace_id: Option<&str>,
) -> Result<
    (
        Vec<octos_core::ui_protocol::WorkspaceSummary>,
        Vec<octos_core::ui_protocol::SessionRefRecord>,
    ),
    RpcError,
> {
    let mut entries: Vec<_> = global
        .into_iter()
        .map(|info| (SessionKey(info.id.clone()), None, info))
        .collect();
    let workspaces = super::workspace_registry::list_workspaces(state, profile_id)?;
    for workspace in workspaces
        .iter()
        .filter(|workspace| workspace_id.is_none_or(|id| id == workspace.workspace_id))
    {
        let root = crate::runtime::session::project_sessions_root(
            Path::new(&workspace.canonical_path),
            profile_id,
        );
        for info in super::handlers::list_profile_sessions(&root, busy_sessions, profile_id) {
            entries.push((SessionKey(info.id.clone()), Some(workspace.clone()), info));
        }
    }
    super::workspace_registry::index_session_refs(state, profile_id, entries)?;

    let mut records = super::workspace_registry::catalog_records(state, profile_id, workspace_id)?;
    for record in &mut records {
        record.active_turn = super::handlers::session_id_has_active_turn(
            busy_sessions,
            profile_id,
            &record.session_id,
        );
    }
    Ok((workspaces, records))
}

/// Build one bounded page across the explicitly authorized profile scopes.
/// The existing per-profile registries and keyset function remain canonical;
/// no second registry or offset-based catalog is introduced.
pub fn catalog_bootstrap(
    state: &AppState,
    profile_ids: &[String],
    mut global: HashMap<String, Vec<SessionInfo>>,
    busy_sessions: &HashSet<SessionKey>,
    params: CatalogBootstrapParams,
) -> Result<CatalogBootstrapResult, RpcError> {
    let mut records = Vec::new();
    let mut workspaces = BTreeMap::new();
    for profile_id in profile_ids {
        let (profile_workspaces, mut profile_records) = session_ref_records(
            state,
            profile_id,
            global.remove(profile_id).unwrap_or_default(),
            busy_sessions,
            params.workspace_id.as_deref(),
        )?;
        for workspace in profile_workspaces {
            workspaces
                .entry(workspace.workspace_id.clone())
                .and_modify(|existing: &mut octos_core::ui_protocol::WorkspaceSummary| {
                    for profile in &workspace.profile_ids {
                        if !existing.profile_ids.contains(profile) {
                            existing.profile_ids.push(profile.clone());
                        }
                    }
                    existing.last_opened_at_ms =
                        existing.last_opened_at_ms.max(workspace.last_opened_at_ms);
                })
                .or_insert(workspace);
        }
        records.append(&mut profile_records);
    }
    for record in &mut records {
        if let Some(workspace) = &mut record.workspace {
            workspace
                .profile_ids
                .retain(|profile| profile_ids.contains(profile));
        }
    }
    records.sort_by(|left, right| {
        right
            .updated_at_ms
            .cmp(&left.updated_at_ms)
            .then_with(|| left.session_ref.cmp(&right.session_ref))
    });
    let session_count = records.len();
    let workspace_count = workspaces.len();
    let limit = params.limit.unwrap_or(50).clamp(1, 100) as usize;
    let (sessions, next_cursor) =
        super::workspace_registry::catalog_page(&records, params.cursor.as_deref(), limit)?;
    let page_workspace_ids: HashSet<_> = sessions
        .iter()
        .filter_map(|record| record.workspace_id.as_deref())
        .collect();
    let workspaces = workspaces
        .into_values()
        .filter(|workspace| {
            // The first response is the client's Workspace bootstrap, even
            // when a Workspace has no Session yet. Continuations need only
            // the summaries referenced by that page.
            params.cursor.is_none() || page_workspace_ids.contains(workspace.workspace_id.as_str())
        })
        .map(|mut workspace| {
            // The profile scope is part of the response's authorization
            // projection; do not disclose sibling profile ids stored on a
            // shared Workspace row.
            workspace
                .profile_ids
                .retain(|profile| profile_ids.contains(profile));
            workspace
        })
        .collect::<Vec<_>>();
    let returned_count = sessions.len();
    let has_more = next_cursor.is_some();
    Ok(CatalogBootstrapResult {
        connection_scope: octos_core::ui_protocol::CatalogConnectionScope {
            profile_ids: profile_ids.to_vec(),
        },
        workspaces,
        sessions,
        next_cursor,
        summary: CatalogBootstrapSummary {
            workspace_count,
            session_count,
            returned_count,
            has_more,
        },
    })
}

/// Build the negotiated catalog on the blocking pool. Known legacy rows are
/// reconciled in one batch; unchanged reads do not rewrite either catalog.
pub fn session_ref_catalog(
    state: &Arc<AppState>,
    profile_id: &str,
    global: Vec<SessionInfo>,
    busy_sessions: HashSet<SessionKey>,
    params: SessionListParams,
) -> Result<SessionListResult, octos_core::ui_protocol::RpcError> {
    let (workspaces, records) = session_ref_records(
        state,
        profile_id,
        global,
        &busy_sessions,
        params.workspace_id.as_deref(),
    )?;
    let limit = params.limit.unwrap_or(50).clamp(1, 100) as usize;
    let (page, next_cursor) =
        super::workspace_registry::catalog_page(&records, params.cursor.as_deref(), limit)?;
    let workspace_root = params.workspace_id.as_ref().and_then(|workspace_id| {
        workspaces
            .iter()
            .find(|workspace| workspace.workspace_id == *workspace_id)
            .map(|workspace| workspace.canonical_path.clone())
    });
    Ok(SessionListResult {
        sessions: serde_json::to_value(page).unwrap_or_else(|_| json!([])),
        workspace_root,
        profile_id: Some(profile_id.to_owned()),
        next_cursor,
        scope: Some(
            if params.workspace_id.is_some() {
                "workspace"
            } else {
                "mixed"
            }
            .to_owned(),
        ),
    })
}

#[cfg(test)]
#[path = "workspace_bootstrap_tests.rs"]
mod bootstrap_tests;
