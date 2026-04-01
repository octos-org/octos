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
    fn wrap_command(&self, shell_command: &str, cwd: &Path) -> Command {
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

        cmd.arg(&self.config.image);
        cmd.arg("sh").arg("-c").arg(shell_command);
        cmd
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_docker_sandbox_command() {
        let sb = DockerSandbox {
            config: DockerConfig::default(),
            allow_network: false,
        };
        let cmd = sb.wrap_command("echo hi", Path::new("/tmp/work"));
        let prog = cmd.as_std().get_program().to_string_lossy().to_string();
        assert_eq!(prog, "docker");
        let args: Vec<_> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert!(args.contains(&"run".to_string()));
        assert!(args.contains(&"--rm".to_string()));
        assert!(args.contains(&"none".to_string())); // --network none
        assert!(args.contains(&"no-new-privileges".to_string()));
        assert!(args.contains(&"ALL".to_string())); // --cap-drop ALL
        assert!(args.contains(&"ubuntu:24.04".to_string()));
        assert!(args.contains(&"echo hi".to_string()));
    }

    #[test]
    fn test_docker_sandbox_resource_limits() {
        let sb = DockerSandbox {
            config: DockerConfig {
                cpu_limit: Some("1.5".into()),
                memory_limit: Some("256m".into()),
                pids_limit: Some(100),
                ..DockerConfig::default()
            },
            allow_network: true,
        };
        let cmd = sb.wrap_command("ls", Path::new("/tmp"));
        let args: Vec<_> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert!(args.contains(&"1.5".to_string())); // --cpus
        assert!(args.contains(&"256m".to_string())); // --memory
        assert!(args.contains(&"100".to_string())); // --pids-limit
        // Network allowed -- no --network none
        assert!(!args.iter().any(|a| a == "none"));
    }

    #[test]
    fn test_docker_sandbox_mount_modes() {
        // Read-write (default)
        let sb = DockerSandbox {
            config: DockerConfig::default(),
            allow_network: false,
        };
        let cmd = sb.wrap_command("ls", Path::new("/home/user/project"));
        let args: Vec<_> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert!(args.contains(&"/home/user/project:/workspace".to_string()));

        // Read-only
        let sb_ro = DockerSandbox {
            config: DockerConfig {
                mount_mode: MountMode::ReadOnly,
                ..DockerConfig::default()
            },
            allow_network: false,
        };
        let cmd_ro = sb_ro.wrap_command("ls", Path::new("/home/user/project"));
        let args_ro: Vec<_> = cmd_ro
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert!(args_ro.contains(&"/home/user/project:/workspace:ro".to_string()));

        // No mount -- uses /tmp as workdir instead of /workspace
        let sb_none = DockerSandbox {
            config: DockerConfig {
                mount_mode: MountMode::None,
                ..DockerConfig::default()
            },
            allow_network: false,
        };
        let cmd_none = sb_none.wrap_command("ls", Path::new("/home/user/project"));
        let args_none: Vec<_> = cmd_none
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert!(!args_none.iter().any(|a| a.contains(":/workspace")));
        assert!(args_none.contains(&"/tmp".to_string())); // -w /tmp
    }

    #[test]
    fn test_docker_sandbox_rejects_colon_in_path() {
        let sb = DockerSandbox {
            config: DockerConfig::default(),
            allow_network: false,
        };
        let cmd = sb.wrap_command("ls", Path::new("/tmp/evil:/host"));
        let prog = cmd.as_std().get_program().to_string_lossy().to_string();
        assert_eq!(prog, "sh"); // falls back to error command
        let args: Vec<_> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert!(args.iter().any(|a| a.contains("exit 1")));
    }

    #[test]
    fn test_docker_sandbox_rejects_newline_in_path() {
        let sb = DockerSandbox {
            config: DockerConfig::default(),
            allow_network: false,
        };
        let cmd = sb.wrap_command("ls", Path::new("/tmp/evil\n--privileged"));
        let prog = cmd.as_std().get_program().to_string_lossy().to_string();
        assert_eq!(prog, "sh");
        let args: Vec<_> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert!(args.iter().any(|a| a.contains("exit 1")));
    }

    #[test]
    fn test_docker_sandbox_env_sanitization() {
        let sb = DockerSandbox {
            config: DockerConfig::default(),
            allow_network: false,
        };
        let cmd = sb.wrap_command("ls", Path::new("/tmp"));
        let args: Vec<_> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        for var in BLOCKED_ENV_VARS {
            assert!(
                args.contains(&format!("{var}=")),
                "missing env clear for {var}"
            );
        }
    }

    #[test]
    fn test_docker_sandbox_accepts_valid_path() {
        let sb = DockerSandbox {
            config: DockerConfig::default(),
            allow_network: false,
        };
        let cmd = sb.wrap_command("echo ok", Path::new("/home/user/project"));
        let prog = cmd.as_std().get_program().to_string_lossy().to_string();
        assert_eq!(prog, "docker");
    }

    #[test]
    fn test_docker_sandbox_rejects_null_byte_in_path() {
        let sb = DockerSandbox {
            config: DockerConfig::default(),
            allow_network: false,
        };
        let cmd = sb.wrap_command("ls", Path::new("/tmp/evil\0inject"));
        let prog = cmd.as_std().get_program().to_string_lossy().to_string();
        assert_eq!(prog, "sh");
    }

    #[test]
    fn test_docker_sandbox_rejects_carriage_return_in_path() {
        let sb = DockerSandbox {
            config: DockerConfig::default(),
            allow_network: false,
        };
        let cmd = sb.wrap_command("ls", Path::new("/tmp/evil\r--privileged"));
        let prog = cmd.as_std().get_program().to_string_lossy().to_string();
        assert_eq!(prog, "sh");
        let args: Vec<_> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert!(args.iter().any(|a| a.contains("exit 1")));
    }

    // --- Blocked Docker bind sources ---

    #[test]
    fn should_block_docker_socket_bind() {
        assert!(is_blocked_bind_source("/var/run/docker.sock"));
        assert!(is_blocked_bind_source("/home/user/.docker/docker.sock"));
        assert!(is_blocked_bind_source("docker.sock"));
    }

    #[test]
    fn should_block_dangerous_system_dirs() {
        assert!(is_blocked_bind_source("/etc"));
        assert!(is_blocked_bind_source("/etc/"));
        assert!(is_blocked_bind_source("/etc/passwd"));
        assert!(is_blocked_bind_source("/proc"));
        assert!(is_blocked_bind_source("/sys"));
        assert!(is_blocked_bind_source("/dev"));
    }

    #[test]
    fn should_allow_safe_bind_paths() {
        assert!(!is_blocked_bind_source("/home/user/workspace"));
        assert!(!is_blocked_bind_source("/tmp"));
        assert!(!is_blocked_bind_source("/opt/data"));
    }

    #[test]
    fn should_reject_docker_sandbox_with_blocked_cwd() {
        let sb = DockerSandbox {
            config: DockerConfig::default(),
            allow_network: false,
        };
        let cmd = sb.wrap_command("ls", Path::new("/etc"));
        let prog = cmd.as_std().get_program().to_string_lossy().to_string();
        assert_eq!(prog, "sh");
    }

    #[test]
    fn should_skip_blocked_extra_binds() {
        let sb = DockerSandbox {
            config: DockerConfig {
                extra_binds: vec![
                    "/home/user/data:/data:ro".to_string(),
                    "/var/run/docker.sock:/var/run/docker.sock".to_string(),
                    "/proc:/host-proc:ro".to_string(),
                ],
                ..DockerConfig::default()
            },
            allow_network: false,
        };
        let cmd = sb.wrap_command("ls", Path::new("/tmp/safe"));
        let args: Vec<_> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert!(
            args.iter().any(|a| a.contains("/home/user/data")),
            "safe bind mount should be included"
        );
        assert!(
            !args.iter().any(|a| a.contains("docker.sock")),
            "docker.sock bind should be blocked"
        );
        assert!(
            !args.iter().any(|a| a.contains("/proc")),
            "/proc bind should be blocked"
        );
    }
}
