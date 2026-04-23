//! Integration tests for the LLM-iterative session summarizer (harness M6.4).
//!
//! Covers the typed [`SessionSummary`] round-trip, the iterative refinement
//! contract (prior decisions retained or explicitly marked stale), the
//! 3-strike fallback to the extractive summarizer, and the
//! [`SESSION_SUMMARY_SCHEMA_VERSION`] compatibility behaviours per
//! `docs/OCTOS_HARNESS_ABI_VERSIONING.md`.
//!
//! Run with `cargo test -p octos-agent --test session_summary`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use octos_agent::summarizer::{
    DEFAULT_LLM_SUMMARIZER_FAILURE_THRESHOLD, LlmIterativeSummarizer, Summarizer,
};
use octos_core::{
    DecisionRecord, FileRecord, Message, MessageRole, SESSION_SUMMARY_SCHEMA_VERSION,
    STALE_DECISION_PREFIX, SessionSummary,
};
use octos_llm::{ChatConfig, ChatResponse, LlmProvider, StopReason, TokenUsage, ToolSpec};

fn user(content: &str) -> Message {
    Message {
        role: MessageRole::User,
        content: content.to_string(),
        media: vec![],
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: None,
        timestamp: chrono::Utc::now(),
    }
}

fn assistant(content: &str) -> Message {
    Message {
        role: MessageRole::Assistant,
        content: content.to_string(),
        media: vec![],
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: None,
        timestamp: chrono::Utc::now(),
    }
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

fn load_fixture(name: &str) -> String {
    let path = fixtures_dir().join(name);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("read fixture {}: {err}", path.display()))
}

/// Scripted LLM provider whose `chat()` returns the next queued response and
/// fails once the queue is empty. Supports `MockOutcome::Fail` entries so we
/// can exercise the 3-strike fallback path deterministically.
struct ScriptedLlm {
    queue: Mutex<Vec<MockOutcome>>,
    calls: Mutex<u32>,
}

enum MockOutcome {
    /// Return the provided JSON string as `content`.
    Json(String),
    /// Return an empty content, tripping the "no content" branch.
    EmptyContent,
    /// Return invalid JSON, tripping the parse-error branch.
    Garbage(String),
    /// Return JSON with a schema_version above the current max.
    FutureSchemaVersion,
}

impl ScriptedLlm {
    fn new(outcomes: Vec<MockOutcome>) -> Self {
        Self {
            queue: Mutex::new(outcomes),
            calls: Mutex::new(0),
        }
    }

    fn call_count(&self) -> u32 {
        *self.calls.lock().unwrap()
    }
}

#[async_trait]
impl LlmProvider for ScriptedLlm {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[ToolSpec],
        _config: &ChatConfig,
    ) -> eyre::Result<ChatResponse> {
        {
            let mut calls = self.calls.lock().unwrap();
            *calls += 1;
        }
        let next = {
            let mut q = self.queue.lock().unwrap();
            if q.is_empty() {
                return Err(eyre::eyre!("ScriptedLlm: queue exhausted"));
            }
            q.remove(0)
        };
        let content = match next {
            MockOutcome::Json(text) => Some(text),
            MockOutcome::EmptyContent => None,
            MockOutcome::Garbage(text) => Some(text),
            MockOutcome::FutureSchemaVersion => Some(
                serde_json::json!({
                    "schema_version": SESSION_SUMMARY_SCHEMA_VERSION + 42,
                    "goal": "from tomorrow",
                    "constraints": [],
                    "progress_done": [],
                    "progress_in_progress": [],
                    "decisions": [],
                    "files": [],
                    "next_steps": []
                })
                .to_string(),
            ),
        };
        Ok(ChatResponse {
            content,
            reasoning_content: None,
            tool_calls: vec![],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
            provider_index: None,
        })
    }

    fn context_window(&self) -> u32 {
        128_000
    }

    fn model_id(&self) -> &str {
        "mock-iterative-summarizer"
    }

    fn provider_name(&self) -> &str {
        "mock"
    }
}

fn json_summary(summary: &SessionSummary) -> MockOutcome {
    MockOutcome::Json(serde_json::to_string(summary).unwrap())
}

fn base_summary() -> SessionSummary {
    SessionSummary {
        schema_version: SESSION_SUMMARY_SCHEMA_VERSION,
        goal: "land iterative summarizer".to_string(),
        constraints: vec!["no unsafe".to_string()],
        progress_done: vec!["wired trait".to_string()],
        progress_in_progress: vec!["write tests".to_string()],
        decisions: vec![DecisionRecord {
            at_turn: 1,
            summary: "Use typed SessionSummary payload".to_string(),
            rationale: Some("round-trip guarantee".to_string()),
        }],
        files: vec![FileRecord {
            path: "crates/octos-agent/src/summarizer.rs".to_string(),
            role: "impl".to_string(),
        }],
        next_steps: vec!["ship PR".to_string()],
    }
}

