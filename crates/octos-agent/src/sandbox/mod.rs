//! Sandboxing for shell command execution.
//!
//! Provides platform-specific isolation: bubblewrap or Landlock/seccomp on Linux,
//! sandbox-exec on macOS, AppContainer on Windows, or no sandbox (pass-through).

mod bwrap;
mod docker;
#[cfg(target_os = "linux")]
mod landlock;
mod macos;
#[cfg(windows)]
mod windows;

pub use bwrap::BwrapSandbox;
pub use docker::DockerSandbox;
#[cfg(target_os = "linux")]
pub use landlock::LinuxContainerSandbox;
pub use macos::MacosSandbox;
#[cfg(windows)]
pub use windows::AppContainerSandbox;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::process::Command;

/// Environment variables blocked inside sandboxes (code injection vectors).
///
/// Shared between sandbox backends and MCP server spawning. The canonical list
/// lives in [`octos_core::env_hygiene`] (the bottom crate) so octos-core's own
/// controller-side git ops sanitize against the SAME set; re-exported here so
/// every existing `octos_agent::sandbox::BLOCKED_ENV_VARS` reference is unchanged.
pub use octos_core::env_hygiene::BLOCKED_ENV_VARS;

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

    /// Whether shell/exec commands may write to the workspace cwd
    /// (default: true).
    ///
    /// When `false`, the workspace is mounted/bound read-only for the
    /// shell sandbox: macOS omits the `file-write*` grant for the cwd,
    /// bwrap `--ro-bind`s it, and Docker mounts it `ro`. This is what a
    /// read-only permission profile sets so `--sandbox read-only` stops
    /// shell writes (`touch newfile`), not just the native file tools.
    /// The `default_enabled` (true) default preserves backward-compatible
    /// writable behaviour for configs that never set this field.
    #[serde(default = "default_enabled")]
    pub workspace_write: bool,

    /// Additionally grant the shell WRITE to a repository's `.git` common dir,
    /// beyond the cwd (default: `None`). `Some(<repo>/.git)` is set from a fleet
    /// worker's `FsGrant::Host` — the OPERATOR's explicit trust decision: a
    /// Host-granted worktree worker's `git commit` must reach `<repo>/.git`
    /// (objects/refs/worktree-admin), which lives OUTSIDE its checkout cwd. It is
    /// a TARGETED bind, NOT full-`/`: bwrap adds `--bind <repo>/.git <repo>/.git`
    /// (rw) on top of the usual system-ro / tmpfs / cwd binds — so NO host
    /// AF_UNIX socket (`SSH_AUTH_SOCK`, `/var/run/docker.sock`) is ever exposed —
    /// and macOS emits `(allow file-write* (subpath "<repo>/.git"))` alongside the
    /// cwd grant (viable only under an unrestricted-read profile, since git must
    /// also READ `<repo>/.git`). Docker, Landlock, and AppContainer ignore it
    /// (the fleet worktree flow is gated to bwrap / full-read macOS). The default
    /// (`None`) preserves today's cwd-only-writable behaviour for every config
    /// that never sets it.
    #[serde(default)]
    pub repo_git_write: Option<PathBuf>,

    /// Docker-specific settings (used when mode = "docker").
    #[serde(default)]
    pub docker: DockerConfig,

    /// Restrict file reads to these paths (plus the workspace cwd).
    /// Empty = allow all reads (default, backward compatible).
    /// Non-empty = only allow reads from cwd + these paths (kernel-enforced on macOS/Linux).
    #[serde(default)]
    pub read_allow_paths: Vec<String>,

    /// #1976 — per-path WRITE fence for the shell: workspace-relative globs
    /// (`*` / `?` within one segment — the same v1 syntax as
    /// `WorkerGrant::write_paths`, whose grant this projects) naming the ONLY
    /// paths shell commands may write inside the workspace. `None` (default,
    /// every pre-#1976 config) = `workspace_write` alone governs writes.
    ///
    /// Backend coverage (deny-wins everywhere — no backend widens the fence):
    /// - **macOS (sandbox-exec)** expresses it exactly: one
    ///   `(allow file-write* (regex ...))` per glob replaces the broad cwd
    ///   subpath grant; TMPDIR moves outside the workspace like the
    ///   read-only profile. The OS cannot distinguish create-vs-overwrite,
    ///   so `create_only`'s no-overwrite half stays TOOL-layer enforced.
    /// - **bwrap / Landlock / AppContainer** cannot bind/grant a glob (or a
    ///   create-target that does not exist yet): the workspace degrades to
    ///   READ-ONLY for the shell (fail closed; granted paths stay writable
    ///   via the fenced file tools), warned at sandbox construction.
    /// - **Docker** mounts the workspace `:ro` under a fence (same honest
    ///   degradation).
    /// - **NoSandbox** enforces nothing — construction warns that the shell
    ///   fence is UNENFORCED (tool-layer enforcement still applies).
    #[serde(default)]
    pub write_allow_globs: Option<Vec<String>>,

    /// Profile name for sandbox isolation (used as AppContainer profile ID on Windows).
    #[serde(default)]
    pub profile_name: Option<String>,
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
    // macOS `/etc` is a symlink to `/private/etc`, and SBPL subpath rules match
    // the CANONICAL path — so the `/etc` entry above never covers a real read of
    // `/private/etc/...`. Without this, TLS clients that resolve via the symlink
    // (system `curl`/LibreSSL reading `/etc/ssl/openssl.cnf` + `cert.pem`) fail at
    // init with a confusing "Operation not permitted" — very visible now that
    // network is allowed by default. Mirrors the `/tmp` + `/private/tmp` pairing.
    "/private/etc",
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
            workspace_write: true,
            repo_git_write: None,
            docker: DockerConfig::default(),
            read_allow_paths: Vec::new(),
            write_allow_globs: None,
            profile_name: None,
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
    /// Auto-detect: bwrap on Linux, sandbox-exec on macOS, AppContainer on Windows.
    #[default]
    Auto,
    /// Linux bubblewrap.
    Bwrap,
    /// Linux container sandbox using Landlock filesystem rules plus seccomp.
    Landlock,
    /// macOS sandbox-exec.
    Macos,
    /// Docker container isolation.
    Docker,
    /// Windows AppContainer isolation.
    #[serde(rename = "appcontainer")]
    AppContainer,
    /// No sandboxing (pass-through).
    None,
}

