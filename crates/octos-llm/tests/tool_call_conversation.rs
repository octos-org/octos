//! Integration test: send a multi-turn tool-call conversation to every
//! available LLM provider and verify they handle the tricky empty assistant
//! content case correctly.
//!
//! Run with:
//!   cargo test -p octos-llm --test tool_call_conversation -- --ignored --nocapture

use std::sync::Arc;
use std::time::Instant;

use chrono::Utc;
use octos_core::{Message, MessageRole, ToolCall};
use octos_llm::anthropic::AnthropicProvider;
use octos_llm::gemini::GeminiProvider;
use octos_llm::openai::OpenAIProvider;
use octos_llm::{AdaptiveConfig, AdaptiveRouter, ChatConfig, LlmProvider, ToolSpec};

// ---------------------------------------------------------------------------
// Extensive tool definitions — exercises complex schemas, nested objects,
// enums, arrays, optional fields, and multiple tools at once.
// ---------------------------------------------------------------------------

fn all_tools() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "get_weather".to_string(),
            description: "Get current weather conditions for a location. Returns temperature, \
                          conditions, humidity, wind speed, and optional forecast."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "city": {
                        "type": "string",
                        "description": "City name (e.g. 'Tokyo', 'San Francisco')"
                    },
                    "units": {
                        "type": "string",
                        "enum": ["celsius", "fahrenheit"],
                        "description": "Temperature units. Defaults to celsius."
                    },
                    "include_forecast": {
                        "type": "boolean",
                        "description": "Whether to include a 3-day forecast"
                    }
                },
                "required": ["city"]
            }),
        },
        ToolSpec {
            name: "search_web".to_string(),
            description: "Search the web for real-time information. Returns a list of results \
                          with title, URL, and snippet."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search query"
                    },
                    "max_results": {
                        "type": "integer",
                        "description": "Maximum number of results to return (1-10)"
                    },
                    "site_filter": {
                        "type": "string",
                        "description": "Restrict search to a specific domain (e.g. 'reddit.com')"
                    }
                },
                "required": ["query"]
            }),
        },
        ToolSpec {
            name: "execute_code".to_string(),
            description: "Execute code in a sandboxed environment. Supports Python, JavaScript, \
                          and shell commands. Returns stdout, stderr, and exit code."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "language": {
                        "type": "string",
                        "enum": ["python", "javascript", "bash"],
                        "description": "Programming language"
                    },
                    "code": {
                        "type": "string",
                        "description": "Source code to execute"
                    },
                    "timeout_seconds": {
                        "type": "integer",
                        "description": "Execution timeout in seconds (default: 30)"
                    },
                    "env_vars": {
                        "type": "object",
                        "description": "Environment variables as key-value pairs"
                    }
                },
                "required": ["language", "code"]
            }),
        },
        ToolSpec {
            name: "read_file".to_string(),
            description: "Read the contents of a file from the filesystem.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute or relative file path"
                    },
                    "encoding": {
                        "type": "string",
                        "enum": ["utf-8", "ascii", "base64"],
                        "description": "File encoding (default: utf-8)"
                    },
                    "line_range": {
                        "type": "object",
                        "description": "Optional line range to read",
                        "properties": {
                            "start": { "type": "integer" },
                            "end": { "type": "integer" }
                        }
                    }
                },
                "required": ["path"]
            }),
        },
        ToolSpec {
            name: "write_file".to_string(),
            description: "Write content to a file on the filesystem.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute or relative file path"
                    },
                    "content": {
                        "type": "string",
                        "description": "Content to write"
                    },
                    "mode": {
                        "type": "string",
                        "enum": ["overwrite", "append"],
                        "description": "Write mode (default: overwrite)"
                    }
                },
                "required": ["path", "content"]
            }),
        },
        ToolSpec {
            name: "create_task".to_string(),
            description: "Create a new task or reminder with optional scheduling, tags, \
                          and priority."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "title": {
                        "type": "string",
                        "description": "Task title"
                    },
                    "description": {
                        "type": "string",
                        "description": "Detailed task description"
                    },
                    "priority": {
                        "type": "string",
                        "enum": ["low", "medium", "high", "urgent"],
                        "description": "Task priority level"
                    },
                    "due_date": {
                        "type": "string",
                        "description": "Due date in ISO 8601 format (e.g. 2025-03-15T10:00:00Z)"
                    },
                    "tags": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Tags for categorization"
                    },
                    "assignee": {
                        "type": "string",
                        "description": "Person or team to assign the task to"
                    }
                },
                "required": ["title"]
            }),
        },
        ToolSpec {
            name: "send_message".to_string(),
            description: "Send a message to a user or channel via various platforms \
                          (email, Slack, Telegram, etc.)."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "platform": {
                        "type": "string",
                        "enum": ["email", "slack", "telegram", "discord"],
                        "description": "Messaging platform"
                    },
                    "recipient": {
                        "type": "string",
                        "description": "Recipient address/handle/channel"
                    },
                    "subject": {
                        "type": "string",
                        "description": "Message subject (for email)"
                    },
                    "body": {
                        "type": "string",
                        "description": "Message body (supports markdown)"
                    },
                    "attachments": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "File attachment URLs"
                    }
                },
                "required": ["platform", "recipient", "body"]
            }),
        },
        ToolSpec {
            name: "database_query".to_string(),
            description: "Execute a read-only SQL query against the application database. \
                          Returns rows as JSON objects."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "sql": {
                        "type": "string",
                        "description": "SQL query (SELECT only)"
                    },
                    "params": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Parameterized query values"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of rows (default: 100)"
                    }
                },
                "required": ["sql"]
            }),
        },
    ]
}

