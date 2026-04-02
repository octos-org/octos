//! octos-sandbox: AppContainer helper binary for Windows.
//!
//! Creates/reuses an AppContainer profile and launches a command inside it
//! with restricted filesystem and network access.
//!
//! Usage:
//!   octos-sandbox --profile octos.dspfac --cwd C:\work \
//!     --allow-read C:\tools --allow-network \
//!     -- cmd /C "echo hello"
//!
//! On non-Windows platforms, this binary is a no-op passthrough.

use clap::Parser;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser, Debug)]
#[command(name = "octos-sandbox", about = "AppContainer sandbox helper")]
struct Args {
    /// AppContainer profile name (e.g. "octos.dspfac").
    #[arg(long)]
    profile: String,

    /// Working directory (granted read-write access).
    #[arg(long)]
    cwd: PathBuf,

    /// Paths to grant read-only access to (repeatable).
    #[arg(long = "allow-read")]
    allow_read: Vec<PathBuf>,

    /// Allow network access inside the sandbox.
    #[arg(long)]
    allow_network: bool,

    /// Command and arguments to run inside the sandbox.
    #[arg(last = true, required = true)]
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

    #[cfg(not(windows))]
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

#[cfg(not(windows))]
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

#[cfg(windows)]
fn run_sandboxed(args: &Args) -> eyre::Result<u8> {
    use eyre::WrapErr;
    use rappct::acl::{self, AccessMask, ResourcePath};
    use rappct::launch::{JobLimits, LaunchOptions, StdioConfig};
    use rappct::{AppContainerProfile, SecurityCapabilitiesBuilder, launch_in_container};

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
        caps_builder =
            caps_builder.with_known(&[rappct::KnownCapability::InternetClient]);
    }
    let caps = caps_builder.build().wrap_err("failed to build security capabilities")?;

    // 5. Build command line
    let cmdline = args.command.join(" ");

    // 6. Launch
    let opts = LaunchOptions {
        exe: args
            .command
            .first()
            .cloned()
            .unwrap_or_else(|| "cmd.exe".into()),
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

    let mut child =
        launch_in_container(&caps, &opts).wrap_err("failed to launch sandboxed process")?;

    // 7. Wait for completion
    let exit_code = child.wait().wrap_err("failed to wait for sandboxed process")?;

    Ok(exit_code as u8)
}
