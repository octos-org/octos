//! Supervised specialist runners for AppUI-visible child agents.
//!
//! This module owns the process/MCP adapter edge for specialist child agents.
//! It intentionally stops at the runner boundary: native model orchestration
//! stays in the session runtime.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use octos_agent::tools::mcp_agent::{
    DispatchContextContract, DispatchOutcome, DispatchRequest, DispatchResponse, McpAgentBackend,
};
use octos_core::ui_protocol::{OutputCursor, methods};
use octos_core::{SessionKey, TaskId};
use serde_json::{Value, json};
use tokio::time::MissedTickBehavior;

use super::agent_orchestrator::{AgentArtifactRecord, AgentUpsert, InProcessAgentOrchestrator};
use super::workspace_scope::WorkspaceScope;
use crate::cli_agent_adapter::{
    CliAgentCommandConfig, CliAgentProcess, CliAgentRunResult, CliAgentTermination,
};

const DEFAULT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
const DEFAULT_MCP_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_ARTIFACT_CONTENT_BYTES: u64 = 64 * 1024;

pub(crate) trait AppUiSupervisorEventSink: Send + Sync {
    fn emit_supervisor_event(&self, method: &'static str, params: Value);
}

#[derive(Debug, Clone)]
pub(crate) struct SupervisedSpecialistSpec {
    pub(crate) agent_id: String,
    pub(crate) parent_agent_id: Option<String>,
    pub(crate) session_id: SessionKey,
    pub(crate) task_id: Option<TaskId>,
    pub(crate) path: String,
    pub(crate) role: String,
    pub(crate) nickname: String,
    pub(crate) backend_kind: String,
    pub(crate) task: Option<String>,
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) profile_id: String,
    pub(crate) artifacts: Vec<SpecialistArtifactSpec>,
}

#[derive(Debug, Clone)]
pub(crate) struct SpecialistArtifactSpec {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) kind: String,
    pub(crate) path: PathBuf,
}

#[derive(Debug, Clone)]
pub(crate) struct SupervisedCliSpecialist {
    pub(crate) spec: SupervisedSpecialistSpec,
    pub(crate) command: CliAgentCommandConfig,
    pub(crate) heartbeat_interval: Duration,
    pub(crate) dispatch_policy: Option<Arc<octos_agent::DispatchPolicy>>,
}

#[derive(Clone)]
pub(crate) struct SupervisedMcpSpecialist {
    pub(crate) spec: SupervisedSpecialistSpec,
    pub(crate) backend: Arc<dyn McpAgentBackend>,
    pub(crate) tool_name: String,
    pub(crate) task: Value,
    pub(crate) timeout: Duration,
    pub(crate) heartbeat_interval: Duration,
    pub(crate) dispatch_policy: Option<Arc<octos_agent::DispatchPolicy>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SupervisedSpecialistRunSummary {
    pub(crate) agent_id: String,
    pub(crate) status: String,
    pub(crate) output: String,
    pub(crate) artifact_ids: Vec<String>,
    pub(crate) ping_count: u64,
}

impl SupervisedCliSpecialist {
    pub(crate) fn new(spec: SupervisedSpecialistSpec, command: CliAgentCommandConfig) -> Self {
        Self {
            spec,
            command,
            heartbeat_interval: DEFAULT_HEARTBEAT_INTERVAL,
            dispatch_policy: None,
        }
    }

    pub(crate) fn heartbeat_interval(mut self, interval: Duration) -> Self {
        self.heartbeat_interval = interval;
        self
    }

    pub(crate) fn with_dispatch_policy(mut self, policy: Arc<octos_agent::DispatchPolicy>) -> Self {
        self.dispatch_policy = Some(policy);
        self
    }
}

impl SupervisedMcpSpecialist {
    pub(crate) fn new(
        spec: SupervisedSpecialistSpec,
        backend: Arc<dyn McpAgentBackend>,
        tool_name: impl Into<String>,
        task: Value,
    ) -> Self {
        Self {
            spec,
            backend,
            tool_name: tool_name.into(),
            task,
            timeout: DEFAULT_MCP_TIMEOUT,
            heartbeat_interval: DEFAULT_HEARTBEAT_INTERVAL,
            dispatch_policy: None,
        }
    }

    pub(crate) fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub(crate) fn heartbeat_interval(mut self, interval: Duration) -> Self {
        self.heartbeat_interval = interval;
        self
    }

