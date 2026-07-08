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
    /// Daily consolidation-run budget per profile.
    pub max_consolidations_per_day: u32,
    /// Fast-lane cadence: host / user_request notes trigger a
    /// consolidation at this interval instead of waiting for the main tick.
    pub debounce: Duration,
    /// Durable MEMORY.md size cap enforced by the consolidator.
    pub max_memory_file_tokens: usize,
    /// Auto-archive age for really-stamped entries.
    pub unused_days: u32,
    /// Pending-confirm forget lifetime.
    pub pending_confirm_days: u32,
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
            // The lock fd lives here for the task's lifetime.
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

    // Pre-upgrade cursor backfill — INDEPENDENT of extraction eligibility
    // (idle windows, age caps, budgets): a watermarked session without a
    // cursor was fully consumed by a pre-cursor sweep; stamp it before
    // anything can make the session a candidate, or its next append would
    // fall back to prior=0 and re-feed the whole history. Sessions whose
    // files already changed since the old watermark accept a one-time
    // full re-read (we cannot know the consumed prefix).
    for session in manager.list_for_analysis() {
        if session.internal || session.files.is_empty() {
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
) -> Result<bool> {
    let Some(transcript) = manager.export_transcript(key).await else {
        // Unreadable/empty: nothing to extract; not an error.
        return Ok(false);
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
        return Ok(false);
    }
    memory_store
        .write_staging_extraction(Some(&key.0), provider.model_id(), &items)
        .await?;
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
        prompts: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl LlmProvider for ScriptedProvider {
        async fn chat(
            &self,
            messages: &[Message],
            _tools: &[ToolSpec],
            _config: &ChatConfig,
        ) -> eyre::Result<ChatResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.prompts
                .lock()
                .unwrap()
                .extend(messages.iter().map(|m| m.content.clone()));
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
            max_consolidations_per_day: 12,
            debounce: Duration::from_secs(90),
            max_memory_file_tokens: 8_000,
            unused_days: 30,
            pending_confirm_days: 7,
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
            prompts: std::sync::Mutex::new(Vec::new()),
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
            prompts: std::sync::Mutex::new(Vec::new()),
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
            prompts: std::sync::Mutex::new(Vec::new()),
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
            prompts: std::sync::Mutex::new(Vec::new()),
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

    struct OpsProvider {
        response: String,
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl LlmProvider for OpsProvider {
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
            "ops-model"
        }
        fn provider_name(&self) -> &str {
            "scripted"
        }
    }

    #[tokio::test]
    async fn should_consolidate_extraction_into_memory_md_when_full_pipeline_runs() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(MemoryStore::open(dir.path()).await.unwrap());
        seed_session(
            dir.path(),
            "tg:900",
            "I live in Vancouver and prefer dark mode",
        )
        .await;

        // Extraction provider proposes one fact; consolidation provider
        // turns it into an add op consuming the extraction item.
        let extract = ScriptedProvider {
            response:
                r#"{"items":[{"kind":"fact","content":"lives in Vancouver","evidence":[0]}]}"#
                    .to_string(),
            calls: std::sync::atomic::AtomicUsize::new(0),
            prompts: std::sync::Mutex::new(Vec::new()),
        };
        let knobs = knobs_for_test();
        let report = run_extraction_pass(dir.path(), &store, &extract, &knobs)
            .await
            .unwrap();
        assert_eq!(report.extracted, 1);

        // The engine addresses staging items by <file-stem>#<index>.
        let extract_dir = dir.path().join("memory/staging/extract");
        let mut entries = std::fs::read_dir(&extract_dir).unwrap();
        let stem = entries
            .next()
            .unwrap()
            .unwrap()
            .path()
            .file_stem()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let ops = OpsProvider {
            response: format!(
                r#"{{"ops":[{{"op":"add","section":null,"text":"Lives in Vancouver (updated: 2026-07-08)","sources":["{stem}#0"]}}],"consumed_ids":["{stem}#0"],"dropped":[]}}"#
            ),
            calls: std::sync::atomic::AtomicUsize::new(0),
        };
        run_consolidation_pass(dir.path(), Arc::new(ops), &knobs)
            .await
            .unwrap();

        let memory_md =
            std::fs::read_to_string(dir.path().join("memory/MEMORY.md")).unwrap_or_default();
        assert!(
            memory_md.contains("Lives in Vancouver"),
            "consolidation must land in MEMORY.md: {memory_md}"
        );
        assert_eq!(
            store.count_staging_extractions().await,
            0,
            "consumed staging must be deleted"
        );
        // Budgets accounted.
        let state = RefreshState::load(&dir.path().join("memory").join("refresh_state.json"));
        assert_eq!(state.consolidations_today, 1);
    }

    #[tokio::test]
    async fn should_skip_consolidation_when_daily_cap_reached() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(MemoryStore::open(dir.path()).await.unwrap());
        // A pending note exists, but the cap is exhausted.
        store
            .write_staging_note(&octos_memory::StagingNote {
                origin: octos_memory::NoteOrigin::Model,
                kind: octos_memory::NoteKind::Fact,
                content: "some fact".to_string(),
                session_key: None,
                sensitive: false,
                replaces_id: None,
            })
            .await
            .unwrap();
        let state_path = dir.path().join("memory").join("refresh_state.json");
        RefreshState {
            date: chrono::Local::now().format("%Y-%m-%d").to_string(),
            consolidations_today: 12,
            ..Default::default()
        }
        .save(&state_path)
        .unwrap();

        let ops = OpsProvider {
            response: "{}".to_string(),
            calls: std::sync::atomic::AtomicUsize::new(0),
        };
        let ops = Arc::new(ops);
        run_consolidation_pass(dir.path(), ops.clone(), &knobs_for_test())
            .await
            .unwrap();
        assert_eq!(
            ops.calls.load(Ordering::SeqCst),
            0,
            "budget-capped pass must not call the provider"
        );
    }

    #[tokio::test]
    async fn should_detect_priority_notes_for_fast_lane() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(MemoryStore::open(dir.path()).await.unwrap());
        assert!(!has_priority_note(dir.path()));

        store
            .write_staging_note(&octos_memory::StagingNote {
                origin: octos_memory::NoteOrigin::Model,
                kind: octos_memory::NoteKind::Fact,
                content: "ordinary fact".to_string(),
                session_key: None,
                sensitive: false,
                replaces_id: None,
            })
            .await
            .unwrap();
        assert!(
            !has_priority_note(dir.path()),
            "model facts are not fast-lane"
        );

        store
            .write_staging_note(&octos_memory::StagingNote {
                origin: octos_memory::NoteOrigin::Host,
                kind: octos_memory::NoteKind::Forget,
                content: "forget my old address".to_string(),
                session_key: None,
                sensitive: false,
                replaces_id: None,
            })
            .await
            .unwrap();
        assert!(has_priority_note(dir.path()), "host notes are fast-lane");
    }

    #[tokio::test]
    async fn should_not_charge_budget_for_pending_only_checks() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(MemoryStore::open(dir.path()).await.unwrap());
        // A free-text host forget with nothing to bind to: the engine parks
        // it (or leaves it pending) without provider work.
        store
            .write_staging_note(&octos_memory::StagingNote {
                origin: octos_memory::NoteOrigin::Host,
                kind: octos_memory::NoteKind::Forget,
                content: "forget something that matches no entry".to_string(),
                session_key: None,
                sensitive: false,
                replaces_id: None,
            })
            .await
            .unwrap();

        let ops = Arc::new(OpsProvider {
            response: r#"{"ops":[],"consumed_ids":[],"dropped":[]}"#.to_string(),
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let knobs = knobs_for_test();
        // Several fast-lane style re-checks.
        for _ in 0..5 {
            run_consolidation_pass(dir.path(), ops.clone(), &knobs)
                .await
                .unwrap();
        }
        let state = RefreshState::load(&dir.path().join("memory").join("refresh_state.json"));
        assert!(
            state.consolidations_today <= 1,
            "pending-only checks must not drain the daily cap (got {})",
            state.consolidations_today
        );
    }

    #[tokio::test]
    async fn should_not_fast_lane_already_parked_pending_notes() {
        let dir = tempfile::tempdir().unwrap();
        let notes_dir = dir.path().join("memory/staging/notes");
        std::fs::create_dir_all(&notes_dir).unwrap();
        // A parked pending-confirm note: host forget already stamped with
        // candidates + expires_at by the engine.
        std::fs::write(
            notes_dir.join("0abc-parked.md"),
            "---\norigin: host\nkind: forget\ncreated_at: 2026-07-08T00:00:00Z\ncandidates: []\nexpires_at: 2026-07-15T00:00:00Z\n---\n\nforget my old address\n",
        )
        .unwrap();
        assert!(
            !has_priority_note(dir.path()),
            "parked notes wait on a human, not the fast lane"
        );
    }

    #[tokio::test]
    async fn should_honor_pending_expiry_when_budget_exhausted() {
        let dir = tempfile::tempdir().unwrap();
        let memory_dir = dir.path().join("memory");
        std::fs::create_dir_all(memory_dir.join("staging/notes")).unwrap();
        std::fs::create_dir_all(memory_dir.join("archive")).unwrap();
        // An interim-archived candidate whose pending note expired long ago.
        let secret = "Sensitive detail. (updated: 2026-01-01) ^msenstv";
        std::fs::write(
            memory_dir.join("MEMORY.md"),
            "Keeps bonsai. (updated: 2026-01-02) ^mcccccc\n",
        )
        .unwrap();
        std::fs::write(memory_dir.join("archive/2026-01.md"), format!("{secret}\n")).unwrap();
        let hash = {
            use sha2::{Digest, Sha256};
            let mut h = Sha256::new();
            h.update(secret.as_bytes());
            format!("{:x}", h.finalize())
        };
        std::fs::write(
            memory_dir.join("staging/notes/01fg-expired.md"),
            format!(
                "---\norigin: host\nkind: forget\ncreated_at: 2026-01-01T00:00:00+00:00\nsensitive: true\ncandidates: [{{\"entry_id\":\"^msenstv\",\"content_hash\":\"{hash}\",\"interim_archived\":true}}]\nexpires_at: 2026-01-08T00:00:00+00:00\n---\n\nforget the sensitive detail\n"
            ),
        )
        .unwrap();
        // Budget exhausted.
        RefreshState {
            date: chrono::Local::now().format("%Y-%m-%d").to_string(),
            consolidations_today: 12,
            ..Default::default()
        }
        .save(&dir.path().join("memory").join("refresh_state.json"))
        .unwrap();

        let ops = Arc::new(OpsProvider {
            response: "{}".to_string(),
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        run_consolidation_pass(dir.path(), ops.clone(), &knobs_for_test())
            .await
            .unwrap();

        assert_eq!(
            ops.calls.load(Ordering::SeqCst),
            0,
            "no provider spend over budget"
        );
        assert!(
            !memory_dir.join("staging/notes/01fg-expired.md").exists(),
            "expired pending note must be processed despite the budget"
        );
        let memory = std::fs::read_to_string(memory_dir.join("MEMORY.md")).unwrap();
        assert!(
            memory.contains("^msenstv"),
            "expiry must restore the interim-archived candidate: {memory}"
        );
    }

    #[tokio::test]
    async fn should_not_fast_lane_when_expires_at_beyond_short_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let notes_dir = dir.path().join("memory/staging/notes");
        std::fs::create_dir_all(&notes_dir).unwrap();
        // Long candidates JSON pushes expires_at past a 512-byte prefix.
        let candidates: Vec<String> = (0..40)
            .map(|i| {
                format!(
                    r#"{{"entry_id":"^mcand{i:02}","content_hash":"{}","interim_archived":false}}"#,
                    "a".repeat(64)
                )
            })
            .collect();
        std::fs::write(
            notes_dir.join("0abc-parked-long.md"),
            format!(
                "---\norigin: host\nkind: forget\ncreated_at: 2026-07-08T00:00:00Z\ncandidates: [{}]\nexpires_at: 2026-07-15T00:00:00Z\n---\n\nforget a lot of things\n",
                candidates.join(",")
            ),
        )
        .unwrap();
        assert!(
            !has_priority_note(dir.path()),
            "a parked note with long candidate metadata must not spin the fast lane"
        );
    }

    #[tokio::test]
    async fn should_consolidate_staged_notes_when_extraction_fails() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(MemoryStore::open(dir.path()).await.unwrap());
        // A poisoned session makes extraction fail…
        seed_session(dir.path(), "tg:500", "broken").await;
        // …while a host remember note waits in staging.
        store
            .write_staging_note(&octos_memory::StagingNote {
                origin: octos_memory::NoteOrigin::Host,
                kind: octos_memory::NoteKind::UserRequest,
                content: "remember: the deploy password rotates monthly".to_string(),
                session_key: None,
                sensitive: false,
                replaces_id: None,
            })
            .await
            .unwrap();

        let extract = ScriptedProvider {
            response: "NOT JSON".to_string(),
            calls: std::sync::atomic::AtomicUsize::new(0),
            prompts: std::sync::Mutex::new(Vec::new()),
        };
        let notes_dir = dir.path().join("memory/staging/notes");
        let note_name = std::fs::read_dir(&notes_dir)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path()
            .file_stem()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let ops = Arc::new(OpsProvider {
            response: format!(
                r#"{{"ops":[{{"op":"add","section":null,"text":"Deploy password rotates monthly.","sources":["{note_name}"]}}],"consumed_ids":["{note_name}"],"dropped":[]}}"#
            ),
            calls: std::sync::atomic::AtomicUsize::new(0),
        });

        let knobs = knobs_for_test();
        let err = run_once(dir.path(), &store, &extract, ops.clone(), &knobs).await;
        assert!(err.is_err(), "extraction failure must still surface");
        assert!(
            ops.calls.load(Ordering::SeqCst) >= 1,
            "consolidation must run despite the extraction failure"
        );
        let memory =
            std::fs::read_to_string(dir.path().join("memory/MEMORY.md")).unwrap_or_default();
        assert!(
            memory.contains("Deploy password rotates monthly"),
            "the staged host note must be applied: {memory}"
        );
    }

    #[tokio::test]
    async fn should_charge_budget_when_merge_fails_with_zero_usage() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(MemoryStore::open(dir.path()).await.unwrap());
        // A model fact note makes the batch dirty; the ops provider returns
        // garbage (merge fails after the re-ask) with zero reported usage.
        store
            .write_staging_note(&octos_memory::StagingNote {
                origin: octos_memory::NoteOrigin::Model,
                kind: octos_memory::NoteKind::Fact,
                content: "bad batch".to_string(),
                session_key: None,
                sensitive: false,
                replaces_id: None,
            })
            .await
            .unwrap();
        let ops = Arc::new(OpsProvider {
            response: "GARBAGE".to_string(),
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        run_consolidation_pass(dir.path(), ops.clone(), &knobs_for_test())
            .await
            .unwrap();
        assert!(ops.calls.load(Ordering::SeqCst) >= 1);
        let state = RefreshState::load(&dir.path().join("memory").join("refresh_state.json"));
        assert!(
            state.consolidations_today >= 1 && state.tokens_today > 0,
            "failed zero-usage merges must be charged: {state:?}"
        );
    }

    #[tokio::test]
    async fn should_charge_staging_payload_when_zero_usage_merge_consumes_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(MemoryStore::open(dir.path()).await.unwrap());
        // A large-ish note the merge will consume (deleting the file).
        store
            .write_staging_note(&octos_memory::StagingNote {
                origin: octos_memory::NoteOrigin::Model,
                kind: octos_memory::NoteKind::Fact,
                content: format!("prefers dark mode. {}", "detail ".repeat(600)),
                session_key: None,
                sensitive: false,
                replaces_id: None,
            })
            .await
            .unwrap();
        let notes_dir = dir.path().join("memory/staging/notes");
        let note_name = std::fs::read_dir(&notes_dir)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path()
            .file_stem()
            .unwrap()
            .to_string_lossy()
            .to_string();

        let ops = Arc::new(OpsProvider {
            response: format!(
                r#"{{"ops":[{{"op":"add","section":null,"text":"Prefers dark mode.","sources":["{note_name}"]}}],"consumed_ids":["{note_name}"],"dropped":[]}}"#
            ),
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        run_consolidation_pass(dir.path(), ops, &knobs_for_test())
            .await
            .unwrap();

        let state = RefreshState::load(&dir.path().join("memory").join("refresh_state.json"));
        assert!(
            state.tokens_today >= 1_000,
            "the consumed staging payload must be charged (pre-capture): {}",
            state.tokens_today
        );
    }

    #[tokio::test]
    async fn should_extract_only_delta_when_session_grows() {
        // A fact stated BEFORE the first sweep must not be re-fed to the
        // extractor after later turns move the watermark — re-reading
        // history re-learns facts the user may have hard-deleted since.
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(MemoryStore::open(dir.path()).await.unwrap());
        seed_session(dir.path(), "api:500", "my favorite tea is oolong").await;

        let provider = ScriptedProvider {
            response: r#"{"items":[]}"#.to_string(),
            calls: std::sync::atomic::AtomicUsize::new(0),
            prompts: std::sync::Mutex::new(Vec::new()),
        };
        let knobs = knobs_for_test();
        run_extraction_pass(dir.path(), &store, &provider, &knobs)
            .await
            .unwrap();
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        assert!(
            provider.prompts.lock().unwrap().concat().contains("oolong"),
            "first sweep reads the full transcript"
        );

        // The session grows by one new exchange.
        seed_session(dir.path(), "api:500", "my staging server is maple").await;
        provider.prompts.lock().unwrap().clear();

        run_extraction_pass(dir.path(), &store, &provider, &knobs)
            .await
            .unwrap();
        assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
        let second = provider.prompts.lock().unwrap().concat();
        assert!(
            second.contains("maple"),
            "the new turn is extracted: {second}"
        );
        // The old statement may appear in CURRENT MEMORY context but must
        // not appear in the TRANSCRIPT slice. Assert on the transcript
        // portion only.
        let transcript_part = second
            .split("TRANSCRIPT of session")
            .nth(1)
            .unwrap_or("")
            .to_string();
        assert!(
            !transcript_part.contains("oolong"),
            "already-consumed turns must not be re-fed: {transcript_part}"
        );
    }

    #[tokio::test]
    async fn should_reset_cursor_when_session_rewritten() {
        // A cleared/recreated session whose byte total SHRANK must reset
        // the cursor — a stale count would silently skip the new content.
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(MemoryStore::open(dir.path()).await.unwrap());
        seed_session(
            dir.path(),
            "api:501",
            "a very long opening statement that pads the first transcript with bytes",
        )
        .await;

        let provider = ScriptedProvider {
            response: r#"{"items":[]}"#.to_string(),
            calls: std::sync::atomic::AtomicUsize::new(0),
            prompts: std::sync::Mutex::new(Vec::new()),
        };
        let knobs = knobs_for_test();
        run_extraction_pass(dir.path(), &store, &provider, &knobs)
            .await
            .unwrap();

        // Replace the session with SHORTER different content.
        let mut mgr = SessionManager::open(dir.path()).unwrap();
        let key = SessionKey("api:501".to_string());
        mgr.clear(&key).await.unwrap();
        drop(mgr);
        seed_session(dir.path(), "api:501", "my cat is Miso").await;
        provider.prompts.lock().unwrap().clear();

        run_extraction_pass(dir.path(), &store, &provider, &knobs)
            .await
            .unwrap();
        let prompts = provider.prompts.lock().unwrap().concat();
        assert!(
            prompts.contains("Miso"),
            "rewritten session must re-extract from the top: {prompts}"
        );
    }

    #[tokio::test]
    async fn should_reset_cursor_when_prefix_rewritten_but_longer() {
        // Rollback-and-continue: the transcript ends up AT LEAST as long
        // with different content below the cursor. Byte/length checks
        // can't see it — the prefix fingerprint must.
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(MemoryStore::open(dir.path()).await.unwrap());
        seed_session(dir.path(), "api:502", "original statement one").await;

        let provider = ScriptedProvider {
            response: r#"{"items":[]}"#.to_string(),
            calls: std::sync::atomic::AtomicUsize::new(0),
            prompts: std::sync::Mutex::new(Vec::new()),
        };
        let knobs = knobs_for_test();
        run_extraction_pass(dir.path(), &store, &provider, &knobs)
            .await
            .unwrap();

        // Replace with DIFFERENT content of the same+greater length.
        let mut mgr = SessionManager::open(dir.path()).unwrap();
        let key = SessionKey("api:502".to_string());
        mgr.clear(&key).await.unwrap();
        drop(mgr);
        seed_session(dir.path(), "api:502", "replacement statement alpha").await;
        seed_session(dir.path(), "api:502", "replacement statement beta").await;
        provider.prompts.lock().unwrap().clear();

        run_extraction_pass(dir.path(), &store, &provider, &knobs)
            .await
            .unwrap();
        let prompts = provider.prompts.lock().unwrap().concat();
        assert!(
            prompts.contains("replacement statement alpha"),
            "rewritten prefix must reset the cursor: {prompts}"
        );
    }

    #[tokio::test]
    async fn should_backfill_cursor_for_pre_upgrade_watermarks() {
        // A session fully consumed by a pre-cursor sweep (watermark set,
        // no cursor) must get a cursor stamped WITHOUT re-extraction, so
        // the next append feeds only the delta.
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(MemoryStore::open(dir.path()).await.unwrap());
        seed_session(dir.path(), "api:503", "ancient fact: tea is oolong").await;

        let provider = ScriptedProvider {
            response: r#"{"items":[]}"#.to_string(),
            calls: std::sync::atomic::AtomicUsize::new(0),
            prompts: std::sync::Mutex::new(Vec::new()),
        };
        let knobs = knobs_for_test();
        run_extraction_pass(dir.path(), &store, &provider, &knobs)
            .await
            .unwrap();

        // Simulate the pre-upgrade state: watermark kept, cursor dropped.
        let state_path = dir.path().join("memory").join("refresh_state.json");
        let mut state = RefreshState::load(&state_path);
        state.extracted_counts.clear();
        state.save(&state_path).unwrap();

        // Clean pass (watermark matches): backfills without extraction.
        run_extraction_pass(dir.path(), &store, &provider, &knobs)
            .await
            .unwrap();
        let state = RefreshState::load(&state_path);
        assert!(
            state.extracted_counts.contains_key("api:503"),
            "cursor backfilled on the clean pass: {:?}",
            state.extracted_counts
        );
        assert_eq!(
            provider.calls.load(Ordering::SeqCst),
            1,
            "no extra extraction"
        );

        // The next append feeds only the delta.
        seed_session(dir.path(), "api:503", "new fact: shell is fish").await;
        provider.prompts.lock().unwrap().clear();
        run_extraction_pass(dir.path(), &store, &provider, &knobs)
            .await
            .unwrap();
        let prompts = provider.prompts.lock().unwrap().concat();
        let transcript_part = prompts.split("TRANSCRIPT of session").nth(1).unwrap_or("");
        assert!(
            transcript_part.contains("fish") && !transcript_part.contains("oolong"),
            "post-backfill append is delta-only: {transcript_part}"
        );
    }

    #[tokio::test]
    async fn should_backfill_cursor_even_when_session_not_extraction_eligible() {
        // The backfill must not depend on idle windows: a pre-upgrade
        // session that is TOO FRESH for extraction still gets its cursor
        // stamped, so an append right after upgrade feeds only the delta.
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(MemoryStore::open(dir.path()).await.unwrap());
        seed_session(dir.path(), "api:504", "pre-upgrade fact: tea is oolong").await;

        let provider = ScriptedProvider {
            response: r#"{"items":[]}"#.to_string(),
            calls: std::sync::atomic::AtomicUsize::new(0),
            prompts: std::sync::Mutex::new(Vec::new()),
        };
        // First pass with normal knobs consumes the session + watermark.
        let knobs = knobs_for_test();
        run_extraction_pass(dir.path(), &store, &provider, &knobs)
            .await
            .unwrap();
        // Simulate pre-upgrade state: cursor dropped, watermark kept.
        let state_path = dir.path().join("memory").join("refresh_state.json");
        let mut state = RefreshState::load(&state_path);
        state.extracted_counts.clear();
        state.save(&state_path).unwrap();

        // Pass with a LONG idle requirement: the session is not
        // extraction-eligible, but the backfill must still stamp it.
        let mut strict = knobs_for_test();
        strict.min_idle = std::time::Duration::from_secs(3600);
        run_extraction_pass(dir.path(), &store, &provider, &strict)
            .await
            .unwrap();
        let state = RefreshState::load(&state_path);
        assert!(
            state.extracted_counts.contains_key("api:504"),
            "backfill is independent of extraction eligibility: {:?}",
            state.extracted_counts
        );
    }

    #[tokio::test]
    async fn should_backfill_cursor_even_when_budget_exhausted() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(MemoryStore::open(dir.path()).await.unwrap());
        seed_session(dir.path(), "api:505", "pre-upgrade fact").await;

        let provider = ScriptedProvider {
            response: r#"{"items":[]}"#.to_string(),
            calls: std::sync::atomic::AtomicUsize::new(0),
            prompts: std::sync::Mutex::new(Vec::new()),
        };
        let knobs = knobs_for_test();
        run_extraction_pass(dir.path(), &store, &provider, &knobs)
            .await
            .unwrap();
        // Pre-upgrade state + exhausted budget.
        let state_path = dir.path().join("memory").join("refresh_state.json");
        let mut state = RefreshState::load(&state_path);
        state.extracted_counts.clear();
        state.extractions_today = knobs.max_extractions_per_day;
        state.save(&state_path).unwrap();

        run_extraction_pass(dir.path(), &store, &provider, &knobs)
            .await
            .unwrap();
        let state = RefreshState::load(&state_path);
        assert!(
            state.extracted_counts.contains_key("api:505"),
            "the no-provider backfill must run on capped passes: {:?}",
            state.extracted_counts
        );
    }

    #[test]
    fn should_grow_backoff_exponentially_with_cap() {
        assert_eq!(backoff_after(1), Duration::from_secs(300));
        assert_eq!(backoff_after(2), Duration::from_secs(600));
        assert_eq!(backoff_after(5), Duration::from_secs(4800));
        assert_eq!(backoff_after(9), Duration::from_secs(4800));
    }
}
