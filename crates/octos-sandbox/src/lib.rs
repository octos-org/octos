//! Confinement for an Octos worker whose only authority is its host's IPC broker.
//!
//! This is separate from the interactive tool sandbox's workspace permissions.
//! The parent must use [`host_managed_command`] and the worker must call
//! [`confine_host_managed`] before starting threads or accepting private input.
//! No configuration or credential files, sockets, subprocesses, or persistent
//! writes are allowed. Unsupported platforms and kernels fail closed.

use eyre::{Result, WrapErr};
use std::path::Path;
use std::process::Command;

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
mod linux;
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
mod macos;

/// The kernel mechanisms successfully installed in this worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostSandbox {
    pub platform: &'static str,
}

/// Create a parent-enforced launcher with no ambient environment or working directory.
///
/// `executable` must be an existing absolute executable path. The returned command
/// accepts the worker's arguments and piped standard streams as usual. The host
/// must not add credentials, user data, or unnecessary environment variables.
/// Confinement precedes the executable's entry point, so even a lying or obsolete
/// worker cannot acquire direct network or personal-file access.
pub fn host_managed_command(executable: &Path) -> Result<Command> {
    eyre::ensure!(
        executable.is_absolute(),
        "host-managed executable must be an absolute path"
    );
    let executable = executable
        .canonicalize()
        .wrap_err("resolve host-managed executable")?;
    eyre::ensure!(
        executable.is_file(),
        "host-managed executable must be a regular file"
    );
    #[cfg(target_os = "macos")]
    let mut command = macos::command(&executable)?;
    #[cfg(target_os = "linux")]
    let mut command = linux::command(&executable)?;
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    return Err(eyre::eyre!(
        "host-managed process confinement is unsupported on this platform"
    ));
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        command.env_clear().current_dir("/");
        Ok(command)
    }
}

/// Complete startup confinement before the worker processes host input.
///
/// Call on the main thread before any runtime threads or host input. An error
/// may mean partial confinement has already happened: terminate the process,
/// never fall back to another agent mode. OS restrictions are inherited by all
/// subsequently created threads. macOS validates the parent's final Seatbelt
/// profile (Seatbelt cannot be reapplied); Linux installs an additional stricter
/// profile. On macOS, immutable system runtime files remain readable and the
/// exact initial executable can reexec itself with the same restrictions.
pub fn confine_host_managed() -> Result<HostSandbox> {
    #[cfg(target_os = "macos")]
    {
        macos::confine()
    }
    #[cfg(target_os = "linux")]
    {
        linux::confine()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Err(eyre::eyre!(
            "host-managed process confinement is unsupported on this platform"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_reject_relative_executable_when_host_managed_command_requested() {
        // Rejected before any platform dispatch, so the refusal is identical
        // on every host.
        let error = host_managed_command(Path::new("octos-worker/agent")).unwrap_err();
        assert!(error.to_string().contains("absolute path"));
    }

    #[test]
    fn should_reject_missing_executable_when_host_managed_command_requested() {
        // Absolute-but-absent path fails at canonicalization, before any
        // platform dispatch.
        let missing = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .join("octos-missing-worker");
        assert!(missing.is_absolute());
        assert!(host_managed_command(&missing).is_err());
    }

    #[test]
    fn should_reject_directory_when_host_managed_command_requested() {
        // A directory canonicalizes fine but is not a runnable regular file.
        let directory = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let error = host_managed_command(&directory).unwrap_err();
        assert!(error.to_string().contains("regular file"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn should_build_cleared_seatbelt_launcher_when_absolute_executable_given() {
        // The wrapper must strip the ambient environment and working
        // directory regardless of what the platform launcher adds itself.
        // (The end-to-end escape probes in tests/host_managed.rs cover the
        // real subprocess round trip; this pins the wrapper's own shape.)
        let command = host_managed_command(Path::new("/bin/ls")).unwrap();
        assert_eq!(command.get_program(), "/usr/bin/sandbox-exec");
        assert_eq!(command.get_current_dir(), Some(Path::new("/")));
        assert_eq!(command.get_envs().count(), 0);
    }
}
