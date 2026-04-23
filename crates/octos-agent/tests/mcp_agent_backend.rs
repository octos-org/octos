//! Integration tests for the MCP agent-tool backend (M7.1).
//!
//! These tests stand up a minimal in-process MCP peer — either a stdio
//! child process running a fake server, or a bespoke loopback HTTP peer
//! built directly on `tokio::net::TcpListener` — and drive the
//! [`StdioMcpAgent`] / [`HttpMcpAgent`] backends through the invariants
//! enumerated in the M7.1 contract:
//!
//! - Dispatch returns the contract-shaped artifact.
//! - Sub-agent internal chatter never leaks to the parent (only the
//!   `tools/call` response surfaces).
//! - Timeouts kill the stdio subprocess within 500 ms.
//! - `BLOCKED_ENV_VARS` is applied to the stdio child env.
//! - HTTP backend honours its read timeout.
//! - Dispatch emits the typed
//!   `HarnessEventPayload::SubAgentDispatch` event.

#![cfg(unix)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use octos_agent::harness_events::{HarnessEvent, HarnessEventPayload};
use octos_agent::tools::mcp_agent::{
    DispatchOutcome, DispatchRequest, HttpMcpAgent, McpAgentBackend, McpAgentBackendConfig,
    StdioMcpAgent, build_backend_from_config, build_dispatch_event_payload, dispatch_with_metrics,
};

// ── Helpers ────────────────────────────────────────────────────────────────

/// Build a throwaway shell-script MCP server that answers two requests:
/// - `initialize` → `{ "result": { "serverInfo": {} } }`
/// - `tools/call` → a `content`-shaped success result. The script emits
///   the tool name + argument payload back to the caller in the text
///   body so tests can assert that the dispatch payload flowed through.
fn write_stdio_responder_script(dir: &tempfile::TempDir) -> PathBuf {
    let script = r#"#!/bin/sh
set -eu
read init
# Respond to initialize
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"serverInfo":{"name":"fake"}}}'
read call
# Extract the tool name out of the call for assertion.
tool=$(printf '%s' "$call" | sed -n 's/.*"name":"\([^"]*\)".*/\1/p')
printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"fake-agent:$tool\"}],\"files_to_send\":[\"/tmp/artifact.md\"]}}"
"#;
    let path = dir.path().join("mcp-responder.sh");
    std::fs::write(&path, script).expect("write responder script");
    let mut perms = std::fs::metadata(&path).expect("read perms").permissions();
    use std::os::unix::fs::PermissionsExt;
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).expect("chmod responder script");
    path
}

/// Build a subprocess MCP server that never responds on stdout, used to
/// exercise the dispatch timeout path. Reads stdin so the parent's
/// `write_all` does not race ahead of the script spawning.
fn write_stdio_hang_script(dir: &tempfile::TempDir) -> PathBuf {
    let script = r#"#!/bin/sh
# Read the initialize request but never respond.
read init
# Sleep long enough that the dispatch timeout fires before we exit.
sleep 30
"#;
    let path = dir.path().join("mcp-hang.sh");
    std::fs::write(&path, script).expect("write hang script");
    let mut perms = std::fs::metadata(&path).expect("read perms").permissions();
    use std::os::unix::fs::PermissionsExt;
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).expect("chmod hang script");
    path
}

/// Write a script that echoes selected env vars to a log file so tests
/// can assert that `BLOCKED_ENV_VARS` are scrubbed from the child env.
fn write_stdio_env_probe_script(dir: &tempfile::TempDir, log_path: &std::path::Path) -> PathBuf {
    let script = format!(
        r#"#!/bin/sh
set -eu
: > "{log}"
for v in LD_PRELOAD DYLD_INSERT_LIBRARIES NODE_OPTIONS BASH_ENV; do
  eval "val=\${{$v:-}}"
  printf '%s=%s\n' "$v" "$val" >> "{log}"
done
# Respond once so the parent can resolve the dispatch cleanly.
read init
printf '%s\n' '{{"jsonrpc":"2.0","id":1,"result":{{"serverInfo":{{}}}}}}'
read call
printf '%s\n' '{{"jsonrpc":"2.0","id":2,"result":{{"content":[{{"type":"text","text":"probed"}}]}}}}'
"#,
        log = log_path.display()
    );
    let path = dir.path().join("mcp-env-probe.sh");
    std::fs::write(&path, script).expect("write env probe script");
    let mut perms = std::fs::metadata(&path).expect("read perms").permissions();
    use std::os::unix::fs::PermissionsExt;
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).expect("chmod env probe script");
    path
}

