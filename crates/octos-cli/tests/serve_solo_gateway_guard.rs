//! Integration test: solo in-process serve must refuse manual gateway starts.
//!
//! #2546: serve gates gateway AUTO-start on `!solo_in_process` — a gateway
//! child for a solo-hosted profile opens its episodes.redb a second time and
//! locks `session/open` out of the profile — but every MANUAL start path
//! (`POST /api/admin/profiles/:id/start|restart`,
//! `POST /api/my/profile/start|restart`) funneled into
//! `ProcessManager::start()` unguarded. One click on the dashboard's Start
//! button (the Telegram tab's only way to run a bot) reproduced the exact
//! `data_dir_locked` lock on the UPCR-018 first-user path.
//!
//! The guard now hangs on `ProcessManager::start()` itself, and these tests
//! drive the REAL octos binary over HTTP:
//!
//! - `solo_serve_refuses_manual_gateway_starts` — with `--solo`, the admin
//!   start/restart routes and the self-service start/restart/sub-account
//!   routes all refuse with the in-process-ownership explanation, and no
//!   `octos gateway` child ever appears.
//! - `non_solo_serve_still_starts_gateways_manually` — without `--solo`, the
//!   same admin start route spawns the gateway exactly as before, proving
//!   the guard does not over-block.
//!
//! Same serial-step discipline as `serve_sigterm` (the #21 outer-loop
//! ruling): these tests spawn real `octos serve` children that contend for
//! model catalog / profile store resources, so the broad integration lane
//! skips them and CI runs them serially.

#[cfg(unix)]
#[path = "support/serve_process.rs"]
mod serve_process;

#[cfg(unix)]
mod serve_solo_gateway_guard {
    use super::serve_process::{
        Cleanup, find_free_port, gateway_pids, serial_guard, serve_command as base_serve_command,
        startup_diagnostics, wait_for_port,
    };
    use std::io::{Read, Write};
    use std::process::{Command, Stdio};

    /// Add the solo flag and guard-specific environment to the base command.
    fn serve_command(port: u16, data_dir: &std::path::Path, solo: bool) -> Command {
        let mut cmd = base_serve_command(port, data_dir);
        if solo {
            cmd.arg("--solo");
        }
        cmd
            // The admin/self-service start routes validate that the profile
            // carries an LLM selection before reaching the guard, so the
            // profiles below are deepseek-family. The gateway child would
            // want this env at boot; the solo scenario never gets that far
            // and the non-solo scenario only needs the child to appear.
            .env("DEEPSEEK_API_KEY", "solo-guard-e2e-dummy")
            // Bootstrap admin token: with no admin_token.json rotation, this
            // grants `AuthIdentity::Admin` for the admin routes and resolves
            // the self-service `my` routes to the `admin` profile.
            .env("OCTOS_AUTH_TOKEN", "solo-guard-e2e-token")
            // The solo condition also rides this env var; scrub it so a
            // developer shell exporting it cannot flip the non-solo scenario.
            .env_remove("OCTOS_SOLO_LOGIN");
        cmd
    }

