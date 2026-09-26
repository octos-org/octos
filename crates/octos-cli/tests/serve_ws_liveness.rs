//! Integration test: `octos serve` must close half-open WS connections.
//!
//! #2447: the AppUI WebSocket heartbeat was send-only — the writer task
//! shipped its 20 s text `server/heartbeat` with no pong requirement and the
//! read loop had no inbound deadline. A half-open socket (NAT timeout, killed
//! client that never got to send Close) therefore sat in the read loop
//! forever while its live forwarders — and every session the AppUI keepalive
//! renews for them — stayed pinned against the idle sweep.
//!
//! The fix ships a protocol-level binary `Ping` on the keepalive tick and
//! closes the connection once `WS_LIVENESS_MISSED_PINGS + 1` ping intervals
//! pass with zero inbound frames (any frame counts: a conforming client's
//! Pong alone proves the peer is alive). These tests drive the REAL octos
//! binary with the cadence shortened via `OCTOS_WS_LIVENESS_PING_SECS=1`
//! (deadline = 4 × 1 s = 4 s):
//!
//! - `serve_ws_liveness_closes_fully_silent_connection` — a client that
//!   completes the upgrade over raw TCP and then never writes a byte (no
//!   Pong can exist, the true half-open shape) receives the server's binary
//!   Pings and then a real Close frame within the deadline.
//! - `serve_ws_liveness_keeps_ponging_client_open_past_deadline` — a client
//!   that reads and answers every `Ping` with a `Pong` (what browsers do at
//!   the protocol layer and octoscode's transport does explicitly) is still
//!   receiving Pings well past the deadline: the deadline counts inbound
//!   frames, not application traffic.
//!
//! The silent client must be raw TCP on purpose: `tokio-tungstenite` answers
//! a Ping as a side effect of polling the stream, so a library client can
//! never reproduce the silent shape. Process-spawning e2e runs in the
//! dedicated serial CI step next to `serve_broken_pipe` / `serve_sigterm`;
//! the broad integration lanes skip these tests via `--skip serve_ws_liveness`.

#![cfg(all(unix, feature = "api"))]

mod serve_ws_liveness {
    use futures::{SinkExt, StreamExt};
    use std::process::{Child, Command, Stdio};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message;

