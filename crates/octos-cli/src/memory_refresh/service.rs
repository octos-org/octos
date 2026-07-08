//! The background memory-refresh service: single-owner lock, session
//! discovery with snapshot watermarks, budgeted extraction passes.
//!
//! Runs only in the long-running process that wins the profile's refresh
//! lock (serve or gateway; `octos chat` never starts it). One `flock`'d
//! file owns the whole pipeline for a profile — extraction now, the
//! consolidation trigger when PR-4 lands — and auto-releases on process
//! exit or crash because the lock rides the open file descriptor.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use eyre::{Result, WrapErr};
use fs2::FileExt;
use octos_bus::SessionManager;
use octos_core::{Message, MessageRole};
use octos_llm::{ChatConfig, LlmProvider};
use octos_memory::MemoryStore;

use super::extract::{EXTRACTION_SYSTEM_PROMPT, parse_extraction_response, validate_items};
use super::input::{build_input_lines, render_transcript};

/// Resolved knobs for the sweep (from `memory.refresh.*` config).
#[derive(Debug, Clone)]
pub struct RefreshKnobs {
    pub min_idle: Duration,
    pub max_session_age: Duration,
    pub max_sessions_per_pass: usize,
    pub max_extractions_per_day: u32,
    pub max_daily_tokens: u64,
    pub interval: Duration,
    /// Hard input budget for one extraction call (tokens; CJK-aware
    /// estimate). Provider metadata carries no context-window size, so
    /// this is a knob rather than "70% of the model window".
    pub max_extract_input_tokens: usize,
    /// Token budget for the CURRENT MEMORY block shown to the extractor.
    pub max_inject_tokens: usize,
}

const MAX_EXTRACT_INPUT_BYTES: usize = 512 * 1024;
/// Per-session consecutive failure cap before the session is skipped.
const MAX_SESSION_FAILURES: u32 = 3;
const BACKOFF_BASE: Duration = Duration::from_secs(5 * 60);
const BACKOFF_MAX: Duration = Duration::from_secs(80 * 60);

/// One file's read snapshot: the exact bytes-state the sweep consumed.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct FileSnap {
    pub path: PathBuf,
    pub mtime_ms: u64,
    pub len: u64,
}

impl FileSnap {
    fn of(path: &Path, modified: SystemTime, len: u64) -> Self {
        Self {
            path: path.to_path_buf(),
            mtime_ms: modified
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            len,
        }
    }
}

/// Durable sweep state under `memory/refresh_state.json`, written ONLY by
/// the lock holder (atomic temp+rename).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct RefreshState {
    /// Local date the daily budgets were last reset on.
    #[serde(default)]
    pub date: String,
    #[serde(default)]
    pub extractions_today: u32,
    #[serde(default)]
    pub tokens_today: u64,
    /// Per session key: the file snapshots actually READ last time.
    #[serde(default)]
    pub watermarks: std::collections::BTreeMap<String, Vec<FileSnap>>,
    /// Per session key: consecutive extraction failures (skip at cap).
    #[serde(default)]
    pub failures: std::collections::BTreeMap<String, u32>,
}

