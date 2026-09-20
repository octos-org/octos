use crate::HostSandbox;
use eyre::{Result, WrapErr};
use std::ffi::{CStr, CString};
use std::io;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

// No mach-lookup, network, process-fork/exec, keychain, or filesystem grants.
// Self process-info supports descriptor/thread checks without exposing peers.
// Anonymous VM, kqueue, pthreads, clocks, and inherited stdio need no grants.
const WORKER_PROFILE: &str = r#"(version 1)
(deny default)
(allow process-info* (target self))
(allow signal (target self))
(allow sysctl-read
    (sysctl-name "hw.ncpu")
    (sysctl-name "hw.activecpu")
    (sysctl-name "hw.physicalcpu")
    (sysctl-name "hw.logicalcpu")
    (sysctl-name "hw.pagesize")
    (sysctl-name "hw.pagesize_compat")
    (sysctl-name "kern.usrstack64")
    (sysctl-name "kern.osrelease"))
(allow file-read-data file-write-data (literal "/dev/null"))
(allow file-read-data (literal "/dev/random") (literal "/dev/urandom"))
"#;

#[link(name = "sandbox")]
unsafe extern "C" {
    fn sandbox_init(
        profile: *const libc::c_char,
        flags: u64,
        error: *mut *mut libc::c_char,
    ) -> libc::c_int;
    fn sandbox_free_error(error: *mut libc::c_char);
    fn sandbox_check(
        pid: libc::pid_t,
        operation: *const libc::c_char,
        filter: libc::c_int,
        ...
    ) -> libc::c_int;
}

fn profile_literal(path: &Path) -> Result<String> {
    let path = path
        .to_str()
        .ok_or_else(|| eyre::eyre!("sandbox path must be UTF-8"))?;
    eyre::ensure!(
        !path.chars().any(char::is_control),
        "sandbox path contains control characters"
    );
    Ok(format!(
        "\"{}\"",
        path.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

pub(super) fn command(executable: &Path) -> Result<Command> {
    eyre::ensure!(
        Path::new("/usr/bin/sandbox-exec").is_file(),
        "macOS sandbox-exec is unavailable"
    );
    // System libraries are needed to enter the executable. Seatbelt cannot be
    // reapplied, so these immutable runtime reads remain available. Only this
    // exact executable may reexec; process-fork remains denied throughout.
    let executable_literal = profile_literal(executable)?;
    let profile = format!(
        "{WORKER_PROFILE}\n\
        (import \"dyld-support.sb\")\n\
        (allow process-exec (literal {executable_literal}))\n\
        (allow file-read* file-map-executable (literal {executable_literal})\n\
            (subpath \"/System/Library\") (subpath \"/usr/lib\")\n\
            (subpath \"/System/Cryptexes/OS\")\n\
            (subpath \"/System/Volumes/Preboot/Cryptexes/OS\")\n\
            (literal \"/Library/Apple/System/Library\")\n\
            (literal \"/private/var/db/dyld\"))\n"
    );
    let mut command = Command::new("/usr/bin/sandbox-exec");
    command.args(["-p", &profile, "--"]);
    command.arg(executable);
    // SAFETY: the closure uses only process-local kernel calls and a fixed
    // stack buffer; no allocation or user locks are touched between fork/exec.
    unsafe {
        command.pre_exec(mark_descriptors_close_on_exec);
    }
    Ok(command)
}

fn descriptors() -> io::Result<([libc::proc_fdinfo; 4096], usize)> {
    // Refuse an oversized inherited descriptor table rather than truncating it.
    let mut entries: [libc::proc_fdinfo; 4096] = unsafe { std::mem::zeroed() };
    let capacity = std::mem::size_of_val(&entries) as i32;
    let bytes = unsafe {
        libc::proc_pidinfo(
            libc::getpid(),
            libc::PROC_PIDLISTFDS,
            0,
            entries.as_mut_ptr().cast(),
            capacity,
        )
    };
    if bytes <= 0 || bytes >= capacity {
        return Err(io::Error::from_raw_os_error(libc::EMFILE));
    }
    Ok((
        entries,
        bytes as usize / std::mem::size_of::<libc::proc_fdinfo>(),
    ))
}

fn mark_descriptors_close_on_exec() -> io::Result<()> {
    let (entries, count) = descriptors()?;
    for entry in &entries[..count] {
        let fd = entry.proc_fd;
        if fd <= 2 {
            continue;
        }
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

pub(super) fn confine() -> Result<HostSandbox> {
    let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
    let bytes = unsafe {
        libc::proc_pidinfo(
            libc::getpid(),
            libc::PROC_PIDTASKINFO,
            0,
            (&mut info as *mut libc::proc_taskinfo).cast(),
            std::mem::size_of_val(&info) as i32,
        )
    };
    eyre::ensure!(
        bytes as usize == std::mem::size_of_val(&info) && info.pti_threadnum == 1,
        "host-managed confinement must run before creating threads"
    );
    let (entries, count) = descriptors().wrap_err("inspect inherited descriptors")?;
    for entry in &entries[..count] {
        if entry.proc_fd > 2 {
            unsafe {
                libc::close(entry.proc_fd);
            }
        }
    }
    std::env::set_current_dir("/").wrap_err("clear inherited working directory")?;
    let core_limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    if unsafe { libc::setrlimit(libc::RLIMIT_CORE, &core_limit) } != 0 {
        return Err(io::Error::last_os_error()).wrap_err("disable worker core dumps");
    }
    // Apple only permits one sandbox_init per process. When launched through
    // host_managed_command the parent has already installed the final profile.
    // These checks catch accidental legacy launchers; they are not attestation
    // of an arbitrary third-party profile. The trusted parent launcher is the
    // security boundary and must never trust a child's confined=true claim.
    let sandboxed = unsafe { sandbox_check(libc::getpid(), std::ptr::null(), 0) };
    if sandboxed > 0 {
        for operation in [c"network-outbound", c"network-inbound", c"process-fork"] {
            eyre::ensure!(
                unsafe { sandbox_check(libc::getpid(), operation.as_ptr(), 0) } > 0,
                "inherited sandbox allows {}",
                operation.to_string_lossy()
            );
        }
        for operation in [c"file-read-data", c"file-read-metadata", c"file-write-data"] {
            eyre::ensure!(
                unsafe {
                    sandbox_check(
                        libc::getpid(),
                        operation.as_ptr(),
                        1,
                        c"/private/etc/passwd".as_ptr(),
                    )
                } > 0,
                "inherited sandbox permits unrelated files"
            );
        }
        return Ok(HostSandbox {
            platform: "macos-seatbelt",
        });
    }
    eyre::ensure!(
        sandboxed == 0,
        "cannot inspect inherited Seatbelt confinement"
    );
    let profile = CString::new(WORKER_PROFILE)?;
    let mut error = std::ptr::null_mut();
    let status = unsafe { sandbox_init(profile.as_ptr(), 0, &mut error) };
    if status != 0 {
        let message = if error.is_null() {
            "unknown Seatbelt error".to_owned()
        } else {
            unsafe { CStr::from_ptr(error) }
                .to_string_lossy()
                .into_owned()
        };
        unsafe {
            sandbox_free_error(error);
        }
        eyre::bail!("host-managed Seatbelt confinement failed: {message}");
    }
    Ok(HostSandbox {
        platform: "macos-seatbelt",
    })
}
