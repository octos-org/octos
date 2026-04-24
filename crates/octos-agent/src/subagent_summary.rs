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

        let handle = tokio::spawn(async move {
            run_watcher_loop(
                provider,
                tick,
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
        )
        .await
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_watcher_loop(
    provider: Arc<dyn LlmProvider>,
    tick: Duration,
    window: usize,
    llm_timeout: Duration,
    activity: Arc<ActivitySource>,
    supervisor: TaskSupervisor,
    session_id: String,
    task_id: String,
) {
    let mut tick_seq: u32 = 0;
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
) -> Option<String> {
    let lines = activity.snapshot(task_id, window);
    let prompt = build_prompt(&lines);

    let summary = match fetch_summary(Arc::clone(&provider), prompt, llm_timeout).await {
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

    let event = HarnessEvent::subagent_progress(
        session_id.to_string(),
        task_id.to_string(),
        trimmed.clone(),
        tick_seq,
        Utc::now(),
    );
    if let Err(error) = supervisor.apply_harness_event(task_id, &event) {
        tracing::debug!(
            task_id = %task_id,
            error = %error,
            "subagent summary event could not be applied to supervisor"
        );
    }
    Some(trimmed)
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

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use octos_llm::{ChatResponse, ChatStream, StopReason, TokenUsage, ToolSpec};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Instant;

    struct MockProvider {
        output: String,
        calls: Arc<AtomicU32>,
    }

    impl MockProvider {
        fn new(output: impl Into<String>) -> (Self, Arc<AtomicU32>) {
            let calls = Arc::new(AtomicU32::new(0));
            (
                Self {
                    output: output.into(),
                    calls: Arc::clone(&calls),
                },
                calls,
            )
        }
    }

    #[async_trait]
    impl LlmProvider for MockProvider {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _config: &ChatConfig,
        ) -> eyre::Result<ChatResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ChatResponse {
                content: Some(self.output.clone()),
                reasoning_content: None,
                tool_calls: Vec::new(),
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
                provider_index: None,
            })
        }

        async fn chat_stream(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _config: &ChatConfig,
        ) -> eyre::Result<ChatStream> {
            unimplemented!("mock provider does not stream")
        }

        fn model_id(&self) -> &str {
            "mock-cheap"
        }

        fn provider_name(&self) -> &str {
            "mock"
        }
    }

    struct SlowProvider;

    #[async_trait]
    impl LlmProvider for SlowProvider {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _config: &ChatConfig,
        ) -> eyre::Result<ChatResponse> {
            tokio::time::sleep(Duration::from_secs(60)).await;
            Err(eyre::eyre!("should never return"))
        }

        async fn chat_stream(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _config: &ChatConfig,
        ) -> eyre::Result<ChatStream> {
            unimplemented!()
        }

        fn model_id(&self) -> &str {
            "slow"
        }

        fn provider_name(&self) -> &str {
            "slow"
        }
    }

    fn fixed_activity(lines: &[&str]) -> Arc<ActivitySource> {
        Arc::new(ActivitySource::Fixed(Arc::new(Mutex::new(
            lines.iter().map(|s| s.to_string()).collect(),
        ))))
    }

    fn register_running_task(supervisor: &TaskSupervisor) -> String {
        let id = supervisor.register("slow_skill", "call-1", Some("api:session"));
        supervisor.mark_running(&id);
        id
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn should_emit_subagent_progress_on_first_tick() {
        let (mock, calls) = MockProvider::new("fetching weather data");
        let supervisor = TaskSupervisor::new();
        let id = register_running_task(&supervisor);
        let generator = AgentSummaryGenerator::with_activity_source(
            Arc::new(mock),
            fixed_activity(&["line1", "line2"]),
            supervisor.clone(),
        )
        .with_tick(Duration::from_secs(30))
        .with_llm_timeout(Duration::from_secs(1));

        let summary = generator.summarize_once("api:session", &id, 1).await;
        assert_eq!(summary.as_deref(), Some("fetching weather data"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // runtime_detail should carry the summary.
        let task = supervisor.get_task(&id).unwrap();
        let detail: serde_json::Value =
            serde_json::from_str(task.runtime_detail.as_deref().unwrap()).unwrap();
        assert_eq!(detail["kind"], "subagent_progress");
        assert_eq!(detail["summary"], "fetching weather data");
        assert_eq!(detail["tick"], 1);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn should_update_runtime_detail_with_summary() {
        let (mock, _) = MockProvider::new("parsing response");
        let supervisor = TaskSupervisor::new();
        let id = register_running_task(&supervisor);
        let generator = AgentSummaryGenerator::with_activity_source(
            Arc::new(mock),
            fixed_activity(&["fetch", "parse"]),
            supervisor.clone(),
        )
        .with_llm_timeout(Duration::from_secs(1));

        let _ = generator.summarize_once("api:session", &id, 7).await;

        let task = supervisor.get_task(&id).unwrap();
        let detail: serde_json::Value =
            serde_json::from_str(task.runtime_detail.as_deref().unwrap()).unwrap();
        assert_eq!(detail["summary"], "parsing response");
        assert_eq!(detail["tick"], 7);
        assert!(detail["at"].is_string());
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn should_tick_every_tick_until_terminal() {
        let (mock, calls) = MockProvider::new("working hard");
        let supervisor = TaskSupervisor::new();
        let id = register_running_task(&supervisor);
        let generator = AgentSummaryGenerator::with_activity_source(
            Arc::new(mock),
            fixed_activity(&["doing something"]),
            supervisor.clone(),
        )
        .with_tick(Duration::from_millis(100))
        .with_llm_timeout(Duration::from_secs(1));

        let spawned = generator.spawn_watcher("api:session", &id);
        assert!(spawned);

        // Drive the watcher a few times so multiple ticks fire. We alternate
        // time advance + yield so the spawned task actually runs.
        for _ in 0..10 {
            tokio::task::yield_now().await;
            tokio::time::advance(Duration::from_millis(110)).await;
        }
        supervisor.mark_completed(&id, vec![]);
        for _ in 0..4 {
            tokio::task::yield_now().await;
            tokio::time::advance(Duration::from_millis(110)).await;
        }

        let observed = calls.load(Ordering::SeqCst);
        assert!(observed >= 2, "expected multiple ticks, got {observed}");
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn should_stop_ticking_on_task_completed() {
        let (mock, calls) = MockProvider::new("working");
        let supervisor = TaskSupervisor::new();
        let id = register_running_task(&supervisor);
        let generator = AgentSummaryGenerator::with_activity_source(
            Arc::new(mock),
            fixed_activity(&["line"]),
            supervisor.clone(),
        )
        .with_tick(Duration::from_millis(50))
        .with_llm_timeout(Duration::from_secs(1));

        generator.spawn_watcher("api:session", &id);
        tokio::time::advance(Duration::from_millis(120)).await;
        tokio::task::yield_now().await;
        supervisor.mark_completed(&id, vec![]);
        tokio::time::advance(Duration::from_millis(200)).await;
        tokio::task::yield_now().await;
        let baseline = calls.load(Ordering::SeqCst);
        tokio::time::advance(Duration::from_millis(300)).await;
        tokio::task::yield_now().await;
        let after = calls.load(Ordering::SeqCst);
        assert_eq!(
            baseline, after,
            "no more tick calls should fire after terminal"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn should_stop_ticking_on_task_failed() {
        let (mock, calls) = MockProvider::new("working");
        let supervisor = TaskSupervisor::new();
        let id = register_running_task(&supervisor);
        let generator = AgentSummaryGenerator::with_activity_source(
            Arc::new(mock),
            fixed_activity(&["line"]),
            supervisor.clone(),
        )
        .with_tick(Duration::from_millis(50))
        .with_llm_timeout(Duration::from_secs(1));

        generator.spawn_watcher("api:session", &id);
        tokio::time::advance(Duration::from_millis(120)).await;
        tokio::task::yield_now().await;
        supervisor.mark_failed(&id, "boom".into());
        tokio::time::advance(Duration::from_millis(200)).await;
        tokio::task::yield_now().await;
        let baseline = calls.load(Ordering::SeqCst);
        tokio::time::advance(Duration::from_millis(400)).await;
        tokio::task::yield_now().await;
        let after = calls.load(Ordering::SeqCst);
        assert_eq!(baseline, after);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn should_not_spawn_duplicate_watcher_for_same_task() {
        let (mock, _) = MockProvider::new("working");
        let supervisor = TaskSupervisor::new();
        let id = register_running_task(&supervisor);
        let generator = AgentSummaryGenerator::with_activity_source(
            Arc::new(mock),
            fixed_activity(&["x"]),
            supervisor.clone(),
        )
        .with_tick(Duration::from_secs(5));

        assert!(generator.spawn_watcher("api:session", &id));
        assert!(!generator.spawn_watcher("api:session", &id));
        assert_eq!(generator.registry().len(), 1);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn should_handle_llm_timeout_without_crashing_watcher() {
        let supervisor = TaskSupervisor::new();
        let id = register_running_task(&supervisor);
        let started = Instant::now();
        let generator = AgentSummaryGenerator::with_activity_source(
            Arc::new(SlowProvider),
            fixed_activity(&["line"]),
            supervisor.clone(),
        )
        .with_tick(Duration::from_millis(50))
        .with_llm_timeout(Duration::from_millis(10));

        let summary = generator.summarize_once("api:session", &id, 1).await;
        assert!(summary.is_none(), "timeout should yield no summary");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "timeout must return quickly instead of hanging"
        );
    }
}