    /// Serve tests spawn real processes that contend for shared resources
    /// (model catalog, profile store) — serialize them. Async-aware so the
    /// guard can be held across the test's await points.
    static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    async fn serial_guard() -> tokio::sync::MutexGuard<'static, ()> {
        SERIAL.lock().await
    }

    /// `OCTOS_WS_LIVENESS_PING_SECS` handed to the serve child: a 1 s cadence
    /// puts the close deadline (`WS_LIVENESS_MISSED_PINGS + 1` intervals) at
    /// 4 s so both scenarios finish in seconds.
    const PING_SECS: u64 = 1;
    const DEADLINE: Duration = Duration::from_secs(4 * PING_SECS);
    const AUTH_TOKEN: &str = "ws-liveness-e2e-token";

    struct ServeProcess {
        child: Child,
        port: u16,
        // Holds the private data dir until the child is gone.
        _data_dir: tempfile::TempDir,
    }

    impl Drop for ServeProcess {
        fn drop(&mut self) {
            // Runs on the success path AND on assertion panics — a leaked
            // serve would hold the port and the temp dir past the test.
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    /// Spawn the REAL `octos serve` binary with a private data dir and the
    /// shortened liveness cadence.
    fn spawn_serve() -> ServeProcess {
        let port = find_free_port();
        let data_dir = tempfile::TempDir::new().unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_octos"))
            .args([
                "serve",
                "--instance-data-dir",
                data_dir.path().to_str().unwrap(),
                "--data-dir",
                data_dir.path().to_str().unwrap(),
                "--solo",
                "--danger-full-access",
                "-p",
                &port.to_string(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .env("OCTOS_WS_LIVENESS_PING_SECS", PING_SECS.to_string())
            .env("OCTOS_AUTH_TOKEN", AUTH_TOKEN)
            .env_remove("OCTOS_INSTANCE_DATA_DIR")
            .env_remove("OCTOS_HOME")
            .env_remove("OCTOS_DATA_DIR")
            .spawn()
            .expect("failed to spawn octos serve");
        ServeProcess {
            child,
            port,
            _data_dir: data_dir,
        }
    }

    fn find_free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    async fn wait_for_port(port: u16) {
        // 45 s barrier, matching the family: on loaded runners the serve
        // child can take tens of seconds to open its port (see the #21
        // outer-loop comment in ci.yml).
        let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
        while tokio::time::Instant::now() < deadline {
            if tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_ok()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        panic!("serve never opened port {port}");
    }

    /// One parsed WebSocket frame from the raw client: just what the
    /// assertions need.
    struct RawFrame {
        opcode: u8,
        payload: Vec<u8>,
    }

    impl RawFrame {
        const OP_PING: u8 = 0x9;
        const OP_CLOSE: u8 = 0x8;
    }

    /// Read one unmasked server→client frame off the raw socket.
    async fn read_raw_frame(socket: &mut TcpStream) -> std::io::Result<RawFrame> {
        let mut header = [0u8; 2];
        socket.read_exact(&mut header).await?;
        let opcode = header[0] & 0x0f;
        let len7 = header[1] & 0x7f;
        let payload_len: usize = match len7 {
            126 => {
                let mut ext = [0u8; 2];
                socket.read_exact(&mut ext).await?;
                u16::from_be_bytes(ext) as usize
            }
            127 => {
                let mut ext = [0u8; 8];
                socket.read_exact(&mut ext).await?;
                u64::from_be_bytes(ext) as usize
            }
            len => len as usize,
        };
        let mut payload = vec![0u8; payload_len];
        socket.read_exact(&mut payload).await?;
        Ok(RawFrame { opcode, payload })
    }

    /// Perform the WS upgrade handshake on a raw socket and return it. No
    /// frame is ever written after the handshake — the caller owns the
    /// fully-silent shape.
    async fn connect_raw_silent(port: u16) -> TcpStream {
        let mut socket = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let request = format!(
            "GET /api/ui-protocol/ws?token={AUTH_TOKEN} HTTP/1.1\r\n\
             Host: 127.0.0.1:{port}\r\n\
             Connection: Upgrade\r\n\
             Upgrade: websocket\r\n\
             Sec-WebSocket-Version: 13\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
             \r\n"
        );
        socket.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        let mut byte = [0u8; 1];
        let handshake_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !response.ends_with(b"\r\n\r\n") {
            assert!(
                tokio::time::Instant::now() < handshake_deadline,
                "WS upgrade handshake never completed"
            );
            if socket.read_exact(&mut byte).await.is_err() {
                panic!("server closed during the WS handshake");
            }
            response.push(byte[0]);
        }
        let status = String::from_utf8_lossy(&response);
        assert!(
            status.starts_with("HTTP/1.1 101"),
            "expected 101 Switching Protocols, got: {status}"
        );
        socket
    }

    /// #2447 — the half-open shape: a client that completes the upgrade and
    /// then sends nothing at all must be closed by the server with a real
    /// Close frame, after the liveness deadline (not instantly — some other
    /// gate must not be the closer).
    #[tokio::test]
    async fn serve_ws_liveness_closes_fully_silent_connection() {
        let _guard = serial_guard().await;
        let serve = spawn_serve();
        wait_for_port(serve.port).await;
        let mut socket = connect_raw_silent(serve.port).await;
        let started = tokio::time::Instant::now();

        let mut pings_seen = 0u32;
        let mut close_code: Option<u16> = None;
        let close_budget = started + DEADLINE + Duration::from_secs(30);
        while tokio::time::Instant::now() < close_budget {
            match tokio::time::timeout(Duration::from_secs(2), read_raw_frame(&mut socket)).await {
                Ok(Ok(frame)) if frame.opcode == RawFrame::OP_CLOSE => {
                    // RFC 6455: the first two payload bytes are the status
                    // code. The liveness path must be the closer.
                    if frame.payload.len() >= 2 {
                        close_code = Some(u16::from_be_bytes([frame.payload[0], frame.payload[1]]));
                    }
                    break;
                }
                Ok(Ok(frame)) if frame.opcode == RawFrame::OP_PING => pings_seen += 1,
                Ok(Ok(_)) => {} // text heartbeats ride the same tick
                Ok(Err(error)) => panic!("silent connection died without a Close frame: {error}"),
                Err(_) => {} // quiet 2 s window — keep waiting for the deadline
            }
        }

        assert_eq!(
            close_code,
            Some(1001),
            "server must close the fully-silent connection with the liveness close code"
        );
        assert!(
            pings_seen >= 1,
            "the writer task must ship protocol-level Pings for the deadline to have something to miss"
        );
        let elapsed = started.elapsed();
        assert!(
            elapsed >= DEADLINE - Duration::from_millis(500),
            "connection was closed after {elapsed:?} — earlier than the {DEADLINE:?} deadline, \
             so something other than liveness reaping closed it"
        );
    }

    /// #2447 — the healthy shape: a conforming client that Pongs every Ping
    /// (browsers do it at the protocol layer, octoscode's transport
    /// explicitly) keeps receiving Pings well past the deadline. Inbound
    /// frames are exactly what refreshes the liveness meter, so
    /// application-level idleness must NOT close the connection — that would
    /// defeat the AppUI keepalive's purpose.
    #[tokio::test]
    async fn serve_ws_liveness_keeps_ponging_client_open_past_deadline() {
        let _guard = serial_guard().await;
        let serve = spawn_serve();
        wait_for_port(serve.port).await;
        let url = format!(
            "ws://127.0.0.1:{}/api/ui-protocol/ws?token={AUTH_TOKEN}",
            serve.port
        );
        let (mut ws, _response) = connect_async(url).await.unwrap();
        let started = tokio::time::Instant::now();

        let mut pings_seen = 0u32;
        let mut quiet_windows = 0u32;
        let probe_window = started + DEADLINE + Duration::from_secs(2 * PING_SECS);
        while tokio::time::Instant::now() < probe_window {
            match tokio::time::timeout(Duration::from_secs(2), ws.next()).await {
                Ok(Some(Ok(Message::Ping(payload)))) => {
                    pings_seen += 1;
                    quiet_windows = 0;
                    // The answer every conforming stack gives — this is what
                    // keeps the server's meter refreshed.
                    ws.send(Message::Pong(payload))
                        .await
                        .expect("failed to send Pong");
                }
                Ok(Some(Ok(Message::Close(_)))) | Ok(None) => panic!(
                    "server closed a healthy ponging client after {pings_seen} pings — \
                     inbound frames must refresh the liveness meter"
                ),
                Ok(Some(Ok(_))) => quiet_windows = 0, // text heartbeats
                Ok(Some(Err(error))) => {
                    panic!("healthy connection errored after {pings_seen} pings: {error}")
                }
                // A loaded runner can starve the serve child past a tick;
                // only a sustained stall is a failure.
                Err(_) => {
                    quiet_windows += 1;
                    assert!(
                        quiet_windows <= 3,
                        "no keepalive frame arrived for three consecutive windows — \
                         writer task stalled"
                    );
                }
            }
        }

        assert!(
            pings_seen >= 3,
            "expected the keepalive cadence to deliver several Pings past the deadline, got {pings_seen}"
        );
    }

    /// #2447 review — frames that queue behind the housekeeping tick (the
    /// biased select polls the tick arm before the read arm) must be
    /// replayed through the per-frame path, never consumed: every pipelined
    /// request id must be answered, or the client hangs on a silent frame.
    /// A burst like this one is what a pipelining client produces; the
    /// drain must be transparent to it.
    #[tokio::test]
    async fn serve_ws_liveness_answers_every_pipelined_request_id() {
        let _guard = serial_guard().await;
        let serve = spawn_serve();
        wait_for_port(serve.port).await;
        let url = format!(
            "ws://127.0.0.1:{}/api/ui-protocol/ws?token={AUTH_TOKEN}",
            serve.port
        );
        let (mut ws, _response) = connect_async(url).await.unwrap();

        // Well-formed requests for an unknown method: each gets a JSON-RPC
        // error reply echoing its id — cheap, side-effect-free, and exactly
        // the id-echo a pipelining client depends on. (The wire contract is
        // string ids — numeric ids are rejected at the envelope.)
        let ids: Vec<String> = (0..8).map(|i| format!("pipeline-{i}")).collect();
        for id in &ids {
            let request = format!(
                r#"{{"jsonrpc":"2.0","id":"{id}","method":"no/such/method","params":{{}}}}"#
            );
            ws.send(Message::Text(request.into()))
                .await
                .expect("failed to send pipelined request");
        }

        // Collect replies over a budget spanning several housekeeping ticks.
        let mut answered: std::collections::HashSet<String> = std::collections::HashSet::new();
        let budget = tokio::time::Instant::now() + Duration::from_secs(10);
        while answered.len() < ids.len() && tokio::time::Instant::now() < budget {
            match tokio::time::timeout(Duration::from_millis(500), ws.next()).await {
                Ok(Some(Ok(Message::Text(text)))) => {
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
                        if let Some(id) = value.get("id").and_then(|v| v.as_str()) {
                            if ids.iter().any(|mine| mine == id) {
                                answered.insert(id.to_string());
                            }
                        }
                    }
                }
                Ok(Some(Ok(_))) => {} // pings and text heartbeats
                Ok(Some(Err(error))) => {
                    panic!("connection errored before every id was answered: {error}")
                }
                Ok(None) => panic!("server closed the connection before every id was answered"),
                Err(_) => {} // 500 ms collection slice — keep waiting
            }
        }
        let expected: std::collections::HashSet<String> = ids.iter().cloned().collect();
        assert_eq!(
            answered, expected,
            "every pipelined request id must be answered — a missing id means a frame was consumed without dispatch"
        );
    }
}
