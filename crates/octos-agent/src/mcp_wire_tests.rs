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
