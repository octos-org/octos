//! Integration test: `octos serve` must reap its gateway children on SIGTERM.
//!
//! #2086 (comment "Separate finding"): killing `octos serve` with SIGTERM —
//! what `pkill`/`systemctl stop`/supervisors send — left every auto-started
//! `octos gateway` child orphaned (38 accumulated over ~15 restart cycles in
//! the reporter's session), because the graceful-shutdown future only caught
//! SIGINT (`tokio::signal::ctrl_c`), so SIGTERM fell through to the OS
//! default disposition and `stop_all()` never ran. The fix installs a
//! stop-signal watcher BEFORE the gateway auto-start loop and routes either
//! signal into the same graceful-shutdown → `stop_all()` → exit(0) sequence
//! ctrl-c already used. These tests drive the REAL octos binary:
//!
//! - `serve_sigterm_reaps_gateway_children` — steady state (HTTP serving),
//!   one enabled profile → one spawned gateway, SIGTERM, assert the gateway
//!   dies with the parent.
//! - `serve_sigterm_during_startup_reaps_gateways` — SIGTERM landing inside
//!   the auto-start window (first of four gateways up, second still
//!   starting), which pins the handler-registration-before-auto-start
//!   ordering: a handler installed only inside axum's shutdown future would
//!   still orphan the gateways here — and that the loop stops STARTING the
//!   remaining profiles once the signal latches.
//!
//! The `serve` subcommand exists under the `api` feature (a default of
//! octos-cli). Process control needs libc setsid/kill — the module is
//! unix-only by design, mirroring `serve_broken_pipe`. The dedicated serial
//! CI step is:
//! `cargo test -p octos-cli --features api --test serve_sigterm -- --test-threads=1`
//! (the broad integration step skips it via `--skip serve_sigterm`).

#[cfg(unix)]
#[allow(unsafe_code)]
mod serve_sigterm {
    use std::process::{Command, Stdio};
    use std::sync::Mutex;

    /// Serve tests spawn real processes that contend for shared resources
    /// (model catalog, profile store) — serialize them.
    static SERIAL: Mutex<()> = Mutex::new(());

