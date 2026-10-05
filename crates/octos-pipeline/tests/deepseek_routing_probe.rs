//! Live tool-selection check for native research/crawl routing.
//! Run with DEEPSEEK_API_KEY set and --ignored; no tools are executed.

use std::path::PathBuf;
use std::sync::Arc;

use octos_agent::Tool;
use octos_agent::{WebFetchTool, WebSearchTool};
use octos_core::Message;
use octos_llm::openai::OpenAIProvider;
use octos_llm::{ChatConfig, LlmProvider, ToolChoice, ToolSpec};
use octos_memory::EpisodeStore;
use octos_pipeline::RunPipelineTool;

fn spec_of(t: &dyn Tool) -> ToolSpec {
    ToolSpec {
        name: t.name().into(),
        description: t.description().into(),
        input_schema: t.input_schema(),
    }
}

fn bundled_spec(source: &str) -> ToolSpec {
    let manifest: serde_json::Value = serde_json::from_str(source).unwrap();
    let tool = &manifest["tools"][0];
    let mut schema = tool["input_schema"].clone();
    // The runtime hides host-owned credentials from the model's schema.
    schema["properties"]
        .as_object_mut()
        .unwrap()
        .remove("synthesis_config");
    ToolSpec {
        name: tool["name"].as_str().unwrap().into(),
        description: tool["description"].as_str().unwrap().into(),
        input_schema: schema,
    }
}

#[tokio::test]
#[ignore = "requires DEEPSEEK_API_KEY (real LLM); native tool-selection probe"]
async fn deepseek_routes_research_and_crawl_to_native_tools() {
    let key = std::env::var("DEEPSEEK_API_KEY").expect("DEEPSEEK_API_KEY required");
    let provider: Arc<dyn LlmProvider> = Arc::new(
        OpenAIProvider::new(key, "deepseek-v4-flash").with_base_url("https://api.deepseek.com/v1"),
    );
    let working = tempfile::TempDir::new().unwrap();
    let data = tempfile::TempDir::new().unwrap();
    let memory = Arc::new(
        EpisodeStore::open(data.path().join(".octos"))
            .await
            .unwrap(),
    );
    let pipeline = RunPipelineTool::new(
        provider.clone(),
        memory,
        PathBuf::from(working.path()),
        PathBuf::from(data.path()),
    );
    let tools = vec![
        spec_of(&pipeline),
        spec_of(&WebSearchTool::new()),
        spec_of(&WebFetchTool::new()),
        bundled_spec(include_str!("../../app-skills/deep-search/manifest.json")),
        bundled_spec(include_str!("../../app-skills/deep-crawl/manifest.json")),
    ];
    for (prompt, expected) in [
        (
            "Do deep research on PDF text extraction limitations and write a cited report.",
            "search",
        ),
        (
            "深度调研开源 PDF 解析工具的功能和局限，整理成带来源的报告。",
            "search",
        ),
        (
            "Crawl https://docs.python.org/3/library/json.html, at most 1 page, depth 0.",
            "deep_crawl",
        ),
        (
            "Quick web lookup: what is the current stable Rust version?",
            "web_search",
        ),
    ] {
        let response = provider.chat(
            &[Message::system("Choose the single most appropriate tool for the request. Call exactly one tool."), Message::user(prompt)],
            &tools,
            &ChatConfig { max_tokens: Some(1200), temperature: Some(0.0), tool_choice: ToolChoice::Auto, ..Default::default() },
        ).await.unwrap();
        let choice = response.tool_calls.first().map(|call| call.name.as_str());
        eprintln!("expected={expected} actual={choice:?}");
        assert_eq!(choice, Some(expected), "wrong tool for {prompt}");
    }
}
