//! OUP adapters for the shared peer team. All identities are resolved by the
//! server before addressing peers; no client-supplied cwd/storage path is used.
use super::*;
use crate::peers::workspace_team as team;

#[derive(Deserialize)]
struct Params {
    session_id: SessionKey,
    #[serde(default)]
    profile_id: Option<String>,
    #[serde(default)]
    agent_id: Option<String>,
    #[serde(default)]
    expected_revision: Option<u64>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    occurrence_id: Option<String>,
    #[serde(default)]
    broadcast: bool,
}

pub(super) async fn rpc(
    ws: &WsConnection,
    state: &Arc<AppState>,
    request: &RpcRequest<Value>,
    connection_profile: Option<&str>,
) -> Result<Value, RpcError> {
    let params: Params = parse_raw_params(request)?;
    let profile = validate_session_scope(
        &params.session_id,
        params.profile_id.as_deref(),
        connection_profile,
    )?;
    let runtime = resolve_session_profile_runtime(state, profile.as_deref())
        .ok_or_else(|| runtime_unavailable_error("profile runtime unavailable"))?;
    let workspace =
        session_workspace_root_for_profile(Some(&runtime.profile_id), &params.session_id)
            .ok_or_else(|| {
                RpcError::invalid_request("open the session before using its workspace team")
            })?;
    let teams = &state.ui_protocol.workspace_teams;
    let snapshot = match request.method.as_str() {
        team::LEADER => teams.set_leader(
            &runtime.data_dir,
            &workspace,
            &params.session_id,
            params
                .agent_id
                .as_deref()
                .ok_or_else(|| RpcError::invalid_params("agent_id is required"))?,
            params.expected_revision.ok_or_else(|| {
                RpcError::invalid_params("expected_revision is required; list the team first")
            })?,
        ),
        _ => teams.get(&runtime.data_dir, &workspace, &params.session_id),
    }
    .map_err(RpcError::invalid_request)?;
    if request.method == team::MESSAGE {
        let message = params
            .message
            .as_deref()
            .ok_or_else(|| RpcError::invalid_params("message is required"))?;
        let occurrence = params
            .occurrence_id
            .as_deref()
            .ok_or_else(|| RpcError::invalid_params("occurrence_id is required"))?;
        if message.trim().is_empty()
            || message.len() > 64 * 1024
            || occurrence.is_empty()
            || occurrence.len() > 256
        {
            return Err(RpcError::invalid_params(
                "message must be 1..65536 bytes and occurrence_id 1..256 bytes",
            ));
        }
        if params.broadcast && params.agent_id.is_some() {
            return Err(RpcError::invalid_params(
                "choose agent_id or broadcast, not both",
            ));
        }
        let targets: Vec<_> = if params.broadcast {
            snapshot
                .members
                .iter()
                .filter(|m| m.session_id != params.session_id)
                .collect()
        } else {
            vec![
                snapshot
                    .member(
                        params
                            .agent_id
                            .as_deref()
                            .ok_or_else(|| RpcError::invalid_params("agent_id is required"))?,
                    )
                    .ok_or_else(|| RpcError::invalid_params("unknown workspace team member"))?,
            ]
        };
        let mut receipts = Vec::new();
        for target in targets {
            let outcome = deliver(
                state,
                &runtime,
                &workspace,
                &params.session_id,
                &target.agent_id,
                occurrence,
                message,
            );
            receipts.push(match outcome {
                Ok(octos_agent::PeerSendInputDelivery::Queued) => {
                    json!({"agent_id": target.agent_id, "status": "queued"})
                }
                Ok(octos_agent::PeerSendInputDelivery::AlreadyQueued) => {
                    json!({"agent_id": target.agent_id, "status": "already_queued"})
                }
                Err(error) => {
                    json!({"agent_id": target.agent_id, "status": "failed", "error": error})
                }
            });
        }
        return Ok(json!({"session_id": params.session_id, "receipts": receipts}));
    }
    let result = projection(state, &runtime, &params.session_id, &snapshot).await;
    teams.watch(
        ws.connection_id().0,
        &runtime.profile_id,
        &params.session_id,
        result.clone(),
    );
    Ok(result)
}