/// Trait for wrapping shell commands in a sandbox.
pub trait Sandbox: Send + Sync {
    /// Wrap a shell command string into a sandboxed `Command`.
    fn wrap_command(&self, shell_command: &str, cwd: &Path) -> Command;

    /// Whether this sandbox provides no confinement (runs commands directly).
    /// Lets callers that require confinement (e.g. the `mcp-serve` server path)
    /// fail closed when `SandboxMode::Auto` resolves to no backend. Real
    /// backends inherit the default `false`.
    fn is_noop(&self) -> bool {
        false
    }

    /// Whether this backend is the Docker container sandbox.
    ///
    /// #1607 (codex-review follow-up): Docker bind-mounts the workspace at a
    /// fixed in-container path (`/workspace`), but `Command` validators
    /// interpolate absolute *host* paths (e.g. `${output.patch_path}` ->
    /// `/host/ws/.../foo.patch`) which don't exist inside the container, so a
    /// previously-passing required validator would start failing. Before
    /// #1607, command validators ran on the host and worked. `ValidatorRunner`
    /// uses this to keep Docker-mode command validators on the pre-#1607 direct
    /// (host) path rather than silently breaking them. Full in-container path
    /// translation is a known follow-up. Non-Docker backends inherit `false`.
    fn is_docker(&self) -> bool {
        false
    }