impl RefreshState {
    pub(crate) fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub(crate) fn save(&self, path: &Path) -> Result<()> {
        let parent = path
            .parent()
            .ok_or_else(|| eyre::eyre!("state path has no parent"))?;
        std::fs::create_dir_all(parent)?;
        let tmp = parent.join(".refresh_state.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, path).wrap_err("failed to publish refresh state")?;
        Ok(())
    }

    /// Reset daily budgets on local-date rollover.
    pub(crate) fn roll_date(&mut self, today: &str) {
        if self.date != today {
            self.date = today.to_string();
            self.extractions_today = 0;
            self.tokens_today = 0;
        }
    }
}

/// Exponential scheduler backoff: 5 → 10 → 20 → 40 → 80 min (capped).
pub(crate) fn backoff_after(consecutive_failures: u32) -> Duration {
    let exp = consecutive_failures.saturating_sub(1).min(4);
    BACKOFF_MAX.min(BACKOFF_BASE * 2u32.pow(exp))
}

/// Handle to a running refresh service; dropping it stops the loop and
/// releases the profile lock.
pub struct MemoryRefreshService {
    shutdown: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for MemoryRefreshService {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        self.task.abort();
    }
}

impl MemoryRefreshService {
    /// Try to become the profile's refresh owner and start the sweep loop.
    ///
    /// Returns `None` (with an info log) when another process already
    /// holds `memory/.refresh.lock` — the winner is arbitrary by design.
    pub fn try_start(
        data_dir: PathBuf,
        memory_store: Arc<MemoryStore>,
        provider: Arc<dyn LlmProvider>,
        knobs: RefreshKnobs,
    ) -> Option<Self> {
        let lock_file = match acquire_refresh_lock(&data_dir) {
            Ok(Some(file)) => file,
            Ok(None) => {
                tracing::info!(
                    data_dir = %data_dir.display(),
                    "memory refresh lock held elsewhere; this process skips the sweep"
                );
                return None;
            }
            Err(e) => {
                tracing::warn!("failed to set up memory refresh lock: {e}");
                return None;
            }
        };

        let shutdown = Arc::new(AtomicBool::new(false));
        let stop = shutdown.clone();
        let task = tokio::spawn(async move {
            // The lock fd lives here for the task's lifetime.
            let _lock = lock_file;
            let mut consecutive_failures: u32 = 0;
            let mut backoff_until: Option<tokio::time::Instant> = None;
            let mut ticker = tokio::time::interval(knobs.interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                if stop.load(Ordering::Acquire) {
                    break;
                }
                if let Some(until) = backoff_until {
                    if tokio::time::Instant::now() < until {
                        continue;
                    }
                    backoff_until = None;
                }
                match run_extraction_pass(&data_dir, &memory_store, provider.as_ref(), &knobs).await
                {
                    Ok(report) => {
                        consecutive_failures = 0;
                        if report.extracted > 0 || report.skipped_budget {
                            tracing::info!(
                                extracted = report.extracted,
                                candidates = report.candidates,
                                skipped_budget = report.skipped_budget,
                                "memory extraction pass complete"
                            );
                        }
                    }
                    Err(e) => {
                        consecutive_failures += 1;
                        let wait = backoff_after(consecutive_failures);
                        backoff_until = Some(tokio::time::Instant::now() + wait);
                        tracing::warn!(
                            failures = consecutive_failures,
                            backoff_secs = wait.as_secs(),
                            "memory extraction pass failed: {e:#}"
                        );
                    }
                }
            }
        });
        Some(Self { shutdown, task })
    }
}

/// Open + `flock` the profile refresh lock. `Ok(None)` = held elsewhere.
fn acquire_refresh_lock(data_dir: &Path) -> Result<Option<std::fs::File>> {
    use std::io::Write;
    let memory_dir = data_dir.join("memory");
    std::fs::create_dir_all(&memory_dir)?;
    let lock_path = memory_dir.join(".refresh.lock");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)?;
    match file.try_lock_exclusive() {
        Ok(()) => {
            // Diagnostics only — the flock is the actual mutex.
            let _ = file.set_len(0);
            let _ = writeln!(
                file,
                "pid: {}\nstarted_at: {}",
                std::process::id(),
                chrono::Utc::now().to_rfc3339()
            );
            Ok(Some(file))
        }
        // Only genuine contention means "held elsewhere"; any other error
        // (EINTR under load, fd pressure) must surface, not masquerade as
        // a running service.
        Err(e) if e.kind() == fs2::lock_contended_error().kind() => Ok(None),
        Err(e) => Err(e).wrap_err("flock on memory refresh lock failed"),
    }
}

