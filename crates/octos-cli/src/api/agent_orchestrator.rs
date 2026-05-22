use std::collections::BTreeSet;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::goal_loop_runtime::{
    BUILT_IN_MAINTENANCE_PROMPT, DenyReason, GoalPolicyDecision, GoalRuntime, GoalRuntimePolicy,
    GoalRuntimeState, LoopFireContext, LoopFireDecision, LoopFireTrigger, LoopInvocation,
    LoopRuntime, LoopRuntimePolicy, MaintenancePromptResolution, MaintenancePromptSource,
    NextDueState, RuntimeIdleState as GoalRuntimeIdleState, SlashCommandAuthorization, WaitUntil,
    resolve_maintenance_prompt,
};
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
/// Default max fires for a single loop record before `LoopRuntime` flags
/// budget exhaustion. `AutonomyLoopRecord` already enforces 7-day expiry
/// and a per-session quota, so the per-loop budget is set generously and
/// is intentionally not user-tunable for the M15-D2 cut. (#977)
const LOOP_DEFAULT_MAX_FIRES: u32 = 10_000;
/// Default rescheduling delay when a self-paced loop fires without
/// emitting a `<<loop-next-in: …>>` hint. Caller can override via
/// `apply_self_paced_response` once richer config lands. (#977 bullet 4)
const SELF_PACED_DEFAULT_DELAY_SECONDS: u64 = 60 * 15;
const MAX_OBJECTIVE_BYTES: usize = 8_192;
const MAX_LOOP_PROMPT_BYTES: usize = 8_192;
const MAX_LOOPS_PER_SESSION: usize = 16;
const AGENT_OUTPUT_CURSOR_INVALID: &str = "agent_output_cursor_invalid";
const AGENT_ARTIFACT_SELECTOR_INVALID: &str = "agent_artifact_selector_invalid";
const AUTONOMY_RECORD_KIND: &str = "autonomy_record_kind";
const AUTONOMY_RECORD_GOAL: &str = "goal";
const AUTONOMY_RECORD_LOOP: &str = "loop";
const AUTONOMY_GOAL_CLEARED: &str = "goal_cleared";
/// #979 / M15-C2 — minimum spacing between two goal continuation turns
/// for the same goal. Stops a busy-loop where the model emits an
/// instant tool turn after each continuation and immediately requeues
/// itself. Tuned conservatively at 30s.
const GOAL_MIN_CONTINUATION_INTERVAL_MS: i64 = 30_000;
/// #979 / M15-C2 — sliding-window cap on goal continuation fires per
/// hour. Caps the worst-case spend if the model finds a stable
/// no-progress turn shape.
const GOAL_MAX_CONTINUATIONS_PER_HOUR: u32 = 12;
const GOAL_RATE_WINDOW_MS: i64 = 3_600_000;
/// #979 / M15-C2 — completion sentinels the model can emit at the
/// trailing edge of a goal turn to mark the goal `complete` without
/// requiring an out-of-band RPC. Matched case-insensitively after a
/// whitespace trim of the assistant content.
const GOAL_COMPLETE_SENTINELS: &[&str] = &[
    "<goal:complete>",
    "[goal:complete]",
    "goal-complete",
    "goal_complete",
];
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

/// #991 / M15-B — scope for `spawn_agent`. The trait keeps the request
/// surface narrow because the orchestrator-owned launcher is the source
/// of truth for backend kind, sandbox stamp, and policy stamp — the
/// caller only declares which child it wants and the task that child
/// should drive. Optional fields are accepted but always re-validated:
/// client-supplied `agent_id`, `parent_agent_id`, and policy stamps are
/// rejected or ignored as effective state per the M15-B acceptance
/// criteria. The default trait impl returns the
/// `method_not_supported` shape so wire-level callers can detect the
/// orchestrator-not-wired condition without panicking.
#[derive(Debug, Clone)]
#[allow(dead_code)] // wired into the JSON-RPC bridge in a follow-up PR (#991)
pub(crate) struct SpawnAgentRequest {
    pub(crate) session_id: SessionKey,
    pub(crate) profile_id: String,
    pub(crate) parent_agent_id: Option<String>,
    pub(crate) backend_kind: String,
    pub(crate) role: String,
    pub(crate) nickname: String,
    pub(crate) task: String,
    pub(crate) cwd: Option<String>,
}

/// #991 / M15-B — scope for `send_input` (push a user message into a
/// running child) and `wait_agent` (block until terminal). Keeping the
/// two requests identical right now avoids leaking transport details
/// (timeout, cursor) into the trait surface; M15-C will refine wait
/// semantics with streaming once a backend implements it.
#[derive(Debug, Clone)]
#[allow(dead_code)] // wired into the JSON-RPC bridge in a follow-up PR (#991)
pub(crate) struct AgentInputRequest {
    pub(crate) agent_id: String,
    pub(crate) session_id: Option<SessionKey>,
    pub(crate) profile_id: String,
    pub(crate) input: String,
}

/// #991 / M15-B — scope for `resume_agent` (re-attach to an existing
/// child by id). Resume is a read-mostly operation today: it returns
/// the agent record so the caller can re-wire its dispatch context
/// without a fresh `agent_list` round-trip.
#[derive(Debug, Clone)]
#[allow(dead_code)] // wired into the JSON-RPC bridge in a follow-up PR (#991)
pub(crate) struct ResumeAgentRequest {
    pub(crate) agent_id: String,
    pub(crate) session_id: Option<SessionKey>,
    pub(crate) profile_id: String,
}

#[allow(dead_code)] // spawn/send_input/wait/resume call sites land in the JSON-RPC bridge follow-up (#991)
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

    /// #991 / M15-B — kick off a new native/CLI/MCP child via the
    /// orchestrator-owned specialist runner. Default impl returns
    /// `method_not_supported` so existing in-process impls stay
    /// buildable; production implementations override this.
    fn spawn_agent(&self, request: SpawnAgentRequest) -> Result<Value, RpcError> {
        let _ = request;
        Err(method_not_supported_error(
            "agent/spawn",
            "spawn_agent",
            None,
            None,
        ))
    }

    /// #991 / M15-B — push a user input into a running child. Default
    /// impl returns `method_not_supported`; production implementations
    /// route to the supervised process / MCP transport.
    fn send_input(&self, request: AgentInputRequest) -> Result<Value, RpcError> {
        Err(method_not_supported_error(
            "agent/send_input",
            "send_input",
            request.session_id.as_ref(),
            Some(&request.profile_id),
        ))
    }

    /// #991 / M15-B — block on or stream the terminal transition of an
    /// agent. The default impl returns `method_not_supported`; in-
    /// process orchestrators can satisfy this synchronously by reading
    /// the current agent record when the agent is already terminal.
    fn wait_agent(&self, request: AgentRequest) -> Result<Value, RpcError> {
        Err(method_not_supported_error(
            "agent/wait",
            "wait_agent",
            request.session_id.as_ref(),
            Some(&request.profile_id),
        ))
    }

    /// #991 / M15-B — re-attach to an existing child by id. Default
    /// impl returns `method_not_supported`.
    fn resume_agent(&self, request: ResumeAgentRequest) -> Result<Value, RpcError> {
        Err(method_not_supported_error(
            "agent/resume",
            "resume_agent",
            request.session_id.as_ref(),
            Some(&request.profile_id),
        ))
    }
}

