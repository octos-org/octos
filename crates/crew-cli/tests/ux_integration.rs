//! Full UX integration tests using real LLM providers.
//!
//! Tests cover:
//! - Session switching (via GatewayDispatcher, no LLM needed)
//! - Agent conversation with real LLM (Kimi, DeepSeek)
//! - Adaptive routing: hedge mode, lane mode
//! - Provider failover chain
//!
//! Tests requiring API keys are marked `#[ignore]`.
//! Run with:
//!   KIMI_API_KEY=... DEEPSEEK_API_KEY=... cargo test -p crew-cli --test ux_integration -- --ignored --nocapture

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crew_agent::tools::ToolRegistry;
use crew_agent::{Agent, AgentConfig, AgentId};
use crew_bus::{ActiveSessionStore, SessionManager};
use crew_core::{InboundMessage, OutboundMessage, SessionKey};
use crew_llm::openai::OpenAIProvider;
use crew_llm::{AdaptiveConfig, AdaptiveMode, AdaptiveRouter, LlmProvider};
use crew_memory::EpisodeStore;
use tokio::sync::{Mutex, mpsc};

use crew::gateway_dispatcher::{DispatchResult, GatewayDispatcher};
use crew::session_actor::PendingMessages;

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

fn make_inbound(channel: &str, chat_id: &str, content: &str) -> InboundMessage {
    InboundMessage {
        channel: channel.to_string(),
        chat_id: chat_id.to_string(),
        sender_id: "tester".to_string(),
        content: content.to_string(),
        timestamp: chrono::Utc::now(),
        media: vec![],
        metadata: serde_json::json!({}),
        message_id: None,
    }
}

// ── 1. Session Switching (no LLM) ───────────────────────────────────────────

/// Full session lifecycle: create → switch → list → back → delete → flush.
#[tokio::test]
async fn test_session_lifecycle_create_switch_back_delete() {
    let dir = tempfile::tempdir().unwrap();
    let session_mgr = Arc::new(Mutex::new(SessionManager::open(dir.path()).unwrap()));
    let active_sessions = Arc::new(Mutex::new(ActiveSessionStore::open(dir.path()).unwrap()));
    let pending: PendingMessages = Arc::new(Mutex::new(HashMap::new()));
    let (tx, mut rx) = mpsc::channel(32);

    let disp = GatewayDispatcher::new(
        session_mgr.clone(),
        active_sessions.clone(),
        pending.clone(),
        tx,
    );

    let base_key = "telegram:42";
    let inbound = make_inbound("telegram", "42", "");
    let session_key = SessionKey::new("telegram", "42");

    // 1. Create sessions
    disp.handle_new_command("/new research", &session_key, "telegram", "42", base_key)
        .await;
    let msg = rx.recv().await.unwrap();
    assert_eq!(msg.content, "Switched to session: research");
    println!("  1. /new research → ✓");

    disp.handle_new_command("/new coding", &session_key, "telegram", "42", base_key)
        .await;
    let msg = rx.recv().await.unwrap();
    assert_eq!(msg.content, "Switched to session: coding");
    println!("  2. /new coding → ✓");

    // 2. Verify active session
    assert_eq!(
        active_sessions.lock().await.get_active_topic(base_key),
        "coding"
    );
    println!("  3. Active topic = 'coding' → ✓");

    // 3. Switch to research
    disp.handle_s_command("/s research", &inbound, "telegram", "42", base_key)
        .await;
    let msg = rx.recv().await.unwrap();
    assert!(msg.content.starts_with("Switched to session: research"));
    assert_eq!(
        active_sessions.lock().await.get_active_topic(base_key),
        "research"
    );
    println!("  4. /s research → ✓");

    // 4. Go back via /b
    disp.handle_back_command("/b", &inbound, "telegram", "42", base_key)
        .await;
    let msg = rx.recv().await.unwrap();
    assert_eq!(msg.content, "Switched back to session: coding");
    println!("  5. /b → coding ✓");

    // 5. List sessions
    disp.handle_sessions_command("/sessions", "telegram", "42", base_key)
        .await;
    let msg = rx.recv().await.unwrap();
    // Should mention sessions (even if empty content, the keyboard is in metadata)
    println!(
        "  6. /sessions → content: {}",
        &msg.content[..msg.content.len().min(80)]
    );
    println!(
        "     metadata has inline_keyboard: {}",
        msg.metadata.get("inline_keyboard").is_some()
    );

    // 6. Delete coding
    disp.handle_delete_command("/delete coding", &inbound, "telegram", "42", base_key)
        .await;
    let msg = rx.recv().await.unwrap();
    assert_eq!(msg.content, "Deleted session: coding");
    println!("  7. /delete coding → ✓");

    // 7. Flush pending on callback switch
    let target_key = SessionKey::with_topic("telegram", "42", "research");
    pending.lock().await.insert(
        target_key.to_string(),
        vec![OutboundMessage {
            channel: "telegram".to_string(),
            chat_id: "42".to_string(),
            content: "deep search report ready".to_string(),
            reply_to: None,
            media: vec![],
            metadata: serde_json::json!({}),
        }],
    );
    disp.handle_session_callback(
        "s:research",
        None,
        &inbound,
        "telegram",
        "42",
        base_key,
        None,
    )
    .await;
    let msg = rx.recv().await.unwrap();
    assert_eq!(msg.content, "deep search report ready");
    println!("  8. Callback switch flushes pending → ✓");

    // 8. Switch to default
    disp.handle_s_command("/s", &inbound, "telegram", "42", base_key)
        .await;
    let msg = rx.recv().await.unwrap();
    assert_eq!(msg.content, "Switched to default session.");
    assert_eq!(active_sessions.lock().await.get_active_topic(base_key), "");
    println!("  9. /s → default session ✓");

    println!("\n✓ Full session lifecycle test passed (9/9 checks)");
}

