//! User-scoped cron viewer + enable toggle (web parity audit P3 item 7: the
//! book documents scheduled tasks, but the dashboard had no cron surface at
//! all — only the admin-token `GET /api/admin/profiles/{id}/cron` list).
//!
//! Reached over the UI Protocol via the `cron/list` / `cron/toggle` methods
//! (gated by `auxiliary.rest_to_ws.v1`), which wrap these handlers on the WS
//! transport. `cron/list` is byte-budget-bounded (row + serialized-size caps
//! with a `truncated` signal) so it fits one WS frame; the former
//! `/api/my/cron*` REST routes were retired (see `router.rs`).
//!
//! Runtime-ownership constraint that shapes this API: `cron.json` has
//! up to three writers, and a bare file edit races all of them (each
//! holds the store in memory and rewrites the whole file on its own
//! events):
//!
//! 1. **The serve-side `ProfileRuntime`'s own `CronService`** (started
//!    for enabled profiles with an LLM). Its in-memory store can be
//!    ARBITRARILY STALE w.r.t. the file (a gateway child may have run
//!    and written since it loaded), so the toggle routes through
//!    `CronService::toggle_job_reconciling`: reload-from-disk +
//!    toggle + persist under ONE store-lock hold — adopting other
//!    owners' writes instead of erasing them, and propagating
//!    persistence failures (codex #1612 r2).
//! 2. **A spawned gateway child process.** Its `CronService` lives in
//!    another process we cannot call into — the toggle REFUSES with
//!    `409 {reason: "gateway_running"}` (re-checked INSIDE the
//!    mutation lock) and the SPA routes the user through the existing
//!    stop/start controls.
//! 3. **Nobody** — the file is the source of truth and the toggle
//!    applies atomically (unique tmp + rename), mirroring
//!    `enable_job`'s next-run semantics so a later `CronService::start`
//!    (which only recomputes jobs whose next run is `None`) does not
//!    fire a stale deadline.
//!
//! Reads (`GET /api/my/cron`) always come from the FILE: every owner
//! persists synchronously after each mutation, so the file is the one
//! view that is never staler than its most recent writer — unlike the
//! parent service's memory (codex #1612 r2 P1).
//!
//! All serve-side mutations are serialized behind a process-wide lock
//! (concurrent PUTs would otherwise race the read-modify-write and drop
//! one another's changes). Residual race documented on the lock.
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::http::{HeaderMap, StatusCode};
use chrono::Utc;
use octos_bus::CronService;
use octos_core::ui_protocol::{CronCreateMode, CronCreateSchedule, CronDeleteParams};

use super::AppState;
use super::router::AuthIdentity;
use crate::api::auth_handlers::resolve_cron_profile_id;

/// Render one job in the same shape the admin list uses, so the SPA
/// can share a row component between the two surfaces.
fn job_json(j: &octos_bus::CronJob, now_ms: i64) -> serde_json::Value {
    let next_in = j.state.next_run_at_ms.map(|t| {
        let secs = (t - now_ms) / 1000;
        if secs < 0 {
            "overdue".to_string()
        } else if secs < 60 {
            format!("{secs}s")
        } else if secs < 3600 {
            format!("{}m", secs / 60)
        } else {
            format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
        }
    });
    let last_run = j.state.last_run_at_ms.map(|t| {
        chrono::DateTime::from_timestamp_millis(t)
            .map(|dt| dt.to_rfc3339())
            .unwrap_or_default()
    });
    serde_json::json!({
        "id": j.id,
        "name": j.name,
        "enabled": j.enabled,
        "schedule": serde_json::to_value(&j.schedule).unwrap_or_default(),
        "message": crate::api::admin::truncate_str(&j.payload.message, 100),
        "channel": j.payload.channel,
        "last_run": last_run,
        "last_status": j.state.last_status,
        "last_error": j.state.last_error,
        "next_run": j.state.next_run_at_ms,
        "next_in": next_in,
        "timezone": j.timezone,
        "origin": j.origin,
    })
}

fn cron_path_for(data_dir: &Path) -> PathBuf {
    data_dir.join("cron.json")
}

/// Whether the profile's gateway CHILD PROCESS is currently running.
/// That process's `CronService` owns `cron.json` from another address
/// space we cannot coordinate with — the toggle refuses while it runs.
/// `None` process manager (solo serve, tests) means no child exists.
async fn gateway_running(state: &AppState, profile_id: &str) -> bool {
    match state.process_manager.as_ref() {
        Some(pm) => pm.status(profile_id).await.running,
        None => false,
    }
}