    fn serial_guard() -> std::sync::MutexGuard<'static, ()> {
        match SERIAL.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Path to a real octos binary that includes the `serve` subcommand.
    ///
    /// When this harness itself is compiled WITH the `api` feature, Cargo
    /// already built `CARGO_BIN_EXE_octos` with `serve` — reuse it directly.
    /// When compiled WITHOUT `api`, bootstrap one via `cargo build` so the
    /// spawned process is always the REAL octos binary with production code
    /// (same strategy as `serve_broken_pipe::octos_binary`).
    fn octos_binary() -> std::path::PathBuf {
        if cfg!(feature = "api") {
            return env!("CARGO_BIN_EXE_octos").into();
        }
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let target_dir = std::path::Path::new(manifest_dir).join("../../target/serve-sigterm");
        let bin = target_dir.join("debug/octos");
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

    /// Build a serve Command with a private instance data dir, mirroring
    /// `serve_broken_pipe::serve_command`. `pre_exec(setsid)` detaches the
    /// child from the test runner's process group so SIGTERM reaches the
    /// real octos process (not suppressed by shell job control).
    /// child.id() IS the real octos PID.
    fn serve_command(port: u16, data_dir: &std::path::Path) -> Command {
        let mut cmd = Command::new(octos_binary());
        cmd.args([
            "serve",
            "--instance-data-dir",
            data_dir.to_str().unwrap(),
            "--data-dir",
            data_dir.to_str().unwrap(),
            "--solo",
            "--danger-full-access",
            "-p",
            &port.to_string(),
        ])
        .stdin(Stdio::null())
        // The gateway child constructs its LLM provider at boot and exits
        // ("DEEPSEEK_API_KEY not set") when the env var is missing — the
        // profile family below is deepseek, so seed a dummy value. It only
        // needs to be non-empty; nothing connects anywhere before the parent
        // receives SIGTERM. Without it the gateway would crash-loop and the test
        // would at best sample a short-lived process instead of the stable
        // long-running child the #2086 orphan bug is about.
        .env("DEEPSEEK_API_KEY", "sigterm-e2e-dummy")
        // Bearer token for the SSE endpoint the third test subscribes to.
        // Harmless to the other scenarios (they never authenticate).
        .env("OCTOS_AUTH_TOKEN", "sigterm-e2e-token")
        // The host session may export OCTOS_INSTANCE_DATA_DIR (shared instance
        // lock). Remove it so the child uses ONLY our private --instance-data-dir.
        .env_remove("OCTOS_INSTANCE_DATA_DIR")
        .env_remove("OCTOS_HOME")
        .env_remove("OCTOS_DATA_DIR");
        #[cfg(unix)]
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

    /// PIDs of live `octos gateway` processes spawned for `marker` (the
    /// private data dir appears in the gateway's `--profile`/`--data-dir`
    /// argv). This is exactly the operator-visible orphan check from #2086
    /// (`pgrep -f 'octos gateway.*<data-dir>'`), so the assertion covers
    /// what a restarting supervisor would actually leave behind.
    fn gateway_orphan_pids(marker: &str) -> Vec<u32> {
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

    /// Last-resort cleanup so a failing assertion cannot leak gateway
    /// orphans into later CI steps.
    fn kill_orphans(marker: &str) {
        for pid in gateway_orphan_pids(marker) {
            unsafe {
                libc::kill(pid as i32, libc::SIGKILL);
            }
        }
    }

    /// Panic-safe cleanup: on ANY failure path (including asserts inside the
    /// scenario body) kill the serve child and any gateway orphans it left,
    /// and remove the scratch data dir, so a failing run cannot poison later
    /// CI steps. On the happy path the serve child has already exited (kill
    /// is then a no-op on a reaped pid).
    struct Cleanup {
        marker: String,
        data_dir: std::path::PathBuf,
        child: Option<std::process::Child>,
    }

    impl Drop for Cleanup {
        fn drop(&mut self) {
            if let Some(mut child) = self.child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
            kill_orphans(&self.marker);
            // Best-effort scratch-dir removal, with one retry: the gateway
            // children were killed a moment ago and their log handles can
            // take a beat to release, which makes a single immediate
            // remove_dir_all fail spuriously and leak the dir.
            std::thread::sleep(std::time::Duration::from_millis(200));
            if std::fs::remove_dir_all(&self.data_dir).is_err() {
                std::thread::sleep(std::time::Duration::from_millis(800));
                let _ = std::fs::remove_dir_all(&self.data_dir);
            }
        }
    }

    /// Assert every gateway for `marker` is gone — and stays gone. Two
    /// phases: first wait out the drain window for the processes to die,
    /// then keep sampling for 3s (longer than the 2s auto-restart sleep in
    /// process_manager) so a restart blip cannot fake a pass by landing
    /// between two samples.
    fn assert_gateways_gone_and_stay_gone(marker: &str, phase: &str) {
        let drain_deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut orphans = gateway_orphan_pids(marker);
        while !orphans.is_empty() && std::time::Instant::now() < drain_deadline {
            std::thread::sleep(std::time::Duration::from_millis(300));
            orphans = gateway_orphan_pids(marker);
        }
        assert!(
            orphans.is_empty(),
            "gateway children survived serve's SIGTERM shutdown ({phase}): {orphans:?} (the #2086 orphan bug)"
        );
        let settle_deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while std::time::Instant::now() < settle_deadline {
            std::thread::sleep(std::time::Duration::from_millis(300));
            orphans = gateway_orphan_pids(marker);
            assert!(
                orphans.is_empty(),
                "gateway children re-appeared after SIGTERM shutdown ({phase}): {orphans:?} — auto-restart blip"
            );
        }
    }

    /// Write `count` enabled deepseek profiles into `data_dir/profiles/`.
    /// Each carries a concrete LLM selection — the exact precondition of
    /// serve's auto-start loop (`enabled && config.has_llm_selection()`) —
    /// so boot spawns one gateway child per profile. The dummy
    /// DEEPSEEK_API_KEY seeded in `serve_command` lets each child finish
    /// provider construction and stay up; nothing ever contacts the network.
    fn write_enabled_profiles(data_dir: &std::path::Path, prefix: &str, count: usize) {
        for i in 0..count {
            let profile_id = format!("{prefix}-{i}");
            std::fs::write(
                data_dir.join("profiles").join(format!("{profile_id}.json")),
                format!(
                    r#"{{"id":"{profile_id}","name":"sigterm probe {i}","enabled":true,"config":{{"llm":{{"primary":{{"family_id":"deepseek","model_id":"deepseek-chat"}}}}}},"created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"}}"#
                ),
            )
            .unwrap();
        }
    }

    /// Wait for the serve child to exit and assert it exited via its
    /// shutdown path (exit code 0), not by signal.
    fn wait_for_clean_exit(child: &mut std::process::Child, data_dir: &std::path::Path) {
        let exit_deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut status = None;
        while std::time::Instant::now() < exit_deadline {
            match child.try_wait() {
                Ok(Some(s)) => {
                    status = Some(s);
                    break;
                }
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(200)),
                Err(e) => panic!("try_wait failed: {e}"),
            }
        }
        let stderr_log = std::fs::read_to_string(data_dir.join("stderr.log")).unwrap_or_default();
        let status = status.unwrap_or_else(|| {
            panic!("serve did not exit within 30s of SIGTERM; stderr:\n{stderr_log}")
        });
        #[cfg(unix)]
        let termsig = {
            use std::os::unix::process::ExitStatusExt;
            status.signal()
        };
        assert_eq!(
            status.code(),
            Some(0),
            "serve must run its shutdown path under SIGTERM (exit 0), not die by signal (signal={termsig:?}); stderr:\n{stderr_log}"
        );
    }

    /// Assert serve's tracing log recorded the `stop_all` cleanup, polling
    /// because the tracing writer's flush can trail process exit on a slow
    /// runner.
    fn assert_stop_all_logged(data_dir: &std::path::Path) {
        let log_dir = data_dir.join("logs");
        let marker_deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut tracing_log = read_dir_logs_concat(&log_dir);
        while !(tracing_log.contains("stopping all gateway child processes")
            || tracing_log.contains("gateways stopped"))
            && std::time::Instant::now() < marker_deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(200));
            tracing_log = read_dir_logs_concat(&log_dir);
        }
        assert!(
            tracing_log.contains("stopping all gateway child processes")
                || tracing_log.contains("gateways stopped"),
            "stop_all cleanup marker missing from tracing log (data_dir/logs), log:\n{tracing_log}"
        );
    }