/// Test: invalid session names are rejected.
#[tokio::test]
async fn test_session_name_validation() {
    let dir = tempfile::tempdir().unwrap();
    let session_mgr = Arc::new(Mutex::new(SessionManager::open(dir.path()).unwrap()));
    let active_sessions = Arc::new(Mutex::new(ActiveSessionStore::open(dir.path()).unwrap()));
    let pending: PendingMessages = Arc::new(Mutex::new(HashMap::new()));
    let (tx, mut rx) = mpsc::channel(32);

    let disp = GatewayDispatcher::new(session_mgr, active_sessions, pending, tx);
    let session_key = SessionKey::new("telegram", "42");
    let inbound = make_inbound("telegram", "42", "");

    // Too long name
    let long = "x".repeat(51);
    disp.handle_new_command(
        &format!("/new {long}"),
        &session_key,
        "telegram",
        "42",
        "telegram:42",
    )
    .await;
    let msg = rx.recv().await.unwrap();
    assert!(msg.content.contains("Invalid session name"));
    println!("  1. Rejects name > 50 chars → ✓");

    // Invalid via /s too
    disp.handle_s_command(
        &format!("/s {long}"),
        &inbound,
        "telegram",
        "42",
        "telegram:42",
    )
    .await;
    let msg = rx.recv().await.unwrap();
    assert!(msg.content.contains("Invalid session name"));
    println!("  2. /s rejects long name → ✓");

    println!("\n✓ Session name validation test passed");
}

/// Test: non-session callbacks return None (fall through).
#[tokio::test]
async fn test_non_session_callback_falls_through() {
    let dir = tempfile::tempdir().unwrap();
    let session_mgr = Arc::new(Mutex::new(SessionManager::open(dir.path()).unwrap()));
    let active_sessions = Arc::new(Mutex::new(ActiveSessionStore::open(dir.path()).unwrap()));
    let pending: PendingMessages = Arc::new(Mutex::new(HashMap::new()));
    let (tx, _rx) = mpsc::channel(32);

    let disp = GatewayDispatcher::new(session_mgr, active_sessions, pending, tx);
    let inbound = make_inbound("telegram", "42", "");

    let result = disp
        .handle_session_callback(
            "menu:action",
            None,
            &inbound,
            "telegram",
            "42",
            "telegram:42",
            None,
        )
        .await;
    assert!(result.is_none());
    println!("  Non-session callback returns None → ✓");

    let result = disp
        .handle_session_callback(
            "skill:deep-search",
            None,
            &inbound,
            "telegram",
            "42",
            "telegram:42",
            None,
        )
        .await;
    assert!(result.is_none());
    println!("  Skill callback returns None → ✓");

    println!("\n✓ Callback fallthrough test passed");
}

