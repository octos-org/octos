//! Sandboxing for shell command execution.
//!
//! Provides platform-specific isolation: bubblewrap on Linux,
//! sandbox-exec on macOS, or no sandbox (pass-through).

use std::path::Path;

use serde::{Deserialize, Serialize};
use tokio::process::Command;

/// Environment variables blocked inside sandboxes (code injection vectors).
///
/// Shared between sandbox backends and MCP server spawning.
pub const BLOCKED_ENV_VARS: &[&str] = &[
    // Linux: shared library injection
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
    "LD_AUDIT",
    // macOS: dylib injection
    "DYLD_INSERT_LIBRARIES",
    "DYLD_LIBRARY_PATH",
    "DYLD_FRAMEWORK_PATH",
    "DYLD_FALLBACK_LIBRARY_PATH",
    "DYLD_VERSIONED_LIBRARY_PATH",
    // Runtime-specific code injection
    "NODE_OPTIONS",
    "PYTHONSTARTUP",
    "PYTHONPATH",
    "PERL5OPT",
    "RUBYOPT",
    "RUBYLIB",
    "JAVA_TOOL_OPTIONS",
    // Shell startup injection
    "BASH_ENV",
    "ENV",
    "ZDOTDIR",
];

/// Sandbox configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SandboxConfig {
    /// Whether sandboxing is enabled.
    #[serde(default)]
    pub enabled: bool,

    /// Sandbox mode (auto-detect by default).
    #[serde(default)]
    pub mode: SandboxMode,

    /// Allow network access inside the sandbox.
    #[serde(default)]
    pub allow_network: bool,

    /// Docker-specific settings (used when mode = "docker").
    #[serde(default)]
    pub docker: DockerConfig,

    /// Restrict file reads to these paths (plus the workspace cwd).
    /// Empty = allow all reads (default, backward compatible).
    /// Non-empty = only allow reads from cwd + these paths (kernel-enforced on macOS/Linux).
    #[serde(default)]
    pub read_allow_paths: Vec<String>,
}

/// Default system paths that must be readable for shell commands to work.
const DEFAULT_READ_ALLOW_PATHS: &[&str] = &[
    "/usr",
    "/bin",
    "/sbin",
    "/lib",
    "/lib64",
    "/opt/homebrew", // macOS Homebrew
    "/Library",      // macOS system libraries
    "/System",       // macOS system
    "/Applications", // macOS apps (for tool binaries)
    "/private/tmp",
    "/private/var/folders",
    "/tmp",
    "/var/tmp",
    "/etc", // system config (needed for DNS resolution, etc.)
    "/dev/null",
    "/dev/urandom",
    "/dev/random",
];

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: SandboxMode::Auto,
            allow_network: false,
            docker: DockerConfig::default(),
            read_allow_paths: Vec::new(),
        }
    }
}

/// Docker sandbox configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DockerConfig {
    /// Docker image to use (default: "ubuntu:24.04").
    #[serde(default = "default_docker_image")]
    pub image: String,

    /// CPU limit (e.g. "1.0").
    #[serde(default)]
    pub cpu_limit: Option<String>,

    /// Memory limit (e.g. "512m").
    #[serde(default)]
    pub memory_limit: Option<String>,

    /// Maximum number of processes.
    #[serde(default)]
    pub pids_limit: Option<u32>,

    /// Workspace mount mode.
    #[serde(default)]
    pub mount_mode: MountMode,

    /// Additional bind mounts (host:container or host:container:ro).
    #[serde(default)]
    pub extra_binds: Vec<String>,
}

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
fn is_blocked_bind_source(source: &str) -> bool {
    let normalized = source.trim_end_matches('/');
    BLOCKED_DOCKER_BIND_SOURCES.iter().any(|blocked| {
        normalized == *blocked
            || normalized.ends_with("/docker.sock")
            || (normalized.starts_with(blocked)
                && normalized.as_bytes().get(blocked.len()) == Some(&b'/'))
    })
}

impl Default for DockerConfig {
    fn default() -> Self {
        Self {
            image: default_docker_image(),
            cpu_limit: None,
            memory_limit: None,
            pids_limit: None,
            mount_mode: MountMode::ReadWrite,
            extra_binds: Vec::new(),
        }
    }
}

fn default_docker_image() -> String {
    "ubuntu:24.04".to_string()
}

