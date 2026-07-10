//! `/api/my/cron*` — user-scoped cron viewer + enable toggle (web
//! parity audit P3 item 7: the book documents scheduled tasks, but the
//! dashboard had no cron surface at all — only the admin-token
//! `GET /api/admin/profiles/{id}/cron` list).
//!
//! Runtime-ownership constraint that shapes this API: `cron.json` has
//! up to three writers, and a bare file edit races all of them (each
//! holds the store in memory and rewrites the whole file on its own
//! events):
//!
//! 1. **The serve-side `ProfileRuntime`'s own `CronService`** (started
//!    for enabled profiles with an LLM). When one is live we route the
//!    toggle THROUGH it (`CronService::enable_job` — mutates the
//!    in-memory store, persists, recomputes `next_run_at_ms`, re-arms
//!    the timer), so there is no drift by construction.
//! 2. **A spawned gateway child process.** Its `CronService` lives in
//!    another process we cannot call into — the toggle REFUSES with
//!    `409 {reason: "gateway_running"}` and the SPA routes the user
//!    through the existing stop/start controls.
//! 3. **Nobody** — the file is the source of truth and the toggle
//!    applies atomically (unique tmp + rename), mirroring
//!    `enable_job`'s next-run semantics so a later `CronService::start`
//!    (which only recomputes jobs whose next run is `None`) does not
//!    fire a stale deadline.
//!
//! All serve-side mutations are serialized behind a process-wide lock
//! (concurrent PUTs would otherwise race the read-modify-write and drop
//! one another's changes). Residual race documented on the lock.
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path as AxumPath, State};
use axum::http::{HeaderMap, StatusCode};
use chrono::Utc;
use octos_bus::CronService;
use serde::Deserialize;

use super::AppState;
use super::router::AuthIdentity;
use crate::api::auth_handlers::resolve_my_profile_id;

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
        "next_in": next_in,
        "timezone": j.timezone,
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
    crate::api::ui_protocol::resolve_session_profile_runtime(state, Some(profile_id))
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

/// GET /api/my/cron
pub async fn my_cron(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<AuthIdentity>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let ps = state
        .profile_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let profile_id = resolve_my_profile_id(&identity, ps, &state, &headers)?;
    let profile = ps
        .get(&profile_id)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    let cron_path = cron_path_for(&ps.resolve_data_dir(&profile));

    let running = gateway_running(&state, &profile_id).await;
    // Prefer the live service's view — it is what the toggle path will
    // mutate, and it is fresher than the file between persists.
    let jobs = if let Some(svc) = live_cron_service(&state, &profile_id) {
        svc.list_all_jobs()
    } else {
        match read_cron_store(&cron_path).await {
            Ok(Some(store)) => store.jobs,
            Ok(None) => Vec::new(),
            Err(status) => return Err(status),
        }
    };

    let now_ms = Utc::now().timestamp_millis();
    let jobs: Vec<serde_json::Value> = jobs.iter().map(|j| job_json(j, now_ms)).collect();
    Ok(Json(serde_json::json!({
        "ok": true,
        "count": jobs.len(),
        "jobs": jobs,
        "gateway_running": running,
    })))
}

