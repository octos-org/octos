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
#[cfg(any(target_os = "linux", test))]
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitCode;

// The write-grant planning helpers below drive Landlock (Linux) enforcement.
// They are gated to Linux + test builds so the macOS/other passthrough build
// (which never sandboxes) does not warn about unused items.

/// System directories that a sandboxed process may write to for scratch/temp
/// use. Landlock and other backends grant write access beneath these. NOTE:
/// Landlock rules are ADDITIVE — you cannot subtract a subtree — so when the
/// workspace itself lives under one of these (e.g. cwd = `/tmp/work`), granting
/// broad write to `/tmp` would re-permit writes to the read-only workspace.
/// `plan_system_write_paths` handles that case (codex P1).
#[cfg(any(target_os = "linux", test))]
const SYSTEM_WRITE_PATHS: &[&str] = &["/tmp", "/var/tmp", "/dev/null"];

/// Candidate roots for a dedicated read-only scratch dir, tried in order. The
/// first one that does NOT contain the workspace cwd is used.
#[cfg(any(target_os = "linux", test))]
const SCRATCH_ROOTS: &[&str] = &["/tmp", "/var/tmp"];

/// Plan which system write paths to grant, and whether a dedicated scratch dir
/// is needed, given the (canonicalized) workspace `cwd` and whether the cwd is
/// writable.
#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, PartialEq, Eq)]
struct WriteGrantPlan {
    /// System paths (subset of `SYSTEM_WRITE_PATHS`) to grant write access to.
    /// A path that CONTAINS a read-only cwd is excluded so it cannot re-permit
    /// workspace writes.
    system_write_paths: Vec<PathBuf>,
    /// A dedicated scratch dir, guaranteed OUTSIDE the workspace, to grant write
    /// to when the workspace is read-only and a broad temp path was dropped.
    /// `None` when the cwd is writable (the workspace itself absorbs tmp writes)
    /// or when no broad temp path had to be dropped.
    scratch_dir: Option<PathBuf>,
}

/// Returns true when `dir` is an ancestor of (or equal to) `cwd` — i.e.
/// granting broad write to `dir` would cover the workspace.
#[cfg(any(target_os = "linux", test))]
fn path_contains(dir: &Path, cwd: &Path) -> bool {
    cwd.starts_with(dir)
}

/// Compute the write-grant plan (codex P1). When `cwd_write` is false and the
/// workspace lives under a broad system temp path (`/tmp`, `/var/tmp`), that
/// path's write grant is dropped (it would otherwise re-permit `touch
/// $cwd/newfile`) and a dedicated scratch dir OUTSIDE the workspace is chosen
/// instead so legitimate temp writes still succeed. `/dev/null` (a file) is
/// always kept. When `cwd_write` is true, behaviour is unchanged: all system
/// write paths are granted and no scratch dir is needed.
#[cfg(any(target_os = "linux", test))]
fn plan_system_write_paths(cwd: &Path, cwd_write: bool) -> WriteGrantPlan {
    if cwd_write {
        return WriteGrantPlan {
            system_write_paths: SYSTEM_WRITE_PATHS.iter().map(PathBuf::from).collect(),
            scratch_dir: None,
        };
    }

    let mut dropped_any = false;
    let mut system_write_paths = Vec::new();
    for &p in SYSTEM_WRITE_PATHS {
        let path = Path::new(p);
        // Drop directory grants that contain the read-only cwd. `/dev/null` is a
        // file and never contains cwd, so it is always kept.
        if path_contains(path, cwd) {
            dropped_any = true;
        } else {
            system_write_paths.push(PathBuf::from(p));
        }
    }

    // If we dropped a broad temp grant, provide a dedicated scratch dir outside
    // the workspace so tools needing tmp space still work.
    let scratch_dir = if dropped_any {
        let unique = format!("octos-sandbox-ro.{}", std::process::id());
        SCRATCH_ROOTS
            .iter()
            .map(|root| Path::new(root).join(&unique))
            .find(|candidate| !candidate.starts_with(cwd))
    } else {
        None
    };

    WriteGrantPlan {
        system_write_paths,
        scratch_dir,
    }
}

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

    /// Working directory. Granted read-write access by default; granted
    /// read-only access when `--readonly-cwd` is set (used by
    /// `--sandbox read-only`, which must stop shell writes to the workspace).
    #[arg(long, default_value = ".")]
    cwd: PathBuf,

    /// Grant the working directory READ-ONLY access instead of read-write.
    /// This is what a read-only permission profile requires so shell commands
    /// (`touch newfile`) cannot mutate the workspace.
    #[arg(long)]
    readonly_cwd: bool,

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
    apply_linux_landlock(&std::env::current_dir()?, &[], true)?;
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

    apply_linux_landlock(&args.cwd, &args.allow_read, !args.readonly_cwd)
        .wrap_err("failed to apply Landlock policy")?;
    apply_linux_seccomp(args.allow_network).wrap_err("failed to apply seccomp policy")?;

    Err(command.exec().into())
}

