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

pub use octos_services::config::RefreshKnobs;

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
    #[serde(default)]
    pub consolidations_today: u32,
    /// Per session key: the file snapshots actually READ last time.
    #[serde(default)]
    pub watermarks: std::collections::BTreeMap<String, Vec<FileSnap>>,
    /// Per session key: (transcript messages CONSUMED, fingerprint of that
    /// consumed prefix). Later sweeps feed only the delta — re-reading
    /// history would re-learn facts the user has since hard-deleted (they
    /// are gone from CURRENT MEMORY, so the "do not re-extract"
    /// instruction cannot see them) and would re-spend tokens on every
    /// old turn. The FINGERPRINT (not file bytes) detects rewrites:
    /// rollback-and-continue can grow the file while replacing folded
    /// messages below the cursor — any prefix change resets it.
    #[serde(default)]
    pub extracted_counts: std::collections::BTreeMap<String, (usize, String)>,
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
            self.consolidations_today = 0;
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
    task: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Drop for MemoryRefreshService {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(task) = self
            .task
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            task.abort();
        }
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
        consolidate_provider: Arc<dyn LlmProvider>,
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
            // The lock guard lives here for the task's lifetime.
            let _lock = lock_file;
            let mut consecutive_failures: u32 = 0;
            let mut backoff_until: Option<tokio::time::Instant> = None;
            let mut ticker = tokio::time::interval(knobs.interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // Fast lane: host / user_request notes deserve minutes-scale
            // consolidation, not the next main tick.
            let mut fast_ticker = tokio::time::interval(knobs.debounce.max(Duration::from_secs(5)));
            fast_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                let full_pass = tokio::select! {
                    _ = ticker.tick() => true,
                    _ = fast_ticker.tick() => false,
                };
                if stop.load(Ordering::Acquire) {
                    break;
                }
                let priority = !full_pass && has_priority_note(&data_dir);
                if let Some(until) = backoff_until {
                    if tokio::time::Instant::now() < until {
                        // Backoff throttles the failing EXTRACTION path; a
                        // host remember/forget written meanwhile still gets
                        // its consolidation-only fast lane.
                        if !priority {
                            continue;
                        }
                    } else {
                        backoff_until = None;
                    }
                }
                if !full_pass && !priority {
                    continue;
                }
                let full_pass = full_pass && backoff_until.is_none();
                let pass = async {
                    // Consolidation must run even when extraction fails —
                    // staged remember/forget notes may not wait behind an
                    // unrelated broken session. First error wins the report.
                    let extract_result = if full_pass {
                        run_extraction_pass(&data_dir, &memory_store, provider.as_ref(), &knobs)
                            .await
                            .map(|report| {
                                if report.extracted > 0 || report.skipped_budget {
                                    tracing::info!(
                                        extracted = report.extracted,
                                        candidates = report.candidates,
                                        skipped_budget = report.skipped_budget,
                                        "memory extraction pass complete"
                                    );
                                }
                            })
                    } else {
                        Ok(())
                    };
                    let consolidate_result =
                        run_consolidation_pass(&data_dir, consolidate_provider.clone(), &knobs)
                            .await;
                    extract_result?;
                    consolidate_result
                };
                match pass.await {
                    Ok(()) => consecutive_failures = 0,
                    Err(e) => {
                        consecutive_failures += 1;
                        let wait = backoff_after(consecutive_failures);
                        backoff_until = Some(tokio::time::Instant::now() + wait);
                        tracing::warn!(
                            failures = consecutive_failures,
                            backoff_secs = wait.as_secs(),
                            "memory refresh pass failed: {e:#}"
                        );
                    }
                }
            }
        });
        Some(Self {
            shutdown,
            task: std::sync::Mutex::new(Some(task)),
        })
    }

    /// Stop the sweep and wait until its task has released the profile lock,
    /// so a replacement runtime that shares this profile's stores can take
    /// ownership immediately. Idempotent.
    pub async fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
        let task = self
            .task
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(task) = task {
            task.abort();
            let _ = task.await;
        }
    }
}

/// Release ownership explicitly when the sweep stops. A concurrently forked
/// child can retain this open file description until exec, so closing only
/// our descriptor would leave flock held and make an immediate replacement
/// skip the refresh service. Unlocking releases that shared description's
/// lock even while the child still has its copy.
struct RefreshLock {
    file: std::fs::File,
}

