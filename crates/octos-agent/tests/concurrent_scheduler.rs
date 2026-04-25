//! M8.8 concurrent-safe vs exclusive scheduler integration tests.
//!
//! Drives the `Agent` loop through a mock LLM with scripted tool-call batches
//! and observes the executor's admission decisions via probe tools that
//! record start/end timestamps on a shared log. The tests assert on two
//! invariants:
//!
//! 1. A batch composed entirely of [`ConcurrencyClass::Safe`] tools runs in
//!    parallel (the last start precedes the first end — i.e. windows
//!    overlap).
//! 2. A batch that contains *any* [`ConcurrencyClass::Exclusive`] tool runs
//!    each call serially in LLM call order (every start strictly follows the
//!    previous end).
//!
//! A third test triggers an error cascade: the first Exclusive call fails
//! and the remaining peers receive a synthetic "cancelled" tool-result
//! message without being dispatched.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use octos_agent::{Agent, AgentConfig, ConcurrencyClass, Tool, ToolRegistry, ToolResult};
use octos_core::{AgentId, Message, ToolCall};
use octos_llm::{ChatConfig, ChatResponse, LlmProvider, StopReason, TokenUsage, ToolSpec};
use octos_memory::EpisodeStore;
use tempfile::TempDir;

/// Per-call timing record captured by [`ProbeTool`].
#[derive(Clone, Debug)]
struct CallSpan {
    name: String,
    /// Monotonic microseconds from [`std::time::Instant`] against a shared
    /// epoch — used to reason about overlap without making assertions about
    /// wall-clock time.
    started_us: u128,
    ended_us: u128,
}

#[derive(Default)]
struct CallLog {
    epoch: Option<std::time::Instant>,
    spans: Vec<CallSpan>,
}

impl CallLog {
    fn new() -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self::default()))
    }

    fn epoch(&mut self) -> std::time::Instant {
        *self.epoch.get_or_insert_with(std::time::Instant::now)
    }

    fn push(&mut self, span: CallSpan) {
        self.spans.push(span);
    }

    fn snapshot(&self) -> Vec<CallSpan> {
        self.spans.clone()
    }
}

/// Instrumented tool that sleeps for a fixed interval to expose the
/// executor's dispatch strategy (overlap vs serialisation).
struct ProbeTool {
    name: &'static str,
    class: ConcurrencyClass,
    /// Duration each call sleeps before returning.
    sleep: Duration,
    /// Whether this call should return a failed [`ToolResult`].
    should_fail: bool,
    log: Arc<Mutex<CallLog>>,
}

impl ProbeTool {
    fn new(
        name: &'static str,
        class: ConcurrencyClass,
        sleep: Duration,
        log: Arc<Mutex<CallLog>>,
    ) -> Self {
        Self {
            name,
            class,
            sleep,
            should_fail: false,
            log,
        }
    }

    fn with_failure(mut self, fail: bool) -> Self {
        self.should_fail = fail;
        self
    }
}

#[async_trait]
impl Tool for ProbeTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "probe"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn concurrency_class(&self) -> ConcurrencyClass {
        self.class
    }
    async fn execute(&self, _args: &serde_json::Value) -> eyre::Result<ToolResult> {
        let started_us = {
            let mut log = self.log.lock().unwrap();
            let epoch = log.epoch();
            epoch.elapsed().as_micros()
        };

        tokio::time::sleep(self.sleep).await;

        let ended_us = {
            let log = self.log.lock().unwrap();
            log.epoch.expect("epoch set at start").elapsed().as_micros()
        };

        {
            let mut log = self.log.lock().unwrap();
            log.push(CallSpan {
                name: self.name.to_string(),
                started_us,
                ended_us,
            });
        }

        if self.should_fail {
            Ok(ToolResult {
                output: format!("{} deliberate failure", self.name),
                success: false,
                ..Default::default()
            })
        } else {
            Ok(ToolResult {
                output: format!("{} ok", self.name),
                success: true,
                ..Default::default()
            })
        }
    }
}

/// Mock LLM provider that returns scripted responses in FIFO order.
struct MockLlm {
    responses: Mutex<Vec<ChatResponse>>,
}

impl MockLlm {
    fn new(responses: Vec<ChatResponse>) -> Self {
        Self {
            responses: Mutex::new(responses),
        }
    }
}

