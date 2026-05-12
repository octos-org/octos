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
        let store = load_store(&store_path).unwrap_or_default();

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
            let mut store = self.store.lock().unwrap_or_else(|e| e.into_inner());
            store.jobs.push(job);
        }

        self.save_store()?;
        self.arm_timer();

        debug!(id = %id, "added cron job");
        Ok(result)
    }

    /// Remove a cron job by ID. Returns true if found and removed.
    pub fn remove_job(self: &std::sync::Arc<Self>, id: &str) -> bool {
        let removed = {
            let mut store = self.store.lock().unwrap_or_else(|e| e.into_inner());
            let before = store.jobs.len();
            store.jobs.retain(|j| j.id != id);
            store.jobs.len() < before
        };

        if removed {
            if let Err(e) = self.save_store() {
                tracing::warn!("failed to save cron store: {e}");
            }
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
                job.enabled = enabled;
                if enabled {
                    job.compute_next_run(now_ms);
                } else {
                    job.state.next_run_at_ms = None;
                }
                true
            } else {
                false
            }
        };

        if found {
            if let Err(e) = self.save_store() {
                tracing::warn!("failed to save cron store: {e}");
            }
            self.arm_timer();
            debug!(id = %id, enabled = %enabled, "toggled cron job");
        }

        found
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

        // Collect due jobs
        let due_jobs: Vec<CronJob> = {
            let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
            store
                .jobs
                .iter()
                .filter(|j| j.is_due(now_ms))
                .cloned()
                .collect()
        };

        for job in &due_jobs {
            self.execute_job(job).await;
        }

        // Update state
        {
            let mut store = self.store.lock().unwrap_or_else(|e| e.into_inner());
            let mut to_delete = Vec::new();

            for stored_job in &mut store.jobs {
                if due_jobs.iter().any(|d| d.id == stored_job.id) {
                    stored_job.state.last_run_at_ms = Some(now_ms);
                    stored_job.state.last_status = Some("ok".into());

                    if stored_job.delete_after_run {
                        to_delete.push(stored_job.id.clone());
                    } else {
                        stored_job.compute_next_run(now_ms);
                    }
                }
            }

            store.jobs.retain(|j| !to_delete.contains(&j.id));
        }

        if let Err(e) = self.save_store_async().await {
            tracing::warn!("failed to save cron store: {e}");
        }
        self.arm_timer();
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
        };

        if let Err(e) = self.inbound_tx.send(msg).await {
            warn!(error = %e, job_id = %job.id, "failed to send cron message to bus");
        }
    }

    fn save_store(&self) -> Result<()> {
        let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        let json =
            serde_json::to_string_pretty(&*store).wrap_err("failed to serialize cron store")?;
        let tmp_path = self.store_path.with_extension("tmp");
        std::fs::write(&tmp_path, &json).wrap_err("failed to write cron store temp")?;
        std::fs::rename(&tmp_path, &self.store_path).wrap_err("failed to rename cron store")?;
        Ok(())
    }

    /// Async version of save_store that uses spawn_blocking to avoid blocking
    /// the tokio runtime thread.
    async fn save_store_async(&self) -> Result<()> {
        let json = {
            let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
            serde_json::to_string_pretty(&*store).wrap_err("failed to serialize cron store")?
        };
        let tmp_path = self.store_path.with_extension("tmp");
        let store_path = self.store_path.clone();

        tokio::task::spawn_blocking(move || {
            std::fs::write(&tmp_path, &json).wrap_err("failed to write cron store temp")?;
            std::fs::rename(&tmp_path, &store_path).wrap_err("failed to rename cron store")?;
            Ok::<_, eyre::Report>(())
        })
        .await
        .map_err(|e| eyre::eyre!("spawn_blocking join error: {e}"))??;

        Ok(())
    }
}

fn load_store(path: &Path) -> Option<CronStore> {
    let data = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&data).ok()
}

/// Generate a short 8-char hex ID.
fn short_id() -> String {
    let id = uuid::Uuid::now_v7();
    format!("{:x}", id.as_u128())[..8].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_service(
        dir: &std::path::Path,
    ) -> (std::sync::Arc<CronService>, mpsc::Receiver<InboundMessage>) {
        let (tx, rx) = mpsc::channel(64);
        let service = std::sync::Arc::new(CronService::new(dir.join("cron.json"), tx));
        (service, rx)
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
}