    pub(crate) fn with_dispatch_policy(mut self, policy: Arc<octos_agent::DispatchPolicy>) -> Self {
        self.dispatch_policy = Some(policy);
        self
    }
}

pub(crate) async fn run_supervised_cli_specialist(
    orchestrator: &InProcessAgentOrchestrator,
    sink: &dyn AppUiSupervisorEventSink,
    mut request: SupervisedCliSpecialist,
) -> Result<SupervisedSpecialistRunSummary, String> {
    validate_spec(&request.spec)?;
    for artifact in &request.spec.artifacts {
        if !request
            .command
            .declared_artifacts
            .iter()
            .any(|path| path == &artifact.path)
        {
            request
                .command
                .declared_artifacts
                .push(artifact.path.clone());
        }
    }
    let workspace_scope = request
        .spec
        .cwd
        .as_deref()
        .and_then(WorkspaceScope::from_path);
    let initial_agent = orchestrator
        .upsert_agent_scoped(
            upsert_for_spec(&request.spec, "running", None),
            workspace_scope,
        )
        .map_err(|error| error.message)?;
    // #1021 / M17-C — CLI specialists are spawned as external subprocesses that never consume the Octos prompt context manager, so the dispatch contract is `external_context_unmanaged` with `risk: "medium"`. Stamping it here surfaces `context_mode` / `context_refs` on every subsequent `agent/updated` event so AppUI clients can audit context regime per child without polling the MCP path.
    let cli_contract = DispatchContextContract::external_unmanaged(
        "cli_specialist_does_not_consume_managed_payload",
    )
    .with_backend_kind("cli")
    .with_agent_id(request.spec.agent_id.clone())
    .with_risk("medium")
    .with_parent_session_key(Some(request.spec.session_id.to_string()))
    .with_child_session_key(Some(request.spec.agent_id.clone()));
    let agent = orchestrator
        .set_agent_context_contract(
            &request.spec.agent_id,
            &request.spec.session_id,
            &request.spec.profile_id,
            cli_contract,
        )
        .unwrap_or(initial_agent);
    emit_agent_updated(sink, &request.spec.session_id, agent);

    if let Some(policy) = request.dispatch_policy.as_ref() {
        let endpoint = request.command.program.to_string_lossy().into_owned();
        let backend = octos_agent::DispatchBackendMetadata::unsandboxed("cli", endpoint);
        let task = json!({
            "task": request.spec.task.as_deref(),
            "cwd": request.spec.cwd.as_ref().map(|path| path.to_string_lossy().into_owned()),
            "program": request.command.program.to_string_lossy().into_owned(),
            "args": request.command.args.clone(),
            "env": request.command.env.clone(),
        });
        if let Err(denial) = octos_agent::enforce_dispatch_gates_for_backend(
            policy.as_ref(),
            &backend,
            octos_agent::DispatchTarget {
                dispatch_id: &request.spec.agent_id,
                tool_name: &request.spec.backend_kind,
                task: &task,
            },
        )
        .await
        {
            return finish_failed_spawn(
                orchestrator,
                sink,
                &request.spec,
                dispatch_policy_denial_message(&denial),
            );
        }
    }

    let process = match CliAgentProcess::spawn(request.command) {
        Ok(process) => process,
        Err(error) => {
            return finish_failed_spawn(orchestrator, sink, &request.spec, error.to_string());
        }
    };

    let run = process.wait();
    tokio::pin!(run);
    let mut heartbeat = heartbeat_interval(request.heartbeat_interval);
    let mut ping_count = 0_u64;
    let result = loop {
        tokio::select! {
            result = &mut run => break result.map_err(|error| error.to_string())?,
            _ = heartbeat.tick() => {
                ping_count = ping_count.saturating_add(1);
                emit_ping(orchestrator, sink, &request.spec, ping_count, None);
            }
        }
    };

    finish_cli_run(orchestrator, sink, request.spec, result, ping_count)
}

pub(crate) async fn run_supervised_mcp_specialist(
    orchestrator: &InProcessAgentOrchestrator,
    sink: &dyn AppUiSupervisorEventSink,
    request: SupervisedMcpSpecialist,
) -> Result<SupervisedSpecialistRunSummary, String> {
    validate_spec(&request.spec)?;
    if request.tool_name.trim().is_empty() {
        return Err("MCP specialist tool_name must not be empty".to_owned());
    }
    if request.timeout.is_zero() {
        return Err("MCP specialist timeout must be greater than zero".to_owned());
    }

    let workspace_scope = request
        .spec
        .cwd
        .as_deref()
        .and_then(WorkspaceScope::from_path);
    let initial_agent = orchestrator
        .upsert_agent_scoped(
            upsert_for_spec(&request.spec, "running", None),
            workspace_scope,
        )
        .map_err(|error| error.message)?;
    // #1021 / M17-C — MCP supervised specialists dispatch through an external transport that does not yet wire a managed context payload, so the contract is `external_context_unmanaged` with `risk: "medium"`. The same contract is forwarded into the dispatch request below so the remote side and the AppUI event ledger agree on context regime.
    let context_contract = DispatchContextContract::external_unmanaged(
        "supervised_mcp_specialist_context_payload_not_wired",
    )
    .with_backend_kind("mcp")
    .with_agent_id(request.spec.agent_id.clone())
    .with_risk("medium")
    .with_parent_session_key(Some(request.spec.session_id.to_string()))
    .with_child_session_key(Some(request.spec.agent_id.clone()));
    let agent = orchestrator
        .set_agent_context_contract(
            &request.spec.agent_id,
            &request.spec.session_id,
            &request.spec.profile_id,
            context_contract.clone(),
        )
        .unwrap_or(initial_agent);
    emit_agent_updated(sink, &request.spec.session_id, agent);

    if let Some(policy) = request.dispatch_policy.as_ref() {
        let backend =
            octos_agent::DispatchBackendMetadata::from_mcp_backend(request.backend.as_ref());
        if let Err(denial) = octos_agent::enforce_dispatch_gates_for_backend(
            policy.as_ref(),
            &backend,
            octos_agent::DispatchTarget {
                dispatch_id: &request.spec.agent_id,
                tool_name: &request.tool_name,
                task: &request.task,
            },
        )
        .await
        {
            return finish_failed_spawn(
                orchestrator,
                sink,
                &request.spec,
                dispatch_policy_denial_message(&denial),
            );
        }
    }

    let dispatch = request.backend.dispatch(
        DispatchRequest::new(request.tool_name.clone(), request.task.clone())
            .with_context_contract(context_contract.clone()),
    );
    tokio::pin!(dispatch);
    let timeout = tokio::time::sleep(request.timeout);
    tokio::pin!(timeout);
    let mut heartbeat = heartbeat_interval(request.heartbeat_interval);
    let mut ping_count = 0_u64;
    let response = loop {
        tokio::select! {
            response = &mut dispatch => break response,
            _ = &mut timeout => {
                break DispatchResponse {
                    outcome: DispatchOutcome::Timeout,
                    output: "MCP specialist dispatch timed out".to_owned(),
                    files_to_send: Vec::new(),
                    error: Some("MCP specialist dispatch timed out".to_owned()),
                    context_contract: Some(context_contract.clone()),
                };
            }
            _ = heartbeat.tick() => {
                ping_count = ping_count.saturating_add(1);
                emit_ping(
                    orchestrator,
                    sink,
                    &request.spec,
                    ping_count,
                    Some(format!(
                        "MCP {} specialist running via {}",
                        request.backend.backend_label(),
                        request.backend.endpoint_label()
                    )),
                );
            }
        }
    };

    finish_mcp_run(
        orchestrator,
        sink,
        request.spec,
        response.with_context_contract(Some(context_contract)),
        ping_count,
    )
}

fn finish_cli_run(
    orchestrator: &InProcessAgentOrchestrator,
    sink: &dyn AppUiSupervisorEventSink,
    spec: SupervisedSpecialistSpec,
    result: CliAgentRunResult,
    ping_count: u64,
) -> Result<SupervisedSpecialistRunSummary, String> {
    let status = cli_status(&result.termination).to_owned();
    let mut output = result.transcript.stdout;
    if !result.transcript.stderr.is_empty() {
        output.push_str(&result.transcript.stderr);
    }
    if output.trim().is_empty() {
        output = format!("{} produced no output\n", spec.agent_id);
    }
    append_output(orchestrator, sink, &spec, &output)?;

    let artifacts = materialize_artifact_records(&spec.artifacts, &[]);
    set_artifacts(orchestrator, sink, &spec, artifacts.clone())?;
    let terminal_message = cli_terminal_message(&result.termination, &output);
    emit_agent_updated(
        sink,
        &spec.session_id,
        orchestrator
            .set_agent_status(
                &spec.agent_id,
                &spec.session_id,
                &spec.profile_id,
                &status,
                Some(terminal_message),
            )
            .map_err(|error| error.message)?,
    );

    Ok(SupervisedSpecialistRunSummary {
        agent_id: spec.agent_id,
        status,
        output,
        artifact_ids: artifacts.into_iter().map(|artifact| artifact.id).collect(),
        ping_count,
    })
}

fn finish_mcp_run(
    orchestrator: &InProcessAgentOrchestrator,
    sink: &dyn AppUiSupervisorEventSink,
    spec: SupervisedSpecialistSpec,
    response: DispatchResponse,
    ping_count: u64,
) -> Result<SupervisedSpecialistRunSummary, String> {
    let status = match response.outcome {
        DispatchOutcome::Success => "completed",
        DispatchOutcome::RemoteError
        | DispatchOutcome::Timeout
        | DispatchOutcome::TransportError
        | DispatchOutcome::ProtocolError
        | DispatchOutcome::SsrfBlocked => "failed",
    }
    .to_owned();
    let mut output = response.output;
    if let Some(error) = response
        .error
        .as_ref()
        .filter(|error| !output.contains(*error))
    {
        if !output.ends_with('\n') {
            output.push('\n');
        }
        output.push_str(error);
        output.push('\n');
    }
    append_output(orchestrator, sink, &spec, &output)?;

    let artifacts = materialize_artifact_records(&spec.artifacts, &response.files_to_send);
    set_artifacts(orchestrator, sink, &spec, artifacts.clone())?;
    let terminal_message = if status == "completed" {
        output
            .lines()
            .next()
            .unwrap_or("MCP specialist completed")
            .to_owned()
    } else {
        response
            .error
            .unwrap_or_else(|| format!("MCP specialist failed with {}", response.outcome.as_str()))
    };
    emit_agent_updated(
        sink,
        &spec.session_id,
        orchestrator
            .set_agent_status(
                &spec.agent_id,
                &spec.session_id,
                &spec.profile_id,
                &status,
                Some(terminal_message),
            )
            .map_err(|error| error.message)?,
    );

    Ok(SupervisedSpecialistRunSummary {
        agent_id: spec.agent_id,
        status,
        output,
        artifact_ids: artifacts.into_iter().map(|artifact| artifact.id).collect(),
        ping_count,
    })
}

fn dispatch_policy_denial_message(denial: &octos_agent::GateDenial) -> String {
    format!(
        "dispatch rejected by policy ({}): {}",
        denial.last_dispatch_outcome, denial.reason
    )
}

fn finish_failed_spawn<T>(
    orchestrator: &InProcessAgentOrchestrator,
    sink: &dyn AppUiSupervisorEventSink,
    spec: &SupervisedSpecialistSpec,
    message: String,
) -> Result<T, String> {
    let _ = orchestrator.append_agent_output(
        &spec.agent_id,
        &spec.session_id,
        &spec.profile_id,
        &message,
    );
    emit_agent_updated(
        sink,
        &spec.session_id,
        orchestrator
            .set_agent_status(
                &spec.agent_id,
                &spec.session_id,
                &spec.profile_id,
                "failed",
                Some(message.clone()),
            )
            .map_err(|error| error.message)?,
    );
    Err(message)
}

fn upsert_for_spec(
    spec: &SupervisedSpecialistSpec,
    status: &str,
    last_task: Option<String>,
) -> AgentUpsert {
    AgentUpsert {
        agent_id: spec.agent_id.clone(),
        parent_agent_id: spec.parent_agent_id.clone(),
        session_id: spec.session_id.clone(),
        task_id: spec.task_id.clone(),
        path: spec.path.clone(),
        role: spec.role.clone(),
        nickname: spec.nickname.clone(),
        backend_kind: spec.backend_kind.clone(),
        status: status.to_owned(),
        last_task: last_task.or_else(|| spec.task.clone()),
        cwd: spec
            .cwd
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned()),
        profile_id: spec.profile_id.clone(),
    }
}