/// Boot a tiny HTTP server that mimics the JSON-RPC /tools/call contract
/// without pulling axum as a dev-dependency. Accepts one connection per
/// test — that is enough for the dispatch paths under test.
async fn boot_fake_http_server(
    response_body: serde_json::Value,
    response_delay: Duration,
) -> (String, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local_addr");
    let url = format!("http://{}/", addr);
    let body_text = response_body.to_string();

    let join = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let body = body_text.clone();
            let delay = response_delay;
            tokio::spawn(async move {
                // Drain the request (one shot — good enough for tests).
                let mut buf = vec![0_u8; 8192];
                let _ =
                    tokio::time::timeout(Duration::from_millis(500), socket.read(&mut buf)).await;
                tokio::time::sleep(delay).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    (url, join)
}

/// Spawn a unique task/session id pair so repeated test runs do not
/// collide in the supervisor's ledger.
fn unique_ids() -> (String, String) {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    (
        format!("api:test-session-{nanos}"),
        format!("task-test-{nanos}"),
    )
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn should_dispatch_to_stdio_mcp_agent_and_return_contract_artifact() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_stdio_responder_script(&dir);
    let config = McpAgentBackendConfig::Local {
        cmd: script.display().to_string(),
        args: vec![],
        env: HashMap::new(),
        dispatch_timeout_secs: Some(5),
    };
    let backend =
        build_backend_from_config(&config, Some(dir.path())).expect("build stdio backend");
    let request = DispatchRequest {
        tool_name: "run_task".into(),
        task: serde_json::json!({"task": "hello"}),
    };
    let response = backend.dispatch(request).await;

    assert_eq!(response.outcome, DispatchOutcome::Success);
    assert!(
        response.output.contains("fake-agent:run_task"),
        "unexpected output: {}",
        response.output
    );
    assert_eq!(
        response.files_to_send,
        vec![PathBuf::from("/tmp/artifact.md")]
    );
}

#[tokio::test]
async fn should_dispatch_to_remote_mcp_agent_and_return_contract_artifact() {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": {
            "content": [{"type": "text", "text": "remote-agent"}],
            "files_to_send": ["/tmp/remote.md"],
        }
    });
    let (url, join) = boot_fake_http_server(body, Duration::from_millis(5)).await;

    let config = McpAgentBackendConfig::Remote {
        url,
        auth_header: Some("Bearer test".into()),
        extra_headers: HashMap::new(),
        connect_timeout_secs: Some(2),
        read_timeout_secs: Some(2),
        dispatch_timeout_secs: Some(5),
    };
    let backend = HttpMcpAgent::from_config(&config)
        .expect("build http backend")
        .with_loopback_allowed_for_tests();
    let request = DispatchRequest {
        tool_name: "run_task".into(),
        task: serde_json::json!({"task": "hi"}),
    };
    let response = backend.dispatch(request).await;
    join.abort();

    assert_eq!(response.outcome, DispatchOutcome::Success);
    assert_eq!(response.output, "remote-agent");
    assert_eq!(
        response.files_to_send,
        vec![PathBuf::from("/tmp/remote.md")]
    );
}