// ---------------------------------------------------------------------------
// System prompt — extensive, realistic agent prompt with tool use instructions
// ---------------------------------------------------------------------------

const SYSTEM_PROMPT: &str = r#"You are Octos, an advanced AI assistant with access to a comprehensive set of tools. You help users with research, coding, task management, and communication.

## Your Capabilities

You have the following tools available:

1. **get_weather** — Retrieve real-time weather data for any city worldwide. Supports Celsius/Fahrenheit and optional 3-day forecasts.
2. **search_web** — Search the internet for up-to-date information, news, documentation, and answers. Use this when you need current data beyond your training cutoff.
3. **execute_code** — Run Python, JavaScript, or Bash code in a sandboxed environment. Use this for calculations, data processing, API calls, or demonstrating code.
4. **read_file** / **write_file** — Read from and write to the local filesystem. Use these for working with configuration files, logs, data files, etc.
5. **create_task** — Create tasks, reminders, and action items with priority levels, due dates, tags, and assignments.
6. **send_message** — Send messages via email, Slack, Telegram, or Discord. Supports markdown formatting and file attachments.
7. **database_query** — Execute read-only SQL queries against the application database for analytics and reporting.

## Tool Use Guidelines

- **Always use tools when they can provide better answers** than relying on your training data alone. For real-time information (weather, news, stock prices), always use the appropriate tool.
- **Chain multiple tools** when needed. For example, to help a user debug a file, you might read_file → analyze → execute_code → write_file.
- **Use parallel tool calls** when the calls are independent. For example, if asked about weather in two cities, call get_weather twice in parallel.
- **Handle tool errors gracefully.** If a tool call fails, explain the error and suggest alternatives.
- **Be concise in tool arguments.** Don't include unnecessary optional parameters.
- **Never fabricate tool results.** If a tool returns an error or unexpected data, report it honestly.

## Response Style

- Be helpful, accurate, and concise
- When presenting tool results, summarize the key information rather than dumping raw JSON
- If the user's request is ambiguous, ask for clarification before calling tools
- Proactively suggest follow-up actions when appropriate"#;

// ---------------------------------------------------------------------------
// Build a multi-turn conversation with the empty assistant content pattern
// and multiple tool calls to stress-test serialization.
// ---------------------------------------------------------------------------