fn emit_ping(
    orchestrator: &InProcessAgentOrchestrator,
    sink: &dyn AppUiSupervisorEventSink,
    spec: &SupervisedSpecialistSpec,
    ping_count: u64,
    message: Option<String>,
) {
    let message = message
        .unwrap_or_else(|| format!("heartbeat {ping_count}: {} is still running", spec.nickname));
    if let Ok(agent) = orchestrator.record_agent_ping(
        &spec.agent_id,
        &spec.session_id,
        &spec.profile_id,
        Some(ping_count.to_string()),
        Some("running".to_owned()),
        Some(message),
        None,
    ) {
        emit_agent_updated(sink, &spec.session_id, agent);
    }
}

fn append_output(
    orchestrator: &InProcessAgentOrchestrator,
    sink: &dyn AppUiSupervisorEventSink,
    spec: &SupervisedSpecialistSpec,
    output: &str,
) -> Result<(), String> {
    orchestrator
        .append_agent_output(&spec.agent_id, &spec.session_id, &spec.profile_id, output)
        .map_err(|error| error.message)?;
    sink.emit_supervisor_event(
        methods::AGENT_OUTPUT_DELTA,
        json!({
            "session_id": spec.session_id,
            "agent_id": spec.agent_id,
            "cursor": OutputCursor { offset: output.len() as u64 },
            "text": output,
        }),
    );
    Ok(())
}

fn set_artifacts(
    orchestrator: &InProcessAgentOrchestrator,
    sink: &dyn AppUiSupervisorEventSink,
    spec: &SupervisedSpecialistSpec,
    artifacts: Vec<AgentArtifactRecord>,
) -> Result<(), String> {
    orchestrator
        .set_agent_artifacts(
            &spec.agent_id,
            &spec.session_id,
            &spec.profile_id,
            artifacts.clone(),
        )
        .map_err(|error| error.message)?;
    sink.emit_supervisor_event(
        methods::AGENT_ARTIFACT_UPDATED,
        json!({
            "session_id": spec.session_id,
            "agent_id": spec.agent_id,
            "artifacts": artifacts.iter().map(agent_artifact_json).collect::<Vec<_>>(),
        }),
    );
    Ok(())
}

fn emit_agent_updated(sink: &dyn AppUiSupervisorEventSink, session_id: &SessionKey, agent: Value) {
    sink.emit_supervisor_event(
        methods::AGENT_UPDATED,
        json!({
            "session_id": session_id,
            "agent": agent,
        }),
    );
}

fn agent_artifact_json(artifact: &AgentArtifactRecord) -> Value {
    json!({
        "id": artifact.id,
        "title": artifact.title,
        "kind": artifact.kind,
        "status": artifact.status,
        "path": artifact.path,
        "content": artifact.content,
    })
}

fn materialize_artifact_records(
    declared: &[SpecialistArtifactSpec],
    backend_files: &[PathBuf],
) -> Vec<AgentArtifactRecord> {
    let mut seen = HashSet::new();
    let mut artifacts = Vec::new();
    for artifact in declared {
        if seen.insert(artifact.path.clone()) {
            artifacts.push(artifact_record(
                &artifact.id,
                &artifact.title,
                &artifact.kind,
                &artifact.path,
            ));
        }
    }
    for path in backend_files {
        if seen.insert(path.clone()) {
            let id = artifact_id_from_path(path);
            artifacts.push(artifact_record(
                &id,
                &id,
                artifact_kind_from_path(path),
                path,
            ));
        }
    }
    artifacts
}

