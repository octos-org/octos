//! Periodic LLM-backed progress summary generator for spawn_only
//! sub-agents (M8.7).
//!
//! `AgentSummaryGenerator` runs a tokio watcher per active sub-agent.
//! Every `tick` seconds (default 30s) it pulls the last N activity lines
//! from a [`crate::subagent_output::SubAgentOutputRouter`] or
//! [`BackgroundTask::runtime_detail`], asks a cheap-lane LLM to condense
//! them into a 3-5 word summary, emits a
//! [`HarnessEvent::SubagentProgress`] event, and folds the summary into
//! `BackgroundTask.runtime_detail` via
//! [`TaskSupervisor::apply_harness_event`].
//!
//! **Guardrail**: watchers only start once a task has been running for
//! longer than `min_runtime` (default 60s). Short tasks that spawn and
//! complete quickly never trigger a summary LLM call.
//!
//! **LLM failure handling**: if a summary call times out or errors the
//! watcher logs a warning and keeps ticking. A single bad tick does not
//! kill the watcher. The tick counter still advances so downstream
//! consumers can detect dropouts.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use octos_core::Message;
use octos_llm::{ChatConfig, LlmProvider};
use tokio::task::JoinHandle;
use tokio::time::timeout;

use crate::harness_events::HarnessEvent;
use crate::subagent_output::SubAgentOutputRouter;
use crate::task_supervisor::{TaskStatus, TaskSupervisor};

/// Default tick cadence between summary calls.
pub const DEFAULT_SUBAGENT_SUMMARY_TICK: Duration = Duration::from_secs(30);

/// Default number of activity lines used as LLM context.
pub const DEFAULT_SUBAGENT_SUMMARY_WINDOW: usize = 20;

/// Default runtime threshold before a watcher is eligible to spawn.
pub const DEFAULT_SUBAGENT_SUMMARY_MIN_RUNTIME: Duration = Duration::from_secs(60);

/// Default timeout for a single summary LLM call.
const DEFAULT_LLM_TIMEOUT: Duration = Duration::from_secs(10);

/// Per-task watcher handle wrapper so callers can abort / join.
pub struct SubAgentSummaryWatcher {
    handle: JoinHandle<()>,
}

impl SubAgentSummaryWatcher {
    /// Abort the watcher. Safe to call multiple times.
    pub fn abort(&self) {
        self.handle.abort();
    }

    /// Poll the underlying tokio task to check if it has finished.
    pub fn is_finished(&self) -> bool {
        self.handle.is_finished()
    }
}

impl std::fmt::Debug for SubAgentSummaryWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubAgentSummaryWatcher")
            .field("finished", &self.handle.is_finished())
            .finish()
    }
}

/// Registry tracking active watchers to prevent double-spawn.
#[derive(Clone, Default)]
pub struct SubAgentSummaryRegistry {
    inner: Arc<Mutex<HashMap<String, Arc<SubAgentSummaryWatcher>>>>,
}

impl SubAgentSummaryRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Return true if a watcher is already tracked for `task_id`.
    pub fn is_active(&self, task_id: &str) -> bool {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.get(task_id).is_some_and(|w| !w.is_finished())
    }

    /// Insert a freshly-started watcher. Returns the prior entry (if any).
    pub fn insert(
        &self,
        task_id: String,
        watcher: SubAgentSummaryWatcher,
    ) -> Option<Arc<SubAgentSummaryWatcher>> {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.insert(task_id, Arc::new(watcher))
    }

    /// Remove and return the watcher for `task_id`, aborting any in-flight
    /// tokio task.
    pub fn remove(&self, task_id: &str) -> Option<Arc<SubAgentSummaryWatcher>> {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let watcher = guard.remove(task_id)?;
        watcher.abort();
        Some(watcher)
    }

    /// Count tracked watchers (including finished ones that have not yet
    /// been explicitly removed).
    pub fn len(&self) -> usize {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.len()
    }

    /// True when the registry holds zero watchers.
    pub fn is_empty(&self) -> bool {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.is_empty()
    }
}

/// Source of the activity lines fed into the summary LLM.
pub enum ActivitySource {
    /// Pull tail lines from the disk output router.
    Router(Arc<SubAgentOutputRouter>),
    /// Stub source used for tests — returns a fixed activity buffer.
    Fixed(Arc<Mutex<Vec<String>>>),
}

impl ActivitySource {
    /// Snapshot the last `window` activity lines for `task_id`.
    fn snapshot(&self, task_id: &str, window: usize) -> Vec<String> {
        match self {
            ActivitySource::Router(router) => {
                router.tail_lines(task_id, window).unwrap_or_default()
            }
            ActivitySource::Fixed(buf) => {
                let guard = buf.lock().unwrap_or_else(|e| e.into_inner());
                guard.iter().rev().take(window).cloned().rev().collect()
            }
        }
    }
}

