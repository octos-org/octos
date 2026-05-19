//! Process boundary for a future CLI-agent adapter.
//!
//! This module is intentionally not wired to AppUI yet. It gives the runtime a
//! small, testable subprocess boundary for command construction, transcript
//! capture, lifecycle termination, and declared output artifacts.
//!
//! TODO(M15): connect this boundary to the server-owned coding runtime once the
//! effective profile/tool/sandbox policy factory is ready.
//! TODO(M15): replace direct process termination with platform-specific graceful
//! shutdown when the child protocol grows an explicit close handshake.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use eyre::{Result, bail};
use serde::Serialize;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;

/// Default upper bound for a CLI-agent dispatch.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);
/// Per-stream transcript capture cap. The reader keeps draining after this
/// limit so the child cannot block on a full pipe, but retained text is bounded.
pub const MAX_TRANSCRIPT_BYTES_PER_STREAM: usize = 1024 * 1024;

/// A command configuration that cannot be interpreted as a shell command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliAgentCommandConfig {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: BTreeMap<String, String>,
    pub timeout: Duration,
    pub declared_artifacts: Vec<PathBuf>,
}

impl CliAgentCommandConfig {
    /// Build a command config from a program path and argv list.
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            cwd: None,
            env: BTreeMap::new(),
            timeout: DEFAULT_TIMEOUT,
            declared_artifacts: Vec::new(),
        }
    }

    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn args(mut self, args: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn declared_artifact(mut self, path: impl Into<PathBuf>) -> Self {
        self.declared_artifacts.push(path.into());
        self
    }

    fn validate(&self) -> Result<()> {
        if self.program.as_os_str().is_empty() {
            bail!("CLI-agent command program must not be empty");
        }
        if self.timeout.is_zero() {
            bail!("CLI-agent command timeout must be greater than zero");
        }
        for key in self.env.keys() {
            if key.is_empty() || key.contains('=') || key.contains('\0') {
                bail!("invalid CLI-agent environment variable name: {key:?}");
            }
        }
        for arg in &self.args {
            if arg.contains('\0') {
                bail!("CLI-agent command arguments must not contain NUL bytes");
            }
        }
        Ok(())
    }
}

/// Captured process output split by stream.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct CliAgentTranscript {
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

/// A declared artifact path and whether it exists at process completion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CliAgentArtifact {
    pub path: PathBuf,
    pub exists: bool,
}

/// Terminal state for the subprocess boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CliAgentTermination {
    Exited { code: Option<i32> },
    TimedOut,
    Cancelled,
    Closed,
}

/// Completed process run, including captured transcript and declared artifacts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CliAgentRunResult {
    pub termination: CliAgentTermination,
    pub transcript: CliAgentTranscript,
    pub artifacts: Vec<CliAgentArtifact>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequestedStop {
    Cancelled,
    Closed,
}

/// Live CLI-agent child process handle.
pub struct CliAgentProcess {
    child: Child,
    stdout_task: JoinHandle<PipeCapture>,
    stderr_task: JoinHandle<PipeCapture>,
    timeout: Duration,
    declared_artifacts: Vec<PathBuf>,
    requested_stop: Option<RequestedStop>,
}