fn artifact_record(id: &str, title: &str, kind: &str, path: &Path) -> AgentArtifactRecord {
    AgentArtifactRecord {
        id: id.to_owned(),
        title: title.to_owned(),
        kind: kind.to_owned(),
        status: if path.exists() { "ready" } else { "missing" }.to_owned(),
        path: Some(path.to_string_lossy().into_owned()),
        content: read_small_text_artifact(path),
    }
}

fn read_small_text_artifact(path: &Path) -> Option<String> {
    let metadata = std::fs::metadata(path).ok()?;
    if metadata.len() > MAX_ARTIFACT_CONTENT_BYTES || !metadata.is_file() {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

fn artifact_id_from_path(path: &Path) -> String {
    path.file_stem()
        .or_else(|| path.file_name())
        .and_then(|name| name.to_str())
        .map(|name| {
            name.chars()
                .map(|ch| {
                    if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                        ch
                    } else {
                        '-'
                    }
                })
                .collect::<String>()
                .trim_matches('-')
                .to_owned()
        })
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "artifact".to_owned())
}

fn artifact_kind_from_path(path: &Path) -> &'static str {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("md" | "markdown" | "txt") => "markdown",
        Some("json") => "json",
        _ => "file",
    }
}

fn cli_status(termination: &CliAgentTermination) -> &'static str {
    match termination {
        CliAgentTermination::Exited { code: Some(0) } => "completed",
        CliAgentTermination::Exited { .. } | CliAgentTermination::TimedOut => "failed",
        CliAgentTermination::Cancelled => "interrupted",
        CliAgentTermination::Closed => "closed",
    }
}

fn cli_terminal_message(termination: &CliAgentTermination, output: &str) -> String {
    match termination {
        CliAgentTermination::Exited { code: Some(0) } => output
            .lines()
            .next()
            .unwrap_or("CLI specialist completed")
            .to_owned(),
        CliAgentTermination::Exited { code } => format!("CLI specialist exited with code {code:?}"),
        CliAgentTermination::TimedOut => "CLI specialist timed out".to_owned(),
        CliAgentTermination::Cancelled => "CLI specialist interrupted".to_owned(),
        CliAgentTermination::Closed => "CLI specialist closed".to_owned(),
    }
}

fn heartbeat_interval(interval: Duration) -> tokio::time::Interval {
    let mut interval = tokio::time::interval(if interval.is_zero() {
        DEFAULT_HEARTBEAT_INTERVAL
    } else {
        interval
    });
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    interval
}

