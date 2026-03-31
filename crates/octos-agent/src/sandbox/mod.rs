//! Sandboxing for shell command execution.
//!
//! Provides platform-specific isolation: bubblewrap on Linux,
//! sandbox-exec on macOS, or no sandbox (pass-through).

mod bwrap;
mod docker;
mod macos;

pub use bwrap::BwrapSandbox;
pub use docker::DockerSandbox;
pub use macos::MacosSandbox;

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
    /// Whether sandboxing is enabled (default: true).
    #[serde(default = "default_enabled")]
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
pub(crate) const DEFAULT_READ_ALLOW_PATHS: &[&str] = &[
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
    "/private/var/select", // macOS shell init (e.g. /private/var/select/sh)
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
            enabled: true,
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

fn default_enabled() -> bool {
    true
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
        #[cfg(windows)]
        {
            let mut cmd = Command::new("cmd");
            cmd.arg("/C").arg(shell_command).current_dir(cwd);
            cmd
        }
        #[cfg(not(windows))]
        {
            let mut cmd = Command::new("sh");
            cmd.arg("-c").arg(shell_command).current_dir(cwd);
            cmd
        }
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
                tracing::warn!(
                    "no sandbox backend found (bwrap, sandbox-exec, or docker). \
                     Shell commands will run WITHOUT isolation. \
                     Install a sandbox backend or set sandbox.enabled = false to silence this warning."
                );
                Box::new(NoSandbox)
            }
        }
    }
}

/// Check if a binary exists on PATH.
fn which_exists(bin: &str) -> bool {
    #[cfg(windows)]
    let prog = "where";
    #[cfg(not(windows))]
    let prog = "which";

    std::process::Command::new(prog)
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
        let tmp = std::env::temp_dir();
        let cmd = sb.wrap_command("echo hello", &tmp);
        let prog = cmd.as_std().get_program().to_string_lossy().to_string();
        #[cfg(windows)]
        assert_eq!(prog, "cmd");
        #[cfg(not(windows))]
        assert_eq!(prog, "sh");
    }

    #[test]
    fn test_create_sandbox_disabled() {
        let config = SandboxConfig {
            enabled: false,
            ..SandboxConfig::default()
        };
        let sb = create_sandbox(&config);
        // Should be NoSandbox -- just verify it doesn't panic
        let _cmd = sb.wrap_command("ls", Path::new("/tmp"));
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
        assert!(config.enabled, "sandbox should be enabled by default");
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
        let config: SandboxConfig = serde_json::from_str("{}").unwrap();
        assert!(config.enabled, "sandbox should be enabled by default when field is missing");
        assert_eq!(config.mode, SandboxMode::Auto);
        assert!(!config.allow_network);
        assert_eq!(config.docker.image, "ubuntu:24.04");
    }

    #[test]
    fn test_sandbox_config_explicit_disable() {
        let config: SandboxConfig =
            serde_json::from_str(r#"{"enabled": false}"#).unwrap();
        assert!(!config.enabled);
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
        let tmp = std::env::temp_dir();
        let cmd = sb.wrap_command("echo test", &tmp);
        let prog = cmd.as_std().get_program().to_string_lossy().to_string();
        #[cfg(windows)]
        assert_eq!(prog, "cmd");
        #[cfg(not(windows))]
        assert_eq!(prog, "sh");
    }
}
