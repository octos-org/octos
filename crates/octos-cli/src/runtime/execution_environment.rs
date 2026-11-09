//! Read-only, cached facts about the session's actual command environment.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use octos_agent::Sandbox;
use octos_core::ui_protocol::{
    ExecutionEnvironmentBackend, ExecutionEnvironmentProbeStatus, ExecutionToolAvailability,
    SessionExecutionEnvironmentReadResult,
};
use tokio::sync::OnceCell;
use tokio::time::timeout;

const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const TOOL_NAMES: [&str; 4] = ["node", "npm", "git", "python"];
const POSIX_PROBE: &str = "for tool in node npm git python; do if command -v \"$tool\" >/dev/null 2>&1; then printf '%s=available\\n' \"$tool\"; else printf '%s=missing\\n' \"$tool\"; fi; done";

/// One probe per materialized session runtime. Runtime invalidation on an
/// effective permission/profile change drops this cell with the runtime.
#[derive(Default)]
pub struct ExecutionEnvironmentCache {
    value: OnceCell<SessionExecutionEnvironmentReadResult>,
}

impl ExecutionEnvironmentCache {
    pub async fn get_or_init(
        &self,
        session_id: &str,
        sandbox: Arc<dyn Sandbox>,
        cwd: &Path,
    ) -> SessionExecutionEnvironmentReadResult {
        self.value
            .get_or_init(|| probe(session_id, sandbox, cwd))
            .await
            .clone()
    }
}

async fn probe(
    session_id: &str,
    sandbox: Arc<dyn Sandbox>,
    cwd: &Path,
) -> SessionExecutionEnvironmentReadResult {
    if sandbox.refusal().is_some() {
        return failed_result(session_id, &sandbox, "sandbox refused command execution");
    }
    if !sandbox.runs_posix_sh() {
        return unsupported_result(session_id, &sandbox);
    }
    let mut command = sandbox.wrap_command(POSIX_PROBE, cwd);
    command.kill_on_drop(true);
    let output = match timeout(PROBE_TIMEOUT, command.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(_)) => {
            return failed_result(session_id, &sandbox, "execution probe failed to start");
        }
        Err(_) => return failed_result(session_id, &sandbox, "execution probe timed out"),
    };
    if !output.status.success() {
        return failed_result(session_id, &sandbox, "execution probe was refused");
    }
    let Ok(stdout) = std::str::from_utf8(&output.stdout) else {
        return failed_result(
            session_id,
            &sandbox,
            "execution probe returned invalid output",
        );
    };
    let Some(tools) = parse_tools(stdout) else {
        return failed_result(
            session_id,
            &sandbox,
            "execution probe returned invalid output",
        );
    };
    SessionExecutionEnvironmentReadResult {
        session_id: session_id.to_owned(),
        backend: backend(&sandbox),
        docker_image: sandbox.docker_image().map(str::to_owned),
        tools,
        probe_status: ExecutionEnvironmentProbeStatus::Complete,
        probed_at: Utc::now(),
        probe_error: None,
    }
}

fn parse_tools(stdout: &str) -> Option<BTreeMap<String, ExecutionToolAvailability>> {
    let mut tools = BTreeMap::new();
    for line in stdout.lines().filter(|line| !line.is_empty()) {
        let (name, value) = line.split_once('=')?;
        if !TOOL_NAMES.contains(&name) || tools.contains_key(name) {
            return None;
        }
        let availability = match value {
            "available" => ExecutionToolAvailability::Available,
            "missing" => ExecutionToolAvailability::Missing,
            _ => return None,
        };
        tools.insert(name.to_owned(), availability);
    }
    (tools.len() == TOOL_NAMES.len()).then_some(tools)
}

fn unknown_tools() -> BTreeMap<String, ExecutionToolAvailability> {
    TOOL_NAMES
        .into_iter()
        .map(|name| (name.to_owned(), ExecutionToolAvailability::Unknown))
        .collect()
}

fn backend(sandbox: &Arc<dyn Sandbox>) -> ExecutionEnvironmentBackend {
    match sandbox.backend_name() {
        "bwrap" => ExecutionEnvironmentBackend::Bwrap,
        "landlock" => ExecutionEnvironmentBackend::Landlock,
        "docker" => ExecutionEnvironmentBackend::Docker,
        "macos" => ExecutionEnvironmentBackend::Macos,
        "appcontainer" => ExecutionEnvironmentBackend::AppContainer,
        "none" => ExecutionEnvironmentBackend::None,
        "unavailable" => ExecutionEnvironmentBackend::Unavailable,
        _ => ExecutionEnvironmentBackend::Unknown,
    }
}

fn unsupported_result(
    session_id: &str,
    sandbox: &Arc<dyn Sandbox>,
) -> SessionExecutionEnvironmentReadResult {
    SessionExecutionEnvironmentReadResult {
        session_id: session_id.to_owned(),
        backend: backend(sandbox),
        docker_image: sandbox.docker_image().map(str::to_owned),
        tools: unknown_tools(),
        probe_status: ExecutionEnvironmentProbeStatus::Unsupported,
        probed_at: Utc::now(),
        probe_error: None,
    }
}

fn failed_result(
    session_id: &str,
    sandbox: &Arc<dyn Sandbox>,
    error: &str,
) -> SessionExecutionEnvironmentReadResult {
    SessionExecutionEnvironmentReadResult {
        session_id: session_id.to_owned(),
        backend: backend(sandbox),
        docker_image: sandbox.docker_image().map(str::to_owned),
        tools: unknown_tools(),
        probe_status: ExecutionEnvironmentProbeStatus::Failed,
        probed_at: Utc::now(),
        probe_error: Some(error.to_owned()),
    }
}
