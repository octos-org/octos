//! M7.2 — MCP server mode acceptance tests.
//!
//! These tests exercise the session-level MCP server: one MCP tool =
//! one full octos session that runs to completion and returns the
//! workspace-contract artifact to the outer caller.
//!
//! The server supports two transports:
//! - stdio (parent-trust auth)
//! - http (bearer token required)
//!
//! Internal agent state (tool calls, progress, iteration traces) is
//! never streamed to the MCP caller. The caller sees a single
//! request/response round-trip.

use std::sync::Arc;

use async_trait::async_trait;
use octos_agent::harness_events::HarnessEventPayload;
use octos_agent::mcp_server::{
    McpServer, McpServerError, McpServerHandle, McpSessionDispatch, McpSessionOutcome,
    SessionLifecycleObserver, build_initialize_response, build_tools_list_response,
    dispatch_run_octos_session, parse_bearer_token, render_mcp_error,
};
use octos_agent::task_supervisor::{TaskLifecycleState, TaskSupervisor};
use octos_agent::{HarnessEvent, TASK_RESULT_SCHEMA_VERSION};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex;

/// A scripted session dispatch that a test can steer into Ready or Failed.
#[derive(Clone)]
struct ScriptedDispatch {
    outcome: Arc<Mutex<McpSessionOutcome>>,
}

impl ScriptedDispatch {
    fn with_outcome(outcome: McpSessionOutcome) -> Self {
        Self {
            outcome: Arc::new(Mutex::new(outcome)),
        }
    }
}

#[async_trait]
impl McpSessionDispatch for ScriptedDispatch {
    async fn run_session(
        &self,
        contract: &str,
        input: &Value,
        observer: &dyn SessionLifecycleObserver,
    ) -> Result<McpSessionOutcome, McpServerError> {
        observer.mark_state(TaskLifecycleState::Queued);
        observer.mark_state(TaskLifecycleState::Running);
        observer.mark_state(TaskLifecycleState::Verifying);
        let _ = (contract, input);
        let mut guard = self.outcome.lock().await;
        let outcome = guard.clone();
        observer.mark_state(outcome.final_state);
        *guard = outcome.clone();
        Ok(outcome)
    }
}

fn sample_ready_outcome() -> McpSessionOutcome {
    McpSessionOutcome {
        final_state: TaskLifecycleState::Ready,
        artifact_path: Some("pf/deck.pptx".to_string()),
        artifact_content: Some("MOCK-PPTX-BYTES".to_string()),
        validator_results: vec![
            json!({"validator": "slides-sanity", "passed": true, "message": "ok"}),
        ],
        cost: json!({"input_tokens": 100, "output_tokens": 42}),
        error: None,
    }
}

fn sample_failed_outcome() -> McpSessionOutcome {
    McpSessionOutcome {
        final_state: TaskLifecycleState::Failed,
        artifact_path: None,
        artifact_content: None,
        validator_results: vec![
            json!({"validator": "slides-sanity", "passed": false, "message": "slide count too low"}),
        ],
        cost: json!({"input_tokens": 40, "output_tokens": 12}),
        error: Some("contract gate failed: slides-sanity".to_string()),
    }
}

#[tokio::test]
async fn should_expose_session_as_mcp_tool_via_stdio() {
    let dispatch = Arc::new(ScriptedDispatch::with_outcome(sample_ready_outcome()));
    let supervisor = Arc::new(TaskSupervisor::new());
    let server = McpServer::new(dispatch, supervisor);

    // tools/list must advertise a single session-level tool named
    // `run_octos_session`. Inner tools are never exposed.
    let tools = build_tools_list_response(&server);
    let tools_arr = tools.get("tools").and_then(Value::as_array).unwrap();
    assert_eq!(tools_arr.len(), 1, "exactly one MCP tool exposed");
    let tool = &tools_arr[0];
    assert_eq!(tool["name"], "run_octos_session");
    let schema = tool.get("inputSchema").expect("input schema present");
    let required = schema
        .get("required")
        .and_then(Value::as_array)
        .expect("schema declares required fields");
    let required: Vec<&str> = required.iter().filter_map(Value::as_str).collect();
    assert!(required.contains(&"contract"));
    assert!(required.contains(&"input"));
}