impl CliAgentProcess {
    /// Spawn a CLI-agent process using argv-style command construction only.
    pub fn spawn(config: CliAgentCommandConfig) -> Result<Self> {
        config.validate()?;

        let mut command = Command::new(&config.program);
        command
            .args(&config.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        if let Some(cwd) = &config.cwd {
            command.current_dir(cwd);
        }
        command.envs(&config.env);

        let mut child = command.spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| eyre::eyre!("CLI-agent stdout pipe unavailable"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| eyre::eyre!("CLI-agent stderr pipe unavailable"))?;

        Ok(Self {
            child,
            stdout_task: tokio::spawn(read_pipe(stdout)),
            stderr_task: tokio::spawn(read_pipe(stderr)),
            timeout: config.timeout,
            declared_artifacts: resolve_declared_artifacts(
                config.cwd.as_ref(),
                config.declared_artifacts,
            )?,
            requested_stop: None,
        })
    }

    /// Cancel the process. The final [`wait`](Self::wait) reports `cancelled`.
    pub async fn cancel(&mut self) -> Result<()> {
        self.requested_stop = Some(RequestedStop::Cancelled);
        self.kill_child().await
    }

    /// Close the process. Until a protocol close handshake exists, this is a
    /// best-effort process termination distinct from user cancellation.
    pub async fn close(&mut self) -> Result<()> {
        self.requested_stop = Some(RequestedStop::Closed);
        self.kill_child().await
    }

    /// Wait for completion, enforcing the configured timeout.
    pub async fn wait(mut self) -> Result<CliAgentRunResult> {
        let status = tokio::time::timeout(self.timeout, self.child.wait()).await;
        let termination = match (self.requested_stop, status) {
            (Some(RequestedStop::Cancelled), _) => CliAgentTermination::Cancelled,
            (Some(RequestedStop::Closed), _) => CliAgentTermination::Closed,
            (None, Ok(Ok(status))) => exit_termination(status),
            (None, Ok(Err(error))) => {
                self.stdout_task.abort();
                self.stderr_task.abort();
                return Err(error.into());
            }
            (None, Err(_)) => {
                if let Err(error) = self.kill_child().await {
                    self.stdout_task.abort();
                    self.stderr_task.abort();
                    return Err(error.wrap_err("failed to kill timed-out CLI-agent process"));
                }
                CliAgentTermination::TimedOut
            }
        };
        let (stdout, stdout_truncated) = task_utf8(self.stdout_task).await?;
        let (stderr, stderr_truncated) = task_utf8(self.stderr_task).await?;

        Ok(CliAgentRunResult {
            termination,
            transcript: CliAgentTranscript {
                stdout,
                stderr,
                stdout_truncated,
                stderr_truncated,
            },
            artifacts: self
                .declared_artifacts
                .into_iter()
                .map(|path| {
                    let exists = path.exists();
                    CliAgentArtifact { path, exists }
                })
                .collect(),
        })
    }

    async fn kill_child(&mut self) -> Result<()> {
        match self.child.id() {
            Some(_) => {
                self.child.kill().await?;
            }
            None => {
                let _ = self.child.start_kill();
            }
        }
        Ok(())
    }
}

/// Spawn and wait for a CLI-agent command in one call.
pub async fn run_cli_agent_command(config: CliAgentCommandConfig) -> Result<CliAgentRunResult> {
    CliAgentProcess::spawn(config)?.wait().await
}

fn exit_termination(status: ExitStatus) -> CliAgentTermination {
    CliAgentTermination::Exited {
        code: status.code(),
    }
}

fn resolve_declared_artifacts(
    cwd: Option<&PathBuf>,
    declared_artifacts: Vec<PathBuf>,
) -> Result<Vec<PathBuf>> {
    Ok(declared_artifacts
        .into_iter()
        .map(|path| {
            if path.is_relative() {
                cwd.map(|cwd| cwd.join(&path)).unwrap_or(path)
            } else {
                path
            }
        })
        .collect())
}

#[derive(Debug, Default)]
struct PipeCapture {
    bytes: Vec<u8>,
    truncated: bool,
}

async fn read_pipe<R>(mut reader: R) -> PipeCapture
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut capture = PipeCapture::default();
    let mut chunk = [0_u8; 8192];
    while let Ok(read) = reader.read(&mut chunk).await {
        if read == 0 {
            break;
        }
        let remaining = MAX_TRANSCRIPT_BYTES_PER_STREAM.saturating_sub(capture.bytes.len());
        if remaining > 0 {
            let keep = remaining.min(read);
            capture.bytes.extend_from_slice(&chunk[..keep]);
        }
        if read > remaining {
            capture.truncated = true;
        }
    }
    capture
}

