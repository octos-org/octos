//! Actual MCP transport -> adapter -> agent history -> main-model HTTP request.
use super::*;
use octos_core::AgentId;
use octos_llm::openai::{ModelHints, OpenAIProvider};
use octos_memory::EpisodeStore;
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::service::{RequestContext, serve_client, serve_server};
use rmcp::{RoleServer, ServerHandler};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

/// T-0241: a mock MCP server that parks every `tools/call` until released
/// and records every `notifications/cancelled` it receives, so the client
/// can prove the exact in-flight request id was cancelled.
struct CancelRecordingServer {
    cancelled: Arc<std::sync::Mutex<Vec<rmcp::model::CancelledNotificationParam>>>,
    released: Arc<AtomicUsize>,
}

impl ServerHandler for CancelRecordingServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn call_tool(
        &self,
        _request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> std::result::Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
        // Park until the test releases us — a slow remote call. The LATE
        // result lets the client prove the cancelled transcript stands.
        while self.released.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        Ok(
            serde_json::from_value(json!({"content":[{"type":"text","text":"LATE_RESULT"}]}))
                .unwrap(),
        )
    }

    async fn on_cancelled(
        &self,
        notification: rmcp::model::CancelledNotificationParam,
        _context: rmcp::service::NotificationContext<RoleServer>,
    ) {
        self.cancelled.lock().unwrap().push(notification);
    }
}

async fn cancel_recording_pair() -> (
    Arc<RunningService<RoleClient, ClientInfo>>,
    Arc<std::sync::Mutex<Vec<rmcp::model::CancelledNotificationParam>>>,
    Arc<AtomicUsize>,
    tokio::task::JoinHandle<()>,
) {
    let cancelled = Arc::new(std::sync::Mutex::new(Vec::new()));
    let released = Arc::new(AtomicUsize::new(0));
    let server = CancelRecordingServer {
        cancelled: cancelled.clone(),
        released: released.clone(),
    };
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let server_task = tokio::spawn(async move {
        serve_server(server, tokio::io::split(server_io))
            .await
            .unwrap()
            .waiting()
            .await
            .unwrap();
    });
    let service = Arc::new(
        serve_client(ClientInfo::default(), tokio::io::split(client_io))
            .await
            .unwrap(),
    );
    (service, cancelled, released, server_task)
}

fn cancel_recording_tool(service: Arc<RunningService<RoleClient, ClientInfo>>) -> McpTool {
    McpTool {
        name: "slow".into(),
        description: "A slow remote call".into(),
        input_schema: json!({"type": "object", "properties": {}}),
        service,
        concurrency_class: crate::tools::ConcurrencyClass::Safe,
        workspace: None,
    }
}

#[tokio::test]
async fn turn_cancel_sends_cancelled_notification_for_in_flight_mcp_call() {
    let (service, cancelled, released, _server_task) = cancel_recording_pair().await;
    let tool = cancel_recording_tool(service.clone());
    let token = tokio_util::sync::CancellationToken::new();
    let ctx = crate::tools::ToolContext {
        turn_cancel: Some(token.clone()),
        ..crate::tools::ToolContext::zero()
    };
    let call = tokio::spawn(async move { tool.execute_with_context(&ctx, &json!({})).await });

    // Let the request land on the wire, then interrupt the turn.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    token.cancel();
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), call)
        .await
        .expect("the cancelled call must return promptly")
        .unwrap()
        .unwrap();

    // Honest text: never claims the remote work stopped.
    assert!(!result.success);
    assert!(
        result.output.contains("notifications/cancelled"),
        "the result must name the cancellation that was sent: {}",
        result.output
    );
    assert!(
        result.output.contains("may still finish"),
        "the result must stay honest about the remote side: {}",
        result.output
    );

    // The server received exactly one cancellation, for the in-flight id.
    let seen = cancelled.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "exactly one cancellation expected: {seen:?}");
    assert!(
        seen[0].request_id.is_some(),
        "the cancellation must reference the in-flight request id"
    );

    // The remote side finishes late; the cancelled transcript already
    // returned above is untouched, and the connection keeps working.
    released.store(1, Ordering::SeqCst);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(
        result.output.contains("notifications/cancelled"),
        "a late remote result must not overwrite the cancelled result"
    );
    // A normal follow-up call on the same connection still works.
    let tool = cancel_recording_tool(service);
    let followup = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tool.execute_with_context(&crate::tools::ToolContext::zero(), &json!({})),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        followup.output.contains("LATE_RESULT"),
        "a normal call after the cancellation still renders server content: {}",
        followup.output
    );
    // And it produced NO further cancellation notification.
    assert_eq!(
        cancelled.lock().unwrap().len(),
        1,
        "a call without a turn cancel must not send notifications/cancelled"
    );
}

