use super::*;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

use async_trait::async_trait;
use octos_core::{AgentId, MessageRole, TaskContext, TaskKind, ToolCall};
use octos_llm::{ChatResponse, LlmError, LlmProvider, StopReason, TokenUsage as LlmTokenUsage};
use octos_memory::EpisodeStore;

use crate::AgentConfig;

fn tool_use(tool_calls: Vec<ToolCall>, input_tokens: u32, output_tokens: u32) -> ChatResponse {
    ChatResponse {
        content: None,
        reasoning_content: None,
        tool_calls,
        stop_reason: StopReason::ToolUse,
        usage: LlmTokenUsage {
            input_tokens,
            output_tokens,
            ..Default::default()
        },
        provider_index: None,
    }
}

fn end_turn(content: &str, input_tokens: u32, output_tokens: u32) -> ChatResponse {
    ChatResponse {
        content: Some(content.to_string()),
        reasoning_content: None,
        tool_calls: vec![],
        stop_reason: StopReason::EndTurn,
        usage: LlmTokenUsage {
            input_tokens,
            output_tokens,
            ..Default::default()
        },
        provider_index: None,
    }
}

struct ScriptedProvider {
    responses: StdMutex<Vec<ChatResponse>>,
}

impl ScriptedProvider {
    fn new(responses: Vec<ChatResponse>) -> Self {
        Self {
            responses: StdMutex::new(responses.into_iter().rev().collect()),
        }
    }
}

#[async_trait]
impl LlmProvider for ScriptedProvider {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &ChatConfig,
    ) -> Result<ChatResponse> {
        self.responses
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pop()
            .ok_or_else(|| eyre::eyre!("scripted provider exhausted"))
    }

    fn model_id(&self) -> &str {
        "planner-test"
    }

    fn provider_name(&self) -> &str {
        "mock"
    }
}

/// Mix complete-but-rejected responses with transport/provider errors. The
/// response-only fixture above cannot reach the error arm's fallback exits.
struct MixedTerminalScript(StdMutex<VecDeque<Result<ChatResponse>>>);

#[async_trait]
impl LlmProvider for MixedTerminalScript {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &ChatConfig,
    ) -> Result<ChatResponse> {
        self.0
            .lock()
            .unwrap()
            .pop_front()
            .expect("mixed terminal script exhausted")
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[octos_llm::ToolSpec],
        config: &ChatConfig,
    ) -> Result<octos_llm::ChatStream> {
        use octos_llm::StreamEvent;
        let response = self.chat(messages, tools, config).await?;
        Ok(Box::pin(futures::stream::iter(vec![
            StreamEvent::ReasoningDelta(response.reasoning_content.unwrap_or_default()),
            StreamEvent::TextDelta(response.content.unwrap_or_default()),
            StreamEvent::Usage(response.usage),
            StreamEvent::Done(response.stop_reason),
        ])))
    }

    fn model_id(&self) -> &str {
        // Known pricing makes the test check attributed spend as well as all
        // four token counters; no actual provider is contacted.
        "claude-sonnet-4"
    }

    fn provider_name(&self) -> &str {
        "mock"
    }
}

fn mixed_terminal_response(content: &str, input: u32, output: u32) -> ChatResponse {
    let mut response = end_turn(content, input, output);
    response.reasoning_content = Some("Still inspecting the image.".into());
    response.usage.cache_read_tokens = 3;
    response.usage.cache_write_tokens = 4;
    response.usage.reasoning_tokens = 5;
    response
}

fn mixed_terminal_transport_error() -> Result<ChatResponse> {
    Err(octos_llm::StreamError::Transport {
        detail: "fixture transport failure".into(),
    }
    .into())
}

async fn assert_mixed_terminal_usage(
    attempts: Vec<Result<ChatResponse>>,
    expected: (u32, u32, u32, u32),
    succeeds: bool,
) {
    let expected_reasoning: u32 = attempts
        .iter()
        .filter_map(|attempt| attempt.as_ref().ok())
        .map(|response| response.usage.reasoning_tokens)
        .sum();
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(MixedTerminalScript(StdMutex::new(attempts.into())));
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent = Agent::new(
        AgentId::new("mixed-terminal-usage"),
        provider.clone(),
        ToolRegistry::new(),
        memory,
    );
    let mut turn = LoopTurnState::new(Instant::now());
    // Existing usage must remain intact, and must not be included again when
    // a recovered response is returned for the caller to record.
    turn.record_usage(
        &TokenUsage {
            input_tokens: 100,
            output_tokens: 200,
            cache_read_tokens: 300,
            cache_write_tokens: 400,
            ..Default::default()
        },
        None,
        Some(0.5),
    );
    let previous = turn.total_usage().clone();
    let result = agent
        .call_llm_with_hooks(
            &[Message::user("Inspect the image")],
            &[],
            &ChatConfig::default(),
            1,
            &previous,
            &mut turn,
        )
        .await;
    assert_eq!(result.is_ok(), succeeds, "{result:?}");
    assert!(provider.0.lock().unwrap().is_empty());
    if let Err(error) = &result {
        assert!(
            error.downcast_ref::<LlmError>().is_some()
                || error.downcast_ref::<octos_llm::StreamError>().is_some(),
            "usage settlement must preserve the typed error: {error:?}",
        );
    }
    if let Ok((response, _, cost)) = result {
        assert_eq!(turn.total_usage().input_tokens, 100);
        assert_eq!(turn.priced_spend(), Some(0.5));
        turn.record_llm_usage(&response.usage, None, cost);
    }
    let total = turn.total_usage();
    assert_eq!(
        total.reasoning_tokens, expected_reasoning,
        "rejected, recovered, and fallback responses retain reasoning exactly once"
    );
    assert_eq!(
        (
            total.input_tokens,
            total.output_tokens,
            total.cache_read_tokens,
            total.cache_write_tokens,
        ),
        (
            100 + expected.0,
            200 + expected.1,
            300 + expected.2,
            400 + expected.3
        ),
    );
    let pricing = octos_llm::pricing::model_pricing("claude-sonnet-4").unwrap();
    // The fixture is a residual-protocol mock, not an Anthropic provider.
    // All disjoint cache traffic must remain priced (read 1x, write 1.25x).
    let expected_cost = pricing.cost(expected.0, expected.1)
        + (f64::from(expected.2) + 1.25 * f64::from(expected.3)) * pricing.input_per_million
            / 1_000_000.0;
    assert!((turn.priced_spend().unwrap() - (0.5 + expected_cost)).abs() < 1e-12);
}