#[tokio::test]
async fn should_require_bearer_token_on_http_transport() {
    // Missing bearer token => 401 synchronously, no session dispatched.
    let dispatch = Arc::new(ScriptedDispatch::with_outcome(sample_ready_outcome()));
    let supervisor = Arc::new(TaskSupervisor::new());
    let server = McpServer::new(dispatch.clone(), supervisor);

    assert_eq!(
        parse_bearer_token(Some("Bearer secret123")),
        Some("secret123".into())
    );
    assert_eq!(
        parse_bearer_token(Some("bearer  secret123")),
        Some("secret123".into())
    );
    assert_eq!(parse_bearer_token(None), None);
    assert_eq!(parse_bearer_token(Some("Basic abc")), None);

    let handle: McpServerHandle = server
        .spawn_http_on_local_port("super-secret".into())
        .await
        .expect("spawn http");

    // No Authorization header -> 401
    let response = http_request(
        handle.addr(),
        "POST",
        "/mcp",
        None,
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 401"), "got {response}");

    // Wrong token -> 401
    let response = http_request(
        handle.addr(),
        "POST",
        "/mcp",
        Some("Bearer nope"),
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 401"), "got {response}");

    // Correct token -> 200
    let response = http_request(
        handle.addr(),
        "POST",
        "/mcp",
        Some("Bearer super-secret"),
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 200"), "got {response}");

    handle.shutdown().await;
}

#[tokio::test]
async fn should_return_contract_artifact_on_session_ready() {
    let dispatch = Arc::new(ScriptedDispatch::with_outcome(sample_ready_outcome()));
    let supervisor = Arc::new(TaskSupervisor::new());
    let server = McpServer::new(dispatch, supervisor);

    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "run_octos_session",
            "arguments": {
                "contract": "slides_delivery",
                "input": {"topic": "Rust 101"}
            }
        }
    });

    let response = server.handle_request(&request, "stdio").await;
    let result = response.get("result").expect("session succeeded");
    let content_arr = result["content"].as_array().expect("content array");
    let body_text = content_arr[0]["text"].as_str().unwrap();
    let parsed: Value = serde_json::from_str(body_text).unwrap();
    assert_eq!(parsed["final_state"], "ready");
    assert_eq!(parsed["artifact_path"], "pf/deck.pptx");
    assert_eq!(parsed["artifact_content"], "MOCK-PPTX-BYTES");
    assert_eq!(parsed["schema_version"], TASK_RESULT_SCHEMA_VERSION);
    let validators = parsed["validator_results"].as_array().unwrap();
    assert_eq!(validators.len(), 1);
    assert_eq!(validators[0]["passed"], true);
    assert!(parsed.get("cost").is_some());
}

#[tokio::test]
async fn should_return_typed_error_on_session_failed() {
    let dispatch = Arc::new(ScriptedDispatch::with_outcome(sample_failed_outcome()));
    let supervisor = Arc::new(TaskSupervisor::new());
    let server = McpServer::new(dispatch, supervisor);

    let request = json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "tools/call",
        "params": {
            "name": "run_octos_session",
            "arguments": {
                "contract": "slides_delivery",
                "input": {"topic": "Rust 101"}
            }
        }
    });

    let response = server.handle_request(&request, "stdio").await;
    // Protocol-level: MCP tools/call should succeed (the server is healthy);
    // the `result.isError = true` flag carries the typed failure.
    let result = response.get("result").expect("still protocol-OK");
    assert_eq!(result["isError"], true);
    let body_text = result["content"][0]["text"].as_str().unwrap();
    let parsed: Value = serde_json::from_str(body_text).unwrap();
    assert_eq!(parsed["final_state"], "failed");
    assert!(
        parsed["error"].as_str().unwrap().contains("slides-sanity"),
        "got {parsed:?}"
    );
    assert!(parsed.get("artifact_path").is_none_or(Value::is_null));
    // Validator list still shipped so the outer orchestrator sees WHY it failed.
    let validators = parsed["validator_results"].as_array().unwrap();
    assert_eq!(validators.len(), 1);
    assert_eq!(validators[0]["passed"], false);
}

#[tokio::test]
async fn should_not_stream_internal_tool_calls_to_mcp_caller() {
    // Dispatch records internal progress (Queued→Running→Verifying) via the
    // observer but the MCP response must contain only the final aggregate
    // result — no per-iteration events, no intermediate tool output.
    let dispatch = Arc::new(ScriptedDispatch::with_outcome(sample_ready_outcome()));
    let supervisor = Arc::new(TaskSupervisor::new());
    let server = McpServer::new(dispatch, supervisor);

    let request = json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "run_octos_session",
            "arguments": {
                "contract": "slides_delivery",
                "input": {"topic": "streaming banned"}
            }
        }
    });

    let response = server.handle_request(&request, "stdio").await;
    let result = response["result"].clone();
    let content = result["content"].as_array().expect("content array");
    assert_eq!(
        content.len(),
        1,
        "MCP response must be a single aggregate content entry, got {content:?}",
    );

    // Shape check: no "events", "trace", "tool_calls" fields leak out.
    let body: Value = serde_json::from_str(content[0]["text"].as_str().unwrap()).unwrap();
    let object = body.as_object().unwrap();
    for forbidden in ["events", "trace", "tool_calls", "iterations", "progress"] {
        assert!(
            !object.contains_key(forbidden),
            "forbidden internal field '{forbidden}' leaked to MCP caller"
        );
    }
}

