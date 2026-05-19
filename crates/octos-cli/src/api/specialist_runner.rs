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
}

#[derive(Clone)]
pub(crate) struct SupervisedMcpSpecialist {
    pub(crate) spec: SupervisedSpecialistSpec,
    pub(crate) backend: Arc<dyn McpAgentBackend>,
    pub(crate) tool_name: String,
    pub(crate) task: Value,
    pub(crate) timeout: Duration,
    pub(crate) heartbeat_interval: Duration,
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
        }
    }

    pub(crate) fn heartbeat_interval(mut self, interval: Duration) -> Self {
        self.heartbeat_interval = interval;
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
    emit_agent_updated(
        sink,
        &request.spec.session_id,
        orchestrator.upsert_agent(upsert_for_spec(&request.spec, "running", None)),
    );

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

    emit_agent_updated(
        sink,
        &request.spec.session_id,
        orchestrator.upsert_agent(upsert_for_spec(&request.spec, "running", None)),
    );

    let context_contract = DispatchContextContract::external_unmanaged(
        "supervised_mcp_specialist_context_payload_not_wired",
    )
    .with_parent_session_key(Some(request.spec.session_id.to_string()))
    .with_child_session_key(Some(request.spec.agent_id.clone()));
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

    #[derive(Default)]
    struct ScriptedMcpBackend {
        calls: AtomicUsize,
        artifact: Mutex<Option<PathBuf>>,
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
}