#[tokio::test]
async fn terminal_integrity_mixed_stream_recovery_charges_exactly_once() {
    // Successful recovery after stream errors: usage is charged exactly once.
    assert_mixed_terminal_usage(
        vec![
            Ok(mixed_terminal_response("", 10, 20)),
            mixed_terminal_transport_error(),
            Ok(mixed_terminal_response("Recovered answer", 30, 40)),
        ],
        (40, 60, 6, 8),
        true,
    )
    .await;
}

#[tokio::test]
async fn terminal_integrity_mixed_rejected_then_error_retains_usage() {
    // Non-retryable error after a rejected response retains its usage.
    assert_mixed_terminal_usage(
        vec![
            Ok(mixed_terminal_response("", 10, 20)),
            Err(LlmError::auth("fixture invalid key").into()),
        ],
        (10, 20, 3, 4),
        false,
    )
    .await;
}

/// Scripted provider that records the exact `(messages, tools)` of every
/// request so a test can assert on the prompt SHAPE the loop sent (roles,
/// positions, tool slices), not merely on message text.
struct RequestRecordingProvider {
    scripted: ScriptedProvider,
    requests: RecordedRequests,
}

/// `(messages, tools)` of every provider request, in call order.
type RecordedRequests = Arc<StdMutex<Vec<(Vec<Message>, Vec<octos_llm::ToolSpec>)>>>;

impl RequestRecordingProvider {
    fn new(responses: Vec<ChatResponse>, requests: RecordedRequests) -> Self {
        Self {
            scripted: ScriptedProvider::new(responses),
            requests,
        }
    }
}

#[async_trait]
impl LlmProvider for RequestRecordingProvider {
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[octos_llm::ToolSpec],
        config: &ChatConfig,
    ) -> Result<ChatResponse> {
        self.requests
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push((messages.to_vec(), tools.to_vec()));
        self.scripted.chat(messages, tools, config).await
    }

    fn model_id(&self) -> &str {
        "planner-test"
    }

    fn provider_name(&self) -> &str {
        "mock"
    }
}

use crate::prompt_context::{
    PromptContextManager, PromptContextPhase, PromptContextReport, PromptContextRequest,
};
use crate::tools::{Tool, ToolRegistry, ToolResult};

struct NamedEchoTool {
    name: &'static str,
    output: &'static str,
}

#[async_trait]
impl Tool for NamedEchoTool {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        "Echo a fixed tool response"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {}
        })
    }

    async fn execute(&self, _args: &serde_json::Value) -> Result<ToolResult> {
        Ok(ToolResult {
            output: self.output.to_string(),
            success: true,
            ..Default::default()
        })
    }
}

struct MultiToolThenEndProvider {
    calls: AtomicUsize,
}

#[async_trait]
impl LlmProvider for MultiToolThenEndProvider {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &octos_llm::ChatConfig,
    ) -> Result<ChatResponse> {
        let call = self.calls.fetch_add(1, AtomicOrdering::SeqCst);
        let response = match call {
            0 => tool_use(
                vec![
                    empty_call("call_alpha", "alpha"),
                    empty_call("call_beta", "beta"),
                ],
                0,
                0,
            ),
            1 => tool_use(vec![empty_call("call_gamma", "gamma")], 0, 0),
            _ => end_turn("done", 0, 0),
        };
        Ok(response)
    }

    fn model_id(&self) -> &str {
        "mock"
    }

    fn provider_name(&self) -> &str {
        "mock"
    }
}

struct TerminalFailureTool {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for TerminalFailureTool {
    fn name(&self) -> &str {
        "lesson_generate"
    }

