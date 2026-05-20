use std::collections::BTreeSet;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use super::master_continuation_scheduler::{
    MasterContinuationEnqueueOutcome, MasterContinuationReason, MasterContinuationRequest,
    MasterContinuationRuntimeState, MasterContinuationScheduler, QueuedMasterContinuation,
};
use super::supervisor_store::{
    ArtifactRecord as SupervisorArtifactRecord, ChildAgentRecord, ChildStatus, ContinuationStatus,
    GroupStatus, HeartbeatPing, PendingContinuationRecord, SupervisedGroupRecord, SupervisorEvent,
    SupervisorMetadata, SupervisorState, SupervisorStore, TerminalKind, TerminalState,
};
use chrono::Utc;
use octos_agent::tools::mcp_agent::DispatchContextContract;
use octos_agent::{Agent, AgentConfig, ToolRegistry};
use octos_core::ui_protocol::{
    OutputCursor, RpcError, autonomy_error_kinds as kinds, methods, rpc_error_codes,
};
use octos_core::{AgentId, MAIN_PROFILE_ID, SessionKey, TaskId};
use octos_llm::LlmProvider;
use octos_memory::EpisodeStore;
use serde_json::{Value, json};
use tokio::sync::mpsc;

const AUTONOMY_POLICY_ID: &str = "coding-autonomy-v1";
const GOAL_DEFAULT_TOKEN_BUDGET: u64 = 50_000;
const GOAL_MAX_TOKEN_BUDGET: u64 = 200_000;
const LOOP_MIN_INTERVAL_SECONDS: u64 = 60;
const LOOP_MAX_INTERVAL_SECONDS: u64 = 86_400;
const LOOP_MAX_AGE_DAYS: i64 = 7;
const MAX_OBJECTIVE_BYTES: usize = 8_192;
const MAX_LOOP_PROMPT_BYTES: usize = 8_192;
const MAX_LOOPS_PER_SESSION: usize = 16;
const AGENT_OUTPUT_CURSOR_INVALID: &str = "agent_output_cursor_invalid";
const AGENT_ARTIFACT_SELECTOR_INVALID: &str = "agent_artifact_selector_invalid";
const AUTONOMY_RECORD_KIND: &str = "autonomy_record_kind";
const AUTONOMY_RECORD_GOAL: &str = "goal";
const AUTONOMY_RECORD_LOOP: &str = "loop";
const AUTONOMY_GOAL_CLEARED: &str = "goal_cleared";
const NATIVE_SPECIALIST_BACKEND_KIND: &str = "native";
const NATIVE_SPECIALIST_SUMMARY_ARTIFACT_ID: &str = "summary";
const NATIVE_SPECIALIST_ARTIFACT_CONTENT_MAX_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone)]
pub(crate) struct AgentListRequest {
    pub(crate) session_id: Option<SessionKey>,
    pub(crate) profile_id: String,
    pub(crate) connection_profile_id: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct AgentRequest {
    pub(crate) agent_id: String,
    pub(crate) session_id: Option<SessionKey>,
    pub(crate) profile_id: String,
}

#[derive(Debug, Clone)]
pub(crate) struct AgentOutputRequest {
    pub(crate) agent_id: String,
    pub(crate) session_id: Option<SessionKey>,
    pub(crate) profile_id: String,
    pub(crate) cursor: Option<Value>,
    pub(crate) limit: Option<usize>,
}

#[derive(Debug, Clone)]
pub(crate) struct AgentArtifactReadRequest {
    pub(crate) agent_id: String,
    pub(crate) artifact_id: Option<String>,
    pub(crate) path: Option<String>,
    pub(crate) session_id: Option<SessionKey>,
    pub(crate) profile_id: String,
}

#[derive(Debug, Clone)]
pub(crate) struct GoalSessionRequest {
    pub(crate) session_id: SessionKey,
    pub(crate) profile_id: String,
}

#[derive(Debug, Clone)]
pub(crate) struct GoalSetRequest {
    pub(crate) session_id: SessionKey,
    pub(crate) profile_id: String,
    pub(crate) objective: String,
    pub(crate) status: Option<String>,
    pub(crate) token_budget: Option<u64>,
    pub(crate) transition_actor: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct LoopCreateRequest {
    pub(crate) session_id: SessionKey,
    pub(crate) profile_id: String,
    pub(crate) prompt: Option<String>,
    pub(crate) command: Option<String>,
    pub(crate) interval_seconds: Option<u64>,
    pub(crate) mode: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct LoopListRequest {
    pub(crate) session_id: Option<SessionKey>,
    pub(crate) profile_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoopControlKind {
    Delete,
    Pause,
    Resume,
    FireNow,
}

#[derive(Debug, Clone)]
pub(crate) struct LoopControlRequest {
    pub(crate) loop_id: String,
    pub(crate) session_id: Option<SessionKey>,
    pub(crate) profile_id: String,
    pub(crate) kind: LoopControlKind,
}

pub(crate) trait AgentOrchestrator: Send + Sync {
    fn list_agents(&self, request: AgentListRequest) -> Result<Value, RpcError>;
    fn read_agent_status(&self, request: AgentRequest) -> Result<Value, RpcError>;
    fn read_agent_output(&self, request: AgentOutputRequest) -> Result<Value, RpcError>;
    fn list_agent_artifacts(&self, request: AgentRequest) -> Result<Value, RpcError>;
    fn read_agent_artifact(&self, request: AgentArtifactReadRequest) -> Result<Value, RpcError>;
    fn interrupt_agent(&self, request: AgentRequest) -> Result<Value, RpcError>;
    fn close_agent(&self, request: AgentRequest) -> Result<Value, RpcError>;
    fn get_goal(&self, request: GoalSessionRequest) -> Result<Value, RpcError>;
    fn set_goal(&self, request: GoalSetRequest) -> Result<Value, RpcError>;
    fn clear_goal(&self, request: GoalSessionRequest) -> Result<Value, RpcError>;
    fn create_loop(&self, request: LoopCreateRequest) -> Result<Value, RpcError>;
    fn list_loops(&self, request: LoopListRequest) -> Result<Value, RpcError>;
    fn control_loop(&self, request: LoopControlRequest) -> Result<Value, RpcError>;
}

#[derive(Debug, Default)]
pub(crate) struct InProcessAgentOrchestrator {
    state: StdMutex<AutonomyRuntimeState>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AgentArtifactRecord {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) kind: String,
    pub(crate) status: String,
    pub(crate) path: Option<String>,
    pub(crate) content: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct AgentUpsert {
    pub(crate) agent_id: String,
    pub(crate) parent_agent_id: Option<String>,
    pub(crate) session_id: SessionKey,
    pub(crate) task_id: Option<TaskId>,
    pub(crate) path: String,
    pub(crate) role: String,
    pub(crate) nickname: String,
    pub(crate) backend_kind: String,
    pub(crate) status: String,
    pub(crate) last_task: Option<String>,
    pub(crate) cwd: Option<String>,
    pub(crate) profile_id: String,
}

pub(crate) type NativeSpecialistEventSender = mpsc::UnboundedSender<NativeSpecialistAppUiEvent>;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NativeSpecialistAppUiEvent {
    pub(crate) method: &'static str,
    pub(crate) params: Value,
}

pub(crate) struct NativeSpecialistLaunchRequest {
    pub(crate) agent_id: Option<String>,
    pub(crate) parent_agent_id: Option<String>,
    pub(crate) session_id: SessionKey,
    pub(crate) profile_id: String,
    pub(crate) role: String,
    pub(crate) nickname: String,
    pub(crate) task: String,
    pub(crate) cwd: PathBuf,
    pub(crate) llm: Arc<dyn LlmProvider>,
    pub(crate) memory: Arc<EpisodeStore>,
    pub(crate) tools: Arc<ToolRegistry>,
    pub(crate) system_prompt: Option<String>,
    pub(crate) agent_config: Option<AgentConfig>,
    pub(crate) task_ledger_path: Option<PathBuf>,
    pub(crate) event_tx: Option<NativeSpecialistEventSender>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NativeSpecialistRunResult {
    pub(crate) agent_id: String,
    pub(crate) task_id: Option<TaskId>,
    pub(crate) status: String,
    pub(crate) output_len: usize,
    pub(crate) artifacts: Vec<AgentArtifactRecord>,
}

pub(crate) fn upsert_background_task_agent(
    task: &octos_agent::BackgroundTask,
) -> Option<(SessionKey, Value)> {
    let session_id = background_task_session_id(task)?;
    let profile_id = session_id
        .profile_id()
        .unwrap_or(MAIN_PROFILE_ID)
        .to_owned();
    let agent_id = background_task_agent_id(task);
    let status = background_task_agent_status(task);
    let artifacts = background_task_artifacts(task);
    let cwd = background_task_cwd(task);
    let task_id = task.id.parse::<TaskId>().ok();
    let last_task = background_task_last_task(task);

    let orchestrator = default_agent_orchestrator();
    let mut agent = orchestrator.upsert_agent(AgentUpsert {
        agent_id: agent_id.clone(),
        parent_agent_id: Some("master".to_owned()),
        session_id: session_id.clone(),
        task_id,
        path: format!("master/{agent_id}"),
        role: "background_task".to_owned(),
        nickname: background_task_nickname(task),
        backend_kind: background_task_backend_kind(task),
        status,
        last_task,
        cwd,
        profile_id: profile_id.clone(),
    });
    if !artifacts.is_empty() {
        if let Ok(updated) =
            orchestrator.set_agent_artifacts(&agent_id, &session_id, &profile_id, artifacts)
        {
            agent = updated;
        }
    }
    Some((session_id, agent))
}

impl InProcessAgentOrchestrator {
    fn state(&self) -> std::sync::MutexGuard<'_, AutonomyRuntimeState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[cfg(test)]
    pub(crate) fn clear_for_test(&self) {
        *self.state() = AutonomyRuntimeState::default();
    }

    pub(crate) fn configure_supervisor_store(
        &self,
        root_dir: impl AsRef<Path>,
    ) -> std::io::Result<()> {
        let store = SupervisorStore::new(root_dir);
        let supervisor_state = store.load_state()?;
        let mut state = self.state();
        state.supervisor_store = Some(store);
        restore_runtime_from_supervisor_state(&mut state, &supervisor_state);
        for continuation in supervisor_state.continuations.values() {
            if continuation.status == ContinuationStatus::Completed {
                continue;
            }
            if let Some(request) = master_continuation_request_from_persisted(continuation) {
                state.continuations.enqueue(request);
            }
        }
        Ok(())
    }

    pub(crate) fn upsert_agent(&self, upsert: AgentUpsert) -> Value {
        let now = now_ms();
        let mut state = self.state();
        let previous_status = state
            .agents
            .get(&upsert.agent_id)
            .map(|agent| agent.status.clone());
        let (agent, payload, transitioned_terminal) = {
            let entry = state
                .agents
                .entry(upsert.agent_id.clone())
                .or_insert_with(|| AutonomyAgentRecord {
                    agent_id: upsert.agent_id.clone(),
                    parent_agent_id: upsert.parent_agent_id.clone(),
                    session_id: upsert.session_id.clone(),
                    task_id: upsert.task_id.clone(),
                    path: upsert.path.clone(),
                    role: upsert.role.clone(),
                    nickname: upsert.nickname.clone(),
                    backend_kind: upsert.backend_kind.clone(),
                    status: upsert.status.clone(),
                    last_task: upsert.last_task.clone(),
                    cwd: upsert.cwd.clone(),
                    profile_id: upsert.profile_id.clone(),
                    output: String::new(),
                    artifacts: Vec::new(),
                    created_at_ms: now,
                    updated_at_ms: now,
                    context_contract: None,
                });
            entry.parent_agent_id = upsert.parent_agent_id;
            entry.session_id = upsert.session_id;
            entry.task_id = upsert.task_id;
            entry.path = upsert.path;
            entry.role = upsert.role;
            entry.nickname = upsert.nickname;
            entry.backend_kind = upsert.backend_kind;
            entry.status = upsert.status;
            entry.last_task = upsert.last_task;
            entry.cwd = upsert.cwd;
            entry.profile_id = upsert.profile_id;
            entry.updated_at_ms = now;
            let transitioned_terminal = is_agent_terminal_status(&entry.status)
                && previous_status.as_deref().is_none_or(|status| {
                    !is_agent_terminal_status(status) || status != entry.status
                });
            (
                entry.clone(),
                autonomy_agent_json(entry),
                transitioned_terminal,
            )
        };
        if transitioned_terminal {
            enqueue_agent_terminal_continuations(&mut state, &agent);
        } else if !is_agent_terminal_status(&agent.status) {
            persist_agent_started(&state, &agent);
        }
        payload
    }

    pub(crate) async fn run_native_specialist(
        &self,
        request: NativeSpecialistLaunchRequest,
    ) -> Result<NativeSpecialistRunResult, RpcError> {
        let NativeSpecialistLaunchRequest {
            agent_id,
            parent_agent_id,
            session_id,
            profile_id,
            role,
            nickname,
            task,
            cwd,
            llm,
            memory,
            tools,
            system_prompt,
            agent_config,
            task_ledger_path,
            event_tx,
        } = request;

        let agent_id = agent_id.unwrap_or_else(|| format!("native-{}", uuid::Uuid::now_v7()));
        let path = format!(
            "{}/{}",
            parent_agent_id.as_deref().unwrap_or("master"),
            agent_id
        );
        let supervisor = tools.supervisor();
        let raw_task_id = supervisor.register_with_lineage(
            "native_agent",
            &agent_id,
            Some(&session_id.to_string()),
            task_ledger_path.as_deref().and_then(Path::to_str),
        );
        let task_id = raw_task_id
            .parse::<TaskId>()
            .ok()
            .filter(|_| !raw_task_id.is_empty());
        if !raw_task_id.is_empty() {
            supervisor.mark_running(&raw_task_id);
            supervisor.mark_runtime_state(
                &raw_task_id,
                octos_agent::TaskRuntimeState::ExecutingTool,
                Some(
                    json!({
                        "workflow_kind": "native_specialist",
                        "current_phase": "model_run",
                        "progress_message": format!("{nickname} is running"),
                    })
                    .to_string(),
                ),
            );
        }

        let initial_agent = self.upsert_agent(AgentUpsert {
            agent_id: agent_id.clone(),
            parent_agent_id: parent_agent_id.clone(),
            session_id: session_id.clone(),
            task_id: task_id.clone(),
            path,
            role,
            nickname: nickname.clone(),
            backend_kind: NATIVE_SPECIALIST_BACKEND_KIND.to_owned(),
            status: "running".to_owned(),
            last_task: Some(task.clone()),
            cwd: Some(cwd.to_string_lossy().into_owned()),
            profile_id: profile_id.clone(),
        });
        // #1021 / M17-C — native specialists currently run on the parent session's context manager without forking; from the dispatch contract's perspective that is `external_context_unmanaged` with `risk: "medium"`. When the native runner starts forking child contexts via `ContextManager::from_forked_child_context` this should switch to `managed_payload(context_ref)` with `risk: "low"` (see #1022).
        let native_contract = DispatchContextContract::external_unmanaged(
            "native_specialist_context_not_yet_managed",
        )
        .with_backend_kind(NATIVE_SPECIALIST_BACKEND_KIND)
        .with_agent_id(agent_id.clone())
        .with_risk("medium")
        .with_parent_session_key(Some(session_id.to_string()))
        .with_child_session_key(Some(agent_id.clone()));
        let agent = self
            .set_agent_context_contract(&agent_id, &session_id, &profile_id, native_contract)
            .unwrap_or(initial_agent);
        emit_native_specialist_event(
            &event_tx,
            methods::AGENT_UPDATED,
            json!({
                "session_id": session_id.clone(),
                "agent": agent,
            }),
        );

        let mut child_tools = tools.snapshot_excluding(&[]);
        child_tools.clear_spawn_only();
        let child_tools = Arc::new(child_tools);
        let mut worker =
            Agent::new_shared(AgentId::new(agent_id.clone()), llm, child_tools, memory)
                .with_config(agent_config.unwrap_or_else(native_specialist_agent_config))
                .with_workspace_root(cwd.clone());
        if let Some(system_prompt) = system_prompt {
            worker = worker.with_system_prompt(system_prompt);
        }
        worker.wire_activate_tools();

        let result = worker.process_message(&task, &[], Vec::new()).await;
        let (status, output, artifacts) = match result {
            Ok(response) => {
                let output = response.content.clone();
                let artifacts = native_specialist_artifacts(
                    &cwd,
                    &output,
                    response
                        .files_to_send
                        .iter()
                        .chain(response.files_modified.iter()),
                );
                ("completed".to_owned(), output, artifacts)
            }
            Err(error) => {
                let output = format!("Native specialist failed: {error}");
                ("failed".to_owned(), output, Vec::new())
            }
        };

        if !output.is_empty() {
            self.append_agent_output(&agent_id, &session_id, &profile_id, &output)?;
            emit_native_specialist_event(
                &event_tx,
                methods::AGENT_OUTPUT_DELTA,
                json!({
                    "session_id": session_id.clone(),
                    "agent_id": agent_id.clone(),
                    "cursor": OutputCursor { offset: output.len() as u64 },
                    "text": output,
                }),
            );
        }

        if !artifacts.is_empty() {
            self.set_agent_artifacts(&agent_id, &session_id, &profile_id, artifacts.clone())?;
            emit_native_specialist_event(
                &event_tx,
                methods::AGENT_ARTIFACT_UPDATED,
                json!({
                    "session_id": session_id.clone(),
                    "agent_id": agent_id.clone(),
                    "artifacts": artifacts.iter().map(agent_artifact_json).collect::<Vec<_>>(),
                }),
            );
        }

        let final_status = if self.agent_status_is_terminal(&agent_id) {
            self.agent_status(&agent_id).unwrap_or(status)
        } else {
            if !raw_task_id.is_empty() {
                if status == "completed" {
                    supervisor.mark_completed(
                        &raw_task_id,
                        artifacts
                            .iter()
                            .filter_map(|artifact| artifact.path.clone())
                            .collect(),
                    );
                } else {
                    supervisor.mark_failed(&raw_task_id, output.clone());
                }
            }
            let agent = self.set_agent_status(
                &agent_id,
                &session_id,
                &profile_id,
                &status,
                Some(output.chars().take(1200).collect()),
            )?;
            emit_native_specialist_event(
                &event_tx,
                methods::AGENT_UPDATED,
                json!({
                    "session_id": session_id.clone(),
                    "agent": agent,
                }),
            );
            status
        };

        Ok(NativeSpecialistRunResult {
            agent_id,
            task_id,
            status: final_status,
            output_len: output.len(),
            artifacts,
        })
    }

    fn agent_status(&self, agent_id: &str) -> Option<String> {
        self.state()
            .agents
            .get(agent_id)
            .map(|agent| agent.status.clone())
    }

    fn agent_status_is_terminal(&self, agent_id: &str) -> bool {
        self.agent_status(agent_id)
            .is_some_and(|status| is_agent_terminal_status(&status))
    }

    /// #1021 / M17-C — stamp the dispatch context contract onto the agent record so subsequent `agent/updated` events surface `context_mode` / `context_refs` / `context_contract` to AppUI clients. Returns the freshly serialised agent JSON so callers can emit it through the supervisor event sink. Idempotent: the stored contract is overwritten on each call, mirroring how MCP dispatches stamp the most-recent contract on every response.
    pub(crate) fn set_agent_context_contract(
        &self,
        agent_id: &str,
        session_id: &SessionKey,
        profile_id: &str,
        contract: DispatchContextContract,
    ) -> Result<Value, RpcError> {
        let mut state = self.state();
        let request = AgentRequest {
            agent_id: agent_id.to_owned(),
            session_id: Some(session_id.clone()),
            profile_id: profile_id.to_owned(),
        };
        let agent = state
            .agents
            .get_mut(agent_id)
            .ok_or_else(|| agent_not_found_error(&request))?;
        ensure_agent_control_scope(agent, Some(session_id), profile_id)?;
        agent.context_contract = Some(contract);
        agent.updated_at_ms = now_ms();
        Ok(autonomy_agent_json(agent))
    }

    pub(crate) fn set_agent_status(
        &self,
        agent_id: &str,
        session_id: &SessionKey,
        profile_id: &str,
        status: &str,
        last_task: Option<String>,
    ) -> Result<Value, RpcError> {
        let mut state = self.state();
        let request = AgentRequest {
            agent_id: agent_id.to_owned(),
            session_id: Some(session_id.clone()),
            profile_id: profile_id.to_owned(),
        };
        let agent = state
            .agents
            .get_mut(agent_id)
            .ok_or_else(|| agent_not_found_error(&request))?;
        ensure_agent_control_scope(agent, Some(session_id), profile_id)?;
        agent.status = status.to_owned();
        if let Some(last_task) = last_task {
            agent.last_task = Some(last_task);
        }
        agent.updated_at_ms = now_ms();
        let agent = agent.clone();
        let payload = autonomy_agent_json(&agent);
        if is_agent_terminal_status(&agent.status) {
            enqueue_agent_terminal_continuations(&mut state, &agent);
        } else {
            persist_agent_started(&state, &agent);
        }
        Ok(payload)
    }

    pub(crate) fn record_agent_ping(
        &self,
        agent_id: &str,
        session_id: &SessionKey,
        profile_id: &str,
        ping_id: Option<String>,
        state_label: Option<String>,
        message: Option<String>,
        progress_percent: Option<u8>,
    ) -> Result<Value, RpcError> {
        let mut state = self.state();
        let request = AgentRequest {
            agent_id: agent_id.to_owned(),
            session_id: Some(session_id.clone()),
            profile_id: profile_id.to_owned(),
        };
        let agent = state
            .agents
            .get_mut(agent_id)
            .ok_or_else(|| agent_not_found_error(&request))?;
        ensure_agent_control_scope(agent, Some(session_id), profile_id)?;
        if !is_agent_terminal_status(&agent.status) {
            agent.status = "running".to_owned();
        }
        if let Some(message) = message.as_ref().filter(|message| !message.is_empty()) {
            agent.last_task = Some(message.clone());
        }
        agent.updated_at_ms = now_ms();
        let agent = agent.clone();
        persist_agent_heartbeat(
            &state,
            &agent,
            ping_id,
            state_label,
            message,
            progress_percent,
        );
        Ok(autonomy_agent_json(&agent))
    }

    pub(crate) fn append_agent_output(
        &self,
        agent_id: &str,
        session_id: &SessionKey,
        profile_id: &str,
        text: &str,
    ) -> Result<(), RpcError> {
        let mut state = self.state();
        let request = AgentRequest {
            agent_id: agent_id.to_owned(),
            session_id: Some(session_id.clone()),
            profile_id: profile_id.to_owned(),
        };
        let agent = state
            .agents
            .get_mut(agent_id)
            .ok_or_else(|| agent_not_found_error(&request))?;
        ensure_agent_control_scope(agent, Some(session_id), profile_id)?;
        agent.output.push_str(text);
        agent.updated_at_ms = now_ms();
        Ok(())
    }

    pub(crate) fn set_agent_artifacts(
        &self,
        agent_id: &str,
        session_id: &SessionKey,
        profile_id: &str,
        artifacts: Vec<AgentArtifactRecord>,
    ) -> Result<Value, RpcError> {
        let mut state = self.state();
        let request = AgentRequest {
            agent_id: agent_id.to_owned(),
            session_id: Some(session_id.clone()),
            profile_id: profile_id.to_owned(),
        };
        let agent = state
            .agents
            .get_mut(agent_id)
            .ok_or_else(|| agent_not_found_error(&request))?;
        ensure_agent_control_scope(agent, Some(session_id), profile_id)?;
        agent.artifacts = artifacts;
        agent.updated_at_ms = now_ms();
        let agent = agent.clone();
        persist_agent_artifacts(&state, &agent);
        Ok(autonomy_agent_json(&agent))
    }

    pub(crate) fn drain_ready_continuations_for_session(
        &self,
        session_id: &SessionKey,
        profile_id: &str,
        runtime_state: MasterContinuationRuntimeState,
        max_items: usize,
    ) -> Vec<QueuedMasterContinuation> {
        let mut state = self.state();
        enqueue_due_loop_continuations(&mut state, session_id, profile_id, runtime_state, now_ms());
        state.continuations.drain_ready_for_session(
            runtime_state,
            max_items,
            &session_id.to_string(),
            profile_id,
        )
    }

    pub(crate) fn due_loop_targets(
        &self,
        profile_filter: Option<&str>,
        max_items: usize,
    ) -> Vec<(SessionKey, String)> {
        if max_items == 0 {
            return Vec::new();
        }

        let state = self.state();
        let now = now_ms();
        let mut targets = Vec::new();
        for loop_record in state.loops.values() {
            if loop_record.status != "active"
                || loop_record.mode != "fixed_interval"
                || loop_record.expires_at_ms <= now
                || profile_filter.is_some_and(|profile_id| loop_record.profile_id != profile_id)
                || loop_record
                    .next_run_at_ms
                    .is_none_or(|next_run_at| next_run_at > now)
            {
                continue;
            }
            let target = (
                loop_record.session_id.clone(),
                loop_record.profile_id.clone(),
            );
            if !targets.contains(&target) {
                targets.push(target);
                if targets.len() >= max_items {
                    break;
                }
            }
        }
        targets
    }

    #[cfg(test)]
    pub(crate) fn tick_due_loops_for_session(
        &self,
        session_id: &SessionKey,
        profile_id: &str,
        runtime_state: MasterContinuationRuntimeState,
    ) -> usize {
        let mut state = self.state();
        enqueue_due_loop_continuations(&mut state, session_id, profile_id, runtime_state, now_ms())
    }

    pub(crate) fn mark_continuation_started(&self, continuation: &QueuedMasterContinuation) {
        let state = self.state();
        if let Some(store) = state.supervisor_store.as_ref() {
            let _ = store.record_continuation_started(
                continuation.group_id.as_str(),
                continuation.dedupe_key.as_str(),
                now_ms_u64(),
            );
        }
    }

    pub(crate) fn mark_continuation_completed(
        &self,
        continuation: &QueuedMasterContinuation,
        result: Option<String>,
    ) {
        let state = self.state();
        if let Some(store) = state.supervisor_store.as_ref() {
            let _ = store.record_continuation_completed(
                continuation.group_id.as_str(),
                continuation.dedupe_key.as_str(),
                now_ms_u64(),
                result,
            );
        }
    }

    #[cfg(test)]
    pub(crate) fn pending_continuation_count_for_test(&self) -> usize {
        self.state().continuations.len()
    }

    #[cfg(test)]
    pub(crate) fn pending_continuation_count_for_session_for_test(
        &self,
        session_id: &SessionKey,
        profile_id: &str,
    ) -> usize {
        self.state()
            .continuations
            .pending_count_for_session(&session_id.to_string(), profile_id)
    }

    #[cfg(test)]
    pub(crate) fn force_loop_due_for_test(&self, loop_id: &str) {
        let mut state = self.state();
        if let Some(loop_record) = state.loops.get_mut(loop_id) {
            loop_record.next_run_at_ms = Some(now_ms().saturating_sub(1));
            loop_record.updated_at_ms = now_ms();
        }
    }
}

impl AgentOrchestrator for InProcessAgentOrchestrator {
    fn list_agents(&self, request: AgentListRequest) -> Result<Value, RpcError> {
        let state = self.state();
        let scoped_profile_id = request
            .connection_profile_id
            .as_deref()
            .unwrap_or(&request.profile_id);
        let agents = state
            .agents
            .values()
            .filter(|agent| {
                request
                    .session_id
                    .as_ref()
                    .is_none_or(|session_id| session_controls_target(session_id, &agent.session_id))
            })
            .filter(|agent| {
                request.connection_profile_id.is_none()
                    || agent.profile_id == scoped_profile_id
                    || agent.session_id.profile_id().is_none()
            })
            .map(autonomy_agent_json)
            .collect::<Vec<_>>();
        Ok(json!({
            "session_id": request.session_id,
            "profile_id": request.profile_id,
            "agents": agents
        }))
    }

    fn read_agent_status(&self, request: AgentRequest) -> Result<Value, RpcError> {
        let state = self.state();
        let agent = get_agent(&state, &request)?;
        Ok(json!({
            "session_id": agent.session_id,
            "agent": autonomy_agent_json(agent)
        }))
    }

    fn read_agent_output(&self, request: AgentOutputRequest) -> Result<Value, RpcError> {
        let state = self.state();
        let profile_id = request.profile_id.clone();
        let cursor = request.cursor.clone();
        let limit = request.limit;
        let agent = get_agent(
            &state,
            &AgentRequest {
                agent_id: request.agent_id,
                session_id: request.session_id,
                profile_id,
            },
        )?;
        let window = agent_output_window(
            &agent.output,
            cursor.as_ref(),
            limit,
            &agent.session_id,
            &agent.profile_id,
        )?;
        Ok(json!({
            "agent_id": agent.agent_id,
            "session_id": agent.session_id,
            "source": "runtime",
            "text": window.text,
            "messages": [],
            "cursor": { "offset": window.start_offset },
            "next_cursor": { "offset": window.end_offset },
            "has_more": window.end_offset < agent.output.len(),
            "complete": matches!(agent.status.as_str(), "completed" | "failed" | "interrupted" | "closed")
        }))
    }

    fn list_agent_artifacts(&self, request: AgentRequest) -> Result<Value, RpcError> {
        let state = self.state();
        let agent = get_agent(&state, &request)?;
        Ok(json!({
            "agent_id": agent.agent_id,
            "session_id": agent.session_id,
            "artifacts": agent.artifacts.iter().map(agent_artifact_json).collect::<Vec<_>>()
        }))
    }

    fn read_agent_artifact(&self, request: AgentArtifactReadRequest) -> Result<Value, RpcError> {
        if request.artifact_id.is_none() && request.path.is_none() {
            return Err(agent_invalid_params_error(
                AGENT_ARTIFACT_SELECTOR_INVALID,
                "agent artifact read requires artifact_id or path",
                request.session_id.as_ref(),
                Some(&request.profile_id),
                Some(("agent_id", request.agent_id.as_str())),
            ));
        }
        let state = self.state();
        let agent = get_agent(
            &state,
            &AgentRequest {
                agent_id: request.agent_id,
                session_id: request.session_id,
                profile_id: request.profile_id.clone(),
            },
        )?;
        let requested_id = request
            .artifact_id
            .as_deref()
            .or(request.path.as_deref())
            .unwrap_or("unknown");
        if let Some(artifact) = agent.artifacts.iter().find(|artifact| {
            request
                .artifact_id
                .as_ref()
                .is_some_and(|id| id == &artifact.id)
                || request
                    .path
                    .as_ref()
                    .is_some_and(|path| artifact.path.as_ref() == Some(path))
        }) {
            // #967 / M13-C — redact well-known credential patterns from
            // artifact `content` before returning it to the AppUI client.
            // The orchestrator may surface child-task artifacts to a
            // parent session through this RPC, and any leaked provider
            // key / bearer token / AWS access key in the payload would
            // become reachable by every successful parent-controls-child
            // caller. See `redact_artifact_secrets` for the full pattern
            // set (intentionally a conservative subset of
            // `octos_agent::sanitize` so legitimate evidence payloads —
            // long hex digests, base64 blobs — pass through unchanged).
            let content = artifact
                .content
                .as_deref()
                .map(|raw| redact_artifact_secrets(raw).into_owned());
            return Ok(json!({
                "agent_id": agent.agent_id,
                "session_id": agent.session_id,
                "artifact": agent_artifact_json(artifact),
                "content": content,
            }));
        }
        Err(autonomy_error(
            kinds::AGENT_ARTIFACT_DENIED,
            "agent artifact is not available",
            Some(&agent.session_id),
            Some(&request.profile_id),
            Some(("artifact_id", requested_id)),
            true,
        ))
    }

    fn interrupt_agent(&self, request: AgentRequest) -> Result<Value, RpcError> {
        update_agent_terminal_status(self, request, "interrupted", true, false)
    }

    fn close_agent(&self, request: AgentRequest) -> Result<Value, RpcError> {
        update_agent_terminal_status(self, request, "closed", false, true)
    }

    fn get_goal(&self, request: GoalSessionRequest) -> Result<Value, RpcError> {
        let state = self.state();
        let goal = state
            .goals
            .get(&request.session_id)
            .filter(|goal| goal.profile_id == request.profile_id)
            .map(autonomy_goal_json);
        Ok(json!({
            "session_id": request.session_id,
            "profile_id": request.profile_id,
            "goal": goal
        }))
    }

    fn set_goal(&self, request: GoalSetRequest) -> Result<Value, RpcError> {
        let objective = request.objective.trim();
        if objective.is_empty() || objective.len() > MAX_OBJECTIVE_BYTES {
            return Err(autonomy_error(
                kinds::GOAL_INVALID_STATE,
                "goal objective is empty or exceeds backend policy limit",
                Some(&request.session_id),
                Some(&request.profile_id),
                None,
                true,
            ));
        }
        let requested_status = request.status.as_deref();
        if requested_status.is_some_and(|status| {
            !matches!(status, "active" | "paused" | "budget_limited" | "complete")
        }) {
            return Err(autonomy_error(
                kinds::GOAL_INVALID_STATE,
                "unsupported goal status",
                Some(&request.session_id),
                Some(&request.profile_id),
                None,
                true,
            ));
        }
        let transition_actor = request.transition_actor.as_deref().unwrap_or("user");
        if !matches!(transition_actor, "user" | "backend" | "model") {
            return Err(autonomy_error(
                kinds::GOAL_INVALID_STATE,
                "unsupported goal transition actor",
                Some(&request.session_id),
                Some(&request.profile_id),
                None,
                true,
            ));
        }
        if request
            .token_budget
            .is_some_and(|token_budget| token_budget > GOAL_MAX_TOKEN_BUDGET)
        {
            return Err(autonomy_error(
                kinds::AUTONOMY_QUOTA_EXCEEDED,
                "goal token budget exceeds backend policy limit",
                Some(&request.session_id),
                Some(&request.profile_id),
                None,
                true,
            ));
        }
        let now = now_ms();
        let mut state = self.state();
        let goal = if let Some(goal) = state.goals.get_mut(&request.session_id) {
            if goal.profile_id != request.profile_id {
                return Err(autonomy_error(
                    kinds::GOAL_UNAVAILABLE,
                    "goal is outside the requested profile scope",
                    Some(&request.session_id),
                    Some(&request.profile_id),
                    None,
                    true,
                ));
            }
            goal.objective = objective.to_owned();
            if let Some(status) = requested_status {
                goal.status = status.to_owned();
            }
            if let Some(token_budget) = request.token_budget {
                goal.token_budget = token_budget;
            }
            goal.updated_at_ms = now;
            goal.clone()
        } else {
            state.next_goal_seq += 1;
            let goal = AutonomyGoalRecord {
                profile_id: request.profile_id.clone(),
                goal_id: format!("goal_{:02}", state.next_goal_seq),
                objective: objective.to_owned(),
                status: requested_status.unwrap_or("active").to_owned(),
                token_budget: request.token_budget.unwrap_or(GOAL_DEFAULT_TOKEN_BUDGET),
                tokens_used: 0,
                time_used_seconds: 0,
                created_at_ms: now,
                updated_at_ms: now,
            };
            state.goals.insert(request.session_id.clone(), goal.clone());
            goal
        };
        if goal.status == "active" {
            enqueue_goal_continuation(&mut state, &request.session_id, &request.profile_id, &goal);
        }
        persist_goal_state(&state, &request.session_id, &goal, false);
        Ok(json!({
            "session_id": request.session_id,
            "profile_id": request.profile_id,
            "goal": autonomy_goal_json(&goal),
            "transition_actor": transition_actor
        }))
    }

    fn clear_goal(&self, request: GoalSessionRequest) -> Result<Value, RpcError> {
        let mut state = self.state();
        let cleared = match state.goals.get(&request.session_id) {
            Some(goal) if goal.profile_id == request.profile_id => {
                state.goals.remove(&request.session_id).is_some()
            }
            Some(_) => {
                return Err(autonomy_error(
                    kinds::GOAL_UNAVAILABLE,
                    "goal is outside the requested profile scope",
                    Some(&request.session_id),
                    Some(&request.profile_id),
                    None,
                    true,
                ));
            }
            None => false,
        };
        if cleared {
            persist_goal_cleared(&state, &request.session_id, &request.profile_id);
        }
        Ok(json!({
            "session_id": request.session_id,
            "profile_id": request.profile_id,
            "cleared": cleared,
            "goal": Value::Null,
            "transition_actor": "user"
        }))
    }

    fn create_loop(&self, request: LoopCreateRequest) -> Result<Value, RpcError> {
        let parsed = parse_loop_create(&request)?;
        let now = now_ms();
        let mut state = self.state();
        let active_count = state
            .loops
            .values()
            .filter(|loop_record| {
                loop_record.session_id == request.session_id
                    && loop_record.profile_id == request.profile_id
                    && loop_record.status != "deleted"
            })
            .count();
        if active_count >= MAX_LOOPS_PER_SESSION {
            return Err(autonomy_error(
                kinds::AUTONOMY_QUOTA_EXCEEDED,
                "session has reached the backend loop limit",
                Some(&request.session_id),
                Some(&request.profile_id),
                None,
                true,
            ));
        }
        state.next_loop_seq += 1;
        let loop_record = AutonomyLoopRecord {
            loop_id: format!("loop_{:02}", state.next_loop_seq),
            session_id: request.session_id.clone(),
            profile_id: request.profile_id.clone(),
            prompt: parsed.prompt,
            mode: parsed.mode,
            interval_seconds: parsed.interval_seconds,
            status: "active".into(),
            next_run_at_ms: parsed.interval_seconds.and_then(|seconds| {
                i64::try_from(seconds)
                    .ok()
                    .and_then(|seconds| seconds.checked_mul(1_000))
                    .and_then(|delay_ms| now.checked_add(delay_ms))
            }),
            last_run_at_ms: None,
            expires_at_ms: now + LOOP_MAX_AGE_DAYS * 24 * 60 * 60 * 1_000,
            created_at_ms: now,
            updated_at_ms: now,
        };
        state
            .loops
            .insert(loop_record.loop_id.clone(), loop_record.clone());
        persist_loop_state(&state, &loop_record);
        Ok(json!({
            "session_id": request.session_id,
            "profile_id": request.profile_id,
            "loop_id": loop_record.loop_id,
            "loop": autonomy_loop_json(&loop_record),
            "ok": true,
            "status": loop_record.status,
            "created": true,
            "fire": {
                "queued": false,
                "reason": "waiting_for_schedule",
                "message": "loop created; it will queue a master continuation when due or when loop/fire_now is called"
            }
        }))
    }

    fn list_loops(&self, request: LoopListRequest) -> Result<Value, RpcError> {
        let state = self.state();
        let loops = state
            .loops
            .values()
            .filter(|loop_record| loop_record.status != "deleted")
            .filter(|loop_record| loop_record.profile_id == request.profile_id)
            .filter(|loop_record| {
                request.session_id.as_ref().is_none_or(|session_id| {
                    session_controls_target(session_id, &loop_record.session_id)
                })
            })
            .map(autonomy_loop_json)
            .collect::<Vec<_>>();
        Ok(json!({
            "session_id": request.session_id,
            "profile_id": request.profile_id,
            "loops": loops
        }))
    }

    fn control_loop(&self, request: LoopControlRequest) -> Result<Value, RpcError> {
        let mut state = self.state();
        let supervisor_store = state.supervisor_store.clone();
        let Some(loop_record) = state.loops.get_mut(&request.loop_id) else {
            return Err(autonomy_error(
                kinds::LOOP_NOT_FOUND,
                "loop not found",
                request.session_id.as_ref(),
                Some(&request.profile_id),
                Some(("loop_id", request.loop_id.as_str())),
                true,
            ));
        };
        ensure_loop_scope(
            loop_record,
            request.session_id.as_ref(),
            &request.profile_id,
        )?;
        if loop_record.status == "deleted" {
            return Err(autonomy_error(
                kinds::LOOP_NOT_FOUND,
                "loop not found",
                request
                    .session_id
                    .as_ref()
                    .or(Some(&loop_record.session_id)),
                Some(&request.profile_id),
                Some(("loop_id", loop_record.loop_id.as_str())),
                true,
            ));
        }
        let now = now_ms();
        match request.kind {
            LoopControlKind::Delete => {
                loop_record.status = "deleted".into();
                loop_record.updated_at_ms = now;
                persist_loop_state_with_store(supervisor_store.as_ref(), loop_record);
                Ok(json!({
                    "loop_id": loop_record.loop_id,
                    "session_id": loop_record.session_id,
                    "deleted": true,
                    "ok": true,
                    "status": loop_record.status,
                    "loop": autonomy_loop_json(loop_record)
                }))
            }
            LoopControlKind::Pause => {
                loop_record.status = "paused".into();
                loop_record.updated_at_ms = now;
                persist_loop_state_with_store(supervisor_store.as_ref(), loop_record);
                Ok(json!({
                    "session_id": loop_record.session_id,
                    "loop_id": loop_record.loop_id,
                    "loop": autonomy_loop_json(loop_record),
                    "ok": true,
                    "status": loop_record.status
                }))
            }
            LoopControlKind::Resume => {
                loop_record.status = "active".into();
                loop_record.updated_at_ms = now;
                persist_loop_state_with_store(supervisor_store.as_ref(), loop_record);
                Ok(json!({
                    "session_id": loop_record.session_id,
                    "loop_id": loop_record.loop_id,
                    "loop": autonomy_loop_json(loop_record),
                    "ok": true,
                    "status": loop_record.status
                }))
            }
            LoopControlKind::FireNow => {
                if loop_record.status != "active" {
                    return Err(autonomy_error(
                        kinds::LOOP_POLICY_DENIED,
                        "loop is not active",
                        Some(&loop_record.session_id),
                        Some(&request.profile_id),
                        Some(("loop_id", loop_record.loop_id.as_str())),
                        true,
                    ));
                }
                let session_id = loop_record.session_id.clone();
                let profile_id = loop_record.profile_id.clone();
                let loop_id = loop_record.loop_id.clone();
                let prompt = loop_record.prompt.clone();
                let interval_seconds = loop_record.interval_seconds;
                loop_record.last_run_at_ms = Some(now);
                loop_record.next_run_at_ms = interval_seconds.and_then(|seconds| {
                    i64::try_from(seconds)
                        .ok()
                        .and_then(|seconds| seconds.checked_mul(1_000))
                        .and_then(|delay_ms| now.checked_add(delay_ms))
                });
                loop_record.updated_at_ms = now;
                let loop_json = autonomy_loop_json(loop_record);
                persist_loop_state_with_store(supervisor_store.as_ref(), loop_record);

                let continuation = MasterContinuationRequest::new(
                    "coding-autonomy",
                    session_id.to_string(),
                    profile_id.clone(),
                    MasterContinuationReason::LoopFire,
                    SystemTime::now(),
                )
                .with_loop_id(loop_id.clone())
                .with_metadata("prompt", prompt);
                let fire = master_continuation_enqueue_json(enqueue_and_persist_continuation(
                    &mut state,
                    continuation,
                ));

                Ok(json!({
                    "session_id": session_id,
                    "profile_id": profile_id,
                    "loop_id": loop_id,
                    "loop": loop_json,
                    "ok": true,
                    "status": "queued",
                    "fire": fire
                }))
            }
        }
    }
}

pub(crate) fn default_agent_orchestrator() -> &'static InProcessAgentOrchestrator {
    static ORCHESTRATOR: OnceLock<InProcessAgentOrchestrator> = OnceLock::new();
    ORCHESTRATOR.get_or_init(InProcessAgentOrchestrator::default)
}

#[cfg(test)]
pub(crate) fn clear_default_agent_orchestrator_for_test() {
    default_agent_orchestrator().clear_for_test();
}

#[derive(Debug, Default)]
struct AutonomyRuntimeState {
    agents: HashMap<String, AutonomyAgentRecord>,
    goals: HashMap<SessionKey, AutonomyGoalRecord>,
    loops: HashMap<String, AutonomyLoopRecord>,
    continuations: MasterContinuationScheduler,
    supervisor_store: Option<SupervisorStore>,
    next_goal_seq: u64,
    next_loop_seq: u64,
}

#[derive(Debug, Clone)]
struct AutonomyAgentRecord {
    agent_id: String,
    parent_agent_id: Option<String>,
    session_id: SessionKey,
    task_id: Option<TaskId>,
    path: String,
    role: String,
    nickname: String,
    backend_kind: String,
    status: String,
    last_task: Option<String>,
    cwd: Option<String>,
    profile_id: String,
    output: String,
    artifacts: Vec<AgentArtifactRecord>,
    created_at_ms: i64,
    updated_at_ms: i64,
    /// #1021 / M17-C — most-recent dispatch context contract for this child agent. Populated by specialist runners (CLI / native / MCP) when they emit a dispatch and surfaced through `agent/updated` so AppUI clients can tell `managed_payload` from `external_context_unmanaged` per child.
    context_contract: Option<DispatchContextContract>,
}

#[derive(Debug, Clone)]
struct AutonomyGoalRecord {
    profile_id: String,
    goal_id: String,
    objective: String,
    status: String,
    token_budget: u64,
    tokens_used: u64,
    time_used_seconds: u64,
    created_at_ms: i64,
    updated_at_ms: i64,
}

#[derive(Debug, Clone)]
struct AutonomyLoopRecord {
    loop_id: String,
    session_id: SessionKey,
    profile_id: String,
    prompt: String,
    mode: String,
    interval_seconds: Option<u64>,
    status: String,
    next_run_at_ms: Option<i64>,
    last_run_at_ms: Option<i64>,
    expires_at_ms: i64,
    created_at_ms: i64,
    updated_at_ms: i64,
}

struct ParsedLoopCreate {
    prompt: String,
    mode: String,
    interval_seconds: Option<u64>,
}

struct DueLoopFire {
    session_id: SessionKey,
    profile_id: String,
    loop_id: String,
    prompt: String,
    scheduled_for_ms: i64,
}

fn enqueue_due_loop_continuations(
    state: &mut AutonomyRuntimeState,
    session_id: &SessionKey,
    profile_id: &str,
    runtime_state: MasterContinuationRuntimeState,
    now: i64,
) -> usize {
    if !runtime_state.is_idle_eligible() {
        return 0;
    }

    let mut due = Vec::new();
    let mut updated_loops = Vec::new();
    for loop_record in state.loops.values_mut() {
        if loop_record.status != "active"
            || loop_record.mode != "fixed_interval"
            || loop_record.profile_id != profile_id
            || !session_controls_target(session_id, &loop_record.session_id)
            || loop_record.expires_at_ms <= now
        {
            continue;
        }
        let Some(next_run_at_ms) = loop_record.next_run_at_ms else {
            continue;
        };
        if next_run_at_ms > now {
            continue;
        }
        let Some(interval_seconds) = loop_record.interval_seconds else {
            continue;
        };
        due.push(DueLoopFire {
            session_id: loop_record.session_id.clone(),
            profile_id: loop_record.profile_id.clone(),
            loop_id: loop_record.loop_id.clone(),
            prompt: loop_record.prompt.clone(),
            scheduled_for_ms: next_run_at_ms,
        });
        loop_record.last_run_at_ms = Some(now);
        loop_record.next_run_at_ms = next_loop_run_at(now, interval_seconds);
        loop_record.updated_at_ms = now;
        updated_loops.push(loop_record.clone());
    }

    for loop_record in &updated_loops {
        persist_loop_state(state, loop_record);
    }

    let mut queued = 0;
    for fire in due {
        let continuation = MasterContinuationRequest::new(
            "coding-autonomy",
            fire.session_id.to_string(),
            fire.profile_id.clone(),
            MasterContinuationReason::LoopFire,
            SystemTime::now(),
        )
        .with_loop_id(fire.loop_id)
        .with_metadata("prompt", fire.prompt)
        .with_metadata("scheduled_for_ms", fire.scheduled_for_ms.to_string());
        if enqueue_and_persist_continuation(state, continuation)
            .queued()
            .is_some()
        {
            queued += 1;
        }
    }
    queued
}

fn next_loop_run_at(now: i64, interval_seconds: u64) -> Option<i64> {
    i64::try_from(interval_seconds)
        .ok()
        .and_then(|seconds| seconds.checked_mul(1_000))
        .and_then(|delay_ms| now.checked_add(delay_ms))
}

fn update_agent_terminal_status(
    orchestrator: &InProcessAgentOrchestrator,
    request: AgentRequest,
    status: &str,
    interrupted: bool,
    closed: bool,
) -> Result<Value, RpcError> {
    let mut state = orchestrator.state();
    let Some(agent) = state.agents.get_mut(&request.agent_id) else {
        return Err(agent_not_found_error(&request));
    };
    ensure_agent_control_scope(agent, request.session_id.as_ref(), &request.profile_id)?;
    if agent.status == status {
        return Ok(json!({
            "agent_id": agent.agent_id,
            "session_id": agent.session_id,
            "status": agent.status,
            "ok": true,
            "interrupted": interrupted,
            "closed": closed,
            "already_terminal": true
        }));
    }
    if is_agent_terminal_status(&agent.status) {
        let mut error = autonomy_error(
            kinds::AGENT_CONTROL_UNAVAILABLE,
            "agent is already terminal",
            request.session_id.as_ref().or(Some(&agent.session_id)),
            Some(&request.profile_id),
            Some(("agent_id", agent.agent_id.as_str())),
            true,
        );
        if let Some(Value::Object(data)) = error.data.as_mut() {
            data.insert("current_status".into(), json!(agent.status));
            data.insert("requested_status".into(), json!(status));
        }
        return Err(error);
    }
    agent.status = status.into();
    agent.updated_at_ms = now_ms();
    let agent = agent.clone();
    enqueue_agent_terminal_continuations(&mut state, &agent);
    Ok(json!({
        "agent_id": agent.agent_id,
        "session_id": agent.session_id,
        "status": agent.status,
        "ok": true,
        "interrupted": interrupted,
        "closed": closed,
        "already_terminal": false
    }))
}

fn get_agent<'a>(
    state: &'a AutonomyRuntimeState,
    request: &AgentRequest,
) -> Result<&'a AutonomyAgentRecord, RpcError> {
    let Some(agent) = state.agents.get(&request.agent_id) else {
        return Err(agent_not_found_error(request));
    };
    ensure_agent_control_scope(agent, request.session_id.as_ref(), &request.profile_id)?;
    Ok(agent)
}

fn agent_not_found_error(request: &AgentRequest) -> RpcError {
    autonomy_error(
        kinds::AGENT_NOT_FOUND,
        "agent not found",
        request.session_id.as_ref(),
        Some(&request.profile_id),
        Some(("agent_id", request.agent_id.as_str())),
        true,
    )
}

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

fn now_ms_u64() -> u64 {
    now_ms().try_into().unwrap_or(0)
}

fn autonomy_error_code(kind: &str) -> i64 {
    match kind {
        kinds::AGENT_CONTROL_FORBIDDEN
        | kinds::AGENT_ARTIFACT_DENIED
        | kinds::LOOP_SLASH_DENIED
        | kinds::LOOP_POLICY_DENIED => rpc_error_codes::PERMISSION_DENIED,
        kinds::AGENT_NOT_FOUND | kinds::GOAL_UNAVAILABLE | kinds::LOOP_NOT_FOUND => {
            rpc_error_codes::RESOURCE_NOT_FOUND
        }
        kinds::AGENT_CONTROL_UNAVAILABLE
        | kinds::GOAL_RUNTIME_UNAVAILABLE
        | kinds::LOOP_RUNTIME_UNAVAILABLE => rpc_error_codes::RUNTIME_NOT_READY,
        kinds::GOAL_RATE_LIMITED | kinds::LOOP_BUSY | kinds::AUTONOMY_QUOTA_EXCEEDED => {
            rpc_error_codes::RATE_LIMITED
        }
        _ => rpc_error_codes::INVALID_PARAMS,
    }
}

fn autonomy_error(
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

fn agent_invalid_params_error(
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

fn session_controls_target(requested: &SessionKey, target: &SessionKey) -> bool {
    requested == target || requested.base_key() == target.base_key()
}

fn ensure_agent_control_scope(
    agent: &AutonomyAgentRecord,
    requested_session_id: Option<&SessionKey>,
    profile_id: &str,
) -> Result<(), RpcError> {
    if agent.profile_id != profile_id {
        return Err(autonomy_error(
            kinds::AGENT_CONTROL_FORBIDDEN,
            "agent is outside the requested profile scope",
            requested_session_id.or(Some(&agent.session_id)),
            Some(profile_id),
            Some(("agent_id", agent.agent_id.as_str())),
            true,
        ));
    }
    if let Some(requested_session_id) = requested_session_id {
        if !session_controls_target(requested_session_id, &agent.session_id) {
            return Err(autonomy_error(
                kinds::AGENT_CONTROL_FORBIDDEN,
                "agent is outside the requested session scope",
                Some(requested_session_id),
                Some(profile_id),
                Some(("agent_id", agent.agent_id.as_str())),
                true,
            ));
        }
    }
    Ok(())
}

fn is_agent_terminal_status(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "interrupted" | "closed")
}

fn enqueue_and_persist_continuation(
    state: &mut AutonomyRuntimeState,
    request: MasterContinuationRequest,
) -> MasterContinuationEnqueueOutcome {
    let outcome = state.continuations.enqueue(request);
    if let MasterContinuationEnqueueOutcome::Queued(continuation) = &outcome {
        persist_continuation_queued(state, continuation);
    }
    outcome
}

fn enqueue_goal_continuation(
    state: &mut AutonomyRuntimeState,
    session_id: &SessionKey,
    profile_id: &str,
    goal: &AutonomyGoalRecord,
) {
    let continuation = MasterContinuationRequest::new(
        "coding-autonomy-goal",
        session_id.to_string(),
        profile_id.to_owned(),
        MasterContinuationReason::GoalContinue,
        SystemTime::now(),
    )
    .with_goal_id(goal.goal_id.clone())
    .with_metadata("objective", goal.objective.clone())
    .with_metadata("status", goal.status.clone());
    enqueue_and_persist_continuation(state, continuation);
}

fn enqueue_agent_terminal_continuations(
    state: &mut AutonomyRuntimeState,
    agent: &AutonomyAgentRecord,
) {
    let group_id = agent_continuation_group_id(agent);
    let mut child = MasterContinuationRequest::new(
        group_id.clone(),
        agent.session_id.to_string(),
        agent.profile_id.clone(),
        MasterContinuationReason::ChildCompleted,
        SystemTime::now(),
    )
    .with_child_agent_id(agent.agent_id.clone())
    .with_metadata("status", agent.status.clone())
    .with_metadata("nickname", agent.nickname.clone())
    .with_metadata("role", agent.role.clone());
    if let Some(last_task) = agent.last_task.as_deref().filter(|value| !value.is_empty()) {
        child = child.with_metadata("summary", last_task.chars().take(1200).collect::<String>());
    }
    enqueue_and_persist_continuation(state, child);
    persist_agent_terminal(state, agent);

    let siblings = state
        .agents
        .values()
        .filter(|candidate| {
            candidate.session_id == agent.session_id
                && candidate.profile_id == agent.profile_id
                && candidate.parent_agent_id == agent.parent_agent_id
        })
        .collect::<Vec<_>>();
    if siblings.is_empty()
        || !siblings
            .iter()
            .all(|candidate| is_agent_terminal_status(&candidate.status))
    {
        return;
    }
    let scatter = MasterContinuationRequest::new(
        group_id,
        agent.session_id.to_string(),
        agent.profile_id.clone(),
        MasterContinuationReason::ScatterJoinComplete,
        SystemTime::now(),
    )
    .with_metadata(
        "parent_agent_id",
        agent
            .parent_agent_id
            .clone()
            .unwrap_or_else(|| "master".to_owned()),
    )
    .with_metadata("terminal_children", siblings.len().to_string());
    enqueue_and_persist_continuation(state, scatter);
}

fn agent_continuation_group_id(agent: &AutonomyAgentRecord) -> String {
    format!(
        "agent-group:{}:{}:{}",
        agent.profile_id,
        agent.session_id,
        agent.parent_agent_id.as_deref().unwrap_or("master")
    )
}

fn background_task_session_id(task: &octos_agent::BackgroundTask) -> Option<SessionKey> {
    task.session_key
        .as_deref()
        .or(task.parent_session_key.as_deref())
        .or(task.child_session_key.as_deref())
        .filter(|value| !value.is_empty())
        .map(|value| SessionKey(value.to_owned()))
}

fn background_task_agent_id(task: &octos_agent::BackgroundTask) -> String {
    task.child_session_key
        .as_deref()
        .filter(|value| !value.is_empty())
        .map(|value| {
            format!(
                "task-{}",
                value
                    .chars()
                    .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
                    .collect::<String>()
            )
        })
        .unwrap_or_else(|| format!("task-{}", task.id))
}

fn background_task_agent_status(task: &octos_agent::BackgroundTask) -> String {
    match &task.status {
        octos_agent::TaskStatus::Spawned | octos_agent::TaskStatus::Running => "running",
        octos_agent::TaskStatus::Completed => "completed",
        octos_agent::TaskStatus::Failed => "failed",
        octos_agent::TaskStatus::Cancelled => "interrupted",
    }
    .to_owned()
}

fn background_task_backend_kind(task: &octos_agent::BackgroundTask) -> String {
    if task.child_session_key.is_some() {
        "spawn_child_session".to_owned()
    } else {
        format!("task_supervisor:{}", task.tool_name)
    }
}

fn background_task_nickname(task: &octos_agent::BackgroundTask) -> String {
    let phase = task
        .runtime_detail
        .as_deref()
        .and_then(|detail| serde_json::from_str::<Value>(detail).ok())
        .and_then(|detail| {
            detail
                .get("workflow_kind")
                .and_then(Value::as_str)
                .or_else(|| detail.get("current_phase").and_then(Value::as_str))
                .map(str::to_owned)
        });
    match phase {
        Some(phase) if !phase.is_empty() => format!("{} {}", task.tool_name, phase),
        _ => task.tool_name.clone(),
    }
}

fn background_task_last_task(task: &octos_agent::BackgroundTask) -> Option<String> {
    if let Some(error) = task.error.as_deref().filter(|error| !error.is_empty()) {
        return Some(error.chars().take(1200).collect());
    }
    if let Some(message) = task
        .runtime_detail
        .as_deref()
        .and_then(|detail| serde_json::from_str::<Value>(detail).ok())
        .and_then(|detail| {
            detail
                .get("progress_message")
                .and_then(Value::as_str)
                .or_else(|| detail.get("current_phase").and_then(Value::as_str))
                .map(str::to_owned)
        })
        .filter(|message| !message.is_empty())
    {
        return Some(message.chars().take(1200).collect());
    }
    if !task.output_files.is_empty() {
        return Some(format!(
            "{} completed with {} output file(s)",
            task.tool_name,
            task.output_files.len()
        ));
    }
    Some(format!("{} {}", task.tool_name, task.status.as_str()))
}

fn background_task_cwd(task: &octos_agent::BackgroundTask) -> Option<String> {
    task.output_files
        .first()
        .and_then(|path| Path::new(path).parent())
        .map(|path| path.to_string_lossy().into_owned())
        .filter(|path| !path.is_empty())
}

fn background_task_artifacts(task: &octos_agent::BackgroundTask) -> Vec<AgentArtifactRecord> {
    task.output_files
        .iter()
        .enumerate()
        .map(|(index, path)| {
            let title = Path::new(path)
                .file_name()
                .and_then(|name| name.to_str())
                .filter(|name| !name.is_empty())
                .unwrap_or(path)
                .to_owned();
            AgentArtifactRecord {
                id: format!("output-{:02}", index + 1),
                title,
                kind: "file".to_owned(),
                status: "ready".to_owned(),
                path: Some(path.clone()),
                content: None,
            }
        })
        .collect()
}

fn persist_agent_started(state: &AutonomyRuntimeState, agent: &AutonomyAgentRecord) {
    let Some(store) = state.supervisor_store.as_ref() else {
        return;
    };
    let group_id = agent_continuation_group_id(agent);
    let observed_at_ms = now_ms_u64();
    let mut group = SupervisedGroupRecord::new(group_id.clone(), observed_at_ms);
    group.parent_session_id = Some(agent.session_id.to_string());
    group.objective = agent.last_task.clone();
    let _ = store.record_group_registered(group);

    let mut child = ChildAgentRecord::new(group_id, agent.agent_id.clone(), observed_at_ms);
    child.label = Some(agent.nickname.clone());
    child.profile_id = Some(agent.profile_id.clone());
    child.task = agent.last_task.clone();
    child.workspace_path = agent.cwd.clone();
    child.status = ChildStatus::Running;
    child.metadata = supervisor_metadata_for_agent(agent);
    let _ = store.record_child_started(child);
}

fn persist_agent_terminal(state: &AutonomyRuntimeState, agent: &AutonomyAgentRecord) {
    let Some(store) = state.supervisor_store.as_ref() else {
        return;
    };
    persist_agent_started(state, agent);
    let group_id = agent_continuation_group_id(agent);
    let finished_at_ms = now_ms_u64();
    let terminal = match agent.status.as_str() {
        "completed" => TerminalState::completed(finished_at_ms, agent.last_task.clone()),
        "failed" => TerminalState::failed(finished_at_ms, None, agent.last_task.clone()),
        "interrupted" | "closed" => {
            TerminalState::cancelled(finished_at_ms, agent.last_task.clone())
        }
        _ => return,
    };
    let _ = store.record_child_terminal(group_id, agent.agent_id.clone(), terminal);
}

fn persist_agent_heartbeat(
    state: &AutonomyRuntimeState,
    agent: &AutonomyAgentRecord,
    ping_id: Option<String>,
    state_label: Option<String>,
    message: Option<String>,
    progress_percent: Option<u8>,
) {
    let Some(store) = state.supervisor_store.as_ref() else {
        return;
    };
    persist_agent_started(state, agent);
    let group_id = agent_continuation_group_id(agent);
    let mut metadata = SupervisorMetadata::new();
    metadata.insert("backend_kind".into(), json!(agent.backend_kind));
    let _ = store.record_heartbeat(HeartbeatPing {
        group_id,
        child_id: agent.agent_id.clone(),
        ping_id,
        observed_at_ms: now_ms_u64(),
        state: state_label,
        message,
        progress_percent,
        metadata,
    });
}

fn persist_agent_artifacts(state: &AutonomyRuntimeState, agent: &AutonomyAgentRecord) {
    let Some(store) = state.supervisor_store.as_ref() else {
        return;
    };
    let group_id = agent_continuation_group_id(agent);
    for artifact in &agent.artifacts {
        let Some(path) = artifact.path.clone() else {
            continue;
        };
        let _ = store.record_artifact_updated(SupervisorArtifactRecord {
            group_id: group_id.clone(),
            child_id: Some(agent.agent_id.clone()),
            artifact_id: artifact.id.clone(),
            kind: artifact.kind.clone(),
            path,
            display_name: Some(artifact.title.clone()),
            version: now_ms_u64(),
            updated_at_ms: now_ms_u64(),
            sha256: None,
            bytes: artifact
                .content
                .as_ref()
                .and_then(|content| content.len().try_into().ok()),
            metadata: SupervisorMetadata::new(),
        });
    }
}

fn persist_continuation_queued(
    state: &AutonomyRuntimeState,
    continuation: &QueuedMasterContinuation,
) {
    let Some(store) = state.supervisor_store.as_ref() else {
        return;
    };
    let mut metadata = SupervisorMetadata::new();
    metadata.insert("session_id".into(), json!(continuation.session_id.as_str()));
    metadata.insert("profile_id".into(), json!(continuation.profile_id.as_str()));
    metadata.insert(
        "reason".into(),
        json!(master_continuation_reason_wire_name(&continuation.reason)),
    );
    metadata.insert("dedupe_key".into(), json!(continuation.dedupe_key.as_str()));
    metadata.insert("priority".into(), json!(continuation.priority.rank()));
    if let Some(goal_id) = continuation.goal_id.as_ref() {
        metadata.insert("goal_id".into(), json!(goal_id.as_str()));
    }
    if let Some(loop_id) = continuation.loop_id.as_ref() {
        metadata.insert("loop_id".into(), json!(loop_id.as_str()));
    }
    for (key, value) in &continuation.metadata {
        metadata.insert(format!("payload:{key}"), json!(value));
    }
    let record = PendingContinuationRecord {
        group_id: continuation.group_id.as_str().to_owned(),
        continuation_id: continuation.dedupe_key.as_str().to_owned(),
        child_id: continuation
            .child_agent_id
            .as_ref()
            .map(|child_id| child_id.as_str().to_owned()),
        prompt: None,
        status: ContinuationStatus::Queued,
        queued_at_ms: now_ms_u64(),
        started_at_ms: None,
        completed_at_ms: None,
        result: None,
        attempt: 1,
        metadata,
    };
    let _ = store.record_continuation_queued(record);
}

fn master_continuation_request_from_persisted(
    continuation: &PendingContinuationRecord,
) -> Option<MasterContinuationRequest> {
    let session_id = supervisor_metadata_str(&continuation.metadata, "session_id")?;
    let profile_id = supervisor_metadata_str(&continuation.metadata, "profile_id")?;
    let reason = master_continuation_reason_from_wire_name(supervisor_metadata_str(
        &continuation.metadata,
        "reason",
    )?)?;
    let dedupe_key = supervisor_metadata_str(&continuation.metadata, "dedupe_key")
        .unwrap_or(&continuation.continuation_id);
    let mut request = MasterContinuationRequest::new(
        continuation.group_id.clone(),
        session_id.to_owned(),
        profile_id.to_owned(),
        reason,
        SystemTime::now(),
    )
    .with_dedupe_key(dedupe_key.to_owned());
    if let Some(child_id) = continuation.child_id.clone() {
        request = request.with_child_agent_id(child_id);
    }
    if let Some(goal_id) = supervisor_metadata_str(&continuation.metadata, "goal_id") {
        request = request.with_goal_id(goal_id.to_owned());
    }
    if let Some(loop_id) = supervisor_metadata_str(&continuation.metadata, "loop_id") {
        request = request.with_loop_id(loop_id.to_owned());
    }
    for (key, value) in &continuation.metadata {
        let Some(payload_key) = key.strip_prefix("payload:") else {
            continue;
        };
        if let Some(value) = value.as_str() {
            request = request.with_metadata(payload_key.to_owned(), value.to_owned());
        }
    }
    Some(request)
}

fn restore_runtime_from_supervisor_state(
    state: &mut AutonomyRuntimeState,
    supervisor_state: &SupervisorState,
) {
    restore_autonomy_records_from_supervisor_state(state, supervisor_state);
    restore_agents_from_supervisor_state(state, supervisor_state);
}

fn restore_autonomy_records_from_supervisor_state(
    state: &mut AutonomyRuntimeState,
    supervisor_state: &SupervisorState,
) {
    for group in supervisor_state.groups.values() {
        match supervisor_metadata_str(&group.metadata, AUTONOMY_RECORD_KIND) {
            Some(AUTONOMY_RECORD_GOAL) => restore_goal_from_group(state, group),
            Some(AUTONOMY_RECORD_LOOP) => restore_loop_from_group(state, group),
            _ => {}
        }
    }
}

fn restore_goal_from_group(state: &mut AutonomyRuntimeState, group: &SupervisedGroupRecord) {
    let Some(session_id) = supervisor_metadata_str(&group.metadata, "session_id") else {
        return;
    };
    let session_id = SessionKey(session_id.to_owned());
    if supervisor_metadata_bool(&group.metadata, AUTONOMY_GOAL_CLEARED).unwrap_or(false) {
        state.goals.remove(&session_id);
        return;
    }
    let Some(profile_id) = supervisor_metadata_str(&group.metadata, "profile_id") else {
        return;
    };
    let Some(goal_id) = supervisor_metadata_str(&group.metadata, "goal_id") else {
        return;
    };
    let goal = AutonomyGoalRecord {
        profile_id: profile_id.to_owned(),
        goal_id: goal_id.to_owned(),
        objective: supervisor_metadata_str(&group.metadata, "objective")
            .unwrap_or_default()
            .to_owned(),
        status: supervisor_metadata_str(&group.metadata, "status")
            .unwrap_or("paused")
            .to_owned(),
        token_budget: supervisor_metadata_u64(&group.metadata, "token_budget")
            .unwrap_or(GOAL_DEFAULT_TOKEN_BUDGET),
        tokens_used: supervisor_metadata_u64(&group.metadata, "tokens_used").unwrap_or(0),
        time_used_seconds: supervisor_metadata_u64(&group.metadata, "time_used_seconds")
            .unwrap_or(0),
        created_at_ms: supervisor_metadata_i64(&group.metadata, "created_at_ms")
            .unwrap_or(group.created_at_ms.try_into().unwrap_or(i64::MAX)),
        updated_at_ms: supervisor_metadata_i64(&group.metadata, "updated_at_ms")
            .unwrap_or(group.updated_at_ms.try_into().unwrap_or(i64::MAX)),
    };
    state.next_goal_seq = state.next_goal_seq.max(sequence_suffix(&goal.goal_id));
    state.goals.insert(session_id, goal);
}

fn restore_loop_from_group(state: &mut AutonomyRuntimeState, group: &SupervisedGroupRecord) {
    let Some(session_id) = supervisor_metadata_str(&group.metadata, "session_id") else {
        return;
    };
    let Some(profile_id) = supervisor_metadata_str(&group.metadata, "profile_id") else {
        return;
    };
    let Some(loop_id) = supervisor_metadata_str(&group.metadata, "loop_id") else {
        return;
    };
    let loop_record = AutonomyLoopRecord {
        loop_id: loop_id.to_owned(),
        session_id: SessionKey(session_id.to_owned()),
        profile_id: profile_id.to_owned(),
        prompt: supervisor_metadata_str(&group.metadata, "prompt")
            .unwrap_or_default()
            .to_owned(),
        mode: supervisor_metadata_str(&group.metadata, "mode")
            .unwrap_or("self_paced")
            .to_owned(),
        interval_seconds: supervisor_metadata_u64(&group.metadata, "interval_seconds"),
        status: supervisor_metadata_str(&group.metadata, "status")
            .unwrap_or("paused")
            .to_owned(),
        next_run_at_ms: supervisor_metadata_i64(&group.metadata, "next_run_at_ms"),
        last_run_at_ms: supervisor_metadata_i64(&group.metadata, "last_run_at_ms"),
        expires_at_ms: supervisor_metadata_i64(&group.metadata, "expires_at_ms")
            .unwrap_or(group.updated_at_ms.try_into().unwrap_or(i64::MAX)),
        created_at_ms: supervisor_metadata_i64(&group.metadata, "created_at_ms")
            .unwrap_or(group.created_at_ms.try_into().unwrap_or(i64::MAX)),
        updated_at_ms: supervisor_metadata_i64(&group.metadata, "updated_at_ms")
            .unwrap_or(group.updated_at_ms.try_into().unwrap_or(i64::MAX)),
    };
    state.next_loop_seq = state
        .next_loop_seq
        .max(sequence_suffix(&loop_record.loop_id));
    state.loops.insert(loop_record.loop_id.clone(), loop_record);
}

fn restore_agents_from_supervisor_state(
    state: &mut AutonomyRuntimeState,
    supervisor_state: &SupervisorState,
) {
    for child in supervisor_state.children.values() {
        let Some((session_id, profile_id)) =
            restored_agent_scope(child, supervisor_state.groups.get(&child.group_id))
        else {
            continue;
        };
        let artifacts = supervisor_state
            .artifacts
            .values()
            .filter(|artifact| {
                artifact.group_id == child.group_id
                    && artifact.child_id.as_deref() == Some(child.child_id.as_str())
            })
            .map(restored_agent_artifact)
            .collect::<Vec<_>>();
        let status = restored_agent_status(child);
        let updated_at_ms = child.updated_at_ms.try_into().unwrap_or(i64::MAX);
        let created_at_ms = child.started_at_ms.try_into().unwrap_or(i64::MAX);
        let agent = AutonomyAgentRecord {
            agent_id: child.child_id.clone(),
            parent_agent_id: supervisor_metadata_str(&child.metadata, "parent_agent_id")
                .map(str::to_owned),
            session_id,
            task_id: None,
            path: supervisor_metadata_str(&child.metadata, "path")
                .map(str::to_owned)
                .unwrap_or_else(|| format!("{}/{}", child.group_id, child.child_id)),
            role: supervisor_metadata_str(&child.metadata, "role")
                .unwrap_or("worker")
                .to_owned(),
            nickname: child
                .label
                .clone()
                .or_else(|| supervisor_metadata_str(&child.metadata, "nickname").map(str::to_owned))
                .unwrap_or_else(|| child.child_id.clone()),
            backend_kind: supervisor_metadata_str(&child.metadata, "backend_kind")
                .unwrap_or("restored")
                .to_owned(),
            status,
            last_task: restored_agent_last_task(child),
            cwd: child.workspace_path.clone(),
            profile_id,
            output: restored_agent_output(child),
            artifacts,
            created_at_ms,
            updated_at_ms,
            context_contract: None,
        };
        state.agents.insert(agent.agent_id.clone(), agent);
    }
}

fn restored_agent_scope(
    child: &ChildAgentRecord,
    group: Option<&SupervisedGroupRecord>,
) -> Option<(SessionKey, String)> {
    let session_id = supervisor_metadata_str(&child.metadata, "session_id")
        .or_else(|| group.and_then(|group| group.parent_session_id.as_deref()))?;
    let session_key = SessionKey(session_id.to_owned());
    let profile_id = child
        .profile_id
        .clone()
        .or_else(|| supervisor_metadata_str(&child.metadata, "profile_id").map(str::to_owned))
        .or_else(|| session_key.profile_id().map(str::to_owned))?;
    Some((session_key, profile_id))
}

fn restored_agent_status(child: &ChildAgentRecord) -> String {
    if let Some(terminal) = child.terminal.as_ref() {
        return match &terminal.kind {
            TerminalKind::Completed => "completed",
            TerminalKind::Failed => "failed",
            TerminalKind::Cancelled => "interrupted",
        }
        .to_owned();
    }
    match &child.status {
        ChildStatus::Starting | ChildStatus::Running => "running",
        ChildStatus::Completed => "completed",
        ChildStatus::Failed => "failed",
        ChildStatus::Cancelled => "interrupted",
    }
    .to_owned()
}

fn restored_agent_last_task(child: &ChildAgentRecord) -> Option<String> {
    child
        .terminal
        .as_ref()
        .and_then(|terminal| terminal.message.clone().or_else(|| terminal.reason.clone()))
        .or_else(|| child.task.clone())
}

fn restored_agent_output(child: &ChildAgentRecord) -> String {
    restored_agent_last_task(child)
        .map(|summary| format!("{summary}\n"))
        .unwrap_or_default()
}

fn restored_agent_artifact(artifact: &SupervisorArtifactRecord) -> AgentArtifactRecord {
    AgentArtifactRecord {
        id: artifact.artifact_id.clone(),
        title: artifact
            .display_name
            .clone()
            .unwrap_or_else(|| artifact.artifact_id.clone()),
        kind: artifact.kind.clone(),
        status: "ready".to_owned(),
        path: Some(artifact.path.clone()),
        content: None,
    }
}

fn persist_goal_state(
    state: &AutonomyRuntimeState,
    session_id: &SessionKey,
    goal: &AutonomyGoalRecord,
    cleared: bool,
) {
    persist_goal_state_with_store(state.supervisor_store.as_ref(), session_id, goal, cleared);
}

fn persist_goal_cleared(state: &AutonomyRuntimeState, session_id: &SessionKey, profile_id: &str) {
    let now = now_ms();
    let goal = AutonomyGoalRecord {
        profile_id: profile_id.to_owned(),
        goal_id: format!("cleared_{}", now.max(0)),
        objective: String::new(),
        status: "cleared".to_owned(),
        token_budget: GOAL_DEFAULT_TOKEN_BUDGET,
        tokens_used: 0,
        time_used_seconds: 0,
        created_at_ms: now,
        updated_at_ms: now,
    };
    persist_goal_state(state, session_id, &goal, true);
}

fn persist_goal_state_with_store(
    store: Option<&SupervisorStore>,
    session_id: &SessionKey,
    goal: &AutonomyGoalRecord,
    cleared: bool,
) {
    let Some(store) = store else {
        return;
    };
    let now = now_ms_u64();
    let mut group = SupervisedGroupRecord::new(autonomy_goal_group_id(session_id), now);
    group.parent_session_id = Some(session_id.to_string());
    group.objective = (!goal.objective.is_empty()).then(|| goal.objective.clone());
    group.status = if cleared {
        GroupStatus::Completed
    } else {
        GroupStatus::Running
    };
    group.updated_at_ms = now;
    group
        .metadata
        .insert(AUTONOMY_RECORD_KIND.into(), json!(AUTONOMY_RECORD_GOAL));
    group
        .metadata
        .insert(AUTONOMY_GOAL_CLEARED.into(), json!(cleared));
    group
        .metadata
        .insert("session_id".into(), json!(session_id.to_string()));
    group
        .metadata
        .insert("profile_id".into(), json!(goal.profile_id));
    group.metadata.insert("goal_id".into(), json!(goal.goal_id));
    group
        .metadata
        .insert("objective".into(), json!(goal.objective));
    group.metadata.insert("status".into(), json!(goal.status));
    group
        .metadata
        .insert("token_budget".into(), json!(goal.token_budget));
    group
        .metadata
        .insert("tokens_used".into(), json!(goal.tokens_used));
    group
        .metadata
        .insert("time_used_seconds".into(), json!(goal.time_used_seconds));
    group
        .metadata
        .insert("created_at_ms".into(), json!(goal.created_at_ms));
    group
        .metadata
        .insert("updated_at_ms".into(), json!(goal.updated_at_ms));
    let event_id = format!(
        "autonomy_goal_state:{}:{}",
        group.group_id,
        unique_event_suffix()
    );
    let _ = store.append_event(event_id, SupervisorEvent::GroupRegistered { group });
}

fn persist_loop_state(state: &AutonomyRuntimeState, loop_record: &AutonomyLoopRecord) {
    persist_loop_state_with_store(state.supervisor_store.as_ref(), loop_record);
}

fn persist_loop_state_with_store(
    store: Option<&SupervisorStore>,
    loop_record: &AutonomyLoopRecord,
) {
    let Some(store) = store else {
        return;
    };
    let now = now_ms_u64();
    let mut group = SupervisedGroupRecord::new(autonomy_loop_group_id(loop_record), now);
    group.parent_session_id = Some(loop_record.session_id.to_string());
    group.objective = Some(loop_record.prompt.clone());
    group.status = if loop_record.status == "deleted" {
        GroupStatus::Completed
    } else {
        GroupStatus::Running
    };
    group.updated_at_ms = now;
    group
        .metadata
        .insert(AUTONOMY_RECORD_KIND.into(), json!(AUTONOMY_RECORD_LOOP));
    group.metadata.insert(
        "session_id".into(),
        json!(loop_record.session_id.to_string()),
    );
    group
        .metadata
        .insert("profile_id".into(), json!(loop_record.profile_id));
    group
        .metadata
        .insert("loop_id".into(), json!(loop_record.loop_id));
    group
        .metadata
        .insert("prompt".into(), json!(loop_record.prompt));
    group
        .metadata
        .insert("mode".into(), json!(loop_record.mode));
    group.metadata.insert(
        "interval_seconds".into(),
        json!(loop_record.interval_seconds),
    );
    group
        .metadata
        .insert("status".into(), json!(loop_record.status));
    group
        .metadata
        .insert("next_run_at_ms".into(), json!(loop_record.next_run_at_ms));
    group
        .metadata
        .insert("last_run_at_ms".into(), json!(loop_record.last_run_at_ms));
    group
        .metadata
        .insert("expires_at_ms".into(), json!(loop_record.expires_at_ms));
    group
        .metadata
        .insert("created_at_ms".into(), json!(loop_record.created_at_ms));
    group
        .metadata
        .insert("updated_at_ms".into(), json!(loop_record.updated_at_ms));
    let event_id = format!(
        "autonomy_loop_state:{}:{}",
        group.group_id,
        unique_event_suffix()
    );
    let _ = store.append_event(event_id, SupervisorEvent::GroupRegistered { group });
}

fn autonomy_goal_group_id(session_id: &SessionKey) -> String {
    format!("autonomy-goal:{}", session_id)
}

fn autonomy_loop_group_id(loop_record: &AutonomyLoopRecord) -> String {
    format!(
        "autonomy-loop:{}:{}",
        loop_record.session_id, loop_record.loop_id
    )
}

fn sequence_suffix(id: &str) -> u64 {
    id.rsplit_once('_')
        .and_then(|(_, suffix)| suffix.parse::<u64>().ok())
        .unwrap_or(0)
}

fn unique_event_suffix() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

fn supervisor_metadata_str<'a>(metadata: &'a SupervisorMetadata, key: &str) -> Option<&'a str> {
    metadata.get(key).and_then(Value::as_str)
}

fn supervisor_metadata_i64(metadata: &SupervisorMetadata, key: &str) -> Option<i64> {
    metadata.get(key).and_then(Value::as_i64)
}

fn supervisor_metadata_u64(metadata: &SupervisorMetadata, key: &str) -> Option<u64> {
    metadata.get(key).and_then(Value::as_u64)
}

fn supervisor_metadata_bool(metadata: &SupervisorMetadata, key: &str) -> Option<bool> {
    metadata.get(key).and_then(Value::as_bool)
}

fn supervisor_metadata_for_agent(agent: &AutonomyAgentRecord) -> SupervisorMetadata {
    let mut metadata = SupervisorMetadata::new();
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

pub(crate) fn master_continuation_reason_name(reason: &MasterContinuationReason) -> &str {
    match reason {
        MasterContinuationReason::ChildCompleted => "child_completed",
        MasterContinuationReason::ScatterJoinComplete => "scatter_join_complete",
        MasterContinuationReason::LoopFire => "loop_fire",
        MasterContinuationReason::GoalContinue => "goal_continue",
        MasterContinuationReason::External(_) => "external",
    }
}

pub(crate) fn master_continuation_prompt(continuation: &QueuedMasterContinuation) -> String {
    let metadata = continuation
        .metadata
        .iter()
        .map(|(key, value)| format!("- {key}: {value}"))
        .collect::<Vec<_>>()
        .join("\n");
    let metadata = if metadata.is_empty() {
        "- none".to_owned()
    } else {
        metadata
    };
    match &continuation.reason {
        MasterContinuationReason::ChildCompleted => format!(
            "[system-internal]\nA supervised child agent finished.\n\nChild agent: {child}\nGroup: {group}\nMetadata:\n{metadata}\n\nGive the user a concise progress update. Mention what this child completed, whether follow-up work remains, and reference artifacts only when metadata or visible task state provides them.",
            child = continuation
                .child_agent_id
                .as_ref()
                .map(|id| id.as_str())
                .unwrap_or("unknown"),
            group = continuation.group_id.as_str(),
        ),
        MasterContinuationReason::ScatterJoinComplete => format!(
            "[system-internal]\nAll supervised child agents in this scatter-join group are terminal.\n\nGroup: {group}\nMetadata:\n{metadata}\n\nProduce the joined answer for the user. Summarize each child result, call out unresolved failures or missing artifacts, and state the next concrete action if one is required.",
            group = continuation.group_id.as_str(),
        ),
        MasterContinuationReason::LoopFire => format!(
            "[system-internal]\nA scheduled /loop continuation fired.\n\nLoop: {loop_id}\nMetadata:\n{metadata}\n\nExecute the loop prompt now. Keep the answer brief unless the loop prompt requires a full report.",
            loop_id = continuation
                .loop_id
                .as_ref()
                .map(|id| id.as_str())
                .unwrap_or("unknown"),
        ),
        MasterContinuationReason::GoalContinue => format!(
            "[system-internal]\nAn active goal continuation is ready.\n\nGoal: {goal_id}\nMetadata:\n{metadata}\n\nAdvance the goal by one bounded step. If the goal needs user input, ask a numbered choice question and recommend one option.",
            goal_id = continuation
                .goal_id
                .as_ref()
                .map(|id| id.as_str())
                .unwrap_or("unknown"),
        ),
        MasterContinuationReason::External(kind) => format!(
            "[system-internal]\nAn external master continuation was requested.\n\nKind: {kind}\nGroup: {group}\nMetadata:\n{metadata}\n\nHandle the continuation conservatively and summarize the visible state for the user.",
            group = continuation.group_id.as_str(),
        ),
    }
}

fn master_continuation_reason_wire_name(reason: &MasterContinuationReason) -> String {
    match reason {
        MasterContinuationReason::ChildCompleted => "child_completed".to_owned(),
        MasterContinuationReason::ScatterJoinComplete => "scatter_join_complete".to_owned(),
        MasterContinuationReason::LoopFire => "loop_fire".to_owned(),
        MasterContinuationReason::GoalContinue => "goal_continue".to_owned(),
        MasterContinuationReason::External(kind) => format!("external:{kind}"),
    }
}

fn master_continuation_reason_from_wire_name(value: &str) -> Option<MasterContinuationReason> {
    match value {
        "child_completed" | "ChildCompleted" => Some(MasterContinuationReason::ChildCompleted),
        "scatter_join_complete" | "ScatterJoinComplete" => {
            Some(MasterContinuationReason::ScatterJoinComplete)
        }
        "loop_fire" | "LoopFire" => Some(MasterContinuationReason::LoopFire),
        "goal_continue" | "GoalContinue" => Some(MasterContinuationReason::GoalContinue),
        value => value
            .strip_prefix("external:")
            .map(|kind| MasterContinuationReason::External(kind.to_owned())),
    }
}

struct AgentOutputWindow {
    start_offset: usize,
    end_offset: usize,
    text: String,
}

fn agent_output_window(
    text: &str,
    cursor: Option<&Value>,
    limit: Option<usize>,
    session_id: &SessionKey,
    profile_id: &str,
) -> Result<AgentOutputWindow, RpcError> {
    let start_offset = agent_output_cursor_offset(cursor, text, session_id, profile_id)?;
    let limit = limit.unwrap_or(usize::MAX);
    let mut end_offset = start_offset.saturating_add(limit).min(text.len());
    while end_offset > start_offset && !text.is_char_boundary(end_offset) {
        end_offset -= 1;
    }

    Ok(AgentOutputWindow {
        start_offset,
        end_offset,
        text: text[start_offset..end_offset].to_owned(),
    })
}

fn agent_output_cursor_offset(
    cursor: Option<&Value>,
    text: &str,
    session_id: &SessionKey,
    profile_id: &str,
) -> Result<usize, RpcError> {
    let Some(cursor) = cursor else {
        return Ok(0);
    };
    let Some(offset) = cursor.get("offset").and_then(Value::as_u64) else {
        return Err(agent_invalid_params_error(
            AGENT_OUTPUT_CURSOR_INVALID,
            "agent output cursor must be an object with numeric offset",
            Some(session_id),
            Some(profile_id),
            None,
        ));
    };
    let mut offset = usize::try_from(offset)
        .unwrap_or(usize::MAX)
        .min(text.len());
    while offset < text.len() && !text.is_char_boundary(offset) {
        offset += 1;
    }
    Ok(offset)
}

fn ensure_loop_scope(
    loop_record: &AutonomyLoopRecord,
    requested_session_id: Option<&SessionKey>,
    profile_id: &str,
) -> Result<(), RpcError> {
    if loop_record.profile_id != profile_id {
        return Err(autonomy_error(
            kinds::LOOP_POLICY_DENIED,
            "loop is outside the requested profile scope",
            requested_session_id.or(Some(&loop_record.session_id)),
            Some(profile_id),
            Some(("loop_id", loop_record.loop_id.as_str())),
            true,
        ));
    }
    if let Some(requested_session_id) = requested_session_id {
        if !session_controls_target(requested_session_id, &loop_record.session_id) {
            return Err(autonomy_error(
                kinds::LOOP_POLICY_DENIED,
                "loop is outside the requested session scope",
                Some(requested_session_id),
                Some(profile_id),
                Some(("loop_id", loop_record.loop_id.as_str())),
                true,
            ));
        }
    }
    Ok(())
}

fn autonomy_agent_json(agent: &AutonomyAgentRecord) -> Value {
    // #1021 / M17-C — surface `context_mode` / `context_refs` per child so AppUI clients can tell which dispatch context regime each specialist child is running under. `context_refs` is an array even though we only ever emit one ref today, so future managed-multiplex contracts (e.g. parent + sidecar) can extend it without a wire-format break.
    let context_mode = agent
        .context_contract
        .as_ref()
        .map(|contract| contract.mode.clone());
    let context_refs: Vec<String> = agent
        .context_contract
        .as_ref()
        .and_then(|contract| contract.context_ref.clone())
        .map(|context_ref| vec![context_ref])
        .unwrap_or_default();
    let context_contract = agent
        .context_contract
        .as_ref()
        .and_then(|contract| serde_json::to_value(contract).ok());
    json!({
        "agent_id": agent.agent_id,
        "parent_agent_id": agent.parent_agent_id,
        "session_id": agent.session_id,
        "task_id": agent.task_id.as_ref().map(ToString::to_string),
        "path": agent.path,
        "role": agent.role,
        "nickname": agent.nickname,
        "title": agent.nickname,
        "backend_kind": agent.backend_kind,
        "status": agent.status,
        "last_task": agent.last_task,
        "summary": agent.last_task,
        "output_tail": if agent.output.is_empty() {
            None
        } else {
            Some(agent.output.chars().rev().take(1200).collect::<Vec<_>>().into_iter().rev().collect::<String>())
        },
        "cwd": agent.cwd,
        "profile_id": agent.profile_id,
        "runtime_policy_stamp": {
            "profile_id": agent.profile_id,
            "sandbox": "workspace-write",
            "approval_policy": "on-request",
            "tool_policy_id": "coding-v1"
        },
        "artifact_count": agent.artifacts.len(),
        "artifacts": agent.artifacts.iter().map(agent_artifact_json).collect::<Vec<_>>(),
        "context_mode": context_mode,
        "context_refs": context_refs,
        "context_contract": context_contract,
        "created_at_ms": agent.created_at_ms,
        "updated_at_ms": agent.updated_at_ms,
    })
}

fn agent_artifact_json(artifact: &AgentArtifactRecord) -> Value {
    json!({
        "id": artifact.id,
        "title": artifact.title,
        "kind": artifact.kind,
        "status": artifact.status,
    })
}

/// #967 / M13-C — strip well-known credential patterns from an artifact
/// `content` payload before it is returned through `task/artifact/read`
/// or `agent/artifact/read`. The matching rules are intentionally a
/// conservative subset of the broader tool-output sanitizer in the
/// agent crate: only deterministic credential prefixes (api keys,
/// bearer tokens, AWS access keys, secret-assignment patterns). Base64
/// blobs / long hex strings are NOT redacted because legitimate artifact
/// payloads (e.g. validator-results.jsonl, captured diffs, log files)
/// regularly contain such substrings and stripping them would mangle
/// evidence.
///
/// Returns the input unchanged when no pattern matches.
fn redact_artifact_secrets(input: &str) -> std::borrow::Cow<'_, str> {
    use regex::Regex;
    use std::sync::LazyLock;

    /// Anthropic API keys (must run before the generic `sk-` pattern).
    static ANTHROPIC_KEY_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"sk-ant-[A-Za-z0-9_-]{20,}").unwrap());
    /// OpenAI-style `sk-` keys (catches OpenAI, OpenRouter, Together, ...).
    static OPENAI_KEY_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"sk-[A-Za-z0-9_-]{20,}").unwrap());
    /// AWS access key IDs.
    static AWS_KEY_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"AKIA[0-9A-Z]{16}").unwrap());
    /// GitHub PAT / OAuth / server / refresh / fine-grained PAT prefixes.
    static GITHUB_TOKEN_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?:ghp_|gho_|ghs_|ghr_|github_pat_)[A-Za-z0-9_]{20,}").unwrap()
    });
    /// GitLab personal access tokens.
    static GITLAB_TOKEN_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"glpat-[A-Za-z0-9_-]{20,}").unwrap());
    /// `Authorization: Bearer <token>` header values.
    static BEARER_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"Bearer\s+[A-Za-z0-9_.+/=-]{20,}").unwrap());
    /// Generic `password|secret|token|api_key = "..."` assignments.
    static SECRET_ASSIGN_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r#"(?i)(?:password|secret|api_key|apikey|access_token|auth_token|private_key)\s*[=:]\s*["']?[A-Za-z0-9_.+/=-]{8,}["']?"#,
        )
        .unwrap()
    });

    fn redact(text: &str) -> String {
        let prefix: String = text.chars().take(4).collect();
        format!("{}...[credential-redacted]", prefix)
    }

    let after_anth = ANTHROPIC_KEY_RE
        .replace_all(input, |caps: &regex::Captures<'_>| redact(&caps[0]))
        .into_owned();
    let after_openai = OPENAI_KEY_RE
        .replace_all(&after_anth, |caps: &regex::Captures<'_>| redact(&caps[0]))
        .into_owned();
    let after_aws = AWS_KEY_RE
        .replace_all(&after_openai, |caps: &regex::Captures<'_>| redact(&caps[0]))
        .into_owned();
    let after_gh = GITHUB_TOKEN_RE
        .replace_all(&after_aws, |caps: &regex::Captures<'_>| redact(&caps[0]))
        .into_owned();
    let after_gl = GITLAB_TOKEN_RE
        .replace_all(&after_gh, |caps: &regex::Captures<'_>| redact(&caps[0]))
        .into_owned();
    let after_bearer = BEARER_RE
        .replace_all(&after_gl, |caps: &regex::Captures<'_>| redact(&caps[0]))
        .into_owned();
    let after_assign = SECRET_ASSIGN_RE
        .replace_all(&after_bearer, |caps: &regex::Captures<'_>| redact(&caps[0]))
        .into_owned();
    if after_assign == input {
        std::borrow::Cow::Borrowed(input)
    } else {
        std::borrow::Cow::Owned(after_assign)
    }
}

fn emit_native_specialist_event(
    sender: &Option<NativeSpecialistEventSender>,
    method: &'static str,
    params: Value,
) {
    if let Some(sender) = sender {
        let _ = sender.send(NativeSpecialistAppUiEvent { method, params });
    }
}

fn native_specialist_agent_config() -> AgentConfig {
    AgentConfig {
        max_iterations: 20,
        suppress_auto_send_files: true,
        ..Default::default()
    }
}

fn native_specialist_artifacts<'a>(
    cwd: &Path,
    output: &str,
    files: impl Iterator<Item = &'a PathBuf>,
) -> Vec<AgentArtifactRecord> {
    let mut artifacts = Vec::new();
    if !output.trim().is_empty() {
        artifacts.push(AgentArtifactRecord {
            id: NATIVE_SPECIALIST_SUMMARY_ARTIFACT_ID.to_owned(),
            title: "Specialist summary".to_owned(),
            kind: "markdown".to_owned(),
            status: "ready".to_owned(),
            path: None,
            content: Some(output.to_owned()),
        });
    }

    let mut seen_paths = BTreeSet::new();
    for path in files {
        let resolved = if path.is_relative() {
            cwd.join(path)
        } else {
            path.clone()
        };
        let display_path = resolved.to_string_lossy().into_owned();
        if !seen_paths.insert(display_path.clone()) {
            continue;
        }
        let file_name = resolved
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("artifact")
            .to_owned();
        let artifact_id = sanitize_artifact_id(&file_name, artifacts.len() + 1);
        let (status, content) = read_small_text_artifact(&resolved);
        artifacts.push(AgentArtifactRecord {
            id: artifact_id,
            title: file_name,
            kind: artifact_kind(&resolved),
            status,
            path: Some(display_path),
            content,
        });
    }
    artifacts
}

fn sanitize_artifact_id(file_name: &str, fallback_index: usize) -> String {
    let id = file_name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if id.is_empty() {
        format!("artifact-{fallback_index}")
    } else {
        id
    }
}

fn artifact_kind(path: &Path) -> String {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("md" | "markdown") => "markdown",
        Some("json") => "json",
        Some("html" | "htm") => "html",
        Some("png" | "jpg" | "jpeg" | "gif" | "webp") => "image",
        Some("mp3" | "wav" | "m4a" | "ogg") => "audio",
        Some("mp4" | "mov" | "webm") => "video",
        _ => "file",
    }
    .to_owned()
}

fn read_small_text_artifact(path: &Path) -> (String, Option<String>) {
    let Ok(metadata) = std::fs::metadata(path) else {
        return ("missing".to_owned(), None);
    };
    if !metadata.is_file() || metadata.len() > NATIVE_SPECIALIST_ARTIFACT_CONTENT_MAX_BYTES as u64 {
        return ("ready".to_owned(), None);
    }
    match std::fs::read_to_string(path) {
        Ok(content) => ("ready".to_owned(), Some(content)),
        Err(_) => ("ready".to_owned(), None),
    }
}

fn autonomy_goal_json(goal: &AutonomyGoalRecord) -> Value {
    json!({
        "profile_id": goal.profile_id,
        "goal_id": goal.goal_id,
        "objective": goal.objective,
        "status": goal.status,
        "token_budget": goal.token_budget,
        "tokens_used": goal.tokens_used,
        "time_used_seconds": goal.time_used_seconds,
        "created_at_ms": goal.created_at_ms,
        "updated_at_ms": goal.updated_at_ms,
    })
}

fn autonomy_loop_json(loop_record: &AutonomyLoopRecord) -> Value {
    json!({
        "loop_id": loop_record.loop_id,
        "session_id": loop_record.session_id,
        "profile_id": loop_record.profile_id,
        "prompt": loop_record.prompt,
        "mode": loop_record.mode,
        "interval_seconds": loop_record.interval_seconds,
        "status": loop_record.status,
        "next_run_at_ms": loop_record.next_run_at_ms,
        "last_run_at_ms": loop_record.last_run_at_ms,
        "expires_at_ms": loop_record.expires_at_ms,
        "created_at_ms": loop_record.created_at_ms,
        "updated_at_ms": loop_record.updated_at_ms,
    })
}

fn master_continuation_enqueue_json(outcome: MasterContinuationEnqueueOutcome) -> Value {
    match outcome {
        MasterContinuationEnqueueOutcome::Queued(continuation) => json!({
            "queued": true,
            "duplicate": false,
            "continuation_id": continuation.id.as_u64(),
            "dedupe_key": continuation.dedupe_key.as_str(),
            "reason": format!("{:?}", continuation.reason),
            "priority": continuation.priority.rank(),
        }),
        MasterContinuationEnqueueOutcome::Duplicate {
            dedupe_key,
            existing_id,
        } => json!({
            "queued": true,
            "duplicate": true,
            "continuation_id": existing_id.as_u64(),
            "dedupe_key": dedupe_key.as_str(),
        }),
    }
}

fn nonempty(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_owned())
    })
}

fn parse_duration_seconds(token: &str) -> Option<u64> {
    let split_at = token
        .char_indices()
        .find(|(_, ch)| !ch.is_ascii_digit())
        .map(|(index, _)| index)?;
    if split_at == 0 {
        return None;
    }
    let (digits, unit) = token.split_at(split_at);
    if digits.is_empty() || unit.is_empty() || !digits.chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }
    let value = digits.parse::<u64>().ok()?;
    let multiplier = match unit {
        "s" | "sec" | "secs" | "second" | "seconds" => 1,
        "m" | "min" | "mins" | "minute" | "minutes" => 60,
        "h" | "hr" | "hrs" | "hour" | "hours" => 60 * 60,
        "d" | "day" | "days" => 24 * 60 * 60,
        _ => return None,
    };
    value.checked_mul(multiplier)
}

