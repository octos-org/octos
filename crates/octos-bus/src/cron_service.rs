//! Cron service that fires scheduled jobs into the message bus.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::Utc;
use eyre::{Result, WrapErr};
use octos_core::InboundMessage;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::cron_types::{CronJob, CronPayload, CronSchedule, CronStore};

/// Service that manages and executes cron jobs.
pub struct CronService {
    store_path: PathBuf,
    store: Mutex<CronStore>,
    inbound_tx: mpsc::Sender<InboundMessage>,
    running: AtomicBool,
    timer_handle: tokio::sync::Mutex<Option<JoinHandle<()>>>,
    /// Shutdown notification: every sleeper task in `arm_timer`
    /// `tokio::select!`s on this `Notify` alongside its
    /// `tokio::time::sleep`. A single `notify_waiters()` call from
    /// `shutdown_signal` / `stop` wakes ALL pending sleepers at once
    /// so they drop their self-held `Arc<CronService>` immediately
    /// rather than waiting out the (possibly long) `delay_ms`. This
    /// is the round-3 codex fix for the arm_timer-vs-shutdown race
    /// that lets a sleeper get installed AFTER `running=false`: even
    /// when that happens, the notify wakes the sleeper on its next
    /// poll and the Arc releases without a delay_ms-long tail.
    shutdown_notify: tokio::sync::Notify,
}

impl CronService {
    /// Create a new cron service, loading persisted jobs from disk.
    pub fn new(store_path: impl AsRef<Path>, inbound_tx: mpsc::Sender<InboundMessage>) -> Self {
        let store_path = store_path.as_ref().to_path_buf();
        // #2005 — never silently start empty on a corrupt store; see
        // `load_store_or_quarantine`.
        let store = load_store_or_quarantine(&store_path);

        Self {
            store_path,
            store: Mutex::new(store),
            inbound_tx,
            running: AtomicBool::new(false),
            timer_handle: tokio::sync::Mutex::new(None),
            shutdown_notify: tokio::sync::Notify::new(),
        }
    }

    /// Start the cron service: recompute next runs and arm the timer.
    pub fn start(self: &std::sync::Arc<Self>) {
        self.running.store(true, Ordering::Relaxed);
        let now_ms = Utc::now().timestamp_millis();

        {
            let mut store = self.store.lock().unwrap_or_else(|e| e.into_inner());
            for job in &mut store.jobs {
                if job.enabled && job.state.next_run_at_ms.is_none() {
                    job.compute_next_run(now_ms);
                }
            }
        }

        self.arm_timer();
        info!("cron service started");
    }

    /// Stop the cron service, cancelling any pending timer.
    pub async fn stop(&self) {
        self.running.store(false, Ordering::Relaxed);
        // Wake every pending sleeper in `arm_timer` so they release
        // their self-held `Arc<CronService>` immediately. Without
        // this, a sleeper that started after the running flag flipped
        // (but before `try_lock` succeeded) would self-Arc-pin the
        // service for `delay_ms`.
        self.shutdown_notify.notify_waiters();
        let mut handle = self.timer_handle.lock().await;
        if let Some(h) = handle.take() {
            h.abort();
        }
        info!("cron service stopped");
    }

    /// Whether the service is currently armed (i.e. `start()` has been
    /// called and no shutdown signal has fired). Used by lifecycle
    /// tests that need to observe the post-`Drop` shutdown signal
    /// without racing the timer task's terminal Arc release.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    /// Synchronous shutdown signal. Sets `running = false` so the
    /// timer's reschedule chain (`arm_timer` → `on_timer` → `arm_timer`)
    /// terminates on its next tick, and attempts a non-blocking abort
    /// of the currently-armed `JoinHandle` so the in-flight
    /// `tokio::time::sleep` does not delay shutdown.
    ///
    /// Intended for `Drop` impls and other sync contexts that hold the
    /// final `Arc<CronService>` (e.g. profile-scope runtime drop). The
    /// async [`Self::stop`] remains the preferred path when an `await`
    /// is available because it acquires the timer mutex deterministically.
    ///
    /// The non-blocking `try_lock` path is best-effort: if another
    /// caller is mutating `timer_handle` at the exact moment of drop,
    /// we leave the abort to the runtime tear-down. The `running` flag
    /// is the durable signal — once it flips, the next reschedule
    /// breaks the chain and the timer task drops its self-held
    /// `Arc<CronService>`, allowing the service to deallocate.
    pub fn shutdown_signal(&self) {
        self.running.store(false, Ordering::Relaxed);
        // Wake every pending sleeper in `arm_timer`. `notify_waiters`
        // does NOT race the running-flag check — even if a new
        // sleeper gets installed after the flag flipped (the
        // `arm_timer` task held the timer_handle lock when shutdown
        // ran, then proceeded to spawn its sleeper), that sleeper's
        // `tokio::select!` arm wakes on this notify and short-circuits
        // before its self-held `Arc<CronService>` is held for the
        // long `delay_ms` interval. `notify_waiters` is fire-and-
        // forget — sleepers registered AFTER this call do not
        // observe it, but `arm_timer`'s post-lock running check
        // catches that case and never spawns the sleeper in the
        // first place. The two mechanisms together close the race
        // codex flagged on the round-2 review.
        self.shutdown_notify.notify_waiters();
        if let Ok(mut handle) = self.timer_handle.try_lock() {
            if let Some(h) = handle.take() {
                h.abort();
            }
        }
        info!("cron service shutdown signalled");
    }

    /// Add a new cron job.
    pub fn add_job(
        self: &std::sync::Arc<Self>,
        name: String,
        schedule: CronSchedule,
        payload: CronPayload,
    ) -> Result<CronJob> {
        self.add_job_with_tz(name, schedule, payload, None)
    }

