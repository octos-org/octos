//! Pure wire helpers for the agent orchestrator (A-03).
//! Error constructors, parsers, and string/key formatting with no
//! orchestrator state access.

use super::*;

pub(crate) fn agent_not_found_error(request: &AgentRequest) -> RpcError {
    autonomy_error(
        kinds::AGENT_NOT_FOUND,
        "agent not found",
        request.session_id.as_ref(),
        Some(&request.profile_id),
        Some(("agent_id", request.agent_id.as_str())),
        true,
    )
}

pub(crate) fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

pub(crate) fn now_ms_u64() -> u64 {
    now_ms().try_into().unwrap_or(0)
}

/// Sanitize a `goal_id` for use as a ledger filename. Goal ids are uuids in
/// practice, but we strip anything that isn't alphanumeric / `-` / `_` so a
/// hostile or legacy id cannot escape the ledger dir.
pub(crate) fn sanitize_filename_for_ledger(goal_id: &str) -> String {
    goal_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// #1967 — the resolution text a timed-out escalation records: `[timeout]
/// <default_action>` when the producer declared what the peer would do,
/// `[timeout] expired` otherwise — so an audit can tell an answered row
/// (`[answer]`/`[approval]`), an abandoned one (`[closed]`) and an expired
/// one apart at a glance.
pub(crate) fn escalation_timeout_resolution(default_action: Option<&str>) -> String {
    format!("[timeout] {}", default_action.unwrap_or("expired"))
}

pub(crate) fn autonomy_error_code(kind: &str) -> i64 {
    match kind {
        kinds::AGENT_CONTROL_FORBIDDEN
        | kinds::AGENT_ARTIFACT_DENIED
        | kinds::LOOP_SLASH_DENIED
        | kinds::LOOP_POLICY_DENIED
        | kinds::MONITOR_POLICY_DENIED => rpc_error_codes::PERMISSION_DENIED,
        kinds::AGENT_NOT_FOUND
        | kinds::GOAL_UNAVAILABLE
        | kinds::LOOP_NOT_FOUND
        | kinds::MONITOR_NOT_FOUND => rpc_error_codes::RESOURCE_NOT_FOUND,
        kinds::AGENT_CONTROL_UNAVAILABLE
        | kinds::GOAL_RUNTIME_UNAVAILABLE
        | kinds::LOOP_RUNTIME_UNAVAILABLE => rpc_error_codes::RUNTIME_NOT_READY,
        kinds::GOAL_RATE_LIMITED
        | kinds::LOOP_BUSY
        | kinds::MONITOR_FLOODED
        | kinds::AUTONOMY_QUOTA_EXCEEDED => rpc_error_codes::RATE_LIMITED,
        _ => rpc_error_codes::INVALID_PARAMS,
    }
}

pub(crate) fn autonomy_error(
    kind: &'static str,
    message: impl Into<String>,
    session_id: Option<&SessionKey>,
    profile_id: Option<&str>,
    entity: Option<(&str, &str)>,
    recoverable: bool,
) -> RpcError {
    let mut data = serde_json::Map::new();
    data.insert("kind".into(), json!(kind));
    data.insert("policy_id".into(), json!(AUTONOMY_POLICY_ID));
    data.insert(
        "profile_id".into(),
        json!(profile_id.unwrap_or(MAIN_PROFILE_ID)),
    );
    data.insert("recoverable".into(), json!(recoverable));
    if let Some(session_id) = session_id {
        data.insert("session_id".into(), json!(session_id));
    }
    if let Some((key, value)) = entity {
        data.insert(key.into(), json!(value));
    }
    RpcError::new(autonomy_error_code(kind), message).with_data(Value::Object(data))
}

/// #1977 blocker 6 — the typed `monitor_invalid_spec` RPC error for the raw
/// dispatch layer (e.g. an unknown `mode`), matching the shape `create_monitor`
/// produces when [`crate::autonomy::monitor_runtime::MonitorSpec::validate`] fails.
pub(crate) fn monitor_invalid_spec_error(
    session_id: &SessionKey,
    profile_id: &str,
    message: impl Into<String>,
) -> RpcError {
    autonomy_error(
        kinds::MONITOR_INVALID_SPEC,
        message,
        Some(session_id),
        Some(profile_id),
        None,
        true,
    )
}

pub(crate) fn agent_invalid_params_error(
    kind: &'static str,
    message: impl Into<String>,
    session_id: Option<&SessionKey>,
    profile_id: Option<&str>,
    entity: Option<(&str, &str)>,
) -> RpcError {
    let mut data = serde_json::Map::new();
    data.insert("kind".into(), json!(kind));
    data.insert("policy_id".into(), json!(AUTONOMY_POLICY_ID));
    data.insert(
        "profile_id".into(),
        json!(profile_id.unwrap_or(MAIN_PROFILE_ID)),
    );
    data.insert("recoverable".into(), json!(true));
    if let Some(session_id) = session_id {
        data.insert("session_id".into(), json!(session_id));
    }
    if let Some((key, value)) = entity {
        data.insert(key.into(), json!(value));
    }
    RpcError::invalid_params(message).with_data(Value::Object(data))
}

pub(crate) fn parse_agent_output_cursor(
    cursor: Option<Value>,
    session_id: Option<&SessionKey>,
    profile_id: &str,
) -> Result<Option<OutputCursor>, RpcError> {
    let Some(cursor) = cursor else {
        return Ok(None);
    };
    let Some(offset) = cursor.get("offset").and_then(Value::as_u64) else {
        return Err(agent_invalid_params_error(
            AGENT_OUTPUT_CURSOR_INVALID,
            "agent output cursor must be an object with numeric offset",
            session_id,
            Some(profile_id),
            None,
        ));
    };
    Ok(Some(OutputCursor { offset }))
}

pub(crate) fn is_agent_terminal_status(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "interrupted" | "closed")
}