/// #991 / M15-B — uniform error shape for trait methods that have a
/// declared default impl but are not implemented on the current
/// orchestrator. Uses the spec §3 `UNSUPPORTED_CAPABILITY` slot so
/// AppUI clients can distinguish "method exists but not wired" from
/// the `METHOD_NOT_FOUND` JSON-RPC dispatch miss.
#[allow(dead_code)] // bridge consumer lands in the follow-up PR (#991)
pub(crate) fn method_not_supported_error(
    method: &str,
    capability: &str,
    session_id: Option<&SessionKey>,
    profile_id: Option<&str>,
) -> RpcError {
    let mut data = serde_json::Map::new();
    data.insert("kind".into(), json!("agent_method_not_supported"));
    data.insert("method".into(), json!(method));
    data.insert("capability".into(), json!(capability));
    data.insert("recoverable".into(), json!(false));
    if let Some(session_id) = session_id {
        data.insert("session_id".into(), json!(session_id));
    }
    if let Some(profile_id) = profile_id {
        data.insert("profile_id".into(), json!(profile_id));
    }
    RpcError::new(
        rpc_error_codes::UNSUPPORTED_CAPABILITY,
        format!("{method} is not implemented on this orchestrator"),
    )
    .with_data(Value::Object(data))
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
    pub(crate) dispatch_policy: Option<Arc<octos_agent::DispatchPolicy>>,
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
            dispatch_policy,
        } = request;

        let agent_id = agent_id.unwrap_or_else(|| format!("native-{}", uuid::Uuid::now_v7()));
        let path = format!(
            "{}/{}",
            parent_agent_id.as_deref().unwrap_or("master"),
            agent_id
        );
        if let Some(policy) = dispatch_policy.as_ref() {
            let backend = octos_agent::DispatchBackendMetadata::sandboxed(
                NATIVE_SPECIALIST_BACKEND_KIND,
                cwd.to_string_lossy().into_owned(),
            );
            let task_payload = json!({
                "task": task.as_str(),
                "cwd": cwd.to_string_lossy().into_owned(),
            });
            if let Err(denial) = octos_agent::enforce_dispatch_gates_for_backend(
                policy.as_ref(),
                &backend,
                octos_agent::DispatchTarget {
                    dispatch_id: &agent_id,
                    tool_name: NATIVE_SPECIALIST_BACKEND_KIND,
                    task: &task_payload,
                },
            )
            .await
            {
                return Err(autonomy_error(
                    kinds::AGENT_CONTROL_FORBIDDEN,
                    format!(
                        "dispatch rejected by policy ({}): {}",
                        denial.last_dispatch_outcome, denial.reason
                    ),
                    Some(&session_id),
                    Some(&profile_id),
                    Some(("agent_id", agent_id.as_str())),
                    true,
                ));
            }
        }
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

        // #1127 codex P2 follow-up to #991 / M15-B: arm the
        // cancellation handle BEFORE we publish the agent as `running`
        // and emit the AGENT_UPDATED event. A client that sees the
        // running event and immediately calls `interrupt_agent` /
        // `close_agent` must hit a registered token. With the prior
        // ordering (register after publish + after worker construction)
        // the notification was lost and the worker ran to completion
        // even though the agent's terminal status had been stamped.
        let cancel_token = self.register_agent_cancellation(&agent_id);

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

        // #991 / M15-B — `cancel_token` was registered above (see the
        // P2 follow-up comment) before the agent was published. The
        // `tokio::select!` below short-circuits `process_message` with
        // an `interrupted` status when a notify lands, instead of
        // letting the model finish.
        let run = worker.process_message(&task, &[], Vec::new());
        tokio::pin!(run);
        let cancel_wait = cancel_token.notified();
        tokio::pin!(cancel_wait);
        let result = tokio::select! {
            biased;
            _ = &mut cancel_wait => {
                Err(eyre::eyre!("native specialist cancelled"))
            }
            result = &mut run => result,
        };
        let cancelled = !self.state().cancellations.contains_key(&agent_id)
            && self.agent_status_is_terminal(&agent_id);
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
            Err(error) if cancelled => {
                let output = format!("Native specialist cancelled: {error}");
                ("interrupted".to_owned(), output, Vec::new())
            }
            Err(error) => {
                let output = format!("Native specialist failed: {error}");
                ("failed".to_owned(), output, Vec::new())
            }
        };
        // Clear the registered handle regardless of outcome — by the
        // time we reach this point the worker has stopped running, so
        // any subsequent `signal_agent_cancellation` would be a no-op.
        self.deregister_agent_cancellation(&agent_id);

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

    /// #991 / M15-B — register (or replace) the cancellation handle
    /// for `agent_id`. Returns the registered `Notify` so the worker
    /// can `notified()` on the same instance. Callers should drop
    /// their clone when they finish or transition the agent into a
    /// terminal state — the orchestrator clears the slot on
    /// `interrupt_agent` / `close_agent` after signalling.
    pub(crate) fn register_agent_cancellation(&self, agent_id: &str) -> Arc<tokio::sync::Notify> {
        let token = Arc::new(tokio::sync::Notify::new());
        self.state()
            .cancellations
            .insert(agent_id.to_owned(), token.clone());
        token
    }

    /// #991 / M15-B — drop the registered cancellation handle for
    /// `agent_id` (typically called by the runner once it has reached
    /// a terminal state and no longer wants to be wakeable). Safe to
    /// call when no handle is registered.
    pub(crate) fn deregister_agent_cancellation(&self, agent_id: &str) {
        self.state().cancellations.remove(agent_id);
    }

    /// #991 / M15-B — signal cancellation for the running agent task
    /// (if any) and drop the handle. Returns whether a handle was
    /// found. Used by `interrupt_agent` / `close_agent` to wake the
    /// worker after the in-memory terminal status has been stamped.
    ///
    /// #1127 codex P2 follow-up: use `notify_one()` instead of
    /// `notify_waiters()` so a notification that lands BEFORE the
    /// worker has had a chance to `.notified().await` is queued as
    /// a permit and consumed by the next await. With
    /// `notify_waiters()`, a fast interrupt that arrived in the
    /// window between agent publish (the `running` event) and the
    /// worker's first `notified()` await was silently lost.
    pub(crate) fn signal_agent_cancellation(&self, agent_id: &str) -> bool {
        let token = self.state().cancellations.remove(agent_id);
        if let Some(token) = token {
            token.notify_one();
            true
        } else {
            false
        }
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
        let now = now_ms();
        enqueue_due_loop_continuations(&mut state, session_id, profile_id, runtime_state, now);
        // #1129 codex P1 follow-up: active goals whose
        // `last_continued_at_ms + GOAL_MIN_CONTINUATION_INTERVAL_MS`
        // is past must also be re-queued here. Previously the only
        // goal enqueue happened immediately after `record_goal_turn`
        // (which had just stamped `last_continued_at_ms = now`,
        // tripping the min-delay gate), so an active goal only ran
        // its initial continuation and never recurred.
        enqueue_due_goal_continuations(&mut state, session_id, profile_id, runtime_state, now);
        // #1150 codex P2 follow-up to #1145: `pending_continuation_is_schedulable`
        // gates which sessions `due_loop_targets` surfaces, but the
        // scheduler's drain pops by `(session_key, profile)` without
        // re-applying the predicate. So a session correctly woken by
        // a fresh active continuation could drain an older stale
        // wrap-up first if both share the same `(session, profile)`
        // (lower sequence pops first under FIFO tie-break). Re-apply
        // the predicate here at the drain site and DROP unschedulable
        // items — do NOT re-enqueue. This matches `due_loop_targets`'s
        // silent-skip semantics for stale wrap-ups whose owning
        // entity has been paused/cleared/replaced.
        //
        // #1160 codex P3 follow-up to #1150/#1159: dropped stale items
        // already consumed a slot of the scheduler's `max_items`
        // budget, so a caller with `max_items=1` (production AppUI
        // tick loop) that finds a stale item at heap head returns an
        // empty vec — the fresh continuation queued behind it waits a
        // full tick (~30s) before draining. Refill from the scheduler
        // until `kept.len() == max_items` or the queue is empty for
        // this `(session, profile)`. The scheduler removes each popped
        // item from `pending_by_key` and pushes back any non-matching
        // heap entries, so repeated calls cannot revisit a previously
        // drained item.
        // Cap the initial allocation: callers (notably tests and
        // sweep paths) sometimes pass `usize::MAX` to mean
        // "everything", which would overflow `Vec::with_capacity`.
        let mut kept: Vec<QueuedMasterContinuation> = Vec::with_capacity(max_items.min(32));
        while kept.len() < max_items {
            let remaining = max_items - kept.len();
            let drained = state.continuations.drain_ready_for_session(
                runtime_state,
                remaining,
                &session_id.to_string(),
                profile_id,
            );
            if drained.is_empty() {
                break;
            }
            for item in drained {
                if pending_continuation_is_schedulable(&state, &item) {
                    kept.push(item);
                } else {
                    // #1159 codex P2 follow-up: only TOMBSTONE drops whose
                    // owning entity is genuinely gone (goal cleared and
                    // replaced, loop deleted), where the same dedupe_key
                    // cannot recur. For the *paused* subset (loop status
                    // != active, goal status != active but goal_id still
                    // matches), leave the supervisor ledger untouched —
                    // resuming the entity is expected to re-queue the
                    // same dedupe_key, and a Completed tombstone would
                    // make `upsert_continuation` silently drop the new
                    // Queued event because Completed outranks Queued.
                    if stale_drop_should_tombstone(&state, &item)
                        && let Some(store) = state.supervisor_store.as_ref()
                    {
                        let _ = store.record_continuation_completed(
                            item.group_id.as_str(),
                            item.dedupe_key.as_str(),
                            now_ms_u64(),
                            Some("discarded:stale_at_drain (#1150)".into()),
                        );
                    }
                    tracing::debug!(
                        session_key = %session_id.0,
                        profile_id = %profile_id,
                        reason = ?item.reason,
                        continuation_id = ?item.id,
                        goal_id = ?item.goal_id,
                        loop_id = ?item.loop_id,
                        "dropping stale continuation at drain site (#1150)"
                    );
                }
            }
        }
        kept
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
        let now_system = SystemTime::now();
        let mut targets = Vec::new();
        for loop_record in state.loops.values() {
            // #1128 codex P1 follow-up: `due_loop_targets` previously
            // skipped every loop whose mode was not `fixed_interval`,
            // which meant self-paced and maintenance loops with a
            // recorded `next_run_at_ms` (set by
            // `apply_self_paced_response` after a model
            // `<<loop-next-in: ...>>` reply) never fired again
            // automatically. The schedule cue for every active mode is
            // the same — `next_run_at_ms <= now` — so we drop the mode
            // filter here and let the per-mode fire-decision logic
            // handle slash re-auth / budget / wait policies downstream.
            if loop_record.status != "active"
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
        // #1129 codex P1 follow-up: include sessions whose active goal
        // is past the min-delay so the AppUI / session-actor scheduler
        // visits them too. The drain path
        // (`drain_ready_continuations_for_session`) is where the
        // actual goal-continuation enqueue happens; this scan only
        // tells the scheduler WHICH sessions need a visit. Without
        // this, sessions with a goal but no loop never tick again
        // after `set_goal`'s initial enqueue.
        if targets.len() < max_items {
            let idle_state = GoalRuntimeIdleState::idle();
            for (session_id, goal) in &state.goals {
                if profile_filter.is_some_and(|profile_id| goal.profile_id != profile_id) {
                    continue;
                }
                // #1140 codex P2 re-review #3: skip sessions whose
                // AppUI tick path has already dispatched a goal
                // continuation that hasn't reached post-accounting
                // yet. The `last_continued_at_ms` stamp alone is not
                // enough — for goal turns that run longer than
                // `GOAL_MIN_CONTINUATION_INTERVAL_MS` (30s), the
                // stamp expires before `record_goal_turn` re-stamps
                // it, opening a race where the scheduler tick can
                // re-dispatch in the await gap. The in-flight set is
                // cleared by `clear_goal_dispatch_in_flight` from the
                // post-accountant, so a session leaves the set
                // exactly when it's safe to re-dispatch.
                if state.in_flight_goal_sessions.contains(session_id) {
                    continue;
                }
                if !goal_policy_allows_fire(goal, idle_state, now_system, now) {
                    continue;
                }
                let target = (session_id.clone(), goal.profile_id.clone());
                if !targets.contains(&target) {
                    targets.push(target);
                    if targets.len() >= max_items {
                        break;
                    }
                }
            }
        }
        // #1141 — sweep the master continuation queue itself so any
        // session with a pending continuation (e.g. the wrap-up turn
        // enqueued by `record_goal_turn` when token_budget is
        // exhausted) gets a scheduler visit even if its owning goal
        // is no longer `active` (e.g. `budget_limited`) and it has no
        // active loop. Without this sweep the wrap-up remains queued
        // indefinitely for goal-only AppUI sessions because the
        // loop+goal scans above gate on active status.
        // #1145 codex P1 follow-up: filter the pending-queue sweep so
        // a paused/cleared goal or paused/deleted loop with a queued
        // continuation doesn't get woken by the scheduler. The
        // existing control paths (pause/clear/delete) don't cancel
        // queued items, so we filter here at scheduling time.
        if targets.len() < max_items {
            let mut seen_sessions: std::collections::HashSet<SessionKey> = targets
                .iter()
                .map(|(session_id, _)| session_id.clone())
                .collect();
            for item in state.continuations.pending_items() {
                if profile_filter.is_some_and(|profile_id| item.profile_id.as_str() != profile_id) {
                    continue;
                }
                if !pending_continuation_is_schedulable(&state, item) {
                    continue;
                }
                let session_key = SessionKey(item.session_id.as_str().to_owned());
                if seen_sessions.insert(session_key.clone()) {
                    targets.push((session_key, item.profile_id.as_str().to_owned()));
                    if targets.len() >= max_items {
                        break;
                    }
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

    /// #979 / M15-C2 — record an actual goal continuation turn as
    /// having fired. Bumps `continuations_used`, the sliding rate
    /// window, token and time counters, and — if this fires the
    /// token-budget exhaustion edge — enqueues the wrap-up turn and
    /// transitions the goal to `budget_limited`.
    /// #1129 codex P2 re-review #2 — dispatch-only timestamp update
    /// for the AppUI tick path. Only bumps `last_continued_at_ms` and
    /// the `updated_at_ms` field so the 30s min-delay gate fires
    /// immediately on dispatch. Does NOT touch `continuations_used`
    /// or the sliding rate-window counter — those are the
    /// caller-budget accountants and must only be incremented when a
    /// turn actually consumes tokens (which the AppUI path can't
    /// observe yet — see follow-up #1133).
    ///
    /// Returns true if the timestamp was updated, false if the goal
    /// was not found or the profile didn't match.
    /// #1129 codex P1 re-review #3 — count the dispatch toward the
    /// continuation budget + sliding-window cap so AppUI-backed
    /// active goals can't recur indefinitely. We deliberately do NOT
    /// bump `tokens_used` here — token spend is observed by the real
    /// LLM turn (only `SessionActor` records this today; AppUI parity
    /// is tracked in #1133). Counting dispatch against the
    /// continuation budget is the conservative interim: the 12/hr
    /// hard cap fires correctly, and the derived continuation budget
    /// (`token_budget / 2500`) bounds total fires until token-side
    /// accounting catches up.
    ///
    /// #1140 codex P2 follow-up — dispatch-time stamp that ONLY
    /// touches `last_continued_at_ms` (and `updated_at_ms`), with NO
    /// counter increments. Used by the AppUI tick path before a goal
    /// turn starts so `due_loop_targets` doesn't keep seeing the
    /// same goal as due every 2s while the turn is in flight. The
    /// post-turn `record_goal_turn` then handles the full counter
    /// + token accounting once `run_standalone_turn` returns.
    ///
    /// Returns true if the timestamp was updated, false if the goal
    /// is not found or the profile didn't match.
    pub(crate) fn record_goal_dispatch_timestamp_only(
        &self,
        session_id: &SessionKey,
        profile_id: &str,
    ) -> bool {
        let now = now_ms();
        let mut state = self.state();
        let Some(goal) = state.goals.get_mut(session_id) else {
            return false;
        };
        if goal.profile_id != profile_id {
            return false;
        }
        goal.last_continued_at_ms = now;
        goal.updated_at_ms = now;
        let snapshot = goal.clone();
        persist_goal_state(&state, session_id, &snapshot, false);
        true
    }

    /// #1140 codex P2 re-review #3 — mark a session as having an
    /// in-flight goal dispatch. `due_loop_targets`'s goal sweep skips
    /// in-flight sessions so a long-running goal turn (> 30s) can't
    /// be re-dispatched in the await gap between turn-terminal
    /// emission and `record_goal_turn`. Idempotent.
    pub(crate) fn mark_goal_dispatch_in_flight(&self, session_id: &SessionKey) {
        self.state()
            .in_flight_goal_sessions
            .insert(session_id.clone());
    }

    /// #1140 codex P2 re-review #3 — clear the in-flight marker.
    /// Called by the post-turn accountant after `record_goal_turn`
    /// (and on error/interrupt paths) so subsequent scheduler ticks
    /// can re-dispatch the goal once the min-delay elapses.
    pub(crate) fn clear_goal_dispatch_in_flight(&self, session_id: &SessionKey) {
        self.state().in_flight_goal_sessions.remove(session_id);
    }

    /// #1140 codex P1 re-review #4 — RAII drop-guard for the
    /// in-flight marker. Use this from the AppUI tick path so the
    /// marker is cleared on ANY exit path (cancellation,
    /// early-terminal-error, panic), not just the happy
    /// post-accounting path. The guard captures a 'static reference
    /// to the orchestrator singleton, so it's safe to move across
    /// await points / into spawned tasks.
    pub(crate) fn goal_dispatch_in_flight_guard(
        &'static self,
        session_id: SessionKey,
    ) -> GoalDispatchInFlightGuard {
        self.mark_goal_dispatch_in_flight(&session_id);
        GoalDispatchInFlightGuard {
            orchestrator: self,
            session_id,
            disarmed: false,
        }
    }

    /// #1133 — the AppUI tick path no longer calls this helper.
    /// `run_standalone_turn` now folds real `tokens_consumed +
    /// elapsed` into `record_goal_turn` AFTER the agent task returns,
    /// which is the single accountant that bumps every counter
    /// (`continuations_used`, `rate_window_count`, `tokens_used`,
    /// `last_continued_at_ms`). The helper is preserved for any
    /// future caller that genuinely only needs a timestamp bump (e.g.
    /// a session actor whose tokens aren't known immediately) — the
    /// `#[allow(dead_code)]` reflects "kept by design", not "stale".
    #[allow(dead_code)]
    pub(crate) fn record_goal_dispatch_only(
        &self,
        session_id: &SessionKey,
        profile_id: &str,
    ) -> bool {
        let now = now_ms();
        let mut state = self.state();
        let Some(goal) = state.goals.get_mut(session_id) else {
            return false;
        };
        if goal.profile_id != profile_id {
            return false;
        }
        goal.last_continued_at_ms = now;
        goal.continuations_used = goal.continuations_used.saturating_add(1);
        if now.saturating_sub(goal.rate_window_start_ms) >= GOAL_RATE_WINDOW_MS {
            goal.rate_window_start_ms = now;
            goal.rate_window_count = 1;
        } else {
            goal.rate_window_count = goal.rate_window_count.saturating_add(1);
        }
        goal.updated_at_ms = now;
        let snapshot = goal.clone();
        persist_goal_state(&state, session_id, &snapshot, false);
        true
    }

    pub(crate) fn record_goal_turn(
        &self,
        session_id: &SessionKey,
        profile_id: &str,
        tokens_consumed: u64,
        elapsed_seconds: u64,
    ) {
        let now = now_ms();
        let now_system = SystemTime::now();
        let mut state = self.state();
        let Some(goal) = state.goals.get_mut(session_id) else {
            return;
        };
        if goal.profile_id != profile_id {
            return;
        }
        let goal_id = goal.goal_id.clone();
        let wrap_up = record_goal_turn_internal(goal, tokens_consumed, elapsed_seconds, now);
        let goal_snapshot = goal.clone();
        persist_goal_state(&state, session_id, &goal_snapshot, false);
        if let Some(prompt) = wrap_up {
            // #1131 — Enqueue a one-shot wrap-up turn under the
            // dedicated `GoalWrapUp` reason so the prompt renderer
            // emits the wrap-up directive verbatim instead of the
            // standard "Advance the goal..." template. Use an
            // explicit dedupe key so the wrap-up cannot collide with
            // the normal-continuation key shape.
            let mut wrap_up_request = MasterContinuationRequest::new(
                "coding-autonomy-goal",
                session_id.to_string(),
                profile_id.to_owned(),
                MasterContinuationReason::GoalWrapUp,
                now_system,
            )
            .with_goal_id(goal_id.clone())
            .with_metadata("objective", goal_snapshot.objective.clone())
            .with_metadata("status", "budget_limited".to_owned())
            .with_metadata("wrap_up", "true".to_owned())
            .with_metadata("wrap_up_prompt", prompt);
            wrap_up_request = wrap_up_request.with_dedupe_key(format!(
                "coding-autonomy-goal/wrap_up/{}/{}",
                profile_id, goal_id
            ));
            enqueue_and_persist_continuation(&mut state, wrap_up_request);
        }
    }

    /// #979 / M15-C2 — after a goal-driven turn finishes, re-queue
    /// another continuation only if the runtime is idle AND the
    /// per-goal policy still allows another fire. This is the
    /// recurring path that keeps an active goal alive without
    /// burst-firing or busy-looping.
    pub(crate) fn maybe_enqueue_goal_after_turn(
        &self,
        session_id: &SessionKey,
        profile_id: &str,
        idle_state: GoalRuntimeIdleState,
    ) -> bool {
        let mut state = self.state();
        let Some(goal) = state.goals.get(session_id).cloned() else {
            return false;
        };
        if goal.profile_id != profile_id {
            return false;
        }
        enqueue_goal_continuation_with_idle(&mut state, session_id, profile_id, &goal, idle_state)
            .map(|outcome| matches!(outcome, MasterContinuationEnqueueOutcome::Queued(_)))
            .unwrap_or(false)
    }

    /// #979 / M15-C2 — flip the goal to `complete` when the model
    /// emits a known completion sentinel during a goal turn.
    pub(crate) fn maybe_complete_goal_from_model(
        &self,
        session_id: &SessionKey,
        profile_id: &str,
        assistant_content: &str,
    ) -> bool {
        if !detect_goal_complete_sentinel(assistant_content) {
            return false;
        }
        let mut state = self.state();
        let Some(goal) = state.goals.get_mut(session_id) else {
            return false;
        };
        if goal.profile_id != profile_id {
            return false;
        }
        if goal.status == "complete" {
            return false;
        }
        goal.status = "complete".to_owned();
        goal.updated_at_ms = now_ms();
        let snapshot = goal.clone();
        persist_goal_state(&state, session_id, &snapshot, false);
        true
    }

    #[cfg(test)]
    pub(crate) fn force_goal_tokens_used_for_test(
        &self,
        session_id: &SessionKey,
        tokens_used: u64,
    ) {
        if let Some(goal) = self.state().goals.get_mut(session_id) {
            goal.tokens_used = tokens_used;
        }
    }

    #[cfg(test)]
    pub(crate) fn goal_status_for_test(&self, session_id: &SessionKey) -> Option<String> {
        self.state()
            .goals
            .get(session_id)
            .map(|goal| goal.status.clone())
    }

    /// #1133 — accessor used by the AppUI goal-turn acceptance tests to
    /// pin that `record_goal_turn` actually bumped `tokens_used` /
    /// `continuations_used` after a turn completed.
    #[cfg(test)]
    pub(crate) fn goal_counters_for_test(
        &self,
        session_id: &SessionKey,
    ) -> Option<(u64, u32, u32)> {
        self.state().goals.get(session_id).map(|goal| {
            (
                goal.tokens_used,
                goal.continuations_used,
                goal.rate_window_count,
            )
        })
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

    /// #977 Bullet 4 — self-paced "model selects next delay".
    ///
    /// After a self-paced loop fires, the session actor passes the
    /// model's response back through this entry point. The parser
    /// extracts the `<<loop-next-in: …>>` sentinel and reschedules the
    /// loop's `next_run_at_ms`. Returns the applied delay so callers can
    /// log / surface it; returns `Ok(None)` when the sentinel is
    /// absent — the caller decides whether to apply
    /// [`SELF_PACED_DEFAULT_DELAY_SECONDS`] or to wait for an explicit
    /// fire_now.
    pub(crate) fn apply_self_paced_response(
        &self,
        loop_id: &str,
        profile_id: &str,
        response: &str,
    ) -> Result<Option<Duration>, RpcError> {
        let mut state = self.state();
        let supervisor_store = state.supervisor_store.clone();
        let Some(loop_record) = state.loops.get_mut(loop_id) else {
            return Err(autonomy_error(
                kinds::LOOP_NOT_FOUND,
                "loop not found",
                None,
                Some(profile_id),
                Some(("loop_id", loop_id)),
                true,
            ));
        };
        if loop_record.profile_id != profile_id {
            return Err(autonomy_error(
                kinds::LOOP_POLICY_DENIED,
                "loop is outside the requested profile scope",
                Some(&loop_record.session_id),
                Some(profile_id),
                Some(("loop_id", loop_id)),
                true,
            ));
        }
        if loop_record.mode != "self_paced" && loop_record.mode != "maintenance" {
            return Ok(None);
        }
        let parsed = parse_self_paced_next_delay(response);
        let delay = parsed.unwrap_or_else(|| Duration::from_secs(SELF_PACED_DEFAULT_DELAY_SECONDS));
        let now = now_ms();
        let delay_ms = i64::try_from(delay.as_millis().min(i64::MAX as u128))
            .unwrap_or(LOOP_MAX_INTERVAL_SECONDS as i64 * 1_000);
        loop_record.next_run_at_ms = now.checked_add(delay_ms);
        loop_record.updated_at_ms = now;
        persist_loop_state_with_store(supervisor_store.as_ref(), loop_record);
        Ok(parsed)
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
        // #1127 codex P1 follow-up to #991 / M15-B: validate AND stamp
        // the terminal state BEFORE we wake the worker. The prior shape
        // signaled first, which (a) let any same-profile caller wake +
        // remove another session's cancellation token even when the
        // RPC would later return forbidden, and (b) on multithreaded
        // runtimes let an authorized interrupt wake the worker before
        // the status flip became visible — so workers raced through
        // their wrap-up code and reported `failed` instead of
        // `interrupted`/`closed`. `update_agent_terminal_status` does
        // the scope check + stamp under the same state lock, so we
        // only signal after a successful stamp.
        let agent_id = request.agent_id.clone();
        let result = update_agent_terminal_status(self, request, "interrupted", true, false)?;
        self.signal_agent_cancellation(&agent_id);
        Ok(result)
    }

    fn close_agent(&self, request: AgentRequest) -> Result<Value, RpcError> {
        // #1127 codex P1 follow-up to #991 / M15-B: validate + stamp,
        // then signal — see `interrupt_agent` for the rationale.
        let agent_id = request.agent_id.clone();
        let result = update_agent_terminal_status(self, request, "closed", false, true)?;
        self.signal_agent_cancellation(&agent_id);
        Ok(result)
    }

    /// #991 / M15-B — in-process `spawn_agent` registers a *pending*
    /// agent record so subsequent `agent_list` / `agent_status` calls
    /// observe the new child immediately. The actual model / CLI /
    /// MCP work is driven by the caller (typically the session
    /// runtime factory or the specialist runner) which retrieves the
    /// registered cancellation handle when it begins running. This
    /// keeps the trait surface synchronous (matches the rest of the
    /// orchestrator API) while still letting backend implementations
    /// satisfy the spawn contract — they pre-register the record, then
    /// run the work in a follow-up tokio task.
    fn spawn_agent(&self, request: SpawnAgentRequest) -> Result<Value, RpcError> {
        let backend_kind = request.backend_kind.trim();
        if backend_kind.is_empty() {
            return Err(autonomy_error(
                kinds::AGENT_CONTROL_UNAVAILABLE,
                "spawn_agent requires a non-empty backend_kind",
                Some(&request.session_id),
                Some(&request.profile_id),
                None,
                true,
            ));
        }
        let role = request.role.trim();
        let nickname = request.nickname.trim();
        if role.is_empty() || nickname.is_empty() {
            return Err(autonomy_error(
                kinds::AGENT_CONTROL_UNAVAILABLE,
                "spawn_agent requires non-empty role and nickname",
                Some(&request.session_id),
                Some(&request.profile_id),
                None,
                true,
            ));
        }
        // Server-owned agent ids — never trust the client. The id
        // shape matches `run_native_specialist` so AppUI clients can
        // round-trip the value through `agent/status/read` and
        // `agent/interrupt` without translation.
        let agent_id = format!("{backend_kind}-{}", uuid::Uuid::now_v7());
        let path = format!(
            "{}/{}",
            request.parent_agent_id.as_deref().unwrap_or("master"),
            agent_id
        );
        let agent = self.upsert_agent(AgentUpsert {
            agent_id: agent_id.clone(),
            parent_agent_id: request.parent_agent_id,
            session_id: request.session_id.clone(),
            task_id: None,
            path,
            role: role.to_owned(),
            nickname: nickname.to_owned(),
            backend_kind: backend_kind.to_owned(),
            status: "running".to_owned(),
            last_task: (!request.task.trim().is_empty()).then(|| {
                request
                    .task
                    .chars()
                    .take(MAX_OBJECTIVE_BYTES)
                    .collect::<String>()
            }),
            cwd: request.cwd.filter(|cwd| !cwd.is_empty()),
            profile_id: request.profile_id.clone(),
        });
        Ok(json!({
            "session_id": request.session_id,
            "profile_id": request.profile_id,
            "agent_id": agent_id,
            "agent": agent,
            "ok": true,
        }))
    }

    /// #991 / M15-B — synchronous `send_input` appends the input as a
    /// new `last_task` marker and bumps the agent record's
    /// `updated_at_ms`. A future backend impl can override to route
    /// the input to a running supervised process / MCP transport
    /// stdin. Refuses to deliver input to terminal agents.
    fn send_input(&self, request: AgentInputRequest) -> Result<Value, RpcError> {
        let input = request.input.trim();
        if input.is_empty() {
            return Err(autonomy_error(
                kinds::AGENT_CONTROL_UNAVAILABLE,
                "send_input requires a non-empty input",
                request.session_id.as_ref(),
                Some(&request.profile_id),
                Some(("agent_id", request.agent_id.as_str())),
                true,
            ));
        }
        let scope_request = AgentRequest {
            agent_id: request.agent_id.clone(),
            session_id: request.session_id.clone(),
            profile_id: request.profile_id.clone(),
        };
        let mut state = self.state();
        let agent = state
            .agents
            .get_mut(&request.agent_id)
            .ok_or_else(|| agent_not_found_error(&scope_request))?;
        ensure_agent_control_scope(agent, request.session_id.as_ref(), &request.profile_id)?;
        if is_agent_terminal_status(&agent.status) {
            return Err(autonomy_error(
                kinds::AGENT_CONTROL_UNAVAILABLE,
                "send_input cannot deliver to a terminal agent",
                request.session_id.as_ref().or(Some(&agent.session_id)),
                Some(&request.profile_id),
                Some(("agent_id", agent.agent_id.as_str())),
                true,
            ));
        }
        agent.last_task = Some(input.chars().take(MAX_OBJECTIVE_BYTES).collect());
        agent.updated_at_ms = now_ms();
        Ok(json!({
            "agent_id": agent.agent_id,
            "session_id": agent.session_id,
            "delivered": true,
            "ok": true,
            "agent": autonomy_agent_json(agent),
        }))
    }

    /// #991 / M15-B — synchronous `wait_agent` resolves immediately
    /// when the agent is already terminal, otherwise returns the
    /// current (non-terminal) agent record with `terminal: false`.
    /// True streaming/blocking semantics will land with the backend
    /// impl when subprocess JoinHandles are wired through the trait.
    fn wait_agent(&self, request: AgentRequest) -> Result<Value, RpcError> {
        let state = self.state();
        let agent = get_agent(&state, &request)?;
        let terminal = is_agent_terminal_status(&agent.status);
        Ok(json!({
            "agent_id": agent.agent_id,
            "session_id": agent.session_id,
            "terminal": terminal,
            "status": agent.status,
            "agent": autonomy_agent_json(agent),
            "ok": true,
        }))
    }

    /// #991 / M15-B — `resume_agent` is a re-attach: it returns the
    /// current agent record so the caller can rebuild its dispatch
    /// context without a separate `agent/status/read` round-trip.
    /// Refuses to resume terminal agents (use `spawn_agent` for a
    /// fresh child).
    fn resume_agent(&self, request: ResumeAgentRequest) -> Result<Value, RpcError> {
        let scope = AgentRequest {
            agent_id: request.agent_id.clone(),
            session_id: request.session_id.clone(),
            profile_id: request.profile_id.clone(),
        };
        let state = self.state();
        let agent = get_agent(&state, &scope)?;
        if is_agent_terminal_status(&agent.status) {
            return Err(autonomy_error(
                kinds::AGENT_CONTROL_UNAVAILABLE,
                "resume_agent cannot attach to a terminal agent",
                request.session_id.as_ref().or(Some(&agent.session_id)),
                Some(&request.profile_id),
                Some(("agent_id", agent.agent_id.as_str())),
                true,
            ));
        }
        Ok(json!({
            "agent_id": agent.agent_id,
            "session_id": agent.session_id,
            "agent": autonomy_agent_json(agent),
            "ok": true,
        }))
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
            let prior_status = goal.status.clone();
            if let Some(status) = requested_status {
                goal.status = status.to_owned();
            }
            if let Some(token_budget) = request.token_budget {
                goal.token_budget = token_budget;
            }
            goal.updated_at_ms = now;
            // #979 / M15-C2 — re-activating a goal (paused/budget_limited
            // → active) must clear the wrap-up flag so a re-budgeted goal
            // can fire a fresh exhaustion wrap-up; without this the new
            // active window silently never emits its summary turn.
            if goal.status == "active" && prior_status != "active" {
                goal.wrap_up_emitted = false;
                if goal.tokens_used < goal.token_budget {
                    // user-driven re-activation also restarts the
                    // sliding rate-limit window so the prior burst
                    // does not penalize a freshly-budgeted goal.
                    goal.rate_window_start_ms = now;
                    goal.rate_window_count = 0;
                }
            }
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
                continuations_used: 0,
                last_continued_at_ms: 0,
                rate_window_start_ms: now,
                rate_window_count: 0,
                wrap_up_emitted: false,
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
            // #1130 — fresh loop has zero fires consumed.
            fires_used: 0,
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
                // #977 Bullets 1–3: route every fire-now through
                // `LoopRuntime::decide_fire`. FireNow is a manual user
                // gesture, so slash commands are authorized "now"; the
                // runtime still enforces pause/delete/budget/slash-policy
                // gates and surfaces the denial reason on the wire.
                let runtime = loop_runtime_view(loop_record);
                let fire_context = LoopFireContext::idle()
                    .with_slash_authorization(SlashCommandAuthorization::authorized_now());
                let decision =
                    runtime.decide_fire(SystemTime::now(), LoopFireTrigger::FireNow, fire_context);
                match decision {
                    LoopFireDecision::Denied(reason) | LoopFireDecision::Exhausted { reason } => {
                        return Err(loop_runtime_denied_error(loop_record, &reason));
                    }
                    LoopFireDecision::WaitUntil(wait) => {
                        return Err(loop_runtime_wait_error(loop_record, &wait));
                    }
                    LoopFireDecision::Fire(_plan) => {}
                }

                // Bullet 3: resolve maintenance prompts at fire time —
                // the persisted record may carry the stale create-time
                // string, but the operator's `.octos/loop.md` is the
                // source of truth for each individual fire.
                let (resolved_prompt, prompt_source_label) =
                    if matches!(runtime.invocation, LoopInvocation::MaintenancePrompt) {
                        let resolution = resolve_maintenance_prompt_at_fire_time();
                        (
                            resolution.prompt,
                            maintenance_prompt_source_label(resolution.source),
                        )
                    } else {
                        (loop_record.prompt.clone(), "record")
                    };

                let session_id = loop_record.session_id.clone();
                let profile_id = loop_record.profile_id.clone();
                let loop_id = loop_record.loop_id.clone();
                let interval_seconds = loop_record.interval_seconds;
                loop_record.last_run_at_ms = Some(now);
                loop_record.next_run_at_ms = interval_seconds.and_then(|seconds| {
                    i64::try_from(seconds)
                        .ok()
                        .and_then(|seconds| seconds.checked_mul(1_000))
                        .and_then(|delay_ms| now.checked_add(delay_ms))
                });
                loop_record.updated_at_ms = now;
                // Persist the schedule-side timestamp updates regardless
                // of enqueue outcome (we still attempted a fire).
                persist_loop_state_with_store(supervisor_store.as_ref(), loop_record);

                let continuation = MasterContinuationRequest::new(
                    "coding-autonomy",
                    session_id.to_string(),
                    profile_id.clone(),
                    MasterContinuationReason::LoopFire,
                    SystemTime::now(),
                )
                .with_loop_id(loop_id.clone())
                .with_metadata("prompt", resolved_prompt)
                .with_metadata("prompt_source", prompt_source_label);
                let outcome = enqueue_and_persist_continuation(&mut state, continuation);
                // #1138 codex P2 follow-up to #1130: only count the
                // fire toward the persisted `fires_used` budget when a
                // NEW continuation was actually queued. `Duplicate`
                // outcomes mean the prior continuation is still
                // pending — a retry/spam should NOT burn the safety
                // budget, otherwise users can exhaust a loop early by
                // repeatedly clicking `fire_now` while a fire is in
                // flight. `saturating_add` defends against a corrupt
                // snapshot restore.
                let newly_queued = matches!(outcome, MasterContinuationEnqueueOutcome::Queued(_));
                if newly_queued {
                    let loop_record = state
                        .loops
                        .get_mut(&loop_id)
                        .expect("loop record still present");
                    loop_record.fires_used = loop_record.fires_used.saturating_add(1);
                    persist_loop_state_with_store(supervisor_store.as_ref(), loop_record);
                }
                let loop_json = state
                    .loops
                    .get(&loop_id)
                    .map(autonomy_loop_json)
                    .unwrap_or(Value::Null);
                let fire = master_continuation_enqueue_json(outcome);

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

/// #1140 codex P1 re-review #4 — RAII drop-guard returned by
/// `InProcessAgentOrchestrator::goal_dispatch_in_flight_guard`. On
/// `Drop` it clears the in-flight marker for the captured session
/// id, so the marker is removed even when the AppUI turn is
/// aborted, panics, or returns through an early-terminal path
/// before the post-accounting block runs.
///
/// Call `disarm()` from the post-accounting block (after the
/// orchestrator already cleared the marker explicitly) so the
/// drop-time clear becomes a no-op. The guard is `must_use` to
/// discourage accidental immediate drop at the dispatch site.
#[must_use = "GoalDispatchInFlightGuard clears the in-flight marker on drop; hold it for the duration of the goal turn"]
pub(crate) struct GoalDispatchInFlightGuard {
    orchestrator: &'static InProcessAgentOrchestrator,
    session_id: SessionKey,
    disarmed: bool,
}

impl GoalDispatchInFlightGuard {
    /// Mark the guard as disarmed so its `Drop` does NOT clear the
    /// in-flight marker. Use this when the post-accounting block has
    /// already called `clear_goal_dispatch_in_flight` explicitly,
    /// to avoid a redundant clear.
    #[allow(dead_code)]
    pub(crate) fn disarm(mut self) {
        self.disarmed = true;
    }
}

impl Drop for GoalDispatchInFlightGuard {
    fn drop(&mut self) {
        if !self.disarmed {
            self.orchestrator
                .clear_goal_dispatch_in_flight(&self.session_id);
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
    /// #991 / M15-B — per-agent cancellation handles registered by
    /// `run_native_specialist` (and future specialist runners) so that
    /// `interrupt_agent` / `close_agent` can signal a *real* abort to
    /// the running task instead of only mutating in-memory status. The
    /// handle is dropped when the agent reaches a terminal state. A
    /// `tokio::sync::Notify` is sufficient here: the worker holds an
    /// `Arc<Notify>` and selects on `notified()` against its workload;
    /// `notify_waiters()` wakes every clone. Compared to a
    /// `CancellationToken` this avoids pulling in `tokio_util` for one
    /// signal type, and the orchestrator does not need to inspect the
    /// "armed" state (the worker already owns the source of truth via
    /// the agent status transition).
    cancellations: HashMap<String, Arc<tokio::sync::Notify>>,
    /// #1140 codex P2 re-review #3 — sessions whose AppUI tick path
    /// has dispatched a goal continuation and not yet finished
    /// post-turn accounting. `due_loop_targets`'s goal sweep skips
    /// these so a long-running goal turn (model + tool work > 30s
    /// `GOAL_MIN_CONTINUATION_INTERVAL_MS`) can't be re-dispatched
    /// in the await gap between turn-terminal emission and
    /// `record_goal_turn`. Entries are added by
    /// `mark_goal_dispatch_in_flight` and cleared by
    /// `clear_goal_dispatch_in_flight`. Independent of (and
    /// complementary to) the `last_continued_at_ms` timestamp, which
    /// remains the authoritative min-delay gate for all other callers.
    in_flight_goal_sessions: std::collections::HashSet<SessionKey>,
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
    /// #979 / M15-C2 — number of goal continuation turns this goal has
    /// driven since `set_goal` was first called (or since the goal was
    /// last reset to `active`). Used together with
    /// `last_continued_at_ms` and `rate_window_*` to enforce the
    /// min-delay + max-per-hour fire policy.
    continuations_used: u32,
    /// Wall-clock ms of the last successful goal-continuation fire.
    /// Zero means no continuation has fired yet. Drives the min-delay
    /// gate on subsequent fires.
    last_continued_at_ms: i64,
    /// Start of the current sliding rate-limit window (one hour).
    rate_window_start_ms: i64,
    /// Number of continuations counted within `rate_window_start_ms`.
    rate_window_count: u32,
    /// `true` once the orchestrator has enqueued the budget-exhaustion
    /// wrap-up turn so a `record_goal_turn` call after `budget_limited`
    /// does not re-emit duplicate wrap-ups on every subsequent
    /// continuation attempt.
    wrap_up_emitted: bool,
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
    /// #1130 — number of fires this loop has consumed against
    /// `LOOP_DEFAULT_MAX_FIRES`. Persisted to the supervisor store so the
    /// runtime budget gate is enforced across daemon restarts and across
    /// repeated `fire_now` invocations (`loop_runtime_view` was previously
    /// rebuilding the runtime with a zeroed `fires_used` counter, so the
    /// max-fires safety cap never tripped). Defaults to 0 for legacy
    /// snapshots that pre-date this field.
    fires_used: u32,
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
    /// #1135: carries the `MaintenancePromptSource` resolved at fire
    /// time for maintenance loops, so the queued continuation metadata
    /// reports the same `project` / `user` / `built_in` provenance as
    /// the `fire_now` path. `None` for non-maintenance modes — those
    /// fall back to the legacy `"record"` label.
    prompt_source: Option<MaintenancePromptSource>,
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
        // #1128 codex P1 follow-up: drop the `mode != "fixed_interval"`
        // filter so self-paced and maintenance loops are also drained
        // when their stamped `next_run_at_ms` is past. The runtime
        // fire decision below still gates on mode-specific policy.
        if loop_record.status != "active"
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
        // #1128 codex P1 follow-up: `interval_seconds` is only required
        // for fixed_interval mode (used to recompute `next_run_at_ms`
        // after firing). Self-paced / maintenance loops compute their
        // own next delay from the model reply (`<<loop-next-in: ...>>`)
        // and may legitimately omit `interval_seconds` — don't reject
        // them here; we conditionally update next_run_at_ms below.
        if loop_record.mode == "fixed_interval" && loop_record.interval_seconds.is_none() {
            continue;
        }
        // #977 Bullets 1–2: consult `LoopRuntime` on the scheduled-due
        // path. A scheduled tick is not a fresh user gesture, so slash
        // commands present the `authorized_at_creation_only` claim —
        // re-auth-each-fire policy denies them; legacy prompts pass
        // through. The runtime also enforces budget / pause / idle gates.
        let runtime = loop_runtime_view(loop_record);
        let fire_context = LoopFireContext::idle()
            .with_slash_authorization(SlashCommandAuthorization::authorized_at_creation_only());
        match runtime.decide_fire(
            SystemTime::now(),
            LoopFireTrigger::ScheduledDue,
            fire_context,
        ) {
            LoopFireDecision::Fire(_plan) => {}
            // Bullet 1: do NOT enqueue if the runtime denies (paused,
            // exhausted, slash-without-reauth, busy, …). The scheduler
            // will reconsider the loop on the next tick — if the deny
            // reason is transient the loop fires then; if it is sticky
            // (e.g. pause), control_loop will unstick it.
            LoopFireDecision::Denied(_)
            | LoopFireDecision::Exhausted { .. }
            | LoopFireDecision::WaitUntil(_) => {
                continue;
            }
        }
        // #1128 codex P2 follow-up: maintenance loops resolve their
        // prompt from `.octos/loop.md` / `~/.octos/loop.md` / the
        // built-in fallback at FIRE time. `fire_now` already does
        // this; the scheduled-due path now does it too so an operator
        // edit to `.octos/loop.md` between fires actually takes
        // effect on the next scheduled tick. fixed_interval and
        // self_paced keep the persisted prompt.
        // #1135: capture the resolved `MaintenancePromptSource` here
        // and forward it through `DueLoopFire` so the queued
        // continuation metadata reports `project` / `user` /
        // `built_in` instead of the legacy `"record"` placeholder.
        let (fire_prompt, fire_prompt_source) = if loop_record.mode == "maintenance" {
            let resolution = resolve_maintenance_prompt_at_fire_time();
            (resolution.prompt, Some(resolution.source))
        } else {
            (loop_record.prompt.clone(), None)
        };
        due.push(DueLoopFire {
            session_id: loop_record.session_id.clone(),
            profile_id: loop_record.profile_id.clone(),
            loop_id: loop_record.loop_id.clone(),
            prompt: fire_prompt,
            scheduled_for_ms: next_run_at_ms,
            prompt_source: fire_prompt_source,
        });
        loop_record.last_run_at_ms = Some(now);
        // #1128 codex P1 follow-up: only `fixed_interval` mode
        // recomputes `next_run_at_ms` here using `interval_seconds`.
        // Self-paced loops have their next delay parsed from the
        // model reply (`<<loop-next-in: ...>>`) by
        // `apply_self_paced_response` after the turn completes, so we
        // clear the timestamp here to prevent the scheduler from
        // re-picking-up the same loop in a tight loop before the
        // response handler has stamped the new delay. Maintenance
        // loops behave the same way.
        if loop_record.mode == "fixed_interval" {
            if let Some(interval_seconds) = loop_record.interval_seconds {
                loop_record.next_run_at_ms = next_loop_run_at(now, interval_seconds);
            }
        } else {
            loop_record.next_run_at_ms = None;
        }
        loop_record.updated_at_ms = now;
        updated_loops.push(loop_record.clone());
    }

    for loop_record in &updated_loops {
        persist_loop_state(state, loop_record);
    }

    let mut queued = 0;
    for fire in due {
        // #1135: align the scheduled-due metadata with `fire_now` —
        // maintenance loops report the resolved provenance, every
        // other mode falls back to the legacy `"record"` label.
        let prompt_source_label = fire
            .prompt_source
            .map(maintenance_prompt_source_label)
            .unwrap_or("record");
        let loop_id_for_increment = fire.loop_id.clone();
        let continuation = MasterContinuationRequest::new(
            "coding-autonomy",
            fire.session_id.to_string(),
            fire.profile_id.clone(),
            MasterContinuationReason::LoopFire,
            SystemTime::now(),
        )
        .with_loop_id(fire.loop_id)
        .with_metadata("prompt", fire.prompt)
        .with_metadata("prompt_source", prompt_source_label)
        .with_metadata("scheduled_for_ms", fire.scheduled_for_ms.to_string());
        let outcome = enqueue_and_persist_continuation(state, continuation);
        // #1138 codex P2 follow-up to #1130: only count the scheduled
        // fire toward the persisted `fires_used` budget when a NEW
        // continuation was actually queued. `Duplicate` outcomes (the
        // prior continuation is still pending) must not burn the
        // safety budget, otherwise a sticky pending fire could
        // exhaust the loop's MAX_FIRES with no real LLM executions.
        if outcome.queued().is_some() {
            let snapshot = state
                .loops
                .get_mut(&loop_id_for_increment)
                .map(|loop_record| {
                    loop_record.fires_used = loop_record.fires_used.saturating_add(1);
                    loop_record.clone()
                });
            if let Some(snapshot) = snapshot {
                persist_loop_state(state, &snapshot);
            }
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
    // Codex P1 follow-up to #1121: spec-conforming M13 clients call
    // `task/artifact/*` with `task_id` (the `TaskListEntry.id`), not
    // `agent_id`. Task-backed records (native specialists, mirrored
    // background tasks) carry the task id under `task_id` and the
    // agent id can differ (`native-…` prefixes, sanitisations, etc.).
    // First try direct agent_id lookup (legacy + agent-only records),
    // then fall back to scanning by `task_id` so the alias actually
    // resolves to the right agent record.
    //
    // Codex P1 re-review #4 on #1121: this fallback is shared by all
    // agent-keyed endpoints — `agent/artifact/*`, `agent/status/read`,
    // `agent/output/read`, and the legacy `agent_id` branch of
    // `task/artifact/*`. Without the session_id gate, a same-profile
    // caller could put a known task UUID in `agent_id` (bypassing the
    // params-layer `task_id`-requires-`session_id` check) and the
    // fallback would resolve it, with `ensure_agent_control_scope`
    // collapsing to profile-only when `session_id` is `None`. Require
    // `session_id` for the task-id fallback path so the session/
    // parent-child ownership check is always exercised on task-keyed
    // lookups. Legacy direct `agent_id` lookups remain unaffected.
    let direct = state.agents.get(&request.agent_id);
    let agent = if let Some(found) = direct {
        found
    } else if request.session_id.is_some() {
        match state.agents.values().find(|candidate| {
            candidate
                .task_id
                .as_ref()
                .is_some_and(|task| task.to_string() == request.agent_id)
        }) {
            Some(found) => found,
            None => return Err(agent_not_found_error(request)),
        }
    } else {
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

/// #1129 codex P1 follow-up to #979 / M15-C2 — scan the session's
/// active goal (if any) and enqueue a continuation when the policy
/// gate now allows it. Mirrors `enqueue_due_loop_continuations` for
/// the goal-recurrence path. Without this scan, the only goal-enqueue
/// happens inside `maybe_enqueue_goal_after_turn` immediately after
/// `record_goal_turn` stamped `last_continued_at_ms = now`, which the
/// 30s min-delay always denies — so active goals only ever fired
/// their initial continuation and silently stopped.
/// #1145 codex P1 follow-up — decide whether a pending master
/// continuation should still be exposed to the AppUI scheduler.
/// Goal/loop continuations are filtered when their owning record has
/// been paused/cleared/deleted so the new pending-queue sweep
/// (#1141) doesn't reanimate stale autonomy work. Continuations
/// without an owning goal/loop (e.g. `ChildCompleted`, `External`)
/// pass through — they were the original wrap-up-style use case for
/// the sweep.
fn pending_continuation_is_schedulable(
    state: &AutonomyRuntimeState,
    item: &QueuedMasterContinuation,
) -> bool {
    match &item.reason {
        MasterContinuationReason::LoopFire => {
            // Loop is identified by `loop_id` (string). Skip if the
            // loop record is absent, deleted, or paused.
            let Some(loop_id) = item.loop_id.as_ref() else {
                return true;
            };
            let Some(loop_record) = state.loops.get(loop_id.as_str()) else {
                return false;
            };
            matches!(loop_record.status.as_str(), "active")
        }
        MasterContinuationReason::GoalContinue => {
            // Goals are session-scoped. Skip if the goal is paused,
            // cleared, complete, or absent.
            let session_key = SessionKey(item.session_id.as_str().to_owned());
            let Some(goal) = state.goals.get(&session_key) else {
                return false;
            };
            // #1145 codex P2 re-review #2: enforce goal-id identity
            // BEFORE the legacy wrap-up exemption. When the user
            // cleared the old goal and created a different one for
            // the same `SessionKey`, the stale legacy wrap-up still
            // carries the old `goal_id`. Without this check, the
            // wrap-up exemption below would bypass the identity
            // guard and wake the session against the new goal,
            // letting the stale wrap-up render against an
            // unrelated objective.
            if let Some(item_goal_id) = item.goal_id.as_ref() {
                if item_goal_id.as_str() != goal.goal_id {
                    return false;
                }
            }
            // #1145 codex P2 re-review: pre-#1131 wrap-up turns were
            // queued as `GoalContinue` + `wrap_up_prompt` metadata,
            // and the prompt renderer promotes that shape at render
            // time (see `legacy_goal_continue_with_wrap_up_metadata_promotes_to_wrap_up`).
            // After budget exhaustion the owning goal is
            // `budget_limited`, so the active-only gate would
            // strand legacy persisted wrap-ups indefinitely.
            // Detect the legacy shape and let it through — the goal
            // record's id already matched above.
            if item.metadata.contains_key("wrap_up_prompt") || item.metadata.contains_key("wrap_up")
            {
                return true;
            }
            matches!(goal.status.as_str(), "active")
        }
        MasterContinuationReason::GoalWrapUp => {
            // Wrap-up is the explicit terminal goal turn — must drain
            // even when the goal is `budget_limited`. Skip only if
            // the goal has since been cleared (operator nuked it
            // mid-wrap-up) OR was replaced by a different goal.
            let session_key = SessionKey(item.session_id.as_str().to_owned());
            let Some(goal) = state.goals.get(&session_key) else {
                return false;
            };
            if let Some(item_goal_id) = item.goal_id.as_ref() {
                if item_goal_id.as_str() != goal.goal_id {
                    return false;
                }
            }
            true
        }
        // ChildCompleted, ScatterJoinComplete, External — no owning
        // goal/loop record to inspect, pass through.
        _ => true,
    }
}

/// #1159 codex P2 follow-up to #1150 — decide whether a drain-time
/// "stale drop" should write a `ContinuationCompleted` ledger event.
///
/// We tombstone ONLY when the owning entity is gone in a way that
/// guarantees the same dedupe_key cannot recur — goal cleared and
/// replaced (different goal_id) or loop deleted. Without that
/// guarantee, tombstoning would defeat a legitimate re-queue: the
/// supervisor store ranks `Completed > Queued` in `upsert_continuation`,
/// so a fresh Queued event arriving after a Completed tombstone for
/// the same `(group, continuation_id)` key is silently ignored.
///
/// The "paused" subset of unschedulability (loop status != "active",
/// goal status != "active" but goal_id still matches) intentionally
/// returns false here: when the user resumes the entity, the periodic
/// `enqueue_due_*_continuations` sweep is expected to re-queue with
/// the same stable dedupe_key, and any Completed tombstone written
/// during the pause would prevent the new Queued event from sticking
/// in the ledger.
fn stale_drop_should_tombstone(
    state: &AutonomyRuntimeState,
    item: &QueuedMasterContinuation,
) -> bool {
    match &item.reason {
        MasterContinuationReason::LoopFire => {
            let Some(loop_id) = item.loop_id.as_ref() else {
                return false;
            };
            // `control_loop` does NOT remove a deleted loop from
            // `state.loops`; it sets `status = "deleted"`. So a
            // queued LoopFire whose owning loop has been deleted
            // still finds a record on lookup. Treat that as
            // tombstone-worthy: a deleted loop cannot re-queue with
            // the same dedupe_key (a future loop with the same
            // user-supplied id would surface as a fresh record on
            // re-create, but operator deletion is the user's signal
            // that the stale fire is unwanted).
            match state.loops.get(loop_id.as_str()) {
                None => true,
                Some(loop_record) => loop_record.status == "deleted",
            }
        }
        MasterContinuationReason::GoalContinue | MasterContinuationReason::GoalWrapUp => {
            let session_key = SessionKey(item.session_id.as_str().to_owned());
            let Some(goal) = state.goals.get(&session_key) else {
                // Goal was cleared. dedupe_key includes goal_id; a
                // future goal under the same session will have a
                // distinct goal_id and thus a distinct dedupe_key.
                return true;
            };
            if let Some(item_goal_id) = item.goal_id.as_ref() {
                if item_goal_id.as_str() != goal.goal_id {
                    // Different goal took the session's slot — same
                    // session_key but new goal_id, so dedupe_key
                    // can't recur. Safe to tombstone.
                    return true;
                }
            }
            // Same goal_id is still present (e.g. paused,
            // budget_limited). Resuming it can re-queue the same
            // dedupe_key; don't tombstone.
            false
        }
        // ChildCompleted, ScatterJoinComplete, External — no entity
        // identity attached. Leave the ledger entry alone; the
        // in-memory drop is sufficient.
        _ => false,
    }
}

fn enqueue_due_goal_continuations(
    state: &mut AutonomyRuntimeState,
    session_id: &SessionKey,
    profile_id: &str,
    runtime_state: MasterContinuationRuntimeState,
    now: i64,
) -> usize {
    if !runtime_state.is_idle_eligible() {
        return 0;
    }
    // #1140 codex P2 re-review #4: also gate the goal-enqueue path
    // on `in_flight_goal_sessions`. `due_loop_targets` already skips
    // in-flight sessions for its goal sweep, but
    // `drain_ready_continuations_for_session` (which calls this
    // function) is also invoked when a session is selected by an
    // active loop target — in that path the goal enqueue would
    // otherwise queue a stale `GoalContinue` despite the in-flight
    // turn. The two guards together ensure the in-flight marker is
    // the authoritative gate on every enqueue path.
    if state.in_flight_goal_sessions.contains(session_id) {
        return 0;
    }
    let Some(goal) = state.goals.get(session_id).cloned() else {
        return 0;
    };
    if goal.profile_id != profile_id {
        return 0;
    }
    // Re-use the canonical policy gate. `idle_state` is "idle" here
    // because the AppUI / session-actor tick path only calls into
    // this drain when no other turn is active.
    let idle_state = GoalRuntimeIdleState::idle();
    let now_system = system_time_from_ms(now).unwrap_or_else(SystemTime::now);
    if !goal_policy_allows_fire(&goal, idle_state, now_system, now) {
        return 0;
    }
    match enqueue_goal_continuation_with_idle(state, session_id, profile_id, &goal, idle_state) {
        Some(MasterContinuationEnqueueOutcome::Queued(_)) => 1,
        _ => 0,
    }
}

fn enqueue_goal_continuation(
    state: &mut AutonomyRuntimeState,
    session_id: &SessionKey,
    profile_id: &str,
    goal: &AutonomyGoalRecord,
) -> Option<MasterContinuationEnqueueOutcome> {
    enqueue_goal_continuation_with_idle(
        state,
        session_id,
        profile_id,
        goal,
        GoalRuntimeIdleState::idle(),
    )
}

/// #979 / M15-C2 — gated enqueue path used by every production
/// `set_goal` and after-turn re-queue. Defers to a transient
/// [`GoalRuntime`] view so the orchestrator and the standalone runtime
/// primitives agree on the fire policy: min-delay, total budget,
/// active/paused state. The hourly rate limit is a thin wrapper on top
/// of the runtime view since `GoalRuntime` does not natively express a
/// sliding-window cap. Returns `None` when the policy denies the fire.
fn enqueue_goal_continuation_with_idle(
    state: &mut AutonomyRuntimeState,
    session_id: &SessionKey,
    profile_id: &str,
    goal: &AutonomyGoalRecord,
    idle_state: GoalRuntimeIdleState,
) -> Option<MasterContinuationEnqueueOutcome> {
    let now_system = SystemTime::now();
    let now = now_ms();
    if !goal_policy_allows_fire(goal, idle_state, now_system, now) {
        return None;
    }
    let continuation = MasterContinuationRequest::new(
        "coding-autonomy-goal",
        session_id.to_string(),
        profile_id.to_owned(),
        MasterContinuationReason::GoalContinue,
        now_system,
    )
    .with_goal_id(goal.goal_id.clone())
    .with_metadata("objective", goal.objective.clone())
    .with_metadata("status", goal.status.clone());
    Some(enqueue_and_persist_continuation(state, continuation))
}

/// #979 / M15-C2 — build a [`GoalRuntime`] view from the orchestrator
/// record so policy gates (min-delay, total budget, paused) all derive
/// from one place. The hourly cap is enforced separately by the caller
/// (see [`goal_policy_allows_fire`]).
fn goal_runtime_view(goal: &AutonomyGoalRecord) -> GoalRuntime {
    let total_budget = goal_total_continuation_budget(goal);
    let mut runtime = GoalRuntime::new(
        goal.goal_id.clone(),
        goal.objective.clone(),
        GoalRuntimePolicy::fixed_interval(
            std::time::Duration::from_millis(GOAL_MIN_CONTINUATION_INTERVAL_MS as u64),
            total_budget,
        ),
    );
    runtime.continuations_used = goal.continuations_used;
    runtime.state = match goal.status.as_str() {
        "paused" => GoalRuntimeState::Paused,
        "complete" | "completed" | "cleared" => GoalRuntimeState::Completed,
        _ => GoalRuntimeState::Active,
    };
    if goal.last_continued_at_ms > 0 {
        let due_at = goal
            .last_continued_at_ms
            .saturating_add(GOAL_MIN_CONTINUATION_INTERVAL_MS);
        if let Some(system_time) = system_time_from_ms(due_at) {
            runtime.next_due = NextDueState::ScheduledAt(system_time);
        }
    }
    runtime
}

/// #979 / M15-C2 — derived total budget for the goal (in continuation
/// turn count). Token budget is converted with a conservative
/// per-turn estimate (4 KB ≈ 1000 tokens) so the runtime view's
/// `max_continuations` matches what the model can actually spend.
/// Saturating math keeps this safe for `token_budget = 0`.
fn goal_total_continuation_budget(goal: &AutonomyGoalRecord) -> u32 {
    const TOKENS_PER_TURN_ESTIMATE: u64 = 2_500;
    if goal.token_budget == 0 {
        return 0;
    }
    goal.token_budget
        .div_ceil(TOKENS_PER_TURN_ESTIMATE)
        .min(u32::MAX as u64) as u32
}

fn system_time_from_ms(ms: i64) -> Option<SystemTime> {
    if ms <= 0 {
        return None;
    }
    UNIX_EPOCH.checked_add(std::time::Duration::from_millis(ms as u64))
}

/// #979 / M15-C2 — policy gate for goal continuation fires. Combines:
///   * [`GoalRuntime::decide_when_idle`] — min-delay + total budget +
///     active/paused/complete state + idle eligibility.
///   * Sliding-window hourly cap — enforced here because
///     `GoalRuntime` does not natively express a per-hour cap.
///   * Token-budget exhaustion — already known by the record.
fn goal_policy_allows_fire(
    goal: &AutonomyGoalRecord,
    idle_state: GoalRuntimeIdleState,
    now_system: SystemTime,
    now_ms_value: i64,
) -> bool {
    if goal.status != "active" {
        return false;
    }
    if goal.tokens_used >= goal.token_budget && goal.token_budget > 0 {
        return false;
    }
    let runtime = goal_runtime_view(goal);
    match runtime.decide_when_idle(now_system, idle_state) {
        GoalPolicyDecision::ContinueNow { .. } => {}
        _ => return false,
    }
    // Sliding-window hourly cap. A fresh window starts whenever the
    // recorded window is older than GOAL_RATE_WINDOW_MS.
    let window_age = now_ms_value.saturating_sub(goal.rate_window_start_ms);
    if window_age < GOAL_RATE_WINDOW_MS && goal.rate_window_count >= GOAL_MAX_CONTINUATIONS_PER_HOUR
    {
        return false;
    }
    true
}

/// #979 / M15-C2 — record a goal continuation turn fire, advancing the
/// per-goal counters used by [`goal_policy_allows_fire`]. The caller
/// passes `tokens_consumed` so the runtime tracks LLM-side token spend
/// against the goal's `token_budget`. Returns the wrap-up prompt when
/// this call exhausts the budget so the session actor can enqueue the
/// final "summarize and stop" turn.
fn record_goal_turn_internal(
    goal: &mut AutonomyGoalRecord,
    tokens_consumed: u64,
    elapsed_seconds: u64,
    now_ms_value: i64,
) -> Option<String> {
    goal.continuations_used = goal.continuations_used.saturating_add(1);
    goal.last_continued_at_ms = now_ms_value;
    goal.updated_at_ms = now_ms_value;
    goal.tokens_used = goal.tokens_used.saturating_add(tokens_consumed);
    goal.time_used_seconds = goal.time_used_seconds.saturating_add(elapsed_seconds);
    let window_age = now_ms_value.saturating_sub(goal.rate_window_start_ms);
    if window_age >= GOAL_RATE_WINDOW_MS {
        goal.rate_window_start_ms = now_ms_value;
        goal.rate_window_count = 1;
    } else {
        goal.rate_window_count = goal.rate_window_count.saturating_add(1);
    }
    let budget_exhausted =
        goal.token_budget > 0 && goal.tokens_used >= goal.token_budget && !goal.wrap_up_emitted;
    if budget_exhausted {
        goal.status = "budget_limited".to_owned();
        goal.wrap_up_emitted = true;
        Some(format!(
            "Goal `{}` has exhausted its continuation budget. Summarize the current state, call out remaining work, and stop starting new work.",
            goal.goal_id
        ))
    } else {
        None
    }
}

/// #979 / M15-C2 — detect the model-driven completion sentinels and
/// flip the goal to `complete`. Returns `true` if any sentinel matched
/// so the caller can stop re-queueing.
fn detect_goal_complete_sentinel(content: &str) -> bool {
    // #1129 codex P2 follow-up: only match when the sentinel appears
    // at the END of the assistant reply, not anywhere in the body.
    // The prior `contains` check meant any assistant message that
    // merely mentioned `goal_complete` / `<goal:complete>` in prose,
    // code samples, or instructions silently completed the goal and
    // stopped recurrence. Anchor to the trimmed last line / trailing
    // token so the sentinel must be a deliberate end-of-reply
    // declaration, not an incidental mention.
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    let last_line = lower
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .map(str::trim)
        .unwrap_or("");
    GOAL_COMPLETE_SENTINELS
        .iter()
        .any(|sentinel| last_line == *sentinel || last_line.ends_with(sentinel))
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
        continuations_used: supervisor_metadata_u64(&group.metadata, "continuations_used")
            .unwrap_or(0)
            .min(u32::MAX as u64) as u32,
        last_continued_at_ms: supervisor_metadata_i64(&group.metadata, "last_continued_at_ms")
            .unwrap_or(0),
        rate_window_start_ms: supervisor_metadata_i64(&group.metadata, "rate_window_start_ms")
            .unwrap_or(0),
        rate_window_count: supervisor_metadata_u64(&group.metadata, "rate_window_count")
            .unwrap_or(0)
            .min(u32::MAX as u64) as u32,
        wrap_up_emitted: supervisor_metadata_bool(&group.metadata, "wrap_up_emitted")
            .unwrap_or(false),
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
        // #1130 — replay the persisted `fires_used` counter so the
        // `LoopRuntime` budget gate sees the real consumed-fires value
        // (not a fresh zero) after a daemon restart. Legacy snapshots
        // that pre-date #1130 lack this key — `unwrap_or(0)` keeps them
        // working without forcing a manual migration.
        fires_used: supervisor_metadata_u64(&group.metadata, "fires_used")
            .unwrap_or(0)
            .min(u32::MAX as u64) as u32,
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
        continuations_used: 0,
        last_continued_at_ms: 0,
        rate_window_start_ms: now,
        rate_window_count: 0,
        wrap_up_emitted: false,
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
    group.metadata.insert(
        "continuations_used".into(),
        json!(goal.continuations_used as u64),
    );
    group.metadata.insert(
        "last_continued_at_ms".into(),
        json!(goal.last_continued_at_ms),
    );
    group.metadata.insert(
        "rate_window_start_ms".into(),
        json!(goal.rate_window_start_ms),
    );
    group.metadata.insert(
        "rate_window_count".into(),
        json!(goal.rate_window_count as u64),
    );
    group
        .metadata
        .insert("wrap_up_emitted".into(), json!(goal.wrap_up_emitted));
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
    // #1130 — persist the cumulative fires counter alongside the other
    // runtime accountants (`next_run_at_ms`, `last_run_at_ms`, …). Without
    // this every restart resets `fires_used` to zero and the
    // `LOOP_DEFAULT_MAX_FIRES` safety cap silently becomes unenforceable
    // for any loop that out-lives the daemon process.
    group
        .metadata
        .insert("fires_used".into(), json!(loop_record.fires_used as u64));
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
        MasterContinuationReason::GoalWrapUp => "goal_wrap_up",
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
        MasterContinuationReason::GoalContinue => {
            // #1139 codex P2 follow-up: legacy promotion — wrap-up
            // continuations queued by the pre-#1131 wire shape (which
            // used `GoalContinue` + `wrap_up_prompt` metadata) survive
            // a restart with the old reason. Detect that legacy shape
            // here and render it as a wrap-up turn so the in-flight
            // final turn instructs the model to summarize-and-stop
            // instead of "Advance the goal...". New continuations
            // queued post-#1131 use `GoalWrapUp` directly; this
            // promotion is a one-way restore-time fixup.
            let goal_id = continuation
                .goal_id
                .as_ref()
                .map(|id| id.as_str())
                .unwrap_or("unknown");
            if let Some(directive) = continuation.metadata.get("wrap_up_prompt") {
                return format!(
                    "[system-internal]\nThe active goal exhausted its continuation budget. This is the final wrap-up turn.\n\nGoal: {goal_id}\nMetadata:\n{metadata}\n\n{directive}",
                );
            }
            format!(
                "[system-internal]\nAn active goal continuation is ready.\n\nGoal: {goal_id}\nMetadata:\n{metadata}\n\nAdvance the goal by one bounded step. If the goal needs user input, ask a numbered choice question and recommend one option.",
            )
        }
        // #1131 — wrap-up turns must instruct the model to summarize
        // and stop, NOT continue work. Render the per-goal wrap-up
        // directive (stored in metadata by `record_goal_turn`) as
        // the actual prompt body so the LLM sees the instruction
        // verbatim instead of the generic "Advance the goal..."
        // template. Fall back to a safe default directive if the
        // metadata is missing (e.g. legacy persisted continuations).
        MasterContinuationReason::GoalWrapUp => {
            let goal_id = continuation
                .goal_id
                .as_ref()
                .map(|id| id.as_str())
                .unwrap_or("unknown");
            let directive = continuation
                .metadata
                .get("wrap_up_prompt")
                .map(String::as_str)
                .unwrap_or(
                    "This goal has exhausted its continuation budget. Summarize the current state, call out remaining work, and stop starting new work.",
                );
            format!(
                "[system-internal]\nThe active goal exhausted its continuation budget. This is the final wrap-up turn.\n\nGoal: {goal_id}\nMetadata:\n{metadata}\n\n{directive}",
            )
        }
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
        MasterContinuationReason::GoalWrapUp => "goal_wrap_up".to_owned(),
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
        "goal_wrap_up" | "GoalWrapUp" => Some(MasterContinuationReason::GoalWrapUp),
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

// ───── M15-D2/D3 LoopRuntime fire-path wiring (#977) ─────
//
// These helpers translate the persisted `AutonomyLoopRecord` into a
// `LoopRuntime` view, gate the fire path through `decide_fire`, resolve
// maintenance prompts at fire time, and parse the self-paced
// `<<loop-next-in: …>>` sentinel emitted by the model.

/// Project-level maintenance doc — resolved lazily at fire time. The
/// CLI/serve daemon already runs with the project root as cwd, so a
/// relative path is sufficient.
const PROJECT_MAINTENANCE_PROMPT_PATH: &str = ".octos/loop.md";
/// User-level fallback. Tilde expansion mirrors `tools/hooks` semantics
/// (HOME-prefixed, no `~user` form).
const USER_MAINTENANCE_PROMPT_PATH: &str = "~/.octos/loop.md";

/// Build a fresh [`LoopRuntime`] view from the persisted record. The
/// runtime is stateless across fires — it inspects the record's status,
/// schedule, and prompt-kind, then runs the policy gate.
fn loop_runtime_view(record: &AutonomyLoopRecord) -> LoopRuntime {
    let invocation = if record.mode == "maintenance" {
        LoopInvocation::maintenance_prompt()
    } else if record.prompt.trim_start().starts_with('/') {
        LoopInvocation::slash_command(record.prompt.clone())
    } else {
        LoopInvocation::prompt(record.prompt.clone())
    };
    let policy = match record.mode.as_str() {
        "fixed_interval" => LoopRuntimePolicy::fixed_interval(
            Duration::from_secs(record.interval_seconds.unwrap_or(LOOP_MIN_INTERVAL_SECONDS)),
            LOOP_DEFAULT_MAX_FIRES,
        ),
        "maintenance" => LoopRuntimePolicy::maintenance(LOOP_DEFAULT_MAX_FIRES),
        _ => LoopRuntimePolicy::self_paced(LOOP_DEFAULT_MAX_FIRES),
    };
    // #1130 — seed the runtime with the persisted `fires_used` counter.
    // Previously `LoopRuntime::new` zeroed this field on every decision
    // call, so the `LOOP_DEFAULT_MAX_FIRES` safety cap could never trip
    // for a loop that survived past a single decision (every `fire_now`,
    // every scheduled tick, every restart). The wire-through makes
    // `decide_fire` budget-aware across the entire loop lifetime.
    let mut runtime = LoopRuntime::new(record.loop_id.clone(), invocation, policy)
        .with_fires_used(record.fires_used);
    match record.status.as_str() {
        "paused" => runtime.pause(),
        "deleted" => runtime.delete(),
        _ => {}
    }
    runtime
}

/// Convert a `LoopRuntime` denial into a wire-shaped autonomy error.
/// Bullet 1 / Bullet 2: every denial path carries `runtime_reason` so
/// the AppUI can distinguish runtime-policy denials from legacy
/// validation errors.
fn loop_runtime_denied_error(record: &AutonomyLoopRecord, reason: &DenyReason) -> RpcError {
    let kind = match reason {
        DenyReason::SlashCommandDenied => kinds::LOOP_SLASH_DENIED,
        DenyReason::Paused | DenyReason::Deleted | DenyReason::ExhaustedBudget => {
            kinds::LOOP_POLICY_DENIED
        }
        DenyReason::RuntimeBusy => kinds::LOOP_BUSY,
        DenyReason::InvalidInterval | DenyReason::MissingPolicy => kinds::LOOP_INVALID_INTERVAL,
        DenyReason::PromptResolutionFailed => kinds::LOOP_PROMPT_EMPTY,
        DenyReason::Failed(_) => kinds::LOOP_RUNTIME_UNAVAILABLE,
    };
    let mut error = autonomy_error(
        kind,
        format!("loop fire denied by runtime policy: {reason}"),
        Some(&record.session_id),
        Some(&record.profile_id),
        Some(("loop_id", record.loop_id.as_str())),
        true,
    );
    if let Some(Value::Object(data)) = error.data.as_mut() {
        data.insert("runtime_reason".into(), json!(reason.to_string()));
    }
    error
}

/// Convert a `WaitUntil` outcome into a wire-shaped rate-limited error.
fn loop_runtime_wait_error(record: &AutonomyLoopRecord, wait: &WaitUntil) -> RpcError {
    let detail = match wait {
        WaitUntil::At(_) => "loop is not yet due",
        WaitUntil::SelfPacedSignal => "self-paced loop is waiting for its next signal",
        WaitUntil::RuntimeIdle(_) => "runtime is not idle",
    };
    let mut error = autonomy_error(
        kinds::LOOP_BUSY,
        format!("loop fire deferred: {detail}"),
        Some(&record.session_id),
        Some(&record.profile_id),
        Some(("loop_id", record.loop_id.as_str())),
        true,
    );
    if let Some(Value::Object(data)) = error.data.as_mut() {
        data.insert("runtime_reason".into(), json!(detail));
    }
    error
}

/// Resolve a maintenance loop's prompt at fire time. Project doc takes
/// precedence over user doc, which takes precedence over the built-in
/// fallback. Bullet 3 of #977.
fn resolve_maintenance_prompt_at_fire_time() -> MaintenancePromptResolution {
    let project = std::fs::read_to_string(PROJECT_MAINTENANCE_PROMPT_PATH).ok();
    let user = expand_home_path(USER_MAINTENANCE_PROMPT_PATH)
        .and_then(|path| std::fs::read_to_string(path).ok());
    // `resolve_maintenance_prompt` only errors when *every* candidate is
    // empty; we always pass the built-in as the final fallback, so the
    // result is infallible here.
    resolve_maintenance_prompt(
        project.as_deref(),
        user.as_deref(),
        BUILT_IN_MAINTENANCE_PROMPT,
    )
    .unwrap_or_else(|_| MaintenancePromptResolution {
        source: MaintenancePromptSource::BuiltIn,
        prompt: BUILT_IN_MAINTENANCE_PROMPT.to_owned(),
    })
}

fn expand_home_path(input: &str) -> Option<PathBuf> {
    let suffix = input.strip_prefix("~/")?;
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(suffix))
}

fn maintenance_prompt_source_label(source: MaintenancePromptSource) -> &'static str {
    match source {
        MaintenancePromptSource::Project => "project",
        MaintenancePromptSource::User => "user",
        MaintenancePromptSource::BuiltIn => "built_in",
    }
}

/// Extract the `<<loop-next-in: N(s|m|h)>>` sentinel from a model
/// response. The sentinel lets a self-paced loop tell the runtime when
/// to fire next without round-tripping through a tool call. Returns
/// `None` when the sentinel is absent or malformed, so callers can fall
/// back to a configured default. Bullet 4 of #977.
pub(crate) fn parse_self_paced_next_delay(text: &str) -> Option<Duration> {
    let start = text.find("<<loop-next-in:")?;
    let after = &text[start + "<<loop-next-in:".len()..];
    let end = after.find(">>")?;
    let value = after[..end].trim();
    let (num, unit) = match value.chars().last()? {
        's' => (&value[..value.len() - 1], 1),
        'm' => (&value[..value.len() - 1], 60),
        'h' => (&value[..value.len() - 1], 3_600),
        digit if digit.is_ascii_digit() => (value, 1),
        _ => return None,
    };
    let seconds: u64 = num.trim().parse().ok()?;
    if seconds == 0 {
        return None;
    }
    Some(Duration::from_secs(seconds.saturating_mul(unit)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #1135 codex P2: serialize all cwd-mutating tests in this module
    /// (currently `maintenance_loop_resolves_prompt_at_fire_time_from_project_doc`
    /// and `scheduled_maintenance_fire_emits_resolved_prompt_source`).
    /// Rust runs tests in parallel by default; both tests `chdir` to
    /// their own tempdir and write `.octos/loop.md` there. Without a
    /// shared lock the two tests can overlap, with one resolving the
    /// OTHER's project doc and producing nondeterministic content
    /// failures. The lock is poisoning-safe — we recover from a poisoned
    /// lock so an earlier panic doesn't permanently disable the suite.
    static CWD_MUTATING_TEST_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> =
        std::sync::OnceLock::new();
    fn cwd_mutating_test_guard() -> std::sync::MutexGuard<'static, ()> {
        CWD_MUTATING_TEST_LOCK
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

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
                dispatch_policy: None,
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
    async fn native_specialist_dispatch_policy_accepts_sandbox_requirement() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let orchestrator = InProcessAgentOrchestrator::default();
        let tools = Arc::new(ToolRegistry::with_builtins_and_sandbox(
            dir.path(),
            octos_agent::create_sandbox(&octos_agent::SandboxConfig::default()),
        ));
        let memory = Arc::new(
            EpisodeStore::open(dir.path().join("memory"))
                .await
                .expect("memory store"),
        );
        let llm: Arc<dyn LlmProvider> = Arc::new(NativeMockProvider {
            content: Ok("native specialist respected sandbox policy".to_owned()),
        });
        let policy = Arc::new(octos_agent::DispatchPolicy {
            require_sandboxed: true,
            ..Default::default()
        });

        let result = orchestrator
            .run_native_specialist(NativeSpecialistLaunchRequest {
                agent_id: Some("native-policy-sandbox".to_owned()),
                parent_agent_id: Some("master".to_owned()),
                session_id: SessionKey::with_profile("tenant-a", "api", "native-policy"),
                profile_id: "tenant-a".to_owned(),
                role: "reviewer".to_owned(),
                nickname: "Native Policy".to_owned(),
                task: "review sandbox policy".to_owned(),
                cwd: dir.path().to_path_buf(),
                llm,
                memory,
                tools,
                system_prompt: Some("You are a focused reviewer.".to_owned()),
                agent_config: None,
                task_ledger_path: None,
                event_tx: None,
                dispatch_policy: Some(policy),
            })
            .await
            .expect("native specialist should satisfy sandbox dispatch policy");

        assert_eq!(result.status, "completed");
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
                dispatch_policy: None,
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

    /// #1121 codex P1 follow-up: task-backed records (where `task_id`
    /// differs from `agent_id` — e.g. native specialists with
    /// `native-*` agent ids carrying a separate task UUID) must still
    /// resolve through `get_agent` when a spec-conforming M13 client
    /// passes `task_id` from a `TaskListEntry.id`. Pins the lookup so
    /// task/artifact/list/read aliases reach the right agent record.
    #[test]
    fn get_agent_resolves_request_id_against_task_id_fallback() {
        let orchestrator = InProcessAgentOrchestrator::default();
        let task_id = TaskId::new();
        let mut agent = sample_agent("native-specialist-7", MAIN_PROFILE_ID);
        agent.task_id = Some(task_id.clone());
        let session_id = agent.session_id.clone();
        orchestrator
            .state()
            .agents
            .insert(agent.agent_id.clone(), agent);

        // Direct agent_id still works.
        let by_agent = orchestrator
            .list_agent_artifacts(AgentRequest {
                agent_id: "native-specialist-7".into(),
                session_id: Some(session_id.clone()),
                profile_id: MAIN_PROFILE_ID.into(),
            })
            .expect("agent_id lookup must work");
        assert_eq!(by_agent["agent_id"], json!("native-specialist-7"));

        // Task_id lookup also resolves to the same agent.
        let by_task = orchestrator
            .list_agent_artifacts(AgentRequest {
                agent_id: task_id.to_string(),
                session_id: Some(session_id),
                profile_id: MAIN_PROFILE_ID.into(),
            })
            .expect("task_id lookup must fall back through task_id field");
        assert_eq!(by_task["agent_id"], json!("native-specialist-7"));
    }

    /// #1121 codex P1 re-review #4 acceptance: the task_id fallback in
    /// `get_agent` MUST NOT fire when the caller omits `session_id`.
    /// Otherwise a same-profile attacker could put a known task UUID
    /// directly into `agent_id` (bypassing the params-layer
    /// `task_id`-requires-`session_id` check), the direct map lookup
    /// would miss, the fallback would resolve it, and
    /// `ensure_agent_control_scope` would collapse to profile-only
    /// matching — leaking artifacts across sessions.
    #[test]
    fn task_id_fallback_requires_session_id_to_prevent_same_profile_bypass() {
        let orchestrator = InProcessAgentOrchestrator::default();
        let task_id = TaskId::new();
        let mut agent = sample_agent("native-specialist-8", MAIN_PROFILE_ID);
        agent.task_id = Some(task_id.clone());
        orchestrator
            .state()
            .agents
            .insert(agent.agent_id.clone(), agent);

        // Pass the task UUID through `agent_id` WITHOUT `session_id` —
        // the legacy direct lookup misses, and the fallback must
        // refuse to resolve so `agent_not_found` is returned instead
        // of a profile-only scope match.
        let err = orchestrator
            .list_agent_artifacts(AgentRequest {
                agent_id: task_id.to_string(),
                session_id: None,
                profile_id: MAIN_PROFILE_ID.into(),
            })
            .expect_err("task_id-in-agent_id without session_id must be rejected");
        // The error data carries `kind` for the autonomy error envelope.
        let envelope_kind = err
            .data
            .as_ref()
            .and_then(|data| data.get("kind"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        assert_eq!(envelope_kind.as_deref(), Some(kinds::AGENT_NOT_FOUND));
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
            source: None,
            role: None,
            summary: None,
            artifact_count: None,
            runtime_policy_stamp: None,
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

    // ── #979 / M15-C2: GoalRuntime production wiring ────────────────────────

    /// Bullet 2: idle-only recurrence — after a goal turn fires, the
    /// orchestrator should re-queue another GoalContinue only when the
    /// runtime is still idle. A busy idle state must suppress the
    /// re-queue path.
    #[test]
    fn maybe_enqueue_goal_after_turn_respects_idle_gate() {
        let orchestrator = InProcessAgentOrchestrator::default();
        let session_id = SessionKey::with_profile("tenant-a", "api", "goal-recurrence");
        orchestrator
            .set_goal(GoalSetRequest {
                session_id: session_id.clone(),
                profile_id: "tenant-a".into(),
                objective: "advance one bounded step at a time".into(),
                status: Some("active".into()),
                token_budget: None,
                transition_actor: None,
            })
            .expect("set active goal");

        // Drain the initial fire so the queue is empty.
        let initial = orchestrator.drain_ready_continuations_for_session(
            &session_id,
            "tenant-a",
            MasterContinuationRuntimeState::idle(),
            usize::MAX,
        );
        assert_eq!(initial.len(), 1);
        assert_eq!(
            orchestrator.pending_continuation_count_for_test(),
            0,
            "queue should be empty after draining the initial goal continuation"
        );

        // Busy idle state → no re-queue.
        let busy_idle = GoalRuntimeIdleState::busy();
        assert!(!orchestrator.maybe_enqueue_goal_after_turn(&session_id, "tenant-a", busy_idle,));
        assert_eq!(orchestrator.pending_continuation_count_for_test(), 0);

        // User input pending → no re-queue.
        let pending_input = GoalRuntimeIdleState::idle().with_user_input_pending(true);
        assert!(!orchestrator.maybe_enqueue_goal_after_turn(
            &session_id,
            "tenant-a",
            pending_input,
        ));
        assert_eq!(orchestrator.pending_continuation_count_for_test(), 0);

        // Recording a turn advances `last_continued_at_ms` to now, so the
        // next fire is gated by the 30s min-delay policy. Force it back to
        // 0 so the policy permits an immediate re-queue.
        orchestrator.record_goal_turn(&session_id, "tenant-a", 0, 1);
        {
            if let Some(goal) = orchestrator.state().goals.get_mut(&session_id) {
                goal.last_continued_at_ms = 0;
            }
        }

        // Fully idle → re-queue succeeds.
        assert!(orchestrator.maybe_enqueue_goal_after_turn(
            &session_id,
            "tenant-a",
            GoalRuntimeIdleState::idle(),
        ));
        assert_eq!(orchestrator.pending_continuation_count_for_test(), 1);
    }

    /// #1129 codex P1 acceptance: after a goal turn, the
    /// `drain_ready_continuations_for_session` tick path MUST pick up
    /// the goal and re-queue once the 30s min-delay window has elapsed.
    /// The prior shape never enqueued a delayed continuation, so a
    /// goal that recorded a turn could only run again if the operator
    /// re-called `set_goal`. We simulate the elapsed delay by forcing
    /// `last_continued_at_ms` to the past and assert the next drain
    /// observes a queued GoalContinue.
    #[test]
    fn drain_path_picks_up_active_goal_after_min_delay() {
        let orchestrator = InProcessAgentOrchestrator::default();
        let session_id = SessionKey::with_profile("tenant-a", "api", "goal-recurrence");
        orchestrator
            .set_goal(GoalSetRequest {
                session_id: session_id.clone(),
                profile_id: "tenant-a".into(),
                objective: "keep going".into(),
                status: Some("active".into()),
                token_budget: None,
                transition_actor: None,
            })
            .expect("set active goal");
        // Consume the initial continuation queued by `set_goal`.
        let initial = orchestrator.drain_ready_continuations_for_session(
            &session_id,
            "tenant-a",
            MasterContinuationRuntimeState::idle(),
            usize::MAX,
        );
        assert_eq!(
            initial.len(),
            1,
            "set_goal must queue exactly one initial continuation"
        );

        // Record a turn (this stamps `last_continued_at_ms = now`).
        orchestrator.record_goal_turn(&session_id, "tenant-a", 0, 1);

        // Right after the turn, the drain path is still gated by the
        // 30s min-delay — no new continuation should be queued.
        let drained_immediately = orchestrator.drain_ready_continuations_for_session(
            &session_id,
            "tenant-a",
            MasterContinuationRuntimeState::idle(),
            usize::MAX,
        );
        assert!(
            drained_immediately.is_empty(),
            "min-delay gate must block immediate recurrence (got {drained_immediately:?})",
        );

        // Simulate the min-delay window having passed.
        if let Some(goal) = orchestrator.state().goals.get_mut(&session_id) {
            goal.last_continued_at_ms = now_ms() - GOAL_MIN_CONTINUATION_INTERVAL_MS - 1;
        }

        // Now the drain path MUST observe a queued GoalContinue.
        let drained_after_delay = orchestrator.drain_ready_continuations_for_session(
            &session_id,
            "tenant-a",
            MasterContinuationRuntimeState::idle(),
            usize::MAX,
        );
        assert_eq!(
            drained_after_delay.len(),
            1,
            "after min-delay elapses, active goal must re-queue (got {drained_after_delay:?})",
        );
        assert_eq!(
            drained_after_delay[0].reason,
            MasterContinuationReason::GoalContinue,
            "drained continuation must be a GoalContinue",
        );
    }

    /// #1129 codex P2 acceptance: `detect_goal_complete_sentinel` must
    /// only match when the sentinel appears at the END of the reply,
    /// not anywhere in the body. Otherwise an assistant message that
    /// merely mentions `goal_complete` in prose silently completes the
    /// goal and stops recurrence.
    #[test]
    fn detect_goal_complete_sentinel_requires_trailing_position() {
        // Trailing sentinels match — happy path preserved.
        assert!(detect_goal_complete_sentinel(
            "All steps done.\ngoal_complete"
        ));
        assert!(detect_goal_complete_sentinel("<goal:complete>"));
        assert!(detect_goal_complete_sentinel(
            "Summary…\n\n<goal:complete>\n"
        ));

        // Sentinel in the body but with other content after must NOT match.
        assert!(!detect_goal_complete_sentinel(
            "I noticed the sentinel is goal_complete, but I'll keep working on step 2."
        ));
        assert!(!detect_goal_complete_sentinel(
            "If you say <goal:complete>, recurrence stops. For now, advancing step 3."
        ));
        // Empty/whitespace inputs still produce no match.
        assert!(!detect_goal_complete_sentinel(""));
        assert!(!detect_goal_complete_sentinel("   \n\n"));
    }

    /// Bullet 1 / 2: min-delay gate — a fire that happened less than
    /// `GOAL_MIN_CONTINUATION_INTERVAL_MS` ago must NOT be allowed to
    /// re-queue immediately.
    #[test]
    fn maybe_enqueue_goal_after_turn_respects_min_delay() {
        let orchestrator = InProcessAgentOrchestrator::default();
        let session_id = SessionKey::with_profile("tenant-a", "api", "goal-min-delay");
        orchestrator
            .set_goal(GoalSetRequest {
                session_id: session_id.clone(),
                profile_id: "tenant-a".into(),
                objective: "respect min delay".into(),
                status: Some("active".into()),
                token_budget: None,
                transition_actor: None,
            })
            .expect("set active goal");
        let _ = orchestrator.drain_ready_continuations_for_session(
            &session_id,
            "tenant-a",
            MasterContinuationRuntimeState::idle(),
            usize::MAX,
        );
        orchestrator.record_goal_turn(&session_id, "tenant-a", 0, 1);

        // last_continued_at_ms is now wall-clock now → re-queue must be
        // denied by the min-delay gate.
        assert!(!orchestrator.maybe_enqueue_goal_after_turn(
            &session_id,
            "tenant-a",
            GoalRuntimeIdleState::idle(),
        ));
        assert_eq!(orchestrator.pending_continuation_count_for_test(), 0);
    }

    /// Bullet 3: budget exhaustion → enqueue a wrap-up turn AND
    /// transition the goal to `budget_limited`. Subsequent calls must
    /// be idempotent (no duplicate wrap-up).
    #[test]
    fn record_goal_turn_emits_wrap_up_on_budget_exhaustion() {
        let orchestrator = InProcessAgentOrchestrator::default();
        let session_id = SessionKey::with_profile("tenant-a", "api", "goal-budget");
        orchestrator
            .set_goal(GoalSetRequest {
                session_id: session_id.clone(),
                profile_id: "tenant-a".into(),
                objective: "exhaust the budget".into(),
                status: Some("active".into()),
                token_budget: Some(1_000),
                transition_actor: None,
            })
            .expect("set active goal");
        let _ = orchestrator.drain_ready_continuations_for_session(
            &session_id,
            "tenant-a",
            MasterContinuationRuntimeState::idle(),
            usize::MAX,
        );

        // Force tokens_used near the budget so the next recorded turn
        // exhausts it.
        orchestrator.force_goal_tokens_used_for_test(&session_id, 900);
        orchestrator.record_goal_turn(&session_id, "tenant-a", 200, 5);

        assert_eq!(
            orchestrator.goal_status_for_test(&session_id).as_deref(),
            Some("budget_limited"),
        );

        // The wrap-up turn must be queued separately from any prior
        // GoalContinue, and rides the new dedicated `GoalWrapUp`
        // reason (#1131) so the prompt renderer treats it as a
        // "summarize and stop" turn instead of a regular advance.
        let drained = orchestrator.drain_ready_continuations_for_session(
            &session_id,
            "tenant-a",
            MasterContinuationRuntimeState::idle(),
            usize::MAX,
        );
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].reason, MasterContinuationReason::GoalWrapUp);
        assert_eq!(
            drained[0].metadata.get("wrap_up").map(String::as_str),
            Some("true")
        );
        assert!(
            drained[0]
                .metadata
                .get("wrap_up_prompt")
                .map(|prompt| prompt.contains("exhausted"))
                .unwrap_or(false)
        );

        // Idempotency — a second turn record after exhaustion must NOT
        // emit a duplicate wrap-up.
        orchestrator.record_goal_turn(&session_id, "tenant-a", 100, 1);
        assert_eq!(orchestrator.pending_continuation_count_for_test(), 0);
    }

    /// #1141 — when an AppUI goal turn exhausts `token_budget`,
    /// `record_goal_turn` transitions the goal to `budget_limited` and
    /// enqueues a one-shot wrap-up continuation. For a goal-only AppUI
    /// session (no loop) the only way the scheduler can drain that
    /// wrap-up is for `due_loop_targets` to surface the session — but
    /// the active-goal scan gates on `status == "active"`, which
    /// `budget_limited` is not. The Option B fix sweeps the master
    /// continuation queue itself so any session with a pending
    /// continuation still gets a scheduler visit.
    #[test]
    fn due_loop_targets_includes_pending_wrap_up_for_budget_limited_goal() {
        let orchestrator = InProcessAgentOrchestrator::default();
        let session_id = SessionKey::with_profile("tenant-a", "api", "goal-budget-wrapup");
        orchestrator
            .set_goal(GoalSetRequest {
                session_id: session_id.clone(),
                profile_id: "tenant-a".into(),
                objective: "exhaust then expect wrap-up scheduling".into(),
                status: Some("active".into()),
                token_budget: Some(1_000),
                transition_actor: None,
            })
            .expect("set active goal");
        // Drain whatever the `set_goal` lifecycle queued so the only
        // pending continuation after exhaustion below is the wrap-up.
        let _ = orchestrator.drain_ready_continuations_for_session(
            &session_id,
            "tenant-a",
            MasterContinuationRuntimeState::idle(),
            usize::MAX,
        );

        // Force tokens_used near the budget and record a turn that
        // exhausts it — this transitions the goal to `budget_limited`
        // AND enqueues the wrap-up continuation.
        orchestrator.force_goal_tokens_used_for_test(&session_id, 900);
        orchestrator.record_goal_turn(&session_id, "tenant-a", 200, 5);
        assert_eq!(
            orchestrator.goal_status_for_test(&session_id).as_deref(),
            Some("budget_limited"),
            "post-exhaustion goal must be `budget_limited`, not `active`",
        );
        assert_eq!(
            orchestrator.pending_continuation_count_for_test(),
            1,
            "exhausting the budget must enqueue exactly one wrap-up continuation",
        );

        // Pre-fix this returned an empty vec: the goal-status gate
        // excludes `budget_limited` and there is no loop for this
        // session, so the wrap-up would have sat pending indefinitely.
        let targets = orchestrator.due_loop_targets(Some("tenant-a"), 8);
        assert!(
            targets.contains(&(session_id.clone(), "tenant-a".to_owned())),
            "due_loop_targets must surface a session with a pending wrap-up \
             continuation even when its goal is `budget_limited`, got {targets:?}",
        );

        // And the drain path for that session must actually return the
        // wrap-up — i.e. the scheduler visit translates into useful
        // work (not a no-op pickup).
        let drained = orchestrator.drain_ready_continuations_for_session(
            &session_id,
            "tenant-a",
            MasterContinuationRuntimeState::idle(),
            usize::MAX,
        );
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].reason, MasterContinuationReason::GoalWrapUp);
    }

    /// #1141 — `due_loop_targets` must respect `profile_filter` when
    /// sweeping the master continuation queue: a pending continuation
    /// for profile B must not surface under a query scoped to
    /// profile A.
    #[test]
    fn due_loop_targets_pending_sweep_respects_profile_filter() {
        let orchestrator = InProcessAgentOrchestrator::default();
        let session_a = SessionKey::with_profile("tenant-a", "api", "goal-a");
        let session_b = SessionKey::with_profile("tenant-b", "api", "goal-b");
        for (session, tenant) in [(&session_a, "tenant-a"), (&session_b, "tenant-b")] {
            orchestrator
                .set_goal(GoalSetRequest {
                    session_id: session.clone(),
                    profile_id: tenant.into(),
                    objective: "wrap-up profile gating".into(),
                    status: Some("active".into()),
                    token_budget: Some(1_000),
                    transition_actor: None,
                })
                .expect("set active goal");
            let _ = orchestrator.drain_ready_continuations_for_session(
                session,
                tenant,
                MasterContinuationRuntimeState::idle(),
                usize::MAX,
            );
            orchestrator.force_goal_tokens_used_for_test(session, 900);
            orchestrator.record_goal_turn(session, tenant, 200, 5);
        }

        let targets_a = orchestrator.due_loop_targets(Some("tenant-a"), 8);
        assert!(targets_a.contains(&(session_a.clone(), "tenant-a".to_owned())));
        assert!(
            !targets_a
                .iter()
                .any(|(_, profile_id)| profile_id == "tenant-b"),
            "profile_filter must exclude other tenants' pending wrap-ups, got {targets_a:?}",
        );
    }

    /// #1150 codex P2 follow-up to #1145: `pending_continuation_is_schedulable`
    /// gates which sessions `due_loop_targets` surfaces, but the drain
    /// path (`drain_ready_continuations_for_session` →
    /// `MasterContinuationScheduler::drain_ready_for_session`) pops by
    /// `(session_key, profile)` without re-applying the predicate. So
    /// if the same session's queue holds both a fresh schedulable
    /// continuation AND an older stale wrap-up whose owning goal has
    /// been replaced, the stale wrap-up (lower sequence → higher heap
    /// priority by FIFO tie-break) would drain first. This regression
    /// test pins drain-site filtering: only the fresh continuation is
    /// returned, and the stale wrap-up is dropped from the queue
    /// rather than silently re-queued for the next tick.
    #[test]
    fn drain_ready_continuations_filters_stale_at_drain_site_per_1150() {
        let orchestrator = InProcessAgentOrchestrator::default();
        let session_id = SessionKey::with_profile("tenant-a", "api", "drain-filter-stale");
        orchestrator
            .set_goal(GoalSetRequest {
                session_id: session_id.clone(),
                profile_id: "tenant-a".into(),
                objective: "fresh active goal".into(),
                status: Some("active".into()),
                token_budget: None,
                transition_actor: None,
            })
            .expect("set active goal");
        // Drain whatever the `set_goal` lifecycle queued so we control
        // the queue contents below precisely.
        let _ = orchestrator.drain_ready_continuations_for_session(
            &session_id,
            "tenant-a",
            MasterContinuationRuntimeState::idle(),
            usize::MAX,
        );

        // Hand-enqueue a stale legacy wrap-up (`GoalContinue` +
        // `wrap_up_prompt` metadata) carrying an OLD `goal_id` — the
        // pre-#1131 persisted shape. This is the item that must be
        // filtered out at drain time: the current `goal.goal_id`
        // differs from `item.goal_id`, so
        // `pending_continuation_is_schedulable` returns false.
        let current_goal_id = orchestrator
            .state()
            .goals
            .get(&session_id)
            .expect("goal exists")
            .goal_id
            .clone();
        let stale_goal_id = format!("{current_goal_id}-superseded");
        assert_ne!(stale_goal_id, current_goal_id);
        {
            let mut state = orchestrator.state();
            let stale = MasterContinuationRequest::new(
                "coding-autonomy-goal",
                session_id.to_string(),
                "tenant-a".to_owned(),
                MasterContinuationReason::GoalContinue,
                SystemTime::now(),
            )
            .with_goal_id(stale_goal_id.clone())
            .with_metadata(
                "wrap_up_prompt",
                "STALE: summarize a goal that no longer owns this session",
            );
            let outcome = enqueue_and_persist_continuation(&mut state, stale);
            assert!(
                outcome.queued().is_some(),
                "stale wrap-up must enqueue (fresh continuation not yet present)"
            );
        }

        // Now hand-enqueue a FRESH `GoalContinue` carrying the CURRENT
        // goal_id. This is what `enqueue_due_goal_continuations` would
        // emit if the min-delay had cleared, and is the item the
        // session was woken for. It must drain; the stale wrap-up
        // queued before it must not.
        {
            let mut state = orchestrator.state();
            let fresh = MasterContinuationRequest::new(
                "coding-autonomy-goal",
                session_id.to_string(),
                "tenant-a".to_owned(),
                MasterContinuationReason::GoalContinue,
                SystemTime::now(),
            )
            .with_goal_id(current_goal_id.clone())
            .with_metadata("objective", "fresh active goal".to_owned())
            .with_metadata("status", "active".to_owned());
            let outcome = enqueue_and_persist_continuation(&mut state, fresh);
            assert!(
                outcome.queued().is_some(),
                "fresh continuation must enqueue under a distinct dedupe key"
            );
        }

        // Sanity: both are queued before the drain.
        assert_eq!(
            orchestrator.pending_continuation_count_for_session_for_test(&session_id, "tenant-a"),
            2,
            "pre-drain queue must hold both stale wrap-up and fresh continuation",
        );

        let drained = orchestrator.drain_ready_continuations_for_session(
            &session_id,
            "tenant-a",
            MasterContinuationRuntimeState::idle(),
            usize::MAX,
        );

        // Only the fresh continuation may be returned. The stale
        // wrap-up — pointing at a superseded goal_id — must be
        // dropped, NOT silently surfaced to the caller.
        assert_eq!(
            drained.len(),
            1,
            "drain must return only the fresh continuation, got {drained:?}",
        );
        let returned = &drained[0];
        assert_eq!(returned.reason, MasterContinuationReason::GoalContinue);
        assert_eq!(
            returned.goal_id.as_ref().map(|id| id.as_str()),
            Some(current_goal_id.as_str()),
            "drain must return the fresh goal_id continuation, not the stale one",
        );
        assert!(
            !returned.metadata.contains_key("wrap_up_prompt"),
            "drain must not surface the stale wrap-up shape",
        );

        // And the stale item must be DROPPED from the queue entirely,
        // not held back for the next tick — matching the silent-skip
        // semantics of `due_loop_targets` / pending-sweep filtering.
        assert_eq!(
            orchestrator.pending_continuation_count_for_session_for_test(&session_id, "tenant-a"),
            0,
            "stale wrap-up must be dropped from the queue, not re-enqueued for next tick",
        );
    }

    /// #1160 codex P3 follow-up to #1150/#1159: the drain path pops up
    /// to `max_items` from the scheduler and THEN filters via
    /// `pending_continuation_is_schedulable`. Items dropped by that
    /// predicate have already consumed a scheduler slot, so a caller
    /// with `max_items=1` (production AppUI tick loop) that finds a
    /// stale wrap-up at the head of the heap returns ZERO items even
    /// though a fresh schedulable continuation is queued right behind
    /// it — the fresh item waits a full AppUI tick (~30s) before the
    /// next drain sees it. This regression test pins the refill
    /// behaviour: when a stale item is dropped, the drain must keep
    /// pulling from the scheduler until either `max_items` schedulable
    /// items are collected or the queue is empty for this session.
    #[test]
    fn drain_with_max_items_one_finds_fresh_when_stale_drains_first_per_1160() {
        let orchestrator = InProcessAgentOrchestrator::default();
        let session_id = SessionKey::with_profile("tenant-a", "api", "drain-refill-max-items");
        orchestrator
            .set_goal(GoalSetRequest {
                session_id: session_id.clone(),
                profile_id: "tenant-a".into(),
                objective: "fresh active goal".into(),
                status: Some("active".into()),
                token_budget: None,
                transition_actor: None,
            })
            .expect("set active goal");
        // Drain whatever the `set_goal` lifecycle queued so we control
        // the queue contents below precisely.
        let _ = orchestrator.drain_ready_continuations_for_session(
            &session_id,
            "tenant-a",
            MasterContinuationRuntimeState::idle(),
            usize::MAX,
        );

        let current_goal_id = orchestrator
            .state()
            .goals
            .get(&session_id)
            .expect("goal exists")
            .goal_id
            .clone();
        let stale_goal_id = format!("{current_goal_id}-superseded");
        assert_ne!(stale_goal_id, current_goal_id);

        // Hand-enqueue a stale wrap-up FIRST so it gets the lower
        // sequence and therefore higher heap priority under FIFO
        // tie-break — exactly the case that surfaces stale items at
        // slot 0 of a `max_items=1` drain.
        {
            let mut state = orchestrator.state();
            let stale = MasterContinuationRequest::new(
                "coding-autonomy-goal",
                session_id.to_string(),
                "tenant-a".to_owned(),
                MasterContinuationReason::GoalContinue,
                SystemTime::now(),
            )
            .with_goal_id(stale_goal_id.clone())
            .with_metadata(
                "wrap_up_prompt",
                "STALE: summarize a goal that no longer owns this session",
            );
            let outcome = enqueue_and_persist_continuation(&mut state, stale);
            assert!(
                outcome.queued().is_some(),
                "stale wrap-up must enqueue (fresh continuation not yet present)"
            );
        }
        // Now hand-enqueue the FRESH continuation behind the stale one.
        {
            let mut state = orchestrator.state();
            let fresh = MasterContinuationRequest::new(
                "coding-autonomy-goal",
                session_id.to_string(),
                "tenant-a".to_owned(),
                MasterContinuationReason::GoalContinue,
                SystemTime::now(),
            )
            .with_goal_id(current_goal_id.clone())
            .with_metadata("objective", "fresh active goal".to_owned())
            .with_metadata("status", "active".to_owned());
            let outcome = enqueue_and_persist_continuation(&mut state, fresh);
            assert!(
                outcome.queued().is_some(),
                "fresh continuation must enqueue under a distinct dedupe key"
            );
        }

        assert_eq!(
            orchestrator.pending_continuation_count_for_session_for_test(&session_id, "tenant-a"),
            2,
            "pre-drain queue must hold both stale wrap-up and fresh continuation",
        );

        // Production AppUI tick path passes max_items=1. The pre-#1160
        // code would pop the stale item, filter it out, and return an
        // empty vec — leaving the fresh item queued for the next tick.
        let drained = orchestrator.drain_ready_continuations_for_session(
            &session_id,
            "tenant-a",
            MasterContinuationRuntimeState::idle(),
            1,
        );

        assert_eq!(
            drained.len(),
            1,
            "drain with max_items=1 must refill past stale items and surface the fresh continuation, got {drained:?}",
        );
        let returned = &drained[0];
        assert_eq!(returned.reason, MasterContinuationReason::GoalContinue);
        assert_eq!(
            returned.goal_id.as_ref().map(|id| id.as_str()),
            Some(current_goal_id.as_str()),
            "drain must return the fresh goal_id continuation, not the stale one",
        );
        assert!(
            !returned.metadata.contains_key("wrap_up_prompt"),
            "drain must not surface the stale wrap-up shape",
        );

        // After the single-slot drain, the fresh continuation has been
        // taken AND the stale wrap-up has been dropped. Nothing should
        // remain queued for this session.
        assert_eq!(
            orchestrator.pending_continuation_count_for_session_for_test(&session_id, "tenant-a"),
            0,
            "fresh continuation must not still be queued after a max_items=1 drain that surfaced it",
        );
    }

    /// #1159 codex P2 follow-up: when a stale continuation is dropped
    /// at the drain site, the supervisor store MUST record a terminal
    /// event for it. Otherwise on restart, `configure_supervisor_store`
    /// reloads every non-completed queued continuation and the stale
    /// wrap-up resurrects — defeating the whole point of the #1150 fix.
    #[test]
    fn drain_time_stale_drop_persists_to_supervisor_store_per_1159() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let store_dir = dir.path().join("supervisor");
        let orchestrator = InProcessAgentOrchestrator::default();
        orchestrator
            .configure_supervisor_store(&store_dir)
            .expect("configure store");
        let session_id = SessionKey::with_profile("tenant-a", "api", "drain-drop-persists");
        orchestrator
            .set_goal(GoalSetRequest {
                session_id: session_id.clone(),
                profile_id: "tenant-a".into(),
                objective: "fresh active goal".into(),
                status: Some("active".into()),
                token_budget: None,
                transition_actor: None,
            })
            .expect("set active goal");
        // Drain the initial set_goal continuation AND mark it completed
        // in the store, so it doesn't get resurrected on restart and
        // pollute the post-restart pending count we're asserting below.
        let initial = orchestrator.drain_ready_continuations_for_session(
            &session_id,
            "tenant-a",
            MasterContinuationRuntimeState::idle(),
            usize::MAX,
        );
        for item in &initial {
            orchestrator.mark_continuation_started(item);
            orchestrator.mark_continuation_completed(item, Some("processed".into()));
        }

        let current_goal_id = orchestrator
            .state()
            .goals
            .get(&session_id)
            .expect("goal exists")
            .goal_id
            .clone();
        let stale_goal_id = format!("{current_goal_id}-superseded");
        // Hand-enqueue a stale wrap-up — same shape as #1150 test.
        {
            let mut state = orchestrator.state();
            let stale = MasterContinuationRequest::new(
                "coding-autonomy-goal",
                session_id.to_string(),
                "tenant-a".to_owned(),
                MasterContinuationReason::GoalContinue,
                SystemTime::now(),
            )
            .with_goal_id(stale_goal_id.clone())
            .with_metadata(
                "wrap_up_prompt",
                "STALE: summarize a goal that no longer owns this session",
            );
            enqueue_and_persist_continuation(&mut state, stale);
        }
        assert_eq!(
            orchestrator.pending_continuation_count_for_session_for_test(&session_id, "tenant-a"),
            1,
            "stale wrap-up must be queued before drain",
        );

        // Drain — this drops the stale wrap-up. Without the #1159 fix
        // we would only remove it from memory; with the fix the
        // supervisor store gets a ContinuationCompleted ledger entry.
        let _ = orchestrator.drain_ready_continuations_for_session(
            &session_id,
            "tenant-a",
            MasterContinuationRuntimeState::idle(),
            usize::MAX,
        );

        // In-memory queue is empty.
        assert_eq!(
            orchestrator.pending_continuation_count_for_session_for_test(&session_id, "tenant-a"),
            0,
            "in-memory queue must be empty after stale drop",
        );

        // Critical: a fresh orchestrator replaying the SAME store must
        // also see zero pending continuations. Pre-fix this asserts 1
        // because the stale wrap-up gets reloaded.
        let restarted = InProcessAgentOrchestrator::default();
        restarted
            .configure_supervisor_store(&store_dir)
            .expect("replay store");
        assert_eq!(
            restarted.pending_continuation_count_for_session_for_test(&session_id, "tenant-a"),
            0,
            "restart must not resurrect a stale wrap-up that was dropped at drain time",
        );
    }

    /// #1159 codex P2 follow-up: when a continuation is dropped at
    /// drain time because its goal is merely *paused* (not gone), we
    /// must NOT tombstone the ledger entry. The supervisor store
    /// ranks `Completed > Queued` in `upsert_continuation`, so a
    /// fresh Queued event arriving after the goal resumes would be
    /// silently ignored — losing a legitimate continuation.
    #[test]
    fn drain_time_drop_does_not_tombstone_paused_entries_per_1159() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let store_dir = dir.path().join("supervisor");
        let orchestrator = InProcessAgentOrchestrator::default();
        orchestrator
            .configure_supervisor_store(&store_dir)
            .expect("configure store");
        let session_id = SessionKey::with_profile("tenant-a", "api", "drain-drop-paused");
        orchestrator
            .set_goal(GoalSetRequest {
                session_id: session_id.clone(),
                profile_id: "tenant-a".into(),
                objective: "will be paused".into(),
                status: Some("active".into()),
                token_budget: None,
                transition_actor: None,
            })
            .expect("set goal");
        // Drain & complete the initial set_goal continuation so it
        // doesn't pollute later assertions.
        let initial = orchestrator.drain_ready_continuations_for_session(
            &session_id,
            "tenant-a",
            MasterContinuationRuntimeState::idle(),
            usize::MAX,
        );
        for item in &initial {
            orchestrator.mark_continuation_started(item);
            orchestrator.mark_continuation_completed(item, Some("processed".into()));
        }

        let goal_id = orchestrator
            .state()
            .goals
            .get(&session_id)
            .expect("goal exists")
            .goal_id
            .clone();
        // Hand-enqueue a GoalContinue against the SAME goal_id (so
        // it's not "superseded"), then pause the goal so the
        // predicate marks the entry unschedulable. Same goal_id is
        // the case that must NOT be tombstoned: resuming the goal
        // can re-queue the same stable dedupe_key.
        {
            let mut state = orchestrator.state();
            let request = MasterContinuationRequest::new(
                "coding-autonomy-goal",
                session_id.to_string(),
                "tenant-a".to_owned(),
                MasterContinuationReason::GoalContinue,
                SystemTime::now(),
            )
            .with_goal_id(goal_id.clone());
            enqueue_and_persist_continuation(&mut state, request);
            // Pause the goal — same goal_id stays in state.goals.
            state.goals.get_mut(&session_id).expect("goal").status = "paused".to_owned();
        }

        // Drain — the predicate marks this unschedulable (goal
        // paused), so the new fix drops it from in-memory queue
        // but must NOT write a ContinuationCompleted to the store.
        let _ = orchestrator.drain_ready_continuations_for_session(
            &session_id,
            "tenant-a",
            MasterContinuationRuntimeState::idle(),
            usize::MAX,
        );

        // Resume the goal and re-enqueue. The simulated "operator
        // un-paused" must succeed; the store must not have
        // tombstoned the dedupe_key.
        {
            let mut state = orchestrator.state();
            state.goals.get_mut(&session_id).expect("goal").status = "active".to_owned();
            let request = MasterContinuationRequest::new(
                "coding-autonomy-goal",
                session_id.to_string(),
                "tenant-a".to_owned(),
                MasterContinuationReason::GoalContinue,
                SystemTime::now(),
            )
            .with_goal_id(goal_id.clone());
            let outcome = enqueue_and_persist_continuation(&mut state, request);
            assert!(
                matches!(
                    outcome,
                    MasterContinuationEnqueueOutcome::Queued(_)
                        | MasterContinuationEnqueueOutcome::Duplicate { .. }
                ),
                "post-resume re-enqueue must succeed (queued or deduplicated against the in-memory entry), got {outcome:?}",
            );
        }

        // Restart and confirm the resumed continuation is still
        // there. Pre-fix, the Completed tombstone written during
        // the paused drain blocks the new Queued event from
        // sticking, so the restart sees 0 pending.
        let restarted = InProcessAgentOrchestrator::default();
        restarted
            .configure_supervisor_store(&store_dir)
            .expect("replay store");
        assert!(
            restarted.pending_continuation_count_for_session_for_test(&session_id, "tenant-a") >= 1,
            "paused-then-resumed continuation must survive restart (pre-fix this asserts 0)",
        );
    }

    /// #1159 codex P2 rev3 follow-up: `control_loop` does NOT remove
    /// a deleted loop from `state.loops` — it keeps the record with
    /// `status = "deleted"`. So a LoopFire queued before the delete
    /// is unschedulable, but a naive `state.loops.contains_key` check
    /// at the drain site would skip the tombstone (record is still
    /// "present"). Same dedupe_key never recurs after delete, so we
    /// MUST tombstone — otherwise restart resurrects the stale fire.
    #[test]
    fn drain_time_drop_tombstones_deleted_loop_fires_per_1159() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let store_dir = dir.path().join("supervisor");
        let orchestrator = InProcessAgentOrchestrator::default();
        orchestrator
            .configure_supervisor_store(&store_dir)
            .expect("configure store");
        let session_id = SessionKey::with_profile("tenant-a", "api", "loop-deleted-tombstone");
        let created = orchestrator
            .create_loop(LoopCreateRequest {
                session_id: session_id.clone(),
                profile_id: "tenant-a".into(),
                prompt: Some("hourly review".into()),
                command: None,
                interval_seconds: Some(60),
                mode: Some("fixed_interval".into()),
            })
            .expect("create loop");
        let loop_id = created["loop"]["loop_id"]
            .as_str()
            .expect("loop_id present")
            .to_owned();

        // Hand-enqueue a LoopFire while the loop is active, then
        // delete the loop.
        {
            let mut state = orchestrator.state();
            let request = MasterContinuationRequest::new(
                "coding-autonomy",
                session_id.to_string(),
                "tenant-a".to_owned(),
                MasterContinuationReason::LoopFire,
                SystemTime::now(),
            )
            .with_loop_id(loop_id.clone());
            enqueue_and_persist_continuation(&mut state, request);
        }
        orchestrator
            .control_loop(LoopControlRequest {
                session_id: Some(session_id.clone()),
                profile_id: "tenant-a".into(),
                loop_id: loop_id.clone(),
                kind: LoopControlKind::Delete,
            })
            .expect("delete loop");

        // Sanity: deleted loop is still in state.loops (per
        // `control_loop` semantics).
        assert!(
            orchestrator.state().loops.contains_key(loop_id.as_str()),
            "control_loop must not REMOVE deleted loops from state.loops",
        );

        // Drain — the predicate marks unschedulable (status =
        // "deleted"), the new fix tombstones because the loop is
        // gone for good.
        let _ = orchestrator.drain_ready_continuations_for_session(
            &session_id,
            "tenant-a",
            MasterContinuationRuntimeState::idle(),
            usize::MAX,
        );

        // Restart against the same store. The stale LoopFire must
        // not resurrect.
        let restarted = InProcessAgentOrchestrator::default();
        restarted
            .configure_supervisor_store(&store_dir)
            .expect("replay store");
        assert_eq!(
            restarted.pending_continuation_count_for_session_for_test(&session_id, "tenant-a"),
            0,
            "deleted-loop fire must be tombstoned at drain time, not resurrected on restart",
        );
    }

    /// #1145 codex P1 follow-up: the pending-queue sweep must FILTER
    /// stale continuations whose owning goal/loop has been
    /// paused/cleared/deleted. Otherwise pausing a goal mid-flight
    /// (with a queued GoalContinue) would silently wake the
    /// continuation on the next AppUI tick, despite the user's
    /// pause intent.
    #[test]
    fn due_loop_targets_pending_sweep_filters_paused_goal_continuations() {
        use crate::api::master_continuation_scheduler::MasterContinuationRequest;
        let orchestrator = InProcessAgentOrchestrator::default();
        let session_id = SessionKey::with_profile("tenant-a", "api", "paused-goal-stale");
        orchestrator
            .set_goal(GoalSetRequest {
                session_id: session_id.clone(),
                profile_id: "tenant-a".into(),
                objective: "will be paused mid-flight".into(),
                status: Some("active".into()),
                token_budget: None,
                transition_actor: None,
            })
            .expect("set active goal");
        // Drain the initial continuation queued by set_goal.
        let _ = orchestrator.drain_ready_continuations_for_session(
            &session_id,
            "tenant-a",
            MasterContinuationRuntimeState::idle(),
            usize::MAX,
        );
        // Hand-enqueue a GoalContinue (simulating the next scheduled
        // continuation before the user pauses).
        {
            let mut state = orchestrator.state();
            let request = MasterContinuationRequest::new(
                "coding-autonomy-goal",
                session_id.to_string(),
                "tenant-a".to_owned(),
                MasterContinuationReason::GoalContinue,
                SystemTime::now(),
            )
            .with_goal_id("stale-goal-id")
            .with_metadata("objective", "stale".to_owned());
            let _ = enqueue_and_persist_continuation(&mut state, request);
            // Pause the goal AFTER the continuation was queued.
            state
                .goals
                .get_mut(&session_id)
                .expect("goal exists")
                .status = "paused".to_owned();
        }
        // With the goal now paused, the scheduler MUST NOT include
        // this session even though it has a pending continuation.
        let targets = orchestrator.due_loop_targets(Some("tenant-a"), 8);
        assert!(
            !targets.iter().any(|(s, _)| s == &session_id),
            "paused goal with pending GoalContinue must not appear in due targets (got {targets:?})",
        );
    }

    /// #1131 — when the budget-exhaustion wrap-up turn is dispatched,
    /// the rendered prompt must contain the wrap-up directive
    /// verbatim (i.e. "Summarize the current state..."), NOT the
    /// regular "Advance the goal by one bounded step" template that
    /// the GoalContinue path emits. Otherwise the model keeps
    /// working instead of summarizing and stopping.
    #[test]
    fn goal_wrap_up_turn_uses_wrap_up_text_as_directive() {
        let orchestrator = InProcessAgentOrchestrator::default();
        let session_id = SessionKey::with_profile("tenant-a", "api", "goal-wrap-prompt");
        orchestrator
            .set_goal(GoalSetRequest {
                session_id: session_id.clone(),
                profile_id: "tenant-a".into(),
                objective: "exhaust then summarize".into(),
                status: Some("active".into()),
                token_budget: Some(1_000),
                transition_actor: None,
            })
            .expect("set active goal");
        // Drain any goal continuation that the `set_goal` lifecycle
        // may have queued so we only observe the wrap-up turn below.
        let _ = orchestrator.drain_ready_continuations_for_session(
            &session_id,
            "tenant-a",
            MasterContinuationRuntimeState::idle(),
            usize::MAX,
        );

        orchestrator.force_goal_tokens_used_for_test(&session_id, 900);
        orchestrator.record_goal_turn(&session_id, "tenant-a", 200, 5);

        let drained = orchestrator.drain_ready_continuations_for_session(
            &session_id,
            "tenant-a",
            MasterContinuationRuntimeState::idle(),
            usize::MAX,
        );
        assert_eq!(drained.len(), 1, "wrap-up must be the only queued turn");
        assert_eq!(drained[0].reason, MasterContinuationReason::GoalWrapUp);

        let rendered = master_continuation_prompt(&drained[0]);
        let wrap_up_directive = drained[0]
            .metadata
            .get("wrap_up_prompt")
            .cloned()
            .expect("wrap_up_prompt metadata must be present");
        assert!(
            rendered.contains(&wrap_up_directive),
            "rendered prompt must contain the wrap-up directive verbatim; rendered=\n{rendered}",
        );
        assert!(
            rendered.contains("Summarize the current state"),
            "rendered prompt must instruct the model to summarize; rendered=\n{rendered}",
        );
        assert!(
            !rendered.contains("Advance the goal by one bounded step"),
            "rendered prompt must NOT use the GoalContinue 'advance' template; rendered=\n{rendered}",
        );
    }

    /// #1139 codex P2 acceptance: a legacy wrap-up continuation
    /// (queued before #1131 with `GoalContinue` + `wrap_up_prompt`
    /// metadata, then restored after an upgrade/restart) MUST render
    /// as a wrap-up directive — NOT as the regular "Advance the goal"
    /// template. This pins the restore-time promotion in
    /// `master_continuation_prompt`.
    ///
    /// We can't ergonomically hand-build a `QueuedMasterContinuation`
    /// (private fields), so we drive the legacy-shaped enqueue
    /// directly: `MasterContinuationRequest::new(GoalContinue, …)`
    /// with a `wrap_up_prompt` metadata key — exactly what
    /// pre-#1131 code emitted on budget exhaustion.
    #[test]
    fn legacy_goal_continue_with_wrap_up_metadata_promotes_to_wrap_up() {
        use crate::api::master_continuation_scheduler::MasterContinuationRequest;

        let orchestrator = InProcessAgentOrchestrator::default();
        let session_id = SessionKey::with_profile("tenant-a", "api", "legacy-wrap-up");
        let mut state = orchestrator.state();
        // Hand-enqueue the legacy shape.
        let legacy = MasterContinuationRequest::new(
            "coding-autonomy-goal",
            session_id.to_string(),
            "tenant-a".to_owned(),
            MasterContinuationReason::GoalContinue,
            SystemTime::now(),
        )
        .with_goal_id("legacy-goal-id")
        .with_metadata(
            "wrap_up_prompt",
            "LEGACY DIRECTIVE: summarize what you've done and stop.",
        );
        let outcome = enqueue_and_persist_continuation(&mut state, legacy);
        let queued = outcome.queued().expect("legacy enqueue must succeed");
        let legacy_continuation = queued.clone();
        drop(state);

        let rendered = master_continuation_prompt(&legacy_continuation);
        assert!(
            rendered.contains("LEGACY DIRECTIVE: summarize what you've done and stop."),
            "legacy promotion must render the persisted wrap-up directive verbatim; rendered=\n{rendered}",
        );
        assert!(
            !rendered.contains("Advance the goal by one bounded step"),
            "legacy promotion must NOT fall through to the regular GoalContinue template; rendered=\n{rendered}",
        );
    }

    /// Bullet 3: a goal in `budget_limited` no longer fires the
    /// regular GoalContinue path even if min-delay/idle conditions
    /// are otherwise met.
    #[test]
    fn budget_limited_goal_blocks_further_continuations() {
        let orchestrator = InProcessAgentOrchestrator::default();
        let session_id = SessionKey::with_profile("tenant-a", "api", "goal-blocked");
        orchestrator
            .set_goal(GoalSetRequest {
                session_id: session_id.clone(),
                profile_id: "tenant-a".into(),
                objective: "test blocked".into(),
                status: Some("active".into()),
                token_budget: Some(500),
                transition_actor: None,
            })
            .expect("set active goal");
        let _ = orchestrator.drain_ready_continuations_for_session(
            &session_id,
            "tenant-a",
            MasterContinuationRuntimeState::idle(),
            usize::MAX,
        );
        orchestrator.force_goal_tokens_used_for_test(&session_id, 500);
        orchestrator.record_goal_turn(&session_id, "tenant-a", 0, 1);
        // Drain the wrap-up turn enqueued by the exhaustion above.
        let _ = orchestrator.drain_ready_continuations_for_session(
            &session_id,
            "tenant-a",
            MasterContinuationRuntimeState::idle(),
            usize::MAX,
        );

        // Even with the rate window cleared and last_continued_at_ms
        // forced into the past, the budget_limited status must block
        // further fires.
        if let Some(goal) = orchestrator.state().goals.get_mut(&session_id) {
            goal.last_continued_at_ms = 0;
        }
        assert!(!orchestrator.maybe_enqueue_goal_after_turn(
            &session_id,
            "tenant-a",
            GoalRuntimeIdleState::idle(),
        ));
        assert_eq!(orchestrator.pending_continuation_count_for_test(), 0);
    }

    /// Bullet 4: model-marks-complete — when an assistant turn ends
    /// with a known completion sentinel, the goal transitions to
    /// `complete` and recurrence stops.
    #[test]
    fn maybe_complete_goal_from_model_recognizes_sentinels() {
        let orchestrator = InProcessAgentOrchestrator::default();
        let session_id = SessionKey::with_profile("tenant-a", "api", "goal-complete");
        orchestrator
            .set_goal(GoalSetRequest {
                session_id: session_id.clone(),
                profile_id: "tenant-a".into(),
                objective: "finish up".into(),
                status: Some("active".into()),
                token_budget: None,
                transition_actor: None,
            })
            .expect("set active goal");
        let _ = orchestrator.drain_ready_continuations_for_session(
            &session_id,
            "tenant-a",
            MasterContinuationRuntimeState::idle(),
            usize::MAX,
        );

        // Plain content → no transition.
        assert!(!orchestrator.maybe_complete_goal_from_model(
            &session_id,
            "tenant-a",
            "still working on it",
        ));
        assert_eq!(
            orchestrator.goal_status_for_test(&session_id).as_deref(),
            Some("active"),
        );

        // Sentinel content → transition to `complete`.
        assert!(orchestrator.maybe_complete_goal_from_model(
            &session_id,
            "tenant-a",
            "All done. <goal:complete>",
        ));
        assert_eq!(
            orchestrator.goal_status_for_test(&session_id).as_deref(),
            Some("complete"),
        );

        // Subsequent re-queue attempts must fail because the goal is
        // no longer active.
        if let Some(goal) = orchestrator.state().goals.get_mut(&session_id) {
            goal.last_continued_at_ms = 0;
        }
        assert!(!orchestrator.maybe_enqueue_goal_after_turn(
            &session_id,
            "tenant-a",
            GoalRuntimeIdleState::idle(),
        ));
    }

    /// `detect_goal_complete_sentinel` covers all canonical sentinels
    /// case-insensitively and ignores plain content.
    #[test]
    fn goal_complete_sentinel_detector_is_case_insensitive() {
        assert!(detect_goal_complete_sentinel("<goal:complete>"));
        assert!(detect_goal_complete_sentinel("<GOAL:COMPLETE>"));
        assert!(detect_goal_complete_sentinel("[goal:complete]"));
        assert!(detect_goal_complete_sentinel(
            "Wrap-up notes…\n\nGOAL-COMPLETE"
        ));
        assert!(detect_goal_complete_sentinel("done -- goal_complete"));
        assert!(!detect_goal_complete_sentinel("still goal-complementary"));
        assert!(!detect_goal_complete_sentinel(
            "active progress, nothing yet"
        ));
        assert!(!detect_goal_complete_sentinel(""));
    }

    /// `set_goal` should populate the new policy fields with sensible
    /// defaults and not regress the prior persistence shape.
    #[test]
    fn set_goal_initializes_policy_fields() {
        let orchestrator = InProcessAgentOrchestrator::default();
        let session_id = SessionKey::with_profile("tenant-a", "api", "goal-init");
        orchestrator
            .set_goal(GoalSetRequest {
                session_id: session_id.clone(),
                profile_id: "tenant-a".into(),
                objective: "initialize".into(),
                status: Some("active".into()),
                token_budget: Some(10_000),
                transition_actor: None,
            })
            .expect("set active goal");

        let state = orchestrator.state();
        let goal = state.goals.get(&session_id).expect("goal must exist");
        assert_eq!(goal.continuations_used, 0);
        assert_eq!(goal.last_continued_at_ms, 0);
        assert_eq!(goal.rate_window_count, 0);
        assert!(!goal.wrap_up_emitted);
        assert!(goal.rate_window_start_ms > 0, "window start initialized");
    }

    /// Re-activating a paused goal must clear `wrap_up_emitted` so a
    /// re-budgeted goal can fire a fresh wrap-up when it next
    /// exhausts.
    #[test]
    fn reactivating_goal_resets_wrap_up_emitted_flag() {
        let orchestrator = InProcessAgentOrchestrator::default();
        let session_id = SessionKey::with_profile("tenant-a", "api", "goal-reactivate");
        orchestrator
            .set_goal(GoalSetRequest {
                session_id: session_id.clone(),
                profile_id: "tenant-a".into(),
                objective: "test".into(),
                status: Some("active".into()),
                token_budget: Some(500),
                transition_actor: None,
            })
            .expect("set active goal");
        let _ = orchestrator.drain_ready_continuations_for_session(
            &session_id,
            "tenant-a",
            MasterContinuationRuntimeState::idle(),
            usize::MAX,
        );
        orchestrator.force_goal_tokens_used_for_test(&session_id, 500);
        orchestrator.record_goal_turn(&session_id, "tenant-a", 0, 1);
        assert_eq!(
            orchestrator.goal_status_for_test(&session_id).as_deref(),
            Some("budget_limited"),
        );

        // Drain the wrap-up so the queue is empty.
        let _ = orchestrator.drain_ready_continuations_for_session(
            &session_id,
            "tenant-a",
            MasterContinuationRuntimeState::idle(),
            usize::MAX,
        );

        // Re-activate by setting a larger budget and flipping to active.
        orchestrator
            .set_goal(GoalSetRequest {
                session_id: session_id.clone(),
                profile_id: "tenant-a".into(),
                objective: "test".into(),
                status: Some("active".into()),
                token_budget: Some(50_000),
                transition_actor: None,
            })
            .expect("reactivate");

        let state = orchestrator.state();
        let goal = state.goals.get(&session_id).expect("goal must exist");
        assert!(
            !goal.wrap_up_emitted,
            "wrap_up_emitted must reset on re-activation"
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

    /// #1128 codex P1 acceptance: self-paced loops whose `next_run_at_ms`
    /// is in the past MUST also be picked up by `due_loop_targets` /
    /// `enqueue_due_loop_continuations`. The prior shape filtered on
    /// `mode != "fixed_interval"` so the only way to fire a self-paced
    /// loop was `fire_now` — the model's `<<loop-next-in: ...>>` hint
    /// was stamped onto the record but never honoured automatically.
    #[test]
    fn due_self_paced_loop_queues_master_continuation() {
        let orchestrator = InProcessAgentOrchestrator::default();
        let session_id = SessionKey::with_profile("tenant-a", "api", "self-paced-due");
        let created = orchestrator
            .create_loop(LoopCreateRequest {
                session_id: session_id.clone(),
                profile_id: "tenant-a".into(),
                prompt: Some("ponder the codebase".into()),
                command: None,
                interval_seconds: None,
                mode: Some("self_paced".into()),
            })
            .expect("create self-paced loop");
        let loop_id = created["loop_id"].as_str().expect("loop id").to_owned();

        // Simulate the post-fire stamp from `apply_self_paced_response`:
        // record a past `next_run_at_ms` as if the model had asked for
        // a near-zero delay.
        {
            let mut state = orchestrator.state();
            let loop_record = state.loops.get_mut(&loop_id).expect("loop record");
            loop_record.next_run_at_ms = Some(now_ms() - 1);
        }

        let targets = orchestrator.due_loop_targets(Some("tenant-a"), 8);
        assert!(
            targets.contains(&(session_id.clone(), "tenant-a".to_owned())),
            "due_loop_targets must include the self-paced loop, got {targets:?}",
        );

        let ticked = orchestrator.tick_due_loops_for_session(
            &session_id,
            "tenant-a",
            MasterContinuationRuntimeState::idle(),
        );
        assert_eq!(ticked, 1, "self-paced loop must enqueue a continuation");

        // After firing, the self-paced loop's next_run_at_ms must be
        // cleared so the scheduler does not pick it up on every tick
        // until `apply_self_paced_response` stamps a fresh delay.
        let state = orchestrator.state();
        let loop_record = state.loops.get(&loop_id).expect("loop record");
        assert!(
            loop_record.next_run_at_ms.is_none(),
            "self-paced loop must clear next_run_at_ms after firing (got {:?}), so scheduler waits for the model reply",
            loop_record.next_run_at_ms,
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

    // ---------------- #991 / M15-B trait extension tests ----------------

    /// #991 / M15-B — a fresh orchestrator type that does NOT override
    /// the new trait methods MUST return the `UNSUPPORTED_CAPABILITY`
    /// shape so wire-level callers can detect the "method declared,
    /// runtime not wired" condition without panicking. This guards
    /// against accidental method-not-found regressions when the trait
    /// surface grows but a specific orchestrator hasn't been updated.
    struct UnimplementedOrchestrator;

    impl AgentOrchestrator for UnimplementedOrchestrator {
        fn list_agents(&self, _: AgentListRequest) -> Result<Value, RpcError> {
            Ok(json!({}))
        }
        fn read_agent_status(&self, _: AgentRequest) -> Result<Value, RpcError> {
            Ok(json!({}))
        }
        fn read_agent_output(&self, _: AgentOutputRequest) -> Result<Value, RpcError> {
            Ok(json!({}))
        }
        fn list_agent_artifacts(&self, _: AgentRequest) -> Result<Value, RpcError> {
            Ok(json!({}))
        }
        fn read_agent_artifact(&self, _: AgentArtifactReadRequest) -> Result<Value, RpcError> {
            Ok(json!({}))
        }
        fn interrupt_agent(&self, _: AgentRequest) -> Result<Value, RpcError> {
            Ok(json!({}))
        }
        fn close_agent(&self, _: AgentRequest) -> Result<Value, RpcError> {
            Ok(json!({}))
        }
        fn get_goal(&self, _: GoalSessionRequest) -> Result<Value, RpcError> {
            Ok(json!({}))
        }
        fn set_goal(&self, _: GoalSetRequest) -> Result<Value, RpcError> {
            Ok(json!({}))
        }
        fn clear_goal(&self, _: GoalSessionRequest) -> Result<Value, RpcError> {
            Ok(json!({}))
        }
        fn create_loop(&self, _: LoopCreateRequest) -> Result<Value, RpcError> {
            Ok(json!({}))
        }
        fn list_loops(&self, _: LoopListRequest) -> Result<Value, RpcError> {
            Ok(json!({}))
        }
        fn control_loop(&self, _: LoopControlRequest) -> Result<Value, RpcError> {
            Ok(json!({}))
        }
        // Intentionally do NOT override spawn_agent / send_input /
        // wait_agent / resume_agent — those should fall through to
        // the default impl and return the UNSUPPORTED_CAPABILITY
        // shape.
    }

    fn default_session(suffix: &str) -> SessionKey {
        SessionKey::with_profile("tenant-a", "api", suffix)
    }

    #[test]
    fn trait_default_spawn_agent_returns_unsupported_capability() {
        let orchestrator = UnimplementedOrchestrator;
        let session_id = default_session("default-spawn");
        let err = orchestrator
            .spawn_agent(SpawnAgentRequest {
                session_id: session_id.clone(),
                profile_id: "tenant-a".into(),
                parent_agent_id: None,
                backend_kind: "native".into(),
                role: "reviewer".into(),
                nickname: "Default".into(),
                task: "do work".into(),
                cwd: None,
            })
            .expect_err("default spawn_agent must error");
        assert_eq!(err.code, rpc_error_codes::UNSUPPORTED_CAPABILITY);
        let data = err.data.expect("default error carries data");
        assert_eq!(data["method"], json!("agent/spawn"));
        assert_eq!(data["kind"], json!("agent_method_not_supported"));
    }

    #[test]
    fn trait_default_send_input_returns_unsupported_capability() {
        let orchestrator = UnimplementedOrchestrator;
        let session_id = default_session("default-send-input");
        let err = orchestrator
            .send_input(AgentInputRequest {
                agent_id: "agent-x".into(),
                session_id: Some(session_id),
                profile_id: "tenant-a".into(),
                input: "hello".into(),
            })
            .expect_err("default send_input must error");
        assert_eq!(err.code, rpc_error_codes::UNSUPPORTED_CAPABILITY);
        let data = err.data.expect("default error carries data");
        assert_eq!(data["method"], json!("agent/send_input"));
    }

    #[test]
    fn trait_default_wait_agent_returns_unsupported_capability() {
        let orchestrator = UnimplementedOrchestrator;
        let session_id = default_session("default-wait");
        let err = orchestrator
            .wait_agent(AgentRequest {
                agent_id: "agent-x".into(),
                session_id: Some(session_id),
                profile_id: "tenant-a".into(),
            })
            .expect_err("default wait_agent must error");
        assert_eq!(err.code, rpc_error_codes::UNSUPPORTED_CAPABILITY);
        let data = err.data.expect("default error carries data");
        assert_eq!(data["method"], json!("agent/wait"));
    }

    #[test]
    fn trait_default_resume_agent_returns_unsupported_capability() {
        let orchestrator = UnimplementedOrchestrator;
        let session_id = default_session("default-resume");
        let err = orchestrator
            .resume_agent(ResumeAgentRequest {
                agent_id: "agent-x".into(),
                session_id: Some(session_id),
                profile_id: "tenant-a".into(),
            })
            .expect_err("default resume_agent must error");
        assert_eq!(err.code, rpc_error_codes::UNSUPPORTED_CAPABILITY);
        let data = err.data.expect("default error carries data");
        assert_eq!(data["method"], json!("agent/resume"));
    }

    #[test]
    fn in_process_spawn_agent_registers_running_record() {
        let orchestrator = InProcessAgentOrchestrator::default();
        let session_id = default_session("spawn-success");
        let result = orchestrator
            .spawn_agent(SpawnAgentRequest {
                session_id: session_id.clone(),
                profile_id: "tenant-a".into(),
                parent_agent_id: Some("master".into()),
                backend_kind: "native".into(),
                role: "reviewer".into(),
                nickname: "Spawned".into(),
                task: "audit changes".into(),
                cwd: None,
            })
            .expect("spawn ok");
        assert_eq!(result["ok"], json!(true));
        let agent_id = result["agent_id"].as_str().expect("agent_id").to_owned();
        assert!(agent_id.starts_with("native-"));
        let status = orchestrator
            .read_agent_status(AgentRequest {
                agent_id: agent_id.clone(),
                session_id: Some(session_id),
                profile_id: "tenant-a".into(),
            })
            .expect("status");
        assert_eq!(status["agent"]["status"], json!("running"));
        assert_eq!(status["agent"]["last_task"], json!("audit changes"));
        assert_eq!(status["agent"]["backend_kind"], json!("native"));
    }

    #[test]
    fn in_process_spawn_agent_rejects_empty_backend_kind() {
        let orchestrator = InProcessAgentOrchestrator::default();
        let session_id = default_session("spawn-reject");
        let err = orchestrator
            .spawn_agent(SpawnAgentRequest {
                session_id,
                profile_id: "tenant-a".into(),
                parent_agent_id: None,
                backend_kind: "  ".into(),
                role: "reviewer".into(),
                nickname: "Bad".into(),
                task: "x".into(),
                cwd: None,
            })
            .expect_err("empty backend_kind is rejected");
        assert_eq!(
            err.data.expect("error data")["kind"],
            json!(kinds::AGENT_CONTROL_UNAVAILABLE)
        );
    }

    #[test]
    fn in_process_send_input_updates_last_task_for_running_agent() {
        let orchestrator = InProcessAgentOrchestrator::default();
        let agent = sample_agent("agent-input", "tenant-a");
        let session_id = agent.session_id.clone();
        orchestrator
            .state()
            .agents
            .insert(agent.agent_id.clone(), agent);
        let result = orchestrator
            .send_input(AgentInputRequest {
                agent_id: "agent-input".into(),
                session_id: Some(session_id.clone()),
                profile_id: "tenant-a".into(),
                input: "next instruction".into(),
            })
            .expect("send_input ok");
        assert_eq!(result["delivered"], json!(true));
        assert_eq!(result["agent"]["last_task"], json!("next instruction"));
    }

    #[test]
    fn in_process_send_input_rejects_empty_payload() {
        let orchestrator = InProcessAgentOrchestrator::default();
        let agent = sample_agent("agent-empty-input", "tenant-a");
        let session_id = agent.session_id.clone();
        orchestrator
            .state()
            .agents
            .insert(agent.agent_id.clone(), agent);
        let err = orchestrator
            .send_input(AgentInputRequest {
                agent_id: "agent-empty-input".into(),
                session_id: Some(session_id),
                profile_id: "tenant-a".into(),
                input: "   ".into(),
            })
            .expect_err("empty input rejected");
        assert_eq!(
            err.data.expect("error data")["kind"],
            json!(kinds::AGENT_CONTROL_UNAVAILABLE)
        );
    }

    // ───── M15-D2/D3 LoopRuntime wiring (#977) ─────
    //
    // These tests pin the production fire path to the `LoopRuntime`
    // primitives in `goal_loop_runtime.rs`. They cover acceptance bullets
    // 1–4: runtime-consumed gating, slash re-auth on every fire,
    // maintenance prompt resolution at fire time, and self-paced next-delay
    // hint parsing. Bullet 5 (live soak) tracked separately.

    #[test]
    fn fire_now_consults_loop_runtime_and_denies_paused_loop() {
        let orchestrator = InProcessAgentOrchestrator::default();
        let session_id = SessionKey::with_profile("tenant-a", "api", "loop-runtime-paused");
        let created = orchestrator
            .create_loop(LoopCreateRequest {
                session_id: session_id.clone(),
                profile_id: "tenant-a".into(),
                prompt: Some("check runtime gating".into()),
                command: None,
                interval_seconds: Some(60),
                mode: Some("fixed_interval".into()),
            })
            .expect("create loop");
        let loop_id = created["loop_id"].as_str().expect("loop id").to_owned();

        orchestrator
            .control_loop(LoopControlRequest {
                loop_id: loop_id.clone(),
                session_id: Some(session_id.clone()),
                profile_id: "tenant-a".into(),
                kind: LoopControlKind::Pause,
            })
            .expect("pause loop");

        let denied = orchestrator
            .control_loop(LoopControlRequest {
                loop_id,
                session_id: Some(session_id.clone()),
                profile_id: "tenant-a".into(),
                kind: LoopControlKind::FireNow,
            })
            .expect_err("paused loop must be denied");
        let data = denied.data.expect("error data");
        assert_eq!(data["kind"], json!(kinds::LOOP_POLICY_DENIED));
        let runtime_reason = data
            .get("runtime_reason")
            .and_then(Value::as_str)
            .expect("loop runtime denial must carry runtime_reason for #977");
        assert_eq!(runtime_reason, "runtime paused");
    }

    #[test]
    fn scheduled_fire_denies_slash_loop_without_reauth_but_fire_now_allows_it() {
        let orchestrator = InProcessAgentOrchestrator::default();
        let session_id = SessionKey::with_profile("tenant-a", "api", "loop-slash-reauth");
        // A slash-command loop: prompt stored as "/status".
        let created = orchestrator
            .create_loop(LoopCreateRequest {
                session_id: session_id.clone(),
                profile_id: "tenant-a".into(),
                prompt: None,
                command: Some("/status".into()),
                interval_seconds: Some(60),
                mode: Some("fixed_interval".into()),
            })
            .expect("create slash loop");
        let loop_id = created["loop_id"].as_str().expect("loop id").to_owned();

        // Make the loop due so the scheduled-tick path is exercised.
        orchestrator.force_loop_due_for_test(&loop_id);
        let ticked = orchestrator.tick_due_loops_for_session(
            &session_id,
            "tenant-a",
            MasterContinuationRuntimeState::idle(),
        );
        assert_eq!(
            ticked, 0,
            "scheduled-due slash loop without fresh user authorization must be skipped"
        );
        assert_eq!(
            orchestrator.pending_continuation_count_for_session_for_test(&session_id, "tenant-a"),
            0,
            "no continuations should have been enqueued"
        );

        // FireNow is user-initiated, so it must succeed (authorized_now=true).
        let fired = orchestrator
            .control_loop(LoopControlRequest {
                loop_id,
                session_id: Some(session_id.clone()),
                profile_id: "tenant-a".into(),
                kind: LoopControlKind::FireNow,
            })
            .expect("fire_now on slash loop must be authorized by the user gesture");
        assert_eq!(fired["status"], json!("queued"));
        assert_eq!(
            orchestrator.pending_continuation_count_for_session_for_test(&session_id, "tenant-a"),
            1
        );
    }

    /// #1130 — pin the persisted-`fires_used` enforcement.
    ///
    /// Before #1130, `loop_runtime_view` rebuilt a fresh `LoopRuntime`
    /// on every decision call and `fires_used` never round-tripped
    /// through the loop record, so the runtime's `LOOP_DEFAULT_MAX_FIRES`
    /// budget gate could never trip — any loop that burned through the
    /// budget kept firing forever. This test directly stages a loop at
    /// `LOOP_DEFAULT_MAX_FIRES - 1` consumed fires, fires it once (must
    /// succeed, bumps the counter to exactly the cap), then attempts a
    /// second fire which the runtime must deny with
    /// `LoopFireDecision::Exhausted` → `LOOP_POLICY_DENIED` on the wire.
    #[test]
    fn loop_fires_used_persists_and_caps_at_max() {
        let orchestrator = InProcessAgentOrchestrator::default();
        let session_id = SessionKey::with_profile("tenant-a", "api", "loop-fires-cap");
        let created = orchestrator
            .create_loop(LoopCreateRequest {
                session_id: session_id.clone(),
                profile_id: "tenant-a".into(),
                prompt: Some("periodic check".into()),
                command: None,
                interval_seconds: Some(60),
                mode: Some("fixed_interval".into()),
            })
            .expect("create loop");
        let loop_id = created["loop_id"].as_str().expect("loop id").to_owned();

        // Stage the loop one fire shy of the cap. The next fire must
        // succeed (consumes the last unit of budget); the one after must
        // be rejected with the runtime's exhausted-budget denial.
        {
            let mut state = orchestrator.state();
            let loop_record = state
                .loops
                .get_mut(&loop_id)
                .expect("loop record present after create");
            loop_record.fires_used = LOOP_DEFAULT_MAX_FIRES - 1;
        }

        let fired = orchestrator
            .control_loop(LoopControlRequest {
                loop_id: loop_id.clone(),
                session_id: Some(session_id.clone()),
                profile_id: "tenant-a".into(),
                kind: LoopControlKind::FireNow,
            })
            .expect("final fire under the cap must succeed");
        assert_eq!(fired["status"], json!("queued"));
        // After firing, the persisted counter must sit at exactly the
        // cap — `loop_runtime_view` will read this back on the next
        // decision call.
        assert_eq!(
            orchestrator
                .state()
                .loops
                .get(&loop_id)
                .expect("loop record post-fire")
                .fires_used,
            LOOP_DEFAULT_MAX_FIRES,
            "fires_used must be incremented and persisted on successful fire",
        );

        // The follow-up fire crosses the cap. `decide_fire` must return
        // `LoopFireDecision::Exhausted` → wire-level
        // `kinds::LOOP_POLICY_DENIED` with the runtime's
        // `exhausted budget` reason carried in the data payload.
        let denied = orchestrator
            .control_loop(LoopControlRequest {
                loop_id,
                session_id: Some(session_id.clone()),
                profile_id: "tenant-a".into(),
                kind: LoopControlKind::FireNow,
            })
            .expect_err("fires_used at cap must deny further fires");
        let data = denied.data.expect("error data");
        assert_eq!(data["kind"], json!(kinds::LOOP_POLICY_DENIED));
        let runtime_reason = data
            .get("runtime_reason")
            .and_then(Value::as_str)
            .expect("loop runtime denial must carry runtime_reason");
        assert_eq!(runtime_reason, "exhausted budget");
    }

    #[test]
    fn in_process_send_input_rejects_terminal_agent() {
        let orchestrator = InProcessAgentOrchestrator::default();
        let mut agent = sample_agent("agent-terminal", "tenant-a");
        agent.status = "completed".into();
        let session_id = agent.session_id.clone();
        orchestrator
            .state()
            .agents
            .insert(agent.agent_id.clone(), agent);
        let err = orchestrator
            .send_input(AgentInputRequest {
                agent_id: "agent-terminal".into(),
                session_id: Some(session_id),
                profile_id: "tenant-a".into(),
                input: "too late".into(),
            })
            .expect_err("terminal agent rejected");
        assert_eq!(
            err.data.expect("error data")["kind"],
            json!(kinds::AGENT_CONTROL_UNAVAILABLE)
        );
    }

    #[test]
    fn in_process_wait_agent_returns_terminal_flag() {
        let orchestrator = InProcessAgentOrchestrator::default();
        let running = sample_agent("agent-running", "tenant-a");
        let mut completed = sample_agent("agent-done", "tenant-a");
        completed.status = "completed".into();
        let session_id = running.session_id.clone();
        orchestrator
            .state()
            .agents
            .insert(running.agent_id.clone(), running);
        orchestrator
            .state()
            .agents
            .insert(completed.agent_id.clone(), completed);

        let running_result = orchestrator
            .wait_agent(AgentRequest {
                agent_id: "agent-running".into(),
                session_id: Some(session_id.clone()),
                profile_id: "tenant-a".into(),
            })
            .expect("wait running");
        assert_eq!(running_result["terminal"], json!(false));
        assert_eq!(running_result["status"], json!("running"));

        let done_result = orchestrator
            .wait_agent(AgentRequest {
                agent_id: "agent-done".into(),
                session_id: Some(session_id),
                profile_id: "tenant-a".into(),
            })
            .expect("wait done");
        assert_eq!(done_result["terminal"], json!(true));
        assert_eq!(done_result["status"], json!("completed"));
    }

    #[test]
    fn in_process_resume_agent_returns_record_and_rejects_terminal() {
        let orchestrator = InProcessAgentOrchestrator::default();
        let running = sample_agent("agent-resume", "tenant-a");
        let mut closed = sample_agent("agent-resume-closed", "tenant-a");
        closed.status = "closed".into();
        let session_id = running.session_id.clone();
        orchestrator
            .state()
            .agents
            .insert(running.agent_id.clone(), running);
        orchestrator
            .state()
            .agents
            .insert(closed.agent_id.clone(), closed);

        let resumed = orchestrator
            .resume_agent(ResumeAgentRequest {
                agent_id: "agent-resume".into(),
                session_id: Some(session_id.clone()),
                profile_id: "tenant-a".into(),
            })
            .expect("resume running ok");
        assert_eq!(resumed["agent"]["agent_id"], json!("agent-resume"));

        let err = orchestrator
            .resume_agent(ResumeAgentRequest {
                agent_id: "agent-resume-closed".into(),
                session_id: Some(session_id),
                profile_id: "tenant-a".into(),
            })
            .expect_err("resume terminal must fail");
        assert_eq!(
            err.data.expect("error data")["kind"],
            json!(kinds::AGENT_CONTROL_UNAVAILABLE)
        );
    }

    /// #991 / M15-B — `interrupt_agent` MUST signal a *real* abort to
    /// a running native-specialist task, not only flip the in-memory
    /// status. The fastest way to assert that is to drive
    /// `run_native_specialist` with an LLM mock that sleeps on the
    /// model call, fire `interrupt_agent` from another task, and
    /// assert the future returns within a short timeout with
    /// `status == interrupted`.
    #[tokio::test]
    async fn interrupt_agent_signals_real_cancellation_to_native_specialist() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let orchestrator = Arc::new(InProcessAgentOrchestrator::default());
        let session_id = default_session("native-cancel");
        let tools = Arc::new(ToolRegistry::with_builtins(dir.path()));
        let memory = Arc::new(
            EpisodeStore::open(dir.path().join("memory"))
                .await
                .expect("memory store"),
        );

        struct SleepyProvider;

        #[async_trait::async_trait]
        impl LlmProvider for SleepyProvider {
            async fn chat(
                &self,
                _messages: &[octos_core::Message],
                _tools: &[octos_llm::ToolSpec],
                _config: &octos_llm::ChatConfig,
            ) -> eyre::Result<octos_llm::ChatResponse> {
                // Sleep "forever" — interrupt_agent must short-
                // circuit this. We do still bound it so a failing
                // cancellation path doesn't hang CI; 30s is far
                // beyond the 5s test timeout.
                tokio::time::sleep(Duration::from_secs(30)).await;
                Ok(octos_llm::ChatResponse {
                    content: Some("never".into()),
                    reasoning_content: None,
                    tool_calls: Vec::new(),
                    stop_reason: octos_llm::StopReason::EndTurn,
                    usage: Default::default(),
                    provider_index: None,
                })
            }
            fn model_id(&self) -> &str {
                "sleepy"
            }
            fn provider_name(&self) -> &str {
                "test"
            }
        }

        use std::time::Duration;
        let llm: Arc<dyn LlmProvider> = Arc::new(SleepyProvider);
        let orchestrator_for_spawn = orchestrator.clone();
        let agent_id = "native-cancel-target".to_owned();
        let agent_id_for_spawn = agent_id.clone();
        let session_id_for_spawn = session_id.clone();
        let spawn = tokio::spawn(async move {
            orchestrator_for_spawn
                .run_native_specialist(NativeSpecialistLaunchRequest {
                    agent_id: Some(agent_id_for_spawn),
                    parent_agent_id: Some("master".to_owned()),
                    session_id: session_id_for_spawn,
                    profile_id: "tenant-a".to_owned(),
                    role: "reviewer".to_owned(),
                    nickname: "Sleepy".to_owned(),
                    task: "wait forever".to_owned(),
                    cwd: dir.path().to_path_buf(),
                    llm,
                    memory,
                    tools,
                    system_prompt: None,
                    agent_config: None,
                    task_ledger_path: None,
                    event_tx: None,
                    dispatch_policy: None,
                })
                .await
        });

        // Wait briefly for the orchestrator to register the
        // cancellation handle. We don't have a hook for "worker
        // ready", so poll until the handle is visible.
        let mut tries = 0;
        loop {
            if orchestrator.state().cancellations.contains_key(&agent_id) {
                break;
            }
            tries += 1;
            assert!(tries < 100, "cancellation handle never registered");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let interrupt_result = orchestrator
            .interrupt_agent(AgentRequest {
                agent_id: agent_id.clone(),
                session_id: Some(session_id.clone()),
                profile_id: "tenant-a".into(),
            })
            .expect("interrupt ok");
        assert_eq!(interrupt_result["status"], json!("interrupted"));

        let outcome = tokio::time::timeout(Duration::from_secs(5), spawn)
            .await
            .expect("native specialist must return within timeout")
            .expect("join ok")
            .expect("specialist result ok");
        assert_eq!(
            outcome.status, "interrupted",
            "real cancellation must surface as `interrupted`"
        );
    }

    /// #1127 codex P1 acceptance: a cross-profile interrupt MUST NOT
    /// signal the worker's cancellation token. The scope check has to
    /// fire BEFORE the notify, otherwise an attacker who knows
    /// another tenant's `agent_id` could wake / remove that worker's
    /// token even though the RPC eventually returns
    /// `permission_denied`. Pins the validate-then-stamp-then-signal
    /// order.
    #[test]
    fn cross_profile_interrupt_does_not_signal_cancellation_token() {
        let orchestrator = InProcessAgentOrchestrator::default();
        let agent = sample_agent("victim-agent", "tenant-a");
        orchestrator
            .state()
            .agents
            .insert(agent.agent_id.clone(), agent.clone());

        // Pre-register a cancellation handle to detect the race.
        let token = orchestrator.register_agent_cancellation(&agent.agent_id);

        // Attacker on `tenant-b` tries to interrupt tenant-a's agent.
        let err = orchestrator
            .interrupt_agent(AgentRequest {
                agent_id: agent.agent_id.clone(),
                session_id: Some(agent.session_id.clone()),
                profile_id: "tenant-b".into(),
            })
            .expect_err("cross-profile interrupt must be denied");
        assert_eq!(err.code, rpc_error_codes::PERMISSION_DENIED);

        // The cancellation token MUST still be registered AND MUST NOT
        // have been notified — verify both invariants. We do a
        // try_recv-style check by spawning a quick notified() future
        // and asserting it doesn't resolve immediately.
        assert!(
            orchestrator
                .state()
                .cancellations
                .contains_key(&agent.agent_id),
            "denied interrupt must NOT have removed the cancellation token"
        );
        // `notify_one` would leave a permit on the token. Detect it.
        let notified_fut = std::pin::pin!(token.notified());
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        match notified_fut.poll(&mut cx) {
            std::task::Poll::Ready(()) => {
                panic!("denied interrupt left a cancellation permit on the victim's token")
            }
            std::task::Poll::Pending => {}
        }
    }

    #[test]
    fn maintenance_loop_resolves_prompt_at_fire_time_from_project_doc() {
        use std::env;
        // #1135 codex P2: serialize cwd-mutating tests in this module.
        let _cwd_guard = cwd_mutating_test_guard();
        let temp = tempfile::TempDir::new().expect("temp dir");
        let cwd_before = env::current_dir().expect("cwd");
        env::set_current_dir(temp.path()).expect("chdir tmp");
        let octos_dir = temp.path().join(".octos");
        std::fs::create_dir_all(&octos_dir).expect("mkdir .octos");
        std::fs::write(octos_dir.join("loop.md"), "  project maintenance steps\n  ")
            .expect("write loop.md");

        let orchestrator = InProcessAgentOrchestrator::default();
        let session_id = SessionKey::with_profile("tenant-a", "api", "loop-maint");
        let created = orchestrator
            .create_loop(LoopCreateRequest {
                session_id: session_id.clone(),
                profile_id: "tenant-a".into(),
                prompt: None,
                command: None,
                interval_seconds: None,
                mode: Some("maintenance".into()),
            })
            .expect("create maintenance loop");
        let loop_id = created["loop_id"].as_str().expect("loop id").to_owned();

        let fired = orchestrator
            .control_loop(LoopControlRequest {
                loop_id,
                session_id: Some(session_id.clone()),
                profile_id: "tenant-a".into(),
                kind: LoopControlKind::FireNow,
            })
            .expect("fire maintenance loop");
        assert_eq!(fired["status"], json!("queued"));

        let drained = orchestrator.drain_ready_continuations_for_session(
            &session_id,
            "tenant-a",
            MasterContinuationRuntimeState::idle(),
            1,
        );
        env::set_current_dir(&cwd_before).expect("restore cwd");
        assert_eq!(drained.len(), 1);
        let prompt_meta = drained[0]
            .metadata
            .get("prompt")
            .cloned()
            .expect("prompt metadata");
        assert_eq!(
            prompt_meta, "project maintenance steps",
            "maintenance prompt must be resolved at fire time from .octos/loop.md (#977)"
        );
        let source = drained[0]
            .metadata
            .get("prompt_source")
            .cloned()
            .expect("prompt_source metadata");
        assert_eq!(source, "project");
    }

    /// #1135 acceptance: the scheduled-due path must also report the
    /// resolved `prompt_source` (`project` / `user` / `built_in`) and
    /// not the legacy `"record"` placeholder. The continuation prompt
    /// must match the file content, proving the resolution actually
    /// ran for the scheduled tick, not just for `fire_now`.
    #[test]
    fn scheduled_maintenance_fire_emits_resolved_prompt_source() {
        use std::env;
        // #1135 codex P2: serialize cwd-mutating tests in this module.
        let _cwd_guard = cwd_mutating_test_guard();
        let temp = tempfile::TempDir::new().expect("temp dir");
        let cwd_before = env::current_dir().expect("cwd");
        env::set_current_dir(temp.path()).expect("chdir tmp");
        let octos_dir = temp.path().join(".octos");
        std::fs::create_dir_all(&octos_dir).expect("mkdir .octos");
        std::fs::write(
            octos_dir.join("loop.md"),
            "scheduled project maintenance steps\n",
        )
        .expect("write loop.md");

        let orchestrator = InProcessAgentOrchestrator::default();
        let session_id = SessionKey::with_profile("tenant-a", "api", "sched-loop-maint");
        let created = orchestrator
            .create_loop(LoopCreateRequest {
                session_id: session_id.clone(),
                profile_id: "tenant-a".into(),
                prompt: None,
                command: None,
                interval_seconds: None,
                mode: Some("maintenance".into()),
            })
            .expect("create maintenance loop");
        let loop_id = created["loop_id"].as_str().expect("loop id").to_owned();

        // Force the scheduled-due path: stamp a past `next_run_at_ms`
        // and tick the scheduler. `fire_now` is NOT involved here.
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
        assert_eq!(ticked, 1, "scheduled maintenance loop should enqueue");

        let drained = orchestrator.drain_ready_continuations_for_session(
            &session_id,
            "tenant-a",
            MasterContinuationRuntimeState::idle(),
            usize::MAX,
        );
        env::set_current_dir(&cwd_before).expect("restore cwd");
        assert_eq!(drained.len(), 1);
        let prompt_meta = drained[0]
            .metadata
            .get("prompt")
            .cloned()
            .expect("prompt metadata");
        assert_eq!(
            prompt_meta.trim(),
            "scheduled project maintenance steps",
            "scheduled maintenance prompt must be resolved from .octos/loop.md (#1135)"
        );
        let source = drained[0]
            .metadata
            .get("prompt_source")
            .cloned()
            .expect("prompt_source metadata");
        assert_eq!(
            source, "project",
            "scheduled fire must carry the resolved MaintenancePromptSource label (#1135)"
        );
    }

    #[test]
    fn parse_self_paced_next_delay_recognizes_sentinel_and_falls_back_to_default() {
        // The model emits a sentinel like `<<loop-next-in: 90s>>` after a
        // self-paced fire. The parser extracts the delay; absence yields
        // `None` so the caller can fall back to its configured default.
        assert_eq!(
            parse_self_paced_next_delay("ok done <<loop-next-in: 90s>> bye"),
            Some(Duration::from_secs(90))
        );
        assert_eq!(
            parse_self_paced_next_delay("status report <<loop-next-in: 5m>>"),
            Some(Duration::from_secs(300))
        );
        assert_eq!(
            parse_self_paced_next_delay("no hint emitted by the model here"),
            None
        );
        // Invalid value (zero / non-numeric) yields None.
        assert_eq!(parse_self_paced_next_delay("<<loop-next-in: 0s>>"), None);
        assert_eq!(parse_self_paced_next_delay("<<loop-next-in: nope>>"), None);
    }

    #[test]
    fn self_paced_loop_reschedules_using_parsed_next_delay() {
        let orchestrator = InProcessAgentOrchestrator::default();
        let session_id = SessionKey::with_profile("tenant-a", "api", "loop-self-paced");
        let created = orchestrator
            .create_loop(LoopCreateRequest {
                session_id: session_id.clone(),
                profile_id: "tenant-a".into(),
                prompt: Some("watch for blockers".into()),
                command: None,
                interval_seconds: None,
                mode: Some("self_paced".into()),
            })
            .expect("create self_paced loop");
        let loop_id = created["loop_id"].as_str().expect("loop id").to_owned();

        let before = now_ms();
        let applied = orchestrator
            .apply_self_paced_response(
                &loop_id,
                "tenant-a",
                "checked things <<loop-next-in: 120s>>",
            )
            .expect("apply self-paced response");
        assert_eq!(applied, Some(Duration::from_secs(120)));

        let state = orchestrator.state();
        let next = state
            .loops
            .get(&loop_id)
            .and_then(|record| record.next_run_at_ms)
            .expect("self-paced loop should have a next_run_at_ms after hint");
        let delta_ms = next - before;
        assert!(
            (110_000..=130_000).contains(&delta_ms),
            "next_run_at_ms should be roughly 120s in the future (got {delta_ms} ms)",
        );
    }

    /// #1133 acceptance 1 — when the AppUI goal-turn path finishes a
    /// real LLM turn, it must call `record_goal_turn` with the actual
    /// tokens consumed AND the elapsed seconds, NOT the dispatch-only
    /// helper. This pins that `tokens_used` is bumped (was permanently
    /// stuck at 0 in the pre-#1133 shape, hiding `budget_limited`
    /// transitions from the AppUI goal soak).
    #[test]
    fn appui_goal_path_record_goal_turn_with_real_tokens_bumps_counters() {
        let orchestrator = InProcessAgentOrchestrator::default();
        let session_id = SessionKey::with_profile("tenant-a", "api", "goal-appui-tokens");
        orchestrator
            .set_goal(GoalSetRequest {
                session_id: session_id.clone(),
                profile_id: "tenant-a".into(),
                objective: "real tokens please".into(),
                status: Some("active".into()),
                token_budget: Some(50_000),
                transition_actor: None,
            })
            .expect("set active goal");
        // Drain the initial set_goal continuation; #1133 acceptance is
        // about the POST-turn accountant, not the dispatch-time queue.
        let _ = orchestrator.drain_ready_continuations_for_session(
            &session_id,
            "tenant-a",
            MasterContinuationRuntimeState::idle(),
            usize::MAX,
        );

        // Pre-condition: counters all start at zero.
        let (tokens_before, continuations_before, window_before) = orchestrator
            .goal_counters_for_test(&session_id)
            .expect("goal exists");
        assert_eq!(tokens_before, 0);
        assert_eq!(continuations_before, 0);
        assert_eq!(window_before, 0);

        // Post-turn AppUI behavior: record a turn that actually consumed
        // tokens (this is what `run_standalone_turn` does once goal
        // context + token accounting are wired through).
        orchestrator.record_goal_turn(&session_id, "tenant-a", 1234, 7);

        let (tokens_after, continuations_after, window_after) = orchestrator
            .goal_counters_for_test(&session_id)
            .expect("goal still exists");
        assert_eq!(
            tokens_after, 1234,
            "record_goal_turn must fold tokens_consumed into goal.tokens_used"
        );
        assert_eq!(
            continuations_after, 1,
            "record_goal_turn must bump continuations_used by exactly one"
        );
        assert_eq!(
            window_after, 1,
            "record_goal_turn must bump the sliding rate-window counter",
        );
    }

    /// #1133 acceptance 2 — when the AppUI goal turn produces a reply
    /// ending in `<goal:complete>`, the post-turn
    /// `maybe_complete_goal_from_model` call flips the goal to
    /// `complete`. Without this wiring, the sentinel-detection path
    /// was unreachable from `run_standalone_turn` (only the
    /// `SessionActor` chat path called it).
    #[test]
    fn appui_goal_path_completes_goal_when_reply_ends_with_sentinel() {
        let orchestrator = InProcessAgentOrchestrator::default();
        let session_id = SessionKey::with_profile("tenant-a", "api", "goal-appui-sentinel");
        orchestrator
            .set_goal(GoalSetRequest {
                session_id: session_id.clone(),
                profile_id: "tenant-a".into(),
                objective: "finish via sentinel".into(),
                status: Some("active".into()),
                token_budget: None,
                transition_actor: None,
            })
            .expect("set active goal");
        let _ = orchestrator.drain_ready_continuations_for_session(
            &session_id,
            "tenant-a",
            MasterContinuationRuntimeState::idle(),
            usize::MAX,
        );

        // Mid-body sentinel must NOT flip the goal.
        assert!(!orchestrator.maybe_complete_goal_from_model(
            &session_id,
            "tenant-a",
            "I am about to write <goal:complete> shortly, but step 2 first.",
        ));
        assert_eq!(
            orchestrator.goal_status_for_test(&session_id).as_deref(),
            Some("active"),
        );

        // Trailing sentinel (the canonical AppUI shape) flips it.
        assert!(orchestrator.maybe_complete_goal_from_model(
            &session_id,
            "tenant-a",
            "All requested checks finished.\n\n<goal:complete>",
        ));
        assert_eq!(
            orchestrator.goal_status_for_test(&session_id).as_deref(),
            Some("complete"),
        );
    }

    /// #1133 acceptance 3 — the AppUI tick path must NOT call
    /// `record_goal_dispatch_only` for a `GoalContinue` dispatch
    /// (option (b) in #1133). The post-turn `record_goal_turn` is the
    /// single accountant that bumps `continuations_used` AND
    /// `rate_window_count`. Otherwise the AppUI path would double-count
    /// every fire and exhaust the per-hour cap after ~6 turns instead
    /// of the documented 12.
    #[test]
    fn appui_goal_dispatch_path_does_not_double_count_continuations() {
        let orchestrator = InProcessAgentOrchestrator::default();
        let session_id = SessionKey::with_profile("tenant-a", "api", "goal-appui-dispatch");
        orchestrator
            .set_goal(GoalSetRequest {
                session_id: session_id.clone(),
                profile_id: "tenant-a".into(),
                objective: "one fire counts as one".into(),
                status: Some("active".into()),
                token_budget: None,
                transition_actor: None,
            })
            .expect("set active goal");
        let _ = orchestrator.drain_ready_continuations_for_session(
            &session_id,
            "tenant-a",
            MasterContinuationRuntimeState::idle(),
            usize::MAX,
        );

        // Simulate the NEW (#1133 option b) AppUI dispatch path: do NOT
        // call `record_goal_dispatch_only` at dispatch time. Only call
        // `record_goal_turn` once the turn returns with real tokens.
        orchestrator.record_goal_turn(&session_id, "tenant-a", 500, 3);

        let (_, continuations_after, window_after) = orchestrator
            .goal_counters_for_test(&session_id)
            .expect("goal exists");
        assert_eq!(
            continuations_after, 1,
            "AppUI option (b) must produce exactly ONE continuations_used increment per turn"
        );
        assert_eq!(
            window_after, 1,
            "AppUI option (b) must produce exactly ONE rate_window_count increment per turn"
        );
    }

    /// #1140 codex P2 re-review #3 acceptance: a goal session that
    /// has been marked in-flight MUST be excluded from
    /// `due_loop_targets`'s goal sweep, EVEN IF the
    /// `last_continued_at_ms` timestamp has gone stale (>30s past).
    /// This is the race-free guard for long-running goal turns —
    /// without it, a scheduler tick landing in the await gap between
    /// turn-terminal emission and post-accounting could re-dispatch.
    #[test]
    fn in_flight_goal_session_is_excluded_from_due_loop_targets() {
        let orchestrator = InProcessAgentOrchestrator::default();
        let session_id = SessionKey::with_profile("tenant-a", "api", "goal-in-flight");
        orchestrator
            .set_goal(GoalSetRequest {
                session_id: session_id.clone(),
                profile_id: "tenant-a".into(),
                objective: "long-running goal".into(),
                status: Some("active".into()),
                token_budget: None,
                transition_actor: None,
            })
            .expect("set active goal");
        // Drain the initial continuation so the session is "between turns".
        let _ = orchestrator.drain_ready_continuations_for_session(
            &session_id,
            "tenant-a",
            MasterContinuationRuntimeState::idle(),
            usize::MAX,
        );
        // Force `last_continued_at_ms` to a value > 30s in the past
        // so the timestamp gate would normally PERMIT a re-dispatch.
        // The in-flight marker is the ONLY thing that should block it.
        if let Some(goal) = orchestrator.state().goals.get_mut(&session_id) {
            goal.last_continued_at_ms = now_ms() - (GOAL_MIN_CONTINUATION_INTERVAL_MS * 2);
        }
        // Sanity: without the in-flight marker, the goal IS due.
        let due_before = orchestrator.due_loop_targets(Some("tenant-a"), 8);
        assert!(
            due_before.iter().any(|(s, _)| s == &session_id),
            "without in-flight marker, stale-timestamp goal must be due (got {due_before:?})",
        );

        // Mark in-flight. Now the same `due_loop_targets` call MUST
        // exclude this session.
        orchestrator.mark_goal_dispatch_in_flight(&session_id);
        let due_during = orchestrator.due_loop_targets(Some("tenant-a"), 8);
        assert!(
            !due_during.iter().any(|(s, _)| s == &session_id),
            "in-flight goal session must be excluded from due_loop_targets (got {due_during:?})",
        );

        // Clearing in-flight restores the session to the due list.
        orchestrator.clear_goal_dispatch_in_flight(&session_id);
        let due_after = orchestrator.due_loop_targets(Some("tenant-a"), 8);
        assert!(
            due_after.iter().any(|(s, _)| s == &session_id),
            "after clearing in-flight, goal must be due again (got {due_after:?})",
        );
    }
}