    /// Add a new cron job with an optional IANA timezone.
    pub fn add_job_with_tz(
        self: &std::sync::Arc<Self>,
        name: String,
        schedule: CronSchedule,
        payload: CronPayload,
        timezone: Option<String>,
    ) -> Result<CronJob> {
        let now_ms = Utc::now().timestamp_millis();
        let id = short_id();

        let delete_after_run = matches!(schedule, CronSchedule::At { .. });

        let mut job = CronJob {
            id: id.clone(),
            name,
            enabled: true,
            schedule,
            payload,
            state: Default::default(),
            created_at_ms: now_ms,
            delete_after_run,
            timezone,
        };
        job.compute_next_run(now_ms);

        let result = job.clone();

        {
            // Mutate + persist under ONE lock hold (persistence
            // invariant — see persist_store_locked). Roll the push back
            // on a failed write so memory never diverges from the file.
            let mut store = self.store.lock().unwrap_or_else(|e| e.into_inner());
            store.jobs.push(job);
            if let Err(error) = persist_store_locked(&self.store_path, &store) {
                store.jobs.retain(|j| j.id != id);
                return Err(error);
            }
        }

        self.arm_timer();

        debug!(id = %id, "added cron job");
        Ok(result)
    }

    /// Remove a cron job by ID. Returns true if found and removed.
    pub fn remove_job(self: &std::sync::Arc<Self>, id: &str) -> bool {
        let removed = {
            let mut store = self.store.lock().unwrap_or_else(|e| e.into_inner());
            let mut extracted = Vec::new();
            let mut kept = Vec::with_capacity(store.jobs.len());
            for job in store.jobs.drain(..) {
                if job.id == id {
                    extracted.push(job);
                } else {
                    kept.push(job);
                }
            }
            store.jobs = kept;
            if extracted.is_empty() {
                false
            } else if let Err(e) = persist_store_locked(&self.store_path, &store) {
                // Failed write: put the job back so memory matches the
                // file (persistence invariant), and report not-removed.
                store.jobs.extend(extracted);
                tracing::warn!("failed to save cron store: {e}");
                false
            } else {
                true
            }
        };

        if removed {
            self.arm_timer();
            debug!(id = %id, "removed cron job");
        }

        removed
    }

    /// List all enabled jobs, sorted by next run time.
    pub fn list_jobs(&self) -> Vec<CronJob> {
        let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        let mut jobs: Vec<_> = store.jobs.iter().filter(|j| j.enabled).cloned().collect();
        jobs.sort_by_key(|j| j.state.next_run_at_ms.unwrap_or(i64::MAX));
        jobs
    }

    /// List all jobs (including disabled), sorted by next run time.
    pub fn list_all_jobs(&self) -> Vec<CronJob> {
        let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        let mut jobs: Vec<_> = store.jobs.clone();
        jobs.sort_by_key(|j| j.state.next_run_at_ms.unwrap_or(i64::MAX));
        jobs
    }

    /// Enable or disable a cron job. Returns true if found.
    pub fn enable_job(self: &std::sync::Arc<Self>, id: &str, enabled: bool) -> bool {
        let found = {
            let now_ms = Utc::now().timestamp_millis();
            let mut store = self.store.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(job) = store.jobs.iter_mut().find(|j| j.id == id) {
                let prior = job.clone();
                job.enabled = enabled;
                if enabled {
                    job.compute_next_run(now_ms);
                } else {
                    job.state.next_run_at_ms = None;
                }
                if let Err(e) = persist_store_locked(&self.store_path, &store) {
                    // Failed write: revert so memory matches the file.
                    if let Some(job) = store.jobs.iter_mut().find(|j| j.id == id) {
                        *job = prior;
                    }
                    tracing::warn!("failed to save cron store: {e}");
                    false
                } else {
                    true
                }
            } else {
                false
            }
        };

        if found {
            self.arm_timer();
            debug!(id = %id, enabled = %enabled, "toggled cron job");
        }

        found
    }

    /// Enable/disable a job with RECONCILIATION + durable persistence:
    /// under one store-lock hold, re-read `cron.json` into memory
    /// (adopting writes from other owners — a gateway child that ran
    /// and exited, CLI edits), apply the toggle with `enable_job`'s
    /// next-run semantics, and persist — propagating save failures
    /// instead of logging them away.
    ///
    /// This exists for the serve-side `/api/my/cron` toggle: the
    /// long-lived ProfileRuntime service's in-memory store can be
    /// arbitrarily stale w.r.t. the file, and blindly persisting the
    /// stale store would erase other owners' jobs (codex #1612 r2).
    /// `Ok(None)` = job not found; `Err` = persistence failed (the
    /// in-memory store keeps the reloaded + toggled state either way —
    /// strictly fresher than what it held before).
    pub fn toggle_job_reconciling(
        self: &std::sync::Arc<Self>,
        id: &str,
        enabled: bool,
    ) -> Result<Option<CronJob>> {
        let found = {
            let now_ms = Utc::now().timestamp_millis();
            // Reload + toggle + persist under ONE lock hold. Every other
            // mutation persists before releasing this lock (persistence
            // invariant — see persist_store_locked), so the file we
            // reload is never behind unflushed memory, and nothing can
            // interleave between our reload and our write (codex #1612
            // r3 — both P1s).
            let mut store = self.store.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(fresh) = load_store(&self.store_path) {
                *store = fresh;
            }
            let prior = store.jobs.clone();
            let found = if let Some(job) = store.jobs.iter_mut().find(|j| j.id == id) {
                job.enabled = enabled;
                if enabled {
                    job.compute_next_run(now_ms);
                } else {
                    job.state.next_run_at_ms = None;
                }
                Some(job.clone())
            } else {
                None
            };
            if found.is_some()
                && let Err(error) = persist_store_locked(&self.store_path, &store)
            {
                // Failed write: revert so memory matches the file.
                store.jobs = prior;
                return Err(error);
            }
            found
        };

        if found.is_some() {
            self.arm_timer();
        }
        Ok(found)
    }