/// Gap-1 step 3: explicit `ChildCompleted` dedupe key, symmetric to the
/// failure key `external/<kind>/<session>/<task_id>`. Keyed on stable
/// identity only (group + session + agent_id) so metadata drift
/// (status/summary/nickname/role) never splits the dedupe across the
/// strangler double-delivery or repeated terminal marks.
pub(crate) fn child_completed_dedupe_key(
    group_id: &str,
    session_id: &str,
    agent_id: &str,
) -> String {
    format!("child/{group_id}/{session_id}/{agent_id}")
}

/// Build the `ChildCompleted` request for one terminal agent. Shared by the
/// first-delivery and status-correction arms of
/// `enqueue_agent_terminal_continuations` so both enqueue byte-identical
/// request shapes (#1707 round 3).
pub(crate) fn build_child_terminal_request(
    agent: &AutonomyAgentRecord,
    group_id: &str,
    child_key: &str,
) -> MasterContinuationRequest {
    let mut child = MasterContinuationRequest::new(
        group_id.to_owned(),
        agent.session_id.to_string(),
        agent.profile_id.clone(),
        MasterContinuationReason::ChildCompleted,
        SystemTime::now(),
    )
    .with_child_agent_id(agent.agent_id.clone())
    // Gap-1 step 3: explicit success dedupe key, symmetric to the failure
    // key `external/<kind>/<session>/<task_id>`. Keyed ONLY on stable
    // identity (group + session + agent_id) so repeated terminal marks of
    // the same agent (live + cascade + orphan-sweep, AND the strangler's
    // legacy on_change + unified on_terminal double-delivery) collapse to
    // one ChildCompleted continuation via `pending_by_key` — independent of
    // metadata drift (status/summary/nickname/role), which the auto-derived
    // `stable_dedupe_key` would otherwise fold into the key and split into
    // distinct entries.
    .with_dedupe_key(child_key.to_owned())
    .with_metadata("status", agent.status.clone())
    .with_metadata("nickname", agent.nickname.clone())
    .with_metadata("role", agent.role.clone());
    // #1707: stamp the child's workspace so a future drain-site guard can drop
    // a continuation replayed under a wire session key that has since been
    // rebound to a DIFFERENT project (sessions_in_cwd reuse). Self-describing;
    // no behavior change on its own.
    if let Some(scope) = agent.workspace_scope.as_ref() {
        child = child.with_metadata("workspace_scope", scope.key().to_owned());
    }
    if let Some(cwd) = agent.cwd.as_deref().filter(|value| !value.is_empty()) {
        child = child.with_metadata("workspace", cwd.to_owned());
    }
    if let Some(last_task) = agent.last_task.as_deref().filter(|value| !value.is_empty()) {
        child = child.with_metadata("summary", last_task.chars().take(1200).collect::<String>());
    }
    child
}