fn build_tool_call_conversation() -> Vec<Message> {
    vec![
        // 1. System prompt with extensive tool instructions
        Message {
            role: MessageRole::System,
            content: SYSTEM_PROMPT.to_string(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            timestamp: Utc::now(),
        },
        // 2. User: multi-part request
        Message {
            role: MessageRole::User,
            content: "I need to prepare for a trip to Tokyo next week. Can you check the weather \
                      and also search for the best sushi restaurants there?"
                .to_string(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            timestamp: Utc::now(),
        },
        // 3. Assistant: parallel tool calls with EMPTY content (the problematic pattern)
        Message {
            role: MessageRole::Assistant,
            content: String::new(), // <-- empty content with tool_calls
            media: vec![],
            tool_calls: Some(vec![
                ToolCall {
                    id: "call_weather_001".to_string(),
                    name: "get_weather".to_string(),
                    arguments: serde_json::json!({
                        "city": "Tokyo",
                        "units": "celsius",
                        "include_forecast": true
                    }),
                    metadata: None,
                },
                ToolCall {
                    id: "call_search_001".to_string(),
                    name: "search_web".to_string(),
                    arguments: serde_json::json!({
                        "query": "best sushi restaurants Tokyo 2025",
                        "max_results": 5
                    }),
                    metadata: None,
                },
            ]),
            tool_call_id: None,
            reasoning_content: None,
            timestamp: Utc::now(),
        },
        // 4. Tool result: weather
        Message {
            role: MessageRole::Tool,
            content: serde_json::json!({
                "city": "Tokyo",
                "temperature": 18,
                "units": "celsius",
                "condition": "Partly cloudy",
                "humidity": 62,
                "wind_speed_kmh": 15,
                "forecast": [
                    {"date": "2025-03-10", "high": 20, "low": 12, "condition": "Sunny"},
                    {"date": "2025-03-11", "high": 17, "low": 10, "condition": "Rain"},
                    {"date": "2025-03-12", "high": 22, "low": 14, "condition": "Clear"}
                ]
            })
            .to_string(),
            media: vec![],
            tool_calls: None,
            tool_call_id: Some("call_weather_001".to_string()),
            reasoning_content: None,
            timestamp: Utc::now(),
        },
        // 5. Tool result: search
        Message {
            role: MessageRole::Tool,
            content: serde_json::json!({
                "results": [
                    {
                        "title": "Top 10 Sushi Restaurants in Tokyo (2025 Guide)",
                        "url": "https://example.com/tokyo-sushi",
                        "snippet": "From the legendary Sukiyabashi Jiro to hidden gems in Tsukiji..."
                    },
                    {
                        "title": "Best Budget Sushi in Tokyo - Where Locals Eat",
                        "url": "https://example.com/budget-sushi",
                        "snippet": "Conveyor belt sushi chains like Sushiro and Hamazushi offer..."
                    }
                ]
            })
            .to_string(),
            media: vec![],
            tool_calls: None,
            tool_call_id: Some("call_search_001".to_string()),
            reasoning_content: None,
            timestamp: Utc::now(),
        },
        // 6. User: follow-up that requires synthesizing both results
        Message {
            role: MessageRole::User,
            content: "Great! It looks like it might rain on the 11th. Can you also create a task \
                      to remind me to pack a rain jacket?"
                .to_string(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            timestamp: Utc::now(),
        },
        // 7. Assistant: another tool call with empty content
        Message {
            role: MessageRole::Assistant,
            content: String::new(), // <-- empty again
            media: vec![],
            tool_calls: Some(vec![ToolCall {
                id: "call_task_001".to_string(),
                name: "create_task".to_string(),
                arguments: serde_json::json!({
                    "title": "Pack rain jacket for Tokyo trip",
                    "description": "Rain forecast for March 11 in Tokyo. Don't forget the rain jacket!",
                    "priority": "medium",
                    "due_date": "2025-03-09T18:00:00Z",
                    "tags": ["travel", "packing", "tokyo"]
                }),
                metadata: None,
            }]),
            tool_call_id: None,
            reasoning_content: None,
            timestamp: Utc::now(),
        },
        // 8. Tool result: task created
        Message {
            role: MessageRole::Tool,
            content: serde_json::json!({
                "id": "task_42",
                "status": "created",
                "title": "Pack rain jacket for Tokyo trip",
                "due_date": "2025-03-09T18:00:00Z"
            })
            .to_string(),
            media: vec![],
            tool_calls: None,
            tool_call_id: Some("call_task_001".to_string()),
            reasoning_content: None,
            timestamp: Utc::now(),
        },
        // 9. User: final follow-up to get a response
        Message {
            role: MessageRole::User,
            content: "Perfect. Give me a quick summary of everything — weather, restaurants, \
                      and the reminder."
                .to_string(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            timestamp: Utc::now(),
        },
    ]
}

/// Helper to test a single provider.
async fn test_provider(
    name: &str,
    model: &str,
    provider: Arc<dyn LlmProvider>,
) -> (String, String, Result<String, String>) {
    let messages = build_tool_call_conversation();
    let tools = all_tools();
    let config = ChatConfig {
        max_tokens: Some(400),
        temperature: Some(0.3),
        ..Default::default()
    };

    let start = Instant::now();
    let result = provider.chat(&messages, &tools, &config).await;
    let elapsed = start.elapsed();

    match result {
        Ok(resp) => {
            let content = resp.content.unwrap_or_default();
            let tokens = format!(
                "in={} out={}",
                resp.usage.input_tokens, resp.usage.output_tokens
            );
            let preview: String = content.chars().take(120).collect();
            println!(
                "  {} {name:20} ({model:30}) {:5.1}s  {tokens}",
                "\u{2713}",
                elapsed.as_secs_f64(),
            );
            println!("    => {preview}");
            if content.len() > 120 {
                println!("    ...({}chars total)", content.len());
            }
            (name.to_string(), model.to_string(), Ok(content))
        }
        Err(e) => {
            let err_str = format!("{e:#}");
            println!(
                "  {} {name:20} ({model:30}) {:5.1}s  ERROR: {}",
                "\u{2717}",
                elapsed.as_secs_f64(),
                &err_str[..err_str.len().min(120)]
            );
            (name.to_string(), model.to_string(), Err(err_str))
        }
    }
}

// ---------------------------------------------------------------------------
// NVIDIA NIM helper
// ---------------------------------------------------------------------------

fn nvidia_nim(key: &str, model_id: &str) -> Arc<dyn LlmProvider> {
    Arc::new(
        OpenAIProvider::new(key, model_id).with_base_url("https://integrate.api.nvidia.com/v1"),
    )
}

// ---------------------------------------------------------------------------
// Individual provider tests
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn test_dashscope_qwen3_coder_flash() {
    let key = std::env::var("DASHSCOPE_API_KEY").expect("DASHSCOPE_API_KEY not set");
    let p: Arc<dyn LlmProvider> = Arc::new(
        OpenAIProvider::new(key, "qwen3-coder-flash")
            .with_base_url("https://dashscope.aliyuncs.com/compatible-mode/v1"),
    );
    let (_, _, result) = test_provider("DashScope", "qwen3-coder-flash", p).await;
    assert!(result.is_ok(), "DashScope failed: {:?}", result.err());
}

#[tokio::test]
#[ignore]
async fn test_dashscope_qwen35_plus() {
    let key = std::env::var("DASHSCOPE_API_KEY").expect("DASHSCOPE_API_KEY not set");
    let p: Arc<dyn LlmProvider> = Arc::new(
        OpenAIProvider::new(key, "qwen3.5-plus")
            .with_base_url("https://dashscope.aliyuncs.com/compatible-mode/v1"),
    );
    let (_, _, result) = test_provider("DashScope", "qwen3.5-plus", p).await;
    assert!(result.is_ok(), "DashScope failed: {:?}", result.err());
}

#[tokio::test]
#[ignore]
async fn test_openai_gpt4o() {
    let key = std::env::var("OPENAI_API_KEY").expect("OPENAI_API_KEY not set");
    let p: Arc<dyn LlmProvider> = Arc::new(OpenAIProvider::new(key, "gpt-4o"));
    let (_, _, result) = test_provider("OpenAI", "gpt-4o", p).await;
    assert!(result.is_ok(), "OpenAI failed: {:?}", result.err());
}

#[tokio::test]
#[ignore]
async fn test_openai_gpt4o_mini() {
    let key = std::env::var("OPENAI_API_KEY").expect("OPENAI_API_KEY not set");
    let p: Arc<dyn LlmProvider> = Arc::new(OpenAIProvider::new(key, "gpt-4o-mini"));
    let (_, _, result) = test_provider("OpenAI", "gpt-4o-mini", p).await;
    assert!(
        result.is_ok(),
        "OpenAI gpt-4o-mini failed: {:?}",
        result.err()
    );
}

#[tokio::test]
#[ignore]
async fn test_deepseek_chat() {
    let key = std::env::var("DEEPSEEK_API_KEY").expect("DEEPSEEK_API_KEY not set");
    let p: Arc<dyn LlmProvider> = Arc::new(
        OpenAIProvider::new(key, "deepseek-chat").with_base_url("https://api.deepseek.com/v1"),
    );
    let (_, _, result) = test_provider("DeepSeek", "deepseek-chat", p).await;
    assert!(result.is_ok(), "DeepSeek failed: {:?}", result.err());
}

#[tokio::test]
#[ignore]
async fn test_kimi_k25() {
    let key = std::env::var("KIMI_API_KEY").expect("KIMI_API_KEY not set");
    let p: Arc<dyn LlmProvider> =
        Arc::new(OpenAIProvider::new(key, "kimi-k2.5").with_base_url("https://api.moonshot.ai/v1"));
    let (_, _, result) = test_provider("Kimi/Moonshot", "kimi-k2.5", p).await;
    assert!(result.is_ok(), "Kimi failed: {:?}", result.err());
}

#[tokio::test]
#[ignore]
async fn test_minimax_m1() {
    let key = std::env::var("MINIMAX_API_KEY").expect("MINIMAX_API_KEY not set");
    let p: Arc<dyn LlmProvider> =
        Arc::new(OpenAIProvider::new(key, "MiniMax-M1").with_base_url("https://api.minimax.io/v1"));
    let (_, _, result) = test_provider("MiniMax", "MiniMax-M1", p).await;
    assert!(result.is_ok(), "MiniMax failed: {:?}", result.err());
}

#[tokio::test]
#[ignore]
async fn test_nvidia_deepseek_v32() {
    let key = std::env::var("NVIDIA_API_KEY").expect("NVIDIA_API_KEY not set");
    let (_, _, result) = test_provider(
        "NVIDIA NIM",
        "deepseek-v3.2",
        nvidia_nim(&key, "deepseek-ai/deepseek-v3.2"),
    )
    .await;
    assert!(
        result.is_ok(),
        "NVIDIA NIM deepseek-v3.2 failed: {:?}",
        result.err()
    );
}

#[tokio::test]
#[ignore]
async fn test_nvidia_qwen35_397b() {
    let key = std::env::var("NVIDIA_API_KEY").expect("NVIDIA_API_KEY not set");
    let (_, _, result) = test_provider(
        "NVIDIA NIM",
        "qwen3.5-397b",
        nvidia_nim(&key, "qwen/qwen3.5-397b-a17b"),
    )
    .await;
    assert!(
        result.is_ok(),
        "NVIDIA NIM qwen3.5-397b failed: {:?}",
        result.err()
    );
}

#[tokio::test]
#[ignore]
async fn test_nvidia_glm5() {
    let key = std::env::var("NVIDIA_API_KEY").expect("NVIDIA_API_KEY not set");
    let (_, _, result) = test_provider("NVIDIA NIM", "glm5", nvidia_nim(&key, "z-ai/glm5")).await;
    assert!(result.is_ok(), "NVIDIA NIM glm5 failed: {:?}", result.err());
}

#[tokio::test]
#[ignore]
async fn test_nvidia_kimi_k25() {
    let key = std::env::var("NVIDIA_API_KEY").expect("NVIDIA_API_KEY not set");
    let (_, _, result) = test_provider(
        "NVIDIA NIM",
        "kimi-k2.5",
        nvidia_nim(&key, "moonshotai/kimi-k2.5"),
    )
    .await;
    assert!(
        result.is_ok(),
        "NVIDIA NIM kimi-k2.5 failed: {:?}",
        result.err()
    );
}

#[tokio::test]
#[ignore]
async fn test_nvidia_minimax_m25() {
    let key = std::env::var("NVIDIA_API_KEY").expect("NVIDIA_API_KEY not set");
    let (_, _, result) = test_provider(
        "NVIDIA NIM",
        "minimax-m2.5",
        nvidia_nim(&key, "minimaxai/minimax-m2.5"),
    )
    .await;
    assert!(
        result.is_ok(),
        "NVIDIA NIM minimax-m2.5 failed: {:?}",
        result.err()
    );
}

#[tokio::test]
#[ignore]
async fn test_zai_glm47() {
    let key = std::env::var("ZAI_API_KEY").expect("ZAI_API_KEY not set");
    let p: Arc<dyn LlmProvider> = Arc::new(
        AnthropicProvider::new(key, "glm-4.7").with_base_url("https://api.z.ai/api/anthropic"),
    );
    let (_, _, result) = test_provider("Z.AI", "glm-4.7", p).await;
    assert!(result.is_ok(), "Z.AI failed: {:?}", result.err());
}

#[tokio::test]
#[ignore]
async fn test_gemini_25_flash() {
    let key = std::env::var("GEMINI_API_KEY").expect("GEMINI_API_KEY not set");
    let p: Arc<dyn LlmProvider> = Arc::new(GeminiProvider::new(key, "gemini-2.5-flash"));
    let (_, _, result) = test_provider("Gemini", "gemini-2.5-flash", p).await;
    assert!(result.is_ok(), "Gemini failed: {:?}", result.err());
}

#[tokio::test]
#[ignore]
async fn test_gemini_3_flash() {
    let key = std::env::var("GEMINI_API_KEY").expect("GEMINI_API_KEY not set");
    let p: Arc<dyn LlmProvider> = Arc::new(GeminiProvider::new(key, "gemini-3-flash-preview"));
    let (_, _, result) = test_provider("Gemini", "gemini-3-flash-preview", p).await;
    assert!(result.is_ok(), "Gemini 3 flash failed: {:?}", result.err());
}

#[tokio::test]
#[ignore]
async fn test_gemini_31_pro() {
    let key = std::env::var("GEMINI_API_KEY").expect("GEMINI_API_KEY not set");
    let p: Arc<dyn LlmProvider> = Arc::new(GeminiProvider::new(key, "gemini-3.1-pro-preview"));
    let (_, _, result) = test_provider("Gemini", "gemini-3.1-pro-preview", p).await;
    assert!(result.is_ok(), "Gemini 3.1 pro failed: {:?}", result.err());
}

#[tokio::test]
#[ignore]
async fn test_minimax_m25() {
    let key = std::env::var("MINIMAX_API_KEY").expect("MINIMAX_API_KEY not set");
    let p: Arc<dyn LlmProvider> = Arc::new(
        OpenAIProvider::new(key, "MiniMax-M2.5").with_base_url("https://api.minimax.io/v1"),
    );
    let (_, _, result) = test_provider("MiniMax", "MiniMax-M2.5", p).await;
    assert!(result.is_ok(), "MiniMax-M2.5 failed: {:?}", result.err());
}

// ---------------------------------------------------------------------------
// Combined test: run all available providers and summarize
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn test_all_providers_tool_call_conversation() {
    println!("\n=== Multi-provider tool call conversation test (8 tools, multi-turn) ===\n");

    let mut providers: Vec<(&str, &str, Arc<dyn LlmProvider>)> = Vec::new();

    // DashScope (Qwen)
    if let Ok(key) = std::env::var("DASHSCOPE_API_KEY") {
        providers.push((
            "DashScope",
            "qwen3-coder-flash",
            Arc::new(
                OpenAIProvider::new(key.clone(), "qwen3-coder-flash")
                    .with_base_url("https://dashscope.aliyuncs.com/compatible-mode/v1"),
            ),
        ));
        providers.push((
            "DashScope",
            "qwen3.5-plus",
            Arc::new(
                OpenAIProvider::new(key, "qwen3.5-plus")
                    .with_base_url("https://dashscope.aliyuncs.com/compatible-mode/v1"),
            ),
        ));
    }

    // OpenAI
    if let Ok(key) = std::env::var("OPENAI_API_KEY") {
        providers.push((
            "OpenAI",
            "gpt-4o",
            Arc::new(OpenAIProvider::new(key.clone(), "gpt-4o")),
        ));
        providers.push((
            "OpenAI",
            "gpt-4o-mini",
            Arc::new(OpenAIProvider::new(key, "gpt-4o-mini")),
        ));
    }

    // DeepSeek (native)
    if let Ok(key) = std::env::var("DEEPSEEK_API_KEY") {
        providers.push((
            "DeepSeek",
            "deepseek-chat",
            Arc::new(
                OpenAIProvider::new(key, "deepseek-chat")
                    .with_base_url("https://api.deepseek.com/v1"),
            ),
        ));
    }

    // Kimi/Moonshot (native)
    if let Ok(key) = std::env::var("KIMI_API_KEY") {
        providers.push((
            "Kimi/Moonshot",
            "kimi-k2.5",
            Arc::new(
                OpenAIProvider::new(key, "kimi-k2.5").with_base_url("https://api.moonshot.ai/v1"),
            ),
        ));
    }

    // MiniMax (native)
    if let Ok(key) = std::env::var("MINIMAX_API_KEY") {
        providers.push((
            "MiniMax",
            "MiniMax-M1",
            Arc::new(
                OpenAIProvider::new(key.clone(), "MiniMax-M1")
                    .with_base_url("https://api.minimax.io/v1"),
            ),
        ));
        providers.push((
            "MiniMax",
            "MiniMax-M2.5",
            Arc::new(
                OpenAIProvider::new(key, "MiniMax-M2.5").with_base_url("https://api.minimax.io/v1"),
            ),
        ));
    }

    // NVIDIA NIM — open-source models hosted on NVIDIA infrastructure
    if let Ok(key) = std::env::var("NVIDIA_API_KEY") {
        for (label, model_id) in [
            ("deepseek-v3.2", "deepseek-ai/deepseek-v3.2"),
            ("qwen3.5-397b", "qwen/qwen3.5-397b-a17b"),
            ("glm5", "z-ai/glm5"),
            ("kimi-k2.5", "moonshotai/kimi-k2.5"),
            ("minimax-m2.5", "minimaxai/minimax-m2.5"),
        ] {
            providers.push(("NVIDIA NIM", label, nvidia_nim(&key, model_id)));
        }
    }

    // Local OminiX-API
    if let Ok(port) = std::env::var("OMINIX_PORT") {
        providers.push((
            "Local/OminiX",
            "qwen3.5-27b",
            Arc::new(
                OpenAIProvider::new("not-needed", "qwen3.5-27b")
                    .with_base_url(format!("http://localhost:{port}/v1")),
            ),
        ));
    }

    // Z.AI (Anthropic protocol)
    if let Ok(key) = std::env::var("ZAI_API_KEY") {
        providers.push((
            "Z.AI",
            "glm-4.7",
            Arc::new(
                AnthropicProvider::new(key.clone(), "glm-4.7")
                    .with_base_url("https://api.z.ai/api/anthropic"),
            ),
        ));
        providers.push((
            "Z.AI",
            "glm-5",
            Arc::new(
                AnthropicProvider::new(key, "glm-5")
                    .with_base_url("https://api.z.ai/api/anthropic"),
            ),
        ));
    }

    // Gemini
    if let Ok(key) = std::env::var("GEMINI_API_KEY") {
        providers.push((
            "Gemini",
            "gemini-2.5-flash",
            Arc::new(GeminiProvider::new(key.clone(), "gemini-2.5-flash")),
        ));
        providers.push((
            "Gemini",
            "gemini-3-flash-preview",
            Arc::new(GeminiProvider::new(key.clone(), "gemini-3-flash-preview")),
        ));
        providers.push((
            "Gemini",
            "gemini-3.1-pro-preview",
            Arc::new(GeminiProvider::new(key, "gemini-3.1-pro-preview")),
        ));
    }

    if providers.is_empty() {
        println!("No API keys set. Set env vars and re-run.");
        return;
    }

    println!(
        "Testing {} provider/model combinations...\n",
        providers.len()
    );

    // Run sequentially to avoid overwhelming the system
    let mut successes = 0;
    let mut failures = 0;
    let mut failed_names: Vec<String> = Vec::new();

    for (name, model, provider) in providers {
        let (name, model, result) = test_provider(name, model, provider).await;
        match result {
            Ok(_) => successes += 1,
            Err(ref e) => {
                failures += 1;
                failed_names.push(format!("{name}/{model}"));
                eprintln!("  FAIL: {name}/{model}: {}", &e[..e.len().min(200)]);
            }
        }
    }

    println!("\n=== Results: {successes} passed, {failures} failed ===");
    if !failed_names.is_empty() {
        println!("  Failed: {}", failed_names.join(", "));
    }
    println!();
    assert_eq!(
        failures,
        0,
        "{failures} providers failed: {}",
        failed_names.join(", ")
    );
}

// ---------------------------------------------------------------------------
// Adaptive router test: verify scoring works with real providers
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn test_adaptive_router_with_real_providers() {
    println!("\n=== Adaptive Router test with real providers ===\n");

    let mut providers: Vec<Arc<dyn LlmProvider>> = Vec::new();

    if let Ok(key) = std::env::var("DASHSCOPE_API_KEY") {
        providers.push(Arc::new(
            OpenAIProvider::new(key, "qwen3-coder-flash")
                .with_base_url("https://dashscope.aliyuncs.com/compatible-mode/v1"),
        ));
    }
    if let Ok(key) = std::env::var("OPENAI_API_KEY") {
        providers.push(Arc::new(OpenAIProvider::new(key, "gpt-4o-mini")));
    }
    if let Ok(key) = std::env::var("GEMINI_API_KEY") {
        providers.push(Arc::new(GeminiProvider::new(key, "gemini-2.5-flash")));
    }

    if providers.len() < 2 {
        println!(
            "Need at least 2 providers. Set DASHSCOPE_API_KEY, OPENAI_API_KEY, GEMINI_API_KEY."
        );
        return;
    }

    let config = AdaptiveConfig {
        probe_probability: 0.5,
        probe_interval_secs: 0,
        ..Default::default()
    };

    let router = AdaptiveRouter::new(providers, &[], config);

    let messages = build_tool_call_conversation();
    let tools = all_tools();
    let chat_config = ChatConfig {
        max_tokens: Some(400),
        temperature: Some(0.3),
        ..Default::default()
    };

    println!("Sending 5 requests through adaptive router...\n");
    for i in 1..=5 {
        let start = Instant::now();
        match router.chat(&messages, &tools, &chat_config).await {
            Ok(resp) => {
                let content = resp.content.unwrap_or_default();
                println!(
                    "  Request {i}: {:.1}s  in={} out={}  => {}",
                    start.elapsed().as_secs_f64(),
                    resp.usage.input_tokens,
                    resp.usage.output_tokens,
                    &content[..content.len().min(60)]
                );
            }
            Err(e) => {
                println!(
                    "  Request {i}: {:.1}s  ERROR: {e:#}",
                    start.elapsed().as_secs_f64()
                );
            }
        }
    }

    println!("\n--- Adaptive Router Metrics ---");
    for (provider, model, snap) in router.metrics_snapshots() {
        println!(
            "  {provider}/{model}: ema={:.0}ms p95={:.0}ms err={:.1}% ok={} fail={} consec_fail={}",
            snap.latency_ema_ms,
            snap.p95_latency_ms,
            snap.error_rate * 100.0,
            snap.success_count,
            snap.failure_count,
            snap.consecutive_failures,
        );
    }
    println!();
}
