//! Integration tests for the agent loop using a mock LLM provider.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use octos_agent::{Agent, AgentConfig, ToolRegistry};
use octos_core::{AgentId, Message, ToolCall};
use octos_llm::{ChatConfig, ChatResponse, LlmProvider, StopReason, TokenUsage, ToolSpec};
use octos_memory::EpisodeStore;
use tempfile::TempDir;

/// Mock LLM provider that returns scripted responses in FIFO order.
struct MockLlmProvider {
    responses: Mutex<Vec<ChatResponse>>,
    model: String,
    ctx_window: u32,
}

impl MockLlmProvider {
    fn new(responses: Vec<ChatResponse>) -> Self {
        Self {
            responses: Mutex::new(responses),
            model: "mock-model".to_string(),
            ctx_window: 128_000,
        }
    }

    fn with_context_window(mut self, window: u32) -> Self {
        self.ctx_window = window;
        self
    }
}

#[async_trait]
impl LlmProvider for MockLlmProvider {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[ToolSpec],
        _config: &ChatConfig,
    ) -> eyre::Result<ChatResponse> {
        let mut responses = self.responses.lock().unwrap();
        if responses.is_empty() {
            eyre::bail!("MockLlmProvider: no more scripted responses");
        }
        Ok(responses.remove(0))
    }

    fn context_window(&self) -> u32 {
        self.ctx_window
    }

    fn model_id(&self) -> &str {
        &self.model
    }

    fn provider_name(&self) -> &str {
        "mock"
    }
}

fn end_turn(text: &str, input: u32, output: u32) -> ChatResponse {
    ChatResponse {
        content: Some(text.to_string()),
        reasoning_content: None,
        tool_calls: vec![],
        stop_reason: StopReason::EndTurn,
        usage: TokenUsage {
            input_tokens: input,
            output_tokens: output,
            ..Default::default()
        },
    }
}

fn tool_use(calls: Vec<ToolCall>, input: u32, output: u32) -> ChatResponse {
    ChatResponse {
        content: None,
        reasoning_content: None,
        tool_calls: calls,
        stop_reason: StopReason::ToolUse,
        usage: TokenUsage {
            input_tokens: input,
            output_tokens: output,
            ..Default::default()
        },
    }
}

async fn setup(responses: Vec<ChatResponse>, dir: &TempDir) -> Agent {
    let llm: Arc<dyn LlmProvider> = Arc::new(MockLlmProvider::new(responses));
    let tools = ToolRegistry::with_builtins(dir.path());
    let memory = Arc::new(EpisodeStore::open(dir.path().join(".octos")).await.unwrap());
    Agent::new(AgentId::new("test"), llm, tools, memory).with_config(AgentConfig {
        save_episodes: false,
        ..Default::default()
    })
}

#[tokio::test]
async fn test_agent_simple_response() {
    let dir = TempDir::new().unwrap();
    let agent = setup(vec![end_turn("Hello!", 100, 50)], &dir).await;

    let resp = agent.process_message("Hi", &[], vec![]).await.unwrap();
    assert_eq!(resp.content, "Hello!");
    assert_eq!(resp.token_usage.input_tokens, 100);
    assert_eq!(resp.token_usage.output_tokens, 50);
}

#[tokio::test]
async fn test_agent_tool_call_loop() {
    let dir = TempDir::new().unwrap();

    // Create a file for the agent to read
    std::fs::write(dir.path().join("hello.txt"), "world").unwrap();

    let responses = vec![
        // First response: call read_file tool
        tool_use(
            vec![ToolCall {
                id: "call_1".into(),
                name: "read_file".into(),
                arguments: serde_json::json!({"path": "hello.txt"}),

                metadata: None,
            }],
            200,
            100,
        ),
        // Second response: end turn with answer
        end_turn("The file contains: world", 300, 80),
    ];

    let agent = setup(responses, &dir).await;
    let resp = agent
        .process_message("What's in hello.txt?", &[], vec![])
        .await
        .unwrap();

    assert_eq!(resp.content, "The file contains: world");
    // Tokens accumulated from both iterations
    assert_eq!(resp.token_usage.input_tokens, 500);
    assert_eq!(resp.token_usage.output_tokens, 180);
}

#[tokio::test]
async fn test_agent_max_iterations() {
    let dir = TempDir::new().unwrap();

    // Mock always returns tool use — will hit max iterations
    let responses: Vec<ChatResponse> = (0..5)
        .map(|i| {
            tool_use(
                vec![ToolCall {
                    id: format!("call_{i}"),
                    name: "read_file".into(),
                    arguments: serde_json::json!({"path": "nonexistent.txt"}),

                    metadata: None,
                }],
                100,
                50,
            )
        })
        .collect();

    let agent = setup(responses, &dir).await.with_config(AgentConfig {
        max_iterations: 3,
        save_episodes: false,
        ..Default::default()
    });

    let resp = agent
        .process_message("loop forever", &[], vec![])
        .await
        .unwrap();
    assert_eq!(resp.content, "Reached max iterations.");
}

#[tokio::test]
async fn test_agent_token_budget() {
    let dir = TempDir::new().unwrap();

    let responses = vec![
        // First response uses lots of tokens
        tool_use(
            vec![ToolCall {
                id: "call_1".into(),
                name: "list_dir".into(),
                arguments: serde_json::json!({"path": "."}),

                metadata: None,
            }],
            500,
            500,
        ),
        // Second response would exceed budget
        end_turn("done", 100, 100),
    ];

    let agent = setup(responses, &dir).await.with_config(AgentConfig {
        max_tokens: Some(800),
        save_episodes: false,
        ..Default::default()
    });

    let resp = agent
        .process_message("do stuff", &[], vec![])
        .await
        .unwrap();
    // 500+500 = 1000 tokens after first iteration, exceeds 800 budget
    assert_eq!(resp.content, "Token budget exceeded (1000 of 800).");
}

#[tokio::test]
async fn test_context_trimming() {
    let dir = TempDir::new().unwrap();

    // Use tiny context window to force trimming
    let llm: Arc<dyn LlmProvider> =
        Arc::new(MockLlmProvider::new(vec![end_turn("OK", 50, 20)]).with_context_window(500));
    let tools = ToolRegistry::with_builtins(dir.path());
    let memory = Arc::new(EpisodeStore::open(dir.path().join(".octos")).await.unwrap());

    let agent = Agent::new(AgentId::new("test"), llm, tools, memory).with_config(AgentConfig {
        save_episodes: false,
        ..Default::default()
    });

    // Build long history — each message ~100 chars (~25 tokens)
    let history: Vec<Message> = (0..30)
        .map(|i| Message {
            role: octos_core::MessageRole::User,
            content: format!("This is message number {} with some padding text to make it longer for token estimation purposes.", i),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            timestamp: chrono::Utc::now(),
        })
        .collect();

    // Should not panic — trimming kicks in to fit 500 token window
    let resp = agent
        .process_message("final question", &history, vec![])
        .await
        .unwrap();
    assert_eq!(resp.content, "OK");
}