    /// Open an SSE subscription on `/api/events/harness` and return the
    /// live stream. The caller must keep it alive for the connection to
    /// count as in-flight during the shutdown under test.
    fn open_sse_stream(port: u16) -> Option<std::net::TcpStream> {
        use std::io::{Read, Write};
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).ok()?;
        let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
        let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(2)));
        stream
            .write_all(
                b"GET /api/events/harness HTTP/1.1\r\nHost: localhost\r\n\
                  Authorization: Bearer sigterm-e2e-token\r\n\
                  Accept: text/event-stream\r\n\r\n",
            )
            .ok()?;
        // Read until the status line confirms the subscription is live.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        while std::time::Instant::now() < deadline {
            match stream.read(&mut byte) {
                Ok(1) => {
                    buf.push(byte[0]);
                    if buf.starts_with(b"HTTP/1.1 200") || buf.starts_with(b"HTTP/1.0 200") {
                        return Some(stream);
                    }
                    if buf.len() > 64 {
                        return None; // a status line arrived, but not the 200 we need
                    }
                }
                // Ok(0) is EOF; Ok(n>1) is impossible with a 1-byte buffer
                // but the Read signature permits it — treat both as failure.
                Ok(_) | Err(_) => return None,
            }
        }
        None
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

    /// Test: SIGTERM to `octos serve` must stop the auto-started gateway
    /// children before the process exits.
    ///
    /// Pre-fix behavior: SIGTERM hits the default disposition — the parent
    /// dies by signal (never reaching `stop_all`), the gateway keeps running
    /// (orphaned), and the tracing log never records the cleanup marker.
    #[test]
    fn serve_sigterm_reaps_gateway_children() {
        let _guard = serial_guard();
        let port = find_free_port();
        let data_dir = std::env::temp_dir().join(format!("octos_sigterm_{}", std::process::id()));
        std::fs::create_dir_all(data_dir.join("profiles")).unwrap();

        // One enabled profile → boot spawns exactly one gateway child.
        let profile_prefix = format!("sigterm-probe-{}", std::process::id());
        write_enabled_profiles(&data_dir, &profile_prefix, 1);

        let err_path = data_dir.join("stderr.log");
        let err_file = std::fs::File::create(&err_path).unwrap();
        let mut cmd = serve_command(port, &data_dir);
        cmd.stdout(Stdio::null()).stderr(Stdio::from(err_file));
        let child = cmd.spawn().expect("failed to start octos serve");

        let marker = data_dir.to_str().unwrap().to_string();
        let mut cleanup = Cleanup {
            marker: marker.clone(),
            data_dir: data_dir.clone(),
            child: Some(child),
        };
        run_sigterm_scenario(port, &data_dir, &marker, cleanup.child.as_mut().unwrap());
        // Happy path: serve already exited and was waited inside the scenario,
        // so detach it from the guard (killing a reaped pid is not just a
        // no-op — the pid could in theory be recycled). The guard still
        // sweeps any orphan that slipped through and removes the scratch dir.
        let _ = cleanup.child.take();
        drop(cleanup);
    }

    /// The test body, split out so the Drop guard above can clean up on any
    /// failure path without drowning the original panic message.
    fn run_sigterm_scenario(
        port: u16,
        data_dir: &std::path::Path,
        marker: &str,
        child: &mut std::process::Child,
    ) {
        // Boot barrier 1: the listener is bound.
        if !wait_for_port(port, std::time::Duration::from_secs(45)) {
            panic!("serve did not listen on {port} within 45s");
        }
        // Boot barrier 2: the auto-started gateway child is alive AND stable —
        // two samples 500ms apart, so the test proves a long-running gateway
        // is reaped, not that a crash-looping one happened to be down at the
        // end. The auto-start loop runs BEFORE axum::serve, so this also
        // implies the boot sequence is nearly done.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let mut gateways = Vec::new();
        while std::time::Instant::now() < deadline {
            if !gateway_orphan_pids(marker).is_empty() {
                std::thread::sleep(std::time::Duration::from_millis(500));
                gateways = gateway_orphan_pids(marker);
                if !gateways.is_empty() {
                    break;
                }
            } else {
                std::thread::sleep(std::time::Duration::from_millis(300));
            }
        }
        assert!(
            !gateways.is_empty(),
            "no stable `octos gateway` child appeared for {marker} — auto-start precondition broken"
        );

        // Boot barrier 3: an actual HTTP response byte proves axum::serve is
        // polling, i.e. boot is fully done and the steady state is reached.
        // The stop-signal watcher itself is registered much earlier (before
        // the auto-start loop — that ordering has its own test below); this
        // barrier pins the classic steady-state kill a supervisor performs on
        // a long-running serve.
        let http_deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut http_ready = false;
        while std::time::Instant::now() < http_deadline && !http_ready {
            if let Ok(mut stream) = std::net::TcpStream::connect(("127.0.0.1", port)) {
                use std::io::{Read, Write};
                // Bound the probe: if serve ever accepted a connection but
                // hung mid-response, a blocking read would stall this test
                // until the CI job timeout instead of failing fast.
                let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
                let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(2)));
                let _ = stream.write_all(b"GET /admin/ HTTP/1.0\r\nHost: localhost\r\n\r\n");
                let mut byte = [0u8; 1];
                http_ready = matches!(stream.read(&mut byte), Ok(1));
            }
            if !http_ready {
                std::thread::sleep(std::time::Duration::from_millis(300));
            }
        }
        assert!(http_ready, "serve never answered an HTTP request on {port}");
        // Handler-registration floor (same observed-value reasoning as
        // serve_broken_pipe #37): the response byte proves the future is
        // being polled; this floor absorbs scheduling jitter after it.
        std::thread::sleep(std::time::Duration::from_millis(200));

        // The act under test: SIGTERM, the signal `pkill`/`systemctl stop`
        // send. rc=0 also proves the parent was still alive to receive it.
        let rc = unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
        assert_eq!(rc, 0, "kill(SIGTERM) failed");

        // The parent must exit — by finishing its shutdown, not by signal.
        wait_for_clean_exit(child, data_dir);

        // The gateway children must be gone — and stay gone.
        assert_gateways_gone_and_stay_gone(marker, "steady state");

        // Cleanup evidence from a non-stdout sink: serve's rolling tracing
        // log under data_dir/logs must show stop_all ran.
        assert_stop_all_logged(data_dir);
    }

    /// Test: SIGTERM arriving DURING the gateway auto-start window must also
    /// reap the gateways — and must not keep STARTING the remaining ones.
    ///
    /// The stop-signal watcher is registered before the auto-start loop;
    /// each profile's gateway startup health check holds that loop for ~2s,
    /// so with four profiles the first gateway is already up while the
    /// second is still starting — exactly the window a restarting
    /// supervisor's SIGTERM can land in (the #2086 reporter's kill loop
    /// restarts serve every few seconds, so the kill and the boot overlap).
    /// Killing at "first gateway alive", before any HTTP byte, pins two
    /// properties at once: had the handler been installed only inside
    /// axum's graceful-shutdown future, this SIGTERM would hit the OS
    /// default disposition, exit by signal, and orphan every gateway; and
    /// without the latch check in the loop, profiles 3–4 would keep being
    /// started after the operator already asked to stop.
    #[test]
    fn serve_sigterm_during_startup_reaps_gateways() {
        let _guard = serial_guard();
        let port = find_free_port();
        let data_dir =
            std::env::temp_dir().join(format!("octos_sigterm_startup_{}", std::process::id()));
        std::fs::create_dir_all(data_dir.join("profiles")).unwrap();

        let profile_prefix = format!("sigterm-startup-{}", std::process::id());
        write_enabled_profiles(&data_dir, &profile_prefix, 4);

        let err_path = data_dir.join("stderr.log");
        let err_file = std::fs::File::create(&err_path).unwrap();
        let mut cmd = serve_command(port, &data_dir);
        cmd.stdout(Stdio::null()).stderr(Stdio::from(err_file));
        let child = cmd.spawn().expect("failed to start octos serve");

        let marker = data_dir.to_str().unwrap().to_string();
        let mut cleanup = Cleanup {
            marker: marker.clone(),
            data_dir: data_dir.clone(),
            child: Some(child),
        };
        run_startup_window_scenario(&marker, cleanup.child.as_mut().unwrap(), &data_dir);
        // Happy path: serve already exited and was waited inside the scenario.
        let _ = cleanup.child.take();
        drop(cleanup);
    }

    /// The startup-window scenario body (see the test above for why it
    /// exists). Split out for the same Drop-guard reason as
    /// `run_sigterm_scenario`.
    fn run_startup_window_scenario(
        marker: &str,
        child: &mut std::process::Child,
        data_dir: &std::path::Path,
    ) {
        // Boot barrier: the FIRST gateway child is alive and stable — two
        // samples 500ms apart. No HTTP barrier on purpose: the kill must
        // land while the second profile's startup check is still holding the
        // auto-start loop, i.e. before axum::serve is even polling. With the
        // fix, the signal watcher registered before the loop latches this
        // SIGTERM and the shutdown future observes it on its first poll.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let mut gateways = Vec::new();
        while std::time::Instant::now() < deadline {
            if !gateway_orphan_pids(marker).is_empty() {
                std::thread::sleep(std::time::Duration::from_millis(500));
                gateways = gateway_orphan_pids(marker);
                if !gateways.is_empty() {
                    break;
                }
            } else {
                std::thread::sleep(std::time::Duration::from_millis(300));
            }
        }
        assert!(
            !gateways.is_empty(),
            "no `octos gateway` child appeared for {marker} — auto-start precondition broken"
        );

        // The act under test: SIGTERM while the second profile is still
        // starting up.
        let rc = unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
        assert_eq!(rc, 0, "kill(SIGTERM) failed");

        // The in-flight startup check finishes (nothing interrupts it
        // mid-start), then the latched signal stops the loop and the
        // shutdown future fires immediately — well within the 30s window.
        wait_for_clean_exit(child, data_dir);

        // Every gateway that DID start must be gone — and stay gone.
        assert_gateways_gone_and_stay_gone(marker, "startup window");

        assert_stop_all_logged(data_dir);

        // The loop must not keep STARTING gateways after the signal latched:
        // of the four enabled profiles at most two "auto-starting gateway"
        // log lines may appear — the one in flight when the signal landed
        // plus, at worst, the next iteration if the kill slipped past one
        // startup check. Profiles 3–4 must never start. This also makes the
        // test fail loudly instead of passing vacuously if the kill ever
        // degraded past the whole auto-start window (all four started).
        // These early-boot lines land in the rolling log seconds before
        // exit, so they are long since flushed.
        let tracing_log = read_dir_logs_concat(&data_dir.join("logs"));
        let autostarts = tracing_log.matches("auto-starting gateway").count();
        assert!(
            (1..=2).contains(&autostarts),
            "auto-start loop started {autostarts}/4 gateways around the SIGTERM — \
             the latched-stop break is not bounding it; log:\n{tracing_log}"
        );
    }

    /// Test: SIGTERM with an open SSE stream must still terminate the serve
    /// within the drain cap and reap the gateway.
    ///
    /// Axum's graceful shutdown waits for in-flight connections, and an SSE
    /// stream (`GET /api/events/harness`) never ends on its own — without a
    /// cap the serve would hang until the supervisor escalates to SIGKILL,
    /// skipping `stop_all()` and re-creating the #2086 orphans. The fix
    /// bounds the drain after the signal (10s) and proceeds to the gateway
    /// cleanup anyway.
    ///
    /// The pre-SIGTERM wait also pins that the cap's clock starts at the
    /// SIGNAL, not at boot: a mis-wired deadline (e.g. wrapping
    /// `axum::serve` in a plain `tokio::time::timeout`) would kill a
    /// healthy, signal-free serve inside that window and the kill below
    /// would target a dead pid.
    #[test]
    fn serve_sigterm_with_open_sse_stream_still_reaps_gateways() {
        let _guard = serial_guard();
        let port = find_free_port();
        let data_dir =
            std::env::temp_dir().join(format!("octos_sigterm_sse_{}", std::process::id()));
        std::fs::create_dir_all(data_dir.join("profiles")).unwrap();

        let profile_prefix = format!("sigterm-sse-{}", std::process::id());
        write_enabled_profiles(&data_dir, &profile_prefix, 1);

        let err_path = data_dir.join("stderr.log");
        let err_file = std::fs::File::create(&err_path).unwrap();
        let mut cmd = serve_command(port, &data_dir);
        cmd.stdout(Stdio::null()).stderr(Stdio::from(err_file));
        let child = cmd.spawn().expect("failed to start octos serve");

        let marker = data_dir.to_str().unwrap().to_string();
        let mut cleanup = Cleanup {
            marker: marker.clone(),
            data_dir: data_dir.clone(),
            child: Some(child),
        };
        run_sse_scenario(port, &marker, cleanup.child.as_mut().unwrap(), &data_dir);
        // Happy path: serve already exited and was waited inside the scenario.
        let _ = cleanup.child.take();
        drop(cleanup);
    }

    /// The SSE scenario body (see the test above for why it exists). Split
    /// out for the same Drop-guard reason as `run_sigterm_scenario`.
    fn run_sse_scenario(
        port: u16,
        marker: &str,
        child: &mut std::process::Child,
        data_dir: &std::path::Path,
    ) {
        // Boot barrier: listener + HTTP response byte (the SSE handshake
        // needs a fully serving axum).
        if !wait_for_port(port, std::time::Duration::from_secs(45)) {
            panic!("serve did not listen on {port} within 45s");
        }
        let http_deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut http_ready = false;
        while std::time::Instant::now() < http_deadline && !http_ready {
            if let Ok(mut stream) = std::net::TcpStream::connect(("127.0.0.1", port)) {
                use std::io::{Read, Write};
                let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
                let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(2)));
                let _ = stream.write_all(b"GET /admin/ HTTP/1.0\r\nHost: localhost\r\n\r\n");
                let mut byte = [0u8; 1];
                http_ready = matches!(stream.read(&mut byte), Ok(1));
            }
            if !http_ready {
                std::thread::sleep(std::time::Duration::from_millis(300));
            }
        }
        assert!(http_ready, "serve never answered an HTTP request on {port}");

        // Outlive the drain cap (10s) with NO signal sent: a serve whose
        // deadline started at boot would exit on its own inside this window.
        let age_floor = std::time::Instant::now() + std::time::Duration::from_secs(12);
        while std::time::Instant::now() < age_floor {
            assert!(
                child.try_wait().expect("try_wait failed").is_none(),
                "serve exited before any signal — the drain cap must start at the signal, not at boot"
            );
            std::thread::sleep(std::time::Duration::from_millis(500));
        }

        // The gateway must be up by now (auto-start precedes axum); assert
        // it so the orphan check below cannot pass vacuously.
        let gateways = gateway_orphan_pids(marker);
        assert!(
            !gateways.is_empty(),
            "no `octos gateway` child appeared for {marker} — auto-start precondition broken"
        );

        // Subscribe an SSE stream and keep it open across the kill — this is
        // the in-flight connection graceful shutdown cannot wait out.
        let sse =
            open_sse_stream(port).expect("failed to open an SSE stream (auth or handshake broken)");

        // The act under test: SIGTERM with the stream open. The serve must
        // exit via its shutdown path (drain cap → stop_all → exit 0), not
        // hang and not die by signal.
        let rc = unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
        assert_eq!(rc, 0, "kill(SIGTERM) failed");

        // The cap gives 10s of drain + stop_all; comfortably inside 30s.
        wait_for_clean_exit(child, data_dir);

        // The gateway must still have been reaped despite the open stream.
        assert_gateways_gone_and_stay_gone(marker, "open SSE stream");

        assert_stop_all_logged(data_dir);
        drop(sse);
    }
}
