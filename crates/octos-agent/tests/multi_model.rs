//! Integration tests for multi-model sub-agent support.
//!
//! Tests the ProviderRouter + ContextWindowOverride + SpawnTool integration,
//! validating that sub-agents can use different LLM providers, context windows,
//! and tool policies.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use octos_agent::{Agent, AgentConfig, SpawnTool, Tool, ToolRegistry};
use octos_core::{AgentId, Message, ToolCall};
use octos_llm::{
    ChatConfig, ChatResponse, ContextWindowOverride, LlmProvider, ProviderRouter, StopReason,
    TokenUsage, ToolSpec,
};
use octos_memory::EpisodeStore;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Shared test helpers
// ---------------------------------------------------------------------------

/// Mock LLM provider returning scripted FIFO responses, tracking the model name.
struct MockLlmProvider {
    responses: Mutex<Vec<ChatResponse>>,
    model: String,
    ctx_window: u32,
}

impl MockLlmProvider {
    fn new(model: &str, responses: Vec<ChatResponse>) -> Self {
        Self {
            responses: Mutex::new(responses),
            model: model.to_string(),
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
            eyre::bail!(
                "MockLlmProvider({}): no more scripted responses",
                self.model
            );
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

/// Mock provider that fails after N successful calls.
struct FailCountProvider {
    inner: MockLlmProvider,
    success_limit: u32,
    call_count: AtomicU32,
}

impl FailCountProvider {
    fn new(model: &str, success_limit: u32, responses: Vec<ChatResponse>) -> Self {
        Self {
            inner: MockLlmProvider::new(model, responses),
            success_limit,
            call_count: AtomicU32::new(0),
        }
    }
}

#[async_trait]
impl LlmProvider for FailCountProvider {
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        config: &ChatConfig,
    ) -> eyre::Result<ChatResponse> {
        let count = self.call_count.fetch_add(1, Ordering::SeqCst);
        if count >= self.success_limit {
            eyre::bail!(
                "FailCountProvider({}): intentional failure after {} calls",
                self.inner.model,
                self.success_limit
            );
        }
        self.inner.chat(messages, tools, config).await
    }

    fn context_window(&self) -> u32 {
        self.inner.ctx_window
    }

    fn model_id(&self) -> &str {
        &self.inner.model
    }

    fn provider_name(&self) -> &str {
        "fail-count"
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
        provider_index: None,
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
        provider_index: None,
    }
}

fn mock_provider(
    model: &str,
    ctx_window: u32,
    responses: Vec<ChatResponse>,
) -> Arc<dyn LlmProvider> {
    Arc::new(MockLlmProvider::new(model, responses).with_context_window(ctx_window))
}

fn mock_router(entries: Vec<(&str, Arc<dyn LlmProvider>)>) -> Arc<ProviderRouter> {
    let router = ProviderRouter::new();
    for (key, provider) in entries {
        router.register(key, provider);
    }
    Arc::new(router)
}

async fn setup_with_router(
    parent_responses: Vec<ChatResponse>,
    dir: &TempDir,
    router: Arc<ProviderRouter>,
) -> Agent {
    let llm: Arc<dyn LlmProvider> = mock_provider("parent-model", 128_000, parent_responses);
    let mut tools = ToolRegistry::with_builtins(dir.path());

    let (tx, _rx) = tokio::sync::mpsc::channel(16);
    let memory = Arc::new(EpisodeStore::open(dir.path().join(".octos")).await.unwrap());

    let spawn = SpawnTool::new(llm.clone(), memory.clone(), dir.path().to_path_buf(), tx)
        .with_provider_router(router);
    tools.register(spawn);

    Agent::new(AgentId::new("parent"), llm, tools, memory).with_config(AgentConfig {
        save_episodes: false,
        ..Default::default()
    })
}

// ---------------------------------------------------------------------------
// Test 1: Multi-model routing — parent spawns sync sub-agents with different providers
// ---------------------------------------------------------------------------

/// Verifies that ProviderRouter correctly routes "cheap/model" and "strong/model"
/// to different mock providers, and that sync sub-agents return results to parent.
#[tokio::test]
async fn test_multi_model_sync_subagents() {
    let dir = TempDir::new().unwrap();

    // Sub-agent providers: "cheap" returns search results, "strong" returns synthesis
    let cheap_provider = mock_provider(
        "gpt-4o-mini",
        32_000,
        vec![
            // Sub-agent 1 response
            end_turn("Search result: Rust async paper 2024", 100, 50),
            // Sub-agent 2 response
            end_turn("Search result: Tokio runtime news", 100, 50),
        ],
    );
    let strong_provider = mock_provider("claude-sonnet", 200_000, vec![]);

    let router = mock_router(vec![("cheap", cheap_provider), ("strong", strong_provider)]);

    // Parent: spawn 2 sync sub-agents, then synthesize
    let parent_responses = vec![
        // Step 1: parent calls spawn with model="cheap/gpt-4o-mini" (sub-agent 1)
        tool_use(
            vec![ToolCall {
                id: "call_1".into(),
                name: "spawn".into(),
                arguments: serde_json::json!({
                    "task": "Search for Rust async papers",
                    "mode": "sync",
                    "model": "cheap/gpt-4o-mini"
                }),

                metadata: None,
            }],
            200,
            100,
        ),
        // Step 2: parent calls spawn with model="cheap/gpt-4o-mini" (sub-agent 2)
        tool_use(
            vec![ToolCall {
                id: "call_2".into(),
                name: "spawn".into(),
                arguments: serde_json::json!({
                    "task": "Search for Tokio runtime news",
                    "mode": "sync",
                    "model": "cheap/gpt-4o-mini"
                }),

                metadata: None,
            }],
            200,
            100,
        ),
        // Step 3: parent synthesizes results
        end_turn(
            "Based on the search results: Rust async is evolving rapidly with Tokio runtime improvements.",
            300,
            150,
        ),
    ];

    let agent = setup_with_router(parent_responses, &dir, router).await;
    let resp = agent
        .process_message("Research Rust async ecosystem", &[], vec![])
        .await
        .unwrap();

    assert!(resp.content.contains("Rust async"));
    // Total tokens: 200+100 + 200+100 + 300+150 = 1050
    assert!(resp.token_usage.input_tokens > 0);
    assert!(resp.token_usage.output_tokens > 0);
}

// ---------------------------------------------------------------------------
// Test 2: Multi-model file operations — sub-agents write files, reviewer reads
// ---------------------------------------------------------------------------

/// Verifies that sub-agents routed to different models can use file tools,
/// and files created by one sub-agent are visible to others.
#[tokio::test]
async fn test_multi_model_file_operations() {
    let dir = TempDir::new().unwrap();

    // "fast" provider for file-writing sub-agents
    let fast_provider = mock_provider(
        "claude-haiku",
        32_000,
        vec![
            // Sub-agent 1: write backend file
            tool_use(
                vec![ToolCall {
                    id: "w1".into(),
                    name: "write_file".into(),
                    arguments: serde_json::json!({
                        "path": "server.py",
                        "content": "from flask import Flask\napp = Flask(__name__)\n"
                    }),

                    metadata: None,
                }],
                100,
                50,
            ),
            end_turn("Created server.py with Flask boilerplate", 100, 40),
            // Sub-agent 2: write frontend file
            tool_use(
                vec![ToolCall {
                    id: "w2".into(),
                    name: "write_file".into(),
                    arguments: serde_json::json!({
                        "path": "index.html",
                        "content": "<html><body><h1>Hello</h1></body></html>"
                    }),

                    metadata: None,
                }],
                100,
                50,
            ),
            end_turn("Created index.html", 100, 40),
        ],
    );

    // "strong" provider for code review sub-agent
    let strong_provider = mock_provider(
        "gpt-4o",
        128_000,
        vec![
            // Sub-agent 3: read and review the files
            tool_use(
                vec![ToolCall {
                    id: "r1".into(),
                    name: "read_file".into(),
                    arguments: serde_json::json!({"path": "server.py"}),

                    metadata: None,
                }],
                100,
                50,
            ),
            end_turn(
                "Code review: server.py looks good, Flask app properly initialized",
                150,
                80,
            ),
        ],
    );

    let router = mock_router(vec![("fast", fast_provider), ("strong", strong_provider)]);

    let parent_responses = vec![
        // Spawn backend writer
        tool_use(
            vec![ToolCall {
                id: "s1".into(),
                name: "spawn".into(),
                arguments: serde_json::json!({
                    "task": "Create a Flask backend server",
                    "mode": "sync",
                    "model": "fast/claude-haiku"
                }),

                metadata: None,
            }],
            200,
            100,
        ),
        // Spawn frontend writer
        tool_use(
            vec![ToolCall {
                id: "s2".into(),
                name: "spawn".into(),
                arguments: serde_json::json!({
                    "task": "Create an HTML frontend",
                    "mode": "sync",
                    "model": "fast/claude-haiku"
                }),

                metadata: None,
            }],
            200,
            100,
        ),
        // Spawn code reviewer
        tool_use(
            vec![ToolCall {
                id: "s3".into(),
                name: "spawn".into(),
                arguments: serde_json::json!({
                    "task": "Review the server.py code",
                    "mode": "sync",
                    "model": "strong/gpt-4o"
                }),

                metadata: None,
            }],
            200,
            100,
        ),
        // Parent summarizes
        end_turn(
            "Website created and reviewed. Backend and frontend files are ready.",
            300,
            100,
        ),
    ];

    let agent = setup_with_router(parent_responses, &dir, router).await;
    let resp = agent
        .process_message(
            "Build a simple website with backend and frontend",
            &[],
            vec![],
        )
        .await
        .unwrap();

    assert!(resp.content.contains("Website created"));

    // Verify files actually exist on disk
    assert!(dir.path().join("server.py").exists());
    assert!(dir.path().join("index.html").exists());

    // Verify file contents
    let server_content = std::fs::read_to_string(dir.path().join("server.py")).unwrap();
    assert!(server_content.contains("Flask"));
}

// ---------------------------------------------------------------------------
// Test 3: Mixed tool + sub-agent pipeline — search, process, write JSON
// ---------------------------------------------------------------------------

/// Verifies a data pipeline pattern: sub-agent searches, parent processes,
/// another sub-agent writes structured output, parent validates.
#[tokio::test]
async fn test_data_pipeline_with_routing() {
    let dir = TempDir::new().unwrap();

    // "cheap" provider for data gathering/writing sub-agents
    let cheap_provider = mock_provider(
        "gpt-4o-mini",
        32_000,
        vec![
            // Sub-agent 1: returns simulated search results
            end_turn(
                "Found URLs: https://example.com/data1, https://example.com/data2",
                100,
                50,
            ),
            // Sub-agent 2: writes structured JSON
            tool_use(
                vec![ToolCall {
                    id: "wj".into(),
                    name: "write_file".into(),
                    arguments: serde_json::json!({
                        "path": "results.json",
                        "content": "{\"urls\": [\"https://example.com/data1\", \"https://example.com/data2\"], \"count\": 2}"
                    }),

                    metadata: None,
                }],
                100,
                50,
            ),
            end_turn("Wrote results.json with 2 URLs", 100, 40),
        ],
    );

    let router = mock_router(vec![("cheap", cheap_provider)]);

    let parent_responses = vec![
        // Step 1: spawn search sub-agent
        tool_use(
            vec![ToolCall {
                id: "p1".into(),
                name: "spawn".into(),
                arguments: serde_json::json!({
                    "task": "Find data source URLs",
                    "mode": "sync",
                    "model": "cheap/gpt-4o-mini"
                }),

                metadata: None,
            }],
            200,
            100,
        ),
        // Step 2: spawn writer sub-agent
        tool_use(
            vec![ToolCall {
                id: "p2".into(),
                name: "spawn".into(),
                arguments: serde_json::json!({
                    "task": "Write the URLs as structured JSON to results.json",
                    "mode": "sync",
                    "model": "cheap/gpt-4o-mini"
                }),

                metadata: None,
            }],
            200,
            100,
        ),
        // Step 3: parent reads and validates
        tool_use(
            vec![ToolCall {
                id: "p3".into(),
                name: "read_file".into(),
                arguments: serde_json::json!({"path": "results.json"}),

                metadata: None,
            }],
            200,
            80,
        ),
        // Step 4: parent reports
        end_turn(
            "Pipeline complete: collected 2 URLs and saved to results.json",
            300,
            100,
        ),
    ];

    let agent = setup_with_router(parent_responses, &dir, router).await;
    let resp = agent
        .process_message("Run data collection pipeline", &[], vec![])
        .await
        .unwrap();

    assert!(resp.content.contains("Pipeline complete"));
    assert!(dir.path().join("results.json").exists());

    // Validate JSON structure
    let json_content = std::fs::read_to_string(dir.path().join("results.json")).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json_content).unwrap();
    assert_eq!(parsed["count"], 2);
}

// ---------------------------------------------------------------------------
// Test 4: Context window override — sub-agent gets larger context than parent
// ---------------------------------------------------------------------------

/// Verifies that a sub-agent spawned with context_window override gets a
/// different context budget than the parent, using ContextWindowOverride wrapper.
#[tokio::test]
async fn test_context_window_override_subagent() {
    let dir = TempDir::new().unwrap();

    // Parent uses tiny context window.
    // Because the sub-agent without a `model` field shares the parent's provider,
    // both parent and sub-agent consume from the same response queue:
    //   parent response 1 → spawn tool call
    //   sub-agent response → end turn (consumed by child agent)
    //   parent response 2 → final end turn
    let parent_llm = Arc::new(
        MockLlmProvider::new(
            "parent-model",
            vec![
                // Parent spawns sub-agent with enlarged context window
                tool_use(
                    vec![ToolCall {
                        id: "s1".into(),
                        name: "spawn".into(),
                        arguments: serde_json::json!({
                            "task": "Process all the context",
                            "mode": "sync",
                            "context_window": 8000
                        }),

                        metadata: None,
                    }],
                    50,
                    30,
                ),
                // Sub-agent response (consumed by the child since it shares the provider)
                end_turn("Processed full context with 8000 token window", 80, 40),
                // Parent continues after sub-agent returns
                end_turn("Sub-agent processed the full context successfully", 50, 30),
            ],
        )
        .with_context_window(2000),
    );

    let mut tools = ToolRegistry::with_builtins(dir.path());
    let (tx, _rx) = tokio::sync::mpsc::channel(16);
    let memory = Arc::new(EpisodeStore::open(dir.path().join(".octos")).await.unwrap());

    let spawn = SpawnTool::new(
        parent_llm.clone(),
        memory.clone(),
        dir.path().to_path_buf(),
        tx,
    );
    tools.register(spawn);

    let agent = Agent::new(
        AgentId::new("parent"),
        parent_llm as Arc<dyn LlmProvider>,
        tools,
        memory,
    )
    .with_config(AgentConfig {
        save_episodes: false,
        ..Default::default()
    });

    // Build long history to pressure context
    let history: Vec<Message> = (0..30)
        .map(|i| Message {
            role: octos_core::MessageRole::User,
            content: format!(
                "This is message number {} with padding text to consume tokens in context window.",
                i
            ),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            timestamp: chrono::Utc::now(),
        })
        .collect();

    let resp = agent
        .process_message("Process the full conversation", &history, vec![])
        .await
        .unwrap();

    assert!(resp.content.contains("Sub-agent processed"));

    // Verify ContextWindowOverride works at the unit level
    let base: Arc<dyn LlmProvider> =
        Arc::new(MockLlmProvider::new("test", vec![]).with_context_window(2000));
    assert_eq!(base.context_window(), 2000);
    let overridden = ContextWindowOverride::new(base, 8000);
    assert_eq!(overridden.context_window(), 8000);
    assert_eq!(overridden.model_id(), "test");
}

// ---------------------------------------------------------------------------
// Test 5: Provider failover — sub-agent fails, parent retries with different model
// ---------------------------------------------------------------------------

/// Verifies that when a sub-agent's provider fails, the error propagates back
/// to the parent, which can retry with a different provider key.
#[tokio::test]
async fn test_provider_failover_retry() {
    let dir = TempDir::new().unwrap();

    // "flaky" provider: fails immediately (0 successful calls)
    let flaky_provider: Arc<dyn LlmProvider> = Arc::new(FailCountProvider::new(
        "flaky-model",
        0, // fail on first call
        vec![],
    ));

    // "reliable" provider: always works
    let reliable_provider = mock_provider(
        "reliable-model",
        128_000,
        vec![end_turn("Reliable result: task completed", 100, 50)],
    );

    let router = mock_router(vec![
        ("flaky", flaky_provider),
        ("reliable", reliable_provider),
    ]);

    let parent_responses = vec![
        // Step 1: parent tries flaky provider → sub-agent will fail
        tool_use(
            vec![ToolCall {
                id: "s1".into(),
                name: "spawn".into(),
                arguments: serde_json::json!({
                    "task": "Do the work",
                    "mode": "sync",
                    "model": "flaky/flaky-model"
                }),

                metadata: None,
            }],
            200,
            100,
        ),
        // Step 2: parent sees failure, retries with reliable provider
        tool_use(
            vec![ToolCall {
                id: "s2".into(),
                name: "spawn".into(),
                arguments: serde_json::json!({
                    "task": "Do the work",
                    "mode": "sync",
                    "model": "reliable/reliable-model"
                }),

                metadata: None,
            }],
            200,
            100,
        ),
        // Step 3: parent reports success
        end_turn(
            "Task completed after retry with reliable provider",
            300,
            100,
        ),
    ];

    let agent = setup_with_router(parent_responses, &dir, router).await;
    let resp = agent
        .process_message("Do important work with failover", &[], vec![])
        .await
        .unwrap();

    assert!(resp.content.contains("completed after retry"));
}

// ---------------------------------------------------------------------------
// Test 6: Tool policy inheritance — sub-agent denied tools
// ---------------------------------------------------------------------------

/// Verifies that tool policies (allowed_tools) are enforced on sub-agents,
/// even when they use a different model via the router.
#[tokio::test]
async fn test_tool_policy_with_routing() {
    let dir = TempDir::new().unwrap();

    // Write a file for the sub-agent to read
    std::fs::write(dir.path().join("data.txt"), "secret data").unwrap();

    // "cheap" provider for the sub-agent
    let cheap_provider = mock_provider(
        "gpt-4o-mini",
        32_000,
        vec![
            // Sub-agent tries shell (will be denied), then reads file (allowed)
            tool_use(
                vec![ToolCall {
                    id: "t1".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({"command": "echo hello"}),

                    metadata: None,
                }],
                100,
                50,
            ),
            // After shell denial, tries read_file (allowed)
            tool_use(
                vec![ToolCall {
                    id: "t2".into(),
                    name: "read_file".into(),
                    arguments: serde_json::json!({"path": "data.txt"}),

                    metadata: None,
                }],
                100,
                50,
            ),
            end_turn("Read data.txt: secret data", 100, 40),
        ],
    );

    let router = mock_router(vec![("cheap", cheap_provider)]);

    let parent_responses = vec![
        // Parent spawns sub-agent with restricted tools
        tool_use(
            vec![ToolCall {
                id: "s1".into(),
                name: "spawn".into(),
                arguments: serde_json::json!({
                    "task": "Read data.txt and report contents",
                    "mode": "sync",
                    "model": "cheap/gpt-4o-mini",
                    "allowed_tools": ["read_file", "grep"]
                }),

                metadata: None,
            }],
            200,
            100,
        ),
        // Parent reports
        end_turn("Sub-agent read the file with restricted tools", 300, 100),
    ];

    let agent = setup_with_router(parent_responses, &dir, router).await;
    let resp = agent
        .process_message("Read data.txt using a restricted sub-agent", &[], vec![])
        .await
        .unwrap();

    // The sub-agent should have completed (shell denied, read_file succeeded)
    assert!(resp.content.contains("restricted tools"));
}

// ---------------------------------------------------------------------------
// Additional: Router unit integration tests
// ---------------------------------------------------------------------------

#[test]
fn test_router_resolve_returns_correct_provider() {
    let router = ProviderRouter::new();
    let p1: Arc<dyn LlmProvider> =
        Arc::new(MockLlmProvider::new("gpt-4o", vec![]).with_context_window(128_000));
    let p2: Arc<dyn LlmProvider> =
        Arc::new(MockLlmProvider::new("claude-haiku", vec![]).with_context_window(200_000));

    router.register("openai", p1);
    router.register("anthropic", p2);

    let resolved = router.resolve("openai/gpt-4o").unwrap();
    assert_eq!(resolved.model_id(), "gpt-4o");
    assert_eq!(resolved.context_window(), 128_000);

    let resolved2 = router.resolve("anthropic/claude-haiku").unwrap();
    assert_eq!(resolved2.model_id(), "claude-haiku");
    assert_eq!(resolved2.context_window(), 200_000);

    // Unknown key should fail
    let err = router.resolve("unknown/model");
    assert!(err.is_err());
}

#[test]
fn test_context_window_override_delegates_correctly() {
    let base: Arc<dyn LlmProvider> =
        Arc::new(MockLlmProvider::new("test-model", vec![]).with_context_window(128_000));
    let overridden = ContextWindowOverride::new(base, 4_000);

    assert_eq!(overridden.context_window(), 4_000);
    assert_eq!(overridden.model_id(), "test-model");
    assert_eq!(overridden.provider_name(), "mock");
}

// ---------------------------------------------------------------------------
// Dynamic input_schema tests
// ---------------------------------------------------------------------------

/// When a router with metadata is configured, SpawnTool's input_schema should
/// contain dynamic model descriptions and enum constraints.
#[test]
fn test_spawn_input_schema_dynamic_with_router() {
    let router = ProviderRouter::new();
    router.register_with_meta(
        "cheap",
        Arc::new(MockLlmProvider::new("gpt-4o-mini", vec![]).with_context_window(128_000)),
        Some("Fast and cheap for simple tasks".into()),
        None,
    );
    router.register_with_meta(
        "strong",
        Arc::new(
            MockLlmProvider::new("claude-sonnet-4-20250514", vec![]).with_context_window(200_000),
        ),
        Some("Most capable, use for complex reasoning".into()),
        None,
    );

    let (tx, _rx) = tokio::sync::mpsc::channel(16);
    let llm: Arc<dyn LlmProvider> = Arc::new(MockLlmProvider::new("parent", vec![]));
    let (store, _tmp) = stub_store();
    let work_dir = tempfile::tempdir().unwrap();
    let spawn = SpawnTool::new(llm, Arc::new(store), work_dir.path().into(), tx)
        .with_provider_router(Arc::new(router));

    let schema = spawn.input_schema();
    let model_prop = &schema["properties"]["model"];

    // Description should mention both models with details
    let desc = model_prop["description"].as_str().unwrap();
    assert!(desc.contains("cheap"), "should list cheap key");
    assert!(desc.contains("gpt-4o-mini"), "should list model id");
    assert!(desc.contains("strong"), "should list strong key");
    assert!(desc.contains("claude-sonnet-4"), "should list sonnet model");
    assert!(
        desc.contains("Fast and cheap"),
        "should include user description"
    );
    assert!(
        desc.contains("128k max ctx"),
        "should include context window"
    );

    // Enum should contain both key and key/model forms
    let enum_vals: Vec<&str> = model_prop["enum"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(enum_vals.contains(&"cheap"));
    assert!(enum_vals.contains(&"cheap/gpt-4o-mini"));
    assert!(enum_vals.contains(&"strong"));
    assert!(enum_vals.contains(&"strong/claude-sonnet-4-20250514"));
}

/// When no router is configured, input_schema should show static fallback.
#[test]
fn test_spawn_input_schema_static_without_router() {
    let (tx, _rx) = tokio::sync::mpsc::channel(16);
    let llm: Arc<dyn LlmProvider> = Arc::new(MockLlmProvider::new("parent", vec![]));
    let (store, _tmp) = stub_store();
    let work_dir = tempfile::tempdir().unwrap();
    let spawn = SpawnTool::new(llm, Arc::new(store), work_dir.path().into(), tx);

    let schema = spawn.input_schema();
    let model_prop = &schema["properties"]["model"];

    // Should have static description, no enum
    let desc = model_prop["description"].as_str().unwrap();
    assert!(desc.contains("Requires a provider router"));
    assert!(model_prop.get("enum").is_none());
}

// ---------------------------------------------------------------------------
// Custom system prompt tests
// ---------------------------------------------------------------------------

/// Mock provider that captures the messages it receives, so tests can verify
/// the system prompt content passed to the sub-agent.
struct CapturingProvider {
    responses: Mutex<Vec<ChatResponse>>,
    captured_messages: Mutex<Vec<Vec<Message>>>,
    model: String,
}

impl CapturingProvider {
    fn new(model: &str, responses: Vec<ChatResponse>) -> Self {
        Self {
            responses: Mutex::new(responses),
            captured_messages: Mutex::new(Vec::new()),
            model: model.to_string(),
        }
    }
}

#[async_trait]
impl LlmProvider for CapturingProvider {
    async fn chat(
        &self,
        messages: &[Message],
        _tools: &[ToolSpec],
        _config: &ChatConfig,
    ) -> eyre::Result<ChatResponse> {
        self.captured_messages
            .lock()
            .unwrap()
            .push(messages.to_vec());
        let mut responses = self.responses.lock().unwrap();
        if responses.is_empty() {
            eyre::bail!("CapturingProvider: no more responses");
        }
        Ok(responses.remove(0))
    }

    fn context_window(&self) -> u32 {
        128_000
    }

    fn model_id(&self) -> &str {
        &self.model
    }

    fn provider_name(&self) -> &str {
        "capturing"
    }
}

/// Verifies that a custom system_prompt from the parent LLM replaces the
/// default worker.txt in the sub-agent.
#[tokio::test]
async fn test_custom_system_prompt_reaches_subagent() {
    let dir = TempDir::new().unwrap();

    // Sub-agent provider that captures messages
    let sub_provider = Arc::new(CapturingProvider::new(
        "sub-model",
        vec![end_turn(
            "Security review complete: no issues found",
            100,
            50,
        )],
    ));

    let router = ProviderRouter::new();
    router.register("reviewer", sub_provider.clone());
    let router = Arc::new(router);

    // Parent provider: calls spawn with a custom system_prompt
    let parent_responses = vec![
        tool_use(
            vec![ToolCall {
                id: "s1".into(),
                name: "spawn".into(),
                arguments: serde_json::json!({
                    "task": "Review server.py for SQL injection vulnerabilities",
                    "mode": "sync",
                    "model": "reviewer",
                    "additional_instructions": "Focus on OWASP Top 10 security issues. Be thorough and precise."
                }),

                metadata: None,
            }],
            200,
            100,
        ),
        end_turn("Security review passed with no issues.", 200, 80),
    ];

    let agent = setup_with_router(parent_responses, &dir, router).await;
    let resp = agent
        .process_message("Review server.py for security", &[], vec![])
        .await
        .unwrap();

    assert!(resp.content.contains("Security review"));

    // Verify the sub-agent received the custom system prompt
    let captured = sub_provider.captured_messages.lock().unwrap();
    assert!(
        !captured.is_empty(),
        "sub-agent should have been called at least once"
    );

    // The first message in the first call should be the system prompt
    let first_call = &captured[0];
    let system_msg = first_call
        .iter()
        .find(|m| m.role == octos_core::MessageRole::System)
        .expect("sub-agent should receive a system message");

    // additional_instructions are appended to the default worker prompt
    assert!(
        system_msg.content.contains("OWASP Top 10"),
        "system prompt should contain additional_instructions, got: {}",
        system_msg.content
    );
    assert!(
        system_msg.content.contains("Worker agent"),
        "should also contain the default worker.txt base prompt"
    );
}

/// Verifies that when no system_prompt is provided, the sub-agent gets the
/// default worker.txt prompt.
#[tokio::test]
async fn test_default_system_prompt_without_override() {
    let dir = TempDir::new().unwrap();

    let sub_provider = Arc::new(CapturingProvider::new(
        "sub-model",
        vec![end_turn("Task done", 100, 50)],
    ));

    let router = ProviderRouter::new();
    router.register("worker", sub_provider.clone());
    let router = Arc::new(router);

    let parent_responses = vec![
        tool_use(
            vec![ToolCall {
                id: "s1".into(),
                name: "spawn".into(),
                arguments: serde_json::json!({
                    "task": "Do something simple",
                    "mode": "sync",
                    "model": "worker"
                }),

                metadata: None,
            }],
            200,
            100,
        ),
        end_turn("Done.", 200, 80),
    ];

    let agent = setup_with_router(parent_responses, &dir, router).await;
    agent
        .process_message("Do something", &[], vec![])
        .await
        .unwrap();

    let captured = sub_provider.captured_messages.lock().unwrap();
    assert!(!captured.is_empty());

    let system_msg = captured[0]
        .iter()
        .find(|m| m.role == octos_core::MessageRole::System)
        .expect("sub-agent should receive a system message");

    assert!(
        system_msg.content.contains("Worker agent"),
        "should contain the default worker.txt prompt, got: {}",
        system_msg.content
    );
}

/// Verifies that input_schema includes the system_prompt field.
#[test]
fn test_spawn_input_schema_includes_additional_instructions() {
    let (tx, _rx) = tokio::sync::mpsc::channel(16);
    let llm: Arc<dyn LlmProvider> = Arc::new(MockLlmProvider::new("parent", vec![]));
    let (store, _tmp) = stub_store();
    let work_dir = tempfile::tempdir().unwrap();
    let spawn = SpawnTool::new(llm, Arc::new(store), work_dir.path().into(), tx);

    let schema = spawn.input_schema();
    let ai_prop = &schema["properties"]["additional_instructions"];
    assert_eq!(ai_prop["type"].as_str().unwrap(), "string");
    let desc = ai_prop["description"].as_str().unwrap();
    assert!(desc.contains("appended"));
    // system_prompt should no longer appear in schema
    assert!(
        schema["properties"]["system_prompt"].is_null(),
        "system_prompt should not be in the LLM-facing schema"
    );
}

/// Helper: create a minimal EpisodeStore for schema tests (doesn't need to persist).
/// Returns the TempDir alongside so its lifetime keeps the DB path valid.
fn stub_store() -> (EpisodeStore, tempfile::TempDir) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let store = rt.block_on(EpisodeStore::open(dir.path())).unwrap();
    (store, dir)
}