#[cfg(target_os = "linux")]
fn apply_linux_landlock(
    cwd: &std::path::Path,
    extra_read_paths: &[PathBuf],
    cwd_write: bool,
) -> eyre::Result<()> {
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

    let abi = ABI::V1;
    let read_access = make_bitflags!(AccessFs::{ReadFile | ReadDir});
    let exec_access = read_access | AccessFs::Execute;
    let write_access = read_access | AccessFs::from_write(abi);

    let cwd = cwd
        .canonicalize()
        .wrap_err_with(|| format!("failed to canonicalize cwd {}", cwd.display()))?;

    // Grant the workspace read-write by default, or read-only when the caller
    // requested `--readonly-cwd` (a read-only permission profile). Read-only
    // means shell commands can traverse/read the workspace but cannot mutate
    // it — closing the `--sandbox read-only` shell-write hole (codex P1).
    let cwd_access = if cwd_write { write_access } else { read_access };

    // Plan the system write grants. Landlock rules are ADDITIVE and cannot
    // subtract a subtree, so when the read-only cwd lives under `/tmp` (or
    // `/var/tmp`) we must NOT grant broad write to that root — it would
    // re-permit `touch $cwd/newfile` and defeat read-only (codex P1). The plan
    // drops such overlapping grants and hands back a dedicated scratch dir
    // OUTSIDE the workspace so legitimate temp writes still work.
    let write_plan = plan_system_write_paths(&cwd, cwd_write);
    // The scratch dir must exist for its write grant to take effect; create it
    // (it is guaranteed outside the read-only workspace).
    if let Some(scratch) = &write_plan.scratch_dir {
        let _ = std::fs::create_dir_all(scratch);
    }

    let mut created = Ruleset::default()
        .handle_access(AccessFs::from_all(abi))?
        .create()?
        .add_rules(path_beneath_rules(SYSTEM_READ_PATHS, read_access))?
        .add_rules(path_beneath_rules(SYSTEM_EXEC_PATHS, exec_access))?
        .add_rules(path_beneath_rules(
            write_plan.system_write_paths.iter().map(|p| p.as_path()),
            write_access,
        ))?
        .add_rules(path_beneath_rules([cwd.as_path()], cwd_access))?;

    if let Some(scratch) = &write_plan.scratch_dir {
        created = created.add_rules(path_beneath_rules([scratch.as_path()], write_access))?;
    }

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

/// Minimal `LocalFree` binding. Mirrors `rappct`'s own private binding
/// (`the windows crate binding is not exposed`) so we free Win32 LocalAlloc'd
/// buffers WITHOUT depending on the exact `HLOCAL` newtype shape (which differs
/// between `windows` releases). Takes/returns `isize`, matching the ABI.
#[cfg(windows)]
#[allow(unsafe_code)]
mod win_local_free {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        // SAFETY: standard Win32 signature `HLOCAL LocalFree(HLOCAL)`; HLOCAL is
        // a pointer-sized handle, represented here as isize.
        pub fn LocalFree(h: isize) -> isize;
    }

    /// Free a pointer obtained from a LocalAlloc-compatible Win32 API.
    ///
    /// # Safety
    /// `ptr` must have been allocated by such an API and not freed already.
    pub unsafe fn free(ptr: *mut core::ffi::c_void) {
        if !ptr.is_null() {
            // SAFETY: per the function contract, `ptr` is a live LocalAlloc buffer.
            unsafe {
                let _ = LocalFree(ptr as isize);
            }
        }
    }
}

