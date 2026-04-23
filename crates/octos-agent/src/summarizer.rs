//! Compaction summarizer seam (harness M6.3 / M6.4).
//!
//! Compaction turns a block of old conversation messages into a bounded
//! summary. Different strategies live behind a shared trait so the runtime
//! can swap implementations without reshuffling the agent loop:
//!
//! - [`ExtractiveSummarizer`] — deterministic, dependency-free. Preserves the
//!   existing M0 behaviour (header + per-message first-line extraction +
//!   tool-argument stripping). Default for every caller.
//! - [`LlmIterativeSummarizer`] — M6.4. Calls an LLM to produce a typed
//!   [`SessionSummary`] payload, iteratively refines the prior summary with
//!   new turns, and falls back to the extractive path after three consecutive
//!   LLM failures.
//!
//! The trait is deliberately minimal: take the old messages plus a budget,
//! hand back a `Result<String>`. Failures fall back to the extractive path
//! so no single implementation can break the loop.

use std::sync::{Arc, Mutex};

use eyre::{Result, WrapErr, eyre};
use octos_core::{
    DecisionRecord, FileRecord, Message, SESSION_SUMMARY_SCHEMA_VERSION, SessionSummary,
};
use octos_llm::{ChatConfig, LlmProvider, ResponseFormat};
use tokio::runtime::Handle;
use tracing::warn;

use crate::compaction::compact_messages;
use crate::workspace_policy::CompactionSummarizerKind;

/// Seam for compaction summarization strategies.
///
/// Implementors take a slice of messages to compact and a `budget_tokens`
/// ceiling, and return a text summary whose token estimate stays at or below
/// the budget. Deterministic implementations should be pure. Async-only
/// implementations (e.g. an LLM summarizer) can block on `tokio::runtime::Handle::current`
/// if necessary — the signature stays synchronous so the agent loop can run
/// compaction without awaiting inside the message-prep pipeline.
///
/// The trait must be `Send + Sync` so the runtime can keep summarizers in
/// `Arc<dyn Summarizer>` and share them across spawned worker tasks.
pub trait Summarizer: Send + Sync {
    /// Stable, human-readable identifier for this summarizer strategy.
    ///
    /// Reported in compaction phase events so operators can tell whether a
    /// turn was compacted by the extractive (`"extractive"`) or the
    /// LLM-iterative (`"llm_iterative"`) variant. Keep this lowercase
    /// snake_case so it also serializes cleanly through
    /// `CompactionSummarizerKind`.
    fn kind(&self) -> &'static str;

    /// Return a bounded summary of `messages`.
    ///
    /// Implementors MUST respect `budget_tokens`. The extractive fallback
    /// measures token count via `octos_llm::context::estimate_tokens`, so
    /// approximate adherence is acceptable — but wildly overshooting the
    /// budget is a contract violation and will be rejected by the runtime.
    fn summarize(&self, messages: &[Message], budget_tokens: u32) -> Result<String>;
}

/// Deterministic, dependency-free summarizer that preserves the existing
/// extractive behaviour. Used by default so the absence of a policy leaves
/// the loop indistinguishable from the pre-M6.3 runtime.
#[derive(Debug, Default, Clone, Copy)]
pub struct ExtractiveSummarizer;

impl ExtractiveSummarizer {
    /// Construct a new extractive summarizer.
    pub fn new() -> Self {
        Self
    }
}

impl Summarizer for ExtractiveSummarizer {
    fn kind(&self) -> &'static str {
        "extractive"
    }

    fn summarize(&self, messages: &[Message], budget_tokens: u32) -> Result<String> {
        Ok(compact_messages(messages, budget_tokens))
    }
}

/// Default threshold for consecutive LLM failures before the iterative
/// summarizer locks into the extractive fallback for the rest of the session.
pub const DEFAULT_LLM_SUMMARIZER_FAILURE_THRESHOLD: u32 = 3;