fn parse_loop_command_text(
    text: &str,
    session_id: &SessionKey,
    profile_id: &str,
) -> Result<(Option<String>, Option<u64>), RpcError> {
    let trimmed = text.trim();
    let Some(rest) = trimmed
        .strip_prefix("/loop ")
        .or_else(|| (trimmed == "/loop").then_some(""))
    else {
        return Ok((nonempty(Some(trimmed.to_owned())), None));
    };
    let tokens = rest.split_whitespace().collect::<Vec<_>>();
    if tokens.is_empty() {
        return Ok((None, None));
    }
    let leading_interval = parse_duration_seconds(tokens[0]);
    let trailing_interval = if tokens.len() >= 2 && tokens[tokens.len() - 2] == "every" {
        parse_duration_seconds(tokens[tokens.len() - 1])
    } else {
        None
    };
    if leading_interval.is_some() && trailing_interval.is_some() {
        return Err(autonomy_error(
            kinds::LOOP_INVALID_INTERVAL,
            "loop command may not contain both leading and trailing intervals",
            Some(session_id),
            Some(profile_id),
            None,
            true,
        ));
    }
    let start = usize::from(leading_interval.is_some());
    let end = if trailing_interval.is_some() {
        tokens.len().saturating_sub(2)
    } else {
        tokens.len()
    };
    let prompt = (start < end).then(|| tokens[start..end].join(" "));
    Ok((
        prompt.and_then(|prompt| nonempty(Some(prompt))),
        leading_interval.or(trailing_interval),
    ))
}

