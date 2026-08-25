//! Live prompt-cache validation against the official DeepSeek endpoint.
//!
//! DeepSeek's context caching is automatic and server-side — there is no flag
//! a client can set. What a client CAN get wrong is prefix stability: if the
//! serialized head of the request (system prompt, then the tool array) differs
//! between rounds, every round is a full cache miss.
//!
//! This sends the same system prompt + tool array twice in a 2-round chat. On
//! round 1 the cache is written and `cache_read_tokens` is legitimately 0; on
//! round 2 the replayed prefix should come back from the cache. Reading round 1
//! alone proves nothing, which is the whole point of this being a 2-round test.
//!
//! Ignored by default (needs a real key + network). Run with:
//!   DEEPSEEK_API_KEY=... cargo test -p octos-llm --test deepseek_prompt_cache_live -- --ignored --nocapture

use chrono::Utc;
use octos_core::{Message, MessageRole};
use octos_llm::openai::OpenAIProvider;
use octos_llm::{ChatConfig, LlmProvider, ToolSpec};

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

/// A tool array shaped like the real agent's — the registry sorts by name
/// (`tools/registry.rs`), so mirror that here: identical order both rounds.
fn tools() -> Vec<ToolSpec> {
    let mut specs: Vec<ToolSpec> = ["exec_command", "glob", "grep", "list_dir", "read_file"]
        .iter()
        .map(|name| ToolSpec {
            name: (*name).to_string(),
            description: format!(
                "The {name} tool. Use it to inspect the workspace. \
                 Prefer it over guessing at file contents."
            ),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Target path." },
                    "pattern": { "type": "string", "description": "Match pattern." }
                },
                "required": ["path"]
            }),
        })
        .collect();
    specs.sort_by(|a, b| a.name.cmp(&b.name));
    specs
}

#[tokio::test]
#[ignore]
async fn deepseek_two_round_chat_reports_cache_read_tokens_on_round_two() {
    let key = std::env::var("DEEPSEEK_API_KEY").expect("DEEPSEEK_API_KEY not set");
    let model = std::env::var("DEEPSEEK_MODEL").unwrap_or_else(|_| "deepseek-v4-flash".to_string());
    let base_url = std::env::var("DEEPSEEK_BASE_URL")
        .unwrap_or_else(|_| "https://api.deepseek.com/v1".to_string());

    let provider = OpenAIProvider::new(&key, &model)
        .with_provider_label("deepseek")
        .with_base_url(&base_url);

    // DeepSeek caches on a 64-token block granularity with a 64-token minimum.
    // Pad well past it so a hit is unambiguous and roughly the size of a real
    // agent system prompt (~13k tokens as measured in the A/B run).
    let filler =
        "The quick brown fox jumps over the lazy dog near the riverbank at dawn. ".repeat(700);
    let system = format!(
        "You are a terse coding assistant; answer with a single word. \
         Reference corpus (background only, do not quote): {filler}"
    );

    let tools = tools();
    let config = ChatConfig {
        max_tokens: Some(512),
        ..Default::default()
    };

    let mut history = vec![
        msg(MessageRole::System, system),
        msg(MessageRole::User, "Reply with the single word: ping"),
    ];

    let r1 = provider
        .chat(&history, &tools, &config)
        .await
        .expect("round 1 chat failed");
    println!(
        "round 1 usage: input={} output={} cache_read={} cache_write={}",
        r1.usage.input_tokens,
        r1.usage.output_tokens,
        r1.usage.cache_read_tokens,
        r1.usage.cache_write_tokens,
    );

    history.push(msg(
        MessageRole::Assistant,
        r1.content.clone().unwrap_or_else(|| "pong".to_string()),
    ));
    history.push(msg(MessageRole::User, "Reply with the single word: pong"));

    let r2 = provider
        .chat(&history, &tools, &config)
        .await
        .expect("round 2 chat failed");
    println!(
        "round 2 usage: input={} output={} cache_read={} cache_write={}",
        r2.usage.input_tokens,
        r2.usage.output_tokens,
        r2.usage.cache_read_tokens,
        r2.usage.cache_write_tokens,
    );

    assert!(
        r2.usage.cache_read_tokens > 0,
        "expected round 2 to be served from DeepSeek's automatic prompt cache \
         (cache_read_tokens > 0). Either the replayed prefix is unstable between \
         rounds, or this endpoint does not report \
         `prompt_tokens_details.cached_tokens`: {:?}",
        r2.usage
    );
}
