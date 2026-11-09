//! Execution environment abstraction.
//!
//! Provides a trait for swapping between local, Docker, and remote
//! execution backends without modifying individual tools.
//!
//! TODO: Wire into ShellTool and file tools to use the active environment.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use eyre::Result;

use crate::subprocess_env::{EnvAllowlist, sanitize_command_env, should_forward_env_name};

/// Filter out dangerous environment variables (code injection vectors).
fn sanitize_env(env: &HashMap<String, String>) -> HashMap<String, String> {
    let allowlist = EnvAllowlist::empty();
    env.iter()
        .filter(|(k, _)| should_forward_env_name(k, &allowlist))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// Validate a path for Docker argument safety.
/// Rejects paths containing characters that could corrupt argument lists or mount specs.
fn validate_docker_path(path: &Path) -> Result<()> {
    let s = path.to_string_lossy();
    if s.contains(':') || s.contains('\0') || s.contains('\n') || s.contains('\r') {
        eyre::bail!("path contains invalid characters for Docker: {}", s);
    }
    Ok(())
}

/// Output from a command execution.
#[derive(Debug, Clone)]
pub struct ExecOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

impl ExecOutput {
    pub fn success(&self) -> bool {
        self.exit_code == 0
    }
}

/// Abstraction over execution environments (local shell, Docker, K8s, SSH).
#[async_trait]
pub trait ExecEnvironment: Send + Sync {
    /// Execute a command in the environment.
    async fn exec(
        &self,
        command: &str,
        working_dir: &Path,
        env: &HashMap<String, String>,
        timeout_secs: u64,
    ) -> Result<ExecOutput>;

    /// Read a file from the environment.
    async fn read_file(&self, path: &Path) -> Result<String>;

    /// Write a file in the environment.
    async fn write_file(&self, path: &Path, content: &str) -> Result<()>;

    /// Check if a file exists.
    async fn file_exists(&self, path: &Path) -> Result<bool>;

    /// List directory contents.
    async fn list_dir(&self, path: &Path) -> Result<Vec<PathBuf>>;

    /// Environment name (for logging/debugging).
    fn name(&self) -> &str;
}

/// Local execution environment — runs commands on the host system.
pub struct LocalEnvironment;

#[async_trait]
impl ExecEnvironment for LocalEnvironment {
    async fn exec(
        &self,
        command: &str,
        working_dir: &Path,
        env: &HashMap<String, String>,
        timeout_secs: u64,
    ) -> Result<ExecOutput> {
        let safe_env = sanitize_env(env);
        let result = tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), {
            #[cfg(windows)]
            let mut cmd = {
                let mut cmd = tokio::process::Command::new("cmd");
                cmd.arg("/C").arg(command);
                cmd
            };
            #[cfg(not(windows))]
            let mut cmd = {
                let mut cmd = tokio::process::Command::new("sh");
                cmd.arg("-c").arg(command);
                cmd
            };
            cmd.current_dir(working_dir);
            sanitize_command_env(&mut cmd, &EnvAllowlist::empty());
            cmd.envs(&safe_env).output()
        })
        .await;

        match result {
            Ok(Ok(output)) => Ok(ExecOutput {
                stdout: String::from_utf8_lossy(&output.stdout).to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).to_string(),
                exit_code: output.status.code().unwrap_or(-1),
            }),
            Ok(Err(e)) => Err(e.into()),
            Err(_) => eyre::bail!("command timed out after {timeout_secs}s"),
        }
    }

    async fn read_file(&self, path: &Path) -> Result<String> {
        Ok(tokio::fs::read_to_string(path).await?)
    }

    async fn write_file(&self, path: &Path, content: &str) -> Result<()> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        Ok(tokio::fs::write(path, content).await?)
    }

    async fn file_exists(&self, path: &Path) -> Result<bool> {
        Ok(tokio::fs::try_exists(path).await?)
    }

    async fn list_dir(&self, path: &Path) -> Result<Vec<PathBuf>> {
        let mut entries = Vec::new();
        let mut dir = tokio::fs::read_dir(path).await?;
        while let Some(entry) = dir.next_entry().await? {
            entries.push(entry.path());
        }
        Ok(entries)
    }

    fn name(&self) -> &str {
        "local"
    }
}

/// Docker execution environment — runs commands inside a container.
pub struct DockerEnvironment {
    container_id: String,
    _work_dir: PathBuf,
}

impl DockerEnvironment {
    pub fn new(container_id: impl Into<String>, work_dir: impl Into<PathBuf>) -> Self {
        Self {
            container_id: container_id.into(),
            _work_dir: work_dir.into(),
        }
    }
}

#[async_trait]
impl ExecEnvironment for DockerEnvironment {
    async fn exec(
        &self,
        command: &str,
        working_dir: &Path,
        env: &HashMap<String, String>,
        timeout_secs: u64,
    ) -> Result<ExecOutput> {
        validate_docker_path(working_dir)?;
        let wd = working_dir.to_string_lossy();
        let safe_env = sanitize_env(env);
        let mut cmd = tokio::process::Command::new("docker");
        cmd.arg("exec").arg("-w").arg(wd.as_ref());
        for (k, v) in &safe_env {
            cmd.arg("--env").arg(format!("{k}={v}"));
        }
        cmd.arg(&self.container_id).arg("sh").arg("-c").arg(command);
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), cmd.output()).await;

        match result {
            Ok(Ok(output)) => Ok(ExecOutput {
                stdout: String::from_utf8_lossy(&output.stdout).to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).to_string(),
                exit_code: output.status.code().unwrap_or(-1),
            }),
            Ok(Err(e)) => Err(e.into()),
            Err(_) => eyre::bail!("docker exec timed out after {timeout_secs}s"),
        }
    }

    async fn read_file(&self, path: &Path) -> Result<String> {
        validate_docker_path(path)?;
        let output = tokio::process::Command::new("docker")
            .args(["exec", &self.container_id, "cat", &path.to_string_lossy()])
            .output()
            .await?;
        if !output.status.success() {
            eyre::bail!(
                "docker read_file failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    async fn write_file(&self, path: &Path, content: &str) -> Result<()> {
        validate_docker_path(path)?;
        let mut child = tokio::process::Command::new("docker")
            .args([
                "exec",
                "-i",
                &self.container_id,
                "tee",
                &path.to_string_lossy(),
            ])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()?;

        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            stdin.write_all(content.as_bytes()).await?;
        }
        let status = child.wait().await?;
        if !status.success() {
            eyre::bail!(
                "docker write_file failed with exit code {:?}",
                status.code()
            );
        }
        Ok(())
    }

    async fn file_exists(&self, path: &Path) -> Result<bool> {
        validate_docker_path(path)?;
        let output = tokio::process::Command::new("docker")
            .args([
                "exec",
                &self.container_id,
                "test",
                "-e",
                &path.to_string_lossy(),
            ])
            .output()
            .await?;
        Ok(output.status.success())
    }

    async fn list_dir(&self, path: &Path) -> Result<Vec<PathBuf>> {
        validate_docker_path(path)?;
        let output = tokio::process::Command::new("docker")
            .args([
                "exec",
                &self.container_id,
                "ls",
                "-1",
                &path.to_string_lossy(),
            ])
            .output()
            .await?;
        if !output.status.success() {
            eyre::bail!("docker list_dir failed");
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(stdout
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| path.join(l))
            .collect())
    }

    fn name(&self) -> &str {
        "docker"
    }
}
