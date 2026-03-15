//! Full UX integration tests using real LLM providers.
//!
//! Tests cover:
//! - Agent conversation with real LLM (Kimi, DeepSeek)
//! - Adaptive routing: hedge mode, lane mode
//! - Provider failover chain
//!
//! Tests requiring API keys are marked `#[ignore]`.
//! Run with:
//!   KIMI_API_KEY=... DEEPSEEK_API_KEY=... cargo test -p crew-cli --test ux_integration -- --ignored --nocapture

use std::sync::Arc;
use std::time::{Duration, Instant};

use crew_agent::Agent;
use crew_agent::tools::ToolRegistry;
use crew_core::AgentId;
use crew_llm::openai::OpenAIProvider;
use crew_llm::{AdaptiveConfig, AdaptiveMode, AdaptiveRouter, LlmProvider};
use crew_memory::EpisodeStore;

// ── Provider Helpers ────────────────────────────────────────────────────────

fn kimi_key() -> String {
    std::env::var("KIMI_API_KEY").expect("KIMI_API_KEY must be set")
}

fn deepseek_key() -> String {
    std::env::var("DEEPSEEK_API_KEY").expect("DEEPSEEK_API_KEY must be set")
}

fn kimi_provider() -> Arc<dyn LlmProvider> {
    Arc::new(
        OpenAIProvider::new(kimi_key(), "kimi-2.5").with_base_url("https://api.moonshot.ai/v1"),
    )
}

fn deepseek_provider() -> Arc<dyn LlmProvider> {
    Arc::new(
        OpenAIProvider::new(deepseek_key(), "deepseek-chat")
            .with_base_url("https://api.deepseek.com/v1"),
    )
}

// ── Real LLM Agent Tests ────────────────────────────────────────────────

/// Helper: create an agent with real LLM for testing.
async fn make_agent(llm: Arc<dyn LlmProvider>, dir: &tempfile::TempDir) -> Agent {
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let tools = ToolRegistry::with_builtins(dir.path());
    Agent::new(AgentId::new("ux-test"), llm, tools, memory).with_config(crew_agent::AgentConfig {
        save_episodes: false,
        max_iterations: 1,
        ..Default::default()
    })
}

/// Test: basic conversation with Kimi provider.
#[tokio::test]
#[ignore]
async fn test_kimi_basic_conversation() {
    let dir = tempfile::tempdir().unwrap();
    let agent = make_agent(kimi_provider(), &dir).await;

    let start = Instant::now();
    let result = agent
        .process_message("What is 7 * 8? Reply with just the number.", &[], vec![])
        .await;

    let elapsed = start.elapsed();
    assert!(result.is_ok(), "kimi should respond: {:?}", result.err());
    let resp = result.unwrap();
    println!(
        "[kimi] {:.1}s | tokens: {}in/{}out | {}",
        elapsed.as_secs_f64(),
        resp.token_usage.input_tokens,
        resp.token_usage.output_tokens,
        &resp.content[..resp.content.len().min(100)]
    );
    assert!(
        resp.content.contains("56"),
        "should answer 56: {}",
        resp.content
    );

    println!("\n✓ Kimi basic conversation test passed");
}

/// Test: basic conversation with DeepSeek provider.
#[tokio::test]
#[ignore]
async fn test_deepseek_basic_conversation() {
    let dir = tempfile::tempdir().unwrap();
    let agent = make_agent(deepseek_provider(), &dir).await;

    let start = Instant::now();
    let result = agent
        .process_message("What is the capital of France? One word.", &[], vec![])
        .await;

    let elapsed = start.elapsed();
    assert!(
        result.is_ok(),
        "deepseek should respond: {:?}",
        result.err()
    );
    let resp = result.unwrap();
    println!(
        "[deepseek] {:.1}s | tokens: {}in/{}out | {}",
        elapsed.as_secs_f64(),
        resp.token_usage.input_tokens,
        resp.token_usage.output_tokens,
        &resp.content[..resp.content.len().min(100)]
    );
    assert!(
        resp.content.to_lowercase().contains("paris"),
        "should answer Paris: {}",
        resp.content
    );

    println!("\n✓ DeepSeek basic conversation test passed");
}