    fn description(&self) -> &str {
        "Generate one complete lesson"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "tutor_context": { "type": "string" } }
        })
    }

    async fn execute(&self, _args: &serde_json::Value) -> Result<ToolResult> {
        self.calls.fetch_add(1, AtomicOrdering::SeqCst);
        Ok(ToolResult {
            output: "lesson generation exhausted its internal attempts".to_string(),
            success: false,
            structured_metadata: Some(serde_json::json!({
                "retryable": false,
                "do_not_retry_same_turn": true
            })),
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn process_message_preserves_tool_pair_order_across_iterations() {
    let dir = tempfile::tempdir().unwrap();
    let mut tools = ToolRegistry::with_builtins(dir.path());
    tools.register(NamedEchoTool {
        name: "alpha",
        output: "alpha ok",
    });
    tools.register(NamedEchoTool {
        name: "beta",
        output: "beta ok",
    });
    tools.register(NamedEchoTool {
        name: "gamma",
        output: "gamma ok",
    });

    let provider: Arc<dyn LlmProvider> = Arc::new(MultiToolThenEndProvider {
        calls: AtomicUsize::new(0),
    });
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent = Agent::new(AgentId::new("test-agent"), provider, tools, memory);

    let result = agent.process_message("do work", &[], vec![]).await.unwrap();
    let roles: Vec<MessageRole> = result.messages.iter().map(|m| m.role).collect();
    assert_eq!(
        roles,
        vec![
            MessageRole::User,
            MessageRole::Assistant,
            MessageRole::Tool,
            MessageRole::Tool,
            MessageRole::Assistant,
            MessageRole::Tool,
        ]
    );
    assert_eq!(result.content, "done");
    assert_eq!(result.messages[1].tool_calls.as_ref().unwrap().len(), 2);
    assert_eq!(result.messages[4].tool_calls.as_ref().unwrap().len(), 1);
    assert_eq!(
        result.messages[2].tool_call_id.as_deref(),
        Some("call_alpha")
    );
    assert_eq!(
        result.messages[3].tool_call_id.as_deref(),
        Some("call_beta")
    );
    assert_eq!(
        result.messages[5].tool_call_id.as_deref(),
        Some("call_gamma")
    );
}

// ─────────────────────────────────────────────────────────────────────
// Review A F-001 — dispatch_loop_error wiring.
// ─────────────────────────────────────────────────────────────────────

/// Counting summarizer used to prove the `CompactAndRetry` arm of
/// `handle_loop_error_with_dispatch` actually drives `maybe_run_turn_compaction`.
struct CountingSummarizer {
    calls: Arc<AtomicUsize>,
}

impl crate::summarizer::Summarizer for CountingSummarizer {
    fn kind(&self) -> &'static str {
        "counting_spy"
    }

    fn summarize(&self, messages: &[Message], budget_tokens: u32) -> Result<String> {
        self.calls.fetch_add(1, AtomicOrdering::SeqCst);
        Ok(crate::compaction::compact_messages(messages, budget_tokens))
    }
}

// ─────────────────────────────────────────────────────────────────────
// M8.10-C — LOOP DETECTED dedup.
// ─────────────────────────────────────────────────────────────────────

/// Mock LLM that always returns the same shell tool call with the same
/// arguments, forcing the loop detector to fire on iteration 4.
struct AlwaysSameToolProvider;

#[async_trait]
impl LlmProvider for AlwaysSameToolProvider {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &octos_llm::ChatConfig,
    ) -> Result<ChatResponse> {
        Ok(tool_use(
            vec![tool_call(
                "call_loop",
                "read_file",
                serde_json::json!({"path": "loopy.txt"}),
            )],
            0,
            0,
        ))
    }

    fn model_id(&self) -> &str {
        "mock"
    }

    fn provider_name(&self) -> &str {
        "mock"
    }
}

// ─────────────────────────────────────────────────────────────────────
// PR `fix/news-fetch-loop-and-detect-recovery` —
// LOOP DETECTED non-terminal recovery (`session web-1779494658716-mxrxe8`,
// ledger seq 214-562). On first fire we now inject a synthetic tool
// result carrying the warning and continue the loop for one more LLM
// iteration; on second fire we return a terminal `ConversationResponse`.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn inject_synthetic_results_pushes_assistant_then_tool_for_every_call() {
    let response = tool_use(
        vec![
            tool_call(
                "call_a",
                "news_fetch",
                serde_json::json!({"categories": ["tech"]}),
            ),
            tool_call(
                "call_b",
                "news_fetch",
                serde_json::json!({"categories": ["world"]}),
            ),
        ],
        0,
        0,
    );

    let dir = tempfile::tempdir().unwrap();
    let tools = ToolRegistry::with_builtins(dir.path());
    let provider: Arc<dyn LlmProvider> = Arc::new(AlwaysSameToolProvider);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let memory = runtime
        .block_on(async { Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap()) });
    let agent = Agent::new(AgentId::new("inject-test"), provider, tools, memory);

    let mut messages: Vec<Message> = Vec::new();
    super::super::loop_runner::inject_loop_detected_synthetic_results(
        &mut messages,
        &response,
        "[LOOP DETECTED] cycle length 1.",
        &agent,
    );

    // 1 assistant + 2 tool results (one per tool_call).
    assert_eq!(messages.len(), 3, "expected 1 assistant + 2 tool results");
    assert_eq!(messages[0].role, MessageRole::Assistant);
    assert_eq!(
        messages[0]
            .tool_calls
            .as_ref()
            .map(|tcs| tcs.len())
            .unwrap_or(0),
        2,
        "assistant message must carry the looping tool_calls so providers \
             can bind the synthetic tool-result messages back to them"
    );

    for (idx, msg) in messages[1..].iter().enumerate() {
        assert_eq!(msg.role, MessageRole::Tool, "tool message #{idx}");
        let id_expected = if idx == 0 { "call_a" } else { "call_b" };
        assert_eq!(msg.tool_call_id.as_deref(), Some(id_expected));
    }

    // First tool-result carries the warning + synthesis hint; second is
    // a short companion stub so the LLM doesn't think the second call
    // actually executed.
    assert!(
        messages[1].content.contains("[LOOP DETECTED]"),
        "primary tool result must echo the warning: got `{}`",
        messages[1].content
    );
    assert!(
        messages[1].content.contains("synthesise")
            || messages[1].content.contains("different tool"),
        "primary tool result must contain a synthesis hint so the LLM \
             knows how to course-correct: got `{}`",
        messages[1].content
    );
    assert!(
        messages[2].content.contains("[LOOP DETECTED]")
            && messages[2].content.contains("companion"),
        "companion tool result should mark itself as such: got `{}`",
        messages[2].content
    );
}