#[derive(Debug, Default)]
pub struct PassReport {
    pub candidates: usize,
    pub extracted: usize,
    pub skipped_budget: bool,
}

/// One-shot manual pass (`octos memory refresh`): acquire the profile
/// lock non-blocking or report the live owner and bail.
pub async fn run_once(
    data_dir: &Path,
    memory_store: &Arc<MemoryStore>,
    provider: &dyn LlmProvider,
    knobs: &RefreshKnobs,
) -> Result<PassReport> {
    let Some(_lock) = acquire_refresh_lock(data_dir)? else {
        let holder = std::fs::read_to_string(data_dir.join("memory").join(".refresh.lock"))
            .unwrap_or_default();
        eyre::bail!(
            "memory refresh lock is held by a running service — let it sweep, or stop it first.\n{}",
            holder.trim()
        );
    };
    run_extraction_pass(data_dir, memory_store, provider, knobs).await
}

/// Status snapshot for `octos memory status`.
pub async fn refresh_status(data_dir: &Path, memory_store: &Arc<MemoryStore>) -> String {
    let state = RefreshState::load(&data_dir.join("memory").join("refresh_state.json"));
    let lock_path = data_dir.join("memory").join(".refresh.lock");
    let lock_holder = match acquire_refresh_lock(data_dir) {
        Ok(Some(_lock)) => "not running (lock free)".to_string(),
        Ok(None) => format!(
            "running — {}",
            std::fs::read_to_string(&lock_path)
                .unwrap_or_default()
                .trim()
                .replace('\n', ", ")
        ),
        Err(e) => format!("unknown ({e})"),
    };
    format!(
        "sweep: {}\npending notes: {}\npending extractions: {}\nbudget {}: {} extractions, {} tokens\ntracked sessions: {}",
        lock_holder,
        memory_store.count_staging_notes().await,
        memory_store.count_staging_extractions().await,
        state.date,
        state.extractions_today,
        state.tokens_today,
        state.watermarks.len(),
    )
}

/// One extraction pass: discover eligible idle sessions, extract at most
/// `max_sessions_per_pass`, write staging artifacts, advance watermarks.
pub(crate) async fn run_extraction_pass(
    data_dir: &Path,
    memory_store: &Arc<MemoryStore>,
    provider: &dyn LlmProvider,
    knobs: &RefreshKnobs,
) -> Result<PassReport> {
    let mut report = PassReport::default();
    let state_path = data_dir.join("memory").join("refresh_state.json");
    let mut state = RefreshState::load(&state_path);
    state.roll_date(&chrono::Local::now().format("%Y-%m-%d").to_string());

    if state.extractions_today >= knobs.max_extractions_per_day
        || state.tokens_today >= knobs.max_daily_tokens
    {
        report.skipped_budget = true;
        state.save(&state_path)?;
        return Ok(report);
    }

    let manager = SessionManager::open(data_dir).wrap_err("failed to open session manager")?;
    let now = SystemTime::now();

    // Eligible = user-facing, idle long enough, young enough, and changed
    // since the snapshots we last read.
    let mut candidates: Vec<(octos_bus::AnalysisSession, Vec<FileSnap>, SystemTime)> = Vec::new();
    for session in manager.list_for_analysis() {
        if session.internal || session.files.is_empty() {
            continue;
        }
        if state
            .failures
            .get(&session.key.0)
            .is_some_and(|f| *f >= MAX_SESSION_FAILURES)
        {
            continue;
        }
        let newest = session
            .files
            .iter()
            .map(|f| f.modified)
            .max()
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let idle = now.duration_since(newest).unwrap_or_default();
        if idle < knobs.min_idle || idle > knobs.max_session_age {
            continue;
        }
        let snaps: Vec<FileSnap> = session
            .files
            .iter()
            .map(|f| FileSnap::of(&f.path, f.modified, f.len))
            .collect();
        if state.watermarks.get(&session.key.0) == Some(&snaps) {
            continue;
        }
        candidates.push((session, snaps, newest));
    }
    report.candidates = candidates.len();
    // Oldest-first so a backlog drains deterministically.
    candidates.sort_by_key(|(_, _, newest)| *newest);
    candidates.truncate(knobs.max_sessions_per_pass);

    for (session, snaps, newest) in candidates {
        if state.extractions_today >= knobs.max_extractions_per_day
            || state.tokens_today >= knobs.max_daily_tokens
        {
            report.skipped_budget = true;
            break;
        }
        let key = session.key.clone();
        let outcome = extract_one_session(
            &manager,
            memory_store,
            provider,
            knobs,
            &key,
            newest,
            &mut state,
        )
        .await;
        match outcome {
            Ok(wrote) => {
                state.failures.remove(&key.0);
                state.extractions_today += 1;
                if wrote {
                    report.extracted += 1;
                }
                // Advance the watermark ONLY if the files did not change
                // while we were reading them (pre-read snapshot rule).
                if snapshots_current(&snaps) {
                    state.watermarks.insert(key.0.clone(), snaps);
                }
            }
            Err(e) => {
                *state.failures.entry(key.0.clone()).or_default() += 1;
                state.save(&state_path)?;
                return Err(e.wrap_err(format!("extraction failed for session {}", key.0)));
            }
        }
        state.save(&state_path)?;
    }

    state.save(&state_path)?;
    Ok(report)
}

