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

    /// Fail closed when `mode = "auto"` resolves to NO backend (default:
    /// `false`).
    ///
    /// `Auto` means "best available", so by default a host with no backend
    /// degrades to unconfined execution — loudly (warned once per process),
    /// never silently. Operators who would rather have every command REFUSE
    /// to run than ever run unconfined set this to `true` and get a typed
    /// [`SandboxUnavailable`] refusal instead. The explicit opt-outs
    /// (`enabled = false`, `mode = "none"`) still win over this knob.
    /// Explicit backend modes are unaffected: an explicit mode that cannot
    /// be honored on this host ALWAYS refuses, knob or no knob.
    #[serde(default)]
    pub fail_closed: bool,

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

    /// Build-cache pool slot this session's shell may WRITE, beyond the cwd
    /// (default: `None`). Outer-loop #4 (docs/build-cache-pool.md §7.2):
    /// a peer's `CARGO_TARGET_DIR` lives in the pool
    /// (`<data_dir>/build-cache/<repo-key>/slot-N/target`), which is OUTSIDE
    /// the peer's clone cwd — without this grant `(deny default)` denies
    /// every cargo write. The grant is INDEPENDENT of `workspace_write`,
    /// `write_allow_globs`, and the toolchain grants on purpose: those
    /// express "what may change inside the workspace" and are suppressed by
    /// deny-wins fences, while the slot is harness-allocated infrastructure
    /// OUTSIDE the workspace (the same reasoning as the external tmp write
    /// rule). Only the peer's OWN slot is ever granted (I4: other slots and
    /// other repositories' pools stay default-denied).
    ///
    /// Backend cover: macOS emits `(allow file-read*|file-write* (subpath
    /// "<slot>"))`. bwrap/docker/landlock cannot add a writable bind for an
    /// arbitrary out-of-workspace dir in this cut — known degradation (a
    /// slot-pointing build fails there until a bind follow-up lands).
    #[serde(default)]
    pub build_cache_slot: Option<PathBuf>,

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

    /// Grant the shell WRITE to the handful of paths a language toolchain
    /// must touch to function at all (default: `true`).
    ///
    /// The motivating failure: with writes confined to the cwd, `cargo build`
    /// dies before compiling anything — rustup's cargo shim takes a write
    /// lock on `~/.rustup/settings.toml` ("could not read settings file:
    /// Operation not permitted"), and cargo itself must populate
    /// `~/.cargo/registry`. A coding agent that cannot compile writes broken
    /// code with no feedback loop, so the pragmatic default is on.
    ///
    /// The grant is PRECISE, not a blanket toolchain-home write:
    /// `~/.cargo/bin` (on PATH — a writable shim there is persistence) and
    /// `~/.rustup/toolchains` (writable compiler binaries) are deliberately
    /// NOT granted. Deny-wins: a read-only workspace
    /// (`workspace_write: false`) or a #1976 write fence suppresses these
    /// grants entirely — a profile that says "this shell writes nothing /
    /// only these globs" must not quietly regain toolchain caches.
    #[serde(default = "default_enabled")]
    pub allow_toolchains: bool,
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

/// The write grants a detected toolchain needs — split by SBPL rule kind:
/// `literals` are single files, `subpaths` are directory trees.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct ToolchainWriteGrants {
    pub(crate) literals: Vec<String>,
    pub(crate) subpaths: Vec<String>,
}

/// The PRECISE write set a Rust build needs inside the sandbox.
///
/// Reads of `~/.cargo` and `~/.rustup` are globally allowed, so cached
/// dependencies, the registry index, and the installed toolchain are all
/// usable WITHOUT any write grant. The only default write is:
/// - `<cargo>/.package-cache` — cargo's advisory build lock, opened
///   write-intent on every invocation (an EPERM here fails the build).
///
/// When `allow_network` is set (the operator's explicit trust decision,
/// which is also what makes fresh fetches possible), the DOWNLOAD-write
/// set is added: `<cargo>/registry/{index,cache,src}` and `<cargo>/git`.
/// These hold code cargo executes, so they are writable ONLY under
/// network-on; the default keeps them read-only to prevent cross-workspace
/// poisoning.
///
/// Nothing under `~/.rustup` is ever granted (a plain build only reads it),
/// and `<cargo>/bin` / `<rustup>/toolchains` are never writable
/// (persistence vectors). The sandbox supports BUILDING with cached
/// dependencies by default; fresh downloads need `allow_network` (see
/// also the proxy-isolated-fetch follow-up).
pub(crate) fn toolchain_write_grants(allow_network: bool) -> ToolchainWriteGrants {
    let mut grants = ToolchainWriteGrants::default();
    let home = std::env::var("HOME").ok();
    let resolve = |env_name: &str, conventional: &str| -> Option<PathBuf> {
        let path = std::env::var(env_name)
            .ok()
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|h| Path::new(h).join(conventional)))?;
        if !path.is_dir() {
            return None;
        }
        // #2136 review: seatbelt matches the SYMLINK-RESOLVED path (macos.rs
        // canonicalizes cwd/git for the same reason). A symlinked HOME or
        // CARGO_HOME (e.g. /tmp -> /private/tmp) would otherwise emit a rule
        // that never matches and the write stays denied — fail-closed and
        // silent. Canonicalize the (existing) home dir; leaf names append
        // cleanly onto the resolved path.
        Some(std::fs::canonicalize(&path).unwrap_or(path))
    };
    // Cargo, default (network OFF): ONLY the advisory build lock. Reads of
    // the whole cargo home are globally allowed, so a cached/offline build
    // reads its deps and index without any write — the reviewer confirmed
    // a live cached build with the index entirely READ-ONLY. registry
    // index/cache/src and git stay read-only so nothing can corrupt
    // dependency resolution or overwrite a crate across workspaces
    // (#2136 review round 3, P1).
    if let Some(cargo) = resolve("CARGO_HOME", ".cargo") {
        push_cargo_grants(&mut grants, &cargo, allow_network);
    }

    // NO rustup grants (#2136 review round 3, P1): a plain build via the
    // rustup proxy only READS ~/.rustup (default-toolchain lookup), which
    // is globally allowed; it does not write settings.toml or the toolchain
    // dirs. Granting settings.toml write let a sandboxed command
    // persistently change the user's default toolchain/overrides — removed.
    // (rustup's original "could not READ settings" symptom was an
    // octoscode read-restriction, not an octos one.)
    grants
}

/// Pure grant-builder for a KNOWN cargo home (no filesystem probe) — the
/// testable core of [`toolchain_write_grants`]. Default: just the advisory
/// build lock. With `allow_network`, the download-write set
/// (registry/{index,cache,src} + git) is added. Never the persistence
/// vectors (bin, toolchains).
fn push_cargo_grants(grants: &mut ToolchainWriteGrants, cargo: &Path, allow_network: bool) {
    grants
        .literals
        .push(cargo.join(".package-cache").to_string_lossy().into_owned());
    if allow_network {
        for dir in ["registry/index", "registry/cache", "registry/src", "git"] {
            grants
                .subpaths
                .push(cargo.join(dir).to_string_lossy().into_owned());
        }
    }
}