/// Codex MAJOR on PR #1181: the synthetic injection path bypassed the
/// `sanitize_tool_call_id` step that the normal `handle_tool_use` path
/// applies (loop_runner.rs ~line 1685). Moonshot/kimi (which dspfac uses)
/// emits IDs with colons like `admin_view_sessions:11` — OpenAI-style
/// schemas reject those, and our own duplicate-repair logic can collapse
/// them, leaving unanswered tool_calls on the next LLM call.
///
/// This test simulates a looping ChatResponse with a colon-bearing id and
/// asserts:
///   1. The injected synthetic messages carry a sanitized id (no colon).
///   2. The assistant message's `tool_calls[].id` matches the tool
///      result's `tool_call_id` 1:1 (same sanitized id end-to-end).
#[test]
fn inject_synthetic_results_sanitizes_tool_call_ids_with_colons() {
    let raw_id = "admin_view_sessions:11";
    let response = tool_use(
        vec![tool_call(
            raw_id,
            "news_fetch",
            serde_json::json!({"categories": ["tech"]}),
        )],
        0,
        0,
    );

    let dir = tempfile::tempdir().unwrap();
    let tools = ToolRegistry::with_builtins(dir.path());
    let provider: Arc<dyn LlmProvider> = Arc::new(AlwaysSameToolProvider);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let memory = runtime
        .block_on(async { Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap()) });
    let agent = Agent::new(AgentId::new("sanitize-test"), provider, tools, memory);

    let mut messages: Vec<Message> = Vec::new();
    super::super::loop_runner::inject_loop_detected_synthetic_results(
        &mut messages,
        &response,
        "[LOOP DETECTED] cycle length 1.",
        &agent,
    );

    // Layout: 1 assistant + 1 tool result.
    assert_eq!(messages.len(), 2, "expected 1 assistant + 1 tool result");

    // Extract the assistant tool_call id and the tool result's
    // tool_call_id; both must be the SAME sanitized value.
    let assistant_tc_id = messages[0]
        .tool_calls
        .as_ref()
        .and_then(|tcs| tcs.first())
        .map(|tc| tc.id.clone())
        .expect("assistant message must carry sanitized tool_calls");
    let tool_result_id = messages[1]
        .tool_call_id
        .clone()
        .expect("tool result must carry tool_call_id");

    // 1. Sanitized — no colon left over.
    assert!(
        !assistant_tc_id.contains(':'),
        "assistant tool_call id must be sanitized (no colon): got `{assistant_tc_id}`"
    );
    assert!(
        !tool_result_id.contains(':'),
        "tool result tool_call_id must be sanitized (no colon): got `{tool_result_id}`"
    );

    // 2. Same id on BOTH sides — providers bind tool_use ↔ tool_result
    // by exact id match, so any drift here would orphan the pair.
    assert_eq!(
        assistant_tc_id, tool_result_id,
        "assistant tool_calls[].id and tool result tool_call_id must \
             share the SAME sanitized id (1:1 pairing); raw_id was `{raw_id}`"
    );

    // 3. Concrete sanitized form: `:` → `_` per `sanitize_tool_call_id`.
    assert_eq!(
        assistant_tc_id, "admin_view_sessions_11",
        "sanitize_tool_call_id should replace `:` with `_`"
    );
}

// ----- Audit Gap-8: auto-fire check_workspace_contract on Completion -----

/// LLM stub that always returns a single EndTurn — used by the
/// Gap-8 tests to drive `run_task` straight to the contract-check
/// branch without iterating through tool calls.
struct EndTurnOnlyProvider;
#[async_trait]
impl LlmProvider for EndTurnOnlyProvider {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[octos_llm::ToolSpec],
        _config: &octos_llm::ChatConfig,
    ) -> Result<ChatResponse> {
        Ok(end_turn("done", 0, 0))
    }

    fn model_id(&self) -> &str {
        "mock"
    }

    fn provider_name(&self) -> &str {
        "mock"
    }
}

