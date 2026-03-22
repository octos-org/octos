//! Tool Use Capability Benchmark Suite
//!
//! Tests all configured LLM providers for tool calling stability, quality, and stress.
//! Uses real API calls — run with: `cargo test -p octos-llm --test tool_benchmark -- --ignored --nocapture`
//!
//! Quick rank (stability only): `cargo test -p octos-llm --test tool_benchmark stability -- --ignored --nocapture`
//! Full suite: `cargo test -p octos-llm --test tool_benchmark full_benchmark -- --ignored --nocapture`

use std::collections::HashMap;
use std::time::{Duration, Instant};

use octos_llm::{ChatConfig, ToolSpec, LlmProvider};
use octos_llm::openai::OpenAIProvider;
use octos_llm::anthropic::AnthropicProvider;
use octos_llm::gemini::GeminiProvider;
use serde::Serialize;

// ── Synthetic tool definitions ──────────────────────────────────

fn make_tool(name: &str, desc: &str, params: serde_json::Value) -> ToolSpec {
    ToolSpec {
        name: name.to_string(),
        description: desc.to_string(),
        input_schema: params,
    }
}

/// Generate N synthetic tools with realistic schemas.
fn generate_tools(count: usize) -> Vec<ToolSpec> {
    let templates = vec![
        ("get_weather", "Get current weather for a city", serde_json::json!({
            "type": "object",
            "properties": {
                "city": {"type": "string", "description": "City name"},
                "units": {"type": "string", "enum": ["celsius", "fahrenheit"]}
            },
            "required": ["city"]
        })),
        ("web_search", "Search the web for information", serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Search query"},
                "num_results": {"type": "integer", "description": "Number of results"}
            },
            "required": ["query"]
        })),
        ("read_file", "Read contents of a file", serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path"},
                "offset": {"type": "integer"},
                "limit": {"type": "integer"}
            },
            "required": ["path"]
        })),
        ("write_file", "Write content to a file", serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path"},
                "content": {"type": "string", "description": "Content to write"}
            },
            "required": ["path", "content"]
        })),
        ("send_email", "Send an email", serde_json::json!({
            "type": "object",
            "properties": {
                "to": {"type": "string"},
                "subject": {"type": "string"},
                "body": {"type": "string"}
            },
            "required": ["to", "subject", "body"]
        })),
        ("get_time", "Get current time in a timezone", serde_json::json!({
            "type": "object",
            "properties": {
                "timezone": {"type": "string", "description": "IANA timezone"}
            },
            "required": ["timezone"]
        })),
        ("list_dir", "List directory contents", serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"}
            },
            "required": ["path"]
        })),
        ("run_shell", "Execute a shell command", serde_json::json!({
            "type": "object",
            "properties": {
                "command": {"type": "string"},
                "timeout_secs": {"type": "integer"}
            },
            "required": ["command"]
        })),
        ("translate", "Translate text between languages", serde_json::json!({
            "type": "object",
            "properties": {
                "text": {"type": "string"},
                "from": {"type": "string"},
                "to": {"type": "string"}
            },
            "required": ["text", "to"]
        })),
        ("calculate", "Evaluate a math expression", serde_json::json!({
            "type": "object",
            "properties": {
                "expression": {"type": "string"}
            },
            "required": ["expression"]
        })),
        ("create_image", "Generate an image from a text prompt", serde_json::json!({
            "type": "object",
            "properties": {
                "prompt": {"type": "string"},
                "size": {"type": "string", "enum": ["256x256", "512x512", "1024x1024"]}
            },
            "required": ["prompt"]
        })),
        ("database_query", "Run a SQL query", serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "database": {"type": "string"}
            },
            "required": ["query"]
        })),
        ("http_request", "Make an HTTP request", serde_json::json!({
            "type": "object",
            "properties": {
                "url": {"type": "string"},
                "method": {"type": "string", "enum": ["GET", "POST", "PUT", "DELETE"]},
                "body": {"type": "string"},
                "headers": {"type": "object"}
            },
            "required": ["url"]
        })),
        ("compress_file", "Compress a file or directory", serde_json::json!({
            "type": "object",
            "properties": {
                "input_path": {"type": "string"},
                "output_path": {"type": "string"},
                "format": {"type": "string", "enum": ["zip", "tar.gz", "7z"]}
            },
            "required": ["input_path"]
        })),
        ("schedule_task", "Schedule a task for later execution", serde_json::json!({
            "type": "object",
            "properties": {
                "task": {"type": "string"},
                "cron": {"type": "string", "description": "Cron expression"},
                "enabled": {"type": "boolean"}
            },
            "required": ["task", "cron"]
        })),
        ("git_commit", "Create a git commit", serde_json::json!({
            "type": "object",
            "properties": {
                "message": {"type": "string"},
                "files": {"type": "array", "items": {"type": "string"}}
            },
            "required": ["message"]
        })),
        ("resize_image", "Resize an image", serde_json::json!({
            "type": "object",
            "properties": {
                "input_path": {"type": "string"},
                "width": {"type": "integer"},
                "height": {"type": "integer"},
                "output_path": {"type": "string"}
            },
            "required": ["input_path", "width", "height"]
        })),
        ("text_to_speech", "Convert text to audio", serde_json::json!({
            "type": "object",
            "properties": {
                "text": {"type": "string"},
                "voice": {"type": "string"},
                "language": {"type": "string"}
            },
            "required": ["text"]
        })),
        ("pdf_extract", "Extract text from a PDF file", serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "pages": {"type": "string", "description": "Page range, e.g. '1-5'"}
            },
            "required": ["path"]
        })),
        ("memory_store", "Store a key-value pair in memory", serde_json::json!({
            "type": "object",
            "properties": {
                "key": {"type": "string"},
                "value": {"type": "string"},
                "ttl_secs": {"type": "integer"}
            },
            "required": ["key", "value"]
        })),
        ("video_transcribe", "Transcribe audio from a video file", serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "language": {"type": "string"}
            },
            "required": ["path"]
        })),
        ("deploy_service", "Deploy a service to the cloud", serde_json::json!({
            "type": "object",
            "properties": {
                "name": {"type": "string"},
                "image": {"type": "string"},
                "port": {"type": "integer"},
                "env": {"type": "object"}
            },
            "required": ["name", "image"]
        })),
        ("monitor_url", "Set up monitoring for a URL", serde_json::json!({
            "type": "object",
            "properties": {
                "url": {"type": "string"},
                "interval_secs": {"type": "integer"},
                "alert_email": {"type": "string"}
            },
            "required": ["url"]
        })),
        ("encrypt_file", "Encrypt a file with a password", serde_json::json!({
            "type": "object",
            "properties": {
                "input_path": {"type": "string"},
                "output_path": {"type": "string"},
                "algorithm": {"type": "string", "enum": ["aes256", "chacha20"]}
            },
            "required": ["input_path"]
        })),
        ("analyze_csv", "Analyze a CSV file and return statistics", serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "columns": {"type": "array", "items": {"type": "string"}},
                "operations": {"type": "array", "items": {"type": "string", "enum": ["mean", "median", "sum", "count"]}}
            },
            "required": ["path"]
        })),
        ("browser_navigate", "Navigate a headless browser to a URL", serde_json::json!({
            "type": "object",
            "properties": {
                "url": {"type": "string"},
                "wait_for": {"type": "string", "description": "CSS selector to wait for"},
                "screenshot": {"type": "boolean"}
            },
            "required": ["url"]
        })),
        ("deep_search", "Deep multi-engine web search with synthesis", serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "search_engine": {"type": "string", "enum": ["perplexity", "tavily", "brave", "duckduckgo", "all"]},
                "num_angles": {"type": "integer"}
            },
            "required": ["query"]
        })),
        ("run_pipeline", "Execute a multi-step pipeline defined as DOT graph", serde_json::json!({
            "type": "object",
            "properties": {
                "pipeline": {"type": "string", "description": "Inline DOT digraph"},
                "input": {"type": "string"},
                "timeout_secs": {"type": "integer"}
            },
            "required": ["pipeline", "input"]
        })),
        ("voice_clone", "Clone a voice from an audio sample", serde_json::json!({
            "type": "object",
            "properties": {
                "audio_path": {"type": "string"},
                "voice_name": {"type": "string"},
                "language": {"type": "string"}
            },
            "required": ["audio_path", "voice_name"]
        })),
        ("manage_account", "Manage user accounts", serde_json::json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["create", "delete", "list", "update", "start", "stop"]},
                "name": {"type": "string"},
                "config": {"type": "object"}
            },
            "required": ["action"]
        })),
    ];

    templates
        .iter()
        .cycle()
        .take(count)
        .enumerate()
        .map(|(i, (name, desc, schema))| {
            let tool_name = if i < templates.len() {
                name.to_string()
            } else {
                format!("{}_{}", name, i / templates.len())
            };
            make_tool(&tool_name, desc, schema.clone())
        })
        .collect()
}