/// The grants a config asks for: the detected set when `allow_toolchains`
/// is on, nothing otherwise.
fn configured_toolchain_grants(config: &SandboxConfig) -> ToolchainWriteGrants {
    if config.allow_toolchains {
        toolchain_write_grants(config.allow_network)
    } else {
        ToolchainWriteGrants::default()
    }
}

/// The kernel phrasings a sandbox denial surfaces as, per backend: macOS
/// seatbelt returns EPERM ("Operation not permitted"), Landlock returns
/// EACCES ("Permission denied"), bwrap ro-binds and Docker `:ro` mounts
/// return EROFS ("Read-only file system").
const DENIAL_PHRASES: &[&str] = &[
    "Operation not permitted",
    "Permission denied",
    "Read-only file system",
];

/// Explain a sandbox denial the kernel reports as a bare errno string.
///
/// Inside the sandbox, a denied access surfaces as the failing program's
/// own confused error ("could not read settings file: Operation not
/// permitted"), which reads as a bug in the command — the one party that
/// knows the sandbox denied it is the harness, so the harness must say so.
/// Observed cost of not saying so: a coding session whose every `cargo`
/// invocation died on the rustup settings lock, with the model (and user)
/// left debugging cargo instead of the sandbox.
///
/// Returns a hint ONLY when the command FAILED under a real sandbox and
/// `scan_text` carries one of the kernel's denial phrases — a successful
/// command that merely logged the phrase is not a denial. Callers scan the
/// PRE-truncation text (the denial line may be exactly what truncation
/// cuts) and append the hint AFTER truncating, so the hint itself survives.
///
/// The toolchain-cache pointer is macOS-only on purpose: `allow_toolchains`
/// grants are implemented in the seatbelt backend today, and advertising
/// the lever on a backend that ignores it would misstate what is writable.
pub(crate) fn sandbox_denial_hint(
    sandboxed: bool,
    success: bool,
    scan_text: &str,
) -> Option<&'static str> {
    if success || !sandboxed {
        return None;
    }
    if !DENIAL_PHRASES
        .iter()
        .any(|phrase| scan_text.contains(phrase))
    {
        return None;
    }
    if cfg!(target_os = "macos") {
        Some(
            "\n[sandbox] This denial usually means the OS sandbox blocked a file access \
             outside the workspace — not a bug in the command. With \
             `sandbox.allow_toolchains` on (the default), builds with CACHED \
             dependencies work; fetching NEW crates is denied unless \
             `sandbox.allow_network` is also enabled. Other paths need an \
             explicit allowance in the sandbox config.",
        )
    } else {
        Some(
            "\n[sandbox] This denial usually means the OS sandbox blocked a file access \
             outside the workspace — not a bug in the command. Grant the path in the \
             sandbox config, or run under a less restrictive sandbox mode.",
        )
    }
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: SandboxMode::Auto,
            fail_closed: false,
            allow_network: false,
            workspace_write: true,
            repo_git_write: None,
            build_cache_slot: None,
            docker: DockerConfig::default(),
            read_allow_paths: Vec::new(),
            write_allow_globs: None,
            profile_name: None,
            allow_toolchains: true,
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

    /// Wrap a shell call with its current harness-held build-cache slot.
    /// Backends without a contextual slot grant keep their existing behavior.
    /// A supporting backend treats this option as authoritative, including
    /// None, without changing the shared sandbox or legacy wrap_command config.
    fn wrap_command_with_build_cache_slot(
        &self,
        shell_command: &str,
        cwd: &Path,
        _slot: Option<&Path>,
    ) -> Command {
        self.wrap_command(shell_command, cwd)
    }

    /// Whether this sandbox provides no confinement (runs commands directly).
    /// Lets callers that require confinement (e.g. the `mcp-serve` server path)
    /// fail closed when `SandboxMode::Auto` resolves to no backend. Real
    /// backends inherit the default `false`.
    fn is_noop(&self) -> bool {
        false
    }

    /// Typed refusal carried by a fail-closed resolution ([`RefusingSandbox`]):
    /// `Some` means EVERY command must refuse to run — the operator requested
    /// confinement that cannot be honored on this host. Exec-shaped tools
    /// short-circuit on this and return the remediation text instead of
    /// spawning; fail-closed gates (mcp-serve sessions, the fleet pool and
    /// worker) treat it like a missing backend. Real backends and
    /// [`NoSandbox`] inherit `None`.
    fn refusal(&self) -> Option<&SandboxUnavailable> {
        None
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

// ---------------------------------------------------------------------------
// Sandbox resolution — the OS-agnostic decision layer.
//
// `create_sandbox` used to interleave `cfg!`-gated probing with construction,
// which made the resolution matrix untestable off-host and let two explicit
// modes silently degrade to no confinement. The decision is now a pure
// function over (config, host OS, backend availability): every platform's
// matrix is exercised from any host, and construction is a thin projection.
// ---------------------------------------------------------------------------

/// The operating system a resolution runs on — data, not `cfg!`, so the FULL
/// platform matrix is testable from any host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostOs {
    /// Linux (bwrap and the Landlock/seccomp helper are candidates).
    Linux,
    /// macOS (sandbox-exec is a candidate).
    Macos,
    /// Windows (the AppContainer helper is a candidate).
    Windows,
    /// Any other OS: Docker is the only candidate backend.
    Other,
}

impl HostOs {
    /// The OS this binary was built for.
    pub fn current() -> Self {
        if cfg!(target_os = "linux") {
            HostOs::Linux
        } else if cfg!(target_os = "macos") {
            HostOs::Macos
        } else if cfg!(windows) {
            HostOs::Windows
        } else {
            HostOs::Other
        }
    }

    /// Human-readable name for refusal messages.
    fn label(self) -> &'static str {
        match self {
            HostOs::Linux => "Linux",
            HostOs::Macos => "macOS",
            HostOs::Windows => "Windows",
            HostOs::Other => "this OS",
        }
    }
}

/// Host backend availability, probed LAZILY: [`decide_sandbox`] calls each
/// method only when the mode/OS combination actually considers that backend,
/// preserving the probe order (and probe count) `SandboxMode::Auto` always
/// had. Tests substitute a fixed table to exercise every platform's matrix.
pub trait HostBackendProbe {
    /// macOS `sandbox-exec` is on PATH.
    fn sandbox_exec(&self) -> bool;
    /// Linux bubblewrap WORKS (the probe actually runs `bwrap`, not a PATH scan).
    fn bwrap(&self) -> bool;
    /// The `octos-sandbox` Landlock/seccomp helper answers `--probe-linux`.
    fn linux_container_helper(&self) -> bool;
    /// The `octos-sandbox.exe` AppContainer helper is present.
    fn windows_container_helper(&self) -> bool;
    /// `docker` is on PATH.
    fn docker(&self) -> bool;
}

/// The production probe: real host checks; `false` for backends whose probe
/// helpers the build target does not even compile.
struct RealHostProbe;

impl HostBackendProbe for RealHostProbe {
    fn sandbox_exec(&self) -> bool {
        which_exists("sandbox-exec")
    }

    fn bwrap(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            bwrap_works()
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }

    fn linux_container_helper(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            linux_container_sandbox_available()
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }

    fn windows_container_helper(&self) -> bool {
        #[cfg(windows)]
        {
            has_sandbox_helper()
        }
        #[cfg(not(windows))]
        {
            false
        }
    }

    fn docker(&self) -> bool {
        which_exists("docker")
    }
}