#[tokio::test]
async fn should_kill_subprocess_on_timeout_and_reap_within_500ms() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_stdio_hang_script(&dir);
    let config = McpAgentBackendConfig::Local {
        cmd: script.display().to_string(),
        args: vec![],
        env: HashMap::new(),
        // 300ms budget — well under the script's 30s sleep.
        dispatch_timeout_secs: None,
    };
    let mut backend = StdioMcpAgent::from_config(&config).expect("build stdio backend");
    backend = backend
        .with_cwd(dir.path().to_path_buf())
        .with_dispatch_timeout(Duration::from_millis(300));

    let start = Instant::now();
    let response = backend
        .dispatch(DispatchRequest {
            tool_name: "run_task".into(),
            task: serde_json::json!({}),
        })
        .await;
    let elapsed = start.elapsed();

    assert_eq!(response.outcome, DispatchOutcome::Timeout);
    // Timeout plus up to 500ms of reaping slack.
    assert!(
        elapsed < Duration::from_millis(300 + 500),
        "dispatch did not cancel quickly enough: elapsed={elapsed:?}"
    );

    // After dispatch returns, no orphan `mcp-hang.sh` should remain for
    // our working directory. Use `pgrep -f` with the exact script path so
    // we do not accidentally match unrelated shells.
    let ok = wait_until(Duration::from_millis(500), || {
        let out = std::process::Command::new("pgrep")
            .arg("-f")
            .arg(script.display().to_string())
            .output();
        match out {
            Ok(out) => !out.status.success() && out.stdout.is_empty(),
            Err(_) => true, // pgrep not available — skip the assertion
        }
    })
    .await;
    assert!(ok, "hang-script child survived the timeout path");
}

async fn wait_until<F>(budget: Duration, mut check: F) -> bool
where
    F: FnMut() -> bool,
{
    let start = Instant::now();
    while start.elapsed() < budget {
        if check() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    check()
}

#[tokio::test]
async fn should_apply_blocked_env_vars_to_stdio_subprocess() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("env-probe.log");
    let script = write_stdio_env_probe_script(&dir, &log);

    // Caller-configured env deliberately includes injection-vector names
    // alongside a benign marker. The sanitizer must strip the blocked
    // names even when the caller opts them into the allowlist, but keep
    // the benign marker.
    let mut extra = HashMap::new();
    extra.insert("LD_PRELOAD".into(), "evil.so".into());
    extra.insert("DYLD_INSERT_LIBRARIES".into(), "evil.dylib".into());
    extra.insert("NODE_OPTIONS".into(), "--require=bad".into());
    extra.insert("BASH_ENV".into(), "/tmp/bad.sh".into());
    extra.insert("OCTOS_TEST_MARKER".into(), "kept".into());

    let config = McpAgentBackendConfig::Local {
        cmd: script.display().to_string(),
        args: vec![],
        env: extra,
        dispatch_timeout_secs: Some(5),
    };
    let backend = StdioMcpAgent::from_config(&config)
        .unwrap()
        .with_cwd(dir.path().to_path_buf());

    let response = backend
        .dispatch(DispatchRequest {
            tool_name: "run_task".into(),
            task: serde_json::json!({}),
        })
        .await;
    assert_eq!(response.outcome, DispatchOutcome::Success);

    let log_contents = std::fs::read_to_string(&log).expect("read env probe log");
    // Empty value after the `=` proves the blocked name was scrubbed
    // even though the caller explicitly set it.
    assert!(
        log_contents.contains("LD_PRELOAD=\n"),
        "LD_PRELOAD leaked into child: {log_contents}"
    );
    assert!(
        log_contents.contains("NODE_OPTIONS=\n"),
        "NODE_OPTIONS leaked into child: {log_contents}"
    );
    assert!(
        log_contents.contains("BASH_ENV=\n"),
        "BASH_ENV leaked into child: {log_contents}"
    );
    assert!(
        log_contents.contains("DYLD_INSERT_LIBRARIES=\n"),
        "DYLD_INSERT_LIBRARIES leaked into child: {log_contents}"
    );
}

#[tokio::test]
async fn should_enforce_http_timeout_on_remote_backend() {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": {"content": [{"type": "text", "text": "late"}]}
    });
    // Server sleeps 500ms — well past our 150ms read timeout.
    let (url, join) = boot_fake_http_server(body, Duration::from_millis(500)).await;

    let config = McpAgentBackendConfig::Remote {
        url,
        auth_header: None,
        extra_headers: HashMap::new(),
        connect_timeout_secs: Some(1),
        read_timeout_secs: Some(1),
        // Dispatch budget below the server delay so the client aborts.
        dispatch_timeout_secs: None,
    };
    let backend = HttpMcpAgent::from_config(&config)
        .unwrap()
        .with_loopback_allowed_for_tests()
        .with_dispatch_timeout(Duration::from_millis(150));

    let start = Instant::now();
    let response = backend
        .dispatch(DispatchRequest {
            tool_name: "run_task".into(),
            task: serde_json::json!({}),
        })
        .await;
    let elapsed = start.elapsed();
    join.abort();

    assert_eq!(response.outcome, DispatchOutcome::Timeout);
    assert!(
        elapsed < Duration::from_millis(450),
        "HTTP dispatch did not honour timeout: elapsed={elapsed:?}"
    );
}