async fn projection(
    state: &AppState,
    runtime: &crate::runtime::ProfileRuntime,
    caller: &SessionKey,
    snapshot: &team::Team,
) -> Value {
    let busy = active_turn_sessions(&active_turns_registry()).await;
    let members: Vec<_> = snapshot.members.iter().map(|m| json!({
        "agent_id": m.agent_id, "session_id": m.session_id,
        "role": if m.agent_id == snapshot.leader { "coordinator" } else { "member" },
        "attached": state.ui_protocol.workspace_teams.attached(&runtime.data_dir, &snapshot.workspace, &m.session_id),
        "status": if busy.contains(&m.session_id) { "running" } else { "idle" },
        "result_turn_id": m.result_turn_id,
    })).collect();
    json!({"session_id": caller, "workspace": snapshot.workspace,
        "revision": snapshot.revision, "leader": snapshot.leader, "members": members})
}

/// Only connections that listed a team opt into its ephemeral updates.
/// Re-list after reconnect recovers the complete authoritative snapshot.
pub(super) async fn emit_updates(ws: &WsConnection, state: &Arc<AppState>) {
    let teams = &state.ui_protocol.workspace_teams;
    for (profile, session, previous) in teams.watches(ws.connection_id().0) {
        let Some(runtime) = resolve_session_profile_runtime(state, Some(&profile)) else {
            continue;
        };
        let Some(workspace) = session_workspace_root_for_profile(Some(&profile), &session) else {
            continue;
        };
        let Ok(team) = teams.get(&runtime.data_dir, &workspace, &session) else {
            continue;
        };
        let next = projection(state, &runtime, &session, &team).await;
        if next != previous
            && send_raw_notification_ephemeral(ws, team::UPDATED, next.clone()).is_ok()
        {
            teams.watch(ws.connection_id().0, &profile, &session, next);
        }
    }
}

pub(super) fn deliver(
    state: &AppState,
    runtime: &crate::runtime::ProfileRuntime,
    workspace: &Path,
    sender: &SessionKey,
    target: &str,
    occurrence: &str,
    message: &str,
) -> Result<octos_agent::PeerSendInputDelivery, String> {
    let snapshot = state
        .ui_protocol
        .workspace_teams
        .get(&runtime.data_dir, workspace, sender)?;
    deliver_snapshot(
        runtime, sender, target, occurrence, message, &snapshot, false,
    )
}

pub(super) fn deliver_snapshot(
    runtime: &crate::runtime::ProfileRuntime,
    sender: &SessionKey,
    target: &str,
    occurrence: &str,
    message: &str,
    snapshot: &team::Team,
    assignment: bool,
) -> Result<octos_agent::PeerSendInputDelivery, String> {
    let from = snapshot
        .member(&sender.0)
        .ok_or("sender is not a team member")?;
    let to = snapshot
        .member(target)
        .ok_or("unknown workspace team member")?;
    if to.session_id == *sender {
        return Err("choose a different team member".into());
    }
    // Host-owned peers must receive inputs through their host connection.
    if to
        .session_id
        .topic()
        .is_some_and(|t| t.starts_with("peer-"))
    {
        return Err("staged peers use their existing peer name and host delivery path".into());
    }
    let actual_workspace =
        session_workspace_root_for_profile(Some(&runtime.profile_id), &to.session_id).ok_or(
            "recipient must reopen its session after server restart before receiving new messages",
        )?;
    if actual_workspace.canonicalize().map_err(|e| e.to_string())? != snapshot.workspace {
        return Err("recipient is currently open in another workspace".into());
    }
    let envelope = json!({"from_session": sender, "from_agent": from.agent_id,
        "to_session": to.session_id, "workspace": snapshot.workspace,
        "coordinator": snapshot.leader, "revision": snapshot.revision, "assignment": assignment, "message": message});
    use sha2::{Digest, Sha256};
    let occurrence = format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&(
                crate::peers::workspace_scope_encode(&snapshot.workspace),
                sender,
                occurrence,
                assignment
            ))
            .map_err(|e| e.to_string())?
        )
    );
    default_agent_orchestrator()
        .enqueue_workspace_peer_message(
            &default_agent_orchestrator().scoped_goal_key(&to.session_id),
            &runtime.profile_id,
            &occurrence,
            &envelope.to_string(),
        )
        .into_callback_result(&to.agent_id)
}