#[tokio::test]
async fn normal_mcp_call_sends_no_cancellation() {
    let (service, cancelled, released, _server_task) = cancel_recording_pair().await;
    released.store(1, Ordering::SeqCst); // the call completes immediately
    let tool = cancel_recording_tool(service);
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tool.execute_with_context(&crate::tools::ToolContext::zero(), &json!({})),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        result.output.contains("LATE_RESULT"),
        "the normal call returns the server result: {}",
        result.output
    );
    assert!(
        cancelled.lock().unwrap().is_empty(),
        "no in-flight cancellation must be sent for a normal call"
    );
}

struct ScreenshotServer;
impl ServerHandler for ScreenshotServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn call_tool(
        &self,
        _request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> std::result::Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
        Ok(serde_json::from_value(json!({"content":[
            {"type":"text","text":"SCREENSHOT_RESULT"},
            {"type":"image","mimeType":"image/png","data":"iVBORw0KGgo="},
            {"type":"resource","resource":{"uri":"report://screen","text":"RESOURCE_RESULT"}}
        ]}))
        .unwrap())
    }
}

#[tokio::test]
async fn should_send_mcp_screenshot_pixels_to_the_main_model() {
    let workspace = tempfile::tempdir().unwrap();
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let server_task = tokio::spawn(async move {
        serve_server(ScreenshotServer, tokio::io::split(server_io))
            .await
            .unwrap()
            .waiting()
            .await
            .unwrap();
    });
    let service = Arc::new(
        serve_client(ClientInfo::default(), tokio::io::split(client_io))
            .await
            .unwrap(),
    );
    let cancel = service.cancellation_token();
    let mut registry = ToolRegistry::new();
    registry.register(McpTool {
        name: "screenshot".into(),
        description: "Take a screenshot".into(),
        input_schema: json!({"type":"object","properties":{}}),
        service,
        concurrency_class: crate::tools::ConcurrencyClass::Safe,
        workspace: Some(workspace.path().to_path_buf()),
    });

    let http = MockServer::start().await;
    let sequence = AtomicUsize::new(0);
    Mock::given(method("POST")).and(path("/chat/completions"))
        .respond_with(move |request: &wiremock::Request| {
            let first = sequence.fetch_add(1, Ordering::SeqCst) == 0;
            let mut message = if first {
                json!({"role":"assistant","content":null,"tool_calls":[{
                    "id":"call_screen_1","type":"function","function":{"name":"screenshot","arguments":"{}"}
                }]})
            } else { json!({"role":"assistant","content":"done"}) };
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            let reason = if first { "tool_calls" } else { "stop" };
            if body["stream"] == true {
                if first { message["tool_calls"][0]["index"] = json!(0); }
                let chunk = json!({"choices":[{"index":0,"delta":message,"finish_reason":reason}]});
                ResponseTemplate::new(200).append_header("Content-Type", "text/event-stream")
                    .set_body_string(format!("data: {chunk}\n\ndata: [DONE]\n\n"))
            } else {
                ResponseTemplate::new(200).set_body_json(json!({"choices":[{"message":message,"finish_reason":reason}]}))
            }
        }).expect(2).mount(&http).await;
    let mut hints = ModelHints::detect("deepseek-v4-flash");
    hints.retain_tool_images = true;
    let provider = Arc::new(
        OpenAIProvider::new("test", "deepseek-v4-flash")
            .with_base_url(http.uri())
            .with_hints(hints),
    );
    let memory = Arc::new(
        EpisodeStore::open(workspace.path().join(".memory"))
            .await
            .unwrap(),
    );
    let agent = crate::Agent::new(AgentId::new("mcp-image-test"), provider, registry, memory)
        .with_config(crate::AgentConfig {
            save_episodes: false,
            ..Default::default()
        });
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        agent.process_message("Take a screenshot.", &[], vec![]),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result.content, "done");
    let requests = http.received_requests().await.unwrap();
    let next: Value = serde_json::from_slice(&requests[1].body).unwrap();
    let messages = next["messages"].as_array().unwrap();
    assert!(
        messages.iter().any(|m| m["role"] == "tool"
            && m["tool_call_id"] == "call_screen_1"
            && m.to_string().contains("RESOURCE_RESULT")),
        "{messages:?}"
    );
    let urls: Vec<_> = messages
        .iter()
        .filter_map(|m| m["content"].as_array())
        .flatten()
        .filter_map(|p| p["image_url"]["url"].as_str())
        .collect();
    assert_eq!(urls, ["data:image/png;base64,iVBORw0KGgo="]);
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(5), server_task)
        .await
        .unwrap()
        .unwrap();
}

