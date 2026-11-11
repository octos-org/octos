//! Shared process-control helpers for the serial `octos serve` integration tests.
//!
//! This file lives in a nested directory so Cargo does not compile it as a
//! separate integration-test target. Each including test binary gets its own
//! serial mutex, matching the per-target serialization of the original helpers.

#![allow(dead_code)]
#![allow(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

/// Serve tests spawn real processes that contend for shared resources
/// (model catalog, profile store) — serialize them. A poisoned lock carries
/// no broken invariant here, so recover it instead of cascading failures.
static SERIAL: Mutex<()> = Mutex::new(());

pub fn serial_guard() -> MutexGuard<'static, ()> {
    match SERIAL.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Path to a real octos binary that includes the `serve` subcommand.
///
/// When the test harness is compiled WITH the `api` feature, Cargo already
/// built `CARGO_BIN_EXE_octos` with `serve` — reuse it directly. Without
/// `api`, always run Cargo's incremental build into a dedicated target dir so
/// the spawned process is the current, real production binary.
pub fn octos_binary() -> PathBuf {
    if cfg!(feature = "api") {
        return env!("CARGO_BIN_EXE_octos").into();
    }
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let target_dir = Path::new(manifest_dir).join("../../target/serve-process-tests");
    let bin = target_dir.join("debug/octos");
    let out = Command::new("cargo")
        .args(["build", "-p", "octos-cli", "--features", "api"])
        .current_dir(Path::new(manifest_dir).join("../.."))
        .env("CARGO_TARGET_DIR", &target_dir)
        .output()
        .expect("failed to bootstrap api-enabled octos binary");
    assert!(
        out.status.success(),
        "bootstrap cargo build failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    bin
}

/// Build the base serve Command with a private instance data dir.
///
/// Callers add their scenario-specific flags and environment. `setsid`
/// detaches the child from the test runner's process group, so signals reach
/// the real octos process directly and `child.id()` is that process's PID.
pub fn serve_command(port: u16, data_dir: &Path) -> Command {
    let mut cmd = Command::new(octos_binary());
    cmd.args([
        "serve",
        "--instance-data-dir",
        data_dir.to_str().unwrap(),
        // main.rs derives the tracing rolling-log dir from --data-dir; pin it
        // to the same private dir so tests never touch the shared instance.
        "--data-dir",
        data_dir.to_str().unwrap(),
        "-p",
        &port.to_string(),
    ])
    .stdin(Stdio::null())
    // Remove host instance overrides so the child uses ONLY the private dirs.
    .env_remove("OCTOS_INSTANCE_DATA_DIR")
    .env_remove("OCTOS_HOME")
    .env_remove("OCTOS_DATA_DIR");
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd
}

/// Wait for a port to accept TCP connections.
pub fn wait_for_port(port: u16, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    loop {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return true;
        }
        if start.elapsed() > timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Find a free port by binding to port 0.
pub fn find_free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// PIDs of live `octos gateway` processes spawned for `marker` (the private
/// data dir appears in the gateway's profile/data-dir/cwd argv). This is the
/// operator-visible orphan check (`pgrep -f 'octos gateway.*<data-dir>'`).
pub fn gateway_pids(marker: &str) -> Vec<u32> {
    let out = match Command::new("ps").args(["-eo", "pid=,args="]).output() {
        Ok(out) => out,
        Err(_) => return Vec::new(),
    };
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .filter_map(|line| {
            let line = line.trim_start();
            let (pid, args) = line.split_once(' ')?;
            // " gateway " with spaces: the subcommand token, so argv[0]
            // paths can't false-positive on a "gateway" substring.
            if args.contains(" gateway ") && args.contains(marker) {
                pid.parse::<u32>().ok()
            } else {
                None
            }
        })
        .collect()
}

/// Last-resort cleanup so a failing assertion cannot leak gateway orphans
/// into later CI steps.
pub fn kill_gateways(marker: &str) {
    for pid in gateway_pids(marker) {
        unsafe {
            libc::kill(pid as i32, libc::SIGKILL);
        }
    }
}

/// Panic-safe cleanup: kill the serve child and any gateway orphans it left,
/// then remove the scratch data dir, including on assertion failure paths.
pub struct Cleanup {
    pub marker: String,
    pub data_dir: PathBuf,
    pub child: Option<Child>,
}

impl Cleanup {
    pub fn with_child(child: Child, data_dir: &Path) -> (String, Self) {
        let marker = data_dir.to_str().unwrap().to_string();
        let cleanup = Self {
            marker: marker.clone(),
            data_dir: data_dir.to_path_buf(),
            child: Some(child),
        };
        (marker, cleanup)
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        kill_gateways(&self.marker);
        // Killed children can hold log handles for a beat; retry removal once
        // so an immediate failure does not leak the scratch dir.
        std::thread::sleep(Duration::from_millis(200));
        if std::fs::remove_dir_all(&self.data_dir).is_err() {
            std::thread::sleep(Duration::from_millis(800));
            let _ = std::fs::remove_dir_all(&self.data_dir);
        }
    }
}

/// Concatenate every *.log file under a directory (tracing rolling sink).
pub fn read_dir_logs_concat(dir: &Path) -> String {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter_map(|e| std::fs::read_to_string(e.path()).ok())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}