/// Mutable state carried by [`LlmIterativeSummarizer`] across invocations.
///
/// The struct itself is shared via `Arc<Mutex<_>>` so `Summarizer::summarize`
/// (which is `&self`) can update both the prior summary (enabling iterative
/// refinement) and the failure counter (enabling the 3-strike fallback).
#[derive(Debug, Default, Clone)]
struct LlmSummarizerState {
    prior_summary: Option<SessionSummary>,
    consecutive_failures: u32,
    /// Latched to `true` once the failure threshold is exceeded. While
    /// latched, the summarizer delegates to the extractive fallback without
    /// calling the LLM again.
    extractive_latched: bool,
}

/// JSON schema describing the shape of a [`SessionSummary`] returned by the
/// iterative summarizer. Matches the field list of the struct — additional or
/// renamed fields require a corresponding schema bump and a
/// [`SESSION_SUMMARY_SCHEMA_VERSION`] increment.
fn session_summary_json_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": [
            "goal",
            "constraints",
            "progress_done",
            "progress_in_progress",
            "decisions",
            "files",
            "next_steps"
        ],
        "properties": {
            "schema_version": {"type": "integer", "minimum": 1},
            "goal": {"type": "string"},
            "constraints": {"type": "array", "items": {"type": "string"}},
            "progress_done": {"type": "array", "items": {"type": "string"}},
            "progress_in_progress": {"type": "array", "items": {"type": "string"}},
            "decisions": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": ["at_turn", "summary"],
                    "properties": {
                        "at_turn": {"type": "integer", "minimum": 0},
                        "summary": {"type": "string"},
                        "rationale": {"type": ["string", "null"]},
                    },
                },
            },
            "files": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": ["path", "role"],
                    "properties": {
                        "path": {"type": "string"},
                        "role": {"type": "string"},
                    },
                },
            },
            "next_steps": {"type": "array", "items": {"type": "string"}},
        },
    })
}

/// LLM-iterative compaction summarizer (harness M6.4).
///
/// Unlike prose-template summarizers, this implementation produces a typed
/// [`SessionSummary`] object, which the compaction runner serializes back
/// into the message stream. Iterative refinement is first-class: when a prior
/// summary exists, the LLM is asked to update the existing records rather
/// than regenerate from scratch, and decisions either stay verbatim or are
/// marked stale (never silently dropped).
///
/// Failure policy:
/// - Each LLM call that returns a malformed or unsupported payload counts
///   as one consecutive failure.
/// - After
///   [`DEFAULT_LLM_SUMMARIZER_FAILURE_THRESHOLD`] consecutive failures (3 by
///   default), the summarizer latches into extractive fallback and stops
///   calling the LLM. A WARN log explains the reason.
/// - A successful LLM call resets the counter.
///
/// The trait is synchronous; the provider call is bridged through
/// `tokio::runtime::Handle::current().block_on` so the agent loop can invoke
/// compaction without awaiting inside message-prep.
pub struct LlmIterativeSummarizer {
    provider: Arc<dyn LlmProvider>,
    state: Arc<Mutex<LlmSummarizerState>>,
    fallback: ExtractiveSummarizer,
    failure_threshold: u32,
}

impl std::fmt::Debug for LlmIterativeSummarizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmIterativeSummarizer")
            .field("model_id", &self.provider.model_id())
            .field("provider_name", &self.provider.provider_name())
            .field("failure_threshold", &self.failure_threshold)
            .finish()
    }
}

impl LlmIterativeSummarizer {
    /// Construct a new summarizer wired to `provider` with the default
    /// 3-strike fallback threshold.
    pub fn new(provider: Arc<dyn LlmProvider>) -> Self {
        Self {
            provider,
            state: Arc::new(Mutex::new(LlmSummarizerState::default())),
            fallback: ExtractiveSummarizer,
            failure_threshold: DEFAULT_LLM_SUMMARIZER_FAILURE_THRESHOLD,
        }
    }

    /// Override the consecutive-failure threshold before the summarizer
    /// latches into the extractive fallback. Useful in tests.
    pub fn with_failure_threshold(mut self, threshold: u32) -> Self {
        self.failure_threshold = threshold.max(1);
        self
    }