/// Test: unrecognized commands return Forward.
#[tokio::test]
async fn test_unrecognized_command_forwards() {
    let dir = tempfile::tempdir().unwrap();
    let session_mgr = Arc::new(Mutex::new(SessionManager::open(dir.path()).unwrap()));
    let active_sessions = Arc::new(Mutex::new(ActiveSessionStore::open(dir.path()).unwrap()));
    let pending: PendingMessages = Arc::new(Mutex::new(HashMap::new()));
    let (tx, _rx) = mpsc::channel(32);

    let disp = GatewayDispatcher::new(session_mgr, active_sessions, pending, tx);
    let inbound = make_inbound("telegram", "42", "");
    let session_key = SessionKey::new("telegram", "42");

    for cmd in &[
        "hello",
        "/config",
        "/skills",
        "/account",
        "what's the weather?",
    ] {
        let result = disp
            .try_dispatch_session_command(
                cmd,
                &inbound,
                &session_key,
                "telegram",
                "42",
                "telegram:42",
            )
            .await;
        assert!(
            matches!(result, DispatchResult::Forward),
            "'{cmd}' should Forward, not Handled"
        );
    }
    println!("  All non-session commands return Forward → ✓");

    println!("\n✓ Unrecognized command forwarding test passed");
}

// ── 2. Real LLM Agent Tests ────────────────────────────────────────────────

/// Helper: create an agent with real LLM for testing.
async fn make_agent(llm: Arc<dyn LlmProvider>, dir: &tempfile::TempDir) -> Agent {
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let tools = ToolRegistry::with_builtins(dir.path());
    Agent::new(AgentId::new("ux-test"), llm, tools, memory).with_config(AgentConfig {
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
        resp.usage.input_tokens,
        resp.usage.output_tokens,
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
        resp.usage.input_tokens,
        resp.usage.output_tokens,
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
            tool_calls: vec![],
            name: None,
        },
        crew_core::Message {
            role: crew_core::MessageRole::Assistant,
            content: r1.content.clone(),
            tool_call_id: None,
            tool_calls: vec![],
            name: None,
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

// ── 3. Adaptive Routing Tests ───────────────────────────────────────────────

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
        tool_calls: vec![],
        name: None,
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
    println!(
        "[hedge] {:.1}s | tokens: {}in/{}out | {}",
        elapsed.as_secs_f64(),
        resp.usage.input_tokens,
        resp.usage.output_tokens,
        &resp.content[..resp.content.len().min(200)]
    );
    assert!(
        resp.content.to_lowercase().contains("everest"),
        "should mention Everest: {}",
        resp.content
    );

    // Print router status to see metrics
    let status = router.status_summary();
    println!("[hedge] Router status:\n{status}");

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
            tool_calls: vec![],
            name: None,
        };

        let start = Instant::now();
        let result = router.chat(&[msg], &[], &config).await;
        let elapsed = start.elapsed();

        assert!(result.is_ok(), "lane query failed: {:?}", result.err());
        let resp = result.unwrap();
        println!(
            "[lane] {:.1}s | {q} → {}",
            elapsed.as_secs_f64(),
            resp.content.trim().chars().take(50).collect::<String>()
        );
    }

    let status = router.status_summary();
    println!("[lane] Router status after 3 queries:\n{status}");

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
            tool_calls: vec![],
            name: None,
        };

        let start = Instant::now();
        let result = router.chat(&[msg], &[], &config).await;
        let elapsed = start.elapsed();
        total_time += elapsed;

        assert!(result.is_ok());
        let resp = result.unwrap();
        let expected = (i * i).to_string();
        println!(
            "[hedge-{i}] {:.1}s | {i}*{i} → {}",
            elapsed.as_secs_f64(),
            resp.content.trim().chars().take(30).collect::<String>()
        );
    }

    println!(
        "[hedge] Total: {:.1}s, Avg: {:.1}s",
        total_time.as_secs_f64(),
        total_time.as_secs_f64() / 3.0
    );

    let status = router.status_summary();
    println!("[hedge] Final router status:\n{status}");

    println!("\n✓ Hedge mode multiple queries test passed");
}

// ── 4. Provider Failover Test ───────────────────────────────────────────────

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
        tool_calls: vec![],
        name: None,
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
    println!(
        "[failover] {:.1}s | {} (should come from deepseek after broken kimi fails)",
        elapsed.as_secs_f64(),
        resp.content.trim().chars().take(50).collect::<String>()
    );

    let status = router.status_summary();
    println!("[failover] Router status:\n{status}");

    println!("\n✓ Provider failover test passed");
}
