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

use crate::sandbox::BLOCKED_ENV_VARS;

/// Filter out dangerous environment variables (code injection vectors).
fn sanitize_env(env: &HashMap<String, String>) -> HashMap<String, String> {
    env.iter()
        .filter(|(k, _)| !BLOCKED_ENV_VARS.contains(&k.as_str()))
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
            let fut = tokio::process::Command::new("cmd")
                .arg("/C")
                .arg(command)
                .current_dir(working_dir)
                .envs(&safe_env)
                .output();
            #[cfg(not(windows))]
            let fut = tokio::process::Command::new("sh")
                .arg("-c")
                .arg(command)
                .current_dir(working_dir)
                .envs(&safe_env)
                .output();
            fut
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn should_exec_local_command() {
        let env = LocalEnvironment;
        let tmp = std::env::temp_dir();
        let output = env
            .exec("echo hello", &tmp, &HashMap::new(), 10)
            .await
            .unwrap();
        assert!(output.success());
        assert_eq!(output.stdout.trim(), "hello");
    }

    #[tokio::test]
    async fn should_read_write_local_file() {
        let dir = TempDir::new().unwrap();
        let env = LocalEnvironment;
        let path = dir.path().join("test.txt");

        env.write_file(&path, "hello world").await.unwrap();
        assert!(env.file_exists(&path).await.unwrap());

        let content = env.read_file(&path).await.unwrap();
        assert_eq!(content, "hello world");
    }

    #[tokio::test]
    async fn should_list_local_dir() {
        let dir = TempDir::new().unwrap();
        let env = LocalEnvironment;

        env.write_file(&dir.path().join("a.txt"), "a")
            .await
            .unwrap();
        env.write_file(&dir.path().join("b.txt"), "b")
            .await
            .unwrap();

        let entries = env.list_dir(dir.path()).await.unwrap();
        assert_eq!(entries.len(), 2);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn should_timeout_local_command() {
        let env = LocalEnvironment;
        let tmp = std::env::temp_dir();
        let result = env.exec("sleep 10", &tmp, &HashMap::new(), 1).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("timed out"));
    }

    #[tokio::test]
    #[cfg(windows)]
    async fn should_timeout_local_command() {
        let env = LocalEnvironment;
        let tmp = std::env::temp_dir();
        let result = env
            .exec("ping -n 11 127.0.0.1 > nul", &tmp, &HashMap::new(), 1)
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("timed out"));
    }

    #[tokio::test]
    async fn should_report_nonzero_exit() {
        let env = LocalEnvironment;
        let tmp = std::env::temp_dir();
        let output = env
            .exec("exit 42", &tmp, &HashMap::new(), 10)
            .await
            .unwrap();
        assert!(!output.success());
        assert_eq!(output.exit_code, 42);
    }

    #[test]
    fn should_report_env_name() {
        assert_eq!(LocalEnvironment.name(), "local");
        assert_eq!(DockerEnvironment::new("abc123", "/app").name(), "docker");
    }

    #[test]
    fn should_filter_blocked_env_vars() {
        let mut env = HashMap::new();
        env.insert("PATH".into(), "/usr/bin".into());
        env.insert("LD_PRELOAD".into(), "/evil.so".into());
        env.insert("DYLD_INSERT_LIBRARIES".into(), "/evil.dylib".into());
        env.insert("MY_VAR".into(), "safe".into());

        let filtered = sanitize_env(&env);
        assert!(filtered.contains_key("PATH"));
        assert!(filtered.contains_key("MY_VAR"));
        assert!(!filtered.contains_key("LD_PRELOAD"));
        assert!(!filtered.contains_key("DYLD_INSERT_LIBRARIES"));
    }

    #[test]
    fn should_reject_docker_path_with_null() {
        assert!(validate_docker_path(Path::new("/tmp/evil\0path")).is_err());
    }

    #[test]
    fn should_reject_docker_path_with_colon() {
        assert!(validate_docker_path(Path::new("/tmp/evil:path")).is_err());
    }

    #[test]
    fn should_reject_docker_path_with_newline() {
        assert!(validate_docker_path(Path::new("/tmp/evil\npath")).is_err());
    }

    #[test]
    fn should_accept_valid_docker_path() {
        assert!(validate_docker_path(Path::new("/app/src/main.rs")).is_ok());
    }
}