/// The serve-process-local `CronService` for this profile, if one is
/// live (enabled top-level profile with an LLM gets one on its
/// `ProfileRuntime`). Mutations MUST route through it when present —
/// it holds the store in memory and its next save would silently
/// overwrite a bare file edit.
fn live_cron_service(state: &AppState, profile_id: &str) -> Option<Arc<CronService>> {
    crate::api::ui_protocol_transport::resolve_session_profile_runtime(state, Some(profile_id))
        .and_then(|runtime| runtime.cron_service.clone())
}

/// Serializes every serve-side cron mutation (in-process, across all
/// profiles — toggles are rare enough that one lock is fine).
///
/// Residual race, accepted + documented: a `ProfileRuntime` can
/// bootstrap CONCURRENTLY with a file-path toggle (its `CronService`
/// loads `cron.json` in `new()`), in which case a load that
/// interleaves our read-modify-write can hold the pre-toggle store and
/// persist it later. We shrink the window by re-resolving the live
/// service inside the lock; closing it fully needs a cross-owner
/// lifecycle lock that does not exist today.
static CRON_MUTATION_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Transport-independent application boundary shared by the REST compatibility
/// handlers and the OUP auxiliary methods. It owns authorization and cron
/// business results; callers only map these typed values onto their wire.
pub(crate) struct CronApplicationService {
    state: Arc<AppState>,
}

#[derive(Debug)]
pub(crate) struct CronListApplicationResult {
    pub(crate) jobs: Vec<serde_json::Value>,
    pub(crate) count: usize,
    pub(crate) truncated: bool,
    pub(crate) gateway_running: bool,
}

#[derive(Debug)]
pub(crate) struct CronJobApplicationResult {
    pub(crate) job: serde_json::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CronApplicationError {
    ProfilesUnavailable,
    Auth(StatusCode),
    ProfileReadFailed,
    ProfileNotFound,
    AuthScopeViolation,
    MessageRequired,
    EveryMsMustBePositive,
    InvalidAtMs,
    InvalidEveryMs,
    CronExprRequired,
    InvalidTimezone,
    InvalidCronExpr,
    GatewayRunning,
    CronStoreIo,
    JobNotFound,
}

impl CronApplicationError {
    pub(crate) fn status(self) -> StatusCode {
        match self {
            Self::ProfilesUnavailable => StatusCode::SERVICE_UNAVAILABLE,
            Self::Auth(status) => status,
            Self::ProfileReadFailed | Self::CronStoreIo => StatusCode::INTERNAL_SERVER_ERROR,
            Self::ProfileNotFound | Self::JobNotFound => StatusCode::NOT_FOUND,
            Self::AuthScopeViolation => StatusCode::FORBIDDEN,
            Self::MessageRequired
            | Self::EveryMsMustBePositive
            | Self::InvalidAtMs
            | Self::InvalidEveryMs
            | Self::CronExprRequired
            | Self::InvalidTimezone
            | Self::InvalidCronExpr => StatusCode::BAD_REQUEST,
            Self::GatewayRunning => StatusCode::CONFLICT,
        }
    }

    pub(crate) fn reason(self) -> &'static str {
        match self {
            Self::ProfilesUnavailable => "profiles_unavailable",
            Self::Auth(_) => "auth",
            Self::ProfileReadFailed => "profile_read_failed",
            Self::ProfileNotFound => "profile_not_found",
            Self::AuthScopeViolation => "auth_scope_violation",
            Self::MessageRequired => "message_required",
            Self::EveryMsMustBePositive => "every_ms_must_be_positive",
            Self::InvalidAtMs => "invalid_at_ms: expected a run time on or before 2099-12-31",
            Self::InvalidEveryMs => {
                "invalid_every_ms: expected a positive interval with an upcoming run on or before 2099-12-31 and no timestamp overflow"
            }
            Self::CronExprRequired => "cron_expr_required",
            Self::InvalidTimezone => {
                "invalid_timezone: expected an IANA timezone such as Asia/Shanghai"
            }
            Self::InvalidCronExpr => {
                "invalid_cron_expr: expected six fields including seconds, no explicit year, and an upcoming run on or before 2099-12-31; for example 0 0 9 * * *"
            }
            Self::GatewayRunning => "gateway_running",
            Self::CronStoreIo => "cron_store_io",
            Self::JobNotFound => "job_not_found",
        }
    }
}

