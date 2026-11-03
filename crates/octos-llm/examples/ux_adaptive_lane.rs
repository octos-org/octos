//! Manual lane-mode diagnostics with real LLM providers.
//!
//! Prints each answer, request latency, and lane metrics without presenting the
//! output as a best-provider regression assertion.
//!
//! Requires: KIMI_API_KEY and DEEPSEEK_API_KEY (real paid provider calls).
//! Run: KIMI_API_KEY=... DEEPSEEK_API_KEY=... \
//!     cargo run -p octos-llm --example ux_adaptive_lane

use std::error::Error;
use std::sync::Arc;
use std::time::Instant;

use octos_core::{Message, MessageRole};
use octos_llm::openai::OpenAIProvider;
use octos_llm::{AdaptiveConfig, AdaptiveMode, AdaptiveRouter, ChatConfig, LlmProvider};

fn kimi() -> Result<Arc<dyn LlmProvider>, Box<dyn Error>> {
    let key = std::env::var("KIMI_API_KEY")?;
    Ok(Arc::new(
        OpenAIProvider::new(key, "kimi-k2.5").with_base_url("https://api.moonshot.ai/v1"),
    ))
}

fn deepseek() -> Result<Arc<dyn LlmProvider>, Box<dyn Error>> {
    let key = std::env::var("DEEPSEEK_API_KEY")?;
    Ok(Arc::new(
        OpenAIProvider::new(key, "deepseek-chat").with_base_url("https://api.deepseek.com/v1"),
    ))
}

fn msg(content: &str) -> Message {
    Message {
        role: MessageRole::User,
        content: content.to_string(),
        tool_call_id: None,
        tool_calls: None,
        media: vec![],
        reasoning_content: None,
        client_message_id: None,
        thread_id: None,
        timestamp: chrono::Utc::now(),
        source: None,
        participant: None,
    }
}

fn chat_config() -> ChatConfig {
    ChatConfig {
        max_tokens: Some(512),
        ..Default::default()
    }
}

/// Extract text from ChatResponse, checking both content and reasoning_content.
/// kimi-k2.5 is a thinking model that may put output in reasoning_content.
fn resp_text(resp: &octos_llm::ChatResponse) -> String {
    let mut parts = Vec::new();
    if let Some(c) = &resp.content {
        parts.push(c.as_str());
    }
    if let Some(r) = &resp.reasoning_content {
        parts.push(r.as_str());
    }
    parts.join(" ")
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let router = Arc::new(AdaptiveRouter::new(
        vec![kimi()?, deepseek()?],
        &[],
        AdaptiveConfig::default(),
    ));
    router.set_mode(AdaptiveMode::Lane);

    let questions = [
        "What is 1+1? Just the number.",
        "What is 2+2? Just the number.",
        "Capital of Japan? One word.",
    ];

    for q in &questions {
        let start = Instant::now();
        let resp = router.chat(&[msg(q)], &[], &chat_config()).await?;
        let text = resp_text(&resp);
        println!(
            "[lane] {:.1}s | {q} -> {}",
            start.elapsed().as_secs_f64(),
            text.trim().chars().take(30).collect::<String>()
        );
    }

    let snapshots = router.metrics_snapshots();
    for (name, model, snap) in &snapshots {
        println!(
            "  {name}/{model}: success={} latency={:.0}ms",
            snap.success_count, snap.latency_ema_ms
        );
    }
    Ok(())
}
