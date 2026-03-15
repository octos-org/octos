//! UX integration tests for adaptive routing with real LLM providers.
//!
//! Tests: hedge mode, lane mode, provider failover.
//!
//! Requires: KIMI_API_KEY, DEEPSEEK_API_KEY
//! Run: cargo test -p crew-llm --test ux_adaptive -- --ignored --nocapture

use std::sync::Arc;
use std::time::{Duration, Instant};

use crew_core::{Message, MessageRole};
use crew_llm::openai::OpenAIProvider;
use crew_llm::{AdaptiveConfig, AdaptiveMode, AdaptiveRouter, ChatConfig, LlmProvider};

fn kimi() -> Arc<dyn LlmProvider> {
    let key = std::env::var("KIMI_API_KEY").expect("KIMI_API_KEY required");
    Arc::new(OpenAIProvider::new(key, "kimi-k2.5").with_base_url("https://api.moonshot.ai/v1"))
}

fn deepseek() -> Arc<dyn LlmProvider> {
    let key = std::env::var("DEEPSEEK_API_KEY").expect("DEEPSEEK_API_KEY required");
    Arc::new(OpenAIProvider::new(key, "deepseek-chat").with_base_url("https://api.deepseek.com/v1"))
}

fn msg(content: &str) -> Message {
    Message {
        role: MessageRole::User,
        content: content.to_string(),
        tool_call_id: None,
        tool_calls: None,
        media: vec![],
        reasoning_content: None,
        timestamp: chrono::Utc::now(),
    }
}

