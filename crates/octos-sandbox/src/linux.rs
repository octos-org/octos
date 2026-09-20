use crate::HostSandbox;
use eyre::{Result, WrapErr};
use landlock::{
    ABI, Access, AccessFs, CompatLevel, Compatible, Ruleset, RulesetAttr, RulesetCreatedAttr,
    path_beneath_rules,
};
use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule,
};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Seek, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

// ABI 3 (Linux 6.2+) includes truncation and reparenting. Device ioctl is denied
// by seccomp; namespaces hide path metadata, which Landlock does not mediate.
// Older or disabled Landlock kernels fail closed without partial enforcement.
const REQUIRED_ABI: ABI = ABI::V3;

fn ruleset() -> Result<OwnedFd> {
    let mut rules = Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(AccessFs::from_all(REQUIRED_ABI))?
        .create()?;
    rules = rules
        .add_rules(path_beneath_rules(
            ["/dev/null"],
            AccessFs::ReadFile | AccessFs::WriteFile,
        ))?
        .add_rules(path_beneath_rules(
            ["/dev/urandom", "/dev/random"],
            AccessFs::ReadFile,
        ))?;
    // HardRequirement above proves the requested rights exist. Extracting the
    // descriptor lets the post-fork child install it using only kernel calls.
    let fd: Option<OwnedFd> = rules.into();
    fd.ok_or_else(|| eyre::eyre!("host-managed Landlock ruleset was not created"))
}

