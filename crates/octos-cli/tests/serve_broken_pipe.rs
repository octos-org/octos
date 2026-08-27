//! Integration tests for octos serve BrokenPipe-safe shutdown.
//!
//! These tests verify the four integration selectors from
//! `specs/task-s-broken-pipe-shutdown.spec.md`:
//! 1. subprocess_panic_stderr_broken_pipe_no_abort
//! 2. serve_shutdown_broken_pipe_cleanup_marker_observed
//! 3. serve_shutdown_order_preserved
//! 4. serve_startup_broken_pipe_no_panic
//!
//! The `serve` subcommand only exists under the `api` feature. Without it the
//! whole harness compiles to zero tests (feature gate, NOT `#[ignore]`), so
//! feature-less invocations — including agent-spec lifecycle's plain
//! `cargo test -p octos-cli --test serve_broken_pipe` — report ok/0 rather
//! than fail. The real run is the dedicated serial CI step:
//! `cargo test -p octos-cli --features api --test serve_broken_pipe -- --test-threads=1`

// Process-control tests require libc pipe/kill/setsid — unsafe is inherent.
#[allow(unsafe_code)]
mod imp {
    use std::os::fd::FromRawFd;
    use std::process::{Command, Stdio};
    use std::sync::Mutex;

    /// Serve tests spawn real processes that contend for shared resources
    /// (model catalog, profile store) — serialize them.
    static SERIAL: Mutex<()> = Mutex::new(());