#[tokio::test]
async fn should_produce_typed_session_summary_from_llm_response() {
    let scripted = Arc::new(ScriptedLlm::new(vec![json_summary(&base_summary())]));
    let summarizer = LlmIterativeSummarizer::new(scripted.clone());

    let messages = vec![
        user("Plan the summarizer implementation."),
        assistant("Draft typed schema and wire tests."),
    ];
    let encoded = summarizer
        .summarize(&messages, 2_000)
        .expect("summarize succeeds");

    assert_eq!(scripted.call_count(), 1, "LLM should have been called once");
    assert_eq!(summarizer.kind(), "llm_iterative");
    assert!(
        encoded.contains("Conversation Summary (iterative, v1)"),
        "encoded summary must declare schema version v1",
    );
    assert!(
        encoded.contains("Goal: land iterative summarizer"),
        "encoded summary must surface the goal",
    );
    assert!(
        encoded.contains("session_summary_v1: "),
        "encoded summary must carry the inline JSON payload",
    );

    let latest = summarizer
        .latest_summary()
        .expect("latest_summary populated after first pass");
    assert_eq!(latest.schema_version, SESSION_SUMMARY_SCHEMA_VERSION);
    assert_eq!(latest.goal, "land iterative summarizer");
    assert_eq!(latest.decisions.len(), 1);
    assert_eq!(latest.decisions[0].at_turn, 1);
}

#[tokio::test]
async fn should_preserve_prior_decision_through_iterative_refinement() {
    // First pass produces a decision. Second pass returns a summary where
    // the LLM "forgot" that decision; the summarizer must retain it (marked
    // stale), never silently drop it.
    let first = base_summary();
    let second = SessionSummary {
        schema_version: SESSION_SUMMARY_SCHEMA_VERSION,
        goal: "land iterative summarizer".to_string(),
        constraints: vec![],
        progress_done: vec!["wired trait".to_string(), "added tests".to_string()],
        progress_in_progress: vec![],
        decisions: vec![], // LLM omitted the prior decision.
        files: vec![],
        next_steps: vec!["merge PR".to_string()],
    };

    let scripted = Arc::new(ScriptedLlm::new(vec![
        json_summary(&first),
        json_summary(&second),
    ]));
    let summarizer = LlmIterativeSummarizer::new(scripted.clone());

    summarizer
        .summarize(&[user("first turn")], 2_000)
        .expect("first pass succeeds");
    summarizer
        .summarize(&[user("second turn")], 2_000)
        .expect("second pass succeeds");

    let latest = summarizer
        .latest_summary()
        .expect("latest_summary after refinement");
    // Prior decision must still be present — marked stale by the merge.
    let retained = latest
        .decisions
        .iter()
        .find(|d| d.at_turn == 1)
        .expect("prior decision preserved after refinement");
    assert!(
        retained.summary.starts_with(STALE_DECISION_PREFIX),
        "retained prior decision must be marked stale, not silently dropped; got {:?}",
        retained.summary,
    );
    assert_eq!(
        retained.rationale.as_deref(),
        Some("round-trip guarantee"),
        "rationale carried through from the prior summary",
    );
    // Constraints and files from the prior pass must survive too.
    assert!(
        latest.constraints.iter().any(|c| c == "no unsafe"),
        "prior constraint retained",
    );
    assert!(
        latest
            .files
            .iter()
            .any(|f| f.path == "crates/octos-agent/src/summarizer.rs"),
        "prior file retained",
    );
}

#[tokio::test]
async fn should_mark_stale_decision_explicitly() {
    // When the LLM itself emits a stale marker for an existing decision,
    // the retained decision must carry that explicit marker (no silent
    // mutation).
    let first = base_summary();
    let stale_decision = DecisionRecord {
        at_turn: 1,
        summary: format!("{STALE_DECISION_PREFIX} Use typed SessionSummary payload"),
        rationale: Some("superseded by bespoke DSL".to_string()),
    };
    let second = SessionSummary {
        decisions: vec![stale_decision.clone()],
        ..first.clone()
    };

    let scripted = Arc::new(ScriptedLlm::new(vec![
        json_summary(&first),
        json_summary(&second),
    ]));
    let summarizer = LlmIterativeSummarizer::new(scripted);

    summarizer
        .summarize(&[user("t1")], 2_000)
        .expect("first pass ok");
    summarizer
        .summarize(&[user("t2")], 2_000)
        .expect("second pass ok");

    let latest = summarizer.latest_summary().expect("latest");
    let decisions: Vec<_> = latest.decisions.iter().filter(|d| d.at_turn == 1).collect();
    assert_eq!(
        decisions.len(),
        1,
        "stale + original must dedupe to one entry (identity = at_turn + body)",
    );
    assert_eq!(decisions[0].summary, stale_decision.summary);
    assert_eq!(
        decisions[0].rationale.as_deref(),
        Some("superseded by bespoke DSL")
    );
}

