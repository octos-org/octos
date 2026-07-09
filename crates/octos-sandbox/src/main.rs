//! octos-sandbox: platform sandbox helper binary.
//!
//! On Windows, creates/reuses an AppContainer profile and launches a command
//! inside it with restricted filesystem and network access. On Linux, applies
//! a Landlock filesystem policy plus a seccomp denylist before execing the
//! command.
//!
//! Usage:
//!   octos-sandbox --profile octos.dspfac --cwd C:\work \
//!     --allow-read C:\tools --allow-network \
//!     -- cmd /C "echo hello"
//!
//! On other platforms, this binary is a no-op passthrough.

use clap::Parser;
use std::path::PathBuf;
use std::process::ExitCode;

#[cfg(target_os = "linux")]
const BLOCKED_ENV_VARS: &[&str] = &[
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
    "LD_AUDIT",
    "DYLD_INSERT_LIBRARIES",
    "DYLD_LIBRARY_PATH",
    "DYLD_FRAMEWORK_PATH",
    "DYLD_FALLBACK_LIBRARY_PATH",
    "DYLD_VERSIONED_LIBRARY_PATH",
    "NODE_OPTIONS",
    "PYTHONSTARTUP",
    "PYTHONPATH",
    "PERL5OPT",
    "RUBYOPT",
    "RUBYLIB",
    "JAVA_TOOL_OPTIONS",
    "BASH_ENV",
    "ENV",
    "ZDOTDIR",
];

#[derive(Parser, Debug)]
#[command(name = "octos-sandbox", about = "platform sandbox helper")]
struct Args {
    /// AppContainer profile name (e.g. "octos.dspfac").
    #[arg(long, default_value = "octos.default")]
    profile: String,

    /// Working directory (granted read-write access).
    #[arg(long, default_value = ".")]
    cwd: PathBuf,

    /// Paths to grant read-only access to (repeatable).
    #[arg(long = "allow-read")]
    allow_read: Vec<PathBuf>,

    /// Allow network access inside the sandbox.
    #[arg(long)]
    allow_network: bool,

    /// Probe whether the Linux Landlock/seccomp sandbox can be enforced.
    #[arg(long)]
    probe_linux: bool,

    /// Command and arguments to run inside the sandbox.
    #[arg(last = true)]
    command: Vec<String>,
}

fn main() -> ExitCode {
    let args = Args::parse();

    #[cfg(windows)]
    {
        match run_sandboxed(&args) {
            Ok(code) => ExitCode::from(code),
            Err(e) => {
                eprintln!("octos-sandbox error: {e}");
                ExitCode::from(1)
            }
        }
    }

    #[cfg(target_os = "linux")]
    {
        if args.probe_linux {
            return match probe_linux() {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("octos-sandbox probe error: {e}");
                    ExitCode::from(1)
                }
            };
        }

        match run_linux_sandboxed(&args) {
            Ok(code) => ExitCode::from(code),
            Err(e) => {
                eprintln!("octos-sandbox error: {e}");
                ExitCode::from(1)
            }
        }
    }

    #[cfg(all(not(windows), not(target_os = "linux")))]
    {
        // Non-Windows: passthrough — just exec the command directly
        match run_passthrough(&args) {
            Ok(code) => ExitCode::from(code),
            Err(e) => {
                eprintln!("octos-sandbox error: {e}");
                ExitCode::from(1)
            }
        }
    }
}

#[cfg(all(not(windows), not(target_os = "linux")))]
fn run_passthrough(args: &Args) -> eyre::Result<u8> {
    use std::process::Command;

    let (prog, cmd_args) = args
        .command
        .split_first()
        .ok_or_else(|| eyre::eyre!("no command specified"))?;

    let status = Command::new(prog)
        .args(cmd_args)
        .current_dir(&args.cwd)
        .status()?;

    Ok(status.code().unwrap_or(1) as u8)
}