/// Build a slides workspace fixture, optionally fully-ready.
///
/// Pure-filesystem setup. Callers that want a "ready" deck must also
/// invoke [`run_managed_slides_workspace_validators`] (async) or
/// [`run_managed_slides_workspace_validators_sync`] (blocking) to
/// exercise the PRODUCTION project-root validator helper. Splitting
/// the helper this way avoids the "Cannot start a runtime from within a
/// runtime" panic when async tests call the fixture inside their own
/// Tokio runtime.
fn make_managed_slides_workspace(tmp_root: &std::path::Path, slug: &str, ready: bool) {
    use crate::workspace_git::WorkspaceProjectKind;
    use crate::workspace_policy::{WorkspacePolicy, write_workspace_policy};
    let repo_root = tmp_root.join("slides").join(slug);
    std::fs::create_dir_all(&repo_root).unwrap();
    write_workspace_policy(
        &repo_root,
        &WorkspacePolicy::for_kind(WorkspaceProjectKind::Slides),
    )
    .unwrap();
    // Every slides workspace requires script.js / memory.md / changelog.md
    // for turn_end + output/deck.pptx + slide png for completion.
    std::fs::write(repo_root.join("script.js"), "// slides").unwrap();
    std::fs::write(repo_root.join("memory.md"), "# memory").unwrap();
    std::fs::write(repo_root.join("changelog.md"), "# changelog").unwrap();
    if ready {
        std::fs::create_dir_all(repo_root.join("output/imgs")).unwrap();
        // octos #997: write real PPTX magic bytes so the project-scope
        // PPTX `MagicBytes` validator wired in
        // `WorkspacePolicy::for_kind(Slides)` does not fail the gate.
        let mut pptx = vec![0x50, 0x4B, 0x03, 0x04];
        pptx.extend_from_slice(&[0u8; 32]);
        std::fs::write(repo_root.join("output/deck.pptx"), &pptx).unwrap();
        std::fs::write(repo_root.join("output/imgs/slide-01.png"), "fake-png").unwrap();
        // NOTE: caller must invoke
        // `run_managed_slides_workspace_validators[_sync]` to write the
        // slides-kind PPTX MagicBytes Pass row.
    }
}

/// octos #997 (round-2 fix): async variant — exercise the production
/// project-root validator helper so the ready fixture writes a Pass row
/// into the same project ledger that the spawn loop writes to in
/// production. Pre-round-2 the fixture manually `ledger.append(...)`ed a
/// fake Pass; codex flagged that as masking the gap (the validator was
/// declared but never RUN at the project root in production).
async fn run_managed_slides_workspace_validators(tmp_root: &std::path::Path, slug: &str) {
    use crate::workspace_git::WorkspaceProjectKind;
    let registry = std::sync::Arc::new(crate::ToolRegistry::new());
    // Mirror production: the spawn loop hands the plugin's
    // `files_to_send` list through. The fixture stages the deck at
    // the legacy in-project path so the filter accepts it.
    let files_to_send = vec![tmp_root.join("slides").join(slug).join("output/deck.pptx")];
    let _ = crate::workspace_contract::run_project_root_validators(
        &registry,
        tmp_root,
        Some(WorkspaceProjectKind::Slides),
        &files_to_send,
        std::sync::Arc::new(crate::sandbox::NoSandbox),
    )
    .await;
}

#[tokio::test]
async fn run_task_demotes_success_when_contract_fails() {
    // End-to-end integration: an EndTurn that would otherwise be Success
    // gets demoted to success=false when the working_dir contains a
    // policy-managed repo that is not ready.
    let dir = tempfile::tempdir().unwrap();
    // Pre-populate a broken slides repo so contract != ready.
    make_managed_slides_workspace(dir.path(), "demo", false);

    let tools = ToolRegistry::with_builtins(dir.path());
    let provider: Arc<dyn LlmProvider> = Arc::new(EndTurnOnlyProvider);
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent = Agent::new(AgentId::new("contract-demote"), provider, tools, memory);
    let task = Task::new(
        TaskKind::Code {
            instruction: "Build it".into(),
            files: vec![],
        },
        TaskContext {
            working_dir: dir.path().to_path_buf(),
            ..Default::default()
        },
    );

    let result = agent.run_task(&task).await.unwrap();
    assert!(
        !result.success,
        "broken workspace contract must demote task to failure"
    );
    assert!(
        result.output.contains("workspace contract") || result.output.contains("slides/demo"),
        "result output should explain the contract failure: {:?}",
        result.output
    );
}