fn validate_spec(spec: &SupervisedSpecialistSpec) -> Result<(), String> {
    if spec.agent_id.trim().is_empty() {
        return Err("specialist agent_id must not be empty".to_owned());
    }
    if spec.path.trim().is_empty() {
        return Err("specialist path must not be empty".to_owned());
    }
    if spec.role.trim().is_empty() {
        return Err("specialist role must not be empty".to_owned());
    }
    if spec.nickname.trim().is_empty() {
        return Err("specialist nickname must not be empty".to_owned());
    }
    if spec.backend_kind.trim().is_empty() {
        return Err("specialist backend_kind must not be empty".to_owned());
    }
    if spec.profile_id.trim().is_empty() {
        return Err("specialist profile_id must not be empty".to_owned());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::agent_orchestrator::{AgentOrchestrator, AgentOutputRequest, AgentRequest};
    use super::*;
    use async_trait::async_trait;
    use serde_json::json;
    use std::collections::HashSet;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct RecordingSink {
        events: Mutex<Vec<(&'static str, Value)>>,
    }

    impl RecordingSink {
        fn events(&self) -> Vec<(&'static str, Value)> {
            self.events.lock().unwrap().clone()
        }
    }

    impl AppUiSupervisorEventSink for RecordingSink {
        fn emit_supervisor_event(&self, method: &'static str, params: Value) {
            self.events.lock().unwrap().push((method, params));
        }
    }

    #[cfg(unix)]
    fn write_executable(dir: &tempfile::TempDir, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let path = dir.path().join(name);
        std::fs::write(&path, body).unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    fn sample_spec(dir: &tempfile::TempDir, agent_id: &str) -> SupervisedSpecialistSpec {
        SupervisedSpecialistSpec {
            agent_id: agent_id.to_owned(),
            parent_agent_id: Some("master".to_owned()),
            session_id: SessionKey::with_profile("tenant-a", "api", "specialist"),
            task_id: Some(TaskId::new()),
            path: format!("master/{agent_id}"),
            role: "reviewer".to_owned(),
            nickname: "Ada".to_owned(),
            backend_kind: "cli_process".to_owned(),
            task: Some("review".to_owned()),
            cwd: Some(dir.path().to_path_buf()),
            profile_id: "tenant-a".to_owned(),
            artifacts: vec![SpecialistArtifactSpec {
                id: "report".to_owned(),
                title: "Report".to_owned(),
                kind: "markdown".to_owned(),
                path: dir.path().join("report.md"),
            }],
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cli_specialist_emits_pings_terminal_output_and_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_executable(
            &dir,
            "agent",
            r#"#!/bin/sh
sleep 0.08
printf '# report\n' > report.md
printf 'done\n'
"#,
        );
        let orchestrator = InProcessAgentOrchestrator::default();
        orchestrator
            .configure_supervisor_store(dir.path().join("supervisor"))
            .unwrap();
        let sink = RecordingSink::default();
        let spec = sample_spec(&dir, "cli-child");
        let summary = run_supervised_cli_specialist(
            &orchestrator,
            &sink,
            SupervisedCliSpecialist::new(
                spec.clone(),
                CliAgentCommandConfig::new(script).cwd(dir.path()),
            )
            .heartbeat_interval(Duration::from_millis(20)),
        )
        .await
        .unwrap();

        assert_eq!(summary.status, "completed");
        assert!(summary.ping_count > 0);
        assert_eq!(summary.artifact_ids, vec!["report"]);

        let events = sink.events();
        assert!(
            events
                .iter()
                .any(|(method, _)| *method == methods::AGENT_UPDATED)
        );
        assert!(
            events
                .iter()
                .any(|(method, _)| *method == methods::AGENT_OUTPUT_DELTA)
        );
        assert!(
            events
                .iter()
                .any(|(method, _)| *method == methods::AGENT_ARTIFACT_UPDATED)
        );

        let output = orchestrator
            .read_agent_output(AgentOutputRequest {
                agent_id: spec.agent_id.clone(),
                session_id: Some(spec.session_id.clone()),
                profile_id: spec.profile_id.clone(),
                cursor: None,
                limit: None,
            })
            .unwrap();
        assert_eq!(output["complete"], json!(true));
        assert!(output["text"].as_str().unwrap().contains("done"));

        let restored =
            super::super::supervisor_store::SupervisorStore::new(dir.path().join("supervisor"))
                .load_state()
                .unwrap();
        let child = restored
            .children
            .values()
            .find(|child| child.child_id == "cli-child")
            .unwrap();
        assert!(child.last_heartbeat.is_some());
        assert!(child.terminal.is_some());
    }

    /// #1021 / M17-C — the CLI specialist runner MUST stamp a `DispatchContextContract::external_unmanaged` onto the agent record before emitting `agent/updated`, with `backend_kind: cli` and `risk: medium`. This pins the wire contract that AppUI clients rely on to tell apart managed-payload children from external-context children.
    #[cfg(unix)]
    #[tokio::test]
    async fn cli_specialist_stamps_external_unmanaged_context_contract() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_executable(
            &dir,
            "agent",
            r#"#!/bin/sh
printf 'done\n'
"#,
        );
        let orchestrator = InProcessAgentOrchestrator::default();
        let sink = RecordingSink::default();
        let spec = sample_spec(&dir, "cli-ctx-child");
        run_supervised_cli_specialist(
            &orchestrator,
            &sink,
            SupervisedCliSpecialist::new(
                spec.clone(),
                CliAgentCommandConfig::new(script).cwd(dir.path()),
            )
            .heartbeat_interval(Duration::from_millis(50)),
        )
        .await
        .unwrap();

        let events = sink.events();
        let agent_updated = events
            .iter()
            .find_map(|(method, params)| {
                (*method == methods::AGENT_UPDATED).then_some(params.clone())
            })
            .expect("at least one agent/updated event");
        let agent = &agent_updated["agent"];
        assert_eq!(agent["context_mode"], json!("external_context_unmanaged"));
        assert_eq!(agent["context_refs"], json!(Vec::<String>::new()));
        let contract = &agent["context_contract"];
        assert_eq!(contract["mode"], json!("external_context_unmanaged"));
        assert_eq!(contract["backend_kind"], json!("cli"));
        assert_eq!(contract["risk"], json!("medium"));
        assert_eq!(contract["agent_id"], json!(spec.agent_id));
        assert_eq!(
            contract["reason"],
            json!("cli_specialist_does_not_consume_managed_payload")
        );
        assert_eq!(
            contract["parent_session_key"],
            json!(spec.session_id.to_string())
        );
        assert_eq!(contract["child_session_key"], json!(spec.agent_id));
    }

    #[derive(Default)]
    struct ScriptedMcpBackend {
        calls: AtomicUsize,
        artifact: Mutex<Option<PathBuf>>,
    }

    #[cfg(unix)]
    struct OneTurnNativeProvider;

    #[cfg(unix)]
    #[async_trait]
    impl octos_llm::LlmProvider for OneTurnNativeProvider {
        async fn chat(
            &self,
            _messages: &[octos_core::Message],
            _tools: &[octos_llm::ToolSpec],
            _config: &octos_llm::ChatConfig,
        ) -> eyre::Result<octos_llm::ChatResponse> {
            Ok(octos_llm::ChatResponse {
                content: Some("native done".to_owned()),
                reasoning_content: None,
                tool_calls: Vec::new(),
                stop_reason: octos_llm::StopReason::EndTurn,
                usage: octos_llm::TokenUsage {
                    input_tokens: 1,
                    output_tokens: 1,
                    ..Default::default()
                },
                provider_index: None,
            })
        }

        fn model_id(&self) -> &str {
            "one-turn-native"
        }

        fn provider_name(&self) -> &str {
            "test"
        }
    }

    #[async_trait]
    impl McpAgentBackend for ScriptedMcpBackend {
        fn backend_label(&self) -> &'static str {
            "test_mcp"
        }

        fn endpoint_label(&self) -> String {
            "test-endpoint".to_owned()
        }

        async fn dispatch(&self, _request: DispatchRequest) -> DispatchResponse {
            self.calls.fetch_add(1, Ordering::Relaxed);
            tokio::time::sleep(Duration::from_millis(60)).await;
            let artifact = self.artifact.lock().unwrap().clone().unwrap();
            std::fs::write(&artifact, "# mcp\n").unwrap();
            DispatchResponse {
                outcome: DispatchOutcome::Success,
                output: "mcp done".to_owned(),
                files_to_send: vec![artifact],
                error: None,
                context_contract: None,
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn workspace_r6_non_utf8_native_cli_mcp_share_one_purge_without_lossy_neighbor() {
        use super::super::agent_orchestrator::NativeSpecialistLaunchRequest;
        use super::super::supervisor_store::ContinuationStatus;
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let dir = tempfile::tempdir().unwrap();
        let raw_root = dir.path().join(OsStr::from_bytes(b"\xff-root"));
        let lossy_neighbor = dir.path().join("\u{fffd}-root");
        let raw_root_created = match std::fs::create_dir_all(&raw_root) {
            Ok(()) => true,
            Err(error) if error.raw_os_error() == Some(92) => false,
            Err(error) => panic!("create raw-byte workspace: {error}"),
        };
        std::fs::create_dir_all(&lossy_neighbor).unwrap();
        assert_ne!(raw_root, lossy_neighbor);
        assert_eq!(raw_root.to_string_lossy(), lossy_neighbor.to_string_lossy());

        let orchestrator = InProcessAgentOrchestrator::default();
        orchestrator
            .configure_supervisor_store(dir.path().join("supervisor"))
            .unwrap();
        let session = SessionKey::with_profile("tenant-a", "api", "specialist");
        // #39: joins require at least two ordinary children. Retire one seed
        // verdict in each scope before the real runners execute, preserving
        // their 6/2 pending purge counts without synthetic singleton joins.
        for (id, root) in [("seed-raw", &raw_root), ("seed-neighbor", &lossy_neighbor)] {
            orchestrator
                .upsert_agent_scoped(
                    AgentUpsert {
                        agent_id: id.into(),
                        parent_agent_id: Some("master".into()),
                        session_id: session.clone(),
                        task_id: None,
                        path: format!("master/{id}"),
                        role: "worker".into(),
                        nickname: id.into(),
                        backend_kind: "native".into(),
                        status: "completed".into(),
                        last_task: Some("seed completed before the runner burst".into()),
                        cwd: Some(root.to_string_lossy().into_owned()),
                        profile_id: "tenant-a".into(),
                    },
                    WorkspaceScope::from_path(root),
                )
                .unwrap();
            let seeds = orchestrator.drain_ready_continuations_for_session(
                &session,
                "tenant-a",
                super::super::master_continuation_scheduler::MasterContinuationRuntimeState::idle(),
                usize::MAX,
            );
            assert_eq!(seeds.len(), 1);
            assert_eq!(
                seeds[0].child_agent_id.as_ref().map(|id| id.as_str()),
                Some(id)
            );
            orchestrator.mark_continuation_completed(&seeds[0], Some("seed consumed".into()));
        }

        orchestrator
            .run_native_specialist(NativeSpecialistLaunchRequest {
                agent_id: Some("native-raw".to_owned()),
                parent_agent_id: Some("master".to_owned()),
                session_id: session.clone(),
                profile_id: "tenant-a".to_owned(),
                role: "reviewer".to_owned(),
                nickname: "Native Raw".to_owned(),
                task: "review raw workspace".to_owned(),
                cwd: raw_root.clone(),
                llm: Arc::new(OneTurnNativeProvider),
                memory: Arc::new(
                    octos_memory::EpisodeStore::open(dir.path().join("native-memory"))
                        .await
                        .unwrap(),
                ),
                tools: Arc::new(octos_agent::ToolRegistry::with_builtins(dir.path())),
                system_prompt: None,
                agent_config: None,
                task_ledger_path: None,
                event_tx: None,
                dispatch_policy: None,
            })
            .await
            .unwrap();

        let script = write_executable(
            &dir,
            "scope-agent",
            "#!/bin/sh\nprintf done > cli-marker\nprintf 'cli done\\n'\n",
        );
        let sink = RecordingSink::default();
        let mut cli_spec = sample_spec(&dir, "cli-raw");
        cli_spec.cwd = Some(raw_root.clone());
        cli_spec.artifacts.clear();
        let raw_cli_result = run_supervised_cli_specialist(
            &orchestrator,
            &sink,
            SupervisedCliSpecialist::new(
                cli_spec,
                CliAgentCommandConfig::new(&script).cwd(&raw_root),
            ),
        )
        .await;
        if raw_root_created {
            raw_cli_result.unwrap();
        } else {
            assert!(
                raw_cli_result.is_err(),
                "a platform that rejects the raw directory must fail after runner admission"
            );
        }

        let backend = Arc::new(ScriptedMcpBackend::default());
        *backend.artifact.lock().unwrap() = Some(dir.path().join("mcp-report.md"));
        let mut mcp_spec = sample_spec(&dir, "mcp-raw");
        mcp_spec.cwd = Some(raw_root.clone());
        mcp_spec.backend_kind = "mcp_test".to_owned();
        mcp_spec.artifacts.clear();
        run_supervised_mcp_specialist(
            &orchestrator,
            &sink,
            SupervisedMcpSpecialist::new(
                mcp_spec,
                backend,
                "agent/run",
                json!({"prompt": "review raw workspace"}),
            )
            .timeout(Duration::from_secs(1)),
        )
        .await
        .unwrap();

        let mut neighbor_spec = sample_spec(&dir, "cli-lossy-neighbor");
        neighbor_spec.cwd = Some(lossy_neighbor.clone());
        neighbor_spec.artifacts.clear();
        run_supervised_cli_specialist(
            &orchestrator,
            &sink,
            SupervisedCliSpecialist::new(
                neighbor_spec,
                CliAgentCommandConfig::new(script).cwd(&lossy_neighbor),
            ),
        )
        .await
        .unwrap();

        if raw_root_created {
            assert!(raw_root.join("cli-marker").is_file());
        }
        assert!(lossy_neighbor.join("cli-marker").is_file());
        for agent_id in ["native-raw", "cli-raw", "mcp-raw", "cli-lossy-neighbor"] {
            let status = orchestrator
                .read_agent_status(AgentRequest {
                    agent_id: agent_id.to_owned(),
                    session_id: Some(session.clone()),
                    profile_id: "tenant-a".to_owned(),
                })
                .unwrap();
            assert_eq!(
                status["agent"]["cwd"],
                json!(lossy_neighbor.to_string_lossy()),
                "display cwd changed for {agent_id}"
            );
        }

        let raw_scope_key = WorkspaceScope::from_path(&raw_root).unwrap();
        let neighbor_scope_key = WorkspaceScope::from_path(&lossy_neighbor).unwrap();
        assert_ne!(raw_scope_key, neighbor_scope_key);
        let store =
            super::super::supervisor_store::SupervisorStore::new(dir.path().join("supervisor"));
        let snapshot = store.load_state().unwrap();
        let scatter_cohort = |scope: &WorkspaceScope| {
            let mut rows = snapshot
                .continuations
                .values()
                .filter(|record| record.continuation_id.starts_with("scatter_join/"))
                .filter(|record| {
                    record
                        .metadata
                        .get("payload:workspace_scope")
                        .and_then(Value::as_str)
                        == Some(scope.key())
                })
                .map(|record| {
                    let mut segments = record.continuation_id.rsplit('/');
                    let epoch = segments
                        .next()
                        .and_then(|value| value.parse::<u64>().ok())
                        .unwrap();
                    let cohort_hash = segments
                        .next()
                        .and_then(|value| value.parse::<u64>().ok())
                        .unwrap();
                    let terminal_children = record.metadata["payload:terminal_children"]
                        .as_str()
                        .and_then(|value| value.parse::<usize>().ok())
                        .unwrap();
                    (epoch, terminal_children, cohort_hash)
                })
                .collect::<Vec<_>>();
            rows.sort_unstable();
            rows
        };
        let raw_scatter = scatter_cohort(&raw_scope_key);
        assert_eq!(
            raw_scatter
                .iter()
                .map(|(epoch, children, _)| (*epoch, *children))
                .collect::<Vec<_>>(),
            vec![(0, 2), (1, 3), (2, 4)],
            "native, CLI, and MCP must advance one shared raw-byte cohort"
        );
        assert!(
            raw_scatter
                .iter()
                .all(|(_, _, hash)| *hash == raw_scatter[0].2),
            "all raw-byte scatter generations must use one cohort hash"
        );
        let neighbor_scatter = scatter_cohort(&neighbor_scope_key);
        assert_eq!(
            neighbor_scatter
                .iter()
                .map(|(epoch, children, _)| (*epoch, *children))
                .collect::<Vec<_>>(),
            vec![(0, 2)],
            "the real U+FFFD directory must start an independent cohort"
        );
        assert_ne!(raw_scatter[0].2, neighbor_scatter[0].2);

        fn terminal_rows(
            state: &super::super::supervisor_store::SupervisorState,
            scope: &WorkspaceScope,
        ) -> std::collections::HashMap<String, ContinuationStatus> {
            state
                .continuations
                .values()
                .filter(|record| {
                    record
                        .metadata
                        .get("payload:workspace_scope")
                        .and_then(Value::as_str)
                        == Some(scope.key())
                })
                .filter(|record| {
                    matches!(
                        record.metadata.get("reason").and_then(Value::as_str),
                        Some("child_completed" | "scatter_join_complete")
                    )
                })
                .map(|record| (record.continuation_id.clone(), record.status.clone()))
                .collect()
        }
        let raw_rows = terminal_rows(&snapshot, &raw_scope_key);
        let neighbor_rows = terminal_rows(&snapshot, &neighbor_scope_key);
        assert!(
            !raw_rows.is_empty(),
            "raw scope terminal cohort must be nonempty"
        );
        assert!(
            !neighbor_rows.is_empty(),
            "lossy-neighbor terminal cohort must be nonempty"
        );
        assert_eq!(
            raw_rows.len(),
            7,
            "raw cohort must contain six pending rows and one seed"
        );
        assert_eq!(
            neighbor_rows.len(),
            3,
            "lossy-neighbor cohort must contain two pending rows and one seed"
        );
        let raw_pending_ids = raw_rows
            .iter()
            .filter(|(_, status)| **status == ContinuationStatus::Queued)
            .map(|(continuation_id, _)| continuation_id.clone())
            .collect::<HashSet<_>>();
        let neighbor_pending_ids = neighbor_rows
            .iter()
            .filter(|(_, status)| **status == ContinuationStatus::Queued)
            .map(|(continuation_id, _)| continuation_id.clone())
            .collect::<HashSet<_>>();
        assert_eq!(
            raw_pending_ids.len(),
            6,
            "raw scope must expose exactly the six pending purge IDs"
        );
        assert_eq!(
            neighbor_pending_ids.len(),
            2,
            "lossy-neighbor scope must expose exactly the two pending purge IDs"
        );
        let seed_ids_for_scope = |scope: &WorkspaceScope| {
            snapshot
                .continuations
                .values()
                .filter(|record| {
                    record
                        .metadata
                        .get("payload:workspace_scope")
                        .and_then(Value::as_str)
                        == Some(scope.key())
                })
                .filter(|record| {
                    record.status == ContinuationStatus::Completed
                        && record.metadata.get("reason").and_then(Value::as_str)
                            == Some("child_completed")
                })
                .map(|record| record.continuation_id.clone())
                .collect::<HashSet<_>>()
        };
        let raw_seed_ids = seed_ids_for_scope(&raw_scope_key);
        let neighbor_seed_ids = seed_ids_for_scope(&neighbor_scope_key);
        assert_eq!(
            raw_seed_ids.len(),
            1,
            "raw scope must retain its completed seed"
        );
        assert_eq!(
            neighbor_seed_ids.len(),
            1,
            "lossy-neighbor scope must retain its completed seed"
        );
        let completed_rows = |rows: &std::collections::HashMap<String, ContinuationStatus>| {
            rows.keys()
                .map(|continuation_id| (continuation_id.clone(), ContinuationStatus::Completed))
                .collect::<std::collections::HashMap<_, _>>()
        };
        let raw_completed_rows = completed_rows(&raw_rows);
        let neighbor_completed_rows = completed_rows(&neighbor_rows);

        let raw_scope = crate::peers::workspace_scope_encode(&raw_root).unwrap();
        assert_eq!(
            orchestrator.clear_pending_terminal_continuations_for_session(
                &session,
                "tenant-a",
                Some(&raw_scope),
                "stop raw",
            ),
            6,
            "one /stop must purge native, CLI, and MCP continuations in the raw-byte cohort"
        );
        let after_raw_purge = store.load_state().unwrap();
        let raw_rows_after_first_purge = terminal_rows(&after_raw_purge, &raw_scope_key);
        assert_eq!(
            raw_rows_after_first_purge, raw_completed_rows,
            "fresh durable raw cohort load must preserve every row as Completed tombstones"
        );
        assert!(
            raw_rows_after_first_purge
                .values()
                .all(|status| *status == ContinuationStatus::Completed),
            "raw pending IDs must all be Completed after the first purge"
        );
        let neighbor_rows_after_first_purge = terminal_rows(&after_raw_purge, &neighbor_scope_key);
        assert_eq!(
            neighbor_rows_after_first_purge, neighbor_rows,
            "the first purge must preserve both queued neighbor IDs and its completed seed"
        );
        let neighbor_pending_ids_after_first_purge = neighbor_rows_after_first_purge
            .iter()
            .filter(|(_, status)| **status == ContinuationStatus::Queued)
            .map(|(continuation_id, _)| continuation_id.clone())
            .collect::<HashSet<_>>();
        assert_eq!(
            neighbor_pending_ids_after_first_purge, neighbor_pending_ids,
            "the first purge must leave exactly the two previously queued neighbor IDs"
        );
        let neighbor_scope = crate::peers::workspace_scope_encode(&lossy_neighbor).unwrap();
        assert_eq!(
            orchestrator.clear_pending_terminal_continuations_for_session(
                &session,
                "tenant-a",
                Some(&neighbor_scope),
                "stop neighbor",
            ),
            2,
            "the real U+FFFD neighbor must remain after purging the raw-byte cohort"
        );
        let after_neighbor_purge = store.load_state().unwrap();
        let raw_rows_after_second_purge = terminal_rows(&after_neighbor_purge, &raw_scope_key);
        assert_eq!(
            raw_rows_after_second_purge, raw_completed_rows,
            "fresh durable raw cohort load must remain Completed after the neighbor purge"
        );
        let neighbor_rows_after_second_purge =
            terminal_rows(&after_neighbor_purge, &neighbor_scope_key);
        assert_eq!(
            neighbor_rows_after_second_purge, neighbor_completed_rows,
            "fresh durable neighbor cohort load must preserve every row as Completed tombstones"
        );
        assert!(
            neighbor_rows_after_second_purge
                .values()
                .all(|status| *status == ContinuationStatus::Completed),
            "neighbor pending IDs must all be Completed after the second purge"
        );
    }

    fn obstruct_admission_store(orchestrator: &InProcessAgentOrchestrator, root: &Path) {
        orchestrator.configure_supervisor_store(root).unwrap();
        let store = crate::autonomy::supervisor_store::SupervisorStore::new(root);
        std::fs::create_dir_all(store.events_path()).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn admission_r5_cli_does_not_spawn_after_failed_child_write() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_executable(
            &dir,
            "rejected",
            "#!/bin/sh\nprintf started > worker-started\n",
        );
        let orchestrator = InProcessAgentOrchestrator::default();
        obstruct_admission_store(&orchestrator, &dir.path().join("supervisor"));
        let sink = RecordingSink::default();
        let result = run_supervised_cli_specialist(
            &orchestrator,
            &sink,
            SupervisedCliSpecialist::new(
                sample_spec(&dir, "rejected-cli"),
                CliAgentCommandConfig::new(script).cwd(dir.path()),
            ),
        )
        .await;
        assert!(
            !dir.path().join("worker-started").exists(),
            "rejected admission launched a subprocess"
        );
        assert!(result.is_err());
        assert!(
            sink.events().is_empty(),
            "rejected admission published agent events"
        );
    }

    #[tokio::test]
    async fn admission_r5_mcp_does_not_dispatch_after_failed_child_write() {
        let dir = tempfile::tempdir().unwrap();
        let backend = Arc::new(ScriptedMcpBackend::default());
        *backend.artifact.lock().unwrap() = Some(dir.path().join("mcp-report.md"));
        let orchestrator = InProcessAgentOrchestrator::default();
        obstruct_admission_store(&orchestrator, &dir.path().join("supervisor"));
        let sink = RecordingSink::default();
        let result = run_supervised_mcp_specialist(
            &orchestrator,
            &sink,
            SupervisedMcpSpecialist::new(
                sample_spec(&dir, "rejected-mcp"),
                backend.clone(),
                "agent/run",
                json!({"prompt": "review"}),
            ),
        )
        .await;
        assert_eq!(
            backend.calls.load(Ordering::Relaxed),
            0,
            "rejected admission dispatched backend"
        );
        assert!(result.is_err());
        assert!(sink.events().is_empty());
    }

    #[tokio::test]
    async fn mcp_specialist_emits_pings_terminal_output_and_backend_artifact() {
        let dir = tempfile::tempdir().unwrap();
        let backend = Arc::new(ScriptedMcpBackend::default());
        *backend.artifact.lock().unwrap() = Some(dir.path().join("mcp-report.md"));
        let orchestrator = InProcessAgentOrchestrator::default();
        let sink = RecordingSink::default();
        let mut spec = sample_spec(&dir, "mcp-child");
        spec.backend_kind = "mcp_test".to_owned();
        spec.artifacts.clear();
        let summary = run_supervised_mcp_specialist(
            &orchestrator,
            &sink,
            SupervisedMcpSpecialist::new(
                spec.clone(),
                backend.clone(),
                "agent/run",
                json!({ "prompt": "review" }),
            )
            .timeout(Duration::from_secs(1))
            .heartbeat_interval(Duration::from_millis(20)),
        )
        .await
        .unwrap();

        assert_eq!(backend.calls.load(Ordering::Relaxed), 1);
        assert_eq!(summary.status, "completed");
        assert!(summary.ping_count > 0);
        assert_eq!(summary.artifact_ids, vec!["mcp-report"]);

        let status = orchestrator
            .read_agent_status(AgentRequest {
                agent_id: spec.agent_id,
                session_id: Some(spec.session_id),
                profile_id: spec.profile_id,
            })
            .unwrap();
        assert_eq!(status["agent"]["status"], json!("completed"));
        assert!(
            sink.events()
                .iter()
                .any(|(method, params)| *method == methods::AGENT_OUTPUT_DELTA
                    && params["text"] == json!("mcp done"))
        );
    }
    struct DenyRequester;

    #[async_trait]
    impl octos_agent::ToolApprovalRequester for DenyRequester {
        async fn request_approval(
            &self,
            _request: octos_agent::ToolApprovalRequest,
        ) -> octos_agent::ToolApprovalDecision {
            octos_agent::ToolApprovalDecision::Deny
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cli_specialist_policy_denies_approval_before_process_spawn() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("spawned");
        let script = write_executable(
            &dir,
            "agent-policy-approval",
            r#"#!/bin/sh
printf spawned > "$1"
"#,
        );
        let orchestrator = InProcessAgentOrchestrator::default();
        let sink = RecordingSink::default();
        let spec = sample_spec(&dir, "cli-policy-approval");
        let policy = Arc::new(octos_agent::DispatchPolicy {
            require_approval: true,
            approval_requester: Some(Arc::new(DenyRequester)),
            ..Default::default()
        });

        let error = run_supervised_cli_specialist(
            &orchestrator,
            &sink,
            SupervisedCliSpecialist::new(
                spec,
                CliAgentCommandConfig::new(script)
                    .arg(marker.to_string_lossy())
                    .cwd(dir.path()),
            )
            .with_dispatch_policy(policy),
        )
        .await
        .expect_err("approval denial must fail the dispatch");

        assert!(error.contains("approval_denied"), "got: {error}");
        assert!(
            !marker.exists(),
            "denied CLI dispatch must not spawn child process"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cli_specialist_policy_denies_non_allowlisted_env_before_spawn() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("spawned-env");
        let script = write_executable(
            &dir,
            "agent-policy-env",
            r#"#!/bin/sh
printf spawned > "$1"
"#,
        );
        let orchestrator = InProcessAgentOrchestrator::default();
        let sink = RecordingSink::default();
        let spec = sample_spec(&dir, "cli-policy-env");
        let mut allowlist = HashSet::new();
        allowlist.insert("ALLOWED_ONLY".to_owned());
        let policy = Arc::new(octos_agent::DispatchPolicy {
            env_allowlist: Some(allowlist),
            ..Default::default()
        });

        let error = run_supervised_cli_specialist(
            &orchestrator,
            &sink,
            SupervisedCliSpecialist::new(
                spec,
                CliAgentCommandConfig::new(script)
                    .arg(marker.to_string_lossy())
                    .cwd(dir.path())
                    .env("FORBIDDEN_ENV", "secret"),
            )
            .with_dispatch_policy(policy),
        )
        .await
        .expect_err("env allowlist denial must fail the dispatch");

        assert!(error.contains("env_forbidden"), "got: {error}");
        assert!(
            !marker.exists(),
            "env-denied CLI dispatch must not spawn child process"
        );
    }

    #[tokio::test]
    async fn mcp_specialist_policy_denies_approval_before_backend_dispatch() {
        let dir = tempfile::tempdir().unwrap();
        let backend = Arc::new(ScriptedMcpBackend::default());
        *backend.artifact.lock().unwrap() = Some(dir.path().join("mcp-report.md"));
        let orchestrator = InProcessAgentOrchestrator::default();
        let sink = RecordingSink::default();
        let mut spec = sample_spec(&dir, "mcp-policy-approval");
        spec.backend_kind = "mcp_test".to_owned();
        let policy = Arc::new(octos_agent::DispatchPolicy {
            require_approval: true,
            approval_requester: Some(Arc::new(DenyRequester)),
            ..Default::default()
        });

        let error = run_supervised_mcp_specialist(
            &orchestrator,
            &sink,
            SupervisedMcpSpecialist::new(
                spec,
                backend.clone(),
                "agent/run",
                json!({ "prompt": "review" }),
            )
            .with_dispatch_policy(policy),
        )
        .await
        .expect_err("approval denial must fail the MCP dispatch");

        assert!(error.contains("approval_denied"), "got: {error}");
        assert_eq!(backend.calls.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn mcp_specialist_policy_requires_sandbox_before_backend_dispatch() {
        let dir = tempfile::tempdir().unwrap();
        let backend = Arc::new(ScriptedMcpBackend::default());
        *backend.artifact.lock().unwrap() = Some(dir.path().join("mcp-report.md"));
        let orchestrator = InProcessAgentOrchestrator::default();
        let sink = RecordingSink::default();
        let mut spec = sample_spec(&dir, "mcp-policy-sandbox");
        spec.backend_kind = "mcp_test".to_owned();
        let policy = Arc::new(octos_agent::DispatchPolicy {
            require_sandboxed: true,
            ..Default::default()
        });

        let error = run_supervised_mcp_specialist(
            &orchestrator,
            &sink,
            SupervisedMcpSpecialist::new(
                spec,
                backend.clone(),
                "agent/run",
                json!({ "prompt": "review" }),
            )
            .with_dispatch_policy(policy),
        )
        .await
        .expect_err("sandbox requirement must fail unsandboxed MCP dispatch");

        assert!(error.contains("sandbox_required"), "got: {error}");
        assert_eq!(backend.calls.load(Ordering::Relaxed), 0);
    }
}
