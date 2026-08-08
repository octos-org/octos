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
use octos_agent::arc_task::{ARC_AGENT_TASK_SCHEMA_V1, parse_arc_agent_task_input};
use octos_agent::harness_events::HarnessEventPayload;
use octos_agent::mcp_server::{
    McpServer, McpServerError, McpSessionCost, McpSessionDispatch, McpSessionOutcome,
    SessionLifecycleObserver, build_initialize_response, build_tools_list_response,
    constant_time_eq, dispatch_run_octos_session, parse_bearer_token, render_mcp_error,
};
use octos_agent::task_supervisor::{TaskLifecycleState, TaskSupervisor};
use octos_agent::validators::{ValidatorOutcome, ValidatorPhase, ValidatorStatus};
use octos_agent::{HarnessEvent, TASK_RESULT_SCHEMA_VERSION};
use serde_json::{Value, json};
use tempfile::TempDir;
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
        validator_results: vec![sample_validator_outcome(ValidatorStatus::Pass, "ok")],
        cost: McpSessionCost {
            input_tokens: 100,
            output_tokens: 42,
            ..Default::default()
        },
        error: None,
    }
}

fn sample_failed_outcome() -> McpSessionOutcome {
    McpSessionOutcome {
        final_state: TaskLifecycleState::Failed,
        artifact_path: None,
        artifact_content: None,
        validator_results: vec![sample_validator_outcome(
            ValidatorStatus::Fail,
            "slide count too low",
        )],
        cost: McpSessionCost {
            input_tokens: 40,
            output_tokens: 12,
            ..Default::default()
        },
        error: Some("contract gate failed: slides-sanity".to_string()),
    }
}

fn sample_arc_task(workspace: &std::path::Path) -> Value {
    json!({
        "schema": ARC_AGENT_TASK_SCHEMA_V1,
        "task_id": "REQ-1:DESIGN:InterfaceDesigner",
        "stage": "InterfaceDesigner",
        "backend_agent_name": "interface_designer",
        "node_id": "REQ-1",
        "phase": "DESIGN",
        "app_type": "web",
        "workspace_root": workspace.display().to_string(),
        "requirement_path": workspace.join("requirements/requirements.yaml").display().to_string(),
        "thread_id": "REQ-1:DESIGN:InterfaceDesigner",
        "test_type": "",
        "system_prompt": "You are ARC's interface designer.",
        "message": "Design the booking interface.",
        "response_schema": {
            "type": "object",
            "required": ["summary"],
            "properties": {
                "summary": {"type": "string"}
            }
        },
        "inputs": {
            "requirement": {"id": "REQ-1", "title": "Book a ticket"}
        },
        "acceptance": {
            "response_schema_required": true,
            "artifact_kind": "interface_design"
        },
        "skills": ["/skills/leaf-full-design/"]
    })
}

