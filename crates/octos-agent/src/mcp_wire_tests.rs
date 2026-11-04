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

/// R-12: an MCP stdio server emitting a frame past the 1 MiB cap must fail
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
        "#!/bin/sh\n# Emit >1MiB without a newline, then idle.\nhead -c 2000000 /dev/zero | tr '\\0' 'a'\nsleep 5\n",
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
    let error = McpClient::connect_stdio(&config)
        .await
        .expect_err("an oversized frame must fail the connection");
    // rmcp normalizes the transport's io error into a generic "connection
    // closed" initialize failure, so the observable contract here is the
    // timing: the bounded reader fires as soon as >1MiB arrives, while the
    // unbounded transport kept buffering until the child exits (the script
    // idles 5s after emitting the garbage).
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "oversized frame must fail promptly (took {:?}); unbounded buffering \
         would wait for the newline until the child exits",
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