#[async_trait]
impl LlmProvider for MockLlm {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[ToolSpec],
        _config: &ChatConfig,
    ) -> eyre::Result<ChatResponse> {
        let mut responses = self.responses.lock().unwrap();
        if responses.is_empty() {
            eyre::bail!("MockLlm: no more scripted responses");
        }
        Ok(responses.remove(0))
    }

    fn context_window(&self) -> u32 {
        128_000
    }
    fn model_id(&self) -> &str {
        "mock-m88"
    }
    fn provider_name(&self) -> &str {
        "mock"
    }
}

fn tool_use_response(calls: Vec<ToolCall>) -> ChatResponse {
    ChatResponse {
        content: None,
        reasoning_content: None,
        tool_calls: calls,
        stop_reason: StopReason::ToolUse,
        usage: TokenUsage {
            input_tokens: 100,
            output_tokens: 10,
            ..Default::default()
        },
        provider_index: None,
    }
}

fn end_turn(text: &str) -> ChatResponse {
    ChatResponse {
        content: Some(text.to_string()),
        reasoning_content: None,
        tool_calls: vec![],
        stop_reason: StopReason::EndTurn,
        usage: TokenUsage {
            input_tokens: 50,
            output_tokens: 20,
            ..Default::default()
        },
        provider_index: None,
    }
}

fn tc(id: &str, name: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: serde_json::json!({}),
        metadata: None,
    }
}

/// Build an agent configured with the supplied probe tools. The registry
/// keeps only the probe tools (built-ins would clutter the call log).
async fn make_agent(probes: Vec<ProbeTool>, responses: Vec<ChatResponse>, dir: &TempDir) -> Agent {
    let llm: Arc<dyn LlmProvider> = Arc::new(MockLlm::new(responses));
    let mut tools = ToolRegistry::new();
    for probe in probes {
        tools.register(probe);
    }
    let memory = Arc::new(EpisodeStore::open(dir.path().join(".octos")).await.unwrap());
    Agent::new(AgentId::new("m88-test"), llm, tools, memory).with_config(AgentConfig {
        save_episodes: false,
        ..Default::default()
    })
}

#[tokio::test]
async fn executor_dispatches_all_safe_in_parallel() {
    // Three Safe probes each sleep 200ms. If dispatch is parallel the entire
    // batch completes in ~200ms (overlapping windows). If it serialises
    // accidentally the total would be ~600ms. We assert overlap directly via
    // timestamps rather than wall-clock so the test is flake-resistant.
    let dir = TempDir::new().unwrap();
    let log = CallLog::new();
    let probes = vec![
        ProbeTool::new(
            "safe_a",
            ConcurrencyClass::Safe,
            Duration::from_millis(200),
            log.clone(),
        ),
        ProbeTool::new(
            "safe_b",
            ConcurrencyClass::Safe,
            Duration::from_millis(200),
            log.clone(),
        ),
        ProbeTool::new(
            "safe_c",
            ConcurrencyClass::Safe,
            Duration::from_millis(200),
            log.clone(),
        ),
    ];
    let responses = vec![
        tool_use_response(vec![
            tc("call_1", "safe_a"),
            tc("call_2", "safe_b"),
            tc("call_3", "safe_c"),
        ]),
        end_turn("done"),
    ];
    let agent = make_agent(probes, responses, &dir).await;

    let resp = agent
        .process_message("fan out reads", &[], vec![])
        .await
        .expect("agent loop must succeed");
    assert_eq!(resp.content, "done");

    let spans = log.lock().unwrap().snapshot();
    assert_eq!(spans.len(), 3);

    // At least two spans must overlap — the max start time must precede the
    // min end time. This is equivalent to "the windows intersect".
    let max_start = spans.iter().map(|s| s.started_us).max().unwrap();
    let min_end = spans.iter().map(|s| s.ended_us).min().unwrap();
    assert!(
        max_start < min_end,
        "safe batch did not overlap: max_start={}us min_end={}us spans={:?}",
        max_start,
        min_end,
        spans
    );
}