#[ignore = "Pre-migration test: the SpawnOnlyFiles-source MagicBytes validator \
                (post-#997 round-3) rejects no-files-emitted tasks at the project-scope \
                gate. This test's `EndTurnOnlyProvider` agent never calls a plugin tool, \
                so `files_to_send` stays empty and the loop_runner's project-scope \
                validator run after run_task fails the freshly-staged ready workspace. \
                Re-enable by giving the agent a stub plugin tool that returns the staged \
                deck in `tool_result.files_to_send`."]
#[tokio::test]
async fn run_task_keeps_success_when_contract_passes() {
    let dir = tempfile::tempdir().unwrap();
    // Fully-ready workspace.
    make_managed_slides_workspace(dir.path(), "ready", true);
    run_managed_slides_workspace_validators(dir.path(), "ready").await;

    let tools = ToolRegistry::with_builtins(dir.path());
    let provider: Arc<dyn LlmProvider> = Arc::new(EndTurnOnlyProvider);
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent = Agent::new(AgentId::new("contract-ok"), provider, tools, memory);
    let task = Task::new(
        TaskKind::Code {
            instruction: "All good".into(),
            files: vec![],
        },
        TaskContext {
            working_dir: dir.path().to_path_buf(),
            ..Default::default()
        },
    );

    let result = agent.run_task(&task).await.unwrap();
    assert!(
        result.success,
        "ready workspace must keep Success (got {:?})",
        result.output
    );
}

// ── Phase 4: human-approval suspend-and-resume (ROBRIX-PHASE4 ADR) ──────

// ─────────────────────────────────────────────────────────────────────
// Task 8 — FailFast LLM bail: no empty-response 2nd call, classify-once
// TurnFailure projection, hook-deny exclusion.
// ─────────────────────────────────────────────────────────────────────

fn task_for(instruction: &str, dir: &std::path::Path) -> Task {
    Task::new(
        TaskKind::Code {
            instruction: instruction.to_string(),
            files: vec![],
        },
        TaskContext {
            working_dir: dir.to_path_buf(),
            ..Default::default()
        },
    )
}

// --- Mid-turn steer injection (codex `TurnState.pending_input` parity) ---

// ─────────────────────────────────────────────────────────────────────────
// #27d (R4) — malformed tool-call feedback buffer
// ─────────────────────────────────────────────────────────────────────────

// --- #2174: conversation-loop recovery from a degenerate empty MaxTokens ---

// Native task loops must honor the same workspace compaction policy as chat.
fn configured_compaction_policy() -> crate::compaction::CompactionPolicy {
    crate::compaction::CompactionPolicy {
        token_budget: 6_000,
        preflight_threshold: Some(6_000),
        ..Default::default()
    }
}

fn configured_compaction_history(words_per_message: usize) -> Vec<Message> {
    let filler = "word ".repeat(words_per_message);
    (0..14)
        .flat_map(|i| {
            [
                Message::user(format!("old question {i}: {filler}")),
                Message::assistant(format!("old answer {i}: {filler}")),
            ]
        })
        .collect()
}

async fn configured_compaction_agent(
    dir: &std::path::Path,
    responses: Vec<ChatResponse>,
    policy: crate::compaction::CompactionPolicy,
) -> (Agent, RecordedRequests, Arc<AtomicUsize>) {
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let provider = Arc::new(RequestRecordingProvider::new(responses, requests.clone()));
    let memory = Arc::new(EpisodeStore::open(dir.join("memory")).await.unwrap());
    let calls = Arc::new(AtomicUsize::new(0));
    let runner =
        crate::compaction::CompactionRunner::new(policy).with_summarizer(CountingSummarizer {
            calls: calls.clone(),
        });
    let agent = Agent::new(
        AgentId::new("configured-compaction"),
        provider,
        ToolRegistry::with_builtins(dir),
        memory,
    )
    .with_system_prompt("Complete the task.".to_string())
    .with_config(AgentConfig {
        save_episodes: false,
        ..Default::default()
    })
    .with_compaction_runner(Arc::new(runner))
    .with_compaction_workspace(crate::workspace_policy::WorkspacePolicy::for_session())
    .with_file_state_cache(Arc::new(crate::file_state_cache::FileStateCache::new()));
    (agent, requests, calls)
}

fn configured_compaction_read(id: &str) -> ChatResponse {
    tool_use(
        vec![tool_call(
            id,
            "read_file",
            serde_json::json!({"path": "visible.txt"}),
        )],
        10,
        5,
    )
}

#[tokio::test]
async fn configured_compaction_task_runs_after_large_tool_result_without_provider_overflow() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("visible.txt"), "payload ".repeat(2_000)).unwrap();
    let policy = configured_compaction_policy();
    let (agent, requests, calls) = configured_compaction_agent(
        dir.path(),
        vec![
            configured_compaction_read("read_large"),
            end_turn("done", 10, 5),
        ],
        policy.clone(),
    )
    .await;
    let mut task = task_for("read visible.txt", dir.path());
    task.context.working_memory = configured_compaction_history(100);
    let initial = agent.build_initial_messages(&task).await;
    let initial_tokens: u32 = initial
        .iter()
        .map(octos_llm::context::estimate_message_tokens)
        .sum();
    assert!(
        initial_tokens < policy.token_budget,
        "preflight should have nothing to do"
    );

    assert!(agent.run_task(&task).await.unwrap().success);
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].0.len(), initial.len());
    let tool_result = requests[1]
        .0
        .iter()
        .find(|m| m.role == MessageRole::Tool)
        .unwrap();
    assert!(
        tool_result.content.contains("payload payload"),
        "the real tool result must survive"
    );
    let uncompacted_tokens =
        initial_tokens + octos_llm::context::estimate_message_tokens(tool_result);
    assert!(uncompacted_tokens > policy.token_budget);
    assert!(uncompacted_tokens < agent.llm.context_window() / 2);
    assert_eq!(
        calls.load(AtomicOrdering::SeqCst),
        1,
        "the second iteration must apply the workspace budget"
    );
    assert!(
        requests[1].0.len() < requests[0].0.len(),
        "old history must shrink before the next provider call"
    );
}

