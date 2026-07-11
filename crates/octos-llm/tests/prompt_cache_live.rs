//! Live prompt-cache validation against a real Anthropic-protocol endpoint.
//!
//! Sends the same large system prompt twice in a 2-round chat and prints the
//! usage fields; on round 2 the prefix (system + round-1 history) should be
//! served from the provider's prompt cache (`cache_read_tokens > 0`).
//!
//! Ignored by default (needs a real key + network). Run with:
//!   ZAI_API_KEY=... cargo test -p octos-llm --test prompt_cache_live -- --ignored --nocapture

use chrono::Utc;
use octos_core::{Message, MessageRole};
use octos_llm::anthropic::AnthropicProvider;
use octos_llm::{ChatConfig, LlmProvider};

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

#[tokio::test]
#[ignore]
async fn zai_two_round_chat_reports_cache_read_tokens_on_round_two() {
    let key = std::env::var("ZAI_API_KEY").expect("ZAI_API_KEY not set");
    let model = std::env::var("ZAI_MODEL").unwrap_or_else(|_| "glm-4.7".to_string());
    let provider = AnthropicProvider::new(key, &model)
        .with_base_url("https://api.z.ai/api/anthropic")
        .with_provider_label("zai");

    // Anthropic-protocol caches have a minimum cacheable prefix (1024-4096
    // tokens depending on model) — pad the system prompt well past it.
    let filler =
        "The quick brown fox jumps over the lazy dog near the riverbank at dawn. ".repeat(700);
    let system = format!(
        "You are a terse assistant; answer with a single word. \
         Reference corpus (background only, do not quote): {filler}"
    );

    let mut history = vec![
        msg(MessageRole::System, system),
        msg(MessageRole::User, "Reply with the single word: ping"),
    ];
    let config = ChatConfig {
        max_tokens: Some(512),
        ..Default::default()
    };

    let r1 = provider
        .chat(&history, &[], &config)
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
        .chat(&history, &[], &config)
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
        "expected round 2 to be served from the provider prompt cache \
         (cache_read_tokens > 0); the endpoint either ignored the \
         cache_control breakpoints or does not report cache usage: {:?}",
        r2.usage
    );
}