    /// Seed the iterative state with an existing prior summary (for example,
    /// when resuming from a persisted session). Not required for correctness;
    /// the summarizer also builds up its prior from successive invocations.
    pub fn with_prior_summary(self, prior: SessionSummary) -> Self {
        if let Ok(mut state) = self.state.lock() {
            state.prior_summary = Some(prior);
        }
        self
    }

    /// Read-only snapshot of the latest `SessionSummary` emitted by this
    /// summarizer. Useful for diagnostics and for wiring persistence hooks.
    pub fn latest_summary(&self) -> Option<SessionSummary> {
        self.state.lock().ok()?.prior_summary.clone()
    }

    /// Returns true once three consecutive LLM failures have latched the
    /// summarizer into the extractive fallback.
    pub fn is_in_fallback(&self) -> bool {
        self.state
            .lock()
            .map(|s| s.extractive_latched)
            .unwrap_or(false)
    }

    fn build_prompt(&self, prior: &Option<SessionSummary>, messages: &[Message]) -> String {
        let mut prompt = String::new();
        prompt.push_str(
            "You are the octos harness M6.4 iterative summarizer. Emit a JSON object \
matching the SessionSummary schema so the agent loop can round-trip it \
byte-identical. Preserve every prior decision: either keep it verbatim, or \
mark it stale by prefixing the `summary` with `[STALE] ` (square brackets). \
Never silently drop a decision. Update existing progress lists in place \
rather than appending duplicates.\n\n",
        );
        prompt.push_str("Schema (JSON Schema draft 7):\n");
        prompt.push_str(
            &serde_json::to_string_pretty(&session_summary_json_schema())
                .unwrap_or_else(|_| "{}".to_string()),
        );
        prompt.push_str("\n\n");
        prompt.push_str(&format!(
            "Emit `schema_version`: {SESSION_SUMMARY_SCHEMA_VERSION}.\n\n"
        ));

        if let Some(prior) = prior {
            prompt.push_str("Prior SessionSummary (refine this rather than regenerating):\n");
            prompt.push_str(
                &serde_json::to_string_pretty(prior)
                    .unwrap_or_else(|_| "{\"goal\":\"\"}".to_string()),
            );
            prompt.push_str("\n\n");
        } else {
            prompt.push_str("No prior SessionSummary; emit a fresh one.\n\n");
        }

        prompt.push_str("New conversation turns to fold into the summary:\n");
        for (i, msg) in messages.iter().enumerate() {
            let first_line = msg.content.lines().next().unwrap_or("").trim();
            let truncated: String = first_line.chars().take(400).collect();
            prompt.push_str(&format!(
                "[{i:03}] {role}: {text}\n",
                role = msg.role,
                text = truncated,
            ));
        }
        prompt.push_str(
            "\nReturn ONLY the JSON object; no prose, no Markdown fence, no trailing text.\n",
        );
        prompt
    }

    fn call_llm(&self, prompt: String, budget_tokens: u32) -> Result<SessionSummary> {
        let provider = Arc::clone(&self.provider);
        let schema = session_summary_json_schema();
        let config = ChatConfig {
            // Cap generation at the budget so the return fits the outgoing
            // compaction slot. Providers vary in token accounting so add a
            // modest safety floor.
            max_tokens: Some(budget_tokens.max(256)),
            temperature: Some(0.0),
            tool_choice: Default::default(),
            stop_sequences: Vec::new(),
            reasoning_effort: None,
            response_format: Some(ResponseFormat::JsonSchema {
                name: "session_summary".to_string(),
                schema,
                strict: true,
            }),
        };
        let messages = vec![Message::user(prompt)];
        // Bridge the async LLM call to the synchronous Summarizer contract.
        // See [`run_llm_call_blocking`] for the flavor-aware dispatch; we
        // must not call `Handle::block_on` directly from a current-thread
        // runtime (it panics), so the helper off-hosts the future onto a
        // dedicated thread when needed.
        let response =
            run_llm_call_blocking(async move { provider.chat(&messages, &[], &config).await })
                .wrap_err("LLM call for iterative summarizer failed")?;

        let text = response
            .content
            .ok_or_else(|| eyre!("LLM returned no content for iterative summary"))?;
        let trimmed = strip_optional_json_fence(text.trim());
        let mut summary: SessionSummary = serde_json::from_str(trimmed)
            .wrap_err("iterative summarizer response was not valid SessionSummary JSON")?;
        // Pin any missing schema_version to v1 (legacy-compat path).
        if summary.schema_version == 0 {
            summary.schema_version = SESSION_SUMMARY_SCHEMA_VERSION;
        }
        summary
            .validate_schema_version()
            .map_err(|e| eyre!("{e}"))?;
        Ok(summary)
    }