/// R-12: an MCP stdio server emitting a frame past the frame cap must fail
/// the connection loudly instead of letting rmcp's unbounded `read_until`
/// buffer it (memory growth before schema validation).
#[cfg(unix)]
#[tokio::test]
async fn stdio_server_oversized_frame_fails_the_connection() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("oversized-server.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\n# Emit more than the cap without a newline, then idle.\ndd if=/dev/zero bs=65536 count=1024 2>/dev/null\nsleep 5\n",
    )
    .unwrap();
    let mut perms = std::fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).unwrap();

    let config = McpServerConfig {
        command: Some(script.to_str().unwrap().to_string()),
        args: vec![],
        env: HashMap::new(),
        url: None,
        headers: HashMap::new(),
        oauth: false,
        scopes: vec![],
        concurrency_class: None,
        id: None,
        name: None,
        enabled: true,
        allowlist_entry_id: None,
    };

    let start = std::time::Instant::now();
    // The child idles for 5s after emitting the oversized frame, so an
    // unbounded reader would wait for the newline until the child exits.
    // Wrap the connect in a timeout SHORTER than that idle period: a
    // timeout here means the cap did NOT fire and the transport hung.
    let error = tokio::time::timeout(Duration::from_secs(4), McpClient::connect_stdio(&config))
        .await
        .expect("the frame cap must fire before the child exits")
        .expect_err("an oversized frame must fail the connection");
    // rmcp normalizes the transport's io error into a generic "connection
    // closed" initialize failure, so the observable contract here is the
    // timing: the bounded reader fires as soon as the cap is passed, while the
    // unbounded transport kept buffering until the child exits. The timeout
    // above (4s < the child's 5s idle) is the hard contract; the elapsed
    // check below is a soft guard against pathological slowness on loaded
    // CI runners — it must not be so tight that scheduler delay alone
    // fails an otherwise-correct cap.
    assert!(
        start.elapsed() < Duration::from_secs(4),
        "oversized frame must fail before the child exits (took {:?}); \
         unbounded buffering would wait for the newline until the child exits",
        start.elapsed()
    );
    assert!(
        format!("{error:?}").contains("initialize"),
        "expected an initialize-time failure, got: {error:?}"
    );
}

/// R-12: the frame cap fires at the read boundary itself — bytes past the
/// cap without a newline fail the read with the bounded-frame error, while
/// well-formed newline-delimited frames pass through untouched.
#[tokio::test]
async fn bounded_line_read_fails_oversized_frame() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (mut writer, reader) = tokio::io::duplex(MCP_MAX_FRAME_BYTES + 4096);
    let mut bounded = BoundedLineRead::new(reader);

    // Two well-formed frames pass through first.
    writer
        .write_all(b"{\"jsonrpc\":\"2.0\"}\n{}\n")
        .await
        .unwrap();
    let mut small = vec![0u8; 18];
    bounded.read_exact(&mut small).await.unwrap();
    assert_eq!(&small, b"{\"jsonrpc\":\"2.0\"}\n");

    // Then a frame with more than MCP_MAX_FRAME_BYTES before any newline.
    let big = vec![b'a'; MCP_MAX_FRAME_BYTES + 1];
    writer.write_all(&big).await.unwrap();
    let mut sink = vec![0u8; MCP_MAX_FRAME_BYTES + 256];
    let error = bounded
        .read(&mut sink)
        .await
        .expect_err("oversized frame must fail the read");
    assert!(
        error.to_string().contains("oversized MCP frame"),
        "unexpected error: {error}"
    );
}

/// R-12: a frame exactly at the cap followed by its newline is still valid.
#[tokio::test]
async fn bounded_line_read_allows_frame_at_cap() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (mut writer, reader) = tokio::io::duplex(MCP_MAX_FRAME_BYTES + 4096);
    let mut bounded = BoundedLineRead::new(reader);

    let mut frame = vec![b'a'; MCP_MAX_FRAME_BYTES];
    frame.push(b'\n');
    writer.write_all(&frame).await.unwrap();
    let mut out = vec![0u8; MCP_MAX_FRAME_BYTES + 1];
    bounded
        .read_exact(&mut out)
        .await
        .expect("cap-sized frame reads");
    assert_eq!(out, frame);
}