#[tokio::test]
async fn executor_dispatches_exclusive_serially_in_call_order() {
    // Three Exclusive probes must run one at a time in the order the LLM
    // emitted them. We verify that every span starts *at or after* its
    // predecessor's end and that the LLM call order is preserved.
    let dir = TempDir::new().unwrap();
    let log = CallLog::new();
    let probes = vec![
        ProbeTool::new(
            "excl_a",
            ConcurrencyClass::Exclusive,
            Duration::from_millis(50),
            log.clone(),
        ),
        ProbeTool::new(
            "excl_b",
            ConcurrencyClass::Exclusive,
            Duration::from_millis(50),
            log.clone(),
        ),
        ProbeTool::new(
            "excl_c",
            ConcurrencyClass::Exclusive,
            Duration::from_millis(50),
            log.clone(),
        ),
    ];
    let responses = vec![
        tool_use_response(vec![
            tc("call_1", "excl_a"),
            tc("call_2", "excl_b"),
            tc("call_3", "excl_c"),
        ]),
        end_turn("serialized"),
    ];
    let agent = make_agent(probes, responses, &dir).await;

    let resp = agent
        .process_message("serial mutations", &[], vec![])
        .await
        .expect("agent loop must succeed");
    assert_eq!(resp.content, "serialized");

    let spans = log.lock().unwrap().snapshot();
    assert_eq!(spans.len(), 3);

    // Order-preserved: the log is appended in finish order, which equals
    // start order under serialisation.
    assert_eq!(
        spans.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
        vec!["excl_a", "excl_b", "excl_c"]
    );
    // Each subsequent span must start no earlier than the prior one ended.
    for pair in spans.windows(2) {
        assert!(
            pair[1].started_us >= pair[0].ended_us,
            "exclusive batch was not serialized: {:?}",
            spans
        );
    }
}

#[tokio::test]
async fn executor_serializes_mixed_batch_when_any_exclusive() {
    // One Exclusive + two Safe must serialize the WHOLE batch per the M8.8
    // admission rule ("if any Exclusive, run the whole batch serially").
    let dir = TempDir::new().unwrap();
    let log = CallLog::new();
    let probes = vec![
        ProbeTool::new(
            "excl_x",
            ConcurrencyClass::Exclusive,
            Duration::from_millis(50),
            log.clone(),
        ),
        ProbeTool::new(
            "safe_y",
            ConcurrencyClass::Safe,
            Duration::from_millis(50),
            log.clone(),
        ),
        ProbeTool::new(
            "safe_z",
            ConcurrencyClass::Safe,
            Duration::from_millis(50),
            log.clone(),
        ),
    ];
    let responses = vec![
        tool_use_response(vec![
            tc("call_1", "excl_x"),
            tc("call_2", "safe_y"),
            tc("call_3", "safe_z"),
        ]),
        end_turn("mixed-ok"),
    ];
    let agent = make_agent(probes, responses, &dir).await;

    let resp = agent
        .process_message("mixed batch", &[], vec![])
        .await
        .expect("agent loop must succeed");
    assert_eq!(resp.content, "mixed-ok");

    let spans = log.lock().unwrap().snapshot();
    assert_eq!(spans.len(), 3);

    // No two spans may overlap; each start >= previous end.
    for pair in spans.windows(2) {
        assert!(
            pair[1].started_us >= pair[0].ended_us,
            "mixed batch was not serialized: {:?}",
            spans
        );
    }
}

#[tokio::test]
async fn executor_cancels_siblings_after_exclusive_tool_error() {
    // First Exclusive tool fails; the remaining two must not execute. Each
    // cancelled peer gets a synthetic "cancelled" tool-result message so the
    // LLM sees every tool_call_id. The spans log records exactly ONE probe
    // invocation (the failing first call).
    let dir = TempDir::new().unwrap();
    let log = CallLog::new();
    let probes = vec![
        ProbeTool::new(
            "excl_bad",
            ConcurrencyClass::Exclusive,
            Duration::from_millis(20),
            log.clone(),
        )
        .with_failure(true),
        ProbeTool::new(
            "safe_peer1",
            ConcurrencyClass::Safe,
            Duration::from_millis(20),
            log.clone(),
        ),
        ProbeTool::new(
            "safe_peer2",
            ConcurrencyClass::Safe,
            Duration::from_millis(20),
            log.clone(),
        ),
    ];
    let responses = vec![
        tool_use_response(vec![
            tc("call_1", "excl_bad"),
            tc("call_2", "safe_peer1"),
            tc("call_3", "safe_peer2"),
        ]),
        end_turn("post-cascade"),
    ];
    let agent = make_agent(probes, responses, &dir).await;

    let resp = agent
        .process_message("cascade test", &[], vec![])
        .await
        .expect("agent loop must succeed (cancellation is not an error)");
    assert_eq!(resp.content, "post-cascade");

    // Only the failing probe actually ran.
    let spans = log.lock().unwrap().snapshot();
    assert_eq!(spans.len(), 1, "only the failing probe should execute");
    assert_eq!(spans[0].name, "excl_bad");
}