#[tokio::test]
async fn should_emit_sub_agent_dispatch_event_on_typed_payload() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_stdio_responder_script(&dir);
    let config = McpAgentBackendConfig::Local {
        cmd: script.display().to_string(),
        args: vec![],
        env: HashMap::new(),
        dispatch_timeout_secs: Some(5),
    };
    let backend =
        build_backend_from_config(&config, Some(dir.path())).expect("build stdio backend");

    let (session_id, task_id) = unique_ids();
    let request = DispatchRequest {
        tool_name: "run_task".into(),
        task: serde_json::json!({"task": "hi"}),
    };
    let (response, summary) = dispatch_with_metrics(backend.as_ref(), request).await;

    let payload = build_dispatch_event_payload(
        &session_id,
        &task_id,
        Some("coding"),
        Some("dispatch"),
        backend.as_ref(),
        &response,
    );

    let event = HarnessEvent {
        schema: octos_agent::harness_events::HARNESS_EVENT_SCHEMA_V1.to_string(),
        payload,
    };
    event.validate().expect("event must validate");

    match event.payload {
        HarnessEventPayload::SubAgentDispatch { data } => {
            assert_eq!(data.session_id, session_id);
            assert_eq!(data.task_id, task_id);
            assert_eq!(data.backend, "local");
            assert_eq!(data.outcome, "success");
            assert_eq!(data.workflow.as_deref(), Some("coding"));
            assert_eq!(data.phase.as_deref(), Some("dispatch"));
            assert_eq!(
                data.schema_version,
                octos_agent::abi_schema::SUB_AGENT_DISPATCH_SCHEMA_VERSION
            );
        }
        other => panic!("wrong payload: {other:?}"),
    }

    // Summary mirrors the event so callers without access to the payload
    // can still wire metrics/logs.
    assert_eq!(summary.backend, "local");
    assert_eq!(summary.outcome, "success");
    assert!(summary.endpoint.ends_with("mcp-responder.sh"));
}

#[tokio::test]
async fn should_isolate_sub_agent_internal_context_from_parent() {
    // The fake responder emits exactly one final payload. Anything else
    // produced by an "inner" sub-agent tool call must stay inside the
    // MCP call and not surface to the dispatch caller.
    let dir = tempfile::tempdir().unwrap();
    let script_path = dir.path().join("mcp-internal.sh");
    let script = r#"#!/bin/sh
set -eu
read init
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"serverInfo":{}}}'
read call
# Pretend the sub-agent ran several internal tools — but only the final
# JSON-RPC frame is emitted on stdout. Everything else is logged on
# stderr so even a leaky harness cannot accidentally surface it.
>&2 echo "sub-agent internal: reading file A"
>&2 echo "sub-agent internal: calling write_file"
>&2 echo "sub-agent internal: cleaning up"
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"final-artifact-only"}]}}'
"#;
    std::fs::write(&script_path, script).expect("write internal script");
    let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
    use std::os::unix::fs::PermissionsExt;
    perms.set_mode(0o755);
    std::fs::set_permissions(&script_path, perms).unwrap();

    let config = McpAgentBackendConfig::Local {
        cmd: script_path.display().to_string(),
        args: vec![],
        env: HashMap::new(),
        dispatch_timeout_secs: Some(5),
    };
    let backend = StdioMcpAgent::from_config(&config)
        .unwrap()
        .with_cwd(dir.path().to_path_buf());

    let response = backend
        .dispatch(DispatchRequest {
            tool_name: "run_task".into(),
            task: serde_json::json!({"task": "x"}),
        })
        .await;

    assert_eq!(response.outcome, DispatchOutcome::Success);
    assert_eq!(response.output, "final-artifact-only");
    // Internal chatter never appears in the DispatchResponse.
    assert!(
        !response.output.contains("sub-agent internal"),
        "internal sub-agent context leaked to parent"
    );
    assert!(
        response.files_to_send.is_empty(),
        "no file was explicitly declared; workspace contract stays authoritative"
    );
}