    fn encode_summary(&self, summary: &SessionSummary) -> Result<String> {
        // Render the typed summary as a compact system-message-style block so
        // the downstream message stream sees a stable, human-readable shape
        // while the authoritative JSON is preserved inline for round-trip.
        let json = serde_json::to_string(summary)
            .wrap_err("serialize SessionSummary for compaction message")?;
        let mut out = String::new();
        out.push_str("## Conversation Summary (iterative, v");
        out.push_str(&summary.schema_version.to_string());
        out.push_str(")\n");
        out.push_str(&format!("Goal: {}\n", summary.goal));
        if !summary.progress_done.is_empty() {
            out.push_str("Done:\n");
            for item in &summary.progress_done {
                out.push_str(&format!("- {item}\n"));
            }
        }
        if !summary.progress_in_progress.is_empty() {
            out.push_str("In progress:\n");
            for item in &summary.progress_in_progress {
                out.push_str(&format!("- {item}\n"));
            }
        }
        if !summary.decisions.is_empty() {
            out.push_str("Decisions:\n");
            for d in &summary.decisions {
                out.push_str(&format!("- (turn {}) {}\n", d.at_turn, d.summary));
            }
        }
        if !summary.files.is_empty() {
            out.push_str("Files:\n");
            for f in &summary.files {
                out.push_str(&format!("- {} ({})\n", f.path, f.role));
            }
        }
        if !summary.next_steps.is_empty() {
            out.push_str("Next:\n");
            for item in &summary.next_steps {
                out.push_str(&format!("- {item}\n"));
            }
        }
        out.push_str("<!-- session_summary_v1: ");
        out.push_str(&json);
        out.push_str(" -->\n");
        Ok(out)
    }

    fn merge_prior(&self, prior: &Option<SessionSummary>, new: &SessionSummary) -> SessionSummary {
        let Some(prior) = prior else {
            return new.clone();
        };
        let mut merged = new.clone();
        merged.schema_version = SESSION_SUMMARY_SCHEMA_VERSION;

        // Retain prior decisions the LLM omitted. Invariant 5: prior
        // decisions either stay verbatim or arrive explicitly marked stale
        // — never silently dropped. We rely on `at_turn + summary-without-
        // STALE-prefix` as the identity for a decision.
        for prior_decision in &prior.decisions {
            let prior_body = prior_decision
                .summary
                .trim_start_matches(stale_decision_prefix())
                .trim();
            let still_present = merged.decisions.iter().any(|d| {
                d.at_turn == prior_decision.at_turn
                    && d.summary.trim_start_matches(stale_decision_prefix()).trim() == prior_body
            });
            if !still_present {
                merged.decisions.push(DecisionRecord {
                    at_turn: prior_decision.at_turn,
                    summary: format!("{} {}", stale_decision_prefix(), prior_body),
                    rationale: prior_decision.rationale.clone(),
                });
            }
        }

        // Preserve prior files whose paths the LLM dropped so the compaction
        // trail never loses track of what's on disk. If the LLM re-listed a
        // file the new role wins.
        for prior_file in &prior.files {
            if !merged.files.iter().any(|f| f.path == prior_file.path) {
                merged.files.push(FileRecord {
                    path: prior_file.path.clone(),
                    role: prior_file.role.clone(),
                });
            }
        }

        // Preserve prior constraints — these are invariants the session
        // accepted, and iterative compaction shouldn't shed them unless the
        // LLM explicitly re-asserted the constraint list.
        for c in &prior.constraints {
            if !merged.constraints.iter().any(|existing| existing == c) {
                merged.constraints.push(c.clone());
            }
        }

        merged
    }
}