#[tokio::test]
async fn executor_preserves_tool_call_ids_across_cascade() {
    // Every tool_call_id issued by the LLM must appear in the result
    // messages, whether the call executed or was cancelled. This is what
    // the LLM relies on to correlate responses with its outstanding calls.
    use octos_agent::ProgressEvent;

    // Collect tool-completion events to verify each call_id roundtrips.
    #[derive(Default)]
    struct Collector {
        events: Mutex<Vec<(String, String, bool)>>, // (name, tool_id, success)
    }
    impl octos_agent::ProgressReporter for Collector {
        fn report(&self, event: ProgressEvent) {
            if let ProgressEvent::ToolCompleted {
                name,
                tool_id,
                success,
                ..
            } = event
            {
                self.events.lock().unwrap().push((name, tool_id, success));
            }
        }
    }

    let dir = TempDir::new().unwrap();
    let log = CallLog::new();
    let probes = vec![
        ProbeTool::new(
            "excl_fail",
            ConcurrencyClass::Exclusive,
            Duration::from_millis(10),
            log.clone(),
        )
        .with_failure(true),
        ProbeTool::new(
            "safe_late_a",
            ConcurrencyClass::Safe,
            Duration::from_millis(10),
            log.clone(),
        ),
        ProbeTool::new(
            "safe_late_b",
            ConcurrencyClass::Safe,
            Duration::from_millis(10),
            log.clone(),
        ),
    ];
    let responses = vec![
        tool_use_response(vec![
            tc("call_alpha", "excl_fail"),
            tc("call_beta", "safe_late_a"),
            tc("call_gamma", "safe_late_b"),
        ]),
        end_turn("ok"),
    ];
    let reporter = Arc::new(Collector::default());
    let agent = make_agent(probes, responses, &dir)
        .await
        .with_reporter(reporter.clone());

    let resp = agent
        .process_message("id roundtrip", &[], vec![])
        .await
        .expect("agent loop must succeed");

    // 1) The reporter saw the failing probe's ToolCompleted event with the
    //    original call_id preserved.
    let events = reporter.events.lock().unwrap().clone();
    assert!(
        events
            .iter()
            .any(|(name, id, _)| name == "excl_fail" && id == "call_alpha"),
        "expected a ToolCompleted for the failing call_alpha; saw {:?}",
        events
    );

    // 2) Every LLM-issued tool_call_id must appear exactly once among the
    //    tool-result messages — whether the call executed or was cancelled.
    //    This is the invariant the LLM relies on to close its outstanding
    //    tool_uses: missing an id would leave the conversation in an
    //    unresolvable state on the next turn.
    let tool_msgs: Vec<&Message> = resp
        .messages
        .iter()
        .filter(|m| m.role == octos_core::MessageRole::Tool)
        .collect();
    let ids: Vec<&str> = tool_msgs
        .iter()
        .filter_map(|m| m.tool_call_id.as_deref())
        .collect();
    assert!(
        ids.contains(&"call_alpha"),
        "missing call_alpha in {:?}",
        ids
    );
    assert!(
        ids.contains(&"call_beta"),
        "missing cancelled call_beta in {:?}",
        ids
    );
    assert!(
        ids.contains(&"call_gamma"),
        "missing cancelled call_gamma in {:?}",
        ids
    );

    // 3) The two cancelled peers must carry a "cancelled" marker in their
    //    content so the LLM can distinguish them from an ordinary failure.
    let find = |id: &str| -> &str {
        tool_msgs
            .iter()
            .find(|m| m.tool_call_id.as_deref() == Some(id))
            .expect("result for id")
            .content
            .as_str()
    };
    assert!(
        find("call_beta").contains("cancelled"),
        "call_beta result should mark cancellation; got {}",
        find("call_beta")
    );
    assert!(
        find("call_gamma").contains("cancelled"),
        "call_gamma result should mark cancellation; got {}",
        find("call_gamma")
    );
}