async fn task_utf8(task: JoinHandle<PipeCapture>) -> Result<(String, bool)> {
    let capture = task.await?;
    Ok((
        String::from_utf8_lossy(&capture.bytes).into_owned(),
        capture.truncated,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[cfg(unix)]
    #[tokio::test]
    async fn captures_stdout_stderr_and_declared_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        let artifact = dir.path().join("report.md");
        let script = write_executable(
            &dir,
            "agent-fixture",
            r#"#!/bin/sh
printf 'hello stdout\n'
printf 'hello stderr\n' >&2
printf '# report\n' > "$1"
"#,
        );

        let result = run_cli_agent_command(
            CliAgentCommandConfig::new(script)
                .arg(artifact.to_string_lossy())
                .declared_artifact(&artifact),
        )
        .await
        .unwrap();

        assert_eq!(
            result.termination,
            CliAgentTermination::Exited { code: Some(0) }
        );
        assert_eq!(result.transcript.stdout, "hello stdout\n");
        assert_eq!(result.transcript.stderr, "hello stderr\n");
        assert_eq!(
            result.artifacts,
            vec![CliAgentArtifact {
                path: artifact,
                exists: true
            }]
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn relative_declared_artifacts_resolve_against_child_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_executable(
            &dir,
            "relative-artifact-agent",
            r#"#!/bin/sh
printf '# report\n' > report.md
"#,
        );

        let result = run_cli_agent_command(
            CliAgentCommandConfig::new(script)
                .cwd(dir.path())
                .declared_artifact("report.md"),
        )
        .await
        .unwrap();

        assert_eq!(
            result.artifacts,
            vec![CliAgentArtifact {
                path: dir.path().join("report.md"),
                exists: true
            }]
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn transcript_capture_is_bounded_and_marks_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_executable(
            &dir,
            "large-output-agent",
            r#"#!/bin/sh
perl -e 'print "x" x (1024 * 1024 + 64)'
"#,
        );

        let result = run_cli_agent_command(CliAgentCommandConfig::new(script))
            .await
            .unwrap();

        assert_eq!(
            result.transcript.stdout.len(),
            MAX_TRANSCRIPT_BYTES_PER_STREAM
        );
        assert!(result.transcript.stdout_truncated);
        assert!(!result.transcript.stderr_truncated);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn times_out_and_kills_child() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("finished");
        let script = write_executable(
            &dir,
            "slow-agent",
            r#"#!/bin/sh
sleep 2
printf done > "$1"
"#,
        );

        let result = run_cli_agent_command(
            CliAgentCommandConfig::new(script)
                .arg(marker.to_string_lossy())
                .timeout(Duration::from_millis(100)),
        )
        .await
        .unwrap();

        assert_eq!(result.termination, CliAgentTermination::TimedOut);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!marker.exists(), "timed-out child should not keep running");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancel_reports_cancelled_and_stops_process() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("finished");
        let script = write_executable(
            &dir,
            "cancel-agent",
            r#"#!/bin/sh
sleep 2
printf done > "$1"
"#,
        );

        let mut process = CliAgentProcess::spawn(
            CliAgentCommandConfig::new(script)
                .arg(marker.to_string_lossy())
                .timeout(Duration::from_secs(5)),
        )
        .unwrap();
        process.cancel().await.unwrap();
        let result = process.wait().await.unwrap();

        assert_eq!(result.termination, CliAgentTermination::Cancelled);
        assert!(!marker.exists(), "cancelled child should not keep running");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn close_reports_closed_and_stops_process() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("finished");
        let script = write_executable(
            &dir,
            "close-agent",
            r#"#!/bin/sh
sleep 2
printf done > "$1"
"#,
        );

        let mut process = CliAgentProcess::spawn(
            CliAgentCommandConfig::new(script)
                .arg(marker.to_string_lossy())
                .timeout(Duration::from_secs(5)),
        )
        .unwrap();
        process.close().await.unwrap();
        let result = process.wait().await.unwrap();

        assert_eq!(result.termination, CliAgentTermination::Closed);
        assert!(!marker.exists(), "closed child should not keep running");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn argv_config_does_not_invoke_shell_metacharacters() {
        let dir = tempfile::tempdir().unwrap();
        let injected = dir.path().join("pwned");
        let script = write_executable(
            &dir,
            "echo-argv",
            r#"#!/bin/sh
printf '%s\n' "$1"
"#,
        );

        let payload = format!("literal; touch {}", injected.display());
        let result = run_cli_agent_command(CliAgentCommandConfig::new(script).arg(payload.clone()))
            .await
            .unwrap();

        assert_eq!(
            result.termination,
            CliAgentTermination::Exited { code: Some(0) }
        );
        assert_eq!(result.transcript.stdout, format!("{payload}\n"));
        assert!(
            !injected.exists(),
            "argv payload must not be shell-evaluated"
        );
    }

    #[test]
    fn rejects_unsafe_command_config_shapes() {
        assert!(CliAgentCommandConfig::new("").validate().is_err());
        assert!(
            CliAgentCommandConfig::new("agent")
                .timeout(Duration::ZERO)
                .validate()
                .is_err()
        );
        assert!(
            CliAgentCommandConfig::new("agent")
                .env("BAD=KEY", "value")
                .validate()
                .is_err()
        );
        assert!(
            CliAgentCommandConfig::new("agent")
                .arg("bad\0arg")
                .validate()
                .is_err()
        );
    }
}