fn filter(allow_entry: bool) -> Result<BpfProgram> {
    // A syscall allowlist also closes alternate I/O routes: io_uring, SysV IPC,
    // ptrace/process_vm_*, sockets (including Unix), pidfd_getfd, perf, bpf,
    // modules, mounts and namespaces are absent. Unknown syscalls fail closed.
    let mut calls = vec![
        libc::SYS_read,
        libc::SYS_write,
        libc::SYS_readv,
        libc::SYS_writev,
        libc::SYS_recvfrom,
        libc::SYS_sendto,
        libc::SYS_pread64,
        libc::SYS_pwrite64,
        libc::SYS_close,
        libc::SYS_close_range,
        libc::SYS_fstat,
        libc::SYS_lseek,
        libc::SYS_fcntl,
        libc::SYS_dup,
        libc::SYS_dup3,
        libc::SYS_pipe2,
        libc::SYS_mmap,
        libc::SYS_mprotect,
        libc::SYS_munmap,
        libc::SYS_mremap,
        libc::SYS_madvise,
        libc::SYS_brk,
        libc::SYS_futex,
        libc::SYS_futex_waitv,
        libc::SYS_set_tid_address,
        libc::SYS_set_robust_list,
        libc::SYS_rseq,
        libc::SYS_rt_sigaction,
        libc::SYS_rt_sigprocmask,
        libc::SYS_rt_sigreturn,
        libc::SYS_sigaltstack,
        libc::SYS_signalfd4,
        libc::SYS_pselect6,
        libc::SYS_ppoll,
        libc::SYS_epoll_create1,
        libc::SYS_epoll_ctl,
        libc::SYS_epoll_pwait,
        libc::SYS_epoll_pwait2,
        libc::SYS_eventfd2,
        libc::SYS_timerfd_create,
        libc::SYS_timerfd_settime,
        libc::SYS_timerfd_gettime,
        libc::SYS_clock_gettime,
        libc::SYS_clock_getres,
        libc::SYS_clock_nanosleep,
        libc::SYS_nanosleep,
        libc::SYS_gettimeofday,
        libc::SYS_getrandom,
        libc::SYS_getpid,
        libc::SYS_gettid,
        libc::SYS_getppid,
        libc::SYS_getuid,
        libc::SYS_geteuid,
        libc::SYS_getgid,
        libc::SYS_getegid,
        libc::SYS_getresuid,
        libc::SYS_getresgid,
        libc::SYS_getgroups,
        libc::SYS_uname,
        libc::SYS_sysinfo,
        libc::SYS_getrusage,
        libc::SYS_getrlimit,
        libc::SYS_prlimit64,
        libc::SYS_sched_getaffinity,
        libc::SYS_sched_yield,
        libc::SYS_sched_getparam,
        libc::SYS_sched_getscheduler,
        libc::SYS_exit,
        libc::SYS_exit_group,
        // The worker must be able to install its stricter second profile.
        libc::SYS_landlock_create_ruleset,
        libc::SYS_landlock_add_rule,
        libc::SYS_landlock_restrict_self,
        libc::SYS_seccomp,
    ];
    #[cfg(target_arch = "x86_64")]
    calls.extend([
        libc::SYS_arch_prctl,
        libc::SYS_poll,
        libc::SYS_epoll_wait,
        libc::SYS_dup2,
        libc::SYS_setrlimit,
    ]);
    if allow_entry {
        // Bubblewrap's PID-namespace init reaps the already-created worker.
        // Waiting grants no ability to create processes or access host PIDs.
        calls.extend([
            libc::SYS_execve,
            libc::SYS_execveat,
            libc::SYS_wait4,
            libc::SYS_waitid,
            libc::SYS_newfstatat,
            libc::SYS_statx,
            libc::SYS_readlinkat,
            libc::SYS_getdents64,
        ]);
        #[cfg(target_arch = "x86_64")]
        calls.extend([
            libc::SYS_stat,
            libc::SYS_lstat,
            libc::SYS_access,
            libc::SYS_readlink,
        ]);
    }
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> =
        calls.into_iter().map(|n| (n, vec![])).collect();
    // Tokio's signal driver uses an anonymous connected Unix stream pair as
    // its self-pipe. Neither endpoint can reach another process: socket,
    // connect, bind, accept, and FD-passing syscalls remain denied. Datagram
    // pairs are excluded because sendto could address an unrelated endpoint.
    rules.insert(
        libc::SYS_socketpair,
        vec![SeccompRule::new(vec![
            SeccompCondition::new(
                0,
                SeccompCmpArgLen::Dword,
                SeccompCmpOp::Eq,
                libc::AF_UNIX as u64,
            )?,
            SeccompCondition::new(
                1,
                SeccompCmpArgLen::Dword,
                SeccompCmpOp::MaskedEq(!(libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK) as u32 as u64),
                libc::SOCK_STREAM as u64,
            )?,
            SeccompCondition::new(2, SeccompCmpArgLen::Dword, SeccompCmpOp::Eq, 0)?,
        ])?],
    );
    if allow_entry {
        // Bootstrap may open runtime files and O_PATH ruleset handles, but
        // never request write access, including to the entropy devices.
        let write_flags = libc::O_ACCMODE
            | libc::O_CREAT
            | libc::O_TRUNC
            | libc::O_APPEND
            | (libc::O_TMPFILE & !libc::O_DIRECTORY);
        rules.insert(
            libc::SYS_openat,
            vec![SeccompRule::new(vec![SeccompCondition::new(
                2,
                SeccompCmpArgLen::Dword,
                SeccompCmpOp::MaskedEq(write_flags as u64),
                0,
            )?])?],
        );
    }
    // pthreads share the existing address space and restrictions. Processes,
    // namespace creation, and clone variants with an exit signal are refused.
    let required = (libc::CLONE_VM | libc::CLONE_SIGHAND | libc::CLONE_THREAD) as u64;
    let permitted = required
        | (libc::CLONE_FS
            | libc::CLONE_FILES
            | libc::CLONE_SYSVSEM
            | libc::CLONE_SETTLS
            | libc::CLONE_PARENT_SETTID
            | libc::CLONE_CHILD_CLEARTID
            | libc::CLONE_CHILD_SETTID
            | libc::CLONE_DETACHED) as u64;
    rules.insert(
        libc::SYS_clone,
        vec![SeccompRule::new(vec![
            SeccompCondition::new(
                0,
                SeccompCmpArgLen::Qword,
                SeccompCmpOp::MaskedEq(required),
                required,
            )?,
            SeccompCondition::new(
                0,
                SeccompCmpArgLen::Qword,
                SeccompCmpOp::MaskedEq(!permitted),
                0,
            )?,
        ])?],
    );
    let prctl_ops = [
        libc::PR_SET_NAME,
        libc::PR_GET_NAME,
        libc::PR_GET_NO_NEW_PRIVS,
    ];
    let mut prctl = prctl_ops
        .into_iter()
        .map(|op| {
            SeccompRule::new(vec![SeccompCondition::new(
                0,
                SeccompCmpArgLen::Dword,
                SeccompCmpOp::Eq,
                op as u64,
            )?])
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for (op, value) in [(libc::PR_SET_NO_NEW_PRIVS, 1), (libc::PR_SET_DUMPABLE, 0)] {
        prctl.push(SeccompRule::new(vec![
            SeccompCondition::new(0, SeccompCmpArgLen::Dword, SeccompCmpOp::Eq, op as u64)?,
            SeccompCondition::new(1, SeccompCmpArgLen::Qword, SeccompCmpOp::Eq, value)?,
        ])?);
    }
    rules.insert(libc::SYS_prctl, prctl);
    // ENOSYS also makes glibc fall back from clone3 (whose pointed-to flags
    // cannot be inspected by classic BPF) to the checked clone syscall.
    Ok(SeccompFilter::new(
        rules,
        SeccompAction::Errno(libc::ENOSYS as u32),
        SeccompAction::Allow,
        std::env::consts::ARCH.try_into()?,
    )?
    .try_into()?)
}

fn install(rules: &OwnedFd, filter: &BpfProgram) -> io::Result<()> {
    let core_limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    if unsafe { libc::setrlimit(libc::RLIMIT_CORE, &core_limit) } != 0
        || unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0
        || unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0
        || unsafe { libc::syscall(libc::SYS_landlock_restrict_self, rules.as_raw_fd(), 0) } != 0
    {
        return Err(io::Error::last_os_error());
    }
    let program = libc::sock_fprog {
        len: filter
            .len()
            .try_into()
            .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?,
        filter: filter.as_ptr().cast::<libc::sock_filter>().cast_mut(),
    };
    if unsafe {
        libc::syscall(
            libc::SYS_seccomp,
            libc::SECCOMP_SET_MODE_FILTER,
            0,
            &program,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(super) fn command(executable: &Path) -> Result<Command> {
    let helper = ["/usr/bin/bwrap", "/bin/bwrap"]
        .into_iter()
        .find(|path| Path::new(path).is_file())
        .ok_or_else(|| {
            eyre::eyre!(
                "host-managed Linux agents require bubblewrap (bwrap) and enabled user namespaces"
            )
        })?;
    let mounts = runtime_files(executable)?;
    let filter = filter(true)?;
    let raw_fd = unsafe {
        libc::memfd_create(
            c"octos-host-seccomp".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    };
    eyre::ensure!(
        raw_fd >= 0,
        "create host-managed seccomp descriptor: {}",
        io::Error::last_os_error()
    );
    let mut filter_file = unsafe { std::fs::File::from_raw_fd(raw_fd) };
    for instruction in &filter {
        filter_file.write_all(&instruction.code.to_ne_bytes())?;
        filter_file.write_all(&[instruction.jt, instruction.jf])?;
        filter_file.write_all(&instruction.k.to_ne_bytes())?;
    }
    filter_file.rewind()?;
    let seals = libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE;
    eyre::ensure!(
        unsafe { libc::fcntl(raw_fd, libc::F_ADD_SEALS, seals) } == 0,
        "seal seccomp descriptor"
    );
    let mut command = Command::new(helper);
    command.args([
        "--unshare-all",
        "--unshare-user",
        "--disable-userns",
        "--die-with-parent",
        "--new-session",
        "--clearenv",
        "--cap-drop",
        "ALL",
        "--hostname",
        "octos-worker",
        "--chdir",
        "/",
    ]);
    let mut search_paths = BTreeSet::new();
    for (destination, source) in mounts {
        command.arg("--ro-bind").arg(source).arg(&destination);
        if let Some(parent) = destination.parent() {
            search_paths.insert(parent.to_path_buf());
        }
    }
    // No library directories, ld.so cache, /etc, /proc, home, or workspace are
    // mounted. lddtree parses dependencies without executing the target.
    if !search_paths.is_empty() {
        command
            .args(["--setenv", "LD_LIBRARY_PATH"])
            .arg(std::env::join_paths(search_paths)?);
    }
    for device in ["/dev/null", "/dev/random", "/dev/urandom"] {
        command.args(["--ro-bind", device, device]);
    }
    command
        .arg("--ro-bind")
        .arg(executable)
        .arg("/app/octos")
        .args([
            "--remount-ro",
            "/",
            "--seccomp",
            &raw_fd.to_string(),
            "--",
            "/app/octos",
        ]);
    // SAFETY: the pre-exec closure uses only kernel calls, not allocation or
    // mutexes. The sealed BPF descriptor is the sole deliberate extra FD and
    // bubblewrap consumes/closes it before entering the worker.
    unsafe {
        command.pre_exec(move || {
            let fd = filter_file.as_raw_fd();
            let core_limit = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            if libc::syscall(
                libc::SYS_close_range,
                3u32,
                u32::MAX,
                libc::CLOSE_RANGE_CLOEXEC,
            ) != 0
                || libc::fcntl(fd, libc::F_SETFD, 0) < 0
                || libc::setrlimit(libc::RLIMIT_CORE, &core_limit) != 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(command)
}

fn runtime_files(executable: &Path) -> Result<BTreeMap<PathBuf, PathBuf>> {
    let tree = lddtree::DependencyAnalyzer::new(PathBuf::from("/"))
        .library_paths(vec![])
        .analyze(executable)
        .wrap_err("analyze host-managed ELF dependencies")?;
    let mut mounts = BTreeMap::new();
    let mut paths: Vec<PathBuf> = tree
        .libraries
        .values()
        .map(|lib| lib.path.clone())
        .collect();
    if let Some(interpreter) = tree.interpreter {
        paths.push(PathBuf::from(interpreter));
    }
    for path in paths {
        eyre::ensure!(
            path.is_absolute()
                && path.components().all(|part| matches!(
                    part,
                    std::path::Component::RootDir | std::path::Component::Normal(_)
                )),
            "ELF runtime paths must be absolute and contain no traversal"
        );
        let real = path
            .canonicalize()
            .wrap_err("resolve ELF runtime dependency")?;
        eyre::ensure!(
            ["/lib", "/lib64", "/usr/lib", "/usr/lib64"]
                .iter()
                .any(|root| real.starts_with(root)),
            "host-managed executables must use system runtime libraries"
        );
        let metadata = real.metadata()?;
        eyre::ensure!(
            metadata.is_file() && metadata.uid() == 0 && metadata.mode() & 0o022 == 0,
            "host-managed runtime libraries must be immutable to ordinary users"
        );
        for ancestor in real.ancestors().skip(1) {
            let metadata = ancestor.metadata()?;
            eyre::ensure!(
                metadata.uid() == 0 && metadata.mode() & 0o022 == 0,
                "host-managed runtime library directories must be immutable to ordinary users"
            );
        }
        mounts.insert(path, real.clone());
        mounts.insert(real.clone(), real);
    }
    Ok(mounts)
}

pub(super) fn confine() -> Result<HostSandbox> {
    // Under the parent's mount namespace /proc is unavailable. The proc task
    // directory can be preopened by the launcher only at the price of exposing
    // a sensitive inherited FD. Instead the startup-only API checks the kernel
    // thread id: the broker CLI calls this before constructing its runtime.
    eyre::ensure!(
        unsafe { libc::syscall(libc::SYS_gettid) } == unsafe { libc::getpid() } as libc::c_long,
        "host-managed confinement must run on the initial thread"
    );
    let rules = ruleset()?;
    let filter = filter(false)?;
    // close_range must preserve the newly created ruleset until installation.
    // All inherited FDs were CLOEXEC in the parent; close any worker startup
    // descriptors after installing the ruleset (before accepting host input).
    install(&rules, &filter).wrap_err("install host-managed Landlock/seccomp confinement")?;
    drop(rules);
    if unsafe { libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 0) } != 0 {
        return Err(io::Error::last_os_error()).wrap_err("close worker startup descriptors");
    }
    Ok(HostSandbox {
        platform: "linux-bubblewrap-landlock-seccomp",
    })
}