    /// Write one profile JSON into `data_dir/profiles/`. `enabled` controls
    /// the auto-start loop's filter (it starts only enabled profiles), which
    /// lets the non-solo scenario isolate the MANUAL start route onto a
    /// disabled profile. `parent` wires a sub-account profile to its owner
    /// for the sub-account start route.
    fn write_profile(data_dir: &std::path::Path, id: &str, enabled: bool, parent: Option<&str>) {
        let parent_json = parent
            .map(|p| format!(r#","parent_id":"{p}""#))
            .unwrap_or_default();
        std::fs::write(
            data_dir.join("profiles").join(format!("{id}.json")),
            format!(
                r#"{{"id":"{id}","name":"solo guard probe {id}","enabled":{enabled}{parent_json},"config":{{"llm":{{"primary":{{"family_id":"deepseek","model_id":"deepseek-chat"}}}}}},"created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"}}"#
            ),
        )
        .unwrap();
    }

    /// Minimal HTTP/1.1 POST with the bootstrap bearer token; returns
    /// `(status_code, body)`.
    fn http_post(port: u16, path: &str) -> (u16, String) {
        let mut stream =
            std::net::TcpStream::connect(("127.0.0.1", port)).expect("serve port reachable");
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        stream
            .set_write_timeout(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        write!(
            stream,
            "POST {path} HTTP/1.1\r\nHost: localhost\r\n\
             Authorization: Bearer solo-guard-e2e-token\r\n\
             Content-Length: 0\r\n\
             Connection: close\r\n\r\n"
        )
        .unwrap();
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).unwrap();
        let text = String::from_utf8_lossy(&buf).into_owned();
        let status: u16 = text
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| panic!("no status line in response: {text}"));
        let body = text
            .split_once("\r\n\r\n")
            .map(|(_, body)| body.to_string())
            .unwrap_or_default();
        (status, body)
    }

    /// Poll until `pred` holds or `timeout` elapses; returns the last value.
    fn poll_until(timeout: std::time::Duration, mut pred: impl FnMut() -> bool) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if pred() {
                return true;
            }
            if std::time::Instant::now() > deadline {
                return pred();
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
    }

    /// #2546, the solo half: `--solo` serve must refuse every manual
    /// gateway-start route, and no gateway child may appear.
    ///
    /// Pre-fix behavior: the routes reported success and spawned a gateway
    /// child that would take the profile's single-writer data-dir lock away
    /// from the in-process runtime — the exact `data_dir_locked` lock on the
    /// first-user path.
    #[test]
    fn solo_serve_refuses_manual_gateway_starts() {
        let _guard = serial_guard();
        let port = find_free_port();
        let data_dir =
            std::env::temp_dir().join(format!("octos_solo_guard_{}", std::process::id()));
        std::fs::create_dir_all(data_dir.join("profiles")).unwrap();

        // An enabled profile with an LLM selection: auto-start already skips
        // it in solo mode, so ANY gateway for it can only come from the
        // manual routes under test. The second profile services the
        // self-service `/api/my/*` routes, which resolve the bootstrap admin
        // identity to the fixed `admin` profile; the third is its sub-account
        // for `/api/my/profile/accounts/:id/start`.
        write_profile(&data_dir, "solo-guard-0", true, None);
        write_profile(&data_dir, "admin", true, None);
        write_profile(&data_dir, "admin-sub-0", true, Some("admin"));

        let err_path = data_dir.join("stderr.log");
        let err_file = std::fs::File::create(&err_path).unwrap();
        let mut cmd = serve_command(port, &data_dir, true);
        cmd.stdout(Stdio::null()).stderr(Stdio::from(err_file));
        let child = cmd.spawn().expect("failed to start octos serve");

        let (marker, mut cleanup) = Cleanup::with_child(child, &data_dir);

        assert!(
            wait_for_port(port, std::time::Duration::from_secs(45)),
            "serve did not listen on {port} within 45s\n{}",
            startup_diagnostics(&data_dir, port, &marker)
        );

        // Admin manual start → refused with the in-process explanation.
        let (status, body) = http_post(port, "/api/admin/profiles/solo-guard-0/start");
        assert_eq!(
            status, 409,
            "admin start must be refused on a solo serve; body: {body}"
        );
        assert!(
            body.contains("solo serve hosts profiles in-process"),
            "refusal must explain the in-process ownership; body: {body}"
        );

        // Self-service manual start (the dashboard Start button's route) →
        // refused the same way. The route reports via `ok:false`, not HTTP
        // status.
        let (status, body) = http_post(port, "/api/my/profile/start");
        assert_eq!(status, 200, "route must respond; body: {body}");
        assert!(
            body.contains("\"ok\":false") && body.contains("solo serve hosts profiles in-process"),
            "self-service start must be refused on a solo serve; body: {body}"
        );

        // Self-service restart delegates to the same guarded start.
        let (status, body) = http_post(port, "/api/my/profile/restart");
        assert_eq!(status, 200, "route must respond; body: {body}");
        assert!(
            body.contains("\"ok\":false") && body.contains("solo serve hosts profiles in-process"),
            "self-service restart must be refused on a solo serve; body: {body}"
        );

        // Self-service sub-account start resolves through the parent guard.
        let (status, body) = http_post(port, "/api/my/profile/accounts/admin-sub-0/start");
        assert_eq!(status, 200, "route must respond; body: {body}");
        assert!(
            body.contains("\"ok\":false") && body.contains("solo serve hosts profiles in-process"),
            "sub-account start must be refused on a solo serve; body: {body}"
        );

        // Admin restart delegates to the same guarded start → refused.
        let (status, body) = http_post(port, "/api/admin/profiles/solo-guard-0/restart");
        assert_eq!(
            status, 500,
            "admin restart must be refused on a solo serve; body: {body}"
        );
        assert!(
            body.contains("solo serve hosts profiles in-process"),
            "refusal must explain the in-process ownership; body: {body}"
        );

        // The core assertion: no gateway child may appear for any of the
        // refused starts — that child is what takes the data-dir lock.
        assert!(
            poll_until(std::time::Duration::from_secs(5), || {
                gateway_pids(&marker).is_empty()
            }),
            "gateway child spawned despite the solo refusal — the #2546 lock trap"
        );

        let _ = cleanup.child.take();
        drop(cleanup);
    }

    /// Regression half: without `--solo`, the manual start route must keep
    /// spawning gateways. A DISABLED profile isolates the manual route —
    /// the auto-start loop skips disabled profiles, so any gateway for it
    /// can only come from the route under test.
    #[test]
    fn non_solo_serve_still_starts_gateways_manually() {
        let _guard = serial_guard();
        let port = find_free_port();
        let data_dir =
            std::env::temp_dir().join(format!("octos_solo_manual_{}", std::process::id()));
        std::fs::create_dir_all(data_dir.join("profiles")).unwrap();
        write_profile(&data_dir, "manual-only-0", false, None);

        let err_path = data_dir.join("stderr.log");
        let err_file = std::fs::File::create(&err_path).unwrap();
        let mut cmd = serve_command(port, &data_dir, false);
        cmd.stdout(Stdio::null()).stderr(Stdio::from(err_file));
        let child = cmd.spawn().expect("failed to start octos serve");

        let (marker, mut cleanup) = Cleanup::with_child(child, &data_dir);

        assert!(
            wait_for_port(port, std::time::Duration::from_secs(45)),
            "serve did not listen on {port} within 45s\n{}",
            startup_diagnostics(&data_dir, port, &marker)
        );
        // The disabled profile is never auto-started: no gateway before the
        // manual call.
        assert!(
            poll_until(std::time::Duration::from_secs(3), || {
                gateway_pids(&marker).is_empty()
            }),
            "disabled profile was auto-started — test isolation broken"
        );

        let (status, body) = http_post(port, "/api/admin/profiles/manual-only-0/start");
        assert_eq!(
            status, 200,
            "manual start must keep working without --solo; body: {body}"
        );
        assert!(
            body.contains("started"),
            "expected the started confirmation; body: {body}"
        );

        assert!(
            poll_until(std::time::Duration::from_secs(20), || {
                !gateway_pids(&marker).is_empty()
            }),
            "manual start did not spawn the gateway child"
        );

        let _ = cleanup.child.take();
        drop(cleanup);
    }
}