/// Row cap for a single `cron/list` response, and the serialized-byte budget
/// that stops the list well under the 1 MiB WS frame. Per-field cap bounds any
/// one tenant-controlled string (name / channel / timezone / schedule) so no
/// single row can blow the frame on its own.
const MAX_CRON_PANEL_JOBS: usize = 1000;
const CRON_LIST_BYTE_BUDGET: usize = 900 * 1024;
const MAX_CRON_FIELD_CHARS: usize = 256;

/// Cap every string field of a rendered cron row to [`MAX_CRON_FIELD_CHARS`],
/// so an oversized tenant-controlled field can't push the frame over budget.
fn cap_cron_row(mut row: serde_json::Value) -> serde_json::Value {
    if let serde_json::Value::Object(map) = &mut row {
        for value in map.values_mut() {
            if let serde_json::Value::String(s) = value {
                if s.chars().count() > MAX_CRON_FIELD_CHARS {
                    *s = crate::api::admin::truncate_str(s, MAX_CRON_FIELD_CHARS);
                }
            }
        }
    }
    row
}

fn mutation_service(state: &AppState, profile_id: &str, cron_path: PathBuf) -> Arc<CronService> {
    live_cron_service(state, profile_id).unwrap_or_else(|| {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        Arc::new(CronService::new(cron_path, tx))
    })
}

/// Outcome of the pure file-level toggle (kept handler-free so the
/// mutation semantics are unit-testable without a ProcessManager).
#[derive(Debug, PartialEq)]
pub(crate) enum ToggleError {
    NotFound,
    Io,
}

/// Flip `enabled` for one job in `cron.json`, atomically (unique tmp +
/// rename). Mirrors `CronService::enable_job`'s state semantics:
/// disabling clears `next_run_at_ms` (a stale deadline would fire
/// immediately on re-enable + restart, because `CronService::start`
/// only recomputes jobs whose next run is `None`), enabling recomputes
/// it from now. Caller is responsible for owner coordination
/// (gateway-not-running guard + `CRON_MUTATION_LOCK`).
pub(crate) async fn apply_cron_toggle(
    cron_path: &Path,
    job_id: &str,
    enabled: bool,
) -> Result<octos_bus::CronJob, ToggleError> {
    let content = match tokio::fs::read_to_string(cron_path).await {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(ToggleError::NotFound),
        Err(_) => return Err(ToggleError::Io),
    };
    let mut store: octos_bus::CronStore =
        serde_json::from_str(&content).map_err(|_| ToggleError::Io)?;
    let job = store
        .jobs
        .iter_mut()
        .find(|j| j.id == job_id)
        .ok_or(ToggleError::NotFound)?;
    job.enabled = enabled;
    if enabled {
        job.compute_next_run(Utc::now().timestamp_millis());
    } else {
        job.state.next_run_at_ms = None;
    }
    let updated = job.clone();
    let json = serde_json::to_string_pretty(&store).map_err(|_| ToggleError::Io)?;
    // Route through the SAME atomic writer CronService uses — one
    // shared process-global temp-name sequence, so this file path and
    // a live service can never pick the same cron.tmp-<pid>-<seq> and
    // consume each other's temp file (codex #1612 r4 P2). Runs on the
    // blocking pool (it is synchronous fs I/O).
    let cron_path = cron_path.to_path_buf();
    tokio::task::spawn_blocking(move || octos_bus::write_cron_json_atomic(&cron_path, &json))
        .await
        .map_err(|_| ToggleError::Io)?
        .map_err(|_| ToggleError::Io)?;
    Ok(updated)
}

/// Route the toggle through a live `CronService` with reconciliation:
/// reload-from-disk + toggle + persist under one store-lock hold, so a
/// stale parent service ADOPTS other owners' writes instead of erasing
/// them, and save failures surface as errors instead of `ok: true`
/// (codex #1612 r2). Split from the handler so the service path is
/// unit-testable without a `ProfileRuntime`.
pub(crate) fn toggle_via_service(
    svc: &Arc<CronService>,
    job_id: &str,
    enabled: bool,
) -> Result<octos_bus::CronJob, ToggleError> {
    match svc.toggle_job_reconciling(job_id, enabled) {
        Ok(Some(job)) => Ok(job),
        Ok(None) => Err(ToggleError::NotFound),
        Err(_) => Err(ToggleError::Io),
    }
}