#[tokio::test]
async fn should_emit_mcp_server_call_event_on_dispatch() {
    let dispatch = Arc::new(ScriptedDispatch::with_outcome(sample_ready_outcome()));
    let supervisor = Arc::new(TaskSupervisor::new());
    let server = McpServer::new(dispatch.clone(), supervisor);

    let events = Arc::new(Mutex::new(Vec::new()));
    let events_clone = events.clone();
    server
        .set_event_sink(move |event: HarnessEvent| {
            let events_clone = events_clone.clone();
            tokio::spawn(async move {
                events_clone.lock().await.push(event);
            });
        })
        .await;

    let request = json!({
        "jsonrpc": "2.0",
        "id": 11,
        "method": "tools/call",
        "params": {
            "name": "run_octos_session",
            "arguments": {
                "contract": "slides_delivery",
                "input": {"topic": "evented"}
            }
        }
    });
    let _ = server.handle_request(&request, "stdio").await;

    // Allow spawned event recorder to land.
    tokio::task::yield_now().await;
    for _ in 0..20 {
        if !events.lock().await.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let events_snap = events.lock().await;
    let mcp_events: Vec<_> = events_snap
        .iter()
        .filter(|event| matches!(event.payload, HarnessEventPayload::McpServerCall { .. }))
        .collect();
    assert!(
        !mcp_events.is_empty(),
        "at least one McpServerCall event must be emitted"
    );
    match &mcp_events[0].payload {
        HarnessEventPayload::McpServerCall { data } => {
            assert_eq!(data.tool, "run_octos_session");
            assert!(!data.caller_id.is_empty());
            assert_eq!(data.outcome, "ready");
        }
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn should_surface_unknown_tool_as_protocol_error() {
    // Only `run_octos_session` is advertised. Any other tool name should
    // yield a JSON-RPC error (method-not-found style) synchronously without
    // dispatching a session.
    let dispatch = Arc::new(ScriptedDispatch::with_outcome(sample_ready_outcome()));
    let supervisor = Arc::new(TaskSupervisor::new());
    let server = McpServer::new(dispatch, supervisor);

    let request = json!({
        "jsonrpc": "2.0",
        "id": 9,
        "method": "tools/call",
        "params": {
            "name": "run_unknown_tool",
            "arguments": {}
        }
    });
    let response = server.handle_request(&request, "stdio").await;
    assert!(
        response.get("error").is_some(),
        "unknown tool should be a protocol error, got {response:?}"
    );
}

#[tokio::test]
async fn should_honour_initialize_response_for_mcp_handshake() {
    let dispatch = Arc::new(ScriptedDispatch::with_outcome(sample_ready_outcome()));
    let supervisor = Arc::new(TaskSupervisor::new());
    let server = McpServer::new(dispatch, supervisor);

    let init = build_initialize_response(&server);
    assert!(init.get("protocolVersion").is_some());
    assert_eq!(init["serverInfo"]["name"], "octos");
    assert_eq!(init["capabilities"]["tools"]["listChanged"], false);
}

#[tokio::test]
async fn should_render_error_to_typed_mcp_response() {
    let rendered = render_mcp_error(json!(1), McpServerError::ProtocolError("bad input".into()));
    assert_eq!(rendered["jsonrpc"], "2.0");
    assert_eq!(rendered["id"], 1);
    let err = rendered.get("error").expect("error field");
    assert_eq!(err["code"], -32600);
    assert!(err["message"].as_str().unwrap().contains("bad input"),);
}

#[tokio::test]
async fn should_directly_dispatch_session_via_helper() {
    // Lower-level helper used by stdio and http transports. Confirms
    // that the workspace-contract-style outcome flows through to the
    // exposed JSON-RPC response.
    let dispatch = Arc::new(ScriptedDispatch::with_outcome(sample_ready_outcome()));
    let supervisor = Arc::new(TaskSupervisor::new());

    let params = json!({
        "name": "run_octos_session",
        "arguments": {"contract": "slides_delivery", "input": {"topic": "helper"}}
    });
    let result = dispatch_run_octos_session(&*dispatch, &supervisor, &params)
        .await
        .expect("dispatch succeeds");
    let body: Value = serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(body["final_state"], "ready");
    assert_eq!(body["schema_version"], TASK_RESULT_SCHEMA_VERSION);
}

// --- tiny HTTP client used by the bearer-auth test ---

async fn http_request(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    auth: Option<&str>,
    body: &str,
) -> String {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let auth_header = auth
        .map(|value| format!("Authorization: {value}\r\n"))
        .unwrap_or_default();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n{auth_header}Content-Length: {len}\r\nConnection: close\r\n\r\n{body}",
        len = body.len(),
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    String::from_utf8_lossy(&buf).to_string()
}
