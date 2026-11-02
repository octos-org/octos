//! Workspace/SessionRef OUP handlers kept out of the large transport module.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use octos_core::ui_protocol::{SessionListParams, SessionListResult};
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

/// Build the negotiated catalog on the blocking pool. Known legacy rows are
/// reconciled in one batch; unchanged reads do not rewrite either catalog.
pub fn session_ref_catalog(
    state: &Arc<AppState>,
    profile_id: &str,
    global: Vec<SessionInfo>,
    busy_sessions: HashSet<SessionKey>,
    params: SessionListParams,
) -> Result<SessionListResult, octos_core::ui_protocol::RpcError> {
    let mut entries: Vec<_> = global
        .into_iter()
        .map(|info| (SessionKey(info.id.clone()), None, info))
        .collect();
    let workspaces = super::workspace_registry::list_workspaces(state, profile_id)?;
    for workspace in workspaces.iter().filter(|workspace| {
        params
            .workspace_id
            .as_deref()
            .is_none_or(|id| id == workspace.workspace_id)
    }) {
        let root = crate::runtime::session::project_sessions_root(
            Path::new(&workspace.canonical_path),
            profile_id,
        );
        for info in super::handlers::list_profile_sessions(&root, &busy_sessions, profile_id) {
            entries.push((SessionKey(info.id.clone()), Some(workspace.clone()), info));
        }
    }
    super::workspace_registry::index_session_refs(state, profile_id, entries)?;

    let mut records = super::workspace_registry::catalog_records(
        state,
        profile_id,
        params.workspace_id.as_deref(),
    )?;
    for record in &mut records {
        record.active_turn = super::handlers::session_id_has_active_turn(
            &busy_sessions,
            profile_id,
            &record.session_id,
        );
    }
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
