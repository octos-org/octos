//! Representative real-process coverage for BrokenPipe-safe shutdown.
//! One test first proves that EPIPE during shutdown still runs gateway cleanup
//! and exits cleanly, then runs a normal-output phase that pins graceful server
//! shutdown before gateway `stop_all`.
//!
//! Real run: `cargo test -p octos-cli --features api --test serve_broken_pipe -- --test-threads=1`.

// Process-control tests require libc pipe/kill/setsid — unsafe is inherent.
// #39 — the test module is unix-only BY ORIGINAL DESIGN (libc pipes,
// raw-fd Stdio, SIGINT/SIGKILL): the #37 hardening broke the cfg structure
// by leaving the imports and several `libc::` call sites outside any gate
// (E0432/E0433 on Windows). Gate the entire module so the file compiles on
// Windows with the SAME existence shape as before #37: no tests, no module.
// The module is named `serve_broken_pipe` so every test name carries the
// target name: the broad CI integration step excludes these
// process-spawning e2e with `--skip serve_broken_pipe` (they run in the
// dedicated serial step below it), and --skip matches test names, not
// target names — under the old `imp` name the filter silently matched
// nothing and the e2e leaked into the parallel step, flaking under load.
#[cfg(unix)]
#[path = "support/serve_process.rs"]
mod serve_process;

#[cfg(unix)]
#[allow(unsafe_code)]
mod serve_broken_pipe {
    use super::serve_process::{
        find_free_port, read_dir_logs_concat, serial_guard, serve_command as base_serve_command,
        wait_for_port,
    };
    use std::os::fd::FromRawFd;
    use std::process::{Command, Stdio};

    /// Add the broken-pipe scenario's solo/full-access flags to the base
    /// command (whose `setsid` makes `child.id()` the real octos PID).
    fn serve_command(port: u16, data_dir: &std::path::Path) -> Command {
        let mut cmd = base_serve_command(port, data_dir);
        cmd.args(["--solo", "--danger-full-access"]);
        cmd
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
        let _guard = serial_guard();
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
            panic!("serve did not listen on {port} within 45s");
        }

        // Banner barrier: read piped stdout until the banner appears…
        let banner = wait_for_pipe_contains(
            &mut stdout_reader,
            "octos API server",
            std::time::Duration::from_secs(10),
        );
        assert!(banner, "startup banner never arrived on the stdout pipe");
        // The banner is printed exactly once, before axum::serve runs, so a
        // second wait for the same text can only time out. The #2086 ordering
        // installs the stop-signal watcher before gateway auto-start; reaching
        // the port and banner barriers therefore proves registration has run.
        // Keep the existing bounded floor for the watcher task's first poll.
        std::thread::sleep(std::time::Duration::from_millis(200));

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
        // The log guard must flush before process exit. Polling after the
        // process has exited cannot recover records lost by process::exit.
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
            "serve must exit 0 under broken-pipe shutdown; code={code:?} signal={termsig:?} stderr:\n{stderr_log}"
        );
        assert!(
            tracing_log.contains("stopping all gateway child processes")
                || tracing_log.contains("gateways stopped"),
            "cleanup marker missing from tracing log (data_dir/logs), log:\n{tracing_log}\nstderr:\n{stderr_log}"
        );
        assert_shutdown_order_preserved();
    }

    /// The same representative test also proves shutdown order in a second,
    /// non-EPIPE phase: graceful axum shutdown precedes gateway `stop_all`.
    fn assert_shutdown_order_preserved() {
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
            panic!("serve did not listen on {port} within 45s");
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
            "serve should exit 0, stdout:\n{stdout_log}\nstderr:\n{stderr_log}"
        );
        // ORDER assertion: graceful marker BEFORE stop_all marker.
        let shutdown_pos = stdout_log.find("Shutting down server...");
        let stop_pos = stdout_log.find("Stopping gateways...");
        assert!(
            shutdown_pos.is_some(),
            "graceful marker missing:\n{stdout_log}"
        );
        assert!(stop_pos.is_some(), "stop_all marker missing:\n{stdout_log}");
        assert!(
            shutdown_pos.unwrap() < stop_pos.unwrap(),
            "shutdown order violated: 'Shutting down' must precede 'Stopping gateways':\n{stdout_log}"
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
}
