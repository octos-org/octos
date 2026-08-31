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
    /// When `false`, the workspace cwd is granted READ-ONLY (the helper is
    /// invoked with `--readonly-cwd`) so shell commands cannot mutate the
    /// workspace under a read-only permission profile (codex P1). Default
    /// constructions use `true` (read-write cwd) for backward compatibility.
    pub workspace_write: bool,
}

/// Windows system paths that must be readable for shell commands to work.
/// Windows system paths that must be readable for shell commands to work.
/// Only paths that actually exist will be passed to the helper.
const WINDOWS_READ_ALLOW_PATHS: &[&str] = &[
    r"C:\Windows",
    r"C:\Program Files\Git",
    r"C:\Program Files\nodejs",
    r"C:\ProgramData",
];

impl Sandbox for AppContainerSandbox {
    /// Report no-op enforcement when the `octos-sandbox` helper is unavailable.
    ///
    /// Without the helper, [`Self::wrap_command`] falls back to an unsandboxed
    /// `cmd /C`, so the AppContainer provides no confinement. Surfacing that as
    /// `is_noop() == true` lets fail-closed callers (e.g. the `mcp-serve` path)
    /// refuse to run tools rather than silently executing host commands under a
    /// configured-but-inert sandbox — matching how the Linux Landlock backend
    /// refuses when its helper is missing.
    fn is_noop(&self) -> bool {
        find_sandbox_helper().is_none()
    }

    fn workspace_scratch_writable(&self) -> bool {
        // A #1976 fence already degraded `workspace_write` to `false` at
        // construction (`fence_degraded_workspace_write`), so this single
        // flag covers both the read-only profile and the fenced one.
        self.workspace_write
    }

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

        // Profile name — prefix with "octos." for AppContainer namespace
        let raw = self.profile_name.as_deref().unwrap_or("default");
        let profile = if raw.starts_with("octos.") {
            raw.to_string()
        } else {
            format!("octos.{raw}")
        };
        cmd.arg("--profile").arg(&profile);

        // Working directory. Read-write by default; read-only when the
        // permission profile denies workspace writes (`--sandbox read-only`)
        // so shell commands cannot mutate the workspace (codex P1).
        cmd.arg("--cwd").arg(cwd);
        if !self.workspace_write {
            cmd.arg("--readonly-cwd");
        }

        // Read-only paths — only pass paths that exist
        for path in WINDOWS_READ_ALLOW_PATHS {
            if Path::new(path).exists() {
                cmd.arg("--allow-read").arg(path);
            }
        }
        for path in &self.read_allow_paths {
            if Path::new(path).exists() {
                cmd.arg("--allow-read").arg(path);
            }
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
            workspace_write: true,
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
            workspace_write: true,
        };

        let cmd = sandbox.wrap_command("dir", Path::new(r"C:\temp"));
        let prog = cmd.as_std().get_program().to_string_lossy().to_string();

        // Either finds octos-sandbox helper or falls back to cmd
        assert!(
            prog.contains("octos-sandbox") || prog == "cmd",
            "expected octos-sandbox or cmd, got: {prog}"
        );
    }

    #[test]
    fn should_pass_readonly_cwd_to_helper_when_workspace_write_disabled() {
        // P1 (codex): a read-only permission profile must invoke the
        // octos-sandbox helper with `--readonly-cwd` so the AppContainer ACL
        // grants the cwd read-only. When the helper is absent the sandbox
        // degrades to unsandboxed `cmd` (pre-existing warning path).
        let sandbox = AppContainerSandbox {
            allow_network: false,
            read_allow_paths: vec![],
            profile_name: None,
            workspace_write: false,
        };
        let cmd = sandbox.wrap_command("echo hello", Path::new(r"C:\workspace"));
        let prog = cmd.as_std().get_program().to_string_lossy().to_string();
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        if prog.contains("octos-sandbox") {
            assert!(
                args.iter().any(|a| a == "--readonly-cwd"),
                "read-only profile must pass --readonly-cwd, args: {args:?}"
            );
        }
    }

    #[test]
    fn should_not_pass_readonly_cwd_to_helper_when_workspace_write_enabled() {
        let sandbox = AppContainerSandbox {
            allow_network: false,
            read_allow_paths: vec![],
            profile_name: None,
            workspace_write: true,
        };
        let cmd = sandbox.wrap_command("echo hello", Path::new(r"C:\workspace"));
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert!(
            !args.iter().any(|a| a == "--readonly-cwd"),
            "writable profile must NOT pass --readonly-cwd, args: {args:?}"
        );
    }
}