    /// Arm a timer for the earliest due job.
    fn arm_timer(self: &std::sync::Arc<Self>) {
        if !self.running.load(Ordering::Relaxed) {
            return;
        }

        let earliest_ms = {
            let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
            store
                .jobs
                .iter()
                .filter(|j| j.enabled)
                .filter_map(|j| j.state.next_run_at_ms)
                .min()
        };

        let Some(target_ms) = earliest_ms else {
            return;
        };

        let now_ms = Utc::now().timestamp_millis();
        let delay_ms = (target_ms - now_ms).max(0) as u64;

        let this = std::sync::Arc::clone(self);

        // Cancel existing timer
        let this2 = std::sync::Arc::clone(self);
        tokio::spawn(async move {
            let mut handle = this2.timer_handle.lock().await;
            if let Some(h) = handle.take() {
                h.abort();
            }

            // Re-check `running` AFTER acquiring the lock so a
            // concurrent `shutdown_signal` (which flips `running` to
            // false synchronously) is observed deterministically.
            // Without this re-check, the following race window leaks
            // the timer task past shutdown:
            //   T1: shutdown_signal sets running=false, try_lock fails
            //       (this task already holds the lock).
            //   T1: shutdown_signal returns; Drop completes.
            //   T2: this task spawns a new sleeper, stores the handle,
            //       drops the lock — the sleeper now self-holds an
            //       Arc<CronService> for `delay_ms`, blocking the
            //       service from deallocating.
            // With the re-check, `running == false` short-circuits and
            // the lock is released without installing a new handle;
            // the sleeper self-Arc release path collapses immediately.
            if !this2.running.load(Ordering::Relaxed) {
                return;
            }

            let new_handle = tokio::spawn(async move {
                // Round-4 codex fix: race-proof sleep via
                // `tokio::select!` against the service's shutdown
                // notify, with the notify waiter registered BEFORE
                // the final running check.
                //
                // `Notify::notified()` returns a future; the future
                // only registers as a waiter on first poll. Tokio
                // documents that any `notify_waiters()` call that
                // happens after `notified()` has been polled at least
                // once will wake the waiter — but a `notify_waiters`
                // that fires before the first poll is *missed*.
                //
                // To close the window where `shutdown_signal` fires
                // after the post-lock running check (above) but
                // before this sleeper subscribes to the notify, we:
                //   1. Construct the `notified()` future first.
                //   2. Pin it and poll it once via `Future::poll`
                //      indirectly by entering the `select!` block —
                //      `tokio::select!` polls all branches on first
                //      entry, which registers the notify waiter
                //      atomically.
                //   3. Inside the sleep arm, re-check `running`
                //      after the sleep wins so a missed-notify edge
                //      case still short-circuits `on_timer()`.
                //   4. Pre-`select!`, check `running` one more time
                //      so the case where `shutdown_signal` fired
                //      between the parent's `running` check and this
                //      task starting also terminates promptly.
                //
                // Combined: either (a) `running == false` is observed
                // before `select!` and we exit, or (b) the notify
                // waiter is registered atomically with the sleep
                // start and a subsequent `notify_waiters` wakes it,
                // or (c) the sleep wins, sees `running == false`,
                // and skips `on_timer()`. There is no path where
                // the sleeper self-Arc-pins for `delay_ms` after a
                // shutdown has fired.
                if !this.running.load(Ordering::Relaxed) {
                    return;
                }
                let notified = this.shutdown_notify.notified();
                tokio::pin!(notified);
                // Force the `notified()` future to register its
                // waiter before we re-check `running`. After this
                // call returns, any subsequent `notify_waiters` will
                // wake us.
                notified.as_mut().enable();
                // Re-check running AFTER the waiter is registered.
                // If `shutdown_signal` raced in between the previous
                // load and `enable()`, this check catches it. If it
                // races in AFTER `enable()`, the select arm catches
                // it.
                if !this.running.load(Ordering::Relaxed) {
                    return;
                }
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_millis(delay_ms)) => {
                        if this.running.load(Ordering::Relaxed) {
                            this.on_timer().await;
                        }
                    }
                    _ = &mut notified => {
                        // Shutdown raced in — drop the Arc and exit.
                    }
                }
            });

            *handle = Some(new_handle);
        });
    }

    /// Called when the timer fires: execute due jobs, update state, re-arm.
    async fn on_timer(self: &std::sync::Arc<Self>) {
        if !self.running.load(Ordering::Relaxed) {
            return;
        }

        let now_ms = Utc::now().timestamp_millis();

        // Reserve-then-fire: collect due jobs AND advance their schedule
        // state in ONE synchronous critical section, BEFORE any await.
        //
        // The old fire-then-advance ordering double-fired: while
        // `execute_job` was awaiting the bus send, the store still held
        // the firing job's past-due `next_run_at_ms`. Any concurrent
        // `arm_timer` (add_job / remove_job / enable_job) read that stale
        // value, computed a zero delay, aborted this task mid-fire (so
        // the advance below never ran), and spawned a zero-delay sleeper
        // that re-collected the SAME still-due job and fired it again.
        //
        // Advancing under the same lock that collects means no other
        // task can ever observe a job as due once it has been reserved
        // for this tick: a concurrent arm_timer sees the post-advance
        // (future) next_run and arms a future-dated timer. Even if this
        // task is aborted before or during the sends, the job is not
        // re-fired and its next occurrence stays scheduled — cron
        // semantics reserve the NEXT slot regardless of this fire's
        // outcome.
        // The reserve+advance+persist critical section is SYNCHRONOUS
        // (std Mutex + std::fs write) and must stay atomic to keep the
        // persistence invariant (persist under the same lock hold that
        // reserved — codex #1612 r3). Run the WHOLE section on the
        // blocking pool so its fs write never occupies a tokio worker,
        // even on a slow/full/network FS or a one-worker runtime
        // (codex #1612 r4 P1).
        //
        // The reserve→deliver→re-arm chain runs as ONE DETACHED task
        // (codex #1612 r5 P1). This on_timer future runs inside the
        // armed timer task, and EVERY schedule mutation (add_job /
        // remove_job / enable_job) calls arm_timer, which aborts that
        // task. If delivery lived here, an abort landing after the
        // blocking reservation committed but before (or during) the bus
        // sends would swallow the firing: the reservation already
        // advanced+persisted next_run, so the re-armed timer sees a
        // future-dated job and silently skips the occurrence. Detached,
        // an abort only ever cancels the SLEEP between ticks — a tick
        // that has begun always finishes reserve → deliver → re-arm.
        // Overlap is safe by construction: reservation is atomic under
        // the store lock, so a concurrent tick finds nothing due.
        // Shutdown does not leak the unit: execute_job races each send
        // against shutdown_notify, and arm_timer no-ops once `running`
        // is false.
        let this = std::sync::Arc::clone(self);
        tokio::spawn(async move {
            let reserve = std::sync::Arc::clone(&this);
            let due_jobs: Vec<CronJob> = tokio::task::spawn_blocking(move || {
                let mut store = reserve.store.lock().unwrap_or_else(|e| e.into_inner());
                let mut due = Vec::new();
                let mut to_delete = Vec::new();

                for stored_job in &mut store.jobs {
                    if !stored_job.is_due(now_ms) {
                        continue;
                    }
                    due.push(stored_job.clone());

                    stored_job.state.last_run_at_ms = Some(now_ms);
                    stored_job.state.last_status = Some("ok".into());

                    if stored_job.delete_after_run {
                        to_delete.push(stored_job.id.clone());
                    } else {
                        stored_job.compute_next_run(now_ms);
                    }
                }

                store.jobs.retain(|j| !to_delete.contains(&j.id));
                // A failed write is logged and the tick proceeds —
                // next_run stays advanced in memory, so no double-fire;
                // the file catches up on the next successful persist.
                if let Err(e) = persist_store_locked(&reserve.store_path, &store) {
                    tracing::warn!("failed to save cron store: {e}");
                }
                due
            })
            .await
            .unwrap_or_default();

            for job in &due_jobs {
                this.execute_job(job).await;
            }

            this.arm_timer();
        });
    }

    /// Fire a single job by sending an InboundMessage into the bus.
    async fn execute_job(&self, job: &CronJob) {
        info!(job_id = %job.id, name = %job.name, "executing cron job");

        let msg = InboundMessage {
            channel: "system".into(),
            sender_id: "cron".into(),
            chat_id: job.id.clone(),
            content: job.payload.message.clone(),
            timestamp: Utc::now(),
            media: vec![],
            metadata: serde_json::json!({
                "cron_job_id": job.id,
                "deliver_to_channel": job.payload.channel,
                "deliver_to_chat_id": job.payload.chat_id,
            }),
            message_id: None,
            origin: octos_core::MessageOrigin::ExternalUser,
        };

        // The delivery unit is detached (not abort-targeted), so a
        // shutdown can no longer cancel an in-flight send by aborting
        // the timer task. Race the send against the shutdown notify —
        // check-then-select, same missed-notify-proof ordering as the
        // sleeper in arm_timer: register the waiter FIRST, then check
        // `running`, so a shutdown that fires before registration is
        // caught by the flag and one that fires after wakes the select.
        // Without this, a full bus channel whose receiver stopped
        // draining at shutdown would park the send forever, pinning the
        // service Arc. Dropping the delivery on shutdown matches the
        // old abort semantics: the reservation stays advanced, so the
        // occurrence is skipped, never double-fired.
        //
        // `biased` with the notify arm FIRST (codex #1612 r6 P2): when
        // shutdown lands between the `running` check and the select's
        // first poll, BOTH arms are ready (the bus may have capacity).
        // An unbiased select! randomizes the winner, so the send could
        // deliver a cron message after the service stopped — the old
        // abort semantics never allowed that. Biased polling makes
        // cancellation take precedence deterministically.
        let notified = self.shutdown_notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if !self.running.load(Ordering::Relaxed) {
            warn!(job_id = %job.id, "cron service stopped; dropping cron delivery");
            return;
        }
        tokio::select! {
            biased;
            _ = &mut notified => {
                warn!(job_id = %job.id, "cron service shut down during delivery; dropping cron message");
            }
            res = self.inbound_tx.send(msg) => {
                if let Err(e) = res {
                    warn!(error = %e, job_id = %job.id, "failed to send cron message to bus");
                }
            }
        }
    }
}