    /// Path to a real octos binary that includes the `serve` subcommand.
    ///
    /// When this harness itself is compiled WITH the `api` feature, Cargo
    /// already built `CARGO_BIN_EXE_octos` with `serve` — reuse it directly.
    /// When compiled WITHOUT `api` (e.g. agent-spec lifecycle's plain
    /// `cargo test -p octos-cli --test serve_broken_pipe`), that binary has
    /// no `serve` subcommand, so we bootstrap one: `cargo build` an api
    /// binary into a dedicated target dir next to the manifest. Either way
    /// the spawned process is the REAL octos binary with production code.
    fn octos_binary() -> std::path::PathBuf {
        if cfg!(feature = "api") {
            return env!("CARGO_BIN_EXE_octos").into();
        }
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let target_dir = std::path::Path::new(manifest_dir).join("../../target/serve-bp-probe");
        let bin = target_dir.join("debug/octos");
        // ALWAYS run cargo build: Cargo's own incremental freshness check
        // guarantees the binary matches the CURRENT sources (a cached binary
        // that predates a source/HEAD change would produce fake-green runs —
        // outer-loop rejection ③). A no-op build returns in milliseconds.
        let out = std::process::Command::new("cargo")
            .args(["build", "-p", "octos-cli", "--features", "api"])
            .current_dir(std::path::Path::new(manifest_dir).join("../.."))
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

    /// Build a serve Command with a private instance data dir.
    /// `pre_exec(setsid)` detaches the child from the test runner's process
    /// group so SIGINT reaches the real octos process (not suppressed by
    /// shell job control). child.id() IS the real octos PID.
    fn serve_command(port: u16, data_dir: &std::path::Path) -> Command {
        let mut cmd = Command::new(octos_binary());
        cmd.args([
            "serve",
            "--instance-data-dir",
            data_dir.to_str().unwrap(),
            // main.rs derives the tracing rolling-log dir from --data-dir;
            // pin it to the SAME private dir so the test can assert on
            // data_dir/logs without touching the shared instance.
            "--data-dir",
            data_dir.to_str().unwrap(),
            "--solo",
            "--danger-full-access",
            "-p",
            &port.to_string(),
        ])
        .stdin(Stdio::null())
        // The host session may export OCTOS_INSTANCE_DATA_DIR (shared instance
        // lock). Remove it so the child uses ONLY our private --instance-data-dir.
        .env_remove("OCTOS_INSTANCE_DATA_DIR")
        .env_remove("OCTOS_HOME")
        .env_remove("OCTOS_DATA_DIR");
        #[cfg(unix)]
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(|| {
                // Detach from the parent's process group/session so signals
                // from the test harness are delivered directly to octos.
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        cmd
    }

    /// Test 1: subprocess_panic_stderr_broken_pipe_no_abort
    ///
    /// Drive the REAL octos binary with OCTOS_TEST_PANIC_AFTER_BOOT=1, which
    /// triggers a genuine Rust panic AFTER the production hooks are
    /// installed in `main`. stderr is a pipe whose read end is closed
    /// (EPIPE). The production hook (write_panic_report via install_error_hooks)
    /// must swallow BrokenPipe: no second panic, no SIGABRT — exit code 101
    /// (standard Rust panic exit), not a signal death.
    #[test]
    fn subprocess_panic_stderr_broken_pipe_no_abort() {
        let mut fds: [i32; 2] = [0; 2];
        unsafe {
            libc::pipe(fds.as_mut_ptr());
            libc::close(fds[0]); // Close read end → writes get EPIPE
        }

        let output = Command::new(octos_binary())
            .env("OCTOS_TEST_PANIC_AFTER_BOOT", "1")
            .stderr(unsafe { Stdio::from_raw_fd(fds[1]) })
            .stdout(Stdio::null())
            .status()
            .expect("failed to run octos with OCTOS_TEST_PANIC_AFTER_BOOT=1");

        let code = output.code().unwrap_or(-1);
        // Real panic → standard Rust exit code 101. A signal death
        // (SIGABRT=134/SIGPIPE=141 double-panic→abort) yields None → -1.
        assert_ne!(
            code, -1,
            "octos died by signal: production panic hook double-panicked/aborted on broken-pipe stderr"
        );
        assert_ne!(code, 134, "process should not abort with SIGABRT");
        assert_ne!(code, 141, "process should not die on SIGPIPE");
        assert_eq!(code, 101, "real panic should exit with Rust panic code 101");
    }

    /// Test 2: serve_shutdown_broken_pipe_cleanup_marker_observed
    ///
    /// REAL broken-pipe shutdown: serve's stdout is a pipe whose read end we
    /// hold; after the startup banner and SIGINT we CLOSE the read end, so
    /// every subsequent shutdown println ("Shutting down server...", "Stopping
    /// gateways...") writes into EPIPE. The console helper must swallow it,
    /// `stop_all` must still run (proof: the tracing rolling log under
    /// data_dir/logs contains "stopping all gateway child processes"), and
    /// the process must exit 0 — not SIGABRT/SIGPIPE.
    #[test]
    fn serve_shutdown_broken_pipe_cleanup_marker_observed() {
        let _guard = SERIAL.lock().unwrap();
        let port = find_free_port();
        let data_dir = std::env::temp_dir().join(format!("octos_bp_{}", std::process::id()));
        std::fs::create_dir_all(&data_dir).unwrap();

        // stdout = REAL pipe (read end held by this test); stderr = file for
        // diagnostics. The tracing rolling sink lives under data_dir/logs.
        let err_path = data_dir.join("stderr.log");
        let err_file = std::fs::File::create(&err_path).unwrap();
        let mut pipe_fds: [i32; 2] = [0; 2];
        unsafe {
            libc::pipe(pipe_fds.as_mut_ptr());
        }
        let mut cmd = serve_command(port, &data_dir);
        cmd.stdout(unsafe { Stdio::from_raw_fd(pipe_fds[1]) })
            .stderr(Stdio::from(err_file));

        let mut child = cmd.spawn().expect("failed to start octos serve");
        // Take ownership of the read end; from_raw_fd owned the write end.
        let mut stdout_reader = unsafe { std::fs::File::from_raw_fd(pipe_fds[0]) };
        let ready = wait_for_port(port, std::time::Duration::from_secs(45));
        if !ready {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_dir_all(&data_dir);
            panic!("serve did not listen on {} within 45s", port);
        }

        // Banner barrier: read piped stdout until the banner appears…
        let banner = wait_for_pipe_contains(
            &mut stdout_reader,
            "octos API server",
            std::time::Duration::from_secs(10),
        );
        assert!(banner, "startup banner never arrived on the stdout pipe");
        // …but the banner is printed BEFORE axum::serve runs, and tokio's
        // ctrl_c() handler is only registered once the graceful-shutdown
        // future is polled inside axum::serve. A settle wait lets that
        // happen; without it an early SIGINT hits the default disposition
        // and kills the process (observed: signal=Some(2)).
        std::thread::sleep(std::time::Duration::from_millis(1500));

        // SIGINT, then IMMEDIATELY close the read end: every shutdown-time
        // console write now hits a genuinely broken pipe (EPIPE).
        let pid = child.id() as i32;
        let rc = unsafe { libc::kill(pid, libc::SIGINT) };
        assert_eq!(rc, 0, "kill(SIGINT) failed");
        drop(stdout_reader); // read end gone → EPIPE for child stdout writes

        let status = child.wait().expect("failed to wait for serve");
        let stderr_log = std::fs::read_to_string(&err_path).unwrap_or_default();
        // Cleanup evidence must come from a NON-stdout sink: serve's rolling
        // tracing log under data_dir/logs (created by init_tracing).
        let log_dir = data_dir.join("logs");
        let tracing_log = read_dir_logs_concat(&log_dir);
        let orphaned = unsafe { libc::kill(pid, 0) } == 0;
        let _ = std::fs::remove_dir_all(&data_dir);

        let code = status.code();
        #[cfg(unix)]
        let termsig = {
            use std::os::unix::process::ExitStatusExt;
            status.signal()
        };
        #[cfg(not(unix))]
        let termsig = None;
        assert!(!orphaned, "octos should not be orphaned");
        assert_eq!(
            code,
            Some(0),
            "serve must exit 0 under broken-pipe shutdown; code={:?} signal={:?} stderr:\n{}",
            code,
            termsig,
            stderr_log
        );
        assert!(
            tracing_log.contains("stopping all gateway child processes")
                || tracing_log.contains("gateways stopped"),
            "cleanup marker missing from tracing log (data_dir/logs), log:\n{}\nstderr:\n{}",
            tracing_log,
            stderr_log
        );
    }

    /// Test 3: serve_shutdown_order_preserved
    ///
    /// Order: "Shutting down server..." (axum graceful) must appear BEFORE
    /// "Stopping gateways..." (stop_all) in the shutdown output.
    #[test]
    fn serve_shutdown_order_preserved() {
        let _guard = SERIAL.lock().unwrap();
        let port = find_free_port();
        let data_dir = std::env::temp_dir().join(format!("octos_ord_{}", std::process::id()));
        std::fs::create_dir_all(&data_dir).unwrap();

        let out_path = data_dir.join("stdout.log");
        let err_path = data_dir.join("stderr.log");
        let out_file = std::fs::File::create(&out_path).unwrap();
        let err_file = std::fs::File::create(&err_path).unwrap();
        let mut cmd = serve_command(port, &data_dir);
        cmd.stdout(Stdio::from(out_file))
            .stderr(Stdio::from(err_file));

        let mut child = cmd.spawn().expect("failed to start octos serve");
        let ready = wait_for_port(port, std::time::Duration::from_secs(45));
        if !ready {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_dir_all(&data_dir);
            panic!("serve did not listen on {} within 45s", port);
        }

        let banner = wait_for_file_contains(
            &out_path,
            "octos API server",
            std::time::Duration::from_secs(10),
        );
        assert!(banner, "startup banner never reached stdout file");

        let pid = child.id() as i32;
        let rc = unsafe { libc::kill(pid, libc::SIGINT) };
        assert_eq!(rc, 0, "kill(SIGINT) failed");

        let status = child.wait().expect("failed to wait for serve");
        let stdout_log = std::fs::read_to_string(&out_path).unwrap_or_default();
        let stderr_log = std::fs::read_to_string(&err_path).unwrap_or_default();
        let _ = std::fs::remove_dir_all(&data_dir);

        let code = status.code().unwrap_or(-1);
        assert_eq!(
            code, 0,
            "serve should exit 0, stdout:\n{}\nstderr:\n{}",
            stdout_log, stderr_log
        );
        // ORDER assertion: graceful marker BEFORE stop_all marker.
        let shutdown_pos = stdout_log.find("Shutting down server...");
        let stop_pos = stdout_log.find("Stopping gateways...");
        assert!(
            shutdown_pos.is_some(),
            "graceful marker missing:\n{}",
            stdout_log
        );
        assert!(
            stop_pos.is_some(),
            "stop_all marker missing:\n{}",
            stdout_log
        );
        assert!(
            shutdown_pos.unwrap() < stop_pos.unwrap(),
            "shutdown order violated: 'Shutting down' must precede 'Stopping gateways':\n{}",
            stdout_log
        );
    }

    /// Test 4: serve_startup_broken_pipe_no_panic
    ///
    /// serve with a broken stdout pipe must survive startup (no panic) and
    /// keep listening.
    #[test]
    fn serve_startup_broken_pipe_no_panic() {
        let _guard = SERIAL.lock().unwrap();
        let port = find_free_port();
        let data_dir = std::env::temp_dir().join(format!("octos_start_{}", std::process::id()));
        std::fs::create_dir_all(&data_dir).unwrap();

        let mut fds: [i32; 2] = [0; 2];
        unsafe {
            libc::pipe(fds.as_mut_ptr());
            libc::close(fds[0]); // read end closed → writes get EPIPE
        }

        let mut cmd = serve_command(port, &data_dir);
        cmd.stdout(unsafe { Stdio::from_raw_fd(fds[1]) })
            .stderr(Stdio::piped());

        let mut child = cmd.spawn().expect("failed to start octos serve");

        // The port must come up despite stdout being a broken pipe — the
        // startup printlns hit EPIPE and must be swallowed (no panic).
        let ready = wait_for_port(port, std::time::Duration::from_secs(45));
        let running = child.try_wait().expect("check status").is_none();

        unsafe {
            libc::kill(child.id() as i32, libc::SIGKILL);
        }
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&data_dir);

        assert!(running, "serve died during startup with broken stdout");
        assert!(
            ready,
            "serve did not listen on {} within 45s with broken stdout",
            port
        );
    }

    /// Read (non-blocking) from a pipe File until it contains `needle` or
    /// the deadline passes. Uses poll(2) so we never block forever.
    fn wait_for_pipe_contains(
        pipe: &mut std::fs::File,
        needle: &str,
        timeout: std::time::Duration,
    ) -> bool {
        use std::io::Read;
        use std::os::fd::AsRawFd;
        let fd = pipe.as_raw_fd();
        let mut seen = String::new();
        let mut buf = [0u8; 4096];
        let start = std::time::Instant::now();
        loop {
            if seen.contains(needle) {
                return true;
            }
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let rc = unsafe { libc::poll(&mut pfd, 1, 200) };
            if rc > 0 && (pfd.revents & libc::POLLIN) != 0 {
                match pipe.read(&mut buf) {
                    Ok(0) => return seen.contains(needle), // EOF
                    Ok(n) => seen.push_str(&String::from_utf8_lossy(&buf[..n])),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(_) => return seen.contains(needle),
                }
            }
            if start.elapsed() > timeout {
                return seen.contains(needle);
            }
        }
    }

    /// Concatenate every *.log file under a directory (tracing rolling sink).
    fn read_dir_logs_concat(dir: &std::path::Path) -> String {
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

    /// Wait until a file contains the given substring.
    fn wait_for_file_contains(
        path: &std::path::Path,
        needle: &str,
        timeout: std::time::Duration,
    ) -> bool {
        let start = std::time::Instant::now();
        loop {
            if let Ok(content) = std::fs::read_to_string(path) {
                if content.contains(needle) {
                    return true;
                }
            }
            if start.elapsed() > timeout {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    /// Wait for a port to accept TCP connections.
    fn wait_for_port(port: u16, timeout: std::time::Duration) -> bool {
        let start = std::time::Instant::now();
        loop {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return true;
            }
            if start.elapsed() > timeout {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }

    /// Find a free port by binding to port 0.
    fn find_free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }
}
