//! Exercise the real read tool, agent loop and provider serializer together.
//! No external model call is required: the HTTP boundary captures actual JSON.
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use octos_agent::sandbox::NoSandbox;
use octos_agent::tools::coding_tools::BashTool;
use octos_agent::{Agent, AgentConfig, EditFileTool, ReadFileTool, ToolRegistry, WriteFileTool};
use octos_core::AgentId;
use octos_llm::openai::{ModelHints, OpenAIProvider};
use octos_memory::EpisodeStore;
use serde_json::{Value, json};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

#[tokio::test]
async fn four_tools_deliver_image_pixels_and_retain_them_after_another_read() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("screen.png"), b"\x89PNG\r\n\x1a\n").unwrap();
    std::fs::write(workspace.path().join("notes.txt"), "second read").unwrap();
    let server = MockServer::start().await;
    let sequence = Arc::new(AtomicUsize::new(0));
    let original_image = workspace.path().join("screen.png");
    Mock::given(method("POST")).and(path("/chat/completions"))
        .respond_with(move |request: &wiremock::Request| {
            let n = sequence.fetch_add(1, Ordering::SeqCst);
            if n == 1 {
                // A later tool regenerates the same filename. Historical wire
                // pixels must remain unchanged in the subsequent request.
                std::fs::write(&original_image, b"\x89PNG\r\n\x1a\nchanged").unwrap();
            }
            let mut message = if n < 2 {
                json!({"role":"assistant", "content":null, "tool_calls":[{
                    "id":format!("read_{n}"), "type":"function", "function":{
                        "name":"read_file", "arguments":json!({"path":if n == 0 {"screen.png"} else {"notes.txt"}}).to_string()
                    }
                }]})
            } else { json!({"role":"assistant","content":"done"}) };
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            if body["stream"] == true {
                if n < 2 { message["tool_calls"][0]["index"] = json!(0); }
                let chunk = json!({"choices":[{"index":0,"delta":message,
                    "finish_reason":if n < 2 {"tool_calls"} else {"stop"}}]});
                ResponseTemplate::new(200).append_header("Content-Type", "text/event-stream")
                    .set_body_string(format!("data: {chunk}\n\ndata: [DONE]\n\n"))
            } else {
                ResponseTemplate::new(200).set_body_json(json!({
                    "choices":[{"message":message,"finish_reason":if n < 2 {"tool_calls"} else {"stop"}}],
                    "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
                }))
            }
        }).expect(3).mount(&server).await;
    let mut tools = ToolRegistry::new();
    tools.register(ReadFileTool::new(workspace.path()));
    tools.register(WriteFileTool::new(workspace.path()));
    tools.register(EditFileTool::new(workspace.path()));
    tools.register(BashTool::new(workspace.path(), Arc::new(NoSandbox)));
    let mut hints = ModelHints::detect("deepseek-v4-flash");
    hints.retain_tool_images = true;
    let provider = Arc::new(
        OpenAIProvider::new("test", "deepseek-v4-flash")
            .with_base_url(server.uri())
            .with_hints(hints),
    );
    let memory = Arc::new(
        EpisodeStore::open(workspace.path().join(".memory"))
            .await
            .unwrap(),
    );
    let agent = Agent::new(AgentId::new("read-image-parity"), provider, tools, memory).with_config(
        AgentConfig {
            save_episodes: false,
            ..Default::default()
        },
    );
    let result = agent
        .process_message("Read screen.png, then notes.txt.", &[], vec![])
        .await
        .unwrap();
    assert_eq!(result.content, "done");
    let requests = server.received_requests().await.unwrap();
    let bodies: Vec<Value> = requests
        .iter()
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    for body in &bodies {
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 4);
        assert!(tools.iter().all(|t| t["function"]["name"] != "view_image"));
    }
    let first_view = bodies[1]["messages"].as_array().unwrap();
    let later_view = bodies[2]["messages"].as_array().unwrap();
    assert_eq!(&later_view[..first_view.len()], first_view.as_slice());
    for body in &bodies[1..] {
        let urls: Vec<_> = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|m| m["content"].as_array())
            .flatten()
            .filter_map(|p| p["image_url"]["url"].as_str())
            .collect();
        assert_eq!(
            urls,
            ["data:image/png;base64,iVBORw0KGgo="],
            "tool rows: {:?}",
            body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|m| m["role"] == "tool")
                .collect::<Vec<_>>()
        );
    }
}