/// Which concrete backend a resolution selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxBackendChoice {
    /// macOS Seatbelt via `sandbox-exec`.
    Macos,
    /// Linux bubblewrap.
    Bwrap,
    /// Linux Landlock/seccomp via the `octos-sandbox` helper.
    Landlock,
    /// Windows AppContainer via the `octos-sandbox.exe` helper.
    AppContainer,
    /// Docker container isolation (any OS).
    Docker,
}

impl SandboxBackendChoice {
    /// Stable human-readable label (`octos doctor`'s sandbox row).
    fn label(self) -> &'static str {
        match self {
            SandboxBackendChoice::Macos => "macOS Seatbelt (sandbox-exec)",
            SandboxBackendChoice::Bwrap => "bubblewrap (bwrap)",
            SandboxBackendChoice::Landlock => "Linux container helper (Landlock/seccomp)",
            SandboxBackendChoice::AppContainer => "Windows AppContainer",
            SandboxBackendChoice::Docker => "Docker",
        }
    }
}

/// Why a resolution yielded NO confinement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnconfinedReason {
    /// `sandbox.enabled = false` — the operator's explicit opt-out
    /// (`--danger-full-access` sets this). Beats `fail_closed`.
    Disabled,
    /// `sandbox.mode = "none"` — the equally explicit opt-out. Beats
    /// `fail_closed`.
    ExplicitNone,
    /// `SandboxMode::Auto` ("best available" by contract) found no backend
    /// and `sandbox.fail_closed` is unset: the one LEGAL degradation —
    /// warned once per process, never silent.
    AutoNoBackend,
}

/// Typed refusal: the configured sandbox cannot be honored on this host, and
/// the resolution refuses to run commands unconfined instead of degrading.
///
/// Two audiences, deliberately split (#2196 review MUST-FIX):
/// - `Display` is the MODEL-FACING text — it flows verbatim into refused
///   tool results, wrap-time stderr, mcp-serve session errors, and fleet
///   termination reasons. It names the mismatch and points at the operator,
///   and it deliberately does NOT name the config keys that remove
///   confinement (`enabled=false` / `mode="none"` / danger-full-access):
///   advice that teaches a confined model the exact keys that disable its
///   sandbox is itself an escape vector, because config files stay editable
///   through the (unaffected) file tools even while the shell refuses.
/// - [`Self::remediation`] is the OPERATOR-FACING text (concrete per-OS
///   installs plus the explicit opt-outs). It is surfaced only via the
///   creation-time `tracing::error!` and doctor-adjacent surfaces — never
///   through `Display`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxUnavailable {
    /// The requested mode, in config spelling (e.g. `"bwrap"`, `"auto"`).
    pub requested: String,
    /// Why it cannot be honored on this host.
    pub reason: String,
    /// OPERATOR-facing remediation for this OS (installs + explicit
    /// opt-outs). Excluded from `Display` on purpose — see the struct docs.
    pub remediation: String,
}

impl std::fmt::Display for SandboxUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "sandbox unavailable (mode \"{}\"): {}. Refusing to run commands unconfined. \
             This cannot be fixed from inside the session: an operator must repair the \
             sandbox configuration or install a sandbox backend on this host (`octos \
             doctor` shows host-specific remediation). Shell/exec commands will keep \
             refusing until then.",
            self.requested, self.reason
        )
    }
}

impl std::error::Error for SandboxUnavailable {}

impl SandboxUnavailable {
    /// One-line rendering safe to embed in a `sh -c` / `cmd /C` echo: only
    /// shell-inert characters survive, so no quoting bug can ever turn the
    /// refusal back into command execution.
    fn stderr_line(&self) -> String {
        let line = format!(
            "sandbox unavailable, mode {}: {} Refusing to run the command unconfined. \
             Fix the sandbox config or run octos doctor.",
            self.requested, self.reason
        );
        line.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || " ./:_=,-".contains(c) {
                    c
                } else {
                    ' '
                }
            })
            .collect()
    }
}

/// The outcome of resolving a [`SandboxConfig`] against a host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxDecision {
    /// Confine with this backend.
    Confine(SandboxBackendChoice),
    /// Run without confinement, for this reason (always logged, never silent).
    Unconfined(UnconfinedReason),
    /// Fail closed: every command must refuse to run.
    Refuse(SandboxUnavailable),
}

/// The per-OS remediation block every refusal carries.
fn remediation_for(os: HostOs) -> String {
    let install = match os {
        HostOs::Linux => {
            "Install bubblewrap (e.g. `apt install bubblewrap`) or the octos-sandbox \
             Landlock helper, or install Docker."
        }
        HostOs::Macos => "sandbox-exec ships with macOS — restore it on PATH, or install Docker.",
        HostOs::Windows => {
            "Install the octos-sandbox.exe AppContainer helper next to the octos binary, \
             or install Docker Desktop (a native no-helper Windows runner is tracked in \
             octos-org/octos issue 2195)."
        }
        HostOs::Other => "Install Docker (the only supported backend on this OS).",
    };
    format!(
        "{install} To accept running UNCONFINED instead, set sandbox.enabled=false or \
         sandbox.mode=\"none\" (the explicit opt-outs)."
    )
}

/// Refusal for an explicit mode that requires a different OS.
fn refuse_wrong_os(requested: &str, needs: &str, os: HostOs) -> SandboxDecision {
    SandboxDecision::Refuse(SandboxUnavailable {
        requested: requested.to_string(),
        reason: format!(
            "sandbox.mode=\"{requested}\" requires {needs}; this host is {}",
            os.label()
        ),
        remediation: remediation_for(os),
    })
}

/// Refusal for an explicit mode whose backend is missing/broken on this host.
fn refuse_missing(requested: &str, missing: &str, os: HostOs) -> SandboxDecision {
    SandboxDecision::Refuse(SandboxUnavailable {
        requested: requested.to_string(),
        reason: format!("sandbox.mode=\"{requested}\" is set but {missing}"),
        remediation: remediation_for(os),
    })
}