/// Serialize + atomically replace `cron.json`. The caller MUST hold the
/// store lock for the whole call — that is the service's persistence
/// invariant (every mutation persists before releasing the lock), which
/// keeps memory and file in lockstep so no writer can interleave a
/// stale snapshot (codex #1612 r3). Unique temp names keep
/// out-of-process writers from colliding on a shared `cron.tmp`.
fn persist_store_locked(store_path: &Path, store: &CronStore) -> Result<()> {
    let json = serde_json::to_string_pretty(store).wrap_err("failed to serialize cron store")?;
    write_cron_json_atomic(store_path, &json)
}

/// Process-global temp-name sequence, shared by EVERY cron writer
/// (`CronService::persist_store_locked` AND the serve-side file toggle
/// in `cron_panel.rs`), so no two writers can pick the same
/// `cron.tmp-<pid>-<seq>` path and consume each other's temp file
/// (codex #1612 r4 P2).
static CRON_TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Serialize-free atomic replace of `cron.json`: write to a unique temp
/// then rename. The ONLY cron writer primitive — every mutation path
/// and the serve-side file toggle route through it (shared temp
/// sequence, consistent cleanup). Partial temp files are removed on
/// BOTH the write and rename error paths so repeated failures cannot
/// accumulate orphans (codex #1612 r4 P2).
pub fn write_cron_json_atomic(store_path: &Path, json: &str) -> Result<()> {
    let tmp_path = store_path.with_extension(format!(
        "tmp-{}-{}",
        std::process::id(),
        CRON_TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    // #2005 — fsync the temp file BEFORE the rename, and the directory AFTER.
    // The rename was already atomic, but not DURABLE: a hard power loss could
    // leave a zero-length / truncated `cron.json`, which is exactly the input
    // that used to make the service start with no jobs and then overwrite the
    // real ones. Mirrors the pattern already used by
    // `octos-cli::autonomy::supervisor_store` and `octos-memory::memory_store`.
    if let Err(error) = write_and_sync(&tmp_path, json) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(error.wrap_err("failed to write cron store temp"));
    }
    if let Err(error) = std::fs::rename(&tmp_path, store_path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(eyre::Report::new(error).wrap_err("failed to rename cron store"));
    }
    if let Some(dir) = store_path.parent() {
        fsync_dir(dir);
    }
    Ok(())
}

/// Write `json` to `path` and fsync the FILE before it is renamed into place.
fn write_and_sync(path: &Path, json: &str) -> Result<()> {
    use std::io::Write as _;
    let mut file = std::fs::File::create(path)?;
    file.write_all(json.as_bytes())?;
    // `flush()` on a std File is a no-op (no userspace buffer) — `sync_all` is
    // what actually gets the bytes to stable storage.
    file.sync_all()?;
    Ok(())
}

/// fsync a directory so a rename into it is durable. Best-effort: on non-Unix
/// std cannot open a directory for syncing, so the rename there is only as
/// durable as the filesystem makes it (it stays atomic either way).
fn fsync_dir(dir: &Path) {
    #[cfg(unix)]
    {
        if let Ok(handle) = std::fs::File::open(dir) {
            let _ = handle.sync_all();
        }
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
    }
}

fn load_store(path: &Path) -> Option<CronStore> {
    let data = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&data).ok()
}

