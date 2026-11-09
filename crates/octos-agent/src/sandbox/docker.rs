//! Docker container sandbox.

use std::path::Path;

use tokio::process::Command;

use super::{BLOCKED_ENV_VARS, DockerConfig, MountMode, Sandbox};

/// Bind mount sources that could lead to container escape or host compromise.
const BLOCKED_DOCKER_BIND_SOURCES: &[&str] = &[
    "/var/run/docker.sock",
    "docker.sock",
    "/etc",
    "/proc",
    "/sys",
    "/dev",
];

/// Check if a bind mount source is dangerous.
pub(crate) fn is_blocked_bind_source(source: &str) -> bool {
    let normalized = source.trim_end_matches('/');
    BLOCKED_DOCKER_BIND_SOURCES.iter().any(|blocked| {
        normalized == *blocked
            || normalized.ends_with("/docker.sock")
            || (normalized.starts_with(blocked)
                && normalized.as_bytes().get(blocked.len()) == Some(&b'/'))
    })
}

/// Docker container sandbox.
pub struct DockerSandbox {
    pub(crate) config: DockerConfig,
    pub(crate) allow_network: bool,
}

impl Sandbox for DockerSandbox {
    fn backend_name(&self) -> &'static str {
        "docker"
    }

    fn docker_image(&self) -> Option<&str> {
        Some(self.config.image.as_str())
    }

    fn is_docker(&self) -> bool {
        true
    }

    fn runs_posix_sh(&self) -> bool {
        // `wrap_command` terminates in an in-container `sh -c` (Linux image),
        // regardless of the host OS.
        true
    }

    fn wrap_command(&self, shell_command: &str, cwd: &Path) -> Command {
        self.wrap_with_slot(shell_command, cwd, None)
    }

    fn wrap_command_with_build_cache_slot(
        &self,
        shell_command: &str,
        cwd: &Path,
        slot: Option<&Path>,
    ) -> Command {
        self.wrap_with_slot(shell_command, cwd, slot)
    }
}

impl DockerSandbox {
    fn wrap_with_slot(&self, shell_command: &str, cwd: &Path, slot: Option<&Path>) -> Command {
        let mut cmd = Command::new("docker");
        cmd.arg("run").arg("--rm");

        // Resource limits
        if let Some(ref cpu) = self.config.cpu_limit {
            cmd.arg("--cpus").arg(cpu);
        }
        if let Some(ref mem) = self.config.memory_limit {
            cmd.arg("--memory").arg(mem);
        }
        if let Some(pids) = self.config.pids_limit {
            cmd.arg("--pids-limit").arg(pids.to_string());
        }

        // Network
        if !self.allow_network {
            cmd.arg("--network").arg("none");
        }

        // Security hardening
        cmd.arg("--security-opt").arg("no-new-privileges");
        cmd.arg("--cap-drop").arg("ALL");

        // Clear dangerous environment variables (code injection vectors)
        for var in BLOCKED_ENV_VARS {
            cmd.arg("--env").arg(format!("{var}="));
        }

        // Workspace mount -- validate path to prevent volume mount injection via ':'
        let cwd_str = cwd.to_string_lossy();
        if cwd_str.contains(':')
            || cwd_str.contains('\0')
            || cwd_str.contains('\n')
            || cwd_str.contains('\r')
        {
            tracing::error!(
                "cwd contains invalid characters for Docker mount, refusing to execute"
            );
            let mut fail = Command::new("sh");
            fail.arg("-c")
                .arg("echo 'sandbox error: cwd contains invalid characters' >&2; exit 1");
            return fail;
        }
        // Validate cwd against dangerous mount sources
        if is_blocked_bind_source(&cwd_str) {
            tracing::error!(
                cwd = %cwd_str,
                "cwd is a blocked bind mount source, refusing to execute"
            );
            let mut fail = Command::new("sh");
            fail.arg("-c")
                .arg("echo 'sandbox error: dangerous bind mount source blocked' >&2; exit 1");
            return fail;
        }

        match self.config.mount_mode {
            MountMode::ReadWrite => {
                cmd.arg("-v").arg(format!("{cwd_str}:/workspace"));
                cmd.arg("-w").arg("/workspace");
            }
            MountMode::ReadOnly => {
                cmd.arg("-v").arg(format!("{cwd_str}:/workspace:ro"));
                cmd.arg("-w").arg("/workspace");
            }
            MountMode::None => {
                cmd.arg("-w").arg("/tmp");
            }
        }

        // Extra bind mounts (validated against dangerous sources)
        for bind in &self.config.extra_binds {
            let source = bind.split(':').next().unwrap_or(bind);
            if is_blocked_bind_source(source) {
                tracing::warn!(
                    bind = %bind,
                    "skipping dangerous bind mount source"
                );
                continue;
            }
            cmd.arg("-v").arg(bind);
        }

        if let Some(slot) = slot {
            let target = std::fs::canonicalize(slot.join("target"));
            let target = match target {
                Ok(target) if target.is_dir() => target,
                _ => return cache_mount_refusal(),
            };
            let source = target.to_string_lossy();
            if source.contains([':', '\0', '\n', '\r']) || is_blocked_bind_source(&source) {
                return cache_mount_refusal();
            }
            // Only artifacts are writable; the claim's control files stay on the host.
            cmd.arg("-v")
                .arg(format!("{source}:/octos-build-cache/target"));
            cmd.arg("--env")
                .arg("CARGO_TARGET_DIR=/octos-build-cache/target");
            cmd.arg("--env").arg("CARGO_INCREMENTAL=0");
        }

        cmd.arg(&self.config.image);
        cmd.arg("sh").arg("-c").arg(shell_command);
        cmd
    }
}

fn cache_mount_refusal() -> Command {
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg("echo 'sandbox error: build-cache target cannot be mounted' >&2; exit 1");
    cmd
}