/// Resolve a [`SandboxConfig`] against a host: which backend to use, whether
/// to run unconfined, or whether to refuse. Pure — all host facts arrive as
/// arguments — so every OS's matrix is tested from any development host.
///
/// The contract:
/// - `enabled = false` and `mode = "none"` are explicit opt-outs: unconfined,
///   and they beat `fail_closed`.
/// - An EXPLICIT backend mode that cannot be honored on this host (wrong OS,
///   or the backend is missing/broken) REFUSES — it never silently degrades
///   to no confinement, and never constructs a backend whose every spawn
///   would fail with a bare ENOENT.
/// - `Auto` picks the best available backend (native first, then Docker).
///   With none available it degrades to unconfined — warned, never silent —
///   unless `fail_closed` is set, which turns the degradation into a refusal.
pub fn decide_sandbox(
    config: &SandboxConfig,
    os: HostOs,
    probe: &dyn HostBackendProbe,
) -> SandboxDecision {
    if !config.enabled {
        return SandboxDecision::Unconfined(UnconfinedReason::Disabled);
    }
    match &config.mode {
        SandboxMode::None => SandboxDecision::Unconfined(UnconfinedReason::ExplicitNone),
        SandboxMode::Bwrap => {
            if os != HostOs::Linux {
                refuse_wrong_os("bwrap", "Linux", os)
            } else if !probe.bwrap() {
                refuse_missing(
                    "bwrap",
                    "the bubblewrap availability probe failed on this host (bwrap is \
                     missing or cannot create namespaces here)",
                    os,
                )
            } else {
                SandboxDecision::Confine(SandboxBackendChoice::Bwrap)
            }
        }
        SandboxMode::Landlock => {
            if os != HostOs::Linux {
                refuse_wrong_os("landlock", "Linux", os)
            } else if !probe.linux_container_helper() {
                refuse_missing(
                    "landlock",
                    "the octos-sandbox Landlock/seccomp helper is not available",
                    os,
                )
            } else {
                SandboxDecision::Confine(SandboxBackendChoice::Landlock)
            }
        }
        SandboxMode::Macos => {
            if os != HostOs::Macos {
                refuse_wrong_os("macos", "macOS (sandbox-exec)", os)
            } else if !probe.sandbox_exec() {
                refuse_missing("macos", "sandbox-exec was not found on PATH", os)
            } else {
                SandboxDecision::Confine(SandboxBackendChoice::Macos)
            }
        }
        SandboxMode::Docker => {
            if probe.docker() {
                SandboxDecision::Confine(SandboxBackendChoice::Docker)
            } else {
                refuse_missing("docker", "docker was not found on PATH", os)
            }
        }
        SandboxMode::AppContainer => {
            if os != HostOs::Windows {
                refuse_wrong_os("appcontainer", "Windows", os)
            } else if !probe.windows_container_helper() {
                refuse_missing(
                    "appcontainer",
                    "the octos-sandbox.exe AppContainer helper was not found next to \
                     the octos binary or on PATH",
                    os,
                )
            } else {
                SandboxDecision::Confine(SandboxBackendChoice::AppContainer)
            }
        }
        SandboxMode::Auto => {
            let native = match os {
                HostOs::Macos if probe.sandbox_exec() => Some(SandboxBackendChoice::Macos),
                HostOs::Linux if probe.bwrap() => Some(SandboxBackendChoice::Bwrap),
                HostOs::Linux if probe.linux_container_helper() => {
                    Some(SandboxBackendChoice::Landlock)
                }
                HostOs::Windows if probe.windows_container_helper() => {
                    Some(SandboxBackendChoice::AppContainer)
                }
                _ => None,
            };
            match native.or_else(|| probe.docker().then_some(SandboxBackendChoice::Docker)) {
                Some(choice) => SandboxDecision::Confine(choice),
                None if config.fail_closed => SandboxDecision::Refuse(SandboxUnavailable {
                    requested: "auto".to_string(),
                    reason: "sandbox.fail_closed=true and mode \"auto\" found no sandbox \
                             backend on this host"
                        .to_string(),
                    remediation: remediation_for(os),
                }),
                None => SandboxDecision::Unconfined(UnconfinedReason::AutoNoBackend),
            }
        }
    }
}

/// Fail-closed sandbox: [`Sandbox::wrap_command`] NEVER runs the requested
/// command — it substitutes one that prints the refusal to stderr and exits 1
/// (mirroring the Landlock backend's helper-missing refusal). Exec-shaped
/// tools short-circuit even earlier via [`Sandbox::refusal`] and return the
/// model-facing refusal text (`Display` — operator remediation stays in the
/// logs) without spawning anything.
pub struct RefusingSandbox {
    /// Why the configured sandbox cannot be honored on this host.
    pub error: SandboxUnavailable,
}

impl Sandbox for RefusingSandbox {
    fn refusal(&self) -> Option<&SandboxUnavailable> {
        Some(&self.error)
    }