/// Cheap-lane LLM summary generator for sub-agent runtime detail.
pub struct AgentSummaryGenerator {
    provider: Arc<dyn LlmProvider>,
    tick: Duration,
    summary_window: usize,
    min_runtime: Duration,
    llm_timeout: Duration,
    activity: Arc<ActivitySource>,
    supervisor: TaskSupervisor,
    registry: SubAgentSummaryRegistry,
}

impl std::fmt::Debug for AgentSummaryGenerator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentSummaryGenerator")
            .field("model_id", &self.provider.model_id())
            .field("tick", &self.tick)
            .field("summary_window", &self.summary_window)
            .field("min_runtime", &self.min_runtime)
            .finish()
    }
}

impl AgentSummaryGenerator {
    /// Create a new generator rooted at a specific disk output router and
    /// task supervisor.
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        router: Arc<SubAgentOutputRouter>,
        supervisor: TaskSupervisor,
    ) -> Self {
        Self {
            provider,
            tick: DEFAULT_SUBAGENT_SUMMARY_TICK,
            summary_window: DEFAULT_SUBAGENT_SUMMARY_WINDOW,
            min_runtime: DEFAULT_SUBAGENT_SUMMARY_MIN_RUNTIME,
            llm_timeout: DEFAULT_LLM_TIMEOUT,
            activity: Arc::new(ActivitySource::Router(router)),
            supervisor,
            registry: SubAgentSummaryRegistry::new(),
        }
    }

    /// Construct a generator with an arbitrary activity source — used by
    /// tests that pre-seed a fixed buffer.
    pub fn with_activity_source(
        provider: Arc<dyn LlmProvider>,
        activity: Arc<ActivitySource>,
        supervisor: TaskSupervisor,
    ) -> Self {
        Self {
            provider,
            tick: DEFAULT_SUBAGENT_SUMMARY_TICK,
            summary_window: DEFAULT_SUBAGENT_SUMMARY_WINDOW,
            min_runtime: DEFAULT_SUBAGENT_SUMMARY_MIN_RUNTIME,
            llm_timeout: DEFAULT_LLM_TIMEOUT,
            activity,
            supervisor,
            registry: SubAgentSummaryRegistry::new(),
        }
    }

    /// Override the tick cadence (builder-style).
    #[must_use]
    pub fn with_tick(mut self, tick: Duration) -> Self {
        self.tick = tick;
        self
    }

    /// Override the summary window (builder-style).
    #[must_use]
    pub fn with_summary_window(mut self, window: usize) -> Self {
        self.summary_window = window;
        self
    }

    /// Override the minimum runtime threshold (builder-style).
    #[must_use]
    pub fn with_min_runtime(mut self, min: Duration) -> Self {
        self.min_runtime = min;
        self
    }

    /// Override the per-call LLM timeout (builder-style).
    #[must_use]
    pub fn with_llm_timeout(mut self, t: Duration) -> Self {
        self.llm_timeout = t;
        self
    }

    /// Access the watcher registry (useful for tests and for external
    /// orchestrators that want to reason about active watchers).
    pub fn registry(&self) -> SubAgentSummaryRegistry {
        self.registry.clone()
    }

    /// Spawn a watcher for `task_id` if one is not already tracked. The
    /// watcher terminates when the task reaches a terminal status. Returns
    /// `true` when a new watcher was spawned, `false` when a duplicate
    /// attempt was skipped.
    pub fn spawn_watcher(&self, session_id: impl Into<String>, task_id: impl Into<String>) -> bool {
        let task_id = task_id.into();
        if self.registry.is_active(&task_id) {
            return false;
        }
        let session_id = session_id.into();

        let provider = Arc::clone(&self.provider);
        let tick = self.tick;
        let window = self.summary_window;
        let llm_timeout = self.llm_timeout;
        let activity = Arc::clone(&self.activity);
        let supervisor = self.supervisor.clone();
        let watcher_task_id = task_id.clone();
        let watcher_session_id = session_id.clone();
        let min_runtime = self.min_runtime;

        let handle = tokio::spawn(async move {
            run_watcher_loop(
                provider,
                tick,
                min_runtime,
                window,
                llm_timeout,
                activity,
                supervisor,
                watcher_session_id,
                watcher_task_id,
            )
            .await;
        });

        self.registry
            .insert(task_id, SubAgentSummaryWatcher { handle });
        true
    }

    /// Stop the watcher for `task_id`, if any.
    pub fn stop_watcher(&self, task_id: &str) {
        self.registry.remove(task_id);
    }

    /// Execute one tick synchronously and return the generated summary.
    /// Mainly used by integration tests — production code prefers the
    /// auto-spawned watcher loop.
    pub async fn summarize_once(
        &self,
        session_id: &str,
        task_id: &str,
        tick_seq: u32,
    ) -> Option<String> {
        summarize_tick(
            Arc::clone(&self.provider),
            self.llm_timeout,
            Arc::clone(&self.activity),
            &self.supervisor,
            session_id,
            task_id,
            tick_seq,
            self.summary_window,
            // One-off invocation: no cross-tick state, so it always calls.
            &mut None,
        )
        .await
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_watcher_loop(
    provider: Arc<dyn LlmProvider>,
    tick: Duration,
    min_runtime: Duration,
    window: usize,
    llm_timeout: Duration,
    activity: Arc<ActivitySource>,
    supervisor: TaskSupervisor,
    session_id: String,
    task_id: String,
) {
    // Item 4 of OCTOS_M8_FIX_FIRST_CHECKLIST_2026-04-24:
    // honour `min_runtime` before producing the first summary. Short
    // tasks that complete inside the warm-up window never trigger a
    // cheap-lane LLM call. We poll terminal status during the wait so
    // the watcher exits promptly when a fast task finishes. Use
    // `tokio::time::Instant` so `tokio::time::pause()` test fixtures
    // see the warm-up advance with virtual time.
    let warmup_start = tokio::time::Instant::now();
    while warmup_start.elapsed() < min_runtime {
        if is_terminal(&supervisor, &task_id) {
            return;
        }
        // Cap each warm-up nap at the regular tick so terminal detection
        // stays tight even if the operator configured a long min_runtime.
        let remaining = min_runtime.saturating_sub(warmup_start.elapsed());
        let nap = remaining.min(tick);
        if nap.is_zero() {
            break;
        }
        tokio::time::sleep(nap).await;
    }

    let mut tick_seq: u32 = 0;
    // #2194 review: per-watcher dedupe state — see [`LastTickSummary`].
    let mut last_emitted: Option<LastTickSummary> = None;
    loop {
        tick_seq = tick_seq.saturating_add(1);
        let _ = summarize_tick(
            Arc::clone(&provider),
            llm_timeout,
            Arc::clone(&activity),
            &supervisor,
            &session_id,
            &task_id,
            tick_seq,
            window,
            &mut last_emitted,
        )
        .await;

        if is_terminal(&supervisor, &task_id) {
            break;
        }
        tokio::time::sleep(tick).await;
        if is_terminal(&supervisor, &task_id) {
            break;
        }
    }
}

fn is_terminal(supervisor: &TaskSupervisor, task_id: &str) -> bool {
    match supervisor.get_task(task_id) {
        Some(task) => matches!(task.status, TaskStatus::Completed | TaskStatus::Failed),
        // Missing tasks are treated as terminal — there is no work to watch.
        None => true,
    }
}

/// #2194 review — the rendered prompt + summary a watcher last EMITTED,
/// kept per watcher loop so an idle tick can be answered without an LLM call.
///
/// An idle sub-agent rebuilds the byte-identical prompt every 30s tick
/// (deterministic [`build_prompt`] over an unchanged activity tail). Dedupe
/// beats caching here: a prompt-cache hit would still bill ~0.1x of the
/// prompt, a reused summary bills nothing — and it leaves every prompt that
/// DOES reach the provider genuinely fresh, which is what justifies
/// [`fetch_summary`]'s cache-write opt-out.
///
/// Round 2: the dedupe key is the CANONICAL RENDERED PROMPT, not the raw
/// activity lines. `build_prompt` truncates each line to 400 chars, so two
/// snapshots that differ only past char 400 produce the same prompt — the
/// only correct key is the exact string that would hit the provider.
struct LastTickSummary {
    prompt: String,
    summary: String,
    /// When the summary was actually PRODUCED (the model call that emitted
    /// it). Reused on deduped ticks so the event's `at` never lies about
    /// production time; tick_seq alone conveys that the watcher is live.
    produced_at: chrono::DateTime<chrono::Utc>,
}

#[allow(clippy::too_many_arguments)]
async fn summarize_tick(
    provider: Arc<dyn LlmProvider>,
    llm_timeout: Duration,
    activity: Arc<ActivitySource>,
    supervisor: &TaskSupervisor,
    session_id: &str,
    task_id: &str,
    tick_seq: u32,
    window: usize,
    last: &mut Option<LastTickSummary>,
) -> Option<String> {
    let lines = activity.snapshot(task_id, window);
    // Render the prompt FIRST: it is both the dedupe key and the exact bytes
    // the provider would receive, so post-truncation-identical snapshots
    // dedupe correctly.
    let prompt = build_prompt(&lines);

    // Unchanged rendered prompt => identical answer. Re-emit the previous
    // summary with its ORIGINAL production timestamp (tick_seq still advances
    // to show the watcher is live) and skip the LLM round trip entirely. A
    // changed prompt, an LLM failure, or an empty summary all fall through to
    // a real call — failures never latch.
    if let Some(prev) = last.as_ref()
        && prev.prompt == prompt
    {
        let summary = prev.summary.clone();
        emit_progress(
            supervisor,
            session_id,
            task_id,
            &summary,
            tick_seq,
            prev.produced_at,
        );
        return Some(summary);
    }

    let summary = match fetch_summary(Arc::clone(&provider), prompt.clone(), llm_timeout).await {
        Ok(s) => s,
        Err(error) => {
            tracing::warn!(
                task_id = %task_id,
                tick = tick_seq,
                error = %error,
                "subagent summary LLM call failed; continuing without summary update"
            );
            return None;
        }
    };

    let trimmed = clean_summary(&summary);
    if trimmed.is_empty() {
        return None;
    }

    let produced_at = Utc::now();
    *last = Some(LastTickSummary {
        prompt,
        summary: trimmed.clone(),
        produced_at,
    });
    emit_progress(
        supervisor,
        session_id,
        task_id,
        &trimmed,
        tick_seq,
        produced_at,
    );
    Some(trimmed)
}

fn emit_progress(
    supervisor: &TaskSupervisor,
    session_id: &str,
    task_id: &str,
    summary: &str,
    tick_seq: u32,
    produced_at: chrono::DateTime<chrono::Utc>,
) {
    let event = HarnessEvent::subagent_progress(
        session_id.to_string(),
        task_id.to_string(),
        summary.to_string(),
        tick_seq,
        produced_at,
    );
    if let Err(error) = supervisor.apply_harness_event(task_id, &event) {
        tracing::debug!(
            task_id = %task_id,
            error = %error,
            "subagent summary event could not be applied to supervisor"
        );
    }
}

fn build_prompt(lines: &[String]) -> String {
    let mut prompt = String::new();
    prompt.push_str(
        "Summarize what the sub-agent is doing in 3-5 words, present continuous tense. \
Activities: \n",
    );
    if lines.is_empty() {
        prompt.push_str("(no activity recorded yet)\n");
    } else {
        for line in lines {
            prompt.push_str("- ");
            // Cap each line to keep the prompt bounded even when the sub-agent
            // emits very long single lines.
            let trimmed: String = line.chars().take(400).collect();
            prompt.push_str(&trimmed);
            prompt.push('\n');
        }
    }
    prompt.push_str("\nOutput: just the summary, no preamble, no punctuation at the end.");
    prompt
}

fn clean_summary(raw: &str) -> String {
    let mut trimmed = raw.trim().to_string();
    // Strip surrounding quotes a model sometimes emits.
    if trimmed.starts_with('"') && trimmed.ends_with('"') && trimmed.len() >= 2 {
        trimmed = trimmed[1..trimmed.len() - 1].to_string();
    }
    // Trim trailing punctuation to match the "no punctuation at the end" rule.
    while let Some(ch) = trimmed.chars().last() {
        if matches!(ch, '.' | '!' | '?' | ',' | ';' | ':') {
            trimmed.pop();
        } else {
            break;
        }
    }
    trimmed.chars().take(200).collect()
}

async fn fetch_summary(
    provider: Arc<dyn LlmProvider>,
    prompt: String,
    llm_timeout: Duration,
) -> Result<String, String> {
    let config = ChatConfig {
        max_tokens: Some(64),
        temperature: Some(0.0),
        tool_choice: Default::default(),
        stop_sequences: Vec::new(),
        reasoning_effort: None,
        response_format: None,
        context_management: None,
        sampling_params: None,
        // Every prompt that reaches the provider is genuinely fresh: the
        // watcher dedupes unchanged-snapshot ticks BEFORE calling (see
        // [`LastTickSummary`]), so an identical prompt is never re-sent and
        // a cache write would be pure 1.25x premium.
        cache_retention: octos_llm::CacheRetention::None,
        prompt_cache_context: None,
        media_scope_root: None,
    };
    let messages = vec![Message::user(prompt)];
    let fut = async move { provider.chat(&messages, &[], &config).await };
    match timeout(llm_timeout, fut).await {
        Ok(Ok(response)) => response.content.ok_or_else(|| "empty content".to_string()),
        Ok(Err(error)) => Err(error.to_string()),
        Err(_) => Err(format!(
            "LLM summary call timed out after {}s",
            llm_timeout.as_secs()
        )),
    }
}
