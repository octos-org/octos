//! Workspace/SessionRef OUP handlers kept out of the large transport module.

use std::sync::Arc;

use octos_core::MAIN_PROFILE_ID;
use serde_json::json;

use super::AppState;
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