/// #2005 — load the cron store, QUARANTINING a corrupt file instead of
/// silently starting with zero jobs.
///
/// The old path was `load_store(..).unwrap_or_default()`, which collapsed
/// "file absent" (legitimate first run) and "file present but unreadable /
/// unparseable" into the same empty store — with no error and no log line.
/// That is silent total loss, because the store is not read-only: the next
/// `add_job` / `enable_job` persists the empty-plus-one store OVER the real
/// `cron.json`, destroying every other job permanently.
///
/// Absent stays silent (first run is normal). Corrupt is loud AND preserved:
/// the bytes are renamed aside so a later persist cannot overwrite them, and
/// the operator can recover the jobs by hand. We still return an empty store
/// so the service starts — cron being down is bad, but it is recoverable;
/// losing the definitions is not.
fn load_store_or_quarantine(path: &Path) -> CronStore {
    let data = match std::fs::read_to_string(path) {
        Ok(data) => data,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // First run: no file yet. Legitimate, stay quiet.
            return CronStore::default();
        }
        Err(error) => {
            tracing::error!(
                path = %path.display(),
                %error,
                "cron store is unreadable; quarantining it and starting with NO jobs. \
                 Existing schedules will not fire until this is resolved.",
            );
            quarantine_cron_store(path);
            return CronStore::default();
        }
    };
    match serde_json::from_str(&data) {
        Ok(store) => store,
        Err(error) => {
            tracing::error!(
                path = %path.display(),
                %error,
                bytes = data.len(),
                "cron store is corrupt; quarantining it and starting with NO jobs. \
                 Existing schedules will not fire until this is resolved.",
            );
            quarantine_cron_store(path);
            CronStore::default()
        }
    }
}

/// Move a corrupt cron store aside so the next persist cannot overwrite it.
/// Best-effort: if the rename fails we have still logged the corruption, and
/// leaving the file in place is no worse than the pre-#2005 behaviour.
fn quarantine_cron_store(path: &Path) {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    let quarantined = path.with_extension(format!("corrupt-{stamp}"));
    match std::fs::rename(path, &quarantined) {
        Ok(()) => tracing::error!(
            quarantined = %quarantined.display(),
            "corrupt cron store preserved here — recover job definitions from it",
        ),
        Err(error) => tracing::error!(
            path = %path.display(),
            %error,
            "could not quarantine the corrupt cron store",
        ),
    }
}