/// Workspace mount mode for Docker sandbox.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MountMode {
    /// No workspace mount.
    None,
    /// Read-only mount.
    #[serde(rename = "ro")]
    ReadOnly,
    /// Read-write mount (default).
    #[default]
    #[serde(rename = "rw")]
    ReadWrite,
}

/// Which sandbox backend to use.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SandboxMode {
    /// Auto-detect: bwrap on Linux, sandbox-exec on macOS, none elsewhere.
    #[default]
    Auto,
    /// Linux bubblewrap.
    Bwrap,
    /// macOS sandbox-exec.
    Macos,
    /// Docker container isolation.
    Docker,
    /// No sandboxing (pass-through).
    None,
}

/// Trait for wrapping shell commands in a sandbox.
pub trait Sandbox: Send + Sync {
    /// Wrap a shell command string into a sandboxed `Command`.
    fn wrap_command(&self, shell_command: &str, cwd: &Path) -> Command;
}

/// No-op sandbox: executes commands directly.
pub struct NoSandbox;

impl Sandbox for NoSandbox {
    fn wrap_command(&self, shell_command: &str, cwd: &Path) -> Command {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(shell_command).current_dir(cwd);
        cmd
    }
}

/// Linux sandbox using bubblewrap (bwrap).
pub struct BwrapSandbox {
    allow_network: bool,
}

impl Sandbox for BwrapSandbox {
    fn wrap_command(&self, shell_command: &str, cwd: &Path) -> Command {
        let mut cmd = Command::new("bwrap");

        // Clear dangerous environment variables before entering sandbox
        for var in BLOCKED_ENV_VARS {
            cmd.env_remove(var);
        }

        // Read-only bind system directories
        for dir in &["/usr", "/lib", "/lib64", "/bin", "/sbin", "/etc"] {
            if Path::new(dir).exists() {
                cmd.arg("--ro-bind").arg(dir).arg(dir);
            }
        }

        // Read-write bind the working directory
        let cwd_str = cwd.to_string_lossy();
        cmd.arg("--bind").arg(&*cwd_str).arg(&*cwd_str);

        // Bind /tmp for scratch space
        cmd.arg("--tmpfs").arg("/tmp");

        // /dev minimal
        cmd.arg("--dev").arg("/dev");
        cmd.arg("--proc").arg("/proc");

        if !self.allow_network {
            cmd.arg("--unshare-net");
        }

        cmd.arg("--unshare-pid");
        cmd.arg("--die-with-parent");
        cmd.arg("--chdir").arg(&*cwd_str);
        cmd.arg("--").arg("sh").arg("-c").arg(shell_command);

        cmd
    }
}

/// macOS sandbox using sandbox-exec.
pub struct MacosSandbox {
    allow_network: bool,
    /// When non-empty, restrict file-read* to these paths + cwd.
    /// Empty = allow all reads (backward compatible).
    read_allow_paths: Vec<String>,
}

impl Sandbox for MacosSandbox {
    fn wrap_command(&self, shell_command: &str, cwd: &Path) -> Command {
        let cwd_str = cwd.to_string_lossy();

        // Reject paths with control characters or SBPL metacharacters to prevent
        // sandbox profile injection. Fail closed: error instead of running unsandboxed.
        if cwd_str
            .bytes()
            .any(|b| b < 0x20 || b == 0x7F || b == b'(' || b == b')' || b == b'\\' || b == b'"')
        {
            tracing::error!("cwd contains SBPL metacharacters, refusing to execute");
            let mut cmd = Command::new("sh");
            cmd.arg("-c")
                .arg("echo 'sandbox error: cwd contains invalid characters' >&2; exit 1");
            return cmd;
        }

        // Path is validated above — no escaping needed since \ and " are rejected.
        let cwd_escaped = &cwd_str;

        let network_rule = if self.allow_network {
            "(allow network*)"
        } else {
            "(deny network*)"
        };

        // Build file-read rules: global if no read_allow_paths, restricted otherwise
        let read_rules = if self.read_allow_paths.is_empty() {
            "(allow file-read*)".to_string()
        } else {
            let mut rules = Vec::new();
            // Always allow reading the workspace
            rules.push(format!(
                "(allow file-read* (subpath \"{cwd}\"))",
                cwd = cwd_escaped
            ));
            // Add configured read paths
            for path in &self.read_allow_paths {
                rules.push(format!("(allow file-read* (subpath \"{path}\"))"));
            }
            // Add default system paths
            for path in DEFAULT_READ_ALLOW_PATHS {
                if !self.read_allow_paths.iter().any(|p| p == *path) && Path::new(path).exists() {
                    rules.push(format!("(allow file-read* (subpath \"{path}\"))"));
                }
            }
            rules.join("\n")
        };

        // Resolve cwd to its real path (macOS /tmp → /private/tmp symlink).
        // SBPL subpath rules operate on real paths, so if cwd is /tmp/foo the
        // rule must use /private/tmp/foo or writes will be denied.
        let real_cwd = std::fs::canonicalize(cwd)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| cwd_escaped.to_string());
        // Validate the resolved path too
        if real_cwd
            .bytes()
            .any(|b| b < 0x20 || b == 0x7F || b == b'(' || b == b')' || b == b'\\' || b == b'"')
        {
            tracing::error!("resolved cwd contains SBPL metacharacters, refusing to execute");
            let mut cmd = Command::new("sh");
            cmd.arg("-c")
                .arg("echo 'sandbox error: resolved cwd contains invalid characters' >&2; exit 1");
            return cmd;
        }