/// Read + parse `cron.json`; `Ok(None)` when the file does not exist.
async fn read_cron_store(cron_path: &Path) -> Result<Option<octos_bus::CronStore>, StatusCode> {
    let content = match tokio::fs::read_to_string(cron_path).await {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(StatusCode::INTERNAL_SERVER_ERROR),
    };
    serde_json::from_str(&content)
        .map(Some)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

impl CronApplicationService {
    pub(crate) fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }

    fn resolve_profile_id(
        &self,
        identity: &AuthIdentity,
        headers: &HeaderMap,
        session_id: Option<&octos_core::SessionKey>,
    ) -> Result<String, CronApplicationError> {
        let ps = self
            .state
            .profile_store
            .as_ref()
            .ok_or(CronApplicationError::ProfilesUnavailable)?;
        resolve_cron_profile_id(identity, ps, &self.state, headers, session_id).map_err(|status| {
            match status {
                StatusCode::FORBIDDEN => CronApplicationError::AuthScopeViolation,
                StatusCode::NOT_FOUND => CronApplicationError::ProfileNotFound,
                other => CronApplicationError::Auth(other),
            }
        })
    }

    fn profile_data_dir(&self, profile_id: &str) -> Result<PathBuf, CronApplicationError> {
        let ps = self
            .state
            .profile_store
            .as_ref()
            .ok_or(CronApplicationError::ProfilesUnavailable)?;
        let profile = ps
            .get(profile_id)
            .map_err(|_| CronApplicationError::ProfileReadFailed)?
            .ok_or(CronApplicationError::ProfileNotFound)?;
        Ok(ps.resolve_data_dir(&profile))
    }

    pub(crate) async fn list(
        &self,
        identity: &AuthIdentity,
        headers: &HeaderMap,
        session_id: Option<&octos_core::SessionKey>,
    ) -> Result<CronListApplicationResult, CronApplicationError> {
        let profile_id = self.resolve_profile_id(identity, headers, session_id)?;
        let cron_path = cron_path_for(&self.profile_data_dir(&profile_id)?);
        let running = gateway_running(&self.state, &profile_id).await;
        // The FILE is the freshest honest view: every owner persists
        // synchronously after each mutation, while the parent service's
        // memory can predate a gateway child's whole lifetime.
        let jobs = match read_cron_store(&cron_path).await {
            Ok(Some(store)) => store.jobs,
            Ok(None) => Vec::new(),
            Err(_) => return Err(CronApplicationError::CronStoreIo),
        };

        let now_ms = Utc::now().timestamp_millis();
        // Bound the serialized list so `cron/list` always fits one WS frame:
        // cap each row's string fields, stop before the byte budget, and signal
        // via `truncated` with `count` = true total.
        let total = jobs.len();
        let mut rendered: Vec<serde_json::Value> = Vec::new();
        let mut bytes = 0usize;
        for job in jobs.iter() {
            if rendered.len() >= MAX_CRON_PANEL_JOBS {
                break;
            }
            let row = cap_cron_row(job_json(job, now_ms));
            let size = serde_json::to_string(&row).map(|s| s.len()).unwrap_or(0);
            // Always include at least one capped row so an empty list never
            // masks a present job.
            if !rendered.is_empty() && bytes + size > CRON_LIST_BYTE_BUDGET {
                break;
            }
            bytes += size;
            rendered.push(row);
        }
        Ok(CronListApplicationResult {
            truncated: rendered.len() < total,
            count: total,
            jobs: rendered,
            gateway_running: running,
        })
    }

    pub(crate) async fn create(
        &self,
        identity: &AuthIdentity,
        headers: &HeaderMap,
        params: octos_core::ui_protocol::CronCreateParams,
    ) -> Result<CronJobApplicationResult, CronApplicationError> {
        let profile_id = self.resolve_profile_id(identity, headers, Some(&params.session_id))?;
        if params
            .session_id
            .profile_id()
            .is_some_and(|scope| scope != profile_id)
        {
            return Err(CronApplicationError::AuthScopeViolation);
        }
        let data_dir = self.profile_data_dir(&profile_id)?;
        let message = params.message.trim();
        if message.is_empty() {
            return Err(CronApplicationError::MessageRequired);
        }
        if matches!(params.schedule, CronCreateSchedule::Every { every_ms: 0 }) {
            return Err(CronApplicationError::EveryMsMustBePositive);
        }
        if let Some(timezone) = params.timezone.as_deref()
            && !octos_bus::cron_types::is_valid_cron_timezone(timezone)
        {
            return Err(CronApplicationError::InvalidTimezone);
        }
        let now_ms = Utc::now().timestamp_millis();
        let schedule = match params.schedule {
            CronCreateSchedule::At { at_ms } => {
                if !octos_bus::cron_types::is_supported_cron_run_at_ms(at_ms) {
                    return Err(CronApplicationError::InvalidAtMs);
                }
                octos_bus::CronSchedule::At { at_ms }
            }
            CronCreateSchedule::Every { every_ms } => {
                if every_ms <= 0 {
                    return Err(CronApplicationError::EveryMsMustBePositive);
                }
                if octos_bus::cron_types::next_every_run_at_ms(now_ms, every_ms).is_none() {
                    return Err(CronApplicationError::InvalidEveryMs);
                }
                octos_bus::CronSchedule::Every { every_ms }
            }
            CronCreateSchedule::Cron { expr } => {
                if expr.trim().is_empty() {
                    return Err(CronApplicationError::CronExprRequired);
                }
                if !octos_bus::cron_types::is_valid_cron_expression(
                    &expr,
                    params.timezone.as_deref(),
                ) {
                    return Err(CronApplicationError::InvalidCronExpr);
                }
                octos_bus::CronSchedule::Cron { expr }
            }
        };
        let mode = match params.mode {
            CronCreateMode::Agent => octos_bus::CronMode::Agent,
            CronCreateMode::Notify => octos_bus::CronMode::Notify,
        };
        let _mutation = CRON_MUTATION_LOCK.lock().await;
        if gateway_running(&self.state, &profile_id).await {
            return Err(CronApplicationError::GatewayRunning);
        }
        let service = mutation_service(&self.state, &profile_id, cron_path_for(&data_dir));
        let origin = octos_bus::CronOrigin {
            session_id: Some(params.session_id.0),
            profile_id: Some(profile_id.clone()),
            loop_id: None,
        };
        let name = message.chars().take(48).collect::<String>();
        let payload = octos_bus::CronPayload {
            message: message.to_owned(),
            deliver: params.deliver,
            channel: params.channel,
            chat_id: params.chat_id,
            mode,
        };
        let job = service
            .add_job_with_origin(name, schedule, payload, params.timezone, origin)
            .map_err(|_| CronApplicationError::CronStoreIo)?;
        let now_ms = Utc::now().timestamp_millis();
        Ok(CronJobApplicationResult {
            job: cap_cron_row(job_json(&job, now_ms)),
        })
    }

    pub(crate) async fn delete(
        &self,
        identity: &AuthIdentity,
        headers: &HeaderMap,
        job_id: &str,
        params: CronDeleteParams,
    ) -> Result<(), CronApplicationError> {
        let profile_id = self.resolve_profile_id(identity, headers, Some(&params.session_id))?;
        if params
            .session_id
            .profile_id()
            .is_some_and(|scope| scope != profile_id)
        {
            return Err(CronApplicationError::AuthScopeViolation);
        }
        let data_dir = self.profile_data_dir(&profile_id)?;
        let _mutation = CRON_MUTATION_LOCK.lock().await;
        if gateway_running(&self.state, &profile_id).await {
            return Err(CronApplicationError::GatewayRunning);
        }
        let service = mutation_service(&self.state, &profile_id, cron_path_for(&data_dir));
        let existed = service.list_all_jobs().iter().any(|job| job.id == job_id);
        if existed && !service.remove_job(job_id) {
            return Err(CronApplicationError::CronStoreIo);
        }
        Ok(())
    }

    pub(crate) async fn toggle(
        &self,
        identity: &AuthIdentity,
        headers: &HeaderMap,
        job_id: &str,
        enabled: bool,
        session_id: Option<&octos_core::SessionKey>,
    ) -> Result<CronJobApplicationResult, CronApplicationError> {
        let profile_id = self.resolve_profile_id(identity, headers, session_id)?;
        let data_dir = self.profile_data_dir(&profile_id)?;
        let _mutation = CRON_MUTATION_LOCK.lock().await;
        // Ownership checked INSIDE the lock: a gateway child (or a
        // ProfileRuntime) that came up while queued behind another toggle must
        // be respected, not written under.
        if gateway_running(&self.state, &profile_id).await {
            return Err(CronApplicationError::GatewayRunning);
        }
        let outcome = if let Some(service) = live_cron_service(&self.state, &profile_id) {
            toggle_via_service(&service, job_id, enabled)
        } else {
            apply_cron_toggle(&cron_path_for(&data_dir), job_id, enabled).await
        };
        match outcome {
            Ok(job) => {
                let now_ms = Utc::now().timestamp_millis();
                Ok(CronJobApplicationResult {
                    // Field-cap the single row too, so `cron/toggle` cannot blow
                    // the frame on an oversized tenant-controlled field.
                    job: cap_cron_row(job_json(&job, now_ms)),
                })
            }
            Err(ToggleError::NotFound) => Err(CronApplicationError::JobNotFound),
            Err(ToggleError::Io) => Err(CronApplicationError::CronStoreIo),
        }
    }
}