/// Generate a short 8-char hex ID.
///
/// Derived from the RANDOM low bits of a v7 UUID (the `rand_b` field), not the
/// timestamp prefix. The previous `format!("{:x}", …)[..8]` (a) dropped a
/// leading-zero nibble because `{:x}` omits leading zeros, shifting the window,
/// and (b) sliced the HIGH hex chars — the 48-bit millisecond timestamp — so it
/// carried ZERO random bits and every job created within the same time window
/// shared an id. Zero-pad to a full 32 hex chars and take the LOW 8 (the low
/// 32 bits fall entirely inside v7's 62-bit random field).
fn short_id() -> String {
    let id = uuid::Uuid::now_v7();
    let hex = format!("{:032x}", id.as_u128());
    hex[hex.len() - 8..].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_id_is_unique_across_same_window_calls() {
        // Regression: short_id() formatted the v7 UUID with `{:x}` (dropping a
        // leading-zero nibble) and took the HIGH 8 hex chars — pure timestamp
        // bits, ZERO random bits. Every job created within the same time window
        // collided, so on_timer's id-keyed update marked BOTH and delete_after_run
        // deleted a not-yet-due sibling. Rapid calls must now be distinct.
        // 100 rapid calls: the buggy timestamp-only id yielded 1 distinct value
        // (the ms window does not turn over in <1ms); the fix yields 100. A
        // 100-sample 32-bit birthday-collision flake is ~1e-6, negligible.
        let ids: std::collections::HashSet<String> = (0..100).map(|_| short_id()).collect();
        assert_eq!(
            ids.len(),
            100,
            "short_id must be collision-resistant for jobs created close in time; \
             got {} distinct ids out of 100",
            ids.len()
        );
    }

    fn make_service(
        dir: &std::path::Path,
    ) -> (std::sync::Arc<CronService>, mpsc::Receiver<InboundMessage>) {
        let (tx, rx) = mpsc::channel(64);
        let service = std::sync::Arc::new(CronService::new(dir.join("cron.json"), tx));
        (service, rx)
    }

    #[test]
    fn concurrent_adds_and_reconciling_toggles_lose_nothing() {
        // codex #1612 r3: every mutation persists before releasing the
        // store lock, so a reload-based reconcile can never revert an
        // unflushed add, and no writer can interleave a stale snapshot
        // between another's serialize and rename. 8 adds racing 8
        // reconciling toggles must land ALL adds in the file.
        let dir = tempfile::tempdir().unwrap();
        let (tx, _rx) = mpsc::channel(64);
        let service = std::sync::Arc::new(CronService::new(dir.path().join("cron.json"), tx));
        let seeded = service
            .add_job(
                "seed".into(),
                CronSchedule::Every { every_ms: 60_000 },
                CronPayload {
                    message: "m".into(),
                    deliver: false,
                    channel: None,
                    chat_id: None,
                },
            )
            .unwrap();

        let mut handles = Vec::new();
        for i in 0..8 {
            let svc = std::sync::Arc::clone(&service);
            handles.push(std::thread::spawn(move || {
                svc.add_job(
                    format!("racer-{i}"),
                    CronSchedule::Every { every_ms: 60_000 },
                    CronPayload {
                        message: "m".into(),
                        deliver: false,
                        channel: None,
                        chat_id: None,
                    },
                )
                .unwrap();
            }));
            let svc = std::sync::Arc::clone(&service);
            let seed_id = seeded.id.clone();
            handles.push(std::thread::spawn(move || {
                svc.toggle_job_reconciling(&seed_id, i % 2 == 0)
                    .unwrap()
                    .expect("seed job present");
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }

        let file: CronStore =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join("cron.json")).unwrap())
                .unwrap();
        assert_eq!(
            file.jobs.len(),
            9,
            "all 8 racing adds + the seed must survive: {:?}",
            file.jobs.iter().map(|j| j.name.clone()).collect::<Vec<_>>()
        );
        // And memory agrees with the file (persistence invariant).
        assert_eq!(service.list_all_jobs().len(), 9);
    }

    #[tokio::test]
    async fn test_list_empty() {
        let dir = tempfile::tempdir().unwrap();
        let (service, _rx) = make_service(dir.path());
        assert!(service.list_jobs().is_empty());
    }

    #[tokio::test]
    async fn test_add_and_list() {
        let dir = tempfile::tempdir().unwrap();
        let (service, _rx) = make_service(dir.path());

        let job = service
            .add_job(
                "reminder".into(),
                CronSchedule::Every { every_ms: 60_000 },
                CronPayload {
                    message: "check in".into(),
                    deliver: false,
                    channel: None,
                    chat_id: None,
                },
            )
            .unwrap();

        let jobs = service.list_jobs();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, job.id);
        assert_eq!(jobs[0].name, "reminder");
    }

    #[tokio::test]
    async fn test_add_and_remove() {
        let dir = tempfile::tempdir().unwrap();
        let (service, _rx) = make_service(dir.path());

        let job = service
            .add_job(
                "temp".into(),
                CronSchedule::At {
                    at_ms: i64::MAX - 1,
                },
                CronPayload {
                    message: "once".into(),
                    deliver: false,
                    channel: None,
                    chat_id: None,
                },
            )
            .unwrap();

        assert_eq!(service.list_jobs().len(), 1);
        assert!(service.remove_job(&job.id));
        assert!(service.list_jobs().is_empty());
        assert!(!service.remove_job("nonexistent"));
    }

    #[tokio::test]
    async fn test_persistence_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("cron.json");

        {
            let (tx, _rx) = mpsc::channel(64);
            let service = std::sync::Arc::new(CronService::new(&store_path, tx));
            service
                .add_job(
                    "persist".into(),
                    CronSchedule::Every { every_ms: 1000 },
                    CronPayload {
                        message: "msg".into(),
                        deliver: false,
                        channel: None,
                        chat_id: None,
                    },
                )
                .unwrap();
        }

        // Reload
        let (tx, _rx) = mpsc::channel(64);
        let service = std::sync::Arc::new(CronService::new(&store_path, tx));
        let jobs = service.list_jobs();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].name, "persist");
    }

    #[tokio::test]
    async fn test_add_job_with_tz() {
        let dir = tempfile::tempdir().unwrap();
        let (service, _rx) = make_service(dir.path());

        let job = service
            .add_job_with_tz(
                "tz-job".into(),
                CronSchedule::Cron {
                    expr: "0 0 9 * * * *".into(),
                },
                CronPayload {
                    message: "good morning".into(),
                    deliver: false,
                    channel: None,
                    chat_id: None,
                },
                Some("America/New_York".into()),
            )
            .unwrap();

        assert_eq!(job.timezone.as_deref(), Some("America/New_York"));
        assert!(job.state.next_run_at_ms.is_some());

        let jobs = service.list_jobs();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].timezone.as_deref(), Some("America/New_York"));
    }

    #[tokio::test]
    async fn test_add_job_with_tz_none_defaults_utc() {
        let dir = tempfile::tempdir().unwrap();
        let (service, _rx) = make_service(dir.path());

        let job = service
            .add_job_with_tz(
                "utc-job".into(),
                CronSchedule::Cron {
                    expr: "0 0 9 * * * *".into(),
                },
                CronPayload {
                    message: "msg".into(),
                    deliver: false,
                    channel: None,
                    chat_id: None,
                },
                None,
            )
            .unwrap();

        assert!(job.timezone.is_none());
        assert!(job.state.next_run_at_ms.is_some());
    }

    #[tokio::test]
    async fn test_enable_disable_job() {
        let dir = tempfile::tempdir().unwrap();
        let (service, _rx) = make_service(dir.path());

        let job = service
            .add_job(
                "toggle".into(),
                CronSchedule::Every { every_ms: 60_000 },
                CronPayload {
                    message: "ping".into(),
                    deliver: false,
                    channel: None,
                    chat_id: None,
                },
            )
            .unwrap();

        // Disable
        assert!(service.enable_job(&job.id, false));
        let jobs = service.list_jobs();
        assert!(
            jobs.is_empty(),
            "disabled job should not appear in list_jobs"
        );

        let all = service.list_all_jobs();
        assert_eq!(all.len(), 1);
        assert!(!all[0].enabled);
        assert!(all[0].state.next_run_at_ms.is_none());

        // Re-enable
        assert!(service.enable_job(&job.id, true));
        let jobs = service.list_jobs();
        assert_eq!(jobs.len(), 1);
        assert!(jobs[0].enabled);
        assert!(jobs[0].state.next_run_at_ms.is_some());
    }

    #[tokio::test]
    async fn test_enable_nonexistent_job() {
        let dir = tempfile::tempdir().unwrap();
        let (service, _rx) = make_service(dir.path());

        assert!(!service.enable_job("no-such-id", true));
    }

    #[tokio::test]
    async fn test_list_all_jobs_includes_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let (service, _rx) = make_service(dir.path());

        let j1 = service
            .add_job(
                "enabled-job".into(),
                CronSchedule::Every { every_ms: 1000 },
                CronPayload {
                    message: "a".into(),
                    deliver: false,
                    channel: None,
                    chat_id: None,
                },
            )
            .unwrap();

        let j2 = service
            .add_job(
                "to-disable".into(),
                CronSchedule::Every { every_ms: 2000 },
                CronPayload {
                    message: "b".into(),
                    deliver: false,
                    channel: None,
                    chat_id: None,
                },
            )
            .unwrap();

        service.enable_job(&j2.id, false);

        let enabled = service.list_jobs();
        assert_eq!(enabled.len(), 1);
        assert_eq!(enabled[0].id, j1.id);

        let all = service.list_all_jobs();
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn test_list_all_jobs_sorted_by_next_run() {
        let dir = tempfile::tempdir().unwrap();
        let (service, _rx) = make_service(dir.path());

        // Add two jobs with different intervals; shorter interval => sooner next_run
        service
            .add_job(
                "later".into(),
                CronSchedule::Every { every_ms: 100_000 },
                CronPayload {
                    message: "a".into(),
                    deliver: false,
                    channel: None,
                    chat_id: None,
                },
            )
            .unwrap();

        service
            .add_job(
                "sooner".into(),
                CronSchedule::Every { every_ms: 1_000 },
                CronPayload {
                    message: "b".into(),
                    deliver: false,
                    channel: None,
                    chat_id: None,
                },
            )
            .unwrap();

        let all = service.list_all_jobs();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].name, "sooner");
        assert_eq!(all[1].name, "later");
    }

    #[tokio::test]
    async fn test_add_at_sets_delete_after_run() {
        let dir = tempfile::tempdir().unwrap();
        let (service, _rx) = make_service(dir.path());

        let at_job = service
            .add_job(
                "once".into(),
                CronSchedule::At {
                    at_ms: i64::MAX - 1,
                },
                CronPayload {
                    message: "fire".into(),
                    deliver: false,
                    channel: None,
                    chat_id: None,
                },
            )
            .unwrap();
        assert!(at_job.delete_after_run);

        let every_job = service
            .add_job(
                "repeat".into(),
                CronSchedule::Every { every_ms: 1000 },
                CronPayload {
                    message: "tick".into(),
                    deliver: false,
                    channel: None,
                    chat_id: None,
                },
            )
            .unwrap();
        assert!(!every_job.delete_after_run);
    }

    /// Poll `cond` until true or panic after 5s. Condition-polling (not a
    /// blind sleep): under-waiting is impossible, over-waiting is bounded.
    async fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !cond() {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for: {what}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    }

    /// Regression: re-arming the timer while a job is mid-fire must not
    /// fire that job a second time.
    ///
    /// Forcing the window deterministically: the bus channel has capacity
    /// 1 and two jobs are due. `on_timer` collects both, `send(J1)`
    /// succeeds (fire #1, fills the channel), `send(J2)` parks — holding
    /// `on_timer` open BEFORE it advances `next_run_at_ms` (the buggy
    /// fire-then-advance ordering). J1's stored `next_run_at_ms` is still
    /// past-due, pinned for as long as the test leaves the channel full.
    /// A concurrent `add_job` then runs `arm_timer`, which reads that
    /// stale past-due value, computes delay 0, aborts the parked timer
    /// task (killing it before the advance ever happens), and spawns a
    /// zero-delay sleeper that re-collects the still-due J1 and fires it
    /// AGAIN. The test only starts draining after the original timer
    /// task is provably dead (`AbortHandle::is_finished`), so the
    /// interleaving is fixed, not timing-dependent.
    #[tokio::test]
    async fn should_fire_due_job_exactly_once_when_rearm_races_mid_fire() {
        let dir = tempfile::tempdir().unwrap();
        // Capacity 1: first send succeeds, second send blocks.
        let (tx, mut rx) = mpsc::channel::<InboundMessage>(1);
        let service =
            std::sync::Arc::new(CronService::new(dir.path().join("cron.json"), tx.clone()));

        let payload = |msg: &str| CronPayload {
            message: msg.into(),
            deliver: false,
            channel: None,
            chat_id: None,
        };

        // Added while stopped, so arm_timer no-ops until start().
        let j1 = service
            .add_job(
                "first".into(),
                CronSchedule::Every { every_ms: 60_000 },
                payload("a"),
            )
            .unwrap();
        let j2 = service
            .add_job(
                "second".into(),
                CronSchedule::Every { every_ms: 60_000 },
                payload("b"),
            )
            .unwrap();

        // Backdate both jobs (as if their interval elapsed) so they are
        // due the moment the service starts.
        let past_ms = Utc::now().timestamp_millis() - 60_000;
        {
            let mut store = service.store.lock().unwrap();
            for job in store.jobs.iter_mut() {
                job.state.next_run_at_ms = Some(past_ms);
            }
        }

        service.start();

        // J1's fire is in the channel (capacity 1 -> 0) and the timer
        // task is parked on J2's send, mid-fire.
        wait_until("first fire enqueued and timer parked mid-fire", || {
            tx.capacity() == 0
        })
        .await;

        // Capture the parked timer task so its death is observable.
        let timer_abort = {
            let guard = service.timer_handle.lock().await;
            guard
                .as_ref()
                .expect("timer task must be armed while mid-fire")
                .abort_handle()
        };

        // Concurrent schedule mutation mid-fire: add_job -> arm_timer.
        service
            .add_job(
                "third".into(),
                CronSchedule::Every { every_ms: 600_000 },
                payload("c"),
            )
            .unwrap();

        // Only drain once the original timer task is dead, so it can
        // never resume and advance next_run itself: the double-fire
        // ordering is forced, not raced.
        wait_until("mid-fire timer task aborted by re-arm", || {
            timer_abort.is_finished()
        })
        .await;

        // Drain: first message must already be there; then collect until
        // the bus goes quiet for 500ms.
        let mut msgs = Vec::new();
        let first = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("first fire must arrive")
            .expect("bus channel closed unexpectedly");
        msgs.push(first);
        while let Ok(Some(msg)) =
            tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv()).await
        {
            msgs.push(msg);
        }

        let j1_fires = msgs.iter().filter(|m| m.chat_id == j1.id).count();
        assert_eq!(
            j1_fires,
            1,
            "job {} must fire exactly once across a concurrent re-arm, got {} fires \
             (fired messages: {:?})",
            j1.id,
            j1_fires,
            msgs.iter().map(|m| m.chat_id.clone()).collect::<Vec<_>>()
        );

        // Requirement (2): the fired jobs' next occurrence is scheduled
        // (advanced past the stale backdated value), not perpetually due.
        let jobs = service.list_jobs();
        for id in [&j1.id, &j2.id] {
            let next = jobs
                .iter()
                .find(|j| &j.id == id)
                .expect("job must still exist")
                .state
                .next_run_at_ms;
            assert!(
                next.is_some_and(|t| t > past_ms),
                "job {id} must have its next occurrence scheduled, got {next:?}"
            );
        }

        service.stop().await;
    }

    /// codex #1612 r5 P1: a routine schedule mutation must never swallow
    /// a firing whose reservation already committed.
    ///
    /// The tick is reserve-then-fire: the blocking critical section
    /// advances `next_run_at_ms` and persists, and only then does the
    /// task deliver to the bus. Every mutation (`add_job` / `remove_job`
    /// / `enable_job`) calls `arm_timer`, which ABORTS the armed timer
    /// task. If delivery lives in that abortable future, an abort
    /// landing after the reservation committed but before (or during)
    /// the bus send kills the delivery — and the re-armed timer sees
    /// the advanced next_run and silently skips the occurrence.
    ///
    /// Deterministic interleaving: the bus channel (capacity 1) is
    /// PRE-FILLED, so the tick's send is guaranteed parked (or not yet
    /// polled) — it cannot complete before the test drains. We wait for
    /// the reservation to commit (next_run advanced), let a concurrent
    /// `add_job` abort the timer task, PROVE the task is dead, and only
    /// then drain: the reserved fire must still arrive, because the
    /// reserve→deliver→re-arm unit is detached and no abort can sever
    /// a committed reservation from its delivery.
    #[tokio::test]
    async fn should_deliver_reserved_fire_when_rearm_aborts_timer_mid_tick() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, mut rx) = mpsc::channel::<InboundMessage>(1);
        let service =
            std::sync::Arc::new(CronService::new(dir.path().join("cron.json"), tx.clone()));

        let payload = |msg: &str| CronPayload {
            message: msg.into(),
            deliver: false,
            channel: None,
            chat_id: None,
        };

        // Fill the channel BEFORE the timer can fire: the tick's send
        // parks until the test drains.
        tx.try_send(InboundMessage {
            channel: "system".into(),
            sender_id: "test".into(),
            chat_id: "plug".into(),
            content: "plug".into(),
            timestamp: Utc::now(),
            media: vec![],
            metadata: serde_json::Value::Null,
            message_id: None,
            origin: octos_core::MessageOrigin::ExternalUser,
        })
        .expect("pre-fill send must succeed on an empty capacity-1 channel");

        // Added while stopped, then backdated so it is due immediately.
        let job = service
            .add_job(
                "reserved".into(),
                CronSchedule::Every { every_ms: 60_000 },
                payload("reserved fire"),
            )
            .unwrap();
        let past_ms = Utc::now().timestamp_millis() - 60_000;
        {
            let mut store = service.store.lock().unwrap();
            store.jobs[0].state.next_run_at_ms = Some(past_ms);
        }

        service.start();

        // The reservation has committed once next_run is advanced past
        // the backdated value; delivery is parked on the full channel.
        wait_until("reservation committed (next_run advanced)", || {
            service
                .list_jobs()
                .first()
                .and_then(|j| j.state.next_run_at_ms)
                .is_some_and(|t| t > past_ms)
        })
        .await;

        // Capture the armed timer task, then let a routine mutation
        // re-arm (and thus abort) it.
        let timer_abort = {
            let guard = service.timer_handle.lock().await;
            guard
                .as_ref()
                .expect("timer task must be armed")
                .abort_handle()
        };
        service
            .add_job(
                "unrelated".into(),
                CronSchedule::Every { every_ms: 600_000 },
                payload("unrelated"),
            )
            .unwrap();

        // Only drain once the abort has provably landed, so delivery
        // cannot win by racing ahead of the abort.
        wait_until("timer task aborted by the re-arm", || {
            timer_abort.is_finished()
        })
        .await;

        // Drain the plug, then the reserved fire MUST arrive.
        let plug = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("plug message must be readable")
            .expect("bus channel closed unexpectedly");
        assert_eq!(plug.content, "plug");

        let fired = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect(
                "reserved fire must be delivered even though a schedule mutation \
                 aborted the timer task after the reservation committed",
            )
            .expect("bus channel closed unexpectedly");
        assert_eq!(fired.chat_id, job.id);
        assert_eq!(fired.content, "reserved fire");

        service.stop().await;
    }

    #[test]
    fn test_short_id_format() {
        let id = short_id();
        assert_eq!(id.len(), 8);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_load_store_missing_file() {
        let result = load_store(Path::new("/tmp/nonexistent_cron_store.json"));
        assert!(result.is_none());
    }

    #[test]
    fn test_load_store_invalid_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.json");
        std::fs::write(&path, "not json").unwrap();
        assert!(load_store(&path).is_none());
    }

    /// #2005 — a corrupt `cron.json` must NEVER be silently swallowed into an
    /// empty store that the next mutation then overwrites.
    ///
    /// Old behaviour: `load_store(..).unwrap_or_default()` turned a truncated
    /// file into an empty store with no error and no log line; the next
    /// `add_job` persisted over `cron.json` and every other job was gone for
    /// good. The load-side property that prevents that is: the original bytes
    /// must still exist on disk afterwards.
    #[test]
    fn corrupt_cron_store_is_quarantined_not_silently_discarded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cron.json");
        // A truncated store — exactly what an unfsynced write + power loss
        // leaves behind.
        // Truncated mid-token, as an unfsynced write + power loss leaves it.
        // The cut-off word is deliberately NOT a prefix of a real English word:
        // the `typos` CI gate reads a truncated word as a misspelling and fails
        // the build, which is ironic for a fixture whose whole job is to BE
        // truncated — but not a battle worth having with a spell-checker.
        let corrupt = r#"{"jobs":[{"id":"job-1","name":"nightly zzq"#;
        std::fs::write(&path, corrupt).unwrap();

        let store = load_store_or_quarantine(&path);
        assert!(
            store.jobs.is_empty(),
            "the service still starts (cron down is recoverable; losing the definitions is not)",
        );

        // THE load-bearing assertion: the operator can still get the jobs back.
        let preserved: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.contains("corrupt-"))
            })
            .collect();
        assert_eq!(
            preserved.len(),
            1,
            "the corrupt store must be quarantined aside, not discarded (got {preserved:?})",
        );
        assert_eq!(
            std::fs::read_to_string(&preserved[0]).unwrap(),
            corrupt,
            "the quarantined copy must be byte-identical so jobs can be recovered",
        );
        assert!(
            !path.exists(),
            "the corrupt file is moved aside, so a later persist cannot overwrite it in place",
        );
    }

    /// A MISSING store is the normal first run — it must stay silent and must
    /// NOT create a quarantine file.
    #[test]
    fn missing_cron_store_is_not_treated_as_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cron.json");
        let store = load_store_or_quarantine(&path);
        assert!(store.jobs.is_empty());
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            0,
            "first run must not leave a quarantine artifact behind",
        );
    }
}