// ── Provider creation helpers ───────────────────────────────────

struct ProviderInfo {
    name: String,
    provider: Box<dyn LlmProvider + Send + Sync>,
}

fn create_providers() -> Vec<ProviderInfo> {
    let mut providers = Vec::new();

    // Dashscope / qwen3.5-plus
    if let Ok(key) = std::env::var("DASHSCOPE_API_KEY") {
        let p = OpenAIProvider::new(&key, "qwen3.5-plus")
            .with_base_url("https://dashscope.aliyuncs.com/compatible-mode/v1");
        providers.push(ProviderInfo {
            name: "dashscope/qwen3.5-plus".into(),
            provider: Box::new(p),
        });
    }

    // DeepSeek
    if let Ok(key) = std::env::var("DEEPSEEK_API_KEY") {
        let p = OpenAIProvider::new(&key, "deepseek-chat")
            .with_base_url("https://api.deepseek.com/v1");
        providers.push(ProviderInfo {
            name: "deepseek/deepseek-chat".into(),
            provider: Box::new(p),
        });
    }

    // Moonshot / kimi-k2.5
    if let Ok(key) = std::env::var("KIMI_API_KEY") {
        let p = OpenAIProvider::new(&key, "kimi-k2.5")
            .with_base_url("https://api.moonshot.ai/v1");
        providers.push(ProviderInfo {
            name: "moonshot/kimi-k2.5".into(),
            provider: Box::new(p),
        });
    }

    // MiniMax (direct)
    if let Ok(key) = std::env::var("MINIMAX_API_KEY") {
        let p = OpenAIProvider::new(&key, "MiniMax-M2.7")
            .with_base_url("https://api.minimax.io/v1");
        providers.push(ProviderInfo {
            name: "minimax/MiniMax-M2.7".into(),
            provider: Box::new(p),
        });
    }

    // Z.AI / GLM-5 (Anthropic-compatible API)
    if let Ok(key) = std::env::var("ZAI_API_KEY") {
        let p = AnthropicProvider::new(&key, "glm-5")
            .with_base_url("https://api.z.ai/api/anthropic");
        providers.push(ProviderInfo {
            name: "zai/glm-5".into(),
            provider: Box::new(p),
        });
    }

    // Gemini 2.5 Flash
    if let Ok(key) = std::env::var("GEMINI_API_KEY") {
        providers.push(ProviderInfo {
            name: "gemini/gemini-2.5-flash".into(),
            provider: Box::new(GeminiProvider::new(&key, "gemini-2.5-flash")),
        });
        providers.push(ProviderInfo {
            name: "gemini/gemini-2.5-flash-lite".into(),
            provider: Box::new(GeminiProvider::new(&key, "gemini-2.5-flash-lite")),
        });
    }

    // OpenAI
    if let Ok(key) = std::env::var("OPENAI_API_KEY") {
        providers.push(ProviderInfo {
            name: "openai/gpt-4o".into(),
            provider: Box::new(OpenAIProvider::new(&key, "gpt-4o")),
        });
        providers.push(ProviderInfo {
            name: "openai/gpt-4o-mini".into(),
            provider: Box::new(OpenAIProvider::new(&key, "gpt-4o-mini")),
        });
    }

    // NVIDIA NIM
    if let Ok(key) = std::env::var("NVIDIA_API_KEY") {
        let nvidia_models = [
            ("nvidia/qwen3.5-397b", "qwen/qwen3.5-397b-a17b"),
            ("nvidia/deepseek-v3.2", "deepseek-ai/deepseek-v3.2"),
            ("nvidia/kimi-k2.5", "moonshotai/kimi-k2.5"),
            ("nvidia/minimax-m2.5", "minimaxai/minimax-m2.5"),
            ("nvidia/glm-5", "z-ai/glm5"),
        ];
        for (name, model) in nvidia_models {
            let p = OpenAIProvider::new(&key, model)
                .with_base_url("https://integrate.api.nvidia.com/v1");
            providers.push(ProviderInfo {
                name: name.into(),
                provider: Box::new(p),
            });
        }
    }

    providers
}

