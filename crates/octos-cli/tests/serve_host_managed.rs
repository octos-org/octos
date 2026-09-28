//! Integration test: `octos serve --host-managed` (UPCR-2026-036) with the REAL
//! binary. The host owns the process: closing its end of stdin (or dying,
//! even by SIGKILL) stops the server, no pairing code is printed, and an
//! inherited listener descriptor is served instead of a freshly bound port.
//!
//! Serial CI step (the broad integration step skips it):
//! `cargo test -p octos-cli --features api --test serve_host_managed -- --test-threads=1`
//!
//! Unix-only (descriptor passing and process control), like `serve_sigterm`.

#[cfg(unix)]
#[allow(unsafe_code)]
mod serve_host_managed {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::process::{Child, Command, Stdio};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    static SERIAL: Mutex<()> = Mutex::new(());

    const HOST: &str = "host-managed-e2e-host-token-0123456789abcdef";
    const EXTERNAL: &str = "host-managed-e2e-external-token-0123456789ab";

    fn octos_binary() -> std::path::PathBuf {
        if cfg!(feature = "api") {
            return env!("CARGO_BIN_EXE_octos").into();
        }
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let target_dir = std::path::Path::new(manifest_dir).join("../../target/serve-host-managed");
        let out = Command::new("cargo")
            .args(["build", "-p", "octos-cli", "--features", "api"])
            .current_dir(std::path::Path::new(manifest_dir).join("../.."))
            .env("CARGO_TARGET_DIR", &target_dir)
            .output()
            .expect("failed to bootstrap api-enabled octos binary");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        target_dir.join("debug/octos")
    }

    fn command(data_dir: &std::path::Path, extra: &[&str]) -> Command {
        let mut cmd = Command::new(octos_binary());
        cmd.args([
            "serve",
            "--host-managed",
            "--data-dir",
            data_dir.to_str().unwrap(),
            "--instance-data-dir",
            data_dir.to_str().unwrap(),
        ])
        .args(extra)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env("NO_COLOR", "1")
        .env_remove("OCTOS_AUTH_TOKEN")
        .env_remove("OCTOS_HOST_EXTERNAL_TOKEN")
        .env_remove("OCTOS_INSTANCE_DATA_DIR")
        .env_remove("OCTOS_HOME")
        .env_remove("OCTOS_DATA_DIR")
        .env_remove("OCTOS_SOLO_LOGIN");
        cmd
    }

    /// The host's first two stdin lines: the host token, the external token.
    fn send_tokens(child: &mut Child) {
        let stdin = child.stdin.as_mut().unwrap();
        writeln!(stdin, "{HOST}\n{EXTERNAL}").unwrap();
        stdin.flush().unwrap();
    }

