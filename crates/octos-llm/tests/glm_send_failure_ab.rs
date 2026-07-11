//! LIVE A/B: does Anthropic prompt-caching (#1640) change the transport
//! send-failure rate on z.ai's Anthropic endpoint for glm-5.2?
//!
//! Ignored by default (needs a real key + network). Run with:
//!   ZAI_API_KEY=... ZAI_MODEL=glm-5.2 AB_ROUNDS=12 \
//!     cargo test -p octos-llm --test glm_send_failure_ab -- --ignored --nocapture
//!
//! Emits a failure-rate table per config plus a cache-acceptance probe.

use chrono::Utc;
use futures::StreamExt;
use octos_core::{Message, MessageRole};
use octos_llm::anthropic::AnthropicProvider;
use octos_llm::{ChatConfig, LlmProvider, StreamEvent, ToolSpec};

fn msg(role: MessageRole, content: impl Into<String>) -> Message {
    Message {
        role,
        content: content.into(),
        media: vec![],
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: None,
        client_message_id: None,
        thread_id: None,
        timestamp: Utc::now(),
    }
}

fn tools() -> Vec<ToolSpec> {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {"path": {"type": "string"}},
        "required": ["path"]
    });
    vec![
        ToolSpec {
            name: "read_file".into(),
            description: "Read a file".into(),
            input_schema: schema.clone(),
        },
        ToolSpec {
            name: "write_file".into(),
            description: "Write a file".into(),
            input_schema: schema.clone(),
        },
        ToolSpec {
            name: "shell".into(),
            description: "Run a shell command".into(),
            input_schema: schema,
        },
    ]
}

fn big_history() -> Vec<Message> {
    // Pad system past the cacheable minimum so caching actually engages.
    let filler = "Reference material describing an Android AVD emulator install. ".repeat(700);
    let system = format!("You are a terse coding assistant. Background: {filler}");
    vec![
        msg(MessageRole::System, system),
        msg(
            MessageRole::User,
            "List three shell commands to create an Android AVD. One line each, no prose.",
        ),
    ]
}

#[derive(Default, Debug)]
struct Tally {
    ok: u32,
    send_failure: u32,
    http_error: u32,
    stream_error: u32,
    other: u32,
}

/// Fire N sequential streaming requests through ONE provider (shared reqwest
/// connection pool — the condition under which stale keepalive sockets
/// surface as "connection closed before message completed").
async fn run_config(label: &str, provider: &AnthropicProvider, rounds: u32) -> Tally {
    let history = big_history();
    let tools = tools();
    let config = ChatConfig {
        max_tokens: Some(128),
        ..Default::default()
    };
    let mut t = Tally::default();

    for i in 0..rounds {
        match provider.chat_stream(&history, &tools, &config).await {
            Ok(mut stream) => {
                let mut saw_error = None;
                while let Some(ev) = stream.next().await {
                    if let StreamEvent::Error(e) = ev {
                        saw_error = Some(e);
                    }
                }
                match saw_error {
                    None => t.ok += 1,
                    Some(e) => {
                        t.stream_error += 1;
                        println!("[{label}] round {i}: STREAM error: {e}");
                    }
                }
            }
            Err(e) => {
                let s = format!("{e:#}");
                if s.contains("failed to send") {
                    t.send_failure += 1;
                    println!("[{label}] round {i}: SEND FAILURE: {s}");
                } else if s.contains("HTTP") || s.contains("API error") {
                    t.http_error += 1;
                    println!("[{label}] round {i}: HTTP error: {s}");
                } else {
                    t.other += 1;
                    println!("[{label}] round {i}: OTHER error: {s}");
                }
            }
        }
        // Space requests to avoid colliding with a co-running soak agent's
        // rate budget.
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    }
    t
}

#[tokio::test]
#[ignore = "live: needs ZAI_API_KEY + network"]
async fn glm_cache_on_vs_off_send_failure_rate() {
    let key = std::env::var("ZAI_API_KEY").expect("ZAI_API_KEY not set");
    let model = std::env::var("ZAI_MODEL").unwrap_or_else(|_| "glm-5.2".to_string());
    let base = std::env::var("ZAI_BASE_URL")
        .unwrap_or_else(|_| "https://api.z.ai/api/anthropic".to_string());
    let rounds: u32 = std::env::var("AB_ROUNDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(12);

    println!("== model={model} base={base} rounds={rounds} ==");

    // ── Cache-acceptance probe: prove z.ai returns 200 + reports cache usage
    // when cache_control breakpoints are present (i.e. caching per se does not
    // make z.ai drop the connection).
    let cache_on = AnthropicProvider::new(&key, &model)
        .with_base_url(&base)
        .with_provider_label("zai")
        .with_prompt_caching(true);
    let history = big_history();
    let cfg = ChatConfig {
        max_tokens: Some(64),
        ..Default::default()
    };
    match cache_on.chat(&history, &tools(), &cfg).await {
        Ok(r1) => {
            println!(
                "cache-probe round 1: input={} output={} cache_read={} cache_write={}",
                r1.usage.input_tokens,
                r1.usage.output_tokens,
                r1.usage.cache_read_tokens,
                r1.usage.cache_write_tokens
            );
            let mut h2 = history.clone();
            h2.push(msg(
                MessageRole::Assistant,
                r1.content.clone().unwrap_or_default(),
            ));
            h2.push(msg(
                MessageRole::User,
                "Now the same but for iOS simulators.",
            ));
            if let Ok(r2) = cache_on.chat(&h2, &tools(), &cfg).await {
                println!(
                    "cache-probe round 2: input={} output={} cache_read={} cache_write={}  <- 200 OK w/ cache_control",
                    r2.usage.input_tokens,
                    r2.usage.output_tokens,
                    r2.usage.cache_read_tokens,
                    r2.usage.cache_write_tokens
                );
            }
        }
        Err(e) => println!("cache-probe FAILED: {e:#}"),
    }

    // ── A/B send-failure rate.
    let cache_off = AnthropicProvider::new(&key, &model)
        .with_base_url(&base)
        .with_provider_label("zai")
        .with_prompt_caching(false);

    let on = run_config("cache=ON ", &cache_on, rounds).await;
    let off = run_config("cache=OFF", &cache_off, rounds).await;

    println!("\n================ SEND-FAILURE A/B ({rounds} rounds each) ================");
    println!("config     ok  send_fail  http_err  stream_err  other");
    println!(
        "cache=ON  {:4}  {:9}  {:8}  {:10}  {:5}",
        on.ok, on.send_failure, on.http_error, on.stream_error, on.other
    );
    println!(
        "cache=OFF {:4}  {:9}  {:8}  {:10}  {:5}",
        off.ok, off.send_failure, off.http_error, off.stream_error, off.other
    );
    println!("=========================================================================");
}