#[tokio::test]
async fn should_fall_back_to_extractive_after_three_llm_failures() {
    // Four failures: three malformed LLM responses in a row should latch the
    // summarizer into the extractive fallback. The fourth call never
    // reaches the LLM — the summarizer delegates directly to the extractive
    // path.
    let scripted = Arc::new(ScriptedLlm::new(vec![
        MockOutcome::EmptyContent,
        MockOutcome::Garbage("not valid JSON".to_string()),
        MockOutcome::FutureSchemaVersion,
    ]));
    let summarizer = LlmIterativeSummarizer::new(scripted.clone())
        .with_failure_threshold(DEFAULT_LLM_SUMMARIZER_FAILURE_THRESHOLD);

    let messages = vec![user("hello")];
    for attempt in 1..=DEFAULT_LLM_SUMMARIZER_FAILURE_THRESHOLD {
        let out = summarizer
            .summarize(&messages, 2_000)
            .expect("fallback keeps the pipeline running");
        assert!(
            out.contains("Conversation Summary"),
            "extractive fallback must still produce a summary (attempt {attempt})",
        );
    }
    assert!(
        summarizer.is_in_fallback(),
        "three consecutive failures should latch the extractive fallback",
    );
    assert_eq!(
        scripted.call_count(),
        DEFAULT_LLM_SUMMARIZER_FAILURE_THRESHOLD,
        "LLM must be called once per failure until the threshold latches",
    );

    // Subsequent invocations must NOT reach the LLM.
    let _ = summarizer
        .summarize(&messages, 2_000)
        .expect("post-latch summarize succeeds via extractive path");
    assert_eq!(
        scripted.call_count(),
        DEFAULT_LLM_SUMMARIZER_FAILURE_THRESHOLD,
        "LLM must not be called again after the fallback latches",
    );
}

#[test]
fn should_default_missing_schema_version_to_v1() {
    // Fixture is a pre-M6.4 SessionSummary JSON with no `schema_version`
    // line. It must deserialize cleanly and pin to v1 so older persisted
    // summaries replay across harness upgrades.
    let raw = load_fixture("session_summary_legacy.json");
    let parsed: SessionSummary = serde_json::from_str(&raw).expect("legacy fixture parses");
    assert_eq!(
        parsed.schema_version, SESSION_SUMMARY_SCHEMA_VERSION,
        "missing schema_version must default to v1",
    );
    parsed
        .validate_schema_version()
        .expect("legacy version is supported");
    assert_eq!(parsed.decisions.len(), 1);
    assert_eq!(parsed.decisions[0].at_turn, 0);
}

#[test]
fn should_reject_future_schema_version_with_actionable_error() {
    // Craft a payload advertising a future version. The typed error must
    // name the type and advise upgrading — no panic.
    let raw = serde_json::json!({
        "schema_version": SESSION_SUMMARY_SCHEMA_VERSION + 12,
        "goal": "future",
        "constraints": [],
        "progress_done": [],
        "progress_in_progress": [],
        "decisions": [],
        "files": [],
        "next_steps": []
    })
    .to_string();
    let parsed: SessionSummary = serde_json::from_str(&raw).expect("JSON parses");
    let err = parsed
        .validate_schema_version()
        .expect_err("future schema version must be rejected with a typed error, not panic");
    assert_eq!(err.found, SESSION_SUMMARY_SCHEMA_VERSION + 12);
    assert_eq!(err.supported, SESSION_SUMMARY_SCHEMA_VERSION);
    let rendered = err.to_string();
    assert!(rendered.contains("SessionSummary"));
    assert!(rendered.contains("upgrade octos"));
}

#[test]
fn should_round_trip_session_summary_byte_identical() {
    // Invariant 1: `serialize(deserialize(x)) == serialize(deserialize(
    // serialize(deserialize(x))))`. After the first normalization pass, the
    // wire shape is stable; subsequent round-trips must be byte-identical.
    let raw = load_fixture("session_summary_v1.json");
    let parsed: SessionSummary = serde_json::from_str(&raw).expect("fixture parses");
    assert_eq!(parsed.schema_version, SESSION_SUMMARY_SCHEMA_VERSION);
    assert_eq!(parsed.decisions.len(), 2);

    let once = serde_json::to_string(&parsed).expect("serialize once");
    let twice_parsed: SessionSummary = serde_json::from_str(&once).expect("re-parse succeeds");
    let twice = serde_json::to_string(&twice_parsed).expect("serialize twice");
    assert_eq!(
        once, twice,
        "SessionSummary must round-trip byte-identical after normalization",
    );
    assert_eq!(
        twice_parsed, parsed,
        "round-tripped struct must compare equal to the original",
    );

    // Canonicalise both strings through serde_json::Value so ordering
    // differences (pretty-printed whitespace in the fixture vs. compact on
    // round-trip) don't mask semantic drift.
    let fixture_value: serde_json::Value =
        serde_json::from_str(&raw).expect("fixture parses as Value");
    let roundtrip_value: serde_json::Value =
        serde_json::from_str(&once).expect("roundtrip parses as Value");
    assert_eq!(
        fixture_value, roundtrip_value,
        "fixture and roundtrip must be semantically identical",
    );
}
