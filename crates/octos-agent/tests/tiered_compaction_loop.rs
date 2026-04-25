//! M8.5 acceptance test: the three-tier compaction runner is wired into the
//! agent loop and actually shrinks an oversized tool result between
//! iterations.
//!
//! This test runs against a mock `LlmProvider` so there is no network
//! traffic.  It only checks behaviour the call-site owns (tier 1 applied
//! in-place, tier 2 payload lifted into `ChatConfig.context_management`).
//! Contract/M6 semantics are covered by `compaction_policy.rs`.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use eyre::Result;
use octos_agent::compaction::{CompactionPolicy, CompactionRunner as FullCompactionRunner};
use octos_agent::compaction_tiered::FullCompactor;
use octos_agent::{
    Agent, ApiMicroCompactionConfig, MicroCompactionPolicy, TieredCompactionRunner, ToolRegistry,
};
use octos_core::{AgentId, Message, ToolCall};
use octos_llm::{ChatConfig, ChatResponse, LlmProvider, StopReason, ToolSpec};
use octos_memory::EpisodeStore;

/// Provider that:
///   1. First call: returns a tool use of `dump_big` so the agent executes
///      it and produces a huge tool result.
///   2. Second call: captures the messages array (so the test can assert
///      tier 1 pruned the big output) and ends the turn.
///   3. Records every `ChatConfig.context_management` the loop plumbed in,
///      so the tier 2 assertion has something to look at.
struct RecordingProvider {
    calls: AtomicUsize,
    captured_messages: Mutex<Vec<Message>>,
    captured_context_management: Mutex<Vec<Option<serde_json::Value>>>,
    provider_name: &'static str,
}

#[async_trait]
impl LlmProvider for RecordingProvider {
    async fn chat(
        &self,
        messages: &[Message],
        _tools: &[ToolSpec],
        config: &ChatConfig,
    ) -> Result<ChatResponse> {
        let idx = self.calls.fetch_add(1, Ordering::SeqCst);
        self.captured_context_management
            .lock()
            .unwrap()
            .push(config.context_management.clone());
        if idx == 0 {
            Ok(ChatResponse {
                content: None,
                reasoning_content: None,
                tool_calls: vec![ToolCall {
                    id: "call_dump".to_string(),
                    name: "dump_big".to_string(),
                    arguments: serde_json::json!({}),
                    metadata: None,
                }],
                stop_reason: StopReason::ToolUse,
                usage: Default::default(),
                provider_index: None,
            })
        } else {
            *self.captured_messages.lock().unwrap() = messages.to_vec();
            Ok(ChatResponse {
                content: Some("done".to_string()),
                reasoning_content: None,
                tool_calls: vec![],
                stop_reason: StopReason::EndTurn,
                usage: Default::default(),
                provider_index: None,
            })
        }
    }

    fn model_id(&self) -> &str {
        "mock-model"
    }

    fn provider_name(&self) -> &str {
        self.provider_name
    }
}

struct DumpBigTool;

#[async_trait]
impl octos_agent::Tool for DumpBigTool {
    fn name(&self) -> &str {
        "dump_big"
    }

    fn description(&self) -> &str {
        "Emit a ~50KB payload so tier 1 has something to shrink"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }

    async fn execute(&self, _args: &serde_json::Value) -> Result<octos_agent::ToolResult> {
        Ok(octos_agent::ToolResult {
            output: "Z".repeat(50_000),
            success: true,
            ..Default::default()
        })
    }
}

fn build_tiered_runner(
    tier1: MicroCompactionPolicy,
    tier2: ApiMicroCompactionConfig,
) -> Arc<TieredCompactionRunner> {
    let policy = CompactionPolicy::default();
    let tier3: Box<dyn FullCompactor> = Box::new(FullCompactionRunner::new(policy));
    Arc::new(TieredCompactionRunner::new(tier1, tier2, tier3))
}