fn assistant_msg(content: &str) -> Message {
    Message {
        role: MessageRole::Assistant,
        content: content.to_string(),
        tool_call_id: None,
        tool_calls: None,
        media: vec![],
        reasoning_content: None,
        timestamp: chrono::Utc::now(),
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
fn resp_text(resp: &crew_llm::ChatResponse) -> String {
    let mut parts = Vec::new();
    if let Some(c) = &resp.content {
        parts.push(c.as_str());
    }
    if let Some(r) = &resp.reasoning_content {
        parts.push(r.as_str());
    }
    parts.join(" ")
}

// -- Single Provider Smoke Tests -------------------------------------------

#[tokio::test]
#[ignore]
async fn test_kimi_responds() {
    let provider = kimi();
    let start = Instant::now();
    let resp = provider
        .chat(&[msg("What is 7*8? Just the number.")], &[], &chat_config())
        .await
        .expect("kimi should respond");
    let text = resp_text(&resp);
    println!(
        "[kimi] {:.1}s | {}in/{}out | {}",
        start.elapsed().as_secs_f64(),
        resp.usage.input_tokens,
        resp.usage.output_tokens,
        text.trim()
    );
    assert!(text.contains("56"), "expected 56: {text}");
    println!("OK Kimi responds correctly");
}

#[tokio::test]
#[ignore]
async fn test_deepseek_responds() {
    let provider = deepseek();
    let start = Instant::now();
    let resp = provider
        .chat(
            &[msg("Capital of France? One word.")],
            &[],
            &chat_config(),
        )
        .await
        .expect("deepseek should respond");
    let text = resp_text(&resp);
    println!(
        "[deepseek] {:.1}s | {}in/{}out | {}",
        start.elapsed().as_secs_f64(),
        resp.usage.input_tokens,
        resp.usage.output_tokens,
        text.trim()
    );
    assert!(
        text.to_lowercase().contains("paris"),
        "expected Paris: {text}",
    );
    println!("OK DeepSeek responds correctly");
}

// -- Hedge Mode ------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn test_hedge_mode_races_two_providers() {
    let router = Arc::new(AdaptiveRouter::new(
        vec![kimi(), deepseek()],
        AdaptiveConfig::default(),
    ));
    router.set_mode(AdaptiveMode::Hedge);

    let start = Instant::now();
    let resp = router
        .chat(
            &[msg("Tallest mountain in the world? One sentence.")],
            &[],
            &chat_config(),
        )
        .await
        .expect("hedge should respond");
    let elapsed = start.elapsed();
    let text = resp_text(&resp);

    println!(
        "[hedge] {:.1}s | {}in/{}out | {}",
        elapsed.as_secs_f64(),
        resp.usage.input_tokens,
        resp.usage.output_tokens,
        text.trim().chars().take(100).collect::<String>()
    );
    assert!(
        text.to_lowercase().contains("everest"),
        "expected Everest: {text}",
    );

    let status = router.adaptive_status();
    println!(
        "[hedge] mode={:?} providers={} qos={}",
        status.mode, status.provider_count, status.qos_ranking
    );
    println!("OK Hedge mode works");
}

#[tokio::test]
#[ignore]
async fn test_hedge_mode_3_queries_builds_metrics() {
    let router = Arc::new(AdaptiveRouter::new(
        vec![kimi(), deepseek()],
        AdaptiveConfig::default(),
    ));
    router.set_mode(AdaptiveMode::Hedge);

    let mut total = Duration::ZERO;
    for i in 1..=3 {
        let q = format!("What is {i}*{i}? Just the number.");
        let start = Instant::now();
        let resp = router
            .chat(&[msg(&q)], &[], &chat_config())
            .await
            .expect("query should work");
        let elapsed = start.elapsed();
        total += elapsed;
        let text = resp_text(&resp);
        println!(
            "[hedge-{i}] {:.1}s | {q} -> {}",
            elapsed.as_secs_f64(),
            text.trim().chars().take(20).collect::<String>()
        );
    }
    println!(
        "[hedge] total={:.1}s avg={:.1}s",
        total.as_secs_f64(),
        total.as_secs_f64() / 3.0
    );

    // Check metrics built up
    let snapshots = router.metrics_snapshots();
    for (name, model, snap) in &snapshots {
        println!(
            "  {name}/{model}: success={} failures={} latency_ema={:.0}ms",
            snap.success_count, snap.failure_count, snap.latency_ema_ms
        );
    }
    assert!(
        snapshots.iter().any(|(_, _, s)| s.success_count > 0),
        "should have recorded calls"
    );
    println!("OK Hedge mode builds metrics");
}

// -- Lane Mode -------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn test_lane_mode_selects_best_provider() {
    let router = Arc::new(AdaptiveRouter::new(
        vec![kimi(), deepseek()],
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
        let resp = router
            .chat(&[msg(q)], &[], &chat_config())
            .await
            .expect("lane should respond");
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
    println!("OK Lane mode works");
}

// -- Provider Failover -----------------------------------------------------

#[tokio::test]
#[ignore]
async fn test_failover_from_broken_to_working() {
    let broken: Arc<dyn LlmProvider> = Arc::new(
        OpenAIProvider::new("sk-INVALID", "kimi-k2.5")
            .with_base_url("https://api.moonshot.ai/v1"),
    );
    let working = deepseek();

    let router = Arc::new(AdaptiveRouter::new(
        vec![broken, working],
        AdaptiveConfig {
            failure_threshold: 1,
            ..Default::default()
        },
    ));
    router.set_mode(AdaptiveMode::Off);

    let start = Instant::now();
    let resp = router
        .chat(&[msg("5+5? Just the number.")], &[], &chat_config())
        .await
        .expect("should failover to working provider");
    let text = resp_text(&resp);
    println!(
        "[failover] {:.1}s | {}",
        start.elapsed().as_secs_f64(),
        text.trim()
    );
    assert!(text.contains("10"), "expected 10: {text}");

    let snapshots = router.metrics_snapshots();
    for (name, _model, snap) in &snapshots {
        println!(
            "  {name}: success={} failures={}",
            snap.success_count, snap.failure_count
        );
    }

    // The broken provider should have failures
    let broken_snap = &snapshots[0].2;
    assert!(
        broken_snap.failure_count > 0,
        "broken provider should have failures"
    );
    println!("OK Failover works correctly");
}

// -- Multi-turn Context ----------------------------------------------------

#[tokio::test]
#[ignore]
async fn test_multi_turn_context_preservation() {
    let provider = kimi();

    // Turn 1
    let r1 = provider
        .chat(
            &[msg("Remember: secret code is BLUE42. Acknowledge briefly.")],
            &[],
            &chat_config(),
        )
        .await
        .expect("turn 1");
    let r1_text = resp_text(&r1);
    println!(
        "[turn1] {}",
        r1_text.trim().chars().take(80).collect::<String>()
    );

    // Turn 2 with history — use content field for assistant message
    let assistant_content = r1.content.as_deref().unwrap_or(&r1_text);
    let history = vec![
        msg("Remember: secret code is BLUE42. Acknowledge briefly."),
        assistant_msg(assistant_content),
    ];
    let r2 = provider
        .chat(
            &[history, vec![msg("What was the secret code?")]].concat(),
            &[],
            &chat_config(),
        )
        .await
        .expect("turn 2");
    let r2_text = resp_text(&r2).to_lowercase();
    println!(
        "[turn2] {}",
        r2_text.trim().chars().take(120).collect::<String>()
    );
    assert!(
        r2_text.contains("blue42") || r2_text.contains("blue 42"),
        "should recall BLUE42: {r2_text}",
    );
    println!("OK Multi-turn context preserved");
}

// -- Responsiveness Observer -----------------------------------------------

#[tokio::test]
#[ignore]
async fn test_responsiveness_baseline_learning() {
    use crew_llm::ResponsivenessObserver;

    let mut observer = ResponsivenessObserver::new();
    let provider = deepseek();

    // Run 6 queries to build baseline (needs 5 samples)
    for i in 1..=6 {
        let start = Instant::now();
        let resp = provider
            .chat(
                &[msg(&format!("What is {i}+{i}? Just the number."))],
                &[],
                &chat_config(),
            )
            .await
            .expect("should respond");
        let latency = start.elapsed();
        observer.record(latency);
        let text = resp_text(&resp);
        println!(
            "[responsiveness-{i}] {:.0}ms | {}",
            latency.as_millis(),
            text.trim()
        );
    }

    let baseline = observer.baseline();
    let should_activate = observer.should_activate();
    println!(
        "[responsiveness] baseline={:?} should_activate={} samples={}",
        baseline,
        should_activate,
        observer.sample_count()
    );
    assert!(baseline.is_some(), "should have learned baseline after 6 queries");
    assert!(!should_activate, "should not activate with normal latencies");
    println!("OK Responsiveness baseline learning works");
}
