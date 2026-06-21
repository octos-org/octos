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
}

impl Sandbox for LinuxContainerSandbox {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_container_sandbox_uses_helper_or_fails_closed() {
        let sandbox = LinuxContainerSandbox {
            allow_network: false,
            read_allow_paths: vec!["/usr".to_string()],
            profile_name: Some("octos.test".to_string()),
        };
        let cmd = sandbox.wrap_command("echo hello", Path::new("/tmp"));
        let prog = cmd.as_std().get_program().to_string_lossy().to_string();

        assert!(
            prog.contains("octos-sandbox") || prog == "sh",
            "expected octos-sandbox helper or fail-closed shell, got {prog}"
        );
    }

    #[test]
    fn linux_container_sandbox_removes_injection_env() {
        let sandbox = LinuxContainerSandbox {
            allow_network: true,
            read_allow_paths: vec![],
            profile_name: None,
        };
        let cmd = sandbox.wrap_command("echo hello", Path::new("/tmp"));
        let removed: Vec<String> = cmd
            .as_std()
            .get_envs()
            .filter_map(|(key, value)| {
                if value.is_none() {
                    Some(key.to_string_lossy().to_string())
                } else {
                    None
                }
            })
            .collect();

        for var in BLOCKED_ENV_VARS {
            assert!(
                removed.iter().any(|removed| removed == *var),
                "Linux container sandbox should env_remove {var}"
            );
        }
    }
}