/// Re-stat every snapshot file; true when nothing changed since pre-read.
fn snapshots_current(snaps: &[FileSnap]) -> bool {
    snaps.iter().all(|snap| {
        std::fs::metadata(&snap.path)
            .and_then(|m| m.modified().map(|t| (t, m.len())))
            .map(|(modified, len)| FileSnap::of(&snap.path, modified, len) == *snap)
            .unwrap_or(false)
    })
}

/// Extract one session; returns Ok(true) when a staging artifact was written.
async fn extract_one_session(
    manager: &SessionManager,
    memory_store: &Arc<MemoryStore>,
    provider: &dyn LlmProvider,
    knobs: &RefreshKnobs,
    key: &octos_core::SessionKey,
    newest: SystemTime,
    state: &mut RefreshState,
) -> Result<bool> {
    let Some(transcript) = manager.export_transcript(key).await else {
        // Unreadable/empty: nothing to extract; not an error.
        return Ok(false);
    };
    let lines = build_input_lines(&transcript);
    if lines.is_empty() {
        return Ok(false);
    }
    let rendered = render_transcript(
        &lines,
        knobs.max_extract_input_tokens,
        MAX_EXTRACT_INPUT_BYTES,
    );
    let current_memory = memory_store
        .get_injectable_context(knobs.max_inject_tokens)
        .await;
    let session_date: chrono::DateTime<chrono::Local> = newest.into();
    let user_prompt = format!(
        "CURRENT MEMORY (do not re-extract; flag contradictions as kind=correction):\n{}\n\n\
         TRANSCRIPT of session `{}` (last active {}):\n{}",
        if current_memory.is_empty() {
            "(empty)"
        } else {
            &current_memory
        },
        key.0,
        session_date.format("%Y-%m-%d"),
        rendered
    );

    let messages = vec![
        Message {
            role: MessageRole::System,
            content: EXTRACTION_SYSTEM_PROMPT.to_string(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        },
        Message {
            role: MessageRole::User,
            content: user_prompt.clone(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        },
    ];
    let config = ChatConfig {
        max_tokens: Some(2_000),
        ..Default::default()
    };
    let response = provider.chat(&messages, &[], &config).await?;
    let raw = response.content.unwrap_or_default();

    // Budget accounting: provider-reported usage, estimator fallback when
    // a provider reports zeros.
    let reported = (response.usage.input_tokens as u64) + (response.usage.output_tokens as u64);
    let spent = if reported > 0 {
        reported
    } else {
        (octos_memory::estimate_tokens(&user_prompt) + octos_memory::estimate_tokens(&raw)) as u64
    };
    state.tokens_today = state.tokens_today.saturating_add(spent);

    let parsed = parse_extraction_response(&raw)
        .map_err(|e| eyre::eyre!("extraction output was not valid JSON: {e}"))?;
    let items = validate_items(parsed, &lines, &session_date.format("%Y-%m-%d").to_string());
    if items.is_empty() {
        // The no-op gate firing is the expected common case.
        return Ok(false);
    }
    memory_store
        .write_staging_extraction(Some(&key.0), provider.model_id(), &items)
        .await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use octos_core::SessionKey;
    use octos_llm::{ChatResponse, StopReason, TokenUsage, ToolSpec};

    struct ScriptedProvider {
        response: String,
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl LlmProvider for ScriptedProvider {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _config: &ChatConfig,
        ) -> eyre::Result<ChatResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ChatResponse {
                content: Some(self.response.clone()),
                reasoning_content: None,
                tool_calls: vec![],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
                provider_index: None,
            })
        }
        fn model_id(&self) -> &str {
            "scripted-model"
        }
        fn provider_name(&self) -> &str {
            "scripted"
        }
    }

    fn knobs_for_test() -> RefreshKnobs {
        RefreshKnobs {
            min_idle: Duration::ZERO,
            max_session_age: Duration::from_secs(60 * 60 * 24 * 10),
            max_sessions_per_pass: 2,
            max_extractions_per_day: 20,
            max_daily_tokens: 200_000,
            interval: Duration::from_secs(1800),
            max_extract_input_tokens: 24_000,
            max_inject_tokens: 2_500,
        }
    }

    async fn seed_session(data_dir: &Path, key: &str, user_text: &str) {
        let mut mgr = SessionManager::open(data_dir).unwrap();
        let key = SessionKey(key.to_string());
        let mut msg = octos_core::Message {
            role: MessageRole::User,
            content: user_text.to_string(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: Some("m1".to_string()),
            thread_id: Some("m1".to_string()),
            timestamp: chrono::Utc::now(),
        };
        mgr.add_message(&key, msg.clone()).await.unwrap();
        msg.role = MessageRole::Assistant;
        msg.content = "ok".to_string();
        mgr.add_message(&key, msg).await.unwrap();
    }

    #[tokio::test]
    async fn should_write_extraction_and_advance_watermark_when_pass_runs() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(MemoryStore::open(dir.path()).await.unwrap());
        seed_session(
            dir.path(),
            "tg:100",
            "I live in Vancouver and prefer dark mode",
        )
        .await;

        let provider = ScriptedProvider {
            response:
                r#"{"items":[{"kind":"fact","content":"lives in Vancouver","evidence":[0]}]}"#
                    .to_string(),
            calls: std::sync::atomic::AtomicUsize::new(0),
        };
        let knobs = knobs_for_test();
        let report = run_extraction_pass(dir.path(), &store, &provider, &knobs)
            .await
            .unwrap();
        assert_eq!(report.extracted, 1);
        assert_eq!(store.count_staging_extractions().await, 1);

        // Second pass: watermark unchanged → no provider call, no new file.
        let calls_before = provider.calls.load(Ordering::SeqCst);
        let report2 = run_extraction_pass(dir.path(), &store, &provider, &knobs)
            .await
            .unwrap();
        assert_eq!(report2.candidates, 0);
        assert_eq!(provider.calls.load(Ordering::SeqCst), calls_before);
        assert_eq!(store.count_staging_extractions().await, 1);
    }

    #[tokio::test]
    async fn should_write_nothing_when_no_op_gate_fires() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(MemoryStore::open(dir.path()).await.unwrap());
        seed_session(dir.path(), "tg:101", "what time is it").await;

        let provider = ScriptedProvider {
            response: r#"{"items":[]}"#.to_string(),
            calls: std::sync::atomic::AtomicUsize::new(0),
        };
        let report = run_extraction_pass(dir.path(), &store, &provider, &knobs_for_test())
            .await
            .unwrap();
        assert_eq!(report.extracted, 0);
        assert_eq!(store.count_staging_extractions().await, 0);
        // Watermark still advances: an uninformative session isn't retried.
        let report2 = run_extraction_pass(dir.path(), &store, &provider, &knobs_for_test())
            .await
            .unwrap();
        assert_eq!(report2.candidates, 0);
    }

    #[tokio::test]
    async fn should_stop_when_daily_budget_exhausted() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(MemoryStore::open(dir.path()).await.unwrap());
        seed_session(dir.path(), "tg:102", "remember the deploy steps").await;

        let state_path = dir.path().join("memory").join("refresh_state.json");
        let mut state = RefreshState {
            date: chrono::Local::now().format("%Y-%m-%d").to_string(),
            extractions_today: 20,
            ..Default::default()
        };
        state.save(&state_path).unwrap();

        let provider = ScriptedProvider {
            response: r#"{"items":[]}"#.to_string(),
            calls: std::sync::atomic::AtomicUsize::new(0),
        };
        let report = run_extraction_pass(dir.path(), &store, &provider, &knobs_for_test())
            .await
            .unwrap();
        assert!(report.skipped_budget);
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);

        // Date rollover resets the budget.
        state.date = "2020-01-01".to_string();
        state.save(&state_path).unwrap();
        let report2 = run_extraction_pass(dir.path(), &store, &provider, &knobs_for_test())
            .await
            .unwrap();
        assert!(!report2.skipped_budget);
    }

    #[tokio::test]
    async fn should_skip_session_after_repeated_failures() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(MemoryStore::open(dir.path()).await.unwrap());
        seed_session(dir.path(), "tg:103", "poisoned session").await;

        let provider = ScriptedProvider {
            response: "NOT JSON AT ALL".to_string(),
            calls: std::sync::atomic::AtomicUsize::new(0),
        };
        let knobs = knobs_for_test();
        for _ in 0..MAX_SESSION_FAILURES {
            let err = run_extraction_pass(dir.path(), &store, &provider, &knobs).await;
            assert!(err.is_err(), "malformed output must surface as pass error");
        }
        // Fourth pass: the session is skipped, pass succeeds with no calls.
        let calls_before = provider.calls.load(Ordering::SeqCst);
        let report = run_extraction_pass(dir.path(), &store, &provider, &knobs)
            .await
            .unwrap();
        assert_eq!(report.candidates, 0);
        assert_eq!(provider.calls.load(Ordering::SeqCst), calls_before);
    }

    #[tokio::test]
    async fn should_deny_second_lock_holder_when_service_running() {
        let dir = tempfile::tempdir().unwrap();
        let first = acquire_refresh_lock(dir.path()).unwrap();
        assert!(first.is_some());
        let second = acquire_refresh_lock(dir.path()).unwrap();
        assert!(second.is_none(), "flock must be exclusive");
        drop(first);
        // Release rides the fd close; under a heavily parallel test run the
        // kernel-visible release can lag a beat — poll briefly rather than
        // flake, while still failing hard if the lock never frees.
        let mut third = None;
        for _ in 0..40 {
            third = acquire_refresh_lock(dir.path()).unwrap();
            if third.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(third.is_some(), "lock must release on drop");
    }

    #[test]
    fn should_grow_backoff_exponentially_with_cap() {
        assert_eq!(backoff_after(1), Duration::from_secs(300));
        assert_eq!(backoff_after(2), Duration::from_secs(600));
        assert_eq!(backoff_after(5), Duration::from_secs(4800));
        assert_eq!(backoff_after(9), Duration::from_secs(4800));
    }
}
