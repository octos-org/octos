//! Linux container sandbox using the octos-sandbox Landlock/seccomp helper.

use std::path::Path;

use tokio::process::Command;

use super::{BLOCKED_ENV_VARS, Sandbox, find_sandbox_helper_path};

/// Linux sandbox for unprivileged containers.
///
/// Delegates enforcement to the `octos-sandbox` helper so Landlock and
/// seccomp setup happens in the helper process before it `exec`s the shell.
pub struct LinuxContainerSandbox {
    /// Allow network access inside the sandbox.
    pub allow_network: bool,
    /// Additional paths to grant read access to.
    pub read_allow_paths: Vec<String>,
    /// Optional profile label, currently accepted for parity with other helpers.
    pub profile_name: Option<String>,
    /// When `false`, the workspace cwd is granted READ-ONLY (the helper is
    /// invoked with `--readonly-cwd`) so shell commands cannot mutate the
    /// workspace under a read-only permission profile (codex P1). Default
    /// constructions use `true` (read-write cwd) for backward compatibility.
    pub workspace_write: bool,
}

impl Sandbox for LinuxContainerSandbox {
    fn backend_name(&self) -> &'static str {
        "landlock"
    }

    fn runs_posix_sh(&self) -> bool {
        // The helper terminates in `sh -c` (Linux), as does its fail-closed
        // error command.
        true
    }

    fn wrap_command(&self, shell_command: &str, cwd: &Path) -> Command {
        let Some(helper_path) = find_sandbox_helper_path() else {
            tracing::error!("octos-sandbox helper not found, refusing unsandboxed Linux command");
            let mut cmd = Command::new("sh");
            cmd.arg("-c")
                .arg("echo 'sandbox error: octos-sandbox helper not found' >&2; exit 1")
                .current_dir(cwd);
            for var in BLOCKED_ENV_VARS {
                cmd.env_remove(var);
            }
            return cmd;
        };

        let mut cmd = Command::new(helper_path);
        cmd.arg("--profile")
            .arg(self.profile_name.as_deref().unwrap_or("octos.linux"))
            .arg("--cwd")
            .arg(cwd);

        // Read-only permission profile: grant the workspace read-only so the
        // helper's Landlock policy denies workspace writes (`touch newfile`).
        if !self.workspace_write {
            cmd.arg("--readonly-cwd");
        }

        for path in &self.read_allow_paths {
            if Path::new(path).exists() {
                cmd.arg("--allow-read").arg(path);
            }
        }

        if self.allow_network {
            cmd.arg("--allow-network");
        }

        cmd.arg("--").arg("sh").arg("-c").arg(shell_command);
        cmd.current_dir(cwd);

        for var in BLOCKED_ENV_VARS {
            cmd.env_remove(var);
        }

        cmd
    }
}