/// Test: multi-turn conversation preserves context.
#[tokio::test]
#[ignore]
async fn test_multi_turn_context_preservation() {
    let dir = tempfile::tempdir().unwrap();
    let agent = make_agent(kimi_provider(), &dir).await;

    // Turn 1: set context
    let r1 = agent
        .process_message(
            "The secret word is PINEAPPLE. Just acknowledge.",
            &[],
            vec![],
        )
        .await
        .expect("turn 1 should work");
    println!("[turn1] {}", &r1.content[..r1.content.len().min(100)]);

    // Turn 2: recall context
    let history = vec![
        crew_core::Message {
            role: crew_core::MessageRole::User,
            content: "The secret word is PINEAPPLE. Just acknowledge.".to_string(),
            tool_call_id: None,
            tool_calls: None,
            media: vec![],
            reasoning_content: None,
            timestamp: chrono::Utc::now(),
        },
        crew_core::Message {
            role: crew_core::MessageRole::Assistant,
            content: r1.content.clone(),
            tool_call_id: None,
            tool_calls: None,
            media: vec![],
            reasoning_content: None,
            timestamp: chrono::Utc::now(),
        },
    ];

    let r2 = agent
        .process_message("What was the secret word?", &history, vec![])
        .await
        .expect("turn 2 should work");
    println!("[turn2] {}", &r2.content[..r2.content.len().min(200)]);

    assert!(
        r2.content.to_lowercase().contains("pineapple"),
        "should recall PINEAPPLE: {}",
        r2.content
    );

    println!("\n✓ Multi-turn context preservation test passed");
}

// ── Adaptive Routing Tests ───────────────────────────────────────────────

/// Test: hedge mode races Kimi and DeepSeek, returns the faster response.
#[tokio::test]
#[ignore]
async fn test_adaptive_hedge_mode() {
    let kimi = kimi_provider();
    let deepseek = deepseek_provider();

    let router = Arc::new(AdaptiveRouter::new(
        vec![kimi.clone(), deepseek.clone()],
        AdaptiveConfig::default(),
    ));
    router.set_mode(AdaptiveMode::Hedge);

    let msg = crew_core::Message {
        role: crew_core::MessageRole::User,
        content: "What is the tallest mountain? One sentence.".to_string(),
        tool_call_id: None,
        tool_calls: None,
        media: vec![],
        reasoning_content: None,
        timestamp: chrono::Utc::now(),
    };
    let config = crew_llm::ChatConfig {
        max_tokens: Some(100),
        ..Default::default()
    };

    let start = Instant::now();
    let result = router.chat(&[msg], &[], &config).await;
    let elapsed = start.elapsed();

    assert!(
        result.is_ok(),
        "hedge should return a result: {:?}",
        result.err()
    );
    let resp = result.unwrap();
    let content = resp.content.as_deref().unwrap_or("");
    println!(
        "[hedge] {:.1}s | tokens: {}in/{}out | {}",
        elapsed.as_secs_f64(),
        resp.usage.input_tokens,
        resp.usage.output_tokens,
        &content[..content.len().min(200)]
    );
    assert!(
        content.to_lowercase().contains("everest"),
        "should mention Everest: {}",
        content
    );

    // Print router status to see metrics
    let status = router.adaptive_status();
    println!("[hedge] Router status: {:?}", status);

    println!("\n✓ Adaptive hedge mode test passed");
}

/// Test: lane mode selects best provider after building metrics.
#[tokio::test]
#[ignore]
async fn test_adaptive_lane_mode() {
    let kimi = kimi_provider();
    let deepseek = deepseek_provider();

    let router = Arc::new(AdaptiveRouter::new(
        vec![kimi.clone(), deepseek.clone()],
        AdaptiveConfig::default(),
    ));
    router.set_mode(AdaptiveMode::Lane);

    let config = crew_llm::ChatConfig {
        max_tokens: Some(50),
        ..Default::default()
    };

    // Send 3 queries to build up metrics
    let questions = [
        "What is 1+1? Just the number.",
        "What is 2+2? Just the number.",
        "Name the capital of Japan. One word.",
    ];

    for q in &questions {
        let msg = crew_core::Message {
            role: crew_core::MessageRole::User,
            content: q.to_string(),
            tool_call_id: None,
            tool_calls: None,
            media: vec![],
            reasoning_content: None,
            timestamp: chrono::Utc::now(),
        };

        let start = Instant::now();
        let result = router.chat(&[msg], &[], &config).await;
        let elapsed = start.elapsed();

        assert!(result.is_ok(), "lane query failed: {:?}", result.err());
        let resp = result.unwrap();
        let content = resp.content.as_deref().unwrap_or("");
        println!(
            "[lane] {:.1}s | {q} → {}",
            elapsed.as_secs_f64(),
            content.trim().chars().take(50).collect::<String>()
        );
    }

    let status = router.adaptive_status();
    println!("[lane] Router status after 3 queries: {:?}", status);

    println!("\n✓ Adaptive lane mode test passed");
}