#[derive(Debug, Deserialize)]
pub struct ToggleBody {
    pub enabled: bool,
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
    // Unique per-write temp name: a concurrent writer using a shared
    // `cron.tmp` could rename OUR content away (or fail on a vanished
    // temp file). The mutation lock already serializes serve-side
    // writers; the unique name additionally keeps any out-of-process
    // writer from colliding on the temp path itself.
    static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let tmp_path = cron_path.with_extension(format!(
        "tmp-{}-{}",
        std::process::id(),
        TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    if tokio::fs::write(&tmp_path, &json).await.is_err() {
        return Err(ToggleError::Io);
    }
    if tokio::fs::rename(&tmp_path, cron_path).await.is_err() {
        let _ = tokio::fs::remove_file(&tmp_path).await;
        return Err(ToggleError::Io);
    }
    Ok(updated)
}

/// Route the toggle through a live `CronService` (it owns the store:
/// persists, recomputes next-run, re-arms its timer). Split from the
/// handler so the service path is unit-testable without a
/// `ProfileRuntime`.
pub(crate) fn toggle_via_service(
    svc: &Arc<CronService>,
    job_id: &str,
    enabled: bool,
) -> Result<octos_bus::CronJob, ToggleError> {
    if !svc.enable_job(job_id, enabled) {
        return Err(ToggleError::NotFound);
    }
    svc.list_all_jobs()
        .into_iter()
        .find(|j| j.id == job_id)
        // Present a beat ago under the store lock; racing deletion is
        // the only way to lose it. Treat as gone.
        .ok_or(ToggleError::NotFound)
}

/// PUT /api/my/cron/{job_id}/enabled
pub async fn set_my_cron_enabled(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<AuthIdentity>,
    AxumPath(job_id): AxumPath<String>,
    Json(body): Json<ToggleBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let err = |status: StatusCode, reason: &str| {
        (
            status,
            Json(serde_json::json!({ "ok": false, "reason": reason })),
        )
    };

    let ps = state
        .profile_store
        .as_ref()
        .ok_or_else(|| err(StatusCode::SERVICE_UNAVAILABLE, "profiles_unavailable"))?;
    let profile_id = resolve_my_profile_id(&identity, ps, &state, &headers)
        .map_err(|status| err(status, "auth"))?;
    let profile = ps
        .get(&profile_id)
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "profile_read_failed"))?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "profile_not_found"))?;

    // A spawned gateway child owns cron.json from another process; we
    // can neither call into it nor safely edit under it. Refuse and let
    // the SPA route the user through the stop/start controls.
    if gateway_running(&state, &profile_id).await {
        return Err(err(StatusCode::CONFLICT, "gateway_running"));
    }

    let _mutation = CRON_MUTATION_LOCK.lock().await;
    // Resolved INSIDE the lock: a runtime that came up while we waited
    // must be routed through, not written under.
    let outcome = if let Some(svc) = live_cron_service(&state, &profile_id) {
        toggle_via_service(&svc, &job_id, body.enabled)
    } else {
        let cron_path = cron_path_for(&ps.resolve_data_dir(&profile));
        apply_cron_toggle(&cron_path, &job_id, body.enabled).await
    };

    match outcome {
        Ok(job) => {
            let now_ms = Utc::now().timestamp_millis();
            Ok(Json(serde_json::json!({
                "ok": true,
                "job": job_json(&job, now_ms),
            })))
        }
        Err(ToggleError::NotFound) => Err(err(StatusCode::NOT_FOUND, "job_not_found")),
        Err(ToggleError::Io) => Err(err(StatusCode::INTERNAL_SERVER_ERROR, "cron_store_io")),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::ProfileStore;
    use crate::user_store::UserRole;

    fn make_user_profile(id: &str) -> crate::profiles::UserProfile {
        crate::profiles::UserProfile {
            id: id.into(),
            name: id.into(),
            enabled: true,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: crate::profiles::ProfileConfig::default(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    fn temp_state() -> (tempfile::TempDir, Arc<AppState>, Arc<ProfileStore>) {
        let dir = tempfile::tempdir().unwrap();
        let profile_store = Arc::new(ProfileStore::open(dir.path()).unwrap());
        let state = Arc::new(AppState {
            profile_store: Some(profile_store.clone()),
            ..AppState::empty_for_tests()
        });
        (dir, state, profile_store)
    }

    fn user_identity(id: &str) -> axum::Extension<AuthIdentity> {
        axum::Extension(AuthIdentity::User {
            id: id.into(),
            role: UserRole::User,
        })
    }

    fn make_job(id: &str, enabled: bool) -> octos_bus::CronJob {
        octos_bus::CronJob {
            id: id.into(),
            name: format!("job {id}"),
            enabled,
            schedule: octos_bus::CronSchedule::Every {
                every_ms: 1_800_000,
            },
            payload: octos_bus::CronPayload {
                message: "check the queue".into(),
                deliver: false,
                channel: Some("system".into()),
                chat_id: None,
            },
            state: Default::default(),
            created_at_ms: 1,
            delete_after_run: false,
            timezone: None,
        }
    }

    async fn seed_cron(
        ps: &ProfileStore,
        profile: &crate::profiles::UserProfile,
        jobs: Vec<octos_bus::CronJob>,
    ) -> PathBuf {
        let data_dir = ps.resolve_data_dir(profile);
        tokio::fs::create_dir_all(&data_dir).await.unwrap();
        let path = cron_path_for(&data_dir);
        let store = octos_bus::CronStore { version: 1, jobs };
        tokio::fs::write(&path, serde_json::to_string_pretty(&store).unwrap())
            .await
            .unwrap();
        path
    }

    #[tokio::test]
    async fn my_cron_lists_zero_state_without_file() {
        let (_dir, state, ps) = temp_state();
        ps.save(&make_user_profile("tenant")).unwrap();
        let Json(resp) = my_cron(State(state), HeaderMap::new(), user_identity("tenant"))
            .await
            .unwrap();
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["count"], 0);
        assert_eq!(resp["gateway_running"], false);
    }

    #[tokio::test]
    async fn my_cron_lists_jobs_in_admin_shape() {
        let (_dir, state, ps) = temp_state();
        let profile = make_user_profile("tenant2");
        ps.save(&profile).unwrap();
        seed_cron(
            &ps,
            &profile,
            vec![make_job("aa11", true), make_job("bb22", false)],
        )
        .await;

        let Json(resp) = my_cron(State(state), HeaderMap::new(), user_identity("tenant2"))
            .await
            .unwrap();
        assert_eq!(resp["count"], 2);
        assert_eq!(resp["jobs"][0]["id"], "aa11");
        assert_eq!(resp["jobs"][0]["enabled"], true);
        assert_eq!(resp["jobs"][1]["enabled"], false);
        assert_eq!(resp["jobs"][0]["message"], "check the queue");
    }

    #[tokio::test]
    async fn toggle_flips_and_persists_atomically() {
        let (_dir, state, ps) = temp_state();
        let profile = make_user_profile("tenant3");
        ps.save(&profile).unwrap();
        let path = seed_cron(&ps, &profile, vec![make_job("aa11", true)]).await;

        let Json(resp) = set_my_cron_enabled(
            State(state),
            HeaderMap::new(),
            user_identity("tenant3"),
            AxumPath("aa11".into()),
            Json(ToggleBody { enabled: false }),
        )
        .await
        .unwrap();
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["job"]["enabled"], false);

        // Persisted: a fresh read of cron.json reflects the flip and
        // the store shape (version + full job records) survived.
        let content = tokio::fs::read_to_string(&path).await.unwrap();
        let store: octos_bus::CronStore = serde_json::from_str(&content).unwrap();
        assert_eq!(store.version, 1);
        assert_eq!(store.jobs.len(), 1);
        assert!(!store.jobs[0].enabled);
        assert_eq!(store.jobs[0].payload.message, "check the queue");
    }

    #[tokio::test]
    async fn toggle_404s_on_unknown_job_and_missing_store() {
        let (_dir, state, ps) = temp_state();
        let profile = make_user_profile("tenant4");
        ps.save(&profile).unwrap();

        // No cron.json at all.
        let missing_store = set_my_cron_enabled(
            State(state.clone()),
            HeaderMap::new(),
            user_identity("tenant4"),
            AxumPath("aa11".into()),
            Json(ToggleBody { enabled: false }),
        )
        .await;
        assert_eq!(
            missing_store.err().map(|(s, _)| s),
            Some(StatusCode::NOT_FOUND)
        );

        // Store exists but the id doesn't.
        seed_cron(&ps, &profile, vec![make_job("aa11", true)]).await;
        let unknown = set_my_cron_enabled(
            State(state),
            HeaderMap::new(),
            user_identity("tenant4"),
            AxumPath("zz99".into()),
            Json(ToggleBody { enabled: false }),
        )
        .await;
        assert_eq!(unknown.err().map(|(s, _)| s), Some(StatusCode::NOT_FOUND));
    }

    #[tokio::test]
    async fn apply_cron_toggle_is_scoped_to_the_addressed_job() {
        // Pure file-level core: flipping one job leaves siblings alone.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cron.json");
        let store = octos_bus::CronStore {
            version: 1,
            jobs: vec![make_job("one", true), make_job("two", true)],
        };
        tokio::fs::write(&path, serde_json::to_string(&store).unwrap())
            .await
            .unwrap();

        let updated = apply_cron_toggle(&path, "two", false).await.unwrap();
        assert_eq!(updated.id, "two");
        let reread: octos_bus::CronStore =
            serde_json::from_str(&tokio::fs::read_to_string(&path).await.unwrap()).unwrap();
        assert!(reread.jobs[0].enabled, "sibling job must be untouched");
        assert!(!reread.jobs[1].enabled);
    }

    #[tokio::test]
    async fn apply_cron_toggle_matches_cron_service_next_run_semantics() {
        // Disabling clears a stale deadline; re-enabling recomputes it.
        // (CronService::start only recomputes jobs whose next run is
        // None — a stale next_run_at_ms would fire immediately.)
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cron.json");
        let mut job = make_job("one", true);
        job.state.next_run_at_ms = Some(123); // long past
        let store = octos_bus::CronStore {
            version: 1,
            jobs: vec![job],
        };
        tokio::fs::write(&path, serde_json::to_string(&store).unwrap())
            .await
            .unwrap();

        let disabled = apply_cron_toggle(&path, "one", false).await.unwrap();
        assert_eq!(disabled.state.next_run_at_ms, None);
        let reread: octos_bus::CronStore =
            serde_json::from_str(&tokio::fs::read_to_string(&path).await.unwrap()).unwrap();
        assert_eq!(reread.jobs[0].state.next_run_at_ms, None);

        let enabled = apply_cron_toggle(&path, "one", true).await.unwrap();
        let now_ms = Utc::now().timestamp_millis();
        let next = enabled.state.next_run_at_ms.expect("recomputed next run");
        assert!(
            next > now_ms,
            "every-30m job must be scheduled in the future, got {next} vs now {now_ms}"
        );
    }

    #[tokio::test]
    async fn toggle_via_service_routes_through_the_owning_store() {
        // The service path: enable_job mutates the IN-MEMORY store and
        // persists — the file reflects the flip without us touching it.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cron.json");
        let mut job = make_job("svc1", true);
        job.state.next_run_at_ms = Some(123);
        let store = octos_bus::CronStore {
            version: 1,
            jobs: vec![job],
        };
        tokio::fs::write(&path, serde_json::to_string(&store).unwrap())
            .await
            .unwrap();

        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let svc = Arc::new(CronService::new(&path, tx));

        let toggled = toggle_via_service(&svc, "svc1", false).unwrap();
        assert!(!toggled.enabled);
        assert_eq!(toggled.state.next_run_at_ms, None, "deadline cleared");
        let reread: octos_bus::CronStore =
            serde_json::from_str(&tokio::fs::read_to_string(&path).await.unwrap()).unwrap();
        assert!(!reread.jobs[0].enabled, "service persisted the flip");

        assert!(matches!(
            toggle_via_service(&svc, "nope", false),
            Err(ToggleError::NotFound)
        ));
    }

    #[tokio::test]
    async fn concurrent_toggles_both_persist() {
        // Two PUTs for DIFFERENT jobs racing: without serialization one
        // read-modify-write can swallow the other. Both flips must land.
        let (_dir, state, ps) = temp_state();
        let profile = make_user_profile("tenant5");
        ps.save(&profile).unwrap();
        let path = seed_cron(
            &ps,
            &profile,
            vec![make_job("one", true), make_job("two", true)],
        )
        .await;

        let (a, b) = tokio::join!(
            set_my_cron_enabled(
                State(state.clone()),
                HeaderMap::new(),
                user_identity("tenant5"),
                AxumPath("one".into()),
                Json(ToggleBody { enabled: false }),
            ),
            set_my_cron_enabled(
                State(state.clone()),
                HeaderMap::new(),
                user_identity("tenant5"),
                AxumPath("two".into()),
                Json(ToggleBody { enabled: false }),
            )
        );
        assert!(a.is_ok() && b.is_ok());

        let reread: octos_bus::CronStore =
            serde_json::from_str(&tokio::fs::read_to_string(&path).await.unwrap()).unwrap();
        assert!(
            reread.jobs.iter().all(|j| !j.enabled),
            "both flips must survive: {reread:?}"
        );
    }
}