async fn configured_compaction_prime_read(agent: &Agent) -> Vec<Message> {
    let response = configured_compaction_read("first_read");
    let (results, ..) = agent.execute_tools(&response).await.unwrap();
    assert!(results[0].content.contains("visible file body"));
    assert!(!results[0].content.contains("[FILE_UNCHANGED]"));
    assert!(!agent.file_state_cache.as_ref().unwrap().is_empty());
    let mut assistant = Message::assistant("");
    assistant.tool_calls = Some(response.tool_calls);
    let mut history = vec![
        Message::system("Complete the task."),
        Message::user("read visible.txt"),
        assistant,
    ];
    history.extend(results);
    history.push(Message::assistant("read complete"));
    history
}

struct CompactionVisibilityManager {
    mode: &'static str,
}

impl PromptContextManager for CompactionVisibilityManager {
    fn prepare_prompt(
        &self,
        _request: PromptContextRequest,
        messages: &mut Vec<Message>,
    ) -> Result<PromptContextReport, String> {
        let before = messages.len();
        if self.mode.contains("prune") || self.mode == "replace" {
            for message in messages.iter_mut().filter(|m| m.role == MessageRole::Tool) {
                message.content = "[prior tool result compacted]".to_string();
            }
        } else if self.mode == "append" {
            messages.push(Message::user("canonical tail"));
        }
        if self.mode.contains("error") {
            return Err("test bridge error".to_string());
        }
        Ok(PromptContextReport {
            prompt_replaced: true,
            compaction_performed: self.mode.contains("prune") || self.mode == "reported_compaction",
            messages_before: before,
            messages_after: messages.len(),
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn configured_compaction_cache_tracks_actual_visibility_across_all_compaction_paths() {
    use crate::compaction::{CompactionPolicy, CompactionRunner};
    use crate::compaction_tiered::{
        ApiMicroCompactionConfig, MicroCompactionPolicy, Tier1Pass, TieredCompactionRunner,
    };

    for (mode, invalidated) in [
        ("turn", true),
        ("preflight_error", true),
        ("preflight_error_noop", false),
        ("tier1", true),
        ("tier1_noop", false),
        ("fallback", true),
        ("legacy_trim", true),
        ("old_outputs", true),
        ("old_outputs_noop", false),
        ("manager_prune", true),
        ("manager_error_prune", true),
        ("manager_replace", true),
        ("manager_reported_compaction", false),
        ("manager_noop", false),
        ("manager_error", false),
        ("manager_append", false),
    ] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("visible.txt"),
            format!("{}\nvisible file body", "body ".repeat(300)),
        )
        .unwrap();
        let (mut agent, _, _) = configured_compaction_agent(
            dir.path(),
            vec![],
            CompactionPolicy {
                token_budget: 100_000,
                preflight_threshold: Some(1),
                prune_tool_results_after_turns: Some(1),
                ..Default::default()
            },
        )
        .await;
        let mut messages = configured_compaction_prime_read(&agent).await;
        messages.push(Message::user("next turn"));
        messages.push(Message::assistant("recent reply"));
        match mode {
            "turn" => assert!(
                agent
                    .maybe_run_turn_compaction(&mut messages, 2)
                    .unwrap()
                    .is_none()
            ),
            "preflight_error" | "preflight_error_noop" => {
                agent = agent.with_compaction_runner(Arc::new(CompactionRunner::new(
                    CompactionPolicy {
                        token_budget: 100_000,
                        preflight_threshold: Some(1),
                        prune_tool_results_after_turns: (mode == "preflight_error").then_some(1),
                        preserved_invariants: vec!["missing invariant".to_string()],
                        ..Default::default()
                    },
                )));
                assert!(agent.maybe_run_preflight_compaction(&mut messages).is_err());
            }
            "tier1" | "tier1_noop" => {
                agent = agent.with_tiered_compaction(Arc::new(TieredCompactionRunner::new(
                    MicroCompactionPolicy {
                        max_age_turns: 0,
                        max_size_bytes_per_result: if invalidated { 1 } else { u32::MAX },
                        pin_recent_files: 0,
                        dedup_duplicate_reads: false,
                    },
                    ApiMicroCompactionConfig::default(),
                    Box::new(CompactionRunner::new(CompactionPolicy::default())),
                )));
                assert_eq!(
                    agent
                        .run_tier1_compaction(&mut messages, &[], Tier1Pass::Full)
                        .performed(),
                    invalidated
                );
            }
            "fallback" => assert!(agent.fallback_truncate(&mut messages, 40)),
            "legacy_trim" => {
                let filler = "word ".repeat((agent.llm.context_window() / 80) as usize);
                for _ in 0..80 {
                    messages.push(Message::assistant(filler.clone()));
                }
                assert!(agent.trim_to_context_window(&mut messages));
            }
            "old_outputs" | "old_outputs_noop" => {
                if !invalidated {
                    // A repeated truncation reports a repair but leaves identical text.
                    super::super::message_repair::truncate_old_tool_results(&mut messages);
                }
                let mut turn = LoopTurnState::new(Instant::now());
                prepare_conversation_messages(&agent, &mut messages, &mut turn);
            }
            _ => {
                agent = agent.with_prompt_context_manager(Arc::new(CompactionVisibilityManager {
                    mode: mode.strip_prefix("manager_").unwrap(),
                }));
                agent.prepare_prompt_with_context_manager(
                    &mut messages,
                    PromptContextPhase::Iteration,
                    2,
                );
            }
        }
        let (reread, ..) = agent
            .execute_tools(&configured_compaction_read("fresh_read"))
            .await
            .unwrap();
        assert_eq!(
            reread[0].content.contains("[FILE_UNCHANGED]"),
            !invalidated,
            "{mode}: {}",
            reread[0].content
        );
        if invalidated {
            assert!(reread[0].content.contains("visible file body"), "{mode}");
        }
    }
}

fn tool_call(id: &str, name: &str, arguments: serde_json::Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments,
        metadata: None,
    }
}

fn empty_call(id: &str, name: &str) -> ToolCall {
    tool_call(id, name, serde_json::json!({}))
}

fn dispatched_call(id: &str, name: &str, arguments: serde_json::Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: "run_tool".into(),
        arguments: serde_json::json!({"name":name,"arguments":arguments}),
        metadata: None,
    }
}