impl Drop for RefreshLock {
    fn drop(&mut self) {
        if let Err(error) = FileExt::unlock(&self.file) {
            tracing::warn!(%error, "failed to release memory refresh lock");
        }
    }
}

/// Retry budget for [`acquire_refresh_lock`]: a transient lock-setup
/// failure (a loaded host can fail `open`/`flock` with a one-off error —
/// the #2684 flake) must not permanently skip the sweep for a runtime's
/// lifetime, so retry briefly before surfacing the error. Genuine
/// contention never reaches the retry — it is `Ok(None)`, the by-design
/// loser path.
const LOCK_SETUP_ATTEMPTS: usize = 3;
const LOCK_SETUP_RETRY_DELAY: Duration = Duration::from_millis(150);

fn acquire_refresh_lock(data_dir: &Path) -> Result<Option<RefreshLock>> {
    let mut attempt = 1;
    loop {
        match acquire_refresh_lock_once(data_dir) {
            Ok(lock) => return Ok(lock),
            Err(e) if attempt < LOCK_SETUP_ATTEMPTS => {
                tracing::warn!(
                    error = %e,
                    attempt,
                    "memory refresh lock setup failed; retrying"
                );
                attempt += 1;
                // Failure path only: at most `LOCK_SETUP_ATTEMPTS - 1`
                // sleeps (~300ms total) block the caller's thread. Every
                // caller is a cold path — `try_start` at serve/gateway
                // bootstrap or the `octos memory refresh/status`
                // commands — never a hot loop.
                std::thread::sleep(LOCK_SETUP_RETRY_DELAY);
            }
            Err(e) => return Err(e),
        }
    }
}