/// Drive a future to completion from a synchronous caller without crashing
/// the Tokio runtime.
///
/// - On a multi-threaded runtime we use `block_in_place + block_on` so the
///   worker thread can park cleanly.
/// - On a current-thread runtime (e.g. `#[tokio::test]` default) we off-host
///   the work onto a dedicated OS thread whose runtime handle is the same
///   current-thread scheduler — `Handle::block_on` is safe there because the
///   thread is not itself inside the runtime. This keeps the trait
///   signature synchronous without forcing callers into multi-threaded
///   scheduling.
fn run_llm_call_blocking<F, T>(future: F) -> T
where
    F: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let handle = Handle::current();
    match handle.runtime_flavor() {
        tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(|| handle.block_on(future))
        }
        _ => {
            // Current-thread runtime: a plain `block_on` from inside would
            // deadlock because we are the single worker. Spawn an OS thread
            // so it can drive the future via the same runtime handle.
            let (tx, rx) = std::sync::mpsc::channel();
            let thread_handle = handle.clone();
            std::thread::spawn(move || {
                let out = thread_handle.block_on(future);
                let _ = tx.send(out);
            });
            rx.recv().expect("llm summarizer worker thread dropped")
        }
    }
}

fn strip_optional_json_fence(text: &str) -> &str {
    let trimmed = text.trim();
    if let Some(rest) = trimmed.strip_prefix("```json") {
        rest.trim_end_matches("```").trim()
    } else if let Some(rest) = trimmed.strip_prefix("```") {
        rest.trim_end_matches("```").trim()
    } else {
        trimmed
    }
}

impl Summarizer for LlmIterativeSummarizer {
    fn kind(&self) -> &'static str {
        "llm_iterative"
    }

    fn summarize(&self, messages: &[Message], budget_tokens: u32) -> Result<String> {
        // Fast path: once the 3-strike threshold latched us into the
        // extractive fallback we never call the LLM again for this
        // summarizer instance.
        {
            let state = self
                .state
                .lock()
                .map_err(|_| eyre!("state mutex poisoned"))?;
            if state.extractive_latched {
                return self.fallback.summarize(messages, budget_tokens);
            }
        }

        // Snapshot the prior summary under a short-lived lock; we don't want
        // to hold the mutex across an LLM call.
        let prior = {
            let state = self
                .state
                .lock()
                .map_err(|_| eyre!("state mutex poisoned"))?;
            state.prior_summary.clone()
        };

        let prompt = self.build_prompt(&prior, messages);
        match self.call_llm(prompt, budget_tokens) {
            Ok(new_summary) => {
                let merged = self.merge_prior(&prior, &new_summary);
                let encoded = self.encode_summary(&merged)?;
                if let Ok(mut state) = self.state.lock() {
                    state.prior_summary = Some(merged);
                    state.consecutive_failures = 0;
                }
                Ok(encoded)
            }
            Err(err) => {
                let (failures, latched) = {
                    let mut state = self
                        .state
                        .lock()
                        .map_err(|_| eyre!("state mutex poisoned"))?;
                    state.consecutive_failures = state.consecutive_failures.saturating_add(1);
                    if state.consecutive_failures >= self.failure_threshold {
                        state.extractive_latched = true;
                    }
                    (state.consecutive_failures, state.extractive_latched)
                };
                warn!(
                    error = %err,
                    consecutive_failures = failures,
                    latched = latched,
                    "llm_iterative summarizer: LLM call failed; falling back to extractive for this pass"
                );
                self.fallback.summarize(messages, budget_tokens)
            }
        }
    }
}

/// Helper so crate modules can reference the stale-decision prefix without
/// importing the constant from `octos_core` in every call site.
pub(crate) fn stale_decision_prefix() -> &'static str {
    octos_core::STALE_DECISION_PREFIX
}