    /// Whether this backend can grant WRITE to a repo's `.git` common dir
    /// ([`SandboxConfig::repo_git_write`]) TOGETHER WITH the reads git also needs
    /// — the capability the fleet worktree flow probes at serve boot. bwrap can
    /// (a targeted `--bind <repo>/.git <repo>/.git` plus its read binds); macOS
    /// can ONLY under an unrestricted-read profile (a restricted-read profile
    /// would grant the `<repo>/.git` write but deny the `<repo>/.git` READ
    /// `git commit` needs); Docker, Landlock, AppContainer, and [`NoSandbox`]
    /// cannot, so the pool falls back to a scratch workspace (no worktree, no
    /// lost deliverable). The pool gate ANDs this with `FsGrant::Host` and a git
    /// controller root. This is the surviving kernel of the parked
    /// `honors_write_allow_paths`.
    fn supports_repo_git_write(&self) -> bool {
        false
    }
}

/// No-op sandbox: executes commands directly.
pub struct NoSandbox;

impl Sandbox for NoSandbox {
    fn is_noop(&self) -> bool {
        true
    }

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

/// #1976 — the effective `workspace_write` for a backend that CANNOT express
/// a per-path write fence (bwrap/Landlock/AppContainer binds and grants are
/// concrete paths; a glob — or a create-target that does not exist yet —
/// cannot be bound). Deny-wins: under a fence the workspace degrades to
/// READ-ONLY for the shell (granted paths stay writable via the fenced file
/// tools), warned once per sandbox construction (≈ once per spawned attempt).
fn fence_degraded_workspace_write(config: &SandboxConfig, backend: &str) -> bool {
    if config.write_allow_globs.is_some() {
        tracing::warn!(
            backend,
            "per-path write grant: {backend} cannot express per-path shell writes; \
             the workspace is READ-ONLY for the shell on this backend (granted paths \
             remain writable via the fenced file tools only)",
        );
        return false;
    }
    config.workspace_write
}

/// #1976 — Docker's projection of the same degradation: a fenced workspace
/// mounts `:ro` (mount targets are concrete paths, same limitation as bwrap).
fn fence_degraded_docker(config: &SandboxConfig) -> DockerConfig {
    let mut docker = config.docker.clone();
    if config.write_allow_globs.is_some() && docker.mount_mode == MountMode::ReadWrite {
        tracing::warn!(
            "per-path write grant: docker cannot express per-path shell writes; \
             the workspace mounts READ-ONLY (granted paths remain writable via the \
             fenced file tools only)",
        );
        docker.mount_mode = MountMode::ReadOnly;
    }
    docker
}

/// #1976 — a fenced config resolving to NO sandbox leaves the shell fence
/// unenforced entirely; never let that pass silently.
fn warn_fence_unenforced(config: &SandboxConfig) {
    if config.write_allow_globs.is_some() {
        tracing::warn!(
            "per-path write grant: no sandbox backend — the SHELL write fence is \
             UNENFORCED (file-tool enforcement still applies)",
        );
    }
}

/// Create a sandbox from config.
pub fn create_sandbox(config: &SandboxConfig) -> Box<dyn Sandbox> {
    if !config.enabled {
        tracing::info!("sandbox disabled, shell commands run without isolation");
        warn_fence_unenforced(config);
        return Box::new(NoSandbox);
    }

    match &config.mode {
        SandboxMode::None => {
            warn_fence_unenforced(config);
            Box::new(NoSandbox)
        }
        SandboxMode::Bwrap => Box::new(BwrapSandbox {
            allow_network: config.allow_network,
            workspace_write: fence_degraded_workspace_write(config, "bwrap"),
            repo_git_write: config.repo_git_write.clone(),
        }),
        SandboxMode::Landlock => {
            #[cfg(target_os = "linux")]
            {
                Box::new(LinuxContainerSandbox {
                    allow_network: config.allow_network,
                    read_allow_paths: config.read_allow_paths.clone(),
                    profile_name: config.profile_name.clone(),
                    workspace_write: fence_degraded_workspace_write(config, "landlock"),
                })
            }
            #[cfg(not(target_os = "linux"))]
            {
                tracing::warn!(
                    "Landlock/seccomp is only available on Linux, falling back to NoSandbox"
                );
                warn_fence_unenforced(config);
                Box::new(NoSandbox)
            }
        }
        SandboxMode::Macos => Box::new(MacosSandbox {
            allow_network: config.allow_network,
            read_allow_paths: config.read_allow_paths.clone(),
            workspace_write: config.workspace_write,
            repo_git_write: config.repo_git_write.clone(),
            // #1976: macOS EXPRESSES the fence (per-glob SBPL regex rules).
            write_allow_globs: config.write_allow_globs.clone(),
        }),
        SandboxMode::Docker => Box::new(DockerSandbox {
            config: fence_degraded_docker(config),
            allow_network: config.allow_network,
        }),
        SandboxMode::AppContainer => {
            #[cfg(windows)]
            {
                Box::new(AppContainerSandbox {
                    allow_network: config.allow_network,
                    read_allow_paths: config.read_allow_paths.clone(),
                    profile_name: config.profile_name.clone(),
                    workspace_write: fence_degraded_workspace_write(config, "appcontainer"),
                })
            }
            #[cfg(not(windows))]
            {
                tracing::warn!(
                    "AppContainer is only available on Windows, falling back to NoSandbox"
                );
                warn_fence_unenforced(config);
                Box::new(NoSandbox)
            }
        }
        SandboxMode::Auto => create_auto_sandbox(config),
    }
}

/// Which backend [`SandboxMode::Auto`] would select on this host — a stable
/// human-readable label plus whether that selection actually sandboxes
/// (`false` = [`NoSandbox`]). Runs the SAME availability probes as
/// [`create_auto_sandbox`] (on Linux `bwrap_works` actually runs
/// `bwrap --version`), reported instead of instantiated. Used by
/// `octos doctor` so its sandbox row reflects the real runtime selection
/// rather than a PATH existence guess; the boolean keeps callers from
/// sniffing the label text for status.
pub fn auto_sandbox_kind() -> (&'static str, bool) {
    #[cfg(target_os = "macos")]
    {
        if which_exists("sandbox-exec") {
            return ("macOS Seatbelt (sandbox-exec)", true);
        }
    }
    #[cfg(target_os = "linux")]
    {
        if bwrap_works() {
            return ("bubblewrap (bwrap)", true);
        }
        if linux_container_sandbox_available() {
            return ("Linux container helper (Landlock/seccomp)", true);
        }
    }
    #[cfg(target_os = "windows")]
    {
        if has_sandbox_helper() {
            return ("Windows AppContainer", true);
        }
    }
    if which_exists("docker") {
        ("Docker", true)
    } else {
        ("none — shell commands would run UNSANDBOXED", false)
    }
}

fn create_auto_sandbox(config: &SandboxConfig) -> Box<dyn Sandbox> {
    #[cfg(target_os = "macos")]
    {
        if which_exists("sandbox-exec") {
            return Box::new(MacosSandbox {
                allow_network: config.allow_network,
                read_allow_paths: config.read_allow_paths.clone(),
                workspace_write: config.workspace_write,
                repo_git_write: config.repo_git_write.clone(),
                // #1976: macOS EXPRESSES the fence (per-glob regex rules).
                write_allow_globs: config.write_allow_globs.clone(),
            });
        }
    }

    #[cfg(target_os = "linux")]
    {
        if bwrap_works() {
            return Box::new(BwrapSandbox {
                allow_network: config.allow_network,
                workspace_write: fence_degraded_workspace_write(config, "bwrap"),
                repo_git_write: config.repo_git_write.clone(),
            });
        }
        if linux_container_sandbox_available() {
            return Box::new(LinuxContainerSandbox {
                allow_network: config.allow_network,
                read_allow_paths: config.read_allow_paths.clone(),
                profile_name: config.profile_name.clone(),
                workspace_write: fence_degraded_workspace_write(config, "landlock"),
            });
        }
    }

    #[cfg(target_os = "windows")]
    {
        if has_sandbox_helper() {
            return Box::new(AppContainerSandbox {
                allow_network: config.allow_network,
                read_allow_paths: config.read_allow_paths.clone(),
                profile_name: config.profile_name.clone(),
                workspace_write: fence_degraded_workspace_write(config, "appcontainer"),
            });
        }
    }

    if which_exists("docker") {
        Box::new(DockerSandbox {
            config: fence_degraded_docker(config),
            allow_network: config.allow_network,
        })
    } else {
        tracing::warn!(
            "no sandbox backend found (bwrap, Landlock/seccomp, sandbox-exec, docker, or AppContainer). \
             Shell commands will run WITHOUT isolation. \
             Install a sandbox backend or set sandbox.enabled = false to silence this warning."
        );
        warn_fence_unenforced(config);
        Box::new(NoSandbox)
    }
}

#[cfg(target_os = "linux")]
fn bwrap_works() -> bool {
    if !which_exists("bwrap") {
        return false;
    }

    let mut cmd = std::process::Command::new("bwrap");
    for dir in ["/usr", "/lib", "/lib64", "/bin", "/sbin", "/etc"] {
        if Path::new(dir).exists() {
            cmd.arg("--ro-bind").arg(dir).arg(dir);
        }
    }
    cmd.arg("--dev")
        .arg("/dev")
        .arg("--proc")
        .arg("/proc")
        .arg("--tmpfs")
        .arg("/tmp")
        .arg("--die-with-parent")
        .arg("--")
        .arg(if Path::new("/bin/true").exists() {
            "/bin/true"
        } else {
            "/usr/bin/true"
        })
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[cfg(target_os = "linux")]
fn linux_container_sandbox_available() -> bool {
    find_sandbox_helper_path()
        .and_then(|helper| {
            std::process::Command::new(helper)
                .arg("--probe-linux")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .ok()
        })
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Check if the octos-sandbox helper binary is available.
#[cfg(windows)]
fn has_sandbox_helper() -> bool {
    find_sandbox_helper_path().is_some()
}

#[cfg(any(windows, target_os = "linux"))]
fn find_sandbox_helper_path() -> Option<String> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let helper = if cfg!(windows) {
                dir.join("octos-sandbox.exe")
            } else {
                dir.join("octos-sandbox")
            };
            if helper.exists() {
                return Some(helper.to_string_lossy().into_owned());
            }
        }
    }
    if which_exists("octos-sandbox") {
        Some("octos-sandbox".to_string())
    } else {
        None
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
            (SandboxMode::Landlock, "\"landlock\""),
            (SandboxMode::Macos, "\"macos\""),
            (SandboxMode::Docker, "\"docker\""),
            (SandboxMode::AppContainer, "\"appcontainer\""),
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
        assert!(
            config.enabled,
            "sandbox should be enabled by default when field is missing"
        );
        assert_eq!(config.mode, SandboxMode::Auto);
        assert!(!config.allow_network);
        assert_eq!(config.docker.image, "ubuntu:24.04");
    }