#[tokio::test]
async fn tier1_shrinks_50kb_tool_result_to_placeholder_on_next_iteration() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(RecordingProvider {
        calls: AtomicUsize::new(0),
        captured_messages: Mutex::new(Vec::new()),
        captured_context_management: Mutex::new(Vec::new()),
        provider_name: "anthropic",
    });
    let provider_for_agent: Arc<dyn LlmProvider> = provider.clone();

    let mut tools = ToolRegistry::with_builtins(dir.path());
    tools.register(DumpBigTool);

    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let runner = build_tiered_runner(
        MicroCompactionPolicy::default()
            .with_max_age_turns(u32::MAX) // disable stale path, keep size path
            .with_max_size_bytes_per_result(1024),
        ApiMicroCompactionConfig::enabled().with_keep_last_n_turns(4),
    );
    let agent = Agent::new(AgentId::new("m85-test"), provider_for_agent, tools, memory)
        .with_tiered_compaction(runner);

    let result = agent
        .process_message("please dump and return", &[], vec![])
        .await
        .expect("loop should finish");

    // Two LLM calls: the initial tool-use and the follow-up endturn.
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);

    // On iteration 2 the provider sees a messages array where the tool
    // result for `call_dump` is a placeholder, not 50KB of "Z".
    let captured = provider.captured_messages.lock().unwrap().clone();
    let tool_msg = captured
        .iter()
        .find(|m| m.tool_call_id.as_deref() == Some("call_dump"))
        .expect("tool result present in iteration 2");
    assert!(
        tool_msg
            .content
            .starts_with(octos_agent::compaction::TOOL_RESULT_PLACEHOLDER_PREFIX),
        "tier 1 did not shrink the 50KB tool result: {:?}",
        &tool_msg.content.chars().take(80).collect::<String>()
    );
    assert!(
        tool_msg.content.len() < 1_000,
        "placeholder is still too large: {} bytes",
        tool_msg.content.len()
    );

    // Tier 2 payload made it into both outgoing requests.
    let captured_ctx = provider.captured_context_management.lock().unwrap().clone();
    assert_eq!(captured_ctx.len(), 2);
    for (i, ctx) in captured_ctx.iter().enumerate() {
        let payload = ctx
            .as_ref()
            .unwrap_or_else(|| panic!("iteration {i} missing context_management payload"));
        assert_eq!(payload["edits"][0]["type"], "clear_tool_uses_20250919");
        assert_eq!(payload["edits"][0]["keep"]["value"], 4);
    }

    // The conversation still ends cleanly.
    assert_eq!(result.content, "done");
}

#[tokio::test]
async fn tier2_payload_is_omitted_for_non_anthropic_provider() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(RecordingProvider {
        calls: AtomicUsize::new(0),
        captured_messages: Mutex::new(Vec::new()),
        captured_context_management: Mutex::new(Vec::new()),
        provider_name: "openai",
    });
    let provider_for_agent: Arc<dyn LlmProvider> = provider.clone();

    let tools = ToolRegistry::with_builtins(dir.path());
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let runner = build_tiered_runner(
        MicroCompactionPolicy::default(),
        ApiMicroCompactionConfig::enabled(),
    );
    let agent = Agent::new(
        AgentId::new("m85-openai"),
        provider_for_agent,
        tools,
        memory,
    )
    .with_tiered_compaction(runner);

    // One-shot: provider returns EndTurn immediately so the loop makes a
    // single LLM call; the captured config_management must be None.
    // (We reuse RecordingProvider but skip the tool path by filtering on
    // call index; easier: use a custom minimal provider.)
    // The RecordingProvider above returns tool use on first call, so here we
    // deliberately use a different scenario: just observe the first call.
    // Registering no extra tool means `dump_big` is absent and execution
    // would fail, so we override the behaviour inline by using a minimal
    // provider built right here.
    // Simplification: let the loop run one LLM call and verify
    // captured_context_management[0] is None.

    struct EndTurnImmediately(AtomicUsize, Mutex<Vec<Option<serde_json::Value>>>);

    #[async_trait]
    impl LlmProvider for EndTurnImmediately {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            config: &ChatConfig,
        ) -> Result<ChatResponse> {
            self.0.fetch_add(1, Ordering::SeqCst);
            self.1
                .lock()
                .unwrap()
                .push(config.context_management.clone());
            Ok(ChatResponse {
                content: Some("done".into()),
                reasoning_content: None,
                tool_calls: vec![],
                stop_reason: StopReason::EndTurn,
                usage: Default::default(),
                provider_index: None,
            })
        }
        fn model_id(&self) -> &str {
            "mock-openai"
        }
        fn provider_name(&self) -> &str {
            "openai"
        }
    }

    let _ = agent; // discard the RecordingProvider path above; this test
    // builds its own agent so the provider_name inside the agent matches
    // the provider's own `provider_name()`.
    let openai_provider = Arc::new(EndTurnImmediately(
        AtomicUsize::new(0),
        Mutex::new(Vec::new()),
    ));
    let openai_provider_dyn: Arc<dyn LlmProvider> = openai_provider.clone();

    let dir2 = tempfile::tempdir().unwrap();
    let tools2 = ToolRegistry::with_builtins(dir2.path());
    let memory2 = Arc::new(
        EpisodeStore::open(dir2.path().join("memory"))
            .await
            .unwrap(),
    );
    let runner2 = build_tiered_runner(
        MicroCompactionPolicy::default(),
        ApiMicroCompactionConfig::enabled(),
    );
    let agent_openai = Agent::new(
        AgentId::new("m85-openai2"),
        openai_provider_dyn,
        tools2,
        memory2,
    )
    .with_tiered_compaction(runner2);

    let _ = agent_openai
        .process_message("hi", &[], vec![])
        .await
        .expect("loop ends cleanly");

    let captured = openai_provider.1.lock().unwrap().clone();
    assert_eq!(captured.len(), 1);
    assert!(
        captured[0].is_none(),
        "non-Anthropic provider must not receive the tier-2 header: {:?}",
        captured[0]
    );
}