fn parse_loop_create(request: &LoopCreateRequest) -> Result<ParsedLoopCreate, RpcError> {
    let command_parse = match nonempty(request.command.clone()) {
        Some(command) => {
            parse_loop_command_text(&command, &request.session_id, &request.profile_id)?
        }
        None => (None, None),
    };
    if request.interval_seconds.is_some()
        && command_parse.1.is_some()
        && request.interval_seconds != command_parse.1
    {
        return Err(autonomy_error(
            kinds::LOOP_INVALID_INTERVAL,
            "loop interval was specified more than once",
            Some(&request.session_id),
            Some(&request.profile_id),
            None,
            true,
        ));
    }
    let interval_seconds = request.interval_seconds.or(command_parse.1);
    if let Some(interval_seconds) = interval_seconds {
        if !(LOOP_MIN_INTERVAL_SECONDS..=LOOP_MAX_INTERVAL_SECONDS).contains(&interval_seconds) {
            return Err(autonomy_error(
                kinds::LOOP_INVALID_INTERVAL,
                "loop interval is outside backend policy bounds",
                Some(&request.session_id),
                Some(&request.profile_id),
                None,
                true,
            ));
        }
    }

    let mut prompt = nonempty(request.prompt.clone())
        .or(command_parse.0)
        .unwrap_or_default();
    let mode = match nonempty(request.mode.clone()).as_deref() {
        Some("fixed_interval") => {
            if interval_seconds.is_none() {
                return Err(autonomy_error(
                    kinds::LOOP_INVALID_INTERVAL,
                    "fixed interval loop requires interval_seconds",
                    Some(&request.session_id),
                    Some(&request.profile_id),
                    None,
                    true,
                ));
            }
            "fixed_interval"
        }
        Some("self_paced") => "self_paced",
        Some("maintenance") => "maintenance",
        Some(_) => {
            return Err(autonomy_error(
                kinds::LOOP_POLICY_DENIED,
                "unsupported loop mode",
                Some(&request.session_id),
                Some(&request.profile_id),
                None,
                true,
            ));
        }
        None if interval_seconds.is_some() => "fixed_interval",
        None if prompt.is_empty() => "maintenance",
        None => "self_paced",
    }
    .to_owned();

    if mode == "fixed_interval" && prompt.is_empty() {
        return Err(autonomy_error(
            kinds::LOOP_PROMPT_EMPTY,
            "fixed interval loop requires a prompt",
            Some(&request.session_id),
            Some(&request.profile_id),
            None,
            true,
        ));
    }
    if mode == "self_paced" && prompt.is_empty() {
        return Err(autonomy_error(
            kinds::LOOP_PROMPT_EMPTY,
            "self-paced loop requires a prompt",
            Some(&request.session_id),
            Some(&request.profile_id),
            None,
            true,
        ));
    }
    if mode == "maintenance" && prompt.is_empty() {
        prompt = "run maintenance checks".to_owned();
    }
    if prompt.len() > MAX_LOOP_PROMPT_BYTES {
        return Err(autonomy_error(
            kinds::AUTONOMY_QUOTA_EXCEEDED,
            "loop prompt exceeds backend policy limit",
            Some(&request.session_id),
            Some(&request.profile_id),
            None,
            true,
        ));
    }

    Ok(ParsedLoopCreate {
        prompt,
        mode,
        interval_seconds,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NativeMockProvider {
        content: Result<String, String>,
    }

    #[async_trait::async_trait]
    impl LlmProvider for NativeMockProvider {
        async fn chat(
            &self,
            _messages: &[octos_core::Message],
            _tools: &[octos_llm::ToolSpec],
            _config: &octos_llm::ChatConfig,
        ) -> eyre::Result<octos_llm::ChatResponse> {
            match &self.content {
                Ok(content) => Ok(octos_llm::ChatResponse {
                    content: Some(content.clone()),
                    reasoning_content: None,
                    tool_calls: Vec::new(),
                    stop_reason: octos_llm::StopReason::EndTurn,
                    usage: octos_llm::TokenUsage {
                        input_tokens: 3,
                        output_tokens: 5,
                        ..Default::default()
                    },
                    provider_index: None,
                }),
                Err(error) => Err(eyre::eyre!(error.clone())),
            }
        }

        fn model_id(&self) -> &str {
            "native-mock"
        }

        fn provider_name(&self) -> &str {
            "test"
        }
    }

    fn sample_agent(agent_id: &str, profile_id: &str) -> AutonomyAgentRecord {
        AutonomyAgentRecord {
            agent_id: agent_id.to_owned(),
            parent_agent_id: None,
            session_id: SessionKey::with_profile(profile_id, "api", "agent-test"),
            task_id: None,
            path: format!("{profile_id}/{agent_id}"),
            role: "worker".into(),
            nickname: "worker".into(),
            backend_kind: "native".into(),
            status: "running".into(),
            last_task: Some("testing".into()),
            cwd: None,
            profile_id: profile_id.to_owned(),
            output: String::new(),
            artifacts: Vec::new(),
            created_at_ms: 1,
            updated_at_ms: 2,
            context_contract: None,
        }
    }

    #[tokio::test]
    async fn native_specialist_run_is_model_backed_and_emits_appui_events() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let orchestrator = InProcessAgentOrchestrator::default();
        let session_id = SessionKey::with_profile("tenant-a", "api", "native-specialist");
        let tools = Arc::new(ToolRegistry::with_builtins(dir.path()));
        let memory = Arc::new(
            EpisodeStore::open(dir.path().join("memory"))
                .await
                .expect("memory store"),
        );
        let llm: Arc<dyn LlmProvider> = Arc::new(NativeMockProvider {
            content: Ok("native specialist reviewed the policy".to_owned()),
        });
        let (tx, mut rx) = mpsc::unbounded_channel();

        let result = orchestrator
            .run_native_specialist(NativeSpecialistLaunchRequest {
                agent_id: Some("native-reviewer".to_owned()),
                parent_agent_id: Some("master".to_owned()),
                session_id: session_id.clone(),
                profile_id: "tenant-a".to_owned(),
                role: "reviewer".to_owned(),
                nickname: "Native Reviewer".to_owned(),
                task: "review policy validators".to_owned(),
                cwd: dir.path().to_path_buf(),
                llm,
                memory,
                tools: tools.clone(),
                system_prompt: Some("You are a focused reviewer.".to_owned()),
                agent_config: None,
                task_ledger_path: None,
                event_tx: Some(tx),
            })
            .await
            .expect("native specialist run");

        assert_eq!(result.agent_id, "native-reviewer");
        assert_eq!(result.status, "completed");
        assert!(result.task_id.is_some(), "native specialist is task-backed");
        assert_eq!(
            result.artifacts[0].id,
            NATIVE_SPECIALIST_SUMMARY_ARTIFACT_ID
        );

        let mut methods = Vec::new();
        while let Ok(event) = rx.try_recv() {
            methods.push(event.method);
        }
        assert_eq!(
            methods,
            vec![
                methods::AGENT_UPDATED,
                methods::AGENT_OUTPUT_DELTA,
                methods::AGENT_ARTIFACT_UPDATED,
                methods::AGENT_UPDATED,
            ]
        );

        let status = orchestrator
            .read_agent_status(AgentRequest {
                agent_id: "native-reviewer".to_owned(),
                session_id: Some(session_id.clone()),
                profile_id: "tenant-a".to_owned(),
            })
            .expect("agent status");
        let task_id = result.task_id.as_ref().expect("task id").to_string();
        assert_eq!(status["agent"]["backend_kind"], json!("native"));
        assert_eq!(status["agent"]["status"], json!("completed"));
        assert_eq!(status["agent"]["task_id"], json!(task_id.clone()));

        let output = orchestrator
            .read_agent_output(AgentOutputRequest {
                agent_id: "native-reviewer".to_owned(),
                session_id: Some(session_id.clone()),
                profile_id: "tenant-a".to_owned(),
                cursor: None,
                limit: None,
            })
            .expect("agent output");
        assert_eq!(
            output["text"],
            json!("native specialist reviewed the policy")
        );

        let artifact = orchestrator
            .read_agent_artifact(AgentArtifactReadRequest {
                agent_id: "native-reviewer".to_owned(),
                artifact_id: Some(NATIVE_SPECIALIST_SUMMARY_ARTIFACT_ID.to_owned()),
                path: None,
                session_id: Some(session_id),
                profile_id: "tenant-a".to_owned(),
            })
            .expect("summary artifact");
        assert_eq!(
            artifact["content"],
            json!("native specialist reviewed the policy")
        );

        let task = tools
            .supervisor()
            .get_task(&task_id)
            .expect("supervised task");
        assert_eq!(task.status, octos_agent::TaskStatus::Completed);
        assert_eq!(task.runtime_state, octos_agent::TaskRuntimeState::Completed);
    }

    #[tokio::test]
    async fn native_specialist_failure_marks_agent_and_task_failed() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let orchestrator = InProcessAgentOrchestrator::default();
        let session_id = SessionKey::with_profile("tenant-a", "api", "native-failed");
        let tools = Arc::new(ToolRegistry::with_builtins(dir.path()));
        let memory = Arc::new(
            EpisodeStore::open(dir.path().join("memory"))
                .await
                .expect("memory store"),
        );
        let llm: Arc<dyn LlmProvider> = Arc::new(NativeMockProvider {
            content: Err("provider unavailable".to_owned()),
        });

        let result = orchestrator
            .run_native_specialist(NativeSpecialistLaunchRequest {
                agent_id: Some("native-failure".to_owned()),
                parent_agent_id: Some("master".to_owned()),
                session_id: session_id.clone(),
                profile_id: "tenant-a".to_owned(),
                role: "reviewer".to_owned(),
                nickname: "Native Failure".to_owned(),
                task: "review policy validators".to_owned(),
                cwd: dir.path().to_path_buf(),
                llm,
                memory,
                tools: tools.clone(),
                system_prompt: None,
                agent_config: None,
                task_ledger_path: None,
                event_tx: None,
            })
            .await
            .expect("native specialist run");

        assert_eq!(result.status, "failed");
        let output = orchestrator
            .read_agent_output(AgentOutputRequest {
                agent_id: "native-failure".to_owned(),
                session_id: Some(session_id.clone()),
                profile_id: "tenant-a".to_owned(),
                cursor: None,
                limit: None,
            })
            .expect("agent output");
        assert!(
            output["text"]
                .as_str()
                .expect("output text")
                .contains("provider unavailable")
        );

        let status = orchestrator
            .read_agent_status(AgentRequest {
                agent_id: "native-failure".to_owned(),
                session_id: Some(session_id),
                profile_id: "tenant-a".to_owned(),
            })
            .expect("agent status");
        assert_eq!(status["agent"]["status"], json!("failed"));

        let task = tools
            .supervisor()
            .get_task(&result.task_id.unwrap().to_string())
            .expect("supervised task");
        assert_eq!(task.status, octos_agent::TaskStatus::Failed);
        assert_eq!(task.runtime_state, octos_agent::TaskRuntimeState::Failed);
    }

    #[test]
    fn output_and_artifact_list_are_backed_by_runtime_state() {
        let orchestrator = InProcessAgentOrchestrator::default();
        let mut agent = sample_agent("agent-1", MAIN_PROFILE_ID);
        let session_id = agent.session_id.clone();
        agent.output = "review output\n".into();
        agent.artifacts = vec![AgentArtifactRecord {
            id: "report".into(),
            title: "Report".into(),
            kind: "review_report".into(),
            status: "ready".into(),
            path: Some("report.md".into()),
            content: Some("# report\n".into()),
        }];
        orchestrator
            .state()
            .agents
            .insert(agent.agent_id.clone(), agent);

        let output = orchestrator
            .read_agent_output(AgentOutputRequest {
                agent_id: "agent-1".into(),
                session_id: Some(session_id.clone()),
                profile_id: MAIN_PROFILE_ID.into(),
                cursor: None,
                limit: None,
            })
            .expect("output response");
        assert_eq!(output["source"], json!("runtime"));
        assert_eq!(output["text"], json!("review output\n"));

        let artifacts = orchestrator
            .list_agent_artifacts(AgentRequest {
                agent_id: "agent-1".into(),
                session_id: Some(session_id),
                profile_id: MAIN_PROFILE_ID.into(),
            })
            .expect("artifact list response");
        assert_eq!(artifacts["artifacts"][0]["id"], json!("report"));
    }

    #[test]
    fn poisoned_state_lock_recovers_without_panicking_api_reads() {
        let orchestrator = InProcessAgentOrchestrator::default();
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = orchestrator.state();
            panic!("poison autonomy state for recovery coverage");
        }));
        assert!(poisoned.is_err());

        let result = orchestrator
            .list_agents(AgentListRequest {
                session_id: None,
                profile_id: MAIN_PROFILE_ID.into(),
                connection_profile_id: None,
            })
            .expect("poisoned state should be recovered");
        assert_eq!(result["agents"].as_array().expect("agents").len(), 0);
    }

    #[test]
    fn agent_output_reads_are_cursor_windowed_and_profile_scoped() {
        let orchestrator = InProcessAgentOrchestrator::default();
        let mut agent = sample_agent("agent-output", "tenant-a");
        let session_id = agent.session_id.clone();
        agent.output = "hello world".into();
        orchestrator
            .state()
            .agents
            .insert(agent.agent_id.clone(), agent);

        let window = orchestrator
            .read_agent_output(AgentOutputRequest {
                agent_id: "agent-output".into(),
                session_id: Some(session_id.clone()),
                profile_id: "tenant-a".into(),
                cursor: Some(json!({ "offset": 6 })),
                limit: Some(5),
            })
            .expect("windowed output");
        assert_eq!(window["text"], json!("world"));
        assert_eq!(window["cursor"]["offset"], json!(6));
        assert_eq!(window["next_cursor"]["offset"], json!(11));
        assert_eq!(window["has_more"], json!(false));

        let forbidden = orchestrator
            .read_agent_output(AgentOutputRequest {
                agent_id: "agent-output".into(),
                session_id: Some(session_id),
                profile_id: "tenant-b".into(),
                cursor: None,
                limit: None,
            })
            .expect_err("cross-profile output read must fail closed");
        assert_eq!(
            forbidden.data.expect("error data")["kind"],
            json!(kinds::AGENT_CONTROL_FORBIDDEN)
        );
    }

    #[test]
    fn artifact_read_requires_selector_before_lookup() {
        let orchestrator = InProcessAgentOrchestrator::default();
        let agent = sample_agent("agent-artifact", "tenant-a");
        let session_id = agent.session_id.clone();
        orchestrator
            .state()
            .agents
            .insert(agent.agent_id.clone(), agent);

        let err = orchestrator
            .read_agent_artifact(AgentArtifactReadRequest {
                agent_id: "agent-artifact".into(),
                artifact_id: None,
                path: None,
                session_id: Some(session_id),
                profile_id: "tenant-a".into(),
            })
            .expect_err("artifact selector is required");
        assert_eq!(
            err.data.expect("error data")["kind"],
            json!(AGENT_ARTIFACT_SELECTOR_INVALID)
        );
    }

    /// #967 / M13-C — task/artifact/list and task/artifact/read MUST
    /// deny cross-profile access. ensure_agent_control_scope already
    /// gates on agent.profile_id, but until now there was no explicit
    /// guard test for the artifact methods. Pins the property.
    #[test]
    fn task_artifact_list_and_read_deny_cross_profile_access() {
        let orchestrator = InProcessAgentOrchestrator::default();
        let mut agent = sample_agent("agent-ownership", "tenant-a");
        let session_id = agent.session_id.clone();
        agent.artifacts = vec![AgentArtifactRecord {
            id: "report".into(),
            title: "Report".into(),
            kind: "review_report".into(),
            status: "ready".into(),
            path: Some("report.md".into()),
            content: Some("secret".into()),
        }];
        orchestrator
            .state()
            .agents
            .insert(agent.agent_id.clone(), agent);

        // Cross-profile list: tenant-b cannot list tenant-a's agent.
        let forbidden_list = orchestrator
            .list_agent_artifacts(AgentRequest {
                agent_id: "agent-ownership".into(),
                session_id: Some(session_id.clone()),
                profile_id: "tenant-b".into(),
            })
            .expect_err("cross-profile artifact list must fail closed");
        assert_eq!(
            forbidden_list.data.expect("error data")["kind"],
            json!(kinds::AGENT_CONTROL_FORBIDDEN)
        );

        // Cross-profile read: tenant-b cannot read tenant-a's artifact.
        let forbidden_read = orchestrator
            .read_agent_artifact(AgentArtifactReadRequest {
                agent_id: "agent-ownership".into(),
                artifact_id: Some("report".into()),
                path: None,
                session_id: Some(session_id),
                profile_id: "tenant-b".into(),
            })
            .expect_err("cross-profile artifact read must fail closed");
        assert_eq!(
            forbidden_read.data.expect("error data")["kind"],
            json!(kinds::AGENT_CONTROL_FORBIDDEN)
        );
    }

    /// #967 / M13-C — task/artifact/* MUST deny requests whose
    /// session_id is unrelated (different base_key) from the agent's
    /// session, even when the profile_id matches. Prevents cross-
    /// session leakage within the same tenant.
    #[test]
    fn task_artifact_list_and_read_deny_unrelated_session_within_profile() {
        let orchestrator = InProcessAgentOrchestrator::default();
        let mut agent = sample_agent("agent-cross-session", "tenant-a");
        agent.artifacts = vec![AgentArtifactRecord {
            id: "report".into(),
            title: "Report".into(),
            kind: "review_report".into(),
            status: "ready".into(),
            path: Some("report.md".into()),
            content: Some("data".into()),
        }];
        orchestrator
            .state()
            .agents
            .insert(agent.agent_id.clone(), agent);

        // Unrelated session within the same profile — different base_key.
        let intruder_session = SessionKey::with_profile("tenant-a", "api", "intruder");
        let forbidden = orchestrator
            .list_agent_artifacts(AgentRequest {
                agent_id: "agent-cross-session".into(),
                session_id: Some(intruder_session.clone()),
                profile_id: "tenant-a".into(),
            })
            .expect_err("unrelated-session artifact list must fail closed");
        assert_eq!(
            forbidden.data.expect("error data")["kind"],
            json!(kinds::AGENT_CONTROL_FORBIDDEN)
        );

        let forbidden_read = orchestrator
            .read_agent_artifact(AgentArtifactReadRequest {
                agent_id: "agent-cross-session".into(),
                artifact_id: Some("report".into()),
                path: None,
                session_id: Some(intruder_session),
                profile_id: "tenant-a".into(),
            })
            .expect_err("unrelated-session artifact read must fail closed");
        assert_eq!(
            forbidden_read.data.expect("error data")["kind"],
            json!(kinds::AGENT_CONTROL_FORBIDDEN)
        );
    }

    /// #967 / M13-C — parent sessions whose `base_key` matches the
    /// child's session can list/read the child's artifacts. This
    /// pins the merge-join branch of `session_controls_target` so a
    /// regression to "strict equality only" doesn't silently break
    /// parent access to child artifacts.
    #[test]
    fn task_artifact_list_allows_parent_session_via_base_key() {
        let orchestrator = InProcessAgentOrchestrator::default();
        let mut agent = sample_agent("agent-child", "tenant-a");
        // Child session shares the parent's base_key (with a topic suffix).
        let parent_session = SessionKey::with_profile("tenant-a", "api", "parent");
        let child_session = SessionKey(format!("{}#child-1", parent_session.base_key()));
        agent.session_id = child_session.clone();
        agent.artifacts = vec![AgentArtifactRecord {
            id: "report".into(),
            title: "Report".into(),
            kind: "review_report".into(),
            status: "ready".into(),
            path: Some("report.md".into()),
            content: Some("ok".into()),
        }];
        orchestrator
            .state()
            .agents
            .insert(agent.agent_id.clone(), agent);

        // Parent reads the child's artifact list via base_key match.
        let listed = orchestrator
            .list_agent_artifacts(AgentRequest {
                agent_id: "agent-child".into(),
                session_id: Some(parent_session.clone()),
                profile_id: "tenant-a".into(),
            })
            .expect("parent must read child artifacts via base_key");
        assert_eq!(listed["artifacts"][0]["id"], json!("report"));

        let read = orchestrator
            .read_agent_artifact(AgentArtifactReadRequest {
                agent_id: "agent-child".into(),
                artifact_id: Some("report".into()),
                path: None,
                session_id: Some(parent_session),
                profile_id: "tenant-a".into(),
            })
            .expect("parent must read child artifact via base_key");
        assert_eq!(read["artifact"]["id"], json!("report"));
    }

    /// #967 / M13-C secret-redaction acceptance: artifact `content`
    /// returned through `read_agent_artifact` (and its `task/artifact/read`
    /// alias) must have well-known credential prefixes redacted so a
    /// child task that captured a provider key into its log/output cannot
    /// leak it to the parent session via the AppUI read RPC.
    #[test]
    fn read_agent_artifact_redacts_credential_patterns_from_content() {
        let orchestrator = InProcessAgentOrchestrator::default();
        let mut agent = sample_agent("agent-leak", "tenant-a");
        agent.artifacts = vec![AgentArtifactRecord {
            id: "trace".into(),
            title: "Run trace".into(),
            kind: "trace_log".into(),
            status: "ready".into(),
            path: Some("trace.log".into()),
            content: Some(
                concat!(
                    "step 1: GET https://api.example.com\n",
                    "Authorization: Bearer abcdef0123456789ABCDEF0123\n",
                    "OPENAI_API_KEY=sk-proj-aaaaaaaaaaaaaaaaaaaaaaaaaaaa\n",
                    "ANTHROPIC_API_KEY=sk-ant-aaaaaaaaaaaaaaaaaaaaaaaa\n",
                    "AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE\n",
                    "GITHUB_TOKEN=ghp_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n",
                    "step N: done",
                )
                .into(),
            ),
        }];
        let session_id = agent.session_id.clone();
        orchestrator
            .state()
            .agents
            .insert(agent.agent_id.clone(), agent);

        let read = orchestrator
            .read_agent_artifact(AgentArtifactReadRequest {
                agent_id: "agent-leak".into(),
                artifact_id: Some("trace".into()),
                path: None,
                session_id: Some(session_id),
                profile_id: "tenant-a".into(),
            })
            .expect("artifact read");
        let content = read["content"].as_str().expect("content present");
        // Structure preserved.
        assert!(content.starts_with("step 1: "));
        assert!(content.contains("step N: done"));
        // Every leaked credential pattern is redacted.
        for needle in [
            "sk-proj-aaaaaaaaaaaaaaaaaaaa",
            "sk-ant-aaaaaaaaaaaaaaaaaaaa",
            "AKIAIOSFODNN7EXAMPLE",
            "ghp_aaaaaaaaaaaaaaaaaaaaaaaa",
        ] {
            assert!(
                !content.contains(needle),
                "raw credential pattern {needle:?} leaked through artifact content"
            );
        }
        // The redaction marker shows up at least once per credential
        // family so the consumer can audit redaction count if needed.
        assert!(content.matches("[credential-redacted]").count() >= 4);
    }

    #[test]
    fn interrupt_and_close_enforce_terminal_transitions() {
        let orchestrator = InProcessAgentOrchestrator::default();
        let interrupt_agent = sample_agent("agent-interrupt", "tenant-a");
        let close_agent = sample_agent("agent-close", "tenant-a");
        let completed_agent = AutonomyAgentRecord {
            status: "completed".into(),
            ..sample_agent("agent-completed", "tenant-a")
        };
        let session_id = interrupt_agent.session_id.clone();
        orchestrator
            .state()
            .agents
            .insert(interrupt_agent.agent_id.clone(), interrupt_agent);
        orchestrator
            .state()
            .agents
            .insert(close_agent.agent_id.clone(), close_agent);
        orchestrator
            .state()
            .agents
            .insert(completed_agent.agent_id.clone(), completed_agent);

        let interrupted = orchestrator
            .interrupt_agent(AgentRequest {
                agent_id: "agent-interrupt".into(),
                session_id: Some(session_id.clone()),
                profile_id: "tenant-a".into(),
            })
            .expect("interrupt running agent");
        assert_eq!(interrupted["status"], json!("interrupted"));
        assert_eq!(interrupted["already_terminal"], json!(false));

        let repeated = orchestrator
            .interrupt_agent(AgentRequest {
                agent_id: "agent-interrupt".into(),
                session_id: Some(session_id.clone()),
                profile_id: "tenant-a".into(),
            })
            .expect("repeated same terminal control is idempotent");
        assert_eq!(repeated["already_terminal"], json!(true));

        let close_after_interrupt = orchestrator
            .close_agent(AgentRequest {
                agent_id: "agent-interrupt".into(),
                session_id: Some(session_id.clone()),
                profile_id: "tenant-a".into(),
            })
            .expect_err("terminal state cannot be changed");
        assert_eq!(
            close_after_interrupt.data.expect("error data")["kind"],
            json!(kinds::AGENT_CONTROL_UNAVAILABLE)
        );

        let completed_close = orchestrator
            .close_agent(AgentRequest {
                agent_id: "agent-completed".into(),
                session_id: Some(session_id.clone()),
                profile_id: "tenant-a".into(),
            })
            .expect_err("completed agent cannot be closed");
        assert_eq!(
            completed_close.data.expect("error data")["requested_status"],
            json!("closed")
        );

        let closed = orchestrator
            .close_agent(AgentRequest {
                agent_id: "agent-close".into(),
                session_id: Some(session_id.clone()),
                profile_id: "tenant-a".into(),
            })
            .expect("close running agent");
        assert_eq!(closed["status"], json!("closed"));

        let interrupt_after_close = orchestrator
            .interrupt_agent(AgentRequest {
                agent_id: "agent-close".into(),
                session_id: Some(session_id),
                profile_id: "tenant-a".into(),
            })
            .expect_err("closed agent cannot be interrupted");
        assert_eq!(
            interrupt_after_close.data.expect("error data")["current_status"],
            json!("closed")
        );
    }

    #[test]
    fn list_agents_uses_connection_profile_scope_value() {
        let orchestrator = InProcessAgentOrchestrator::default();
        let tenant_a_agent = sample_agent("agent-a", "tenant-a");
        let tenant_b_agent = sample_agent("agent-b", "tenant-b");
        orchestrator
            .state()
            .agents
            .insert(tenant_a_agent.agent_id.clone(), tenant_a_agent);
        orchestrator
            .state()
            .agents
            .insert(tenant_b_agent.agent_id.clone(), tenant_b_agent);

        let result = orchestrator
            .list_agents(AgentListRequest {
                session_id: None,
                profile_id: "tenant-a".into(),
                connection_profile_id: Some("tenant-b".into()),
            })
            .expect("agent list");
        assert_eq!(result["agents"].as_array().expect("agents").len(), 1);
        assert_eq!(result["agents"][0]["agent_id"], json!("agent-b"));
    }

    #[test]
    fn loop_listing_and_controls_respect_profile_and_deleted_state() {
        let orchestrator = InProcessAgentOrchestrator::default();
        let session_a = SessionKey::with_profile("tenant-a", "api", "loop-test");
        let session_b = SessionKey::with_profile("tenant-b", "api", "loop-test");
        let loop_a = orchestrator
            .create_loop(LoopCreateRequest {
                session_id: session_a.clone(),
                profile_id: "tenant-a".into(),
                prompt: Some("check a".into()),
                command: None,
                interval_seconds: None,
                mode: Some("self_paced".into()),
            })
            .expect("tenant a loop");
        let loop_b = orchestrator
            .create_loop(LoopCreateRequest {
                session_id: session_b,
                profile_id: "tenant-b".into(),
                prompt: Some("check b".into()),
                command: None,
                interval_seconds: None,
                mode: Some("self_paced".into()),
            })
            .expect("tenant b loop");

        let result = orchestrator
            .list_loops(LoopListRequest {
                session_id: None,
                profile_id: "tenant-a".into(),
            })
            .expect("tenant a list");
        assert_eq!(result["loops"].as_array().expect("loops").len(), 1);
        assert_eq!(result["loops"][0]["loop_id"], loop_a["loop_id"]);

        let loop_id_b = loop_b["loop_id"].as_str().expect("loop id").to_owned();
        let forbidden = orchestrator
            .control_loop(LoopControlRequest {
                loop_id: loop_id_b,
                session_id: Some(session_a.clone()),
                profile_id: "tenant-a".into(),
                kind: LoopControlKind::Pause,
            })
            .expect_err("cross-profile control must be rejected");
        assert_eq!(
            forbidden.data.expect("error data")["kind"],
            json!(kinds::LOOP_POLICY_DENIED)
        );

        let loop_id_a = loop_a["loop_id"].as_str().expect("loop id").to_owned();
        let deleted = orchestrator
            .control_loop(LoopControlRequest {
                loop_id: loop_id_a.clone(),
                session_id: Some(session_a.clone()),
                profile_id: "tenant-a".into(),
                kind: LoopControlKind::Delete,
            })
            .expect("delete");
        assert_eq!(deleted["loop"]["status"], json!("deleted"));
        let err = orchestrator
            .control_loop(LoopControlRequest {
                loop_id: loop_id_a,
                session_id: Some(session_a),
                profile_id: "tenant-a".into(),
                kind: LoopControlKind::Resume,
            })
            .expect_err("deleted loop cannot be resumed");
        assert_eq!(
            err.data.expect("error data")["kind"],
            json!(kinds::LOOP_NOT_FOUND)
        );
    }

    #[test]
    fn goals_preserve_omitted_fields_and_clear_checks_profile() {
        let orchestrator = InProcessAgentOrchestrator::default();
        let session_id = SessionKey::with_profile("tenant-a", "api", "goal-test");
        let created = orchestrator
            .set_goal(GoalSetRequest {
                session_id: session_id.clone(),
                profile_id: "tenant-a".into(),
                objective: "ship milestone".into(),
                status: Some("paused".into()),
                token_budget: Some(12_000),
                transition_actor: None,
            })
            .expect("create goal");
        assert_eq!(created["goal"]["status"], json!("paused"));
        assert_eq!(created["goal"]["token_budget"], json!(12_000));

        let updated = orchestrator
            .set_goal(GoalSetRequest {
                session_id: session_id.clone(),
                profile_id: "tenant-a".into(),
                objective: "ship milestone safely".into(),
                status: None,
                token_budget: None,
                transition_actor: None,
            })
            .expect("partial update");
        assert_eq!(updated["goal"]["status"], json!("paused"));
        assert_eq!(updated["goal"]["token_budget"], json!(12_000));

        let forbidden = orchestrator
            .clear_goal(GoalSessionRequest {
                session_id: session_id.clone(),
                profile_id: "tenant-b".into(),
            })
            .expect_err("cross-profile clear must fail");
        assert_eq!(
            forbidden.data.expect("error data")["kind"],
            json!(kinds::GOAL_UNAVAILABLE)
        );

        let cleared = orchestrator
            .clear_goal(GoalSessionRequest {
                session_id,
                profile_id: "tenant-a".into(),
            })
            .expect("scoped clear");
        assert_eq!(cleared["cleared"], json!(true));
    }

    #[test]
    fn terminal_child_status_queues_master_continuations() {
        let orchestrator = InProcessAgentOrchestrator::default();
        let mut child_a = sample_agent("child-a", "tenant-a");
        child_a.parent_agent_id = Some("master".into());
        let mut child_b = sample_agent("child-b", "tenant-a");
        child_b.parent_agent_id = Some("master".into());
        let session_id = child_a.session_id.clone();
        orchestrator
            .state()
            .agents
            .insert(child_a.agent_id.clone(), child_a);
        orchestrator
            .state()
            .agents
            .insert(child_b.agent_id.clone(), child_b);

        orchestrator
            .set_agent_status(
                "child-a",
                &session_id,
                "tenant-a",
                "completed",
                Some("api review done".into()),
            )
            .expect("complete first child");
        let first = orchestrator.drain_ready_continuations_for_session(
            &session_id,
            "tenant-a",
            MasterContinuationRuntimeState::idle(),
            usize::MAX,
        );
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].reason, MasterContinuationReason::ChildCompleted);

        orchestrator
            .set_agent_status(
                "child-b",
                &session_id,
                "tenant-a",
                "completed",
                Some("tests review done".into()),
            )
            .expect("complete second child");
        let second = orchestrator.drain_ready_continuations_for_session(
            &session_id,
            "tenant-a",
            MasterContinuationRuntimeState::idle(),
            usize::MAX,
        );
        let reasons = second
            .iter()
            .map(|item| item.reason.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            reasons,
            vec![
                MasterContinuationReason::ChildCompleted,
                MasterContinuationReason::ScatterJoinComplete
            ]
        );
    }

    #[test]
    fn background_task_mirror_uses_agent_orchestrator_and_queues_continuations() {
        let session_id = SessionKey::with_profile("tenant-a", "api", "background-task");
        let now = Utc::now();
        let task = octos_agent::BackgroundTask {
            id: "bg-1".into(),
            tool_name: "run_pipeline".into(),
            tool_call_id: "call-1".into(),
            parent_session_key: Some(session_id.to_string()),
            child_session_key: None,
            child_terminal_state: None,
            child_join_state: None,
            child_joined_at: None,
            child_failure_action: None,
            task_ledger_path: None,
            status: octos_agent::TaskStatus::Completed,
            runtime_state: octos_agent::TaskRuntimeState::Completed,
            runtime_detail: Some(
                json!({
                    "workflow_kind": "code_review",
                    "current_phase": "done",
                    "progress_message": "review pipeline completed"
                })
                .to_string(),
            ),
            started_at: now,
            updated_at: now,
            completed_at: Some(now),
            output_files: vec!["/tmp/octos-review/report.md".into()],
            error: None,
            session_key: Some(session_id.to_string()),
            tool_input: Some(json!({"task": "review"})),
            originating_client_message_id: None,
        };

        let (mirrored_session, agent) =
            upsert_background_task_agent(&task).expect("task should mirror");

        assert_eq!(mirrored_session, session_id);
        assert_eq!(agent["status"], json!("completed"));
        assert_eq!(agent["backend_kind"], json!("task_supervisor:run_pipeline"));
        assert_eq!(agent["artifact_count"], json!(1));
        assert_eq!(agent["summary"], json!("review pipeline completed"));

        let drained = default_agent_orchestrator().drain_ready_continuations_for_session(
            &session_id,
            "tenant-a",
            MasterContinuationRuntimeState::idle(),
            usize::MAX,
        );
        let reasons = drained
            .iter()
            .map(|item| item.reason.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            reasons,
            vec![
                MasterContinuationReason::ChildCompleted,
                MasterContinuationReason::ScatterJoinComplete
            ]
        );
    }

    #[test]
    fn repeated_terminal_agent_upsert_does_not_queue_duplicate_continuations() {
        let orchestrator = InProcessAgentOrchestrator::default();
        let session_id = SessionKey::with_profile("tenant-a", "api", "terminal-dedupe");
        let upsert = AgentUpsert {
            agent_id: "child-a".into(),
            parent_agent_id: Some("master".into()),
            session_id: session_id.clone(),
            task_id: None,
            path: "master/child-a".into(),
            role: "worker".into(),
            nickname: "Ada".into(),
            backend_kind: "native".into(),
            status: "completed".into(),
            last_task: Some("done".into()),
            cwd: None,
            profile_id: "tenant-a".into(),
        };

        orchestrator.upsert_agent(upsert.clone());
        orchestrator.upsert_agent(upsert);

        let drained = orchestrator.drain_ready_continuations_for_session(
            &session_id,
            "tenant-a",
            MasterContinuationRuntimeState::idle(),
            usize::MAX,
        );
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].reason, MasterContinuationReason::ChildCompleted);
        assert_eq!(
            drained[1].reason,
            MasterContinuationReason::ScatterJoinComplete
        );
    }

    #[test]
    fn continuation_drain_is_session_profile_scoped_and_idle_gated() {
        let orchestrator = InProcessAgentOrchestrator::default();
        let session_a = SessionKey::with_profile("tenant-a", "api", "scope-a");
        let session_b = SessionKey::with_profile("tenant-b", "api", "scope-b");
        orchestrator.upsert_agent(AgentUpsert {
            agent_id: "child-a".into(),
            parent_agent_id: Some("master".into()),
            session_id: session_a.clone(),
            task_id: None,
            path: "master/child-a".into(),
            role: "worker".into(),
            nickname: "Ada".into(),
            backend_kind: "native".into(),
            status: "completed".into(),
            last_task: Some("done a".into()),
            cwd: None,
            profile_id: "tenant-a".into(),
        });
        orchestrator.upsert_agent(AgentUpsert {
            agent_id: "child-b".into(),
            parent_agent_id: Some("master".into()),
            session_id: session_b.clone(),
            task_id: None,
            path: "master/child-b".into(),
            role: "worker".into(),
            nickname: "Hypatia".into(),
            backend_kind: "native".into(),
            status: "completed".into(),
            last_task: Some("done b".into()),
            cwd: None,
            profile_id: "tenant-b".into(),
        });

        let busy = orchestrator.drain_ready_continuations_for_session(
            &session_a,
            "tenant-a",
            MasterContinuationRuntimeState::busy(),
            usize::MAX,
        );
        assert!(busy.is_empty());
        assert_eq!(orchestrator.pending_continuation_count_for_test(), 4);

        let drained_a = orchestrator.drain_ready_continuations_for_session(
            &session_a,
            "tenant-a",
            MasterContinuationRuntimeState::idle(),
            usize::MAX,
        );
        assert_eq!(drained_a.len(), 2);
        assert!(
            drained_a
                .iter()
                .all(|item| item.profile_id.as_str() == "tenant-a")
        );
        assert_eq!(orchestrator.pending_continuation_count_for_test(), 2);
    }

    #[test]
    fn active_goal_queues_goal_continuation() {
        let orchestrator = InProcessAgentOrchestrator::default();
        let session_id = SessionKey::with_profile("tenant-a", "api", "goal-continue");
        orchestrator
            .set_goal(GoalSetRequest {
                session_id: session_id.clone(),
                profile_id: "tenant-a".into(),
                objective: "keep reviewing until clean".into(),
                status: Some("active".into()),
                token_budget: None,
                transition_actor: None,
            })
            .expect("set active goal");

        let drained = orchestrator.drain_ready_continuations_for_session(
            &session_id,
            "tenant-a",
            MasterContinuationRuntimeState::idle(),
            usize::MAX,
        );
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].reason, MasterContinuationReason::GoalContinue);
        assert_eq!(
            drained[0].metadata.get("objective").map(String::as_str),
            Some("keep reviewing until clean")
        );
    }

    #[test]
    fn due_fixed_interval_loop_queues_master_continuation() {
        let orchestrator = InProcessAgentOrchestrator::default();
        let session_id = SessionKey::with_profile("tenant-a", "api", "loop-due");
        let created = orchestrator
            .create_loop(LoopCreateRequest {
                session_id: session_id.clone(),
                profile_id: "tenant-a".into(),
                prompt: Some("check build health".into()),
                command: None,
                interval_seconds: Some(60),
                mode: Some("fixed_interval".into()),
            })
            .expect("create loop");
        let loop_id = created["loop_id"].as_str().expect("loop id").to_owned();
        {
            let mut state = orchestrator.state();
            let loop_record = state.loops.get_mut(&loop_id).expect("loop record");
            loop_record.next_run_at_ms = Some(now_ms() - 1);
        }

        let ticked = orchestrator.tick_due_loops_for_session(
            &session_id,
            "tenant-a",
            MasterContinuationRuntimeState::idle(),
        );
        assert_eq!(ticked, 1);
        assert_eq!(orchestrator.pending_continuation_count_for_test(), 1);

        let drained = orchestrator.drain_ready_continuations_for_session(
            &session_id,
            "tenant-a",
            MasterContinuationRuntimeState::idle(),
            usize::MAX,
        );
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].reason, MasterContinuationReason::LoopFire);
        assert_eq!(
            drained[0].loop_id.as_ref().map(|id| id.as_str()),
            Some(loop_id.as_str())
        );
        assert_eq!(
            drained[0].metadata.get("prompt").map(String::as_str),
            Some("check build health")
        );
    }

    #[test]
    fn busy_runtime_does_not_fire_due_loop() {
        let orchestrator = InProcessAgentOrchestrator::default();
        let session_id = SessionKey::with_profile("tenant-a", "api", "loop-busy");
        let created = orchestrator
            .create_loop(LoopCreateRequest {
                session_id: session_id.clone(),
                profile_id: "tenant-a".into(),
                prompt: Some("check busy loop".into()),
                command: None,
                interval_seconds: Some(60),
                mode: Some("fixed_interval".into()),
            })
            .expect("create loop");
        let loop_id = created["loop_id"].as_str().expect("loop id").to_owned();
        let due_at = now_ms() - 1;
        {
            let mut state = orchestrator.state();
            state
                .loops
                .get_mut(&loop_id)
                .expect("loop record")
                .next_run_at_ms = Some(due_at);
        }

        let drained = orchestrator.drain_ready_continuations_for_session(
            &session_id,
            "tenant-a",
            MasterContinuationRuntimeState::busy(),
            usize::MAX,
        );
        assert!(drained.is_empty());
        assert_eq!(orchestrator.pending_continuation_count_for_test(), 0);
        assert_eq!(
            orchestrator
                .state()
                .loops
                .get(&loop_id)
                .expect("loop record")
                .next_run_at_ms,
            Some(due_at)
        );
    }

    #[test]
    fn duplicate_due_loop_ticks_do_not_enqueue_duplicates() {
        let orchestrator = InProcessAgentOrchestrator::default();
        let session_id = SessionKey::with_profile("tenant-a", "api", "loop-dedupe");
        let created = orchestrator
            .create_loop(LoopCreateRequest {
                session_id: session_id.clone(),
                profile_id: "tenant-a".into(),
                prompt: Some("check dedupe".into()),
                command: None,
                interval_seconds: Some(60),
                mode: Some("fixed_interval".into()),
            })
            .expect("create loop");
        let loop_id = created["loop_id"].as_str().expect("loop id").to_owned();
        {
            let mut state = orchestrator.state();
            state
                .loops
                .get_mut(&loop_id)
                .expect("loop record")
                .next_run_at_ms = Some(now_ms() - 1);
        }

        assert_eq!(
            orchestrator.tick_due_loops_for_session(
                &session_id,
                "tenant-a",
                MasterContinuationRuntimeState::idle(),
            ),
            1
        );
        assert_eq!(
            orchestrator.tick_due_loops_for_session(
                &session_id,
                "tenant-a",
                MasterContinuationRuntimeState::idle(),
            ),
            0
        );
        assert_eq!(orchestrator.pending_continuation_count_for_test(), 1);
    }

    #[test]
    fn supervisor_store_restarts_restore_agents_and_artifacts() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let store_dir = dir.path().join("supervisor");
        let orchestrator = InProcessAgentOrchestrator::default();
        orchestrator
            .configure_supervisor_store(&store_dir)
            .expect("configure store");
        let session_id = SessionKey::with_profile("tenant-a", "api", "restore-agents");
        orchestrator.upsert_agent(AgentUpsert {
            agent_id: "child-restore".into(),
            parent_agent_id: Some("master".into()),
            session_id: session_id.clone(),
            task_id: None,
            path: "master/child-restore".into(),
            role: "reviewer".into(),
            nickname: "Curie".into(),
            backend_kind: "native".into(),
            status: "running".into(),
            last_task: Some("review auth module".into()),
            cwd: Some("/tmp/project".into()),
            profile_id: "tenant-a".into(),
        });
        orchestrator
            .set_agent_artifacts(
                "child-restore",
                &session_id,
                "tenant-a",
                vec![AgentArtifactRecord {
                    id: "review".into(),
                    title: "Review".into(),
                    kind: "markdown".into(),
                    status: "ready".into(),
                    path: Some("artifacts/review.md".into()),
                    content: Some("findings".into()),
                }],
            )
            .expect("persist artifact");

        let restarted = InProcessAgentOrchestrator::default();
        restarted
            .configure_supervisor_store(&store_dir)
            .expect("replay store");

        let status = restarted
            .read_agent_status(AgentRequest {
                agent_id: "child-restore".into(),
                session_id: Some(session_id.clone()),
                profile_id: "tenant-a".into(),
            })
            .expect("restored status");
        assert_eq!(status["agent"]["status"], json!("running"));
        assert_eq!(status["agent"]["nickname"], json!("Curie"));

        let artifacts = restarted
            .list_agent_artifacts(AgentRequest {
                agent_id: "child-restore".into(),
                session_id: Some(session_id),
                profile_id: "tenant-a".into(),
            })
            .expect("restored artifacts");
        assert_eq!(artifacts["artifacts"][0]["id"], json!("review"));
    }

    #[test]
    fn goal_and_loop_state_restore_from_supervisor_store() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let store_dir = dir.path().join("supervisor");
        let orchestrator = InProcessAgentOrchestrator::default();
        orchestrator
            .configure_supervisor_store(&store_dir)
            .expect("configure store");
        let session_id = SessionKey::with_profile("tenant-a", "api", "restore-goal-loop");
        orchestrator
            .set_goal(GoalSetRequest {
                session_id: session_id.clone(),
                profile_id: "tenant-a".into(),
                objective: "keep reviewing".into(),
                status: Some("paused".into()),
                token_budget: Some(42_000),
                transition_actor: None,
            })
            .expect("persist goal");
        let created_loop = orchestrator
            .create_loop(LoopCreateRequest {
                session_id: session_id.clone(),
                profile_id: "tenant-a".into(),
                prompt: Some("periodic review".into()),
                command: None,
                interval_seconds: Some(60),
                mode: Some("fixed_interval".into()),
            })
            .expect("persist loop");
        let loop_id = created_loop["loop_id"]
            .as_str()
            .expect("loop id")
            .to_owned();
        orchestrator
            .control_loop(LoopControlRequest {
                loop_id: loop_id.clone(),
                session_id: Some(session_id.clone()),
                profile_id: "tenant-a".into(),
                kind: LoopControlKind::Pause,
            })
            .expect("persist pause");

        let restarted = InProcessAgentOrchestrator::default();
        restarted
            .configure_supervisor_store(&store_dir)
            .expect("replay store");
        let goal = restarted
            .get_goal(GoalSessionRequest {
                session_id: session_id.clone(),
                profile_id: "tenant-a".into(),
            })
            .expect("restored goal");
        assert_eq!(goal["goal"]["objective"], json!("keep reviewing"));
        assert_eq!(goal["goal"]["token_budget"], json!(42_000));
        let loops = restarted
            .list_loops(LoopListRequest {
                session_id: Some(session_id.clone()),
                profile_id: "tenant-a".into(),
            })
            .expect("restored loops");
        assert_eq!(loops["loops"].as_array().expect("loops").len(), 1);
        assert_eq!(loops["loops"][0]["loop_id"], json!(loop_id));
        assert_eq!(loops["loops"][0]["status"], json!("paused"));

        restarted
            .clear_goal(GoalSessionRequest {
                session_id: session_id.clone(),
                profile_id: "tenant-a".into(),
            })
            .expect("clear persisted goal");
        let after_clear = InProcessAgentOrchestrator::default();
        after_clear
            .configure_supervisor_store(&store_dir)
            .expect("replay after clear");
        let goal = after_clear
            .get_goal(GoalSessionRequest {
                session_id,
                profile_id: "tenant-a".into(),
            })
            .expect("cleared goal");
        assert!(goal["goal"].is_null());
    }

    #[test]
    fn supervisor_store_replays_unfinished_continuations() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let store_dir = dir.path().join("supervisor");
        let orchestrator = InProcessAgentOrchestrator::default();
        orchestrator
            .configure_supervisor_store(&store_dir)
            .expect("configure store");
        let session_id = SessionKey::with_profile("tenant-a", "api", "durable");
        orchestrator.upsert_agent(AgentUpsert {
            agent_id: "child-a".into(),
            parent_agent_id: Some("master".into()),
            session_id: session_id.clone(),
            task_id: None,
            path: "master/child-a".into(),
            role: "worker".into(),
            nickname: "Ada".into(),
            backend_kind: "native".into(),
            status: "completed".into(),
            last_task: Some("durable review done".into()),
            cwd: None,
            profile_id: "tenant-a".into(),
        });
        assert_eq!(orchestrator.pending_continuation_count_for_test(), 2);

        let restarted = InProcessAgentOrchestrator::default();
        restarted
            .configure_supervisor_store(&store_dir)
            .expect("replay store");
        assert_eq!(restarted.pending_continuation_count_for_test(), 2);

        let drained = restarted.drain_ready_continuations_for_session(
            &session_id,
            "tenant-a",
            MasterContinuationRuntimeState::idle(),
            1,
        );
        assert_eq!(drained.len(), 1);
        restarted.mark_continuation_started(&drained[0]);
        restarted.mark_continuation_completed(&drained[0], Some("processed".into()));

        let replayed_after_completion = InProcessAgentOrchestrator::default();
        replayed_after_completion
            .configure_supervisor_store(&store_dir)
            .expect("replay store after completion");
        assert_eq!(
            replayed_after_completion.pending_continuation_count_for_test(),
            1
        );
    }
}