fn sample_validator_outcome(status: ValidatorStatus, reason: &str) -> ValidatorOutcome {
    ValidatorOutcome {
        schema_version: 1,
        validator_id: "slides-sanity".into(),
        phase: ValidatorPhase::Completion,
        kind: "command".into(),
        repo_label: "mcp-serve/slides_delivery".into(),
        required: true,
        required_tier: "hard".into(),
        status,
        reason: reason.into(),
        duration_ms: 1,
        evidence_path: None,
        stderr: None,
        started_at: chrono::Utc::now(),
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

#[test]
fn run_octos_session_schema_advertises_arc_agent_task_v1() {
    let dispatch = Arc::new(ScriptedDispatch::with_outcome(sample_ready_outcome()));
    let supervisor = Arc::new(TaskSupervisor::new());
    let server = McpServer::new(dispatch, supervisor);

    let tools = build_tools_list_response(&server);
    let tool = &tools["tools"][0];
    let schema = &tool["inputSchema"];
    let top_level_required: Vec<&str> = schema["required"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(top_level_required, vec!["contract", "input"]);

    let arc_task = &schema["properties"]["input"]["properties"]["arc_task"];
    assert_eq!(arc_task["type"], "object", "input schema: {schema}");
    assert_eq!(
        arc_task["properties"]["schema"]["const"],
        ARC_AGENT_TASK_SCHEMA_V1
    );
    let required: Vec<&str> = arc_task["required"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    for field in [
        "schema",
        "task_id",
        "stage",
        "node_id",
        "phase",
        "workspace_root",
        "requirement_path",
        "system_prompt",
        "message",
        "inputs",
        "acceptance",
    ] {
        assert!(required.contains(&field), "missing required field {field}");
    }
}

#[test]
fn arc_agent_task_v1_parses_and_preserves_structured_fields() {
    let workspace = TempDir::new().unwrap();
    let input = json!({
        "arc_task": sample_arc_task(workspace.path()),
        "expected_artifact": ".arc/delegated/interface-designer-REQ-1.json",
        "artifact_name": "arc-stage-result"
    });

    let request = parse_arc_agent_task_input(&input, workspace.path())
        .expect("valid ARC task")
        .expect("ARC task present");

    assert_eq!(request.task.schema, ARC_AGENT_TASK_SCHEMA_V1);
    assert_eq!(request.task.task_id, "REQ-1:DESIGN:InterfaceDesigner");
    assert_eq!(request.task.inputs["requirement"]["id"], "REQ-1");
    assert_eq!(request.task.acceptance["artifact_kind"], "interface_design");
    assert_eq!(
        request.task.response_schema.as_ref().unwrap()["type"],
        "object"
    );
    assert_eq!(request.task.skills, vec!["/skills/leaf-full-design/"]);
    assert_eq!(
        request.execution_params()["requirement_path"],
        workspace
            .path()
            .join("requirements/requirements.yaml")
            .display()
            .to_string()
    );
    assert_eq!(
        request.expected_artifact,
        std::path::PathBuf::from(".arc/delegated/interface-designer-REQ-1.json")
    );
}

#[test]
fn arc_agent_task_v1_rejects_invalid_schema_and_workspace_escape() {
    let workspace = TempDir::new().unwrap();

    let mut wrong_schema = sample_arc_task(workspace.path());
    wrong_schema["schema"] = json!("arc.agent-task.v2");
    let error = parse_arc_agent_task_input(
        &json!({
            "arc_task": wrong_schema,
            "expected_artifact": ".arc/delegated/result.json"
        }),
        workspace.path(),
    )
    .unwrap_err();
    assert!(error.to_string().starts_with("arc_task_invalid:"));

    let other_workspace = TempDir::new().unwrap();
    let mismatched_workspace = sample_arc_task(other_workspace.path());
    let error = parse_arc_agent_task_input(
        &json!({
            "arc_task": mismatched_workspace,
            "expected_artifact": ".arc/delegated/result.json"
        }),
        workspace.path(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("workspace_root"));

    let escaping_artifact = sample_arc_task(workspace.path());
    let error = parse_arc_agent_task_input(
        &json!({
            "arc_task": escaping_artifact,
            "expected_artifact": "../outside.json"
        }),
        workspace.path(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("expected_artifact"));

    let mut missing_required_response_schema = sample_arc_task(workspace.path());
    missing_required_response_schema["response_schema"] = Value::Null;
    let error = parse_arc_agent_task_input(
        &json!({
            "arc_task": missing_required_response_schema,
            "expected_artifact": ".arc/delegated/result.json"
        }),
        workspace.path(),
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("acceptance.response_schema_required")
    );
}

#[test]
fn bearer_token_parsing_and_constant_time_comparison() {
    // The HTTP transport's bearer gate is built from these two primitives (the
    // CLI's axum middleware pairs them). The end-to-end 401/allow behavior of
    // the mounted Streamable HTTP service is covered by the octos-cli HTTP
    // integration test, where axum lives.
    assert_eq!(
        parse_bearer_token(Some("Bearer secret123")),
        Some("secret123".into())
    );
    // Case-insensitive scheme, collapses extra whitespace (RFC 6750 §2.1).
    assert_eq!(
        parse_bearer_token(Some("bearer  secret123")),
        Some("secret123".into())
    );
    assert_eq!(parse_bearer_token(None), None);
    assert_eq!(parse_bearer_token(Some("Basic abc")), None);
    assert_eq!(parse_bearer_token(Some("Bearer ")), None);

    assert!(constant_time_eq("super-secret", "super-secret"));
    assert!(!constant_time_eq("super-secret", "super-secre"));
    assert!(!constant_time_eq("super-secret", "wrong-secret"));
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
    assert_eq!(validators[0]["validator_id"], "slides-sanity");
    assert_eq!(validators[0]["status"], "pass");
    assert_eq!(parsed["cost"]["input_tokens"], 100);
    assert_eq!(parsed["cost"]["output_tokens"], 42);
    assert_eq!(parsed["cost"]["reasoning_tokens"], 0);
    assert_eq!(parsed["cost"]["cache_read_tokens"], 0);
    assert_eq!(parsed["cost"]["cache_write_tokens"], 0);
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
    assert_eq!(validators[0]["validator_id"], "slides-sanity");
    assert_eq!(validators[0]["status"], "fail");
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