    #[test]
    fn test_sandbox_config_explicit_disable() {
        let config: SandboxConfig = serde_json::from_str(r#"{"enabled": false}"#).unwrap();
        assert!(!config.enabled);
    }

    // --- create_sandbox with SandboxMode::None ---

    #[test]
    fn test_create_sandbox_mode_none() {
        let config = SandboxConfig {
            enabled: true,
            mode: SandboxMode::None,
            allow_network: false,
            workspace_write: true,
            repo_git_write: None,
            docker: DockerConfig::default(),
            read_allow_paths: Vec::new(),
            write_allow_globs: None,
            profile_name: None,
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

    // --- #1976: per-path write fence (write_allow_globs) ---

    #[test]
    fn sandbox_config_write_allow_globs_defaults_none() {
        // Back-compat: configs written before #1976 carry no fence.
        let config: SandboxConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(config.write_allow_globs, None);
        assert_eq!(SandboxConfig::default().write_allow_globs, None);
    }

    #[test]
    fn fence_degrades_bwrap_to_ro_workspace() {
        // #1976 honest degradation: bwrap binds are CONCRETE paths — a glob
        // (or a create-target that does not exist yet) cannot be bind-mounted,
        // so a fenced workspace is bound READ-ONLY for the shell (fail
        // closed; granted paths stay writable via the fenced file tools).
        let config = SandboxConfig {
            mode: SandboxMode::Bwrap,
            workspace_write: true,
            write_allow_globs: Some(vec!["exemplar.card".to_string()]),
            ..SandboxConfig::default()
        };
        let sb = create_sandbox(&config);
        let dir = tempfile::tempdir().unwrap();
        let cmd = sb.wrap_command("echo hi", dir.path());
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        let cwd_str = dir.path().to_string_lossy().to_string();
        // The workspace is bound with `--ro-bind` (read-only), not the
        // read-write `--bind`. (`--chdir <cwd>` also names cwd_str, so match
        // the bind flags specifically, not merely "cwd appears as w[1]".)
        let bound_ro = args
            .windows(2)
            .any(|w| w[0] == "--ro-bind" && w[1] == cwd_str);
        let bound_rw = args.windows(2).any(|w| w[0] == "--bind" && w[1] == cwd_str);
        assert!(
            bound_ro && !bound_rw,
            "a fenced workspace must be read-only (--ro-bind) under bwrap, args: {args:?}"
        );
    }

    #[test]
    fn fence_degrades_docker_mount_to_ro() {
        // #1976 honest degradation: Docker mounts are concrete too — a
        // fenced workspace mounts `:ro` (fail closed for the shell).
        let config = SandboxConfig {
            mode: SandboxMode::Docker,
            write_allow_globs: Some(vec!["exemplar.card".to_string()]),
            ..SandboxConfig::default()
        };
        let sb = create_sandbox(&config);
        let dir = tempfile::tempdir().unwrap();
        let cmd = sb.wrap_command("echo hi", dir.path());
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert!(
            args.iter().any(|a| a.ends_with(":/workspace:ro")),
            "a fenced workspace must mount read-only under docker, args: {args:?}"
        );
    }

    #[test]
    fn fence_reaches_macos_backend_as_globs() {
        // macOS is the one backend that EXPRESSES the fence (SBPL regex);
        // create_sandbox must thread the globs through, not degrade them.
        let config = SandboxConfig {
            mode: SandboxMode::Macos,
            write_allow_globs: Some(vec!["exemplar.card".to_string()]),
            ..SandboxConfig::default()
        };
        let sb = create_sandbox(&config);
        let dir = tempfile::tempdir().unwrap();
        let cmd = sb.wrap_command("echo hi", dir.path());
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        let profile = args
            .iter()
            .find(|a| a.contains("deny default"))
            .expect("SBPL profile present");
        assert!(
            profile.contains("(allow file-write* (regex #\""),
            "macOS must emit per-glob regex write rules, profile: {profile}"
        );
    }

    // --- is_noop contract (fail-closed callers depend on this) ---

    #[test]
    fn no_sandbox_reports_noop() {
        // The `mcp-serve` fail-closed check and the validator direct-argv path
        // both key off `is_noop()`. NoSandbox provides zero confinement, so it
        // must report `true`; the trait default (real backends) is `false`.
        assert!(
            NoSandbox.is_noop(),
            "NoSandbox must report is_noop() == true"
        );
    }

    #[test]
    fn disabled_and_none_modes_yield_noop_sandbox() {
        // Both an explicitly-disabled sandbox and `mode = none` must resolve to
        // a no-op backend, so a fail-closed caller can distinguish "operator
        // opted out" (respect it) from "wanted a sandbox, none available"
        // (refuse). This is host-independent.
        for config in [
            SandboxConfig {
                enabled: false,
                ..SandboxConfig::default()
            },
            SandboxConfig {
                enabled: true,
                mode: SandboxMode::None,
                ..SandboxConfig::default()
            },
        ] {
            assert!(
                create_sandbox(&config).is_noop(),
                "config {config:?} must produce a no-op sandbox"
            );
        }
    }
}