// ── Test result types ───────────────────────────────────────────

#[derive(Debug, Serialize, Clone)]
struct StabilityResult {
    tool_count: usize,
    attempts: usize,
    successes: usize,
    empty_responses: usize,
    errors: usize,
    response_rate: f64,
    avg_latency_ms: u64,
    p95_latency_ms: u64,
}

#[derive(Debug, Serialize, Clone)]
struct QualityResult {
    tool_selection_accuracy: f64,
    argument_accuracy: f64,
    false_positive_rate: f64,
    sequential_completion: f64,
}

#[derive(Debug, Serialize, Clone)]
struct StressResult {
    parallel_tool_rate: f64,
    complex_schema_rate: f64,
    rapid_fire_degradation: f64,
}

#[derive(Debug, Serialize)]
struct ProviderReport {
    provider: String,
    stability: Vec<StabilityResult>,
    quality: Option<QualityResult>,
    stress: Option<StressResult>,
    cliff_point: usize,
    max_tools_90pct: usize,
    overall_score: f64,
}

// ── Core test functions ─────────────────────────────────────────

async fn test_stability_at_tool_count(
    provider: &dyn LlmProvider,
    tool_count: usize,
    attempts: usize,
) -> StabilityResult {
    let tools = generate_tools(tool_count);
    let tool_specs: Vec<ToolSpec> = tools.iter().cloned().collect();
    let config = ChatConfig::default();

    let messages = vec![octos_core::Message {
        role: octos_core::MessageRole::User,
        content: "What is the weather in Tokyo?".to_string(),
        media: vec![],
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: None,
        timestamp: chrono::Utc::now(),
    }];

    let mut successes = 0;
    let mut empty = 0;
    let mut errors = 0;
    let mut latencies = Vec::new();

    for _ in 0..attempts {
        let start = Instant::now();
        match provider.chat(&messages, &tool_specs, &config).await {
            Ok(resp) => {
                let elapsed = start.elapsed().as_millis() as u64;
                latencies.push(elapsed);

                let has_content = resp.content.as_ref().map(|c| !c.is_empty()).unwrap_or(false);
                let has_tools = !resp.tool_calls.is_empty();

                if has_content || has_tools {
                    successes += 1;
                } else {
                    empty += 1;
                }
            }
            Err(_) => {
                errors += 1;
            }
        }
        // Small delay to avoid rate limiting
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    latencies.sort();
    let avg_latency = if latencies.is_empty() {
        0
    } else {
        latencies.iter().sum::<u64>() / latencies.len() as u64
    };
    let p95_latency = if latencies.is_empty() {
        0
    } else {
        latencies[((latencies.len() as f64) * 0.95) as usize].min(*latencies.last().unwrap())
    };

    StabilityResult {
        tool_count,
        attempts,
        successes,
        empty_responses: empty,
        errors,
        response_rate: successes as f64 / attempts as f64,
        avg_latency_ms: avg_latency,
        p95_latency_ms: p95_latency,
    }
}

async fn test_quality(provider: &dyn LlmProvider) -> QualityResult {
    let tools = generate_tools(15);
    let tool_specs: Vec<ToolSpec> = tools.iter().cloned().collect();
    let config = ChatConfig::default();

    // Tool selection tests: query → expected tool
    let selection_tests = vec![
        ("What's the weather in Paris?", "get_weather"),
        ("Search the web for latest AI news", "web_search"),
        ("Read the file /tmp/readme.md", "read_file"),
        ("Send an email to bob@example.com about the meeting", "send_email"),
        ("What time is it in Tokyo?", "get_time"),
        ("List all files in /home/user/docs", "list_dir"),
        ("Translate 'hello world' to Chinese", "translate"),
        ("Calculate 15 * 37 + 42", "calculate"),
        ("Write 'hello' to /tmp/test.txt", "write_file"),
        ("Run the command 'ls -la'", "run_shell"),
    ];

    let mut correct_selections = 0;
    for (query, expected_tool) in &selection_tests {
        let messages = vec![octos_core::Message {
            role: octos_core::MessageRole::User,
            content: query.to_string(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            timestamp: chrono::Utc::now(),
        }];

        if let Ok(resp) = provider.chat(&messages, &tool_specs, &config).await {
            if resp.tool_calls.iter().any(|tc| tc.name == *expected_tool) {
                correct_selections += 1;
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    // No-tool tests: should return text, not tool calls
    let no_tool_tests = vec![
        "What is 2 + 2?",
        "Tell me a joke",
        "Explain what Rust is",
        "Say hello",
        "What's the meaning of life?",
    ];

    let mut false_positives = 0;
    for query in &no_tool_tests {
        let messages = vec![octos_core::Message {
            role: octos_core::MessageRole::User,
            content: query.to_string(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            timestamp: chrono::Utc::now(),
        }];

        if let Ok(resp) = provider.chat(&messages, &tool_specs, &config).await {
            if !resp.tool_calls.is_empty() {
                false_positives += 1;
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    // Argument accuracy: check key fields are populated
    let arg_tests = vec![
        ("Get weather in London in celsius", "get_weather", vec!["city"]),
        ("Search for 'rust programming' and get 5 results", "web_search", vec!["query"]),
        ("Read file /etc/hosts from line 10 limit 20", "read_file", vec!["path"]),
    ];

    let mut correct_args = 0;
    let total_args = arg_tests.len();
    for (query, expected_tool, required_fields) in &arg_tests {
        let messages = vec![octos_core::Message {
            role: octos_core::MessageRole::User,
            content: query.to_string(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            timestamp: chrono::Utc::now(),
        }];

        if let Ok(resp) = provider.chat(&messages, &tool_specs, &config).await {
            if let Some(tc) = resp.tool_calls.iter().find(|tc| tc.name == *expected_tool) {
                {
                    let args = &tc.arguments;
                    // arguments is already serde_json::Value
                    let all_present = required_fields
                        .iter()
                        .all(|f| args.get(f).is_some());
                    if all_present {
                        correct_args += 1;
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    QualityResult {
        tool_selection_accuracy: correct_selections as f64 / selection_tests.len() as f64,
        argument_accuracy: correct_args as f64 / total_args as f64,
        false_positive_rate: false_positives as f64 / no_tool_tests.len() as f64,
        sequential_completion: 0.0, // TODO: multi-turn test
    }
}

async fn test_stress(provider: &dyn LlmProvider) -> StressResult {
    let tools = generate_tools(15);
    let tool_specs: Vec<ToolSpec> = tools.iter().cloned().collect();
    let config = ChatConfig::default();

    // Parallel tool calls: ask for 3 things at once
    let parallel_query = "Get the weather in Tokyo, Paris, and London simultaneously";
    let messages = vec![octos_core::Message {
        role: octos_core::MessageRole::User,
        content: parallel_query.to_string(),
        media: vec![],
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: None,
        timestamp: chrono::Utc::now(),
    }];

    let parallel_rate = if let Ok(resp) = provider.chat(&messages, &tool_specs, &config).await {
        if resp.tool_calls.len() >= 3 {
            1.0
        } else if resp.tool_calls.len() >= 2 {
            0.5
        } else {
            0.0
        }
    } else {
        0.0
    };

    // Complex schema: tool with many params
    let complex_tools = vec![make_tool(
        "complex_deploy",
        "Deploy a complex service with full configuration",
        serde_json::json!({
            "type": "object",
            "properties": {
                "service_name": {"type": "string"},
                "image": {"type": "string"},
                "port": {"type": "integer"},
                "replicas": {"type": "integer"},
                "env": {"type": "object", "additionalProperties": {"type": "string"}},
                "volumes": {"type": "array", "items": {"type": "object", "properties": {
                    "host": {"type": "string"}, "container": {"type": "string"}, "mode": {"type": "string", "enum": ["ro", "rw"]}
                }}},
                "health_check": {"type": "object", "properties": {
                    "path": {"type": "string"}, "interval_secs": {"type": "integer"}, "timeout_secs": {"type": "integer"}
                }},
                "resource_limits": {"type": "object", "properties": {
                    "cpu": {"type": "string"}, "memory": {"type": "string"}
                }},
                "labels": {"type": "object", "additionalProperties": {"type": "string"}},
                "network": {"type": "string", "enum": ["bridge", "host", "none"]}
            },
            "required": ["service_name", "image", "port"]
        }),
    )];

    let complex_messages = vec![octos_core::Message {
        role: octos_core::MessageRole::User,
        content: "Deploy a web service called 'myapp' using image nginx:latest on port 8080 with 3 replicas, a health check on /health every 30s, and CPU limit of 2 cores".to_string(),
        media: vec![],
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: None,
        timestamp: chrono::Utc::now(),
    }];

    let complex_rate = if let Ok(resp) = provider
        .chat(&complex_messages, &complex_tools, &config)
        .await
    {
        if let Some(tc) = resp.tool_calls.first() {
            let args = &tc.arguments;
            let has_required = args.get("service_name").is_some()
                && args.get("image").is_some()
                && args.get("port").is_some();
            let has_optional = args.get("replicas").is_some()
                || args.get("health_check").is_some()
                || args.get("resource_limits").is_some();
            if has_required && has_optional {
                1.0
            } else if has_required {
                0.5
            } else {
                0.0
            }
        } else {
            0.0
        }
    } else {
        0.0
    };

    // Rapid fire: 5 calls in quick succession
    let mut rapid_successes = 0;
    for _ in 0..5 {
        let messages = vec![octos_core::Message {
            role: octos_core::MessageRole::User,
            content: "What is the weather in Berlin?".to_string(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            timestamp: chrono::Utc::now(),
        }];
        if let Ok(resp) = provider.chat(&messages, &tool_specs, &config).await {
            if !resp.tool_calls.is_empty()
                || resp.content.as_ref().map(|c| !c.is_empty()).unwrap_or(false)
            {
                rapid_successes += 1;
            }
        }
        // Minimal delay
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    StressResult {
        parallel_tool_rate: parallel_rate,
        complex_schema_rate: complex_rate,
        rapid_fire_degradation: 1.0 - (rapid_successes as f64 / 5.0),
    }
}

// ── Main benchmark runner ───────────────────────────────────────

async fn run_benchmark(stability_only: bool) {
    let providers = create_providers();
    if providers.is_empty() {
        eprintln!("No providers configured. Set API key env vars.");
        return;
    }

    println!("\n{}", "=".repeat(70));
    println!("  TOOL USE CAPABILITY BENCHMARK");
    println!("  Providers: {}", providers.len());
    println!("  Mode: {}", if stability_only { "Quick Rank (stability)" } else { "Full Suite" });
    println!("{}\n", "=".repeat(70));

    let tool_counts = vec![5, 10, 15, 20, 25, 30];
    let attempts_per_level = 5;
    let mut reports: Vec<ProviderReport> = Vec::new();

    for pi in &providers {
        println!("\n--- {} ---", pi.name);

        // Phase 1: Stability
        let mut stability_results = Vec::new();
        let mut cliff = 30;
        let mut max_90 = 0;

        for &count in &tool_counts {
            print!("  tools={count:>2}: ");
            let result = test_stability_at_tool_count(
                pi.provider.as_ref(),
                count,
                attempts_per_level,
            )
            .await;

            let bar = "█".repeat((result.response_rate * 10.0) as usize);
            let empty_bar = "░".repeat(10 - (result.response_rate * 10.0) as usize);
            println!(
                "{bar}{empty_bar} {:.0}% ({}/{}) avg={}ms p95={}ms",
                result.response_rate * 100.0,
                result.successes,
                result.attempts,
                result.avg_latency_ms,
                result.p95_latency_ms,
            );

            if result.response_rate >= 0.9 {
                max_90 = count;
            }
            if result.response_rate < 0.5 && cliff == 30 {
                cliff = count;
            }

            stability_results.push(result);
        }

        println!("  → max_tools@90%: {max_90}, cliff: {cliff}");

        // Phase 2: Quality
        let quality = if !stability_only {
            print!("  quality: ");
            let q = test_quality(pi.provider.as_ref()).await;
            println!(
                "select={:.0}% args={:.0}% false_pos={:.0}%",
                q.tool_selection_accuracy * 100.0,
                q.argument_accuracy * 100.0,
                q.false_positive_rate * 100.0,
            );
            Some(q)
        } else {
            None
        };

        // Phase 3: Stress
        let stress = if !stability_only {
            print!("  stress: ");
            let s = test_stress(pi.provider.as_ref()).await;
            println!(
                "parallel={:.0}% complex={:.0}% rapid_degrade={:.0}%",
                s.parallel_tool_rate * 100.0,
                s.complex_schema_rate * 100.0,
                s.rapid_fire_degradation * 100.0,
            );
            Some(s)
        } else {
            None
        };

        // Overall score
        let stability_score = stability_results
            .iter()
            .map(|r| r.response_rate)
            .sum::<f64>()
            / stability_results.len() as f64;

        let quality_score = quality
            .as_ref()
            .map(|q| {
                (q.tool_selection_accuracy + q.argument_accuracy + (1.0 - q.false_positive_rate))
                    / 3.0
            })
            .unwrap_or(0.0);

        let stress_score = stress
            .as_ref()
            .map(|s| {
                (s.parallel_tool_rate + s.complex_schema_rate + (1.0 - s.rapid_fire_degradation))
                    / 3.0
            })
            .unwrap_or(0.0);

        let overall = if stability_only {
            stability_score
        } else {
            stability_score * 0.4 + quality_score * 0.4 + stress_score * 0.2
        };

        reports.push(ProviderReport {
            provider: pi.name.clone(),
            stability: stability_results,
            quality,
            stress,
            cliff_point: cliff,
            max_tools_90pct: max_90,
            overall_score: overall,
        });
    }

    // Sort by overall score
    reports.sort_by(|a, b| {
        b.overall_score
            .partial_cmp(&a.overall_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // Print rankings
    println!("\n\n{}", "=".repeat(70));
    println!("  RANKINGS");
    println!("{}\n", "=".repeat(70));
    println!(
        "{:<30} {:>8} {:>8} {:>8} {:>8}",
        "Provider", "Overall", "Max@90%", "Cliff", "Score"
    );
    println!("{}", "-".repeat(70));
    for (i, r) in reports.iter().enumerate() {
        println!(
            "{:<30} {:>7.0}% {:>7} {:>7} {:>7.0}%",
            format!("{}. {}", i + 1, r.provider),
            r.overall_score * 100.0,
            r.max_tools_90pct,
            r.cliff_point,
            r.overall_score * 100.0,
        );
    }

    // Save JSON report
    let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S");
    let report_path = format!("test-results/tool_benchmark_{timestamp}.json");
    if let Ok(json) = serde_json::to_string_pretty(&reports) {
        let _ = std::fs::create_dir_all("test-results");
        if let Err(e) = std::fs::write(&report_path, &json) {
            eprintln!("Failed to write report: {e}");
        } else {
            println!("\nReport saved to: {report_path}");
        }
    }
}

// ── Test entry points ───────────────────────────────────────────

#[tokio::test]
#[ignore]
async fn stability_quick_rank() {
    run_benchmark(true).await;
}

#[tokio::test]
#[ignore]
async fn full_benchmark() {
    run_benchmark(false).await;
}