    fn wrap_command(&self, _shell_command: &str, cwd: &Path) -> Command {
        let line = self.error.stderr_line();
        #[cfg(windows)]
        {
            // Space-free tokens are passed UNQUOTED by std's command-line
            // builder, so cmd.exe sees the redirect/&/exit metacharacters
            // unquoted and honors them. The original command is never passed
            // at all, so no quoting subtlety can resurrect it.
            let mut cmd = Command::new("cmd");
            cmd.arg("/C").arg("echo");
            for token in line.split_whitespace() {
                cmd.arg(token);
            }
            cmd.arg("1>&2").arg("&").arg("exit").arg("/b").arg("1");
            cmd.current_dir(cwd);
            cmd
        }
        #[cfg(not(windows))]
        {
            // `stderr_line` strips quotes and metacharacters, so the
            // single-quoted embed below cannot be escaped from.
            let mut cmd = Command::new("sh");
            cmd.arg("-c")
                .arg(format!("echo '{line}' >&2; exit 1"))
                .current_dir(cwd);
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
///
/// Resolution is [`decide_sandbox`] over the real host; this projects the
/// decision into a backend:
/// - `Confine` constructs the chosen backend.
/// - `Unconfined` yields [`NoSandbox`] — logged per reason (the Auto
///   degradation warns once per process; the explicit opt-outs stay quiet).
/// - `Refuse` yields [`RefusingSandbox`]: the signature stays infallible for
///   the many construction sites, but every command run under the result
///   refuses with the typed [`SandboxUnavailable`] instead of running
///   unconfined (fail closed).
pub fn create_sandbox(config: &SandboxConfig) -> Box<dyn Sandbox> {
    match decide_sandbox(config, HostOs::current(), &RealHostProbe) {
        SandboxDecision::Confine(choice) => build_backend(choice, config),
        SandboxDecision::Unconfined(reason) => {
            match reason {
                UnconfinedReason::Disabled => {
                    tracing::info!("sandbox disabled, shell commands run without isolation");
                }
                UnconfinedReason::ExplicitNone => {}
                UnconfinedReason::AutoNoBackend => warn_auto_unconfined_once(),
            }
            warn_fence_unenforced(config);
            Box::new(NoSandbox)
        }
        SandboxDecision::Refuse(error) => {
            // The OPERATOR-facing remediation is logged here (and only here /
            // doctor): `Display` is model-facing and deliberately omits it.
            tracing::error!(
                %error,
                remediation = %error.remediation,
                "sandbox unavailable; failing closed — commands will refuse to run"
            );
            Box::new(RefusingSandbox { error })
        }
    }
}

/// `Auto` degraded to no confinement: warn ONCE per process. Every sandbox
/// construction on this host repeats the same fact, and constructions happen
/// per session/registry — the old per-construction warning was log spam in
/// multi-session gateways, which is exactly how it came to be tuned out.
/// Never silent: `octos doctor` reports the same resolution on demand
/// ([`auto_sandbox_kind`]), and `sandbox.fail_closed` upgrades this
/// degradation to a refusal.
fn warn_auto_unconfined_once() {
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| {
        tracing::warn!(
            "no sandbox backend found (bwrap, Landlock/seccomp helper, sandbox-exec, \
             AppContainer helper, or docker): shell commands run WITHOUT isolation. \
             Install a backend (Linux: bubblewrap · macOS: sandbox-exec · Windows: the \
             octos-sandbox helper · any OS: Docker), set sandbox.fail_closed=true to \
             refuse instead of degrading, or set sandbox.enabled=false to accept \
             unconfined runs. Warned once per process; `octos doctor` shows the \
             resolved backend."
        );
    });
}

/// Which backend [`SandboxMode::Auto`] would select on this host — a stable
/// human-readable label plus whether that selection actually sandboxes
/// (`false` = [`NoSandbox`]). Runs the SAME resolution as [`create_sandbox`]
/// over a default config (on Linux the bwrap probe actually runs
/// `bwrap --version`), reported instead of instantiated. Used by
/// `octos doctor` so its sandbox row reflects the real runtime selection
/// rather than a PATH existence guess; the boolean keeps callers from
/// sniffing the label text for status.
pub fn auto_sandbox_kind() -> (&'static str, bool) {
    match decide_sandbox(&SandboxConfig::default(), HostOs::current(), &RealHostProbe) {
        SandboxDecision::Confine(choice) => (choice.label(), true),
        SandboxDecision::Unconfined(_) | SandboxDecision::Refuse(_) => {
            ("none — shell commands would run UNSANDBOXED", false)
        }
    }
}

/// Construct the decided backend. The Landlock/AppContainer types only exist
/// on their build targets; [`decide_sandbox`] never selects them off-target,
/// and the defensive off-target arms fail closed rather than panic if that
/// invariant is ever broken.
fn build_backend(choice: SandboxBackendChoice, config: &SandboxConfig) -> Box<dyn Sandbox> {
    match choice {
        SandboxBackendChoice::Macos => Box::new(MacosSandbox {
            allow_network: config.allow_network,
            read_allow_paths: config.read_allow_paths.clone(),
            workspace_write: config.workspace_write,
            repo_git_write: config.repo_git_write.clone(),
            // Outer-loop #4 (docs/build-cache-pool.md §7.2): the build-cache
            // slot rides its OWN grant on macOS, independent of the
            // workspace/fence/toolchain arms.
            build_cache_slot: config.build_cache_slot.clone(),
            // #1976: macOS EXPRESSES the fence (per-glob SBPL regex rules).
            write_allow_globs: config.write_allow_globs.clone(),
            toolchain_write_grants: configured_toolchain_grants(config),
        }),
        SandboxBackendChoice::Bwrap => Box::new(BwrapSandbox {
            allow_network: config.allow_network,
            workspace_write: fence_degraded_workspace_write(config, "bwrap"),
            repo_git_write: config.repo_git_write.clone(),
            // TODO(outer-loop #4 §7.2, known degradation): bwrap cannot yet
            // add a writable bind for an out-of-workspace slot dir, so
            // `build_cache_slot` is IGNORED here — a slot-pointing cargo
            // build fails under bwrap until a bind follow-up lands.
        }),
        SandboxBackendChoice::Docker => Box::new(DockerSandbox {
            config: fence_degraded_docker(config),
            allow_network: config.allow_network,
            // Per-call cache target mounts are supplied by the shell tool.
        }),
        SandboxBackendChoice::Landlock => {
            #[cfg(target_os = "linux")]
            {
                Box::new(LinuxContainerSandbox {
                    allow_network: config.allow_network,
                    read_allow_paths: config.read_allow_paths.clone(),
                    profile_name: config.profile_name.clone(),
                    workspace_write: fence_degraded_workspace_write(config, "landlock"),
                    // TODO(outer-loop #4 §7.2, known degradation):
                    // `build_cache_slot` is IGNORED here until the Landlock
                    // ruleset gains a per-path writable-access entry for the
                    // slot dir; a slot-pointing cargo build fails meanwhile.
                })
            }
            #[cfg(not(target_os = "linux"))]
            {
                build_backend_unbuildable("landlock")
            }
        }
        SandboxBackendChoice::AppContainer => {
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
                build_backend_unbuildable("appcontainer")
            }
        }
    }
}

/// Defensive fail-closed for a backend this build target cannot construct
/// (unreachable while [`decide_sandbox`] holds its OS invariants).
fn build_backend_unbuildable(requested: &str) -> Box<dyn Sandbox> {
    let error = SandboxUnavailable {
        requested: requested.to_string(),
        reason: format!("internal: the {requested} backend is not compiled into this build"),
        remediation: remediation_for(HostOs::current()),
    };
    tracing::error!(%error, "sandbox resolution invariant broken; failing closed");
    Box::new(RefusingSandbox { error })
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
            allow_toolchains: true,
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
            allow_toolchains: true,
            enabled: true,
            mode: SandboxMode::None,
            fail_closed: false,
            allow_network: false,
            workspace_write: true,
            repo_git_write: None,
            build_cache_slot: None,
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

    // #1976/#1987 — Bwrap/Docker/macOS backend command shapes are
    // host-specific; a Windows host has no bwrap/SBPL and its cwd
    // (`C:\\...`) fails the Docker path validator. Gate off Windows.
    #[cfg(not(target_os = "windows"))]
    #[test]
    fn fence_degrades_bwrap_to_ro_workspace() {
        // #1976 honest degradation: bwrap binds are CONCRETE paths — a glob
        // (or a create-target that does not exist yet) cannot be bind-mounted,
        // so a fenced workspace is bound READ-ONLY for the shell (fail
        // closed; granted paths stay writable via the fenced file tools).
        let config = SandboxConfig {
            allow_toolchains: true,
            workspace_write: true,
            write_allow_globs: Some(vec!["exemplar.card".to_string()]),
            ..SandboxConfig::default()
        };
        let sb = build_backend(SandboxBackendChoice::Bwrap, &config);
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

    // #1976/#1987 — Bwrap/Docker/macOS backend command shapes are
    // host-specific; a Windows host has no bwrap/SBPL and its cwd
    // (`C:\\...`) fails the Docker path validator. Gate off Windows.
    #[cfg(not(target_os = "windows"))]
    #[test]
    fn fence_degrades_docker_mount_to_ro() {
        // #1976 honest degradation: Docker mounts are concrete too — a
        // fenced workspace mounts `:ro` (fail closed for the shell).
        let config = SandboxConfig {
            allow_toolchains: true,
            write_allow_globs: Some(vec!["exemplar.card".to_string()]),
            ..SandboxConfig::default()
        };
        let sb = build_backend(SandboxBackendChoice::Docker, &config);
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

    // #1976/#1987 — Bwrap/Docker/macOS backend command shapes are
    // host-specific; a Windows host has no bwrap/SBPL and its cwd
    // (`C:\\...`) fails the Docker path validator. Gate off Windows.
    #[cfg(not(target_os = "windows"))]
    #[test]
    fn fence_reaches_macos_backend_as_globs() {
        // macOS is the one backend that EXPRESSES the fence (SBPL regex);
        // create_sandbox must thread the globs through, not degrade them.
        let config = SandboxConfig {
            allow_toolchains: true,
            write_allow_globs: Some(vec!["exemplar.card".to_string()]),
            ..SandboxConfig::default()
        };
        let sb = build_backend(SandboxBackendChoice::Macos, &config);
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
                allow_toolchains: true,
                enabled: false,
                ..SandboxConfig::default()
            },
            SandboxConfig {
                allow_toolchains: true,
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

    /// The detected toolchain write set must NEVER include the persistence
    /// vectors: `<cargo>/bin` is on PATH (a writable shim there outlives the
    /// sandbox) and `<rustup>/toolchains` holds the compiler binaries. And
    /// `allow_toolchains: false` must yield nothing at all.
    #[test]
    fn toolchain_grants_exclude_persistence_vectors_and_honor_config() {
        use std::path::Path;
        // Hermetic: exercise the pure builder against a KNOWN cargo home so
        // the test does not depend on the runner having ~/.cargo (Windows CI
        // does not) and uses component-based checks, not `/`-slash strings.
        let cargo = Path::new("/tmp/octos-test-cargo");

        // DEFAULT (network off): only the cargo lock is writable.
        let mut default_grants = ToolchainWriteGrants::default();
        push_cargo_grants(&mut default_grants, cargo, false);
        assert!(
            default_grants.subpaths.is_empty(),
            "no download-write set without network: {:?}",
            default_grants.subpaths
        );
        let lock = cargo.join(".package-cache");
        assert_eq!(
            default_grants.literals,
            vec![lock.to_string_lossy().into_owned()],
            "the only default grant is the cargo lock"
        );

        // NETWORK ON: the download-write set is added, but NEVER the
        // persistence vectors.
        let mut net_grants = ToolchainWriteGrants::default();
        push_cargo_grants(&mut net_grants, cargo, true);
        let net_paths: Vec<&str> = net_grants
            .literals
            .iter()
            .chain(net_grants.subpaths.iter())
            .map(String::as_str)
            .collect();
        assert!(
            net_paths
                .iter()
                .any(|p| Path::new(p).ends_with("registry/cache")),
            "network-on must allow crate downloads: {net_paths:?}"
        );
        for p in &net_paths {
            let path = Path::new(p);
            assert!(
                !path.ends_with(".cargo/bin")
                    && !path.components().any(|c| c.as_os_str() == "toolchains"),
                "persistence vector granted even under network: {p}"
            );
        }

        // allow_toolchains=false yields nothing regardless of environment.
        let off = configured_toolchain_grants(&SandboxConfig {
            allow_toolchains: false,
            ..SandboxConfig::default()
        });
        assert_eq!(off, ToolchainWriteGrants::default());
    }

    /// An old config JSON that predates `allow_toolchains` must deserialize
    /// with the pragmatic default (true) — serde default, not Rust default.
    #[test]
    fn allow_toolchains_defaults_true_for_old_configs() {
        let config: SandboxConfig = serde_json::from_str("{}").expect("empty config");
        assert!(config.allow_toolchains);
    }

    // -----------------------------------------------------------------------
    // Sandbox resolution decision matrix (pure — every OS tested from any
    // host). These lock the fail-closed contract: explicit modes never
    // silently degrade, Auto degrades loudly unless `fail_closed` refuses.
    // -----------------------------------------------------------------------

    /// A fixed backend-availability table standing in for the host probes.
    #[derive(Debug, Clone, Copy, Default)]
    struct FakeProbe {
        sandbox_exec: bool,
        bwrap: bool,
        linux_helper: bool,
        windows_helper: bool,
        docker: bool,
    }

    impl HostBackendProbe for FakeProbe {
        fn sandbox_exec(&self) -> bool {
            self.sandbox_exec
        }
        fn bwrap(&self) -> bool {
            self.bwrap
        }
        fn linux_container_helper(&self) -> bool {
            self.linux_helper
        }
        fn windows_container_helper(&self) -> bool {
            self.windows_helper
        }
        fn docker(&self) -> bool {
            self.docker
        }
    }

    const NO_BACKENDS: FakeProbe = FakeProbe {
        sandbox_exec: false,
        bwrap: false,
        linux_helper: false,
        windows_helper: false,
        docker: false,
    };

    const ALL_BACKENDS: FakeProbe = FakeProbe {
        sandbox_exec: true,
        bwrap: true,
        linux_helper: true,
        windows_helper: true,
        docker: true,
    };

    const ALL_OSES: [HostOs; 4] = [HostOs::Linux, HostOs::Macos, HostOs::Windows, HostOs::Other];

    fn auto_config() -> SandboxConfig {
        SandboxConfig::default()
    }

    fn mode_config(mode: SandboxMode) -> SandboxConfig {
        SandboxConfig {
            mode,
            ..SandboxConfig::default()
        }
    }

    #[test]
    fn should_confine_via_native_backend_when_auto_matches_host_os() {
        // Auto prefers the native backend over Docker on every OS; selection
        // order on Linux is bwrap first, then the Landlock helper.
        let cases = [
            (HostOs::Macos, ALL_BACKENDS, SandboxBackendChoice::Macos),
            (HostOs::Linux, ALL_BACKENDS, SandboxBackendChoice::Bwrap),
            (
                HostOs::Linux,
                FakeProbe {
                    bwrap: false,
                    ..ALL_BACKENDS
                },
                SandboxBackendChoice::Landlock,
            ),
            (
                HostOs::Windows,
                ALL_BACKENDS,
                SandboxBackendChoice::AppContainer,
            ),
            (HostOs::Other, ALL_BACKENDS, SandboxBackendChoice::Docker),
        ];
        for (os, probe, expected) in cases {
            assert_eq!(
                decide_sandbox(&auto_config(), os, &probe),
                SandboxDecision::Confine(expected),
                "auto on {os:?} with {probe:?}"
            );
        }
    }

    #[test]
    fn should_fall_back_to_docker_when_auto_has_no_native_backend() {
        for os in ALL_OSES {
            let probe = FakeProbe {
                docker: true,
                ..NO_BACKENDS
            };
            assert_eq!(
                decide_sandbox(&auto_config(), os, &probe),
                SandboxDecision::Confine(SandboxBackendChoice::Docker),
                "auto on {os:?} must fall back to docker"
            );
        }
    }

    #[test]
    fn should_degrade_loudly_not_refuse_when_auto_finds_no_backend() {
        // The compat constraint: a default config (Auto, fail_closed unset) on
        // a backend-less host — Windows without the AppContainer helper or
        // Docker being the canonical case — keeps WORKING unconfined. The
        // degradation is a warned Unconfined resolution, never a refusal.
        for os in ALL_OSES {
            assert_eq!(
                decide_sandbox(&auto_config(), os, &NO_BACKENDS),
                SandboxDecision::Unconfined(UnconfinedReason::AutoNoBackend),
                "default config on a backend-less {os:?} host must stay unconfined"
            );
        }
    }

    #[test]
    fn should_refuse_when_auto_finds_no_backend_and_fail_closed_set() {
        for os in ALL_OSES {
            let config = SandboxConfig {
                fail_closed: true,
                ..SandboxConfig::default()
            };
            match decide_sandbox(&config, os, &NO_BACKENDS) {
                SandboxDecision::Refuse(error) => {
                    assert_eq!(error.requested, "auto", "refusal names the mode ({os:?})");
                    assert!(
                        error.reason.contains("fail_closed"),
                        "refusal explains the knob that turned degradation into refusal \
                         ({os:?}): {}",
                        error.reason
                    );
                }
                other => panic!("auto+fail_closed on backend-less {os:?} must refuse: {other:?}"),
            }
        }
    }

    #[test]
    fn should_confine_when_auto_finds_backend_even_with_fail_closed() {
        // Mutation-proofing the knob's other edge: fail_closed must NOT
        // over-refuse — a host with any working backend confines normally.
        let config = SandboxConfig {
            fail_closed: true,
            ..SandboxConfig::default()
        };
        let probe = FakeProbe {
            docker: true,
            ..NO_BACKENDS
        };
        for os in ALL_OSES {
            assert_eq!(
                decide_sandbox(&config, os, &probe),
                SandboxDecision::Confine(SandboxBackendChoice::Docker),
                "fail_closed must not refuse when a backend exists ({os:?})"
            );
        }
    }

    #[test]
    fn should_refuse_when_explicit_mode_is_on_wrong_os() {
        // An explicit backend mode on the wrong OS REFUSES — the pre-existing
        // behaviours were both wrong: landlock/appcontainer silently degraded
        // to NO confinement, and bwrap/macos constructed a backend whose every
        // spawn died with a bare ENOENT.
        let cases = [
            (SandboxMode::Bwrap, "bwrap", HostOs::Macos),
            (SandboxMode::Bwrap, "bwrap", HostOs::Windows),
            (SandboxMode::Landlock, "landlock", HostOs::Macos),
            (SandboxMode::Landlock, "landlock", HostOs::Windows),
            (SandboxMode::Macos, "macos", HostOs::Linux),
            (SandboxMode::Macos, "macos", HostOs::Windows),
            (SandboxMode::AppContainer, "appcontainer", HostOs::Linux),
            (SandboxMode::AppContainer, "appcontainer", HostOs::Macos),
        ];
        for (mode, label, os) in cases {
            match decide_sandbox(&mode_config(mode.clone()), os, &ALL_BACKENDS) {
                SandboxDecision::Refuse(error) => {
                    assert_eq!(error.requested, label, "refusal names the mode");
                    assert!(
                        error.reason.contains(os.label()),
                        "refusal names the mismatching host OS: {}",
                        error.reason
                    );
                }
                other => panic!("explicit {mode:?} on {os:?} must refuse, got {other:?}"),
            }
        }
    }

    #[test]
    fn should_refuse_when_explicit_mode_backend_is_missing_on_right_os() {
        // Right OS, but the backend is not actually available: refuse with the
        // typed error instead of constructing a backend that ENOENTs per spawn
        // (bwrap/macos/docker) or degrades at wrap time (landlock/appcontainer).
        let cases = [
            (SandboxMode::Bwrap, "bwrap", HostOs::Linux),
            (SandboxMode::Landlock, "landlock", HostOs::Linux),
            (SandboxMode::Macos, "macos", HostOs::Macos),
            (SandboxMode::AppContainer, "appcontainer", HostOs::Windows),
            (SandboxMode::Docker, "docker", HostOs::Linux),
            (SandboxMode::Docker, "docker", HostOs::Macos),
            (SandboxMode::Docker, "docker", HostOs::Windows),
        ];
        for (mode, label, os) in cases {
            match decide_sandbox(&mode_config(mode.clone()), os, &NO_BACKENDS) {
                SandboxDecision::Refuse(error) => {
                    assert_eq!(error.requested, label, "refusal names the mode");
                }
                other => {
                    panic!(
                        "explicit {mode:?} with backend missing on {os:?} must refuse: {other:?}"
                    )
                }
            }
        }
    }

    #[test]
    fn should_confine_when_explicit_mode_matches_available_backend() {
        let cases = [
            (
                SandboxMode::Bwrap,
                HostOs::Linux,
                SandboxBackendChoice::Bwrap,
            ),
            (
                SandboxMode::Landlock,
                HostOs::Linux,
                SandboxBackendChoice::Landlock,
            ),
            (
                SandboxMode::Macos,
                HostOs::Macos,
                SandboxBackendChoice::Macos,
            ),
            (
                SandboxMode::AppContainer,
                HostOs::Windows,
                SandboxBackendChoice::AppContainer,
            ),
            (
                SandboxMode::Docker,
                HostOs::Linux,
                SandboxBackendChoice::Docker,
            ),
            (
                SandboxMode::Docker,
                HostOs::Other,
                SandboxBackendChoice::Docker,
            ),
        ];
        for (mode, os, expected) in cases {
            assert_eq!(
                decide_sandbox(&mode_config(mode.clone()), os, &ALL_BACKENDS),
                SandboxDecision::Confine(expected),
                "explicit {mode:?} on {os:?}"
            );
        }
    }

    #[test]
    fn should_stay_unconfined_when_opted_out_even_with_fail_closed() {
        // The explicit opt-outs beat fail_closed: `enabled=false` (which
        // --danger-full-access sets via apply_to_sandbox) and `mode="none"`.
        let disabled = SandboxConfig {
            enabled: false,
            fail_closed: true,
            ..SandboxConfig::default()
        };
        let none = SandboxConfig {
            mode: SandboxMode::None,
            fail_closed: true,
            ..SandboxConfig::default()
        };
        for os in ALL_OSES {
            assert_eq!(
                decide_sandbox(&disabled, os, &NO_BACKENDS),
                SandboxDecision::Unconfined(UnconfinedReason::Disabled),
                "enabled=false beats fail_closed ({os:?})"
            );
            assert_eq!(
                decide_sandbox(&none, os, &NO_BACKENDS),
                SandboxDecision::Unconfined(UnconfinedReason::ExplicitNone),
                "mode=none beats fail_closed ({os:?})"
            );
        }
        // --danger-full-access flows through apply_to_sandbox to the same
        // Disabled opt-out even when the inherited config set fail_closed.
        let dangerous = crate::policy::EffectivePermissions::danger_full_access().apply_to_sandbox(
            &SandboxConfig {
                fail_closed: true,
                ..SandboxConfig::default()
            },
        );
        assert_eq!(
            decide_sandbox(&dangerous, HostOs::Windows, &NO_BACKENDS),
            SandboxDecision::Unconfined(UnconfinedReason::Disabled),
            "danger-full-access must stay an unconfined opt-out under fail_closed"
        );
    }

    #[test]
    fn should_default_fail_closed_to_false_for_legacy_configs() {
        // Every pre-existing config parses with the compatible default; the
        // knob only ever takes effect by explicit opt-in.
        let legacy: SandboxConfig = serde_json::from_str("{}").expect("empty config");
        assert!(!legacy.fail_closed, "serde default must be false");
        assert!(
            !SandboxConfig::default().fail_closed,
            "Rust default must be false"
        );
        let opted: SandboxConfig =
            serde_json::from_str(r#"{"fail_closed": true}"#).expect("explicit opt-in");
        assert!(opted.fail_closed, "explicit true must parse");
    }

    #[test]
    fn should_produce_refusing_sandbox_from_create_sandbox_when_mode_unhonorable() {
        // End-to-end through the real constructor: pick a mode that is
        // guaranteed unhonorable on the RUNNING host (landlock off-Linux,
        // macos seatbelt off-macOS), and require the fail-closed backend —
        // not a silent NoSandbox, not a blind ENOENT backend.
        let mode = if cfg!(target_os = "macos") || cfg!(windows) {
            SandboxMode::Landlock
        } else {
            SandboxMode::Macos
        };
        let sb = create_sandbox(&mode_config(mode));
        let refusal = sb
            .refusal()
            .expect("an unhonorable explicit mode must resolve to a refusing sandbox");
        assert!(
            refusal.remediation.contains("sandbox.enabled=false"),
            "the OPERATOR-facing remediation field names the explicit opt-out: {}",
            refusal.remediation
        );
        let model_text = refusal.to_string();
        assert!(
            !model_text.contains("enabled=false") && !model_text.contains("mode=\"none\""),
            "the model-facing Display must not name the disable keys: {model_text}"
        );
        assert!(
            !sb.is_noop(),
            "a refusing sandbox is not a no-op passthrough — nothing runs at all"
        );
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn refusing_sandbox_command_refuses_and_never_runs_the_original() {
        // The wrap-level fail-closed guarantee for consumers that do not check
        // `refusal()`: the produced command exits non-zero, prints the refusal
        // to stderr, and never contains (or executes) the original command.
        let sb = RefusingSandbox {
            error: SandboxUnavailable {
                requested: "bwrap".to_string(),
                reason: "sandbox.mode=\"bwrap\" requires Linux; this host is macOS".to_string(),
                remediation: remediation_for(HostOs::Macos),
            },
        };
        let marker = "echo sandbox-escape-proof-marker";
        let mut cmd = sb.wrap_command(marker, &std::env::temp_dir());
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(
            args.iter().all(|arg| !arg.contains("escape-proof-marker")),
            "the original command must never be passed through: {args:?}"
        );
        let output = cmd.output().await.expect("refusal command spawns");
        assert!(
            !output.status.success(),
            "refusal command must exit non-zero"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("sandbox unavailable"),
            "refusal reaches stderr: {stderr}"
        );
        assert!(
            !stderr.contains("sandbox-escape-proof-marker"),
            "the original command must not run: {stderr}"
        );
    }

    #[test]
    fn should_never_refuse_a_default_config_on_any_host() {
        // Byte-identical compat for default configs: whatever backend (or
        // NoSandbox degradation) Auto resolves to on the running host, it is
        // never a refusal with fail_closed unset.
        let sb = create_sandbox(&SandboxConfig::default());
        assert!(
            sb.refusal().is_none(),
            "a default config must never fail closed"
        );
    }

    #[test]
    fn refusal_text_names_per_os_remediations_and_avoids_denial_phrases() {
        // The message is model-readable remediation, per OS…
        let windows = remediation_for(HostOs::Windows);
        assert!(
            windows.contains("octos-sandbox.exe") && windows.contains("Docker"),
            "windows remediation names the AppContainer helper and Docker: {windows}"
        );
        let linux = remediation_for(HostOs::Linux);
        assert!(
            linux.contains("bubblewrap") && linux.contains("Docker"),
            "linux remediation names bubblewrap and Docker: {linux}"
        );
        let macos = remediation_for(HostOs::Macos);
        assert!(
            macos.contains("sandbox-exec"),
            "macos remediation names sandbox-exec: {macos}"
        );
        for os in ALL_OSES {
            let text = remediation_for(os);
            assert!(
                text.contains("sandbox.enabled=false") && text.contains("\"none\""),
                "every remediation names the explicit opt-outs: {text}"
            );
            // …and must never contain a kernel denial phrase, or the
            // sandbox_denial_hint scanner would append a misleading
            // "the OS sandbox blocked a file access" hint to a refusal.
            for phrase in DENIAL_PHRASES {
                assert!(
                    !text.contains(phrase),
                    "refusal text must not trip the denial scanner: {phrase}"
                );
            }
        }
    }

    #[test]
    fn confining_backends_never_report_noop_or_refusal() {
        // #2196 review MUST-FIX invariant: `is_noop()` is a CONSTRUCTION-TIME
        // property. A backend built from a `Confine` decision must never
        // (dynamically or otherwise) report no-op — `is_noop() == true` is
        // the exact transition validators.rs / tools/check.rs use to run
        // argv DIRECTLY on the host, so a confining backend that flips to
        // no-op converts fail-closed paths into raw host execution. (The
        // Windows AppContainer half of this — whose old override re-probed
        // the helper per call — is cfg(windows) and asserted in
        // sandbox/windows.rs; this locks the buildable-anywhere backends.)
        let config = SandboxConfig::default();
        for choice in [
            SandboxBackendChoice::Macos,
            SandboxBackendChoice::Bwrap,
            SandboxBackendChoice::Docker,
        ] {
            let sb = build_backend(choice, &config);
            assert!(
                !sb.is_noop(),
                "confining backend {choice:?} must never report no-op"
            );
            assert!(
                sb.refusal().is_none(),
                "confining backend {choice:?} must not carry a refusal"
            );
        }
        // The inverse stays true: NoSandbox is the one honest no-op.
        assert!(NoSandbox.is_noop());
    }

    #[test]
    fn refusal_display_never_names_the_disable_keys() {
        // Codex MUST-FIX (#2196 review): the Display text flows VERBATIM into
        // model-visible tool results (shell/exec refusal guards, wrap-time
        // stderr, mcp-serve session errors, fleet termination reasons). Text
        // that names the config keys that remove confinement is itself an
        // escape vector -- a confined model can still edit config files. The
        // operator-facing remediation (which legitimately names the explicit
        // opt-outs) lives in the `remediation` FIELD, surfaced only via the
        // creation-time error log and doctor-adjacent surfaces.
        let mut displays: Vec<String> = Vec::new();
        for os in ALL_OSES {
            // Explicit-mode refusals (wrong OS / missing backend) ...
            for mode in [
                SandboxMode::Bwrap,
                SandboxMode::Landlock,
                SandboxMode::Macos,
                SandboxMode::AppContainer,
                SandboxMode::Docker,
            ] {
                if let SandboxDecision::Refuse(error) =
                    decide_sandbox(&mode_config(mode.clone()), os, &NO_BACKENDS)
                {
                    displays.push(error.to_string());
                }
            }
            // ... and the auto+fail_closed refusal.
            let config = SandboxConfig {
                fail_closed: true,
                ..SandboxConfig::default()
            };
            if let SandboxDecision::Refuse(error) = decide_sandbox(&config, os, &NO_BACKENDS) {
                displays.push(error.to_string());
            }
        }
        assert!(
            displays.len() >= ALL_OSES.len(),
            "matrix must produce refusals to inspect"
        );
        for text in &displays {
            for banned in [
                "enabled=false",
                "mode=\"none\"",
                "mode = \"none\"",
                "danger-full-access",
            ] {
                assert!(
                    !text.contains(banned),
                    "model-visible refusal must not name the disable keys ({banned:?}): {text}"
                );
            }
            for phrase in DENIAL_PHRASES {
                assert!(
                    !text.contains(phrase),
                    "model-visible refusal must not trip the denial scanner: {phrase}"
                );
            }
            assert!(
                text.contains("operator"),
                "model-visible refusal points at the operator, not at config keys: {text}"
            );
        }
    }

    #[test]
    fn refusal_stderr_line_is_shell_inert() {
        let error = SandboxUnavailable {
            requested: "auto".to_string(),
            reason: "tricky 'quote' \"double\" & meta | chars ; $(sub) `tick`".to_string(),
            remediation: remediation_for(HostOs::Windows),
        };
        let line = error.stderr_line();
        for banned in [
            '\'', '"', '&', '|', '<', '>', '^', '%', '(', ')', ';', '$', '`',
        ] {
            assert!(
                !line.contains(banned),
                "stderr line must stay shell-inert, found {banned:?} in: {line}"
            );
        }
        assert!(
            line.contains("sandbox unavailable"),
            "sanitizing keeps the message recognizable: {line}"
        );
    }
}