/// Test: hedge mode with 3 rapid queries to build reliable metrics.
#[tokio::test]
#[ignore]
async fn test_adaptive_hedge_multiple_queries() {
    let kimi = kimi_provider();
    let deepseek = deepseek_provider();

    let router = Arc::new(AdaptiveRouter::new(
        vec![kimi, deepseek],
        AdaptiveConfig::default(),
    ));
    router.set_mode(AdaptiveMode::Hedge);

    let config = crew_llm::ChatConfig {
        max_tokens: Some(50),
        ..Default::default()
    };

    let mut total_time = Duration::ZERO;

    for i in 1..=3 {
        let msg = crew_core::Message {
            role: crew_core::MessageRole::User,
            content: format!("What is {i} * {i}? Just the number."),
            tool_call_id: None,
            tool_calls: None,
            media: vec![],
            reasoning_content: None,
            timestamp: chrono::Utc::now(),
        };

        let start = Instant::now();
        let result = router.chat(&[msg], &[], &config).await;
        let elapsed = start.elapsed();
        total_time += elapsed;

        assert!(result.is_ok());
        let resp = result.unwrap();
        let content = resp.content.as_deref().unwrap_or("");
        println!(
            "[hedge-{i}] {:.1}s | {i}*{i} → {}",
            elapsed.as_secs_f64(),
            content.trim().chars().take(30).collect::<String>()
        );
    }

    println!(
        "[hedge] Total: {:.1}s, Avg: {:.1}s",
        total_time.as_secs_f64(),
        total_time.as_secs_f64() / 3.0
    );

    let status = router.adaptive_status();
    println!("[hedge] Final router status: {:?}", status);

    println!("\n✓ Hedge mode multiple queries test passed");
}

// ── Provider Failover Test ───────────────────────────────────────────────

/// Test: failover from bad provider to good one.
#[tokio::test]
#[ignore]
async fn test_provider_failover() {
    // Create a broken provider (bad API key) and a working one
    let broken = Arc::new(
        OpenAIProvider::new("sk-INVALID-KEY", "kimi-2.5")
            .with_base_url("https://api.moonshot.ai/v1"),
    ) as Arc<dyn LlmProvider>;

    let working = deepseek_provider();

    let router = Arc::new(AdaptiveRouter::new(
        vec![broken, working],
        AdaptiveConfig {
            failure_threshold: 1, // trip circuit breaker fast
            ..Default::default()
        },
    ));
    // Use Off mode (priority order with failover)
    router.set_mode(AdaptiveMode::Off);

    let msg = crew_core::Message {
        role: crew_core::MessageRole::User,
        content: "What is 5+5? Just the number.".to_string(),
        tool_call_id: None,
        tool_calls: None,
        media: vec![],
        reasoning_content: None,
        timestamp: chrono::Utc::now(),
    };
    let config = crew_llm::ChatConfig {
        max_tokens: Some(50),
        ..Default::default()
    };

    let start = Instant::now();
    let result = router.chat(&[msg], &[], &config).await;
    let elapsed = start.elapsed();

    assert!(
        result.is_ok(),
        "should failover to working provider: {:?}",
        result.err()
    );
    let resp = result.unwrap();
    let content = resp.content.as_deref().unwrap_or("");
    println!(
        "[failover] {:.1}s | {} (should come from deepseek after broken kimi fails)",
        elapsed.as_secs_f64(),
        content.trim().chars().take(50).collect::<String>()
    );

    let status = router.adaptive_status();
    println!("[failover] Router status: {:?}", status);

    println!("\n✓ Provider failover test passed");
}