#[cfg(target_os = "linux")]
fn probe_linux() -> eyre::Result<()> {
    apply_linux_landlock(&std::env::current_dir()?, &[])?;
    apply_linux_seccomp(false)?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn run_linux_sandboxed(args: &Args) -> eyre::Result<u8> {
    use eyre::WrapErr;
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    let (prog, cmd_args) = args
        .command
        .split_first()
        .ok_or_else(|| eyre::eyre!("no command specified"))?;

    let mut command = Command::new(prog);
    command.args(cmd_args).current_dir(&args.cwd);
    for var in BLOCKED_ENV_VARS {
        command.env_remove(var);
    }

    apply_linux_landlock(&args.cwd, &args.allow_read)
        .wrap_err("failed to apply Landlock policy")?;
    apply_linux_seccomp(args.allow_network).wrap_err("failed to apply seccomp policy")?;

    Err(command.exec().into())
}

#[cfg(target_os = "linux")]
fn apply_linux_landlock(cwd: &std::path::Path, extra_read_paths: &[PathBuf]) -> eyre::Result<()> {
    use eyre::{WrapErr, bail};
    use landlock::{
        ABI, Access, AccessFs, Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetStatus,
        make_bitflags, path_beneath_rules,
    };

    const SYSTEM_READ_PATHS: &[&str] = &[
        "/usr",
        "/bin",
        "/sbin",
        "/lib",
        "/lib64",
        "/etc",
        "/dev/urandom",
        "/dev/random",
    ];
    const SYSTEM_EXEC_PATHS: &[&str] = &[
        "/usr/bin",
        "/bin",
        // Dynamically linked binaries need execute access to their ELF
        // interpreter, which commonly lives below one of these directories.
        "/usr/lib",
        "/usr/lib64",
        "/lib",
        "/lib64",
    ];
    const SYSTEM_WRITE_PATHS: &[&str] = &["/tmp", "/var/tmp", "/dev/null"];

    let abi = ABI::V1;
    let read_access = make_bitflags!(AccessFs::{ReadFile | ReadDir});
    let exec_access = read_access | AccessFs::Execute;
    let write_access = read_access | AccessFs::from_write(abi);

    let cwd = cwd
        .canonicalize()
        .wrap_err_with(|| format!("failed to canonicalize cwd {}", cwd.display()))?;

    let mut created = Ruleset::default()
        .handle_access(AccessFs::from_all(abi))?
        .create()?
        .add_rules(path_beneath_rules(SYSTEM_READ_PATHS, read_access))?
        .add_rules(path_beneath_rules(SYSTEM_EXEC_PATHS, exec_access))?
        .add_rules(path_beneath_rules(SYSTEM_WRITE_PATHS, write_access))?
        .add_rules(path_beneath_rules([cwd.as_path()], write_access))?;

    for path in extra_read_paths {
        created = created.add_rules(path_beneath_rules([path.as_path()], read_access))?;
    }

    let status = created.restrict_self()?;
    if status.ruleset != RulesetStatus::FullyEnforced {
        bail!(
            "Landlock ruleset was not fully enforced: {:?}",
            status.ruleset
        );
    }

    Ok(())
}

#[cfg(target_os = "linux")]
fn apply_linux_seccomp(allow_network: bool) -> eyre::Result<()> {
    use eyre::WrapErr;
    use seccompiler::{
        BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
        SeccompRule,
    };
    use std::collections::BTreeMap;
    use std::convert::TryInto;

    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = blocked_syscalls()
        .into_iter()
        .map(|syscall| (syscall, Vec::new()))
        .collect();

    if !allow_network {
        let internet_socket_rules = vec![
            SeccompRule::new(vec![SeccompCondition::new(
                0,
                SeccompCmpArgLen::Dword,
                SeccompCmpOp::Eq,
                libc::AF_INET as u64,
            )?])?,
            SeccompRule::new(vec![SeccompCondition::new(
                0,
                SeccompCmpArgLen::Dword,
                SeccompCmpOp::Eq,
                libc::AF_INET6 as u64,
            )?])?,
        ];
        rules.insert(libc::SYS_socket, internet_socket_rules);
    }

    let filter: BpfProgram = SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EPERM as u32),
        std::env::consts::ARCH.try_into()?,
    )?
    .try_into()?;

    seccompiler::apply_filter(&filter).wrap_err("seccomp filter installation failed")?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn blocked_syscalls() -> Vec<i64> {
    vec![
        libc::SYS_bpf,
        libc::SYS_finit_module,
        libc::SYS_init_module,
        libc::SYS_kexec_load,
        libc::SYS_mount,
        libc::SYS_open_by_handle_at,
        libc::SYS_perf_event_open,
        libc::SYS_pivot_root,
        libc::SYS_ptrace,
        libc::SYS_reboot,
        libc::SYS_umount2,
        libc::SYS_userfaultfd,
    ]
}

#[cfg(windows)]
fn run_sandboxed(args: &Args) -> eyre::Result<u8> {
    use eyre::WrapErr;
    use rappct::acl::{self, AccessMask, ResourcePath};
    use rappct::launch::{JobLimits, LaunchOptions, StdioConfig, launch_in_container_with_io};
    use rappct::{AppContainerProfile, SecurityCapabilitiesBuilder};

    // 1. Create or reuse the AppContainer profile
    let profile = AppContainerProfile::ensure(
        &args.profile,
        &format!("octos-sandbox-{}", &args.profile),
        Some("octos agent sandbox"),
    )
    .wrap_err("failed to create AppContainer profile")?;

    // 2. Grant read-write access to working directory
    if args.cwd.exists() {
        acl::grant_to_package(
            ResourcePath::Directory(args.cwd.clone()),
            &profile.sid,
            AccessMask(AccessMask::FILE_GENERIC_READ.0 | AccessMask::FILE_GENERIC_WRITE.0),
        )
        .wrap_err_with(|| format!("failed to grant rw to {}", args.cwd.display()))?;
    }

    // 3. Grant read-only access to additional paths
    for path in &args.allow_read {
        if path.exists() {
            acl::grant_to_package(
                ResourcePath::Directory(path.clone()),
                &profile.sid,
                AccessMask::FILE_GENERIC_READ,
            )
            .wrap_err_with(|| format!("failed to grant ro to {}", path.display()))?;
        }
    }

    // 4. Build capabilities
    let mut caps_builder = SecurityCapabilitiesBuilder::new(&profile.sid);
    if args.allow_network {
        caps_builder = caps_builder.with_known(&[rappct::KnownCapability::InternetClient]);
    }
    let caps = caps_builder
        .build()
        .wrap_err("failed to build security capabilities")?;

    // 5. Build command line
    let cmdline = args.command.join(" ");
    let exe: PathBuf = args
        .command
        .first()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("C:\\Windows\\System32\\cmd.exe"));

    // 6. Launch with IO (needed for wait())
    let opts = LaunchOptions {
        exe,
        cmdline: Some(cmdline),
        cwd: Some(args.cwd.clone()),
        stdio: StdioConfig::Inherit,
        join_job: Some(JobLimits {
            memory_bytes: Some(512 * 1024 * 1024), // 512MB limit
            cpu_rate_percent: None,
            kill_on_job_close: true,
        }),
        ..Default::default()
    };

    let child =
        launch_in_container_with_io(&caps, &opts).wrap_err("failed to launch sandboxed process")?;

    // 7. Wait for completion (no timeout — let the caller handle that)
    let exit_code = child
        .wait(None)
        .wrap_err("failed to wait for sandboxed process")?;

    Ok(exit_code as u8)
}