/// Map a declared [`CompactionSummarizerKind`] to a concrete `Summarizer`
/// instance. The LLM-iterative variant requires a provider and is wired via
/// [`default_summarizer_for_with_provider`]; this shim exists so existing
/// callers that only consume the extractive strategy keep compiling.
///
/// With `LlmIterative`, the returned summarizer is the extractive fallback
/// — the LLM-iterative implementation is only enabled when the caller also
/// supplies an [`LlmProvider`].
pub fn default_summarizer_for(kind: CompactionSummarizerKind) -> Arc<dyn Summarizer> {
    match kind {
        CompactionSummarizerKind::Extractive => Arc::new(ExtractiveSummarizer::new()),
        CompactionSummarizerKind::LlmIterative => {
            // Fallback: the iterative summarizer cannot run without a
            // provider. Callers that want the LLM path must use
            // `default_summarizer_for_with_provider`.
            Arc::new(ExtractiveSummarizer::new())
        }
    }
}

/// Provider-aware factory: map a declared [`CompactionSummarizerKind`] to
/// the concrete summarizer, enabling the LLM-iterative variant when a
/// provider is wired.
///
/// When `provider` is `None` the LLM-iterative variant falls back to the
/// extractive summarizer (same behaviour as [`default_summarizer_for`]).
pub fn default_summarizer_for_with_provider(
    kind: CompactionSummarizerKind,
    provider: Option<Arc<dyn LlmProvider>>,
) -> Arc<dyn Summarizer> {
    match kind {
        CompactionSummarizerKind::Extractive => Arc::new(ExtractiveSummarizer::new()),
        CompactionSummarizerKind::LlmIterative => match provider {
            Some(p) => Arc::new(LlmIterativeSummarizer::new(p)),
            None => Arc::new(ExtractiveSummarizer::new()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use octos_core::MessageRole;

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

    #[test]
    fn extractive_summarizer_reports_stable_kind() {
        assert_eq!(ExtractiveSummarizer::new().kind(), "extractive");
    }

    #[test]
    fn extractive_summarizer_produces_nonempty_summary_within_budget() {
        let messages = vec![user("hello"), user("world")];
        let summary = ExtractiveSummarizer::new()
            .summarize(&messages, 2_000)
            .expect("summarize should succeed");
        assert!(summary.contains("Conversation Summary"));
        assert!(summary.contains("> User: hello"));
    }

    #[test]
    fn strip_json_fence_handles_plain_json() {
        let raw = "{\"goal\": \"x\"}";
        assert_eq!(strip_optional_json_fence(raw), raw);
    }

    #[test]
    fn strip_json_fence_strips_code_fence_block() {
        let raw = "```json\n{\"goal\": \"x\"}\n```";
        assert_eq!(strip_optional_json_fence(raw), "{\"goal\": \"x\"}");
    }

    #[test]
    fn strip_json_fence_strips_bare_triple_backticks() {
        let raw = "```\n{\"goal\": \"x\"}\n```";
        assert_eq!(strip_optional_json_fence(raw), "{\"goal\": \"x\"}");
    }

    #[test]
    fn session_summary_schema_lists_required_fields() {
        let schema = session_summary_json_schema();
        let required = schema["required"].as_array().unwrap();
        let required_strs: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
        for field in [
            "goal",
            "constraints",
            "progress_done",
            "progress_in_progress",
            "decisions",
            "files",
            "next_steps",
        ] {
            assert!(required_strs.contains(&field), "schema requires {field}");
        }
    }

    #[test]
    fn default_summarizer_for_extractive_returns_extractive() {
        let s = default_summarizer_for(CompactionSummarizerKind::Extractive);
        assert_eq!(s.kind(), "extractive");
    }

    #[test]
    fn default_summarizer_for_llm_iterative_without_provider_falls_back_to_extractive() {
        // M6.3 semantics: without a provider, we keep the legacy extractive
        // path so the runtime stays green even when config declares
        // llm_iterative.
        let s = default_summarizer_for(CompactionSummarizerKind::LlmIterative);
        assert_eq!(s.kind(), "extractive");
    }
}