/// Explicit `LoopFire` dedupe key shared by BOTH enqueue paths — the
/// manual `control_loop(FireNow)` arm and the scheduled
/// `enqueue_due_loop_continuations` tick. Keyed ONLY on stable identity
/// (group + profile + loop_id): the auto-derived `stable_dedupe_key`
/// folds every metadata pair into the key, and the two paths' metadata
/// deliberately differs (`scheduled_for_ms` exists only on the tick;
/// maintenance loops resolve prompt / prompt_source at fire time vs the
/// legacy "record" label), so a fire_now racing the tick used to MISS
/// `pending_by_key` and enqueue a SECOND LoopFire — two turns for one
/// due moment. The identity key makes whichever path lands second
/// collapse as a Duplicate while a fire is pending-and-unclaimed.
/// Distinct due moments are unaffected: LoopFire is not `External`, so
/// a CLAIMED (drained) key leaves `pending_by_key` immediately and the
/// next due moment re-enqueues freely (see the recently-claimed guard
/// scoping in `master_continuation_scheduler.rs`).
pub(crate) fn loop_fire_dedupe_key(group_id: &str, profile_id: &str, loop_id: &str) -> String {
    format!("loop_fire/{group_id}/{profile_id}/{loop_id}")
}

pub(crate) fn agent_continuation_group_id(agent: &AutonomyAgentRecord) -> String {
    format!(
        "agent-group:{}:{}:{}",
        agent.profile_id,
        agent.session_id,
        agent.parent_agent_id.as_deref().unwrap_or("master")
    )
}

pub(crate) fn autonomy_goal_group_id(session_id: &SessionKey) -> String {
    format!("autonomy-goal:{session_id}")
}

pub(crate) fn autonomy_loop_group_id(loop_record: &AutonomyLoopRecord) -> String {
    format!(
        "autonomy-loop:{}:{}",
        loop_record.session_id, loop_record.loop_id
    )
}

pub(crate) fn sequence_suffix(id: &str) -> u64 {
    id.rsplit_once('_')
        .and_then(|(_, suffix)| suffix.parse::<u64>().ok())
        .unwrap_or(0)
}

pub(crate) fn unique_event_suffix() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

pub(crate) fn supervisor_metadata_str<'a>(
    metadata: &'a SupervisorMetadata,
    key: &str,
) -> Option<&'a str> {
    metadata.get(key).and_then(Value::as_str)
}

pub(crate) fn supervisor_metadata_i64(metadata: &SupervisorMetadata, key: &str) -> Option<i64> {
    metadata.get(key).and_then(Value::as_i64)
}

pub(crate) fn supervisor_metadata_u64(metadata: &SupervisorMetadata, key: &str) -> Option<u64> {
    metadata.get(key).and_then(Value::as_u64)
}

pub(crate) fn supervisor_metadata_bool(metadata: &SupervisorMetadata, key: &str) -> Option<bool> {
    metadata.get(key).and_then(Value::as_bool)
}

pub(crate) fn supervisor_metadata_for_agent(agent: &AutonomyAgentRecord) -> SupervisorMetadata {
    let mut metadata = SupervisorMetadata::new();
    // Explicit null also replaces an older scoped snapshot on a checked update.
    metadata.insert(
        "workspace_scope".into(),
        json!(agent.workspace_scope.as_ref().map(WorkspaceScope::key)),
    );
    metadata.insert("session_id".into(), json!(agent.session_id));
    metadata.insert("profile_id".into(), json!(agent.profile_id));
    metadata.insert("role".into(), json!(agent.role));
    metadata.insert("backend_kind".into(), json!(agent.backend_kind));
    metadata.insert("path".into(), json!(agent.path));
    metadata.insert("nickname".into(), json!(agent.nickname));
    if let Some(parent_agent_id) = agent.parent_agent_id.as_ref() {
        metadata.insert("parent_agent_id".into(), json!(parent_agent_id));
    }
    metadata
}