#[tokio::test]
async fn fixed_dispatcher_keeps_terminal_native_failure_across_entry_points() {
    let dir = tempfile::tempdir().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRegistry::with_builtins(dir.path());
    tools.register(TerminalFailureTool {
        calls: calls.clone(),
    });

    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_use(
            vec![dispatched_call(
                "first",
                "lesson_generate",
                serde_json::json!({}),
            )],
            1,
            1,
        ),
        tool_use(
            vec![ToolCall {
                id: "again".into(),
                name: "lesson_generate".into(),
                arguments: serde_json::json!({"changed":true}),
                metadata: None,
            }],
            1,
            1,
        ),
    ]));
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent = Agent::new(AgentId::new("dispatch-terminal"), provider, tools, memory);
    let result = agent.process_message("work", &[], vec![]).await.unwrap();
    assert_eq!(calls.load(AtomicOrdering::SeqCst), 1);
    assert_eq!(
        result.content,
        terminal_tool_retry_message("lesson_generate")
    );
}

fn continuation_fixture(root: &std::path::Path, owner: &str) {
    std::fs::create_dir_all(root.join(".octos")).unwrap();
    std::fs::write(
        root.join(".octos/continuation.json"),
        serde_json::to_vec(&serde_json::json!({
            "schema_version":1,"task_id":"fixture","owner_agent":owner,"evidence_file":"evidence.json",
            "stagnant_attempts":3,"deliverables":["candidate.bin"],"required_checks":[{"id":"layout","epsilon":0.0}]
        }))
        .unwrap(),
    )
    .unwrap();
    continuation_report(root, 1, false);
}
fn continuation_report(root: &std::path::Path, attempt: usize, passed: bool) {
    use sha2::{Digest, Sha256};
    let bytes = format!("actually changed artifact {attempt}");
    std::fs::write(root.join("candidate.bin"), &bytes).unwrap();
    std::fs::write(root.join("evidence.json"),serde_json::to_vec(&serde_json::json!({
        "schema_version":1,"task_id":"fixture","attempt":attempt,"phase":"checked",
        "artifacts":[{"path":"candidate.bin","sha256":format!("{:x}",Sha256::digest(bytes.as_bytes()))}],
        "checks":[{"id":"layout","status":if passed {"pass"} else {"fail"},"remaining":if passed {0} else {8}}]
    })).unwrap()).unwrap();
}

#[tokio::test]
async fn continuation_evidence_chat_uses_stable_session_across_recreated_agents() {
    let dir = tempfile::tempdir().unwrap();
    continuation_fixture(dir.path(), "session:profile:local:tui#peer-producer");
    for (id, session, should_fail, expected_requests) in [
        (
            "ui-protocol-turn-1",
            "profile:local:tui#peer-producer",
            true,
            2,
        ),
        (
            "ui-protocol-turn-2",
            "profile:local:tui#peer-producer",
            true,
            1,
        ),
        (
            "ui-protocol-coordinator",
            "profile:local:tui#coordinator",
            false,
            1,
        ),
    ] {
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let provider = Arc::new(RequestRecordingProvider::new(
            vec![end_turn("done", 1, 1), end_turn("done again", 1, 1)],
            requests.clone(),
        ));
        let mut tools = ToolRegistry::new();
        tools.set_workspace_root(dir.path().into());
        let memory = Arc::new(EpisodeStore::open(dir.path().join(id)).await.unwrap());
        let agent = Agent::new(AgentId::new(id), provider, tools, memory)
            .with_parent_session_key(session)
            .with_config(AgentConfig {
                save_episodes: false,
                ..Default::default()
            });
        let result = agent.process_message("finish", &[], vec![]).await;
        assert_eq!(result.is_err(), should_fail, "{id}");
        if let Err(error) = result {
            assert!(error.to_string().contains("needs_replan"));
        }
        assert_eq!(requests.lock().unwrap().len(), expected_requests, "{id}");
    }
}
