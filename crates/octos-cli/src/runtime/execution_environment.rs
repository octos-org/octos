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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::process::Command;

    struct FakeSandbox {
        calls: Arc<AtomicUsize>,
        scripts: Arc<Mutex<Vec<String>>>,
        response: &'static str,
        backend: &'static str,
        image: Option<&'static str>,
        posix: bool,
    }

    impl Sandbox for FakeSandbox {
        fn backend_name(&self) -> &'static str {
            self.backend
        }

        fn docker_image(&self) -> Option<&str> {
            self.image
        }

        fn runs_posix_sh(&self) -> bool {
            self.posix
        }

        fn wrap_command(&self, shell_command: &str, _cwd: &Path) -> Command {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.scripts.lock().unwrap().push(shell_command.to_owned());
            let mut command = Command::new("sh");
            command.arg("-c").arg(self.response);
            command
        }
    }

    fn fake(
        response: &'static str,
        backend: &'static str,
    ) -> (Arc<FakeSandbox>, Arc<AtomicUsize>, Arc<Mutex<Vec<String>>>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let scripts = Arc::new(Mutex::new(Vec::new()));
        (
            Arc::new(FakeSandbox {
                calls: calls.clone(),
                scripts: scripts.clone(),
                response,
                backend,
                image: (backend == "docker").then_some("node:22-bookworm"),
                posix: true,
            }),
            calls,
            scripts,
        )
    }

    #[tokio::test]
    async fn should_probe_inside_sandbox_once_and_cache_the_result() {
        let (sandbox, calls, scripts) = fake(
            "printf 'node=available\\nnpm=available\\ngit=missing\\npython=missing\\n'",
            "docker",
        );
        let cache = ExecutionEnvironmentCache::default();
        let first = cache
            .get_or_init("sess-1", sandbox.clone(), Path::new("/tmp"))
            .await;
        let second = cache
            .get_or_init("sess-1", sandbox, Path::new("/tmp"))
            .await;

        assert_eq!(first, second);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(first.backend, ExecutionEnvironmentBackend::Docker);
        assert_eq!(first.docker_image.as_deref(), Some("node:22-bookworm"));
        assert_eq!(
            first.probe_status,
            ExecutionEnvironmentProbeStatus::Complete
        );
        assert_eq!(first.tools["node"], ExecutionToolAvailability::Available);
        assert_eq!(first.tools["git"], ExecutionToolAvailability::Missing);
        let scripts = scripts.lock().unwrap();
        assert_eq!(scripts.len(), 1);
        for tool in TOOL_NAMES {
            assert!(scripts[0].contains(tool));
        }
        assert!(scripts[0].contains("command -v"));
    }

    #[tokio::test]
    async fn should_report_unsupported_shell_without_probe_failure() {
        let (mut sandbox, calls, scripts) = fake("exit 1", "appcontainer");
        Arc::get_mut(&mut sandbox).unwrap().posix = false;
        let cache = ExecutionEnvironmentCache::default();
        let first = cache
            .get_or_init("sess-unsupported", sandbox.clone(), Path::new("/tmp"))
            .await;
        let second = cache
            .get_or_init("sess-unsupported", sandbox, Path::new("/tmp"))
            .await;

        assert_eq!(first, second);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(scripts.lock().unwrap().is_empty());
        assert_eq!(first.backend, ExecutionEnvironmentBackend::AppContainer);
        assert_eq!(
            first.probe_status,
            ExecutionEnvironmentProbeStatus::Unsupported
        );
        assert_eq!(first.probe_error, None);
        assert!(
            first
                .tools
                .values()
                .all(|status| *status == ExecutionToolAvailability::Unknown)
        );
    }

    #[tokio::test]
    async fn should_cache_probe_failure_without_guessing_tool_availability() {
        let (sandbox, calls, _) = fake("exit 1", "bwrap");
        let cache = ExecutionEnvironmentCache::default();
        let result = cache
            .get_or_init("sess-1", sandbox.clone(), Path::new("/tmp"))
            .await;
        let cached = cache
            .get_or_init("sess-1", sandbox, Path::new("/tmp"))
            .await;

        assert_eq!(result, cached);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(result.backend, ExecutionEnvironmentBackend::Bwrap);
        assert_eq!(result.probe_status, ExecutionEnvironmentProbeStatus::Failed);
        assert!(result.probe_error.is_some());
        assert!(result.probed_at <= Utc::now());
        assert!(
            result
                .tools
                .values()
                .all(|status| *status == ExecutionToolAvailability::Unknown)
        );
    }

    #[tokio::test]
    #[ignore = "requires local Docker and the node:22-bookworm image"]
    async fn should_report_tools_from_the_real_docker_backend() {
        let mut config = octos_agent::SandboxConfig {
            mode: octos_agent::SandboxMode::Docker,
            allow_network: false,
            workspace_write: false,
            ..Default::default()
        };
        config.docker.image = "node:22-bookworm".to_owned();
        let sandbox: Arc<dyn Sandbox> = Arc::from(octos_agent::create_sandbox(&config));
        let cwd = tempfile::tempdir().unwrap();
        let result = probe("sess-docker", sandbox, cwd.path()).await;

        assert_eq!(result.backend, ExecutionEnvironmentBackend::Docker);
        assert_eq!(result.docker_image.as_deref(), Some("node:22-bookworm"));
        assert_eq!(
            result.probe_status,
            ExecutionEnvironmentProbeStatus::Complete
        );
        assert_eq!(result.tools["node"], ExecutionToolAvailability::Available);
        assert_eq!(result.tools["npm"], ExecutionToolAvailability::Available);
        assert_eq!(result.tools["git"], ExecutionToolAvailability::Available);
        // The full Node image has python3 but no `python` alias (T-0066).
        assert_eq!(result.tools["python"], ExecutionToolAvailability::Missing);
    }
}