fn acquire_refresh_lock_once(data_dir: &Path) -> Result<Option<RefreshLock>> {
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
            Ok(Some(RefreshLock { file }))
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
    consolidate_provider: Arc<dyn LlmProvider>,
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
    // Consolidation runs even when extraction fails — staged host notes
    // must apply regardless; the extraction error is surfaced after.
    let extract_result = run_extraction_pass(data_dir, memory_store, provider, knobs).await;
    let consolidate_result = run_consolidation_pass(data_dir, consolidate_provider, knobs).await;
    let report = extract_result?;
    consolidate_result?;
    Ok(report)
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
        "sweep: {}\npending notes: {}\npending extractions: {}\nbudget {}: {} extractions, {} tokens, {} consolidations\ntracked sessions: {}",
        lock_holder,
        memory_store.count_staging_notes().await,
        memory_store.count_staging_extractions().await,
        state.date,
        state.extractions_today,
        state.tokens_today,
        state.consolidations_today,
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

    let manager = SessionManager::open(data_dir).wrap_err("failed to open session manager")?;
    let now = SystemTime::now();
    // UPCR-2026-034 — a host-owned app peer (or one of its request contexts)
    // captures only into its own memory namespace: the profile-level sweep
    // must never read its transcript into the profile's memory.
    let peers_root = data_dir.join("peers");
    let app_bound = |key: &octos_core::SessionKey| {
        crate::peers::app_binding::resolve_session_app_binding(&peers_root, key)
            .is_bound_or_refused()
    };

    // Pre-upgrade cursor backfill — INDEPENDENT of extraction eligibility
    // (idle windows, age caps, budgets): a watermarked session without a
    // cursor was fully consumed by a pre-cursor sweep; stamp it before
    // anything can make the session a candidate, or its next append would
    // fall back to prior=0 and re-feed the whole history. Sessions whose
    // files already changed since the old watermark accept a one-time
    // full re-read (we cannot know the consumed prefix).
    for session in manager.list_for_analysis() {
        if session.internal || session.files.is_empty() || app_bound(&session.key) {
            continue;
        }
        if state.extracted_counts.contains_key(&session.key.0) {
            continue;
        }
        let snaps: Vec<FileSnap> = session
            .files
            .iter()
            .map(|f| FileSnap::of(&f.path, f.modified, f.len))
            .collect();
        if state.watermarks.get(&session.key.0) != Some(&snaps) {
            continue;
        }
        if let Some(transcript) = manager.export_transcript(&session.key).await {
            // Pre-read snapshot rule: a turn appended DURING this read
            // would be stamped consumed without extraction.
            if snapshots_current(&snaps) {
                let fp = transcript_prefix_fingerprint(&transcript, transcript.len());
                state
                    .extracted_counts
                    .insert(session.key.0.clone(), (transcript.len(), fp));
                state.save(&state_path)?;
            }
        }
    }

    // The budget gates PROVIDER work only — the no-provider backfill above
    // must run even on capped passes, or an append before the budget reset
    // would re-feed the whole pre-upgrade history.
    if state.extractions_today >= knobs.max_extractions_per_day
        || state.tokens_today >= knobs.max_daily_tokens
    {
        report.skipped_budget = true;
        state.save(&state_path)?;
        return Ok(report);
    }

    // Eligible = user-facing, idle long enough, young enough, and changed
    // since the snapshots we last read.

    let mut candidates: Vec<(octos_bus::AnalysisSession, Vec<FileSnap>, SystemTime)> = Vec::new();
    for session in manager.list_for_analysis() {
        if session.internal || session.files.is_empty() || app_bound(&session.key) {
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
            Ok(outcome) => {
                state.failures.remove(&key.0);
                // The extraction-call budget counts PROVIDER calls; empty
                // deltas (filtered-only changes) are free.
                if outcome.called_provider {
                    state.extractions_today += 1;
                }
                if outcome.wrote {
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

/// Cheap scan: does staging hold a host-authored or user_request note?
/// (Fast-lane trigger; reads at most the first 512 bytes per note.)
pub(crate) fn has_priority_note(data_dir: &Path) -> bool {
    let notes_dir = data_dir.join("memory").join("staging").join("notes");
    let Ok(entries) = std::fs::read_dir(&notes_dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "md") {
            continue;
        }
        let Ok(file) = std::fs::File::open(&path) else {
            continue;
        };
        use std::io::Read;
        let mut head = String::new();
        // 16KB covers the whole frontmatter even when a parked note's
        // `candidates:` JSON is long (it precedes `expires_at:` in the
        // render order — a short prefix would misread parked notes as
        // fast-lane triggers and spin every debounce).
        if file.take(16 * 1024).read_to_string(&mut head).is_err() {
            continue;
        }
        // Only the fenced FRONTMATTER counts — untrusted note bodies could
        // contain these literals and must not steer the fast lane.
        let Some(fm) = frontmatter_of(&head) else {
            continue;
        };
        // Already-parked pending-confirm notes wait on a HUMAN, not on the
        // fast lane — re-running every debounce would only spin.
        if fm.contains("expires_at:") || fm.contains("candidates:") {
            continue;
        }
        if fm.lines().any(|l| l.trim() == "origin: host")
            || fm.lines().any(|l| l.trim() == "kind: user_request")
        {
            return true;
        }
    }
    false
}

/// The fenced frontmatter region of a staging note, when present.
fn frontmatter_of(content: &str) -> Option<&str> {
    let after = content.strip_prefix("---")?;
    let rest = after
        .strip_prefix("\r\n")
        .or_else(|| after.strip_prefix('\n'))?;
    let end = rest.find("\n---")?;
    Some(&rest[..end])
}

/// One consolidation pass: budget-gated engine run + quarantine mover +
/// pending/error surfacing. Token spend and run counts share the same
/// daily state as extraction.
pub(crate) async fn run_consolidation_pass(
    data_dir: &Path,
    provider: Arc<dyn LlmProvider>,
    knobs: &RefreshKnobs,
) -> Result<()> {
    let state_path = data_dir.join("memory").join("refresh_state.json");
    let mut state = RefreshState::load(&state_path);
    state.roll_date(&chrono::Local::now().format("%Y-%m-%d").to_string());
    // An exhausted budget disables the MERGE, not the whole engine: the
    // no-provider phases (pending expiry, satisfied-forget consumption,
    // crash-recovery re-hide) must keep running or pending_confirm_days
    // would not be honored on busy profiles.
    let allow_merge = state.consolidations_today < knobs.max_consolidations_per_day
        && state.tokens_today < knobs.max_daily_tokens;

    let mut params = crate::memory_consolidate::ConsolidateParams::new(data_dir.join("memory"));
    params.max_memory_file_tokens = knobs.max_memory_file_tokens;
    params.unused_days = knobs.unused_days;
    params.pending_confirm_days = knobs.pending_confirm_days;
    params.allow_merge = allow_merge;

    // Pre-capture the staging payload size: a successful merge deletes
    // consumed files, so a post-hoc scan would under-charge zero-usage
    // providers for the prompt's staging content.
    let mut staging_bytes_before: u64 = 0;
    for dir in ["notes", "extract"] {
        if let Ok(entries) = std::fs::read_dir(data_dir.join("memory/staging").join(dir)) {
            for entry in entries.flatten() {
                if let Ok(meta) = entry.metadata() {
                    staging_bytes_before += meta.len();
                }
            }
        }
    }

    let outcome = crate::memory_consolidate::run_consolidation(provider, &params).await?;

    if outcome.skipped_clean {
        return Ok(());
    }
    // Charge budgets only for ACTUAL work (provider spend or an applied
    // merge/INIT). A parked pending-confirm note keeps returning
    // `skipped_clean == false` purely to stay surfaced; charging those
    // no-op checks would drain the daily cap and block the eventual
    // confirmation.
    let mut spent =
        (outcome.token_usage.input_tokens as u64) + (outcome.token_usage.output_tokens as u64);
    if outcome.provider_attempts > 0 && spent == 0 {
        // Estimator fallback (parity with extraction): some providers
        // report zero usage; a merge call still spent real tokens —
        // roughly the memory file (in the prompt AND regenerated in the
        // reply) plus the staging payload.
        let memory_md =
            std::fs::read_to_string(data_dir.join("memory").join("MEMORY.md")).unwrap_or_default();
        let estimate =
            2 * octos_memory::estimate_tokens(&memory_md) as u64 + staging_bytes_before / 4;
        spent = estimate.max(1);
    }
    // Failed/rejected merges spent provider calls too — charging them is
    // what stops a bad staged note from retrying every tick for free.
    let did_work = outcome.merge_applied
        || outcome.init_performed
        || outcome.provider_attempts > 0
        || spent > 0;
    if did_work {
        state.consolidations_today += 1;
        state.tokens_today = state.tokens_today.saturating_add(spent);
    }

    // Quarantine mover: the engine only signals; the service relocates so
    // repeat offenders leave the batch.
    if !outcome.quarantine_candidates.is_empty() {
        let quarantine_dir = data_dir.join("memory").join("staging").join("quarantine");
        let _ = std::fs::create_dir_all(&quarantine_dir);
        for path in &outcome.quarantine_candidates {
            if let Some(name) = path.file_name() {
                match std::fs::rename(path, quarantine_dir.join(name)) {
                    Ok(()) => tracing::warn!(file = %path.display(), "staging file quarantined"),
                    Err(e) => {
                        tracing::warn!(file = %path.display(), "failed to quarantine: {e}");
                    }
                }
            }
        }
    }

    for pending in &outcome.pending_notes {
        tracing::info!(?pending, "memory forget request pending confirmation");
    }
    for err in &outcome.errors {
        tracing::warn!("memory consolidation reported: {err}");
    }
    if outcome.merge_applied || outcome.init_performed {
        tracing::info!(
            init = outcome.init_performed,
            added = outcome.added.len(),
            updated = outcome.updated.len(),
            superseded = outcome.superseded.len(),
            archived = outcome.archived.len(),
            hard_deleted = outcome.hard_deleted.len(),
            consumed = outcome.consumed_staging_files,
            "memory consolidation applied"
        );
    }
    state.save(&state_path)?;
    Ok(())
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
/// What one session's extraction attempt did — the CALLER charges the
/// daily extraction budget only when a provider call actually happened
/// (empty deltas and unreadable sessions are free).
#[derive(Debug, Clone, Copy, Default)]
struct ExtractOutcome {
    called_provider: bool,
    wrote: bool,
}

/// Stable fingerprint of the first `n` transcript messages — the delta
/// cursor's validity check. Any change below the cursor resets it.
fn transcript_prefix_fingerprint(transcript: &[(usize, Message)], n: usize) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    for (_, msg) in &transcript[..n.min(transcript.len())] {
        // Every field build_input_lines consumes: rewrites that change
        // only tool linkage (rollback/migration) must invalidate the
        // cursor too, or a changed sanitized prefix would be skipped.
        hasher.update(msg.role.as_str().as_bytes());
        hasher.update([0u8]);
        hasher.update(msg.content.as_bytes());
        hasher.update([0u8]);
        if let Some(id) = &msg.tool_call_id {
            hasher.update(id.as_bytes());
        }
        hasher.update([0u8]);
        if let Some(calls) = &msg.tool_calls {
            for call in calls {
                hasher.update(call.id.as_bytes());
                hasher.update([1u8]);
                hasher.update(call.name.as_bytes());
                hasher.update([1u8]);
                hasher.update(call.arguments.to_string().as_bytes());
                hasher.update([1u8]);
            }
        }
        hasher.update([0x1e]);
    }
    format!("{:x}", hasher.finalize())
}

async fn extract_one_session(
    manager: &SessionManager,
    memory_store: &Arc<MemoryStore>,
    provider: &dyn LlmProvider,
    knobs: &RefreshKnobs,
    key: &octos_core::SessionKey,
    newest: SystemTime,
    state: &mut RefreshState,
) -> Result<ExtractOutcome> {
    let Some(transcript) = manager.export_transcript(key).await else {
        // Unreadable/empty: nothing to extract; not an error.
        return Ok(ExtractOutcome::default());
    };
    // Delta extraction: skip messages consumed by earlier sweeps. The
    // cursor is valid only when the consumed PREFIX is byte-identical —
    // fork/truncation/rollback-and-continue all change it and reset the
    // cursor to a full re-read.
    let prior = state
        .extracted_counts
        .get(&key.0)
        .filter(|(n, fp)| {
            *n <= transcript.len() && *fp == transcript_prefix_fingerprint(&transcript, *n)
        })
        .map(|(n, _)| *n)
        .unwrap_or(0);
    // Build lines over the FULL transcript (tool-call names for result
    // filtering can live in already-consumed messages), then keep only
    // the fresh indices. Labels keep original indices, so evidence
    // validation is unaffected.
    let lines: Vec<_> = build_input_lines(&transcript)
        .into_iter()
        .filter(|l| l.idx >= prior)
        .collect();
    if lines.is_empty() {
        // Nothing extractable in the delta; remember we consumed it.
        state.extracted_counts.insert(
            key.0.clone(),
            (
                transcript.len(),
                transcript_prefix_fingerprint(&transcript, transcript.len()),
            ),
        );
        return Ok(ExtractOutcome::default());
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
    // The session key is attacker-controlled channel metadata (email
    // sender/topic, API session ids) and would enter the provider prompt
    // OUTSIDE transcript redaction — instructions there could induce
    // regex-clean, user_said-authorized items (codex round-4 P1). The
    // model doesn't need it; the real key rides only the scanned
    // artifact frontmatter.
    let user_prompt = format!(
        "CURRENT MEMORY (do not re-extract; flag contradictions as kind=correction):\n{}\n\n\
         TRANSCRIPT of one session (last active {}):\n{}",
        if current_memory.is_empty() {
            "(empty)"
        } else {
            &current_memory
        },
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
            source: None,
            participant: None,
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
            source: None,
            participant: None,
        },
    ];
    let config = ChatConfig {
        max_tokens: Some(2_000),
        // #2194 review: one extraction call per session transcript — the
        // prompt embeds that session's rendered transcript and is never
        // replayed, so a cache write is pure premium. (The consolidation
        // pass is different: its corrective re-ask APPENDS to the original
        // messages, genuinely reusing the prefix, so it keeps caching.)
        cache_retention: octos_llm::CacheRetention::None,
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
        // The no-op gate firing is the expected common case — the delta
        // was consumed (nothing worth staging), so advance the cursor or
        // these turns would be re-fed and re-charged every later sweep.
        state.extracted_counts.insert(
            key.0.clone(),
            (
                transcript.len(),
                transcript_prefix_fingerprint(&transcript, transcript.len()),
            ),
        );
        return Ok(ExtractOutcome {
            called_provider: true,
            wrote: false,
        });
    }
    let wrote = memory_store
        .write_staging_extraction(Some(&key.0), provider.model_id(), &items)
        .await?
        .is_some();
    // `None` means the content guard dropped every item — count it like an
    // empty extraction (wrote: false) but still advance the cursor below:
    // re-extracting the same poisoned turns would loop forever.
    // Only now is the extraction durable — advancing earlier would let a
    // failed write mark these turns consumed and the retry would skip
    // them forever.
    state.extracted_counts.insert(
        key.0.clone(),
        (
            transcript.len(),
            transcript_prefix_fingerprint(&transcript, transcript.len()),
        ),
    );
    Ok(ExtractOutcome {
        called_provider: true,
        wrote,
    })
}