        let profile = format!(
            r#"(version 1)
(deny default)
(allow process-exec)
(allow process-fork)
(allow process-info*)
(allow sysctl-read)
(allow mach-lookup)
(allow mach-register)
(allow ipc-posix*)
(allow signal)
(allow file-ioctl)
{read_rules}
(allow file-write* (subpath "{cwd}"))
{network_rule}
"#,
            read_rules = read_rules,
            cwd = real_cwd,
            network_rule = network_rule,
        );

        // Create a per-user tmp dir inside the workspace so programs that
        // need temp files (Python tempfile, compilers, etc.) still work.
        let user_tmp = cwd.join("tmp");
        let _ = std::fs::create_dir_all(&user_tmp);

        let mut cmd = Command::new("sandbox-exec");
        // Redirect TMPDIR/TEMP/TMP to the per-user tmp inside the workspace
        cmd.env("TMPDIR", &user_tmp);
        cmd.env("TEMP", &user_tmp);
        cmd.env("TMP", &user_tmp);
        // Clear dangerous environment variables (sandbox-exec inherits parent env)
        for var in BLOCKED_ENV_VARS {
            cmd.env_remove(var);
        }
        cmd.arg("-p")
            .arg(profile)
            .arg("sh")
            .arg("-c")
            .arg(shell_command)
            .current_dir(cwd);
        cmd
    }
}

/// Docker container sandbox.
pub struct DockerSandbox {
    config: DockerConfig,
    allow_network: bool,
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