    /// Read stdout until the listener announcement; collect every line.
    fn announced_port(child: &mut Child) -> (u16, std::sync::mpsc::Receiver<String>) {
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let line = rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("the server did not announce its listener");
            assert!(
                !line.contains("Pairing code") && !line.contains("pair="),
                "{line}"
            );
            if let Some(origin) = line.strip_prefix("Listening: http://127.0.0.1:") {
                return (origin.trim().parse().unwrap(), rx);
            }
        }
    }

    fn health(port: u16) -> String {
        let mut tcp = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        tcp.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        write!(
            tcp,
            "GET /health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut response = String::new();
        let _ = tcp.read_to_string(&mut response);
        response.lines().next().unwrap_or_default().to_owned()
    }

    fn exits_within(child: &mut Child, limit: Duration) -> Option<std::process::ExitStatus> {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            if let Some(status) = child.try_wait().unwrap() {
                return Some(status);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        None
    }

    #[test]
    fn serve_host_managed_stops_when_the_host_closes_stdin() {
        let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let mut child = command(dir.path(), &["--port", "0"]).spawn().unwrap();
        send_tokens(&mut child);
        let (port, lines) = announced_port(&mut child);
        // The tokens never reach the environment (/proc/<pid>/environ).
        #[cfg(target_os = "linux")]
        {
            let environ = std::fs::read(format!("/proc/{}/environ", child.id())).unwrap();
            let environ = String::from_utf8_lossy(&environ);
            assert!(!environ.contains(HOST) && !environ.contains(EXTERNAL));
        }
        assert!(health(port).contains(" 200 "));
        // Other bytes on stdin are ignored; only EOF stops the server.
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(b"ignored\n")
            .unwrap();
        assert!(exits_within(&mut child, Duration::from_secs(1)).is_none());
        drop(child.stdin.take());
        let status = exits_within(&mut child, Duration::from_secs(30)).unwrap_or_else(|| {
            let _ = child.kill();
            panic!("the server outlived its host's stdin");
        });
        assert!(status.success(), "{status}");
        let printed: Vec<String> = lines.try_iter().collect();
        assert!(
            !printed
                .iter()
                .any(|line| line.contains(HOST) || line.contains(EXTERNAL)),
            "no token on stdout: {printed:?}"
        );
    }

    /// A host that is SIGKILLed never closes anything itself: the OS closes
    /// its end of the pipe, and the server stops. The pipe's only writer here
    /// is a `sleep` standing in for the host; this test holds no copy.
    #[test]
    fn serve_host_managed_stops_when_its_host_is_sigkilled() {
        let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let mut host = Command::new("sh")
            .args([
                "-c",
                &format!("printf '%s\\n%s\\n' {HOST} {EXTERNAL}; exec sleep 600"),
            ])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let lifeline = host.stdout.take().unwrap();
        let mut child = command(dir.path(), &["--port", "0"])
            .stdin(Stdio::from(lifeline))
            .spawn()
            .unwrap();
        let (port, _lines) = announced_port(&mut child);
        assert!(health(port).contains(" 200 "));
        host.kill().unwrap(); // SIGKILL
        host.wait().unwrap();
        let status = exits_within(&mut child, Duration::from_secs(30)).unwrap_or_else(|| {
            let _ = child.kill();
            panic!("the server outlived its SIGKILLed host");
        });
        assert!(status.success(), "{status}");
    }

    #[test]
    fn serve_host_managed_serves_the_listener_the_host_keeps() {
        use std::os::fd::AsRawFd;
        let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let fd = listener.as_raw_fd();
        let mut cmd = command(dir.path(), &["--listen-fd", "3"]);
        // SAFETY: only async-signal-safe dup2/fcntl run between fork and
        // exec. They place the host's listener at descriptor 3 and clear its
        // CLOEXEC (dup2 onto itself would keep the flag).
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(move || {
                if fd != 3 && libc::dup2(fd, 3) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::fcntl(3, libc::F_SETFD, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = cmd.spawn().unwrap();
        send_tokens(&mut child);
        let (announced, _lines) = announced_port(&mut child);
        assert_eq!(announced, port, "the server announces the inherited port");
        assert!(health(port).contains(" 200 "));
        drop(child.stdin.take());
        assert!(
            exits_within(&mut child, Duration::from_secs(30)).is_some_and(|s| s.success()),
            "the server stops on stdin EOF"
        );
        // The host still owns the port: a client connects (queued in the
        // backlog) and no other process could have bound it meanwhile.
        assert!(std::net::TcpStream::connect(("127.0.0.1", port)).is_ok());
        assert!(std::net::TcpListener::bind(("127.0.0.1", port)).is_err());
        drop(listener);
    }

    #[test]
    fn serve_host_managed_refuses_tokens_in_the_environment() {
        let dir = tempfile::tempdir().unwrap();
        let output = Command::new(octos_binary())
            .args(["serve", "--host-managed", "--port", "0", "--data-dir"])
            .arg(dir.path())
            .arg("--instance-data-dir")
            .arg(dir.path())
            .env(
                "OCTOS_AUTH_TOKEN",
                "host-managed-e2e-host-token-0123456789abcdef",
            )
            .env_remove("OCTOS_INSTANCE_DATA_DIR")
            .env_remove("OCTOS_HOME")
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("reads its tokens from stdin"));
    }

    #[cfg(feature = "api")]
    /// A scripted OpenAI-compatible model: records the tool names of every
    /// request and answers "ok" (streamed or not).
    fn mock_model() -> (u16, std::sync::Arc<Mutex<Vec<Vec<String>>>>) {
        mock_model_with_delay(Duration::ZERO)
    }

    #[cfg(feature = "api")]
    /// [`mock_model`] that holds every completion for `delay` first, so a
    /// turn stays running.
    fn mock_model_with_delay(delay: Duration) -> (u16, std::sync::Arc<Mutex<Vec<Vec<String>>>>) {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let record = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let record = record.clone();
                std::thread::spawn(move || {
                    let mut stream = stream;
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut length = 0usize;
                    let mut first = String::new();
                    if reader.read_line(&mut first).is_err() {
                        return;
                    }
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                            break;
                        }
                        if let Some(value) =
                            line.to_ascii_lowercase().strip_prefix("content-length:")
                        {
                            length = value.trim().parse().unwrap_or(0);
                        }
                    }
                    let mut body = vec![0u8; length];
                    let _ = reader.read_exact(&mut body);
                    if first.starts_with("GET") {
                        let data =
                            r#"{"object":"list","data":[{"id":"mock-model","object":"model"}]}"#;
                        let _ = write!(
                            stream,
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{data}",
                            data.len()
                        );
                        return;
                    }
                    let request: serde_json::Value =
                        serde_json::from_slice(&body).unwrap_or_default();
                    let tools: Vec<String> = request["tools"]
                        .as_array()
                        .map(|tools| {
                            tools
                                .iter()
                                .filter_map(|tool| {
                                    tool["function"]["name"].as_str().map(str::to_owned)
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    record.lock().unwrap().push(tools);
                    std::thread::sleep(delay);
                    if request["stream"] == true {
                        let chunk = r#"{"id":"c1","object":"chat.completion.chunk","model":"mock-model","choices":[{"index":0,"delta":{"role":"assistant","content":"ok"},"finish_reason":null}]}"#;
                        let done = r#"{"id":"c1","object":"chat.completion.chunk","model":"mock-model","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#;
                        let _ = write!(
                            stream,
                            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\ndata: {chunk}\n\ndata: {done}\n\ndata: [DONE]\n\n"
                        );
                    } else {
                        let data = r#"{"id":"c1","object":"chat.completion","model":"mock-model","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#;
                        let _ = write!(
                            stream,
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{data}",
                            data.len()
                        );
                    }
                });
            }
        });
        (port, seen)
    }

    #[cfg(feature = "api")]
    fn write_mock_profile(dir: &std::path::Path, model_port: u16) {
        std::fs::create_dir_all(dir.join("profiles")).unwrap();
        std::fs::write(
            dir.join("profiles/_main.json"),
            serde_json::json!({
                "id": "_main", "name": "Main", "enabled": true,
                "created_at": "2026-09-28T00:00:00Z", "updated_at": "2026-09-28T00:00:00Z",
                "config": {"llm": {"primary": {"family_id": "local", "model_id": "mock-model",
                    "route": {"base_url": format!("http://127.0.0.1:{model_port}/v1"), "api_type": "openai"}}}}
            })
            .to_string(),
        )
        .unwrap();
    }

    #[cfg(feature = "api")]
    type Socket = tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >;

    #[cfg(feature = "api")]
    async fn connect(port: u16, token: &str) -> Socket {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let mut request = format!("ws://127.0.0.1:{port}/api/ui-protocol/ws")
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("authorization", format!("Bearer {token}").parse().unwrap());
        tokio_tungstenite::connect_async(request).await.unwrap().0
    }

    #[cfg(feature = "api")]
    /// One JSON-RPC call; the response frame (result or error).
    async fn rpc(
        socket: &mut Socket,
        id: &str,
        method: &str,
        params: serde_json::Value,
    ) -> serde_json::Value {
        use futures::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        let frame =
            serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        socket
            .send(Message::Text(frame.to_string().into()))
            .await
            .unwrap();
        loop {
            let message = tokio::time::timeout(Duration::from_secs(60), socket.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            if let Message::Text(text) = message {
                let value: serde_json::Value = serde_json::from_str(text.as_str()).unwrap();
                if value["id"] == id {
                    return value;
                }
            }
        }
    }

    #[cfg(feature = "api")]
    #[test]
    fn serve_host_managed_refuses_an_external_turn_reusing_a_host_turn_id() {
        let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        // The model holds every completion, so the host's turn keeps running.
        let (model_port, _seen) = mock_model_with_delay(Duration::from_secs(90));
        write_mock_profile(dir.path(), model_port);
        let mut child = command(dir.path(), &["--port", "0"]).spawn().unwrap();
        send_tokens(&mut child);
        let (port, _lines) = announced_port(&mut child);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let system = "_main:api:octosense#system";
            let other = "_main:api:web#mine";
            let host_turn = uuid::Uuid::now_v7().to_string();
            let mut host = connect(port, HOST).await;
            let opened = rpc(&mut host, "open", "session/open", serde_json::json!({"session_id": system, "profile_id": "_main"})).await;
            assert!(opened.get("error").is_none(), "{opened}");
            let started = rpc(&mut host, "turn", "turn/start", serde_json::json!({"session_id": system, "turn_id": host_turn, "input": [{"kind": "text", "text": "hello"}]})).await;
            assert!(started.get("error").is_none(), "{started}");

            let mut external = connect(port, EXTERNAL).await;
            for (id, session) in [("open-other", other), ("open-system", system)] {
                let opened = rpc(&mut external, id, "session/open", serde_json::json!({"session_id": session, "profile_id": "_main"})).await;
                assert!(opened.get("error").is_none(), "{opened}");
            }
            // The host's turn id, reused in the external client's own session.
            let reused = rpc(&mut external, "reuse", "turn/start", serde_json::json!({"session_id": other, "turn_id": host_turn, "input": [{"kind": "text", "text": "hi"}]})).await;
            assert_eq!(reused["error"]["data"]["kind"], "turn_id_in_use", "{reused}");
            // A turn on the busy shared conversation: refused, without the
            // host turn's id.
            let busy = rpc(&mut external, "busy", "turn/start", serde_json::json!({"session_id": system, "turn_id": uuid::Uuid::now_v7().to_string(), "input": [{"kind": "text", "text": "hi"}]})).await;
            assert_eq!(busy["error"]["data"], serde_json::json!({"kind": "turn_in_progress"}), "{busy}");
            // The host's turn cannot be steered or interrupted by id.
            let steer = rpc(&mut external, "steer", "turn/steer", serde_json::json!({"session_id": system, "expected_turn_id": host_turn, "input": [{"kind": "text", "text": "leak"}]})).await;
            assert_eq!(steer["error"]["data"]["kind"], "external_turn_denied", "{steer}");
            let interrupt = rpc(&mut external, "interrupt", "turn/interrupt", serde_json::json!({"session_id": system, "turn_id": host_turn})).await;
            assert_eq!(interrupt["error"]["data"]["kind"], "external_turn_denied", "{interrupt}");
            // The host still owns and stops its turn.
            let stopped = rpc(&mut host, "stop", "turn/interrupt", serde_json::json!({"session_id": system, "turn_id": host_turn})).await;
            assert!(stopped.get("error").is_none(), "{stopped}");
        });
        drop(child.stdin.take());
        if exits_within(&mut child, Duration::from_secs(30)).is_none() {
            let _ = child.kill();
        }
    }

    #[cfg(feature = "api")]
    /// Open the system conversation and start one turn over the real socket
    /// with `token`; return the tool names the model received for it.
    fn tools_for_a_turn(
        port: u16,
        token: &str,
        seen: &std::sync::Arc<Mutex<Vec<Vec<String>>>>,
    ) -> Vec<String> {
        use futures::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};
        let before = seen.lock().unwrap().len();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let mut request = format!("ws://127.0.0.1:{port}/api/ui-protocol/ws").into_client_request().unwrap();
            request.headers_mut().insert("authorization", format!("Bearer {token}").parse().unwrap());
            let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
            let session = "_main:api:octosense#system";
            for (id, method, params) in [
                ("open", "session/open", serde_json::json!({"session_id": session, "profile_id": "_main"})),
                ("turn", "turn/start", serde_json::json!({"session_id": session, "turn_id": uuid::Uuid::now_v7().to_string(), "input": [{"kind": "text", "text": "hello"}]})),
            ] {
                let frame = serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
                socket.send(Message::Text(frame.to_string().into())).await.unwrap();
                loop {
                    let message = tokio::time::timeout(Duration::from_secs(60), socket.next()).await.unwrap().unwrap().unwrap();
                    if let Message::Text(text) = message {
                        let value: serde_json::Value = serde_json::from_str(text.as_str()).unwrap();
                        if value["id"] == id {
                            assert!(value.get("error").is_none(), "{method}: {value}");
                            break;
                        }
                    }
                }
            }
            let deadline = Instant::now() + Duration::from_secs(60);
            while seen.lock().unwrap().len() == before {
                assert!(Instant::now() < deadline, "the model was never called");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        });
        seen.lock().unwrap()[before].clone()
    }

    #[cfg(feature = "api")]
    #[test]
    fn serve_host_managed_gives_an_external_turn_only_the_allowlisted_tools() {
        let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let (model_port, seen) = mock_model();
        write_mock_profile(dir.path(), model_port);
        let mut child = command(dir.path(), &["--port", "0"]).spawn().unwrap();
        send_tokens(&mut child);
        let (port, _lines) = announced_port(&mut child);
        let external = tools_for_a_turn(port, EXTERNAL, &seen);
        let host = tools_for_a_turn(port, HOST, &seen);
        eprintln!(
            "external turn tools: {external:?}\nhost turn tools: {} tools",
            host.len()
        );
        drop(child.stdin.take());
        let _ = exits_within(&mut child, Duration::from_secs(30));
        // Exactly the allowlist the server offers (those it has registered),
        // nothing else: no shell, spawn, peer_*, send_file, task, MCP or
        // plugin tool.
        let allowlist = [
            "read_file",
            "write_file",
            "edit_file",
            "diff_edit",
            "apply_patch",
            "glob",
            "grep",
            "list_dir",
            "code_structure",
            "check_workspace_contract",
            "web_search",
            "web_fetch",
            "ask_user_question",
            "recall",
            "recall_memory",
            "memory_search",
            "memory_load",
            "view_image",
            "view_video",
            "tool_search",
        ];
        assert!(
            !external.is_empty(),
            "the external turn has its read/write tools"
        );
        for tool in &external {
            assert!(
                allowlist.contains(&tool.as_str()),
                "external turn got {tool}: {external:?}"
            );
        }
        let expected: Vec<&str> = allowlist
            .iter()
            .copied()
            .filter(|t| host.iter().any(|h| h == t))
            .collect();
        let mut got: Vec<&str> = external.iter().map(String::as_str).collect();
        got.sort_unstable();
        let mut expected_sorted = expected.clone();
        expected_sorted.sort_unstable();
        assert_eq!(
            got, expected_sorted,
            "the external turn gets every allowlisted tool the host has"
        );
        // The host's own turn keeps its full surface (the filter is per connection).
        assert!(host.len() > external.len(), "host: {host:?}");
        assert!(
            host.iter()
                .any(|t| t == "spawn" || t == "shell" || t.starts_with("peer_")),
            "host: {host:?}"
        );
    }
}
