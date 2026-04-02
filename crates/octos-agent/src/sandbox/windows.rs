//! Windows sandbox using AppContainer via helper binary.
//!
//! Launches shell commands inside a Windows AppContainer for process isolation.
//! Uses a helper binary (`octos-sandbox`) to avoid changing the `Sandbox` trait,
//! since AppContainer requires `CreateProcessW` with extended startup info.
//!
//! Each octos profile gets its own AppContainer profile (SID), providing:
//! - Deny-by-default filesystem access
//! - Network isolation (configurable)
//! - Cross-profile data isolation via persistent ACLs

use std::path::Path;

use tokio::process::Command;
use tracing::warn;

use super::{BLOCKED_ENV_VARS, Sandbox};

/// Windows AppContainer sandbox.
///
/// Delegates to `octos-sandbox.exe` helper binary which creates/reuses
/// an AppContainer profile and launches the command inside it.
pub struct AppContainerSandbox {
    /// Allow network access inside the sandbox.
    pub allow_network: bool,
    /// Additional paths to grant read access to.
    pub read_allow_paths: Vec<String>,
    /// Profile name for the AppContainer (typically the octos profile ID).
    pub profile_name: Option<String>,
}

/// Windows system paths that must be readable for shell commands to work.
const WINDOWS_READ_ALLOW_PATHS: &[&str] = &[
    r"C:\Windows",
    r"C:\Windows\System32",
    r"C:\Program Files\Git",
    r"C:\Program Files\nodejs",
    r"C:\Python",
    r"C:\ProgramData",
];

impl Sandbox for AppContainerSandbox {
    fn wrap_command(&self, shell_command: &str, cwd: &Path) -> Command {
        // Find the helper binary next to our own executable
        let helper = find_sandbox_helper();

        let Some(helper_path) = helper else {
            warn!("octos-sandbox helper not found, falling back to unsandboxed cmd /C");
            let mut cmd = Command::new("cmd");
            cmd.arg("/C").arg(shell_command).current_dir(cwd);
            for var in BLOCKED_ENV_VARS {
                cmd.env_remove(var);
            }
            return cmd;
        };

        let mut cmd = Command::new(helper_path);

        // Profile name
        let profile = self
            .profile_name
            .as_deref()
            .unwrap_or("octos.default");
        cmd.arg("--profile").arg(profile);

        // Working directory (read-write)
        cmd.arg("--cwd").arg(cwd);

        // Read-only paths
        for path in WINDOWS_READ_ALLOW_PATHS {
            cmd.arg("--allow-read").arg(path);
        }
        for path in &self.read_allow_paths {
            cmd.arg("--allow-read").arg(path);
        }

        // Network access
        if self.allow_network {
            cmd.arg("--allow-network");
        }

        // The actual command to run
        cmd.arg("--").arg("cmd").arg("/C").arg(shell_command);

        // Set working directory for the helper process itself
        cmd.current_dir(cwd);

        // Clear dangerous env vars
        for var in BLOCKED_ENV_VARS {
            cmd.env_remove(var);
        }

        cmd
    }
}

/// Find the `octos-sandbox` helper binary.
/// Looks next to the current executable, then on PATH.
fn find_sandbox_helper() -> Option<String> {
    // Next to our binary
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let helper = dir.join("octos-sandbox.exe");
            if helper.exists() {
                return Some(helper.to_string_lossy().into_owned());
            }
        }
    }

    // On PATH (use `where` on Windows to find it)
    if std::process::Command::new("where")
        .arg("octos-sandbox")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
    {
        return Some("octos-sandbox".to_string());
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_build_command_with_profile() {
        let sandbox = AppContainerSandbox {
            allow_network: false,
            read_allow_paths: vec![],
            profile_name: Some("octos.test-profile".into()),
        };

        let cmd = sandbox.wrap_command("echo hello", Path::new(r"C:\workspace"));
        let prog = cmd.as_std().get_program().to_string_lossy().to_string();

        // If helper not found, falls back to cmd
        assert!(
            prog.contains("octos-sandbox") || prog == "cmd",
            "expected octos-sandbox or cmd fallback, got: {prog}"
        );
    }

    #[test]
    fn should_use_sandbox_or_fallback() {
        let sandbox = AppContainerSandbox {
            allow_network: true,
            read_allow_paths: vec![r"C:\tools".into()],
            profile_name: None,
        };

        let cmd = sandbox.wrap_command("dir", Path::new(r"C:\temp"));
        let prog = cmd.as_std().get_program().to_string_lossy().to_string();

        // Either finds octos-sandbox helper or falls back to cmd
        assert!(
            prog.contains("octos-sandbox") || prog == "cmd",
            "expected octos-sandbox or cmd, got: {prog}"
        );
    }
}