/// Set the DACL of `dir` so the AppContainer package identified by `sid_sddl`
/// has EXACTLY read-only (`FILE_GENERIC_READ`) access — REPLACING any existing
/// ACEs for that package, including a stale `FILE_GENERIC_WRITE` ACE left over
/// from a previously-reused RW profile (codex P1, round 3).
///
/// `rappct::acl::grant_to_package` uses `GRANT_ACCESS`, which is ADDITIVE: on a
/// reused profile it would ADD a read ACE while leaving any prior write ACE in
/// place, so `--readonly-cwd` would still let the package write. We instead use
/// an `EXPLICIT_ACCESS_W` entry with `grfAccessMode = SET_ACCESS`, which per the
/// Win32 ACL contract "replaces all previous access-control information for the
/// trustee" — effectively revoking the write ACE and installing read-only.
///
/// This mirrors the FFI flow in `rappct::acl` (the proven reference for the
/// required `windows` crate features/symbols) but flips the access mode.
#[cfg(windows)]
#[allow(unsafe_code)] // Win32 ACL FFI; each block has a SAFETY note.
fn set_package_dir_readonly(dir: &std::path::Path, sid_sddl: &str) -> eyre::Result<()> {
    use eyre::eyre;
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Security::Authorization::{
        ConvertStringSidToSidW, EXPLICIT_ACCESS_W, GetNamedSecurityInfoW, SE_FILE_OBJECT,
        SET_ACCESS, SetEntriesInAclW, SetNamedSecurityInfoW, TRUSTEE_FORM, TRUSTEE_IS_SID,
        TRUSTEE_IS_WELL_KNOWN_GROUP, TRUSTEE_TYPE, TRUSTEE_W,
    };
    use windows::Win32::Security::{
        ACE_FLAGS, ACL, DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    };
    use windows::Win32::Storage::FileSystem::FILE_GENERIC_READ;
    use windows::core::{PCWSTR, PWSTR};

    // Wide, NUL-terminated strings for the Win32 W-suffixed APIs.
    let sddl_w: Vec<u16> = sid_sddl.encode_utf16().chain(std::iter::once(0)).collect();
    let path_w: Vec<u16> = dir
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // Convert the SDDL SID string to a PSID.
    let mut psid = PSID(std::ptr::null_mut());
    // SAFETY: `sddl_w` is a valid NUL-terminated UTF-16 buffer; `psid` is a
    // valid out-pointer. On success the SID is heap-allocated and freed below.
    unsafe { ConvertStringSidToSidW(PCWSTR(sddl_w.as_ptr()), &mut psid) }
        .map_err(|e| eyre!("ConvertStringSidToSidW failed: {e:?}"))?;

    // Ensure the SID allocation is always freed, even on early return.
    struct SidGuard(PSID);
    impl Drop for SidGuard {
        fn drop(&mut self) {
            // SAFETY: `self.0` came from ConvertStringSidToSidW (LocalAlloc).
            unsafe { win_local_free::free(self.0.0) };
        }
    }
    let sid_guard = SidGuard(psid);

    // Read the directory's current DACL.
    let mut p_sd = PSECURITY_DESCRIPTOR(std::ptr::null_mut());
    let mut p_dacl: *mut ACL = std::ptr::null_mut();
    // SAFETY: valid path buffer + out-pointers; the returned SD is freed below.
    let st = unsafe {
        GetNamedSecurityInfoW(
            PCWSTR(path_w.as_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(&mut p_dacl),
            None,
            &mut p_sd,
        )
    };
    if st.0 != 0 {
        return Err(eyre!("GetNamedSecurityInfoW failed: {st:?}"));
    }
    struct SdGuard(PSECURITY_DESCRIPTOR);
    impl Drop for SdGuard {
        fn drop(&mut self) {
            // SAFETY: the SD buffer was allocated by GetNamedSecurityInfoW.
            unsafe { win_local_free::free(self.0.0) };
        }
    }
    let _sd_guard = SdGuard(p_sd);

    // Build a trustee for the package SID.
    // SAFETY: zeroed TRUSTEE_W is a valid initial state; fields set below.
    let mut trustee: TRUSTEE_W = unsafe { std::mem::zeroed() };
    trustee.TrusteeForm = TRUSTEE_FORM(TRUSTEE_IS_SID.0);
    trustee.TrusteeType = TRUSTEE_TYPE(TRUSTEE_IS_WELL_KNOWN_GROUP.0);
    trustee.ptstrName = PWSTR(sid_guard.0.0 as *mut _);

    // SET_ACCESS replaces ALL existing ACEs for this trustee with a single
    // read-only ACE — removing any stale write ACE (the whole point).
    // SAFETY: zeroed EXPLICIT_ACCESS_W is valid; fields set below.
    let mut ea: EXPLICIT_ACCESS_W = unsafe { std::mem::zeroed() };
    ea.grfAccessPermissions = FILE_GENERIC_READ.0;
    ea.grfAccessMode = SET_ACCESS;
    // Inherit onto sub-containers and objects (mirror rappct Directory grant).
    ea.grfInheritance = ACE_FLAGS(0x3);
    ea.Trustee = trustee;

    // Merge the SET_ACCESS entry into the existing DACL, producing a new DACL.
    let mut new_dacl: *mut ACL = std::ptr::null_mut();
    let entries = [ea];
    // SAFETY: `entries` is a valid slice; `p_dacl` is the current DACL (may be
    // null → treated as empty); `new_dacl` is a valid out-pointer.
    let st2 =
        unsafe { SetEntriesInAclW(Some(&entries), Some(p_dacl as *const ACL), &mut new_dacl) };
    if st2.0 != 0 {
        return Err(eyre!("SetEntriesInAclW failed: {st2:?}"));
    }
    struct DaclGuard(*mut ACL);
    impl Drop for DaclGuard {
        fn drop(&mut self) {
            // SAFETY: new_dacl was allocated by SetEntriesInAclW (LocalAlloc).
            unsafe { win_local_free::free(self.0 as *mut _) };
        }
    }
    let new_dacl_guard = DaclGuard(new_dacl);

    // Write the new DACL back onto the directory.
    // SAFETY: valid path buffer + valid new DACL pointer.
    let st3 = unsafe {
        SetNamedSecurityInfoW(
            PCWSTR(path_w.as_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(new_dacl_guard.0 as *const ACL),
            None,
        )
    };
    if st3.0 != 0 {
        return Err(eyre!("SetNamedSecurityInfoW failed: {st3:?}"));
    }
    Ok(())
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

    // 2. Grant access to the working directory. Read-write by default, or
    //    read-only when `--readonly-cwd` is set (a read-only permission
    //    profile) so shell commands cannot mutate the workspace (codex P1).
    if args.cwd.exists() {
        if args.readonly_cwd {
            // Read-only: REPLACE (not add) the package's ACEs so any stale write
            // ACE from a reused RW profile is revoked (codex P1, round 3). An
            // additive `grant_to_package(FILE_GENERIC_READ)` would leave a prior
            // FILE_GENERIC_WRITE ACE in place and still permit writes.
            set_package_dir_readonly(&args.cwd, profile.sid.as_string()).wrap_err_with(|| {
                format!("failed to set read-only DACL on {}", args.cwd.display())
            })?;
        } else {
            acl::grant_to_package(
                ResourcePath::Directory(args.cwd.clone()),
                &profile.sid,
                AccessMask(AccessMask::FILE_GENERIC_READ.0 | AccessMask::FILE_GENERIC_WRITE.0),
            )
            .wrap_err_with(|| format!("failed to grant rw to {}", args.cwd.display()))?;
        }
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

#[cfg(test)]
mod write_grant_plan_tests {
    use super::*;

    #[test]
    fn should_grant_all_system_write_paths_when_cwd_writable() {
        // Writable workspace: behaviour unchanged — every system write path is
        // granted and no dedicated scratch dir is needed.
        let plan = plan_system_write_paths(Path::new("/home/user/project"), true);
        assert_eq!(
            plan.system_write_paths,
            vec![
                PathBuf::from("/tmp"),
                PathBuf::from("/var/tmp"),
                PathBuf::from("/dev/null"),
            ]
        );
        assert_eq!(plan.scratch_dir, None);
    }

    #[test]
    fn should_grant_all_system_write_paths_when_readonly_cwd_outside_tmp() {
        // Read-only cwd that is NOT under any temp root: /tmp and /var/tmp do
        // not contain cwd, so the broad grants are safe and kept; no scratch
        // dir needed.
        let plan = plan_system_write_paths(Path::new("/home/user/project"), false);
        assert_eq!(
            plan.system_write_paths,
            vec![
                PathBuf::from("/tmp"),
                PathBuf::from("/var/tmp"),
                PathBuf::from("/dev/null"),
            ]
        );
        assert_eq!(plan.scratch_dir, None);
    }

    #[test]
    fn should_drop_tmp_write_grant_when_readonly_cwd_under_tmp() {
        // P1 (codex): read-only cwd UNDER /tmp. Granting broad write to /tmp
        // would re-permit `touch $cwd/newfile`. The /tmp grant must be dropped;
        // /var/tmp and /dev/null stay; a scratch dir OUTSIDE cwd is provided.
        let cwd = Path::new("/tmp/work");
        let plan = plan_system_write_paths(cwd, false);

        assert!(
            !plan.system_write_paths.contains(&PathBuf::from("/tmp")),
            "the /tmp write grant that contains the read-only cwd must be dropped"
        );
        assert!(
            plan.system_write_paths
                .contains(&PathBuf::from("/dev/null")),
            "/dev/null must remain writable"
        );
        // No granted system write path may contain the cwd.
        for p in &plan.system_write_paths {
            assert!(
                !path_contains(p, cwd),
                "granted system write path {} must not contain read-only cwd {}",
                p.display(),
                cwd.display()
            );
        }
        // A scratch dir must be provided and must be OUTSIDE the workspace.
        let scratch = plan
            .scratch_dir
            .as_ref()
            .expect("a scratch dir must be provided when a temp grant is dropped");
        assert!(
            !scratch.starts_with(cwd),
            "scratch dir {} must be OUTSIDE the read-only cwd {}",
            scratch.display(),
            cwd.display()
        );
    }

    #[test]
    fn should_drop_var_tmp_write_grant_when_readonly_cwd_under_var_tmp() {
        // Same hazard, workspace under /var/tmp.
        let cwd = Path::new("/var/tmp/build");
        let plan = plan_system_write_paths(cwd, false);
        assert!(
            !plan.system_write_paths.contains(&PathBuf::from("/var/tmp")),
            "the /var/tmp write grant containing the read-only cwd must be dropped"
        );
        assert!(
            plan.system_write_paths.contains(&PathBuf::from("/tmp")),
            "/tmp (which does not contain cwd) stays writable"
        );
        for p in &plan.system_write_paths {
            assert!(!path_contains(p, cwd));
        }
        let scratch = plan.scratch_dir.expect("scratch dir required");
        assert!(!scratch.starts_with(cwd));
        // The scratch dir should come from a root that does not contain cwd
        // (here /tmp), never from /var/tmp which does.
        assert!(
            scratch.starts_with("/tmp"),
            "scratch should live under a root outside cwd, got {}",
            scratch.display()
        );
    }

    #[test]
    fn should_not_grant_write_to_any_path_containing_readonly_cwd_under_tmp() {
        // Round-4 sanity: the WHOLE point — under a read-only cwd anywhere below
        // /tmp, NOTHING in the plan (system paths OR scratch) may cover cwd.
        let cwd = Path::new("/tmp/deeply/nested/work");
        let plan = plan_system_write_paths(cwd, false);
        for p in &plan.system_write_paths {
            assert!(
                !path_contains(p, cwd),
                "{} must not contain cwd {}",
                p.display(),
                cwd.display()
            );
        }
        if let Some(scratch) = &plan.scratch_dir {
            assert!(
                !scratch.starts_with(cwd),
                "scratch {} must not be under cwd {}",
                scratch.display(),
                cwd.display()
            );
        }
    }
}

// Windows-specific tests for the read-only DACL replacement (codex P1, round 3).
// The full behaviour (revoking a stale write ACE) requires a real AppContainer
// profile + filesystem and MUST be validated on Windows/Win01 CI. These tests
// only exercise what is checkable in a unit test: the FFI wiring compiles and
// the helper fails closed (returns Err) on an invalid target instead of
// panicking or silently succeeding.
#[cfg(all(test, windows))]
mod windows_readonly_dacl_tests {
    use super::*;

    #[test]
    fn should_error_when_setting_readonly_dacl_on_nonexistent_path() {
        // A well-formed SDDL SID string, but a path that does not exist:
        // GetNamedSecurityInfoW must fail and we must surface an Err (fail
        // closed) rather than proceeding as if read-only were applied.
        let missing = std::path::Path::new(
            "C:\\octos-sandbox-nonexistent-\\definitely\\not\\here\\readonly-test",
        );
        // "S-1-1-0" = Everyone; a valid SDDL SID that ConvertStringSidToSidW
        // accepts, so we reach (and fail at) the GetNamedSecurityInfoW step.
        let result = set_package_dir_readonly(missing, "S-1-1-0");
        assert!(
            result.is_err(),
            "read-only DACL set on a nonexistent path must fail closed"
        );
    }

    #[test]
    fn should_error_when_sid_string_is_invalid() {
        // An invalid SDDL SID must be rejected at ConvertStringSidToSidW,
        // returning Err rather than dereferencing a null SID.
        let tmp = std::env::temp_dir();
        let result = set_package_dir_readonly(&tmp, "not-a-valid-sid");
        assert!(
            result.is_err(),
            "an invalid SID string must fail closed at conversion"
        );
    }
}