        // Workspace mount — validate path to prevent volume mount injection via ':'
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

/// Create a sandbox from config.
pub fn create_sandbox(config: &SandboxConfig) -> Box<dyn Sandbox> {
    if !config.enabled {
        tracing::info!("sandbox disabled, shell commands run without isolation");
        return Box::new(NoSandbox);
    }

    match &config.mode {
        SandboxMode::None => Box::new(NoSandbox),
        SandboxMode::Bwrap => Box::new(BwrapSandbox {
            allow_network: config.allow_network,
        }),
        SandboxMode::Macos => Box::new(MacosSandbox {
            allow_network: config.allow_network,
            read_allow_paths: config.read_allow_paths.clone(),
        }),
        SandboxMode::Docker => Box::new(DockerSandbox {
            config: config.docker.clone(),
            allow_network: config.allow_network,
        }),
        SandboxMode::Auto => {
            if cfg!(target_os = "linux") && which_exists("bwrap") {
                Box::new(BwrapSandbox {
                    allow_network: config.allow_network,
                })
            } else if cfg!(target_os = "macos") && which_exists("sandbox-exec") {
                Box::new(MacosSandbox {
                    allow_network: config.allow_network,
                    read_allow_paths: config.read_allow_paths.clone(),
                })
            } else if which_exists("docker") {
                Box::new(DockerSandbox {
                    config: config.docker.clone(),
                    allow_network: config.allow_network,
                })
            } else {
                Box::new(NoSandbox)
            }
        }
    }
}

/// Check if a binary exists on PATH.
fn which_exists(bin: &str) -> bool {
    std::process::Command::new("which")
        .arg(bin)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_sandbox_wraps_directly() {
        let sb = NoSandbox;
        let cmd = sb.wrap_command("echo hello", Path::new("/tmp"));
        // Command should be `sh -c "echo hello"` in /tmp
        let prog = cmd.as_std().get_program().to_string_lossy().to_string();
        assert_eq!(prog, "sh");
    }

    #[test]
    fn test_create_sandbox_disabled() {
        let config = SandboxConfig::default();
        let sb = create_sandbox(&config);
        // Should be NoSandbox — just verify it doesn't panic
        let _cmd = sb.wrap_command("ls", Path::new("/tmp"));
    }

    #[test]
    fn test_bwrap_sandbox_command() {
        let sb = BwrapSandbox {
            allow_network: false,
        };
        let cmd = sb.wrap_command("echo hi", Path::new("/tmp/test"));
        let prog = cmd.as_std().get_program().to_string_lossy().to_string();
        assert_eq!(prog, "bwrap");
        let args: Vec<_> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert!(args.contains(&"--unshare-net".to_string()));
        assert!(args.contains(&"echo hi".to_string()));
    }

    #[test]
    fn test_bwrap_sandbox_env_sanitization() {
        let sb = BwrapSandbox {
            allow_network: false,
        };
        let cmd = sb.wrap_command("ls", Path::new("/tmp"));
        // env_remove() sets env vars to None in the command's environment.
        // get_envs() returns (key, Option<value>) — None means env_remove.
        let removed: Vec<String> = cmd
            .as_std()
            .get_envs()
            .filter_map(|(k, v)| {
                if v.is_none() {
                    Some(k.to_string_lossy().to_string())
                } else {
                    None
                }
            })
            .collect();
        for var in BLOCKED_ENV_VARS {
            assert!(
                removed.iter().any(|r| r == *var),
                "bwrap should env_remove {var}"
            );
        }
    }

    #[test]
    fn test_macos_sandbox_command() {
        let sb = MacosSandbox {
            allow_network: true,
            read_allow_paths: Vec::new(),
        };
        let cmd = sb.wrap_command("echo hi", Path::new("/tmp/test"));
        let prog = cmd.as_std().get_program().to_string_lossy().to_string();
        assert_eq!(prog, "sandbox-exec");
        let args: Vec<_> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert!(args.iter().any(|a| a.contains("allow network")));

        // Verify /private/tmp is NOT in SBPL write rules (loophole fixed)
        let profile = args
            .iter()
            .find(|a| a.contains("deny default"))
            .expect("should have SBPL profile");
        assert!(
            !profile.contains("(allow file-write* (subpath \"/private/tmp\"))"),
            "SBPL should NOT allow writes to /private/tmp (loophole)"
        );
        assert!(
            !profile.contains("(allow file-write* (subpath \"/private/var/folders\"))"),
            "SBPL should NOT allow writes to /private/var/folders"
        );
    }

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
        // Network allowed — no --network none
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

        // No mount — uses /tmp as workdir instead of /workspace
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
    fn test_macos_sandbox_rejects_control_chars() {
        let sb = MacosSandbox {
            allow_network: false,
            read_allow_paths: Vec::new(),
        };
        let cmd = sb.wrap_command("ls", Path::new("/tmp/\x01bad"));
        let prog = cmd.as_std().get_program().to_string_lossy().to_string();
        assert_eq!(prog, "sh"); // error command, not sandbox-exec
        let args: Vec<_> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        // Must NOT execute the original command unsandboxed
        assert!(args.iter().any(|a| a.contains("exit 1")));
        assert!(!args.iter().any(|a| a.contains("ls")));
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
    fn test_macos_sandbox_rejects_sbpl_metacharacters() {
        let sb = MacosSandbox {
            allow_network: false,
            read_allow_paths: Vec::new(),
        };
        // Parentheses, backslash, and quote should all be rejected
        for path in &[
            "/tmp/(allow network*)",
            "/tmp/test\\evil",
            "/tmp/test\"evil",
        ] {
            let cmd = sb.wrap_command("ls", Path::new(path));
            let prog = cmd.as_std().get_program().to_string_lossy().to_string();
            assert_eq!(prog, "sh", "should reject path: {path}");
            let args: Vec<_> = cmd
                .as_std()
                .get_args()
                .map(|a| a.to_string_lossy().to_string())
                .collect();
            assert!(
                args.iter().any(|a| a.contains("exit 1")),
                "should exit 1 for path: {path}"
            );
        }
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

    // --- SandboxMode enum tests ---

    #[test]
    fn test_sandbox_mode_default_is_auto() {
        assert_eq!(SandboxMode::default(), SandboxMode::Auto);
    }

    #[test]
    fn test_sandbox_mode_serde_roundtrip() {
        let modes = [
            (SandboxMode::Auto, "\"auto\""),
            (SandboxMode::Bwrap, "\"bwrap\""),
            (SandboxMode::Macos, "\"macos\""),
            (SandboxMode::Docker, "\"docker\""),
            (SandboxMode::None, "\"none\""),
        ];
        for (mode, expected_json) in &modes {
            let json = serde_json::to_string(mode).unwrap();
            assert_eq!(&json, expected_json, "serialize {mode:?}");
            let parsed: SandboxMode = serde_json::from_str(expected_json).unwrap();
            assert_eq!(&parsed, mode, "deserialize {expected_json}");
        }
    }

    #[test]
    fn test_sandbox_mode_debug() {
        // Ensure Debug is implemented and produces expected output
        let dbg = format!("{:?}", SandboxMode::Auto);
        assert_eq!(dbg, "Auto");
    }

    // --- MountMode enum tests ---

    #[test]
    fn test_mount_mode_default_is_readwrite() {
        assert_eq!(MountMode::default(), MountMode::ReadWrite);
    }

    #[test]
    fn test_mount_mode_serde_roundtrip() {
        let modes = [
            (MountMode::None, "\"none\""),
            (MountMode::ReadOnly, "\"ro\""),
            (MountMode::ReadWrite, "\"rw\""),
        ];
        for (mode, expected_json) in &modes {
            let json = serde_json::to_string(mode).unwrap();
            assert_eq!(&json, expected_json, "serialize {mode:?}");
            let parsed: MountMode = serde_json::from_str(expected_json).unwrap();
            assert_eq!(&parsed, mode, "deserialize {expected_json}");
        }
    }

    // --- BLOCKED_ENV_VARS tests ---

    #[test]
    fn test_blocked_env_vars_contains_critical_vars() {
        let critical = [
            "LD_PRELOAD",
            "DYLD_INSERT_LIBRARIES",
            "NODE_OPTIONS",
            "PYTHONSTARTUP",
            "PYTHONPATH",
            "BASH_ENV",
            "LD_LIBRARY_PATH",
            "DYLD_LIBRARY_PATH",
            "JAVA_TOOL_OPTIONS",
        ];
        for var in &critical {
            assert!(
                BLOCKED_ENV_VARS.contains(var),
                "BLOCKED_ENV_VARS missing critical var: {var}"
            );
        }
    }

    #[test]
    fn test_blocked_env_vars_has_expected_count() {
        // Guard against accidental removal; update count if vars are intentionally added/removed
        assert_eq!(
            BLOCKED_ENV_VARS.len(),
            18,
            "BLOCKED_ENV_VARS count changed unexpectedly"
        );
    }

    #[test]
    fn test_blocked_env_vars_no_duplicates() {
        let mut seen = std::collections::HashSet::new();
        for var in BLOCKED_ENV_VARS {
            assert!(seen.insert(var), "duplicate in BLOCKED_ENV_VARS: {var}");
        }
    }

    // --- SandboxConfig / DockerConfig default tests ---

    #[test]
    fn test_sandbox_config_default() {
        let config = SandboxConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.mode, SandboxMode::Auto);
        assert!(!config.allow_network);
    }

    #[test]
    fn test_docker_config_default() {
        let config = DockerConfig::default();
        assert_eq!(config.image, "ubuntu:24.04");
        assert!(config.cpu_limit.is_none());
        assert!(config.memory_limit.is_none());
        assert!(config.pids_limit.is_none());
        assert_eq!(config.mount_mode, MountMode::ReadWrite);
    }

    #[test]
    fn test_sandbox_config_serde_defaults() {
        // Empty JSON object should produce sensible defaults
        let config: SandboxConfig = serde_json::from_str("{}").unwrap();
        assert!(!config.enabled);
        assert_eq!(config.mode, SandboxMode::Auto);
        assert!(!config.allow_network);
        assert_eq!(config.docker.image, "ubuntu:24.04");
    }

    // --- create_sandbox with SandboxMode::None ---

    #[test]
    fn test_create_sandbox_mode_none() {
        let config = SandboxConfig {
            enabled: true,
            mode: SandboxMode::None,
            allow_network: false,
            docker: DockerConfig::default(),
            read_allow_paths: Vec::new(),
        };
        let sb = create_sandbox(&config);
        // Should produce a NoSandbox (sh -c)
        let cmd = sb.wrap_command("echo test", Path::new("/tmp"));
        let prog = cmd.as_std().get_program().to_string_lossy().to_string();
        assert_eq!(prog, "sh");
    }

    // --- Bwrap with network allowed ---

    #[test]
    fn test_bwrap_sandbox_allows_network() {
        let sb = BwrapSandbox {
            allow_network: true,
        };
        let cmd = sb.wrap_command("echo hi", Path::new("/tmp"));
        let args: Vec<_> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert!(
            !args.contains(&"--unshare-net".to_string()),
            "should not unshare net when network is allowed"
        );
    }

    // --- macOS sandbox with network denied ---

    #[test]
    fn test_macos_sandbox_denies_network() {
        let sb = MacosSandbox {
            allow_network: false,
            read_allow_paths: Vec::new(),
        };
        let cmd = sb.wrap_command("echo hi", Path::new("/tmp/test"));
        let args: Vec<_> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert!(
            args.iter().any(|a| a.contains("deny network")),
            "should deny network when allow_network is false"
        );
    }

    // --- Docker path validation: additional injection chars ---

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

    // --- macOS sandbox accepts valid paths ---

    #[test]
    fn test_macos_sandbox_accepts_valid_path() {
        let sb = MacosSandbox {
            allow_network: false,
            read_allow_paths: Vec::new(),
        };
        let cmd = sb.wrap_command("echo ok", Path::new("/Users/test/project"));
        let prog = cmd.as_std().get_program().to_string_lossy().to_string();
        assert_eq!(prog, "sandbox-exec");
    }

    // --- Docker sandbox accepts valid paths ---

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

    // --- macOS sandbox rejects DEL character (0x7F) ---

    #[test]
    fn test_macos_sandbox_rejects_del_character() {
        let sb = MacosSandbox {
            allow_network: false,
            read_allow_paths: Vec::new(),
        };
        let cmd = sb.wrap_command("ls", Path::new("/tmp/evil\x7Fpath"));
        let prog = cmd.as_std().get_program().to_string_lossy().to_string();
        assert_eq!(prog, "sh");
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
        // Should fall back to error shell
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
        // Safe bind should be present
        assert!(
            args.iter().any(|a| a.contains("/home/user/data")),
            "safe bind mount should be included"
        );
        // Dangerous binds should be filtered out
        assert!(
            !args.iter().any(|a| a.contains("docker.sock")),
            "docker.sock bind should be blocked"
        );
        assert!(
            !args.iter().any(|a| a.contains("/proc")),
            "/proc bind should be blocked"
        );
    }

    // --- macOS restricted read paths ---

    #[test]
    fn should_use_global_file_read_when_no_read_paths() {
        let sb = MacosSandbox {
            allow_network: false,
            read_allow_paths: Vec::new(),
        };
        let cmd = sb.wrap_command("echo hi", Path::new("/tmp/test"));
        let args: Vec<_> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        // Should contain unrestricted file-read*
        assert!(
            args.iter().any(|a| a.contains("(allow file-read*)\n")),
            "should have global file-read* when read_allow_paths is empty"
        );
    }

    #[test]
    fn should_restrict_reads_when_read_paths_configured() {
        let sb = MacosSandbox {
            allow_network: false,
            read_allow_paths: vec!["/custom/path".to_string()],
        };
        let cmd = sb.wrap_command("echo hi", Path::new("/tmp/test"));
        let args: Vec<_> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        let profile = args
            .iter()
            .find(|a| a.contains("deny default"))
            .expect("should have SBPL profile");
        // Should NOT contain unrestricted file-read*
        assert!(
            !profile.contains("(allow file-read*)\n"),
            "should not have global file-read*"
        );
        // Should contain workspace read
        assert!(
            profile.contains(r#"(allow file-read* (subpath "/tmp/test"))"#),
            "should allow reading workspace"
        );
        // Should contain custom path
        assert!(
            profile.contains(r#"(allow file-read* (subpath "/custom/path"))"#),
            "should allow reading custom path"
        );
    }
}
