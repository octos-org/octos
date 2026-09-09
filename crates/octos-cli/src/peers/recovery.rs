//! Restart authority for the logical peer lifetime, distinct from a worker.

use super::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const LIFETIME_FILE: &str = "lifetime.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LifetimePhase {
    Pending,
    Running,
    Idle,
    Failed,
}

/// This is execution authority, not the best-effort brief/result audit index.
/// Legacy peers without it are deliberately NOT guessed idle on recovery.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PeerLifetime {
    version: u8,
    task_id: String,
    registry_key: String,
    master: String,
    generation: u64,
    phase: LifetimePhase,
    turn_id: Option<String>,
    result_digest: Option<String>,
}

pub(crate) struct PeerLifetimeTurn {
    peer_dir: PathBuf,
    task_id: String,
    generation: u64,
    turn_id: String,
}

fn lifetime_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

fn read_lifetime(dir: &Path) -> Option<PeerLifetime> {
    let record: PeerLifetime = serde_json::from_str(&peer_io::read_peer_file(
        dir,
        LIFETIME_FILE,
        peer_io::PEER_FILE_READ_CAP_SMALL,
    )?)
    .ok()?;
    (record.version == 1
        && !record.task_id.is_empty()
        && peer_io::read_peer_file(dir, "originator", peer_io::PEER_FILE_READ_CAP_SMALL)
            .is_some_and(|master| master.trim() == record.master))
    .then_some(record)
}

/// A validated (fail-closed) projection view of `lifetime.json` — the
/// trusted fields the status derivation needs. Returned by
/// [`trusted_lifetime_projection`]; private `PeerLifetime` internals stay in
/// this module.
pub(crate) struct TrustedLifetimeView {
    pub(crate) phase: LifetimePhase,
    pub(crate) generation: u64,
    pub(crate) turn_id: Option<String>,
    pub(crate) master: String,
    pub(crate) task_id: String,
}

/// task-evo-peer-turn-status — TRUSTED read-only projection of a peer's
/// `lifetime.json` for status derivation, or `None` when ANY fail-closed
/// check fails (⇒ the caller must report execution=unknown with all identity
/// fields null).
///
/// Layered on the typed [`read_lifetime`] parse (strict enum + Option field
/// types — a numeric `turn_id` or `result_digest` fails DESERIALIZATION
/// here rather than being silently coerced), then adds the checks the status
/// contract needs beyond recovery's own restore gate:
///
/// * `registry_key` must equal `peer_wire_key(profile_id, slug)` — a
///   lifetime staged under another profile/runtime (same slug) is not this
///   peer's current authority.
/// * Writer-shape turn binding, per the REAL writers (outer-loop probe
///   ../outer-peer-projection-probe.json, 0 passed / 2 failed against the
///   first draft): `invalidate_peer_lifetime_for_input` deliberately KEEPS
///   the previous `turn_id` when flipping to Pending (and
///   `finish_peer_lifetime_turn(queued=true)` produces Pending+Some too), so
///   **Pending accepts both None and a valid Some** — rejecting Some would
///   demote every real round2-queued peer to unknown, the exact bug this
///   task exists to fix. Running/Idle/Failed still REQUIRE a valid Some
///   (begin sets it fresh; finish keeps it). Readers interpret a Pending
///   record together with `generation`/`phase`, never from turn_id alone.
/// * Idle additionally binds to the CURRENT durable result: the fd-anchored
///   result.md bytes must SHA256 to the recorded `result_digest` (blank,
///   wrong, or missing digest, or an overwritten result.md ⇒ untrusted).
///   Digest comparison only — the body is never parsed for an outcome.
pub(crate) fn trusted_lifetime_projection(
    dir: &Path,
    profile_id: &str,
    slug: &str,
) -> Option<TrustedLifetimeView> {
    let record = read_lifetime(dir)?;
    if record.registry_key != peer_wire_key(profile_id, slug) {
        return None;
    }
    match (&record.phase, record.turn_id.as_deref()) {
        // The real queue writers keep the PREVIOUS turn's id on Pending —
        // both None (fresh binding) and a NON-EMPTY Some (after any
        // completed turn) are legal production shapes.
        (LifetimePhase::Pending, None) => {}
        (LifetimePhase::Pending, Some(_)) => {}
        (LifetimePhase::Running | LifetimePhase::Idle | LifetimePhase::Failed, Some(_)) => {}
        _ => return None,
    }
    // An empty-string turn_id is never legal (the writers only ever write a
    // real id): refuse it explicitly — the match arms above can't express
    // "non-empty" without a guard.
    if record.turn_id.as_deref().is_some_and(str::is_empty) {
        return None;
    }
    // Empty identity strings are not valid authority even when the legacy
    // restore reader tolerates them: a projection that vouches for an empty
    // master/turn_id fences nothing (outer-loop review #4).
    if record.master.trim().is_empty() || record.task_id.trim().is_empty() {
        return None;
    }
    if record.phase == LifetimePhase::Idle {
        let digest = record.result_digest.as_deref()?;
        let body = peer_io::read_peer_file(dir, "result.md", peer_io::PEER_FILE_READ_CAP_LARGE)?;
        let actual = format!("{:x}", Sha256::digest(body.as_bytes()));
        if actual != digest {
            return None;
        }
    }
    Some(TrustedLifetimeView {
        phase: record.phase,
        generation: record.generation,
        turn_id: record.turn_id,
        master: record.master,
        task_id: record.task_id,
    })
}

fn write_lifetime(dir: &Path, record: &PeerLifetime) -> std::io::Result<()> {
    let json = serde_json::to_string(record).map_err(std::io::Error::other)?;
    peer_io::write_peer_file_atomic(dir, LIFETIME_FILE, &json)
}

pub(crate) fn record_peer_lifetime_binding(
    peers_root: &Path,
    profile: &str,
    slug: &str,
    master: &str,
    task_id: &str,
) -> std::io::Result<()> {
    let _guard = lifetime_lock().lock().unwrap_or_else(|e| e.into_inner());
    let dir = staged_peer_dir(peers_root, slug)
        .ok_or_else(|| std::io::Error::other("peer lifetime requires a staged directory"))?;
    peer_send_input_authorized(peers_root, slug, master).map_err(std::io::Error::other)?;
    write_lifetime(
        &dir,
        &PeerLifetime {
            version: 1,
            task_id: task_id.to_owned(),
            registry_key: peer_wire_key(profile, slug),
            master: master.to_owned(),
            generation: 0,
            phase: LifetimePhase::Pending,
            turn_id: None,
            result_digest: None,
        },
    )
}

/// Invalidate the previous completion BEFORE accepting a follow-up. A failed
/// durable write rejects delivery, rather than leaving a false idle receipt.
pub(crate) fn invalidate_peer_lifetime_for_input(
    peers_root: &Path,
    slug: &str,
) -> std::io::Result<()> {
    let _guard = lifetime_lock().lock().unwrap_or_else(|e| e.into_inner());
    let Some(dir) = staged_peer_dir(peers_root, slug) else {
        return Ok(());
    };
    let Some(mut record) = read_lifetime(&dir) else {
        return Ok(());
    };
    record.generation = record
        .generation
        .checked_add(1)
        .ok_or_else(|| std::io::Error::other("peer lifetime input generation exhausted"))?;
    record.phase = LifetimePhase::Pending;
    record.result_digest = None;
    write_lifetime(&dir, &record)
}

/// Called immediately before actual dispatch. A dropped/aborted turn leaves
/// Running on disk; an old completion cannot clear a newer input generation.
pub(crate) fn begin_peer_lifetime_turn(
    peers_root: &Path,
    session: &SessionKey,
    turn_id: &str,
) -> std::io::Result<Option<PeerLifetimeTurn>> {
    let Some((profile, slug)) = peer_slug_and_profile(session) else {
        return Ok(None);
    };
    let _guard = lifetime_lock().lock().unwrap_or_else(|e| e.into_inner());
    let Some(dir) = staged_peer_dir(peers_root, slug) else {
        return Ok(None);
    };
    let Some(mut record) = read_lifetime(&dir) else {
        return Ok(None);
    };
    if record.registry_key != peer_wire_key(profile, slug) {
        return Ok(None);
    }
    record.phase = LifetimePhase::Running;
    record.turn_id = Some(turn_id.to_owned());
    record.result_digest = None;
    write_lifetime(&dir, &record)?;
    // A peer can reconnect before its master restores. It is genuinely live
    // now, so that later restore must not reap its logical lifetime either.
    peer_task_registry().bind(record.registry_key, record.task_id.clone());
    Ok(Some(PeerLifetimeTurn {
        peer_dir: dir,
        task_id: record.task_id,
        generation: record.generation,
        turn_id: turn_id.to_owned(),
    }))
}

/// Publish idle only after the authoritative latest result was durably written.
/// A queued next input, write failure, error or stale completion cannot certify
/// this lifetime idle. The result digest binds the receipt to those exact bytes.
pub(crate) fn finish_peer_lifetime_turn(
    token: &PeerLifetimeTurn,
    result: &str,
    completed: bool,
    has_queued_input: bool,
) -> std::io::Result<()> {
    let _guard = lifetime_lock().lock().unwrap_or_else(|e| e.into_inner());
    let Some(mut record) = read_lifetime(&token.peer_dir) else {
        return Ok(());
    };
    if record.task_id != token.task_id
        || record.generation != token.generation
        || record.turn_id.as_deref() != Some(token.turn_id.as_str())
        || record.phase != LifetimePhase::Running
    {
        return Ok(());
    }
    if completed && !has_queued_input {
        let durable = peer_io::read_peer_file(
            &token.peer_dir,
            "result.md",
            peer_io::PEER_FILE_READ_CAP_LARGE,
        );
        if durable.as_deref() != Some(result) {
            return Ok(());
        }
        record.phase = LifetimePhase::Idle;
        record.result_digest = Some(format!("{:x}", Sha256::digest(result.as_bytes())));
    } else {
        record.phase = if completed {
            LifetimePhase::Pending
        } else {
            LifetimePhase::Failed
        };
        record.result_digest = None;
    }
    write_lifetime(&token.peer_dir, &record)
}

pub(crate) fn enable_peer_task_persistence(
    supervisor: &octos_agent::TaskSupervisor,
    path: impl Into<PathBuf>,
    peers_root: &Path,
    profile: &str,
    master: &str,
) -> std::io::Result<usize> {
    supervisor.enable_persistence_with_recovery(path, |supervisor, tasks| {
        let _guard = lifetime_lock().lock().unwrap_or_else(|e| e.into_inner());
        let prefix = format!("{profile}:peer:");
        for task in tasks {
            if !task.status.is_active()
                || task.tool_name != "peer_handoff"
                || task.parent_session_key.as_deref() != Some(master)
                || task.session_key.as_deref() != Some(master)
            {
                continue;
            }
            let Some(slug) = task.tool_call_id.strip_prefix(&prefix) else {
                continue;
            };
            let Some(dir) = staged_peer_dir(peers_root, slug) else {
                continue;
            };
            let Some(record) = read_lifetime(&dir) else {
                continue;
            };
            if record.task_id != task.id
                || record.registry_key != task.tool_call_id
                || record.master != master
            {
                continue;
            }
            if peer_is_closed(peers_root, slug) {
                // Only the exact durable binding can retire a closed lifetime;
                // never take a different same-slug registry entry.
                peer_task_registry().take_if_task(&task.tool_call_id, &task.id);
                supervisor.mark_completed(&task.id, Vec::new());
            } else if record.phase == LifetimePhase::Idle
                && record.turn_id.as_ref().is_some_and(|id| !id.is_empty())
                && peer_io::read_peer_file(&dir, "result.md", peer_io::PEER_FILE_READ_CAP_LARGE)
                    .is_some_and(|result| {
                        record.result_digest.as_deref()
                            == Some(format!("{:x}", Sha256::digest(result.as_bytes())).as_str())
                    })
            {
                peer_task_registry().bind(task.tool_call_id.clone(), task.id.clone());
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_restore_completed_open_peer_before_restart_orphan_sweep() {
        let temp = tempfile::TempDir::new().unwrap();
        let peers = temp.path().join("peers");
        let peer = peers.join("auditor");
        std::fs::create_dir_all(&peer).unwrap();
        let profile = format!("peer-recovery-{}", uuid::Uuid::now_v7());
        let master = format!("{profile}:local:master");
        let key = peer_wire_key(&profile, "auditor");
        let ledger = temp.path().join("tasks.jsonl");
        peer_io::write_peer_file_atomic(&peer, "brief.md", "review").unwrap();
        peer_io::write_peer_file_atomic(&peer, "originator", &master).unwrap();
        let staging = octos_agent::TaskSupervisor::new();
        staging.enable_persistence(&ledger).unwrap();
        let task_id = bind_peer_supervised_task(&staging, key.clone(), &master).unwrap();
        record_peer_lifetime_binding(&peers, &profile, "auditor", &master, &task_id).unwrap();
        let peer_session =
            SessionKey::with_profile_topic(&profile, "local", "peer", "peer-auditor");
        let token = begin_peer_lifetime_turn(&peers, &peer_session, "peer-turn-one")
            .unwrap()
            .unwrap();
        let result = "a completed peer result";
        peer_io::write_peer_file_atomic(&peer, "result.md", result).unwrap();
        finish_peer_lifetime_turn(&token, result, true, false).unwrap();
        peer_task_registry().take(&key);
        drop(staging);

        let restored = octos_agent::TaskSupervisor::new();
        let changes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let changed = changes.clone();
        restored.set_on_change(move |task| changed.lock().unwrap().push(task.id.clone()));
        enable_peer_task_persistence(&restored, &ledger, &peers, &profile, &master).unwrap();
        let row = restored.get_task(&task_id).unwrap();
        assert_eq!(
            row.error, None,
            "completed kept-open peer is not a dead worker"
        );
        assert!(
            changes.lock().unwrap().is_empty(),
            "no failed transition/wake"
        );
        // Inspect can be the first factory after boot; the later same-path
        // foreground factory must keep the recovered lease and remain quiet.
        enable_peer_task_persistence(&restored, &ledger, &peers, &profile, &master).unwrap();
        let next_factory = octos_agent::TaskSupervisor::new();
        enable_peer_task_persistence(&next_factory, &ledger, &peers, &profile, &master).unwrap();
        assert!(next_factory.get_task(&task_id).unwrap().status.is_active());
        invalidate_peer_lifetime_for_input(&peers, "auditor").unwrap();
        let second = begin_peer_lifetime_turn(&peers, &peer_session, "peer-turn-two")
            .unwrap()
            .unwrap();
        peer_io::write_peer_file_atomic(&peer, "result.md", "second result").unwrap();
        finish_peer_lifetime_turn(&second, "second result", true, false).unwrap();
        assert_eq!(read_lifetime(&peer).unwrap().phase, LifetimePhase::Idle);
        assert_eq!(
            retire_peer_supervised_task(&restored, &profile, "auditor"),
            Some(task_id)
        );
    }

    struct Fixture {
        _temp: tempfile::TempDir,
        peers: PathBuf,
        dir: PathBuf,
        ledger: PathBuf,
        profile: String,
        master: String,
        peer_session: SessionKey,
        key: String,
        task_id: String,
        supervisor: octos_agent::TaskSupervisor,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::TempDir::new().unwrap();
            let peers = temp.path().join("peers");
            let dir = peers.join("auditor");
            std::fs::create_dir_all(&dir).unwrap();
            let ledger = temp.path().join("tasks.jsonl");
            let profile = format!("lifetime-{}", uuid::Uuid::now_v7());
            let master = format!("{profile}:local:master");
            let peer_session =
                SessionKey::with_profile_topic(&profile, "local", "peer", "peer-auditor");
            let key = peer_wire_key(&profile, "auditor");
            peer_io::write_peer_file_atomic(&dir, "brief.md", "review").unwrap();
            peer_io::write_peer_file_atomic(&dir, "originator", &master).unwrap();
            let supervisor = octos_agent::TaskSupervisor::new();
            supervisor.enable_persistence(&ledger).unwrap();
            let task_id = bind_peer_supervised_task(&supervisor, key.clone(), &master).unwrap();
            record_peer_lifetime_binding(&peers, &profile, "auditor", &master, &task_id).unwrap();
            Self {
                _temp: temp,
                peers,
                dir,
                ledger,
                profile,
                master,
                peer_session,
                key,
                task_id,
                supervisor,
            }
        }

        fn begin(&self, id: &str) -> PeerLifetimeTurn {
            begin_peer_lifetime_turn(&self.peers, &self.peer_session, id)
                .unwrap()
                .unwrap()
        }

        fn complete(&self, token: &PeerLifetimeTurn, result: &str, pending: bool) {
            peer_io::write_peer_file_atomic(&self.dir, "result.md", result).unwrap();
            finish_peer_lifetime_turn(token, result, true, pending).unwrap();
        }

        fn restart(&self) -> octos_agent::TaskSupervisor {
            peer_task_registry().take_if_task(&self.key, &self.task_id);
            let restored = octos_agent::TaskSupervisor::new();
            enable_peer_task_persistence(
                &restored,
                &self.ledger,
                &self.peers,
                &self.profile,
                &self.master,
            )
            .unwrap();
            restored
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            peer_task_registry().take_if_task(&self.key, &self.task_id);
        }
    }

    // task-evo-peer-turn-status — REAL-writer regressions (ported verbatim
    // from the outer loop's probe, ../outer-peer-projection-under-test.rs;
    // receipt ../outer-peer-projection-probe.json). Both drive the PRODUCTION
    // writers (Fixture::begin/complete/invalidate) and then require the
    // read-side projection to keep trusting the record: the queue writers
    // DELIBERATELY keep the previous turn_id on Pending, so Pending+Some is a
    // legal, in-production shape — not a torn write.
    #[test]
    fn outer_peer_projection_accepts_real_followup_pending() {
        let f = Fixture::new();
        let first = f.begin("first-turn");
        f.complete(&first, "completed result", false);
        invalidate_peer_lifetime_for_input(&f.peers, "auditor").unwrap();
        let raw = read_lifetime(&f.dir).unwrap();
        assert_eq!(raw.phase, LifetimePhase::Pending);
        assert_eq!(raw.turn_id.as_deref(), Some("first-turn"));
        assert!(
            trusted_lifetime_projection(&f.dir, &f.profile, "auditor").is_some(),
            "real queue writer keeps previous turn_id: projection must retain queued authority"
        );
    }

    #[test]
    fn outer_peer_projection_accepts_finish_with_queued_input() {
        let f = Fixture::new();
        let first = f.begin("first-turn");
        f.complete(&first, "completed result", true);
        assert_eq!(read_lifetime(&f.dir).unwrap().phase, LifetimePhase::Pending);
        assert!(
            trusted_lifetime_projection(&f.dir, &f.profile, "auditor").is_some(),
            "real finish with queued input produces Pending+Some(turn_id), not an invalid record"
        );
    }

    #[test]
    fn should_require_durable_result_before_certifying_peer_idle() {
        let fixture = Fixture::new();
        let turn = fixture.begin("one");
        finish_peer_lifetime_turn(&turn, "not yet durable", true, false).unwrap();
        assert_eq!(
            read_lifetime(&fixture.dir).unwrap().phase,
            LifetimePhase::Running
        );
        fixture.complete(&turn, "durable", false);
        assert_eq!(
            read_lifetime(&fixture.dir).unwrap().phase,
            LifetimePhase::Idle
        );
        peer_io::write_peer_file_atomic(&fixture.dir, "result.md", "different result").unwrap();
        assert_eq!(
            fixture
                .restart()
                .get_task(&fixture.task_id)
                .unwrap()
                .error
                .as_deref(),
            Some("orphaned across restart")
        );
    }

    #[test]
    fn should_not_let_old_peer_completion_overwrite_new_input_generation() {
        let fixture = Fixture::new();
        let first = fixture.begin("one");
        invalidate_peer_lifetime_for_input(&fixture.peers, "auditor").unwrap();
        fixture.complete(&first, "old completion arrived late", false);
        let record = read_lifetime(&fixture.dir).unwrap();
        assert_eq!(record.phase, LifetimePhase::Pending);
        assert_eq!(record.generation, 1);
        assert!(record.result_digest.is_none());
        let second = fixture.begin("two");
        fixture.complete(&second, "second completion", false);
        fixture.complete(&first, "even later first completion", false);
        let record = read_lifetime(&fixture.dir).unwrap();
        assert_eq!(record.turn_id.as_deref(), Some("two"));
        // Late old output also cannot pass the exact result digest on reboot.
        assert_eq!(
            fixture
                .restart()
                .get_task(&fixture.task_id)
                .unwrap()
                .error
                .as_deref(),
            Some("orphaned across restart")
        );
    }

    #[test]
    fn should_keep_peer_pending_when_two_inputs_queue_before_first_dispatch() {
        use crate::autonomy::master_continuation_scheduler::MasterContinuationRuntimeState;
        let fixture = Fixture::new();
        let orchestrator = default_agent_orchestrator();
        for occurrence in ["one", "two"] {
            invalidate_peer_lifetime_for_input(&fixture.peers, "auditor").unwrap();
            let _ = orchestrator.enqueue_peer_send_input_continuation(
                &fixture.peer_session,
                &fixture.profile,
                "auditor",
                occurrence,
                "instruction",
            );
        }
        let (first, guard) = orchestrator.drain_and_claim_ready_continuation_for_session(
            &fixture.peer_session,
            &fixture.profile,
            MasterContinuationRuntimeState::idle(),
            1,
        );
        assert_eq!(first.len(), 1);
        let token = fixture.begin("first-dispatched");
        let (raced, _) = orchestrator.drain_and_claim_ready_continuation_for_session(
            &fixture.peer_session,
            &fixture.profile,
            MasterContinuationRuntimeState::idle(),
            1,
        );
        assert!(
            raced.is_empty(),
            "claim guard keeps second input visible in pending queue"
        );
        fixture.complete(
            &token,
            "first result",
            orchestrator.has_pending_peer_send_input_for_peer(&fixture.profile, "auditor"),
        );
        assert_eq!(
            read_lifetime(&fixture.dir).unwrap().phase,
            LifetimePhase::Pending
        );
        drop(guard);
        orchestrator.mark_continuation_completed(&first[0], None);
        let (second, guard) = orchestrator.drain_and_claim_ready_continuation_for_session(
            &fixture.peer_session,
            &fixture.profile,
            MasterContinuationRuntimeState::idle(),
            1,
        );
        assert_eq!(second.len(), 1);
        let token = fixture.begin("second-dispatched");
        fixture.complete(
            &token,
            "second result",
            orchestrator.has_pending_peer_send_input_for_peer(&fixture.profile, "auditor"),
        );
        assert_eq!(
            read_lifetime(&fixture.dir).unwrap().phase,
            LifetimePhase::Idle
        );
        drop(guard);
        orchestrator.mark_continuation_completed(&second[0], None);
    }

    #[test]
    fn should_preserve_real_peer_and_ordinary_worker_orphan_failures() {
        for phase in [
            LifetimePhase::Pending,
            LifetimePhase::Running,
            LifetimePhase::Failed,
        ] {
            let fixture = Fixture::new();
            if phase != LifetimePhase::Pending {
                let token = fixture.begin("unfinished");
                if phase == LifetimePhase::Failed {
                    finish_peer_lifetime_turn(&token, "", false, false).unwrap();
                }
            }
            let ordinary =
                fixture
                    .supervisor
                    .register("spawn", "ordinary-child", Some(&fixture.master));
            let restored = fixture.restart();
            for id in [&fixture.task_id, &ordinary] {
                assert_eq!(
                    restored.get_task(id).unwrap().error.as_deref(),
                    Some("orphaned across restart"),
                    "phase {phase:?}, task {id}"
                );
            }
        }
        let fixture = Fixture::new();
        let token = fixture.begin("done");
        fixture.complete(&token, "peer done while detached worker still runs", false);
        let ordinary =
            fixture
                .supervisor
                .register("spawn", "post-terminal-worker", Some(&fixture.master));
        let restored = fixture.restart();
        assert!(
            restored
                .get_task(&fixture.task_id)
                .unwrap()
                .status
                .is_active()
        );
        assert_eq!(
            restored.get_task(&ordinary).unwrap().error.as_deref(),
            Some("orphaned across restart")
        );
        fixture.supervisor.mark_failed(
            &fixture.task_id,
            "actual owner-reported peer failure".to_owned(),
        );
        let restored = fixture.restart();
        assert_eq!(
            restored
                .get_task(&fixture.task_id)
                .unwrap()
                .error
                .as_deref(),
            Some("actual owner-reported peer failure")
        );
        assert_eq!(
            restored.get_task(&ordinary).unwrap().error.as_deref(),
            Some("orphaned across restart")
        );
    }

    #[test]
    fn should_reject_wrong_peer_lifetime_task_owner_profile_and_legacy_authority() {
        for mismatch in ["task", "master", "originator", "profile", "legacy"] {
            let fixture = Fixture::new();
            let token = fixture.begin("done");
            fixture.complete(&token, "done", false);
            let mut record = read_lifetime(&fixture.dir).unwrap();
            match mismatch {
                "task" => record.task_id = "unrelated-task".to_owned(),
                "master" => record.master = "other-master".to_owned(),
                "originator" => {
                    peer_io::write_peer_file_atomic(&fixture.dir, "originator", "other-master")
                        .unwrap()
                }
                "profile" => record.registry_key = "other:peer:auditor".to_owned(),
                "legacy" => record.version = 0,
                _ => unreachable!(),
            }
            write_lifetime(&fixture.dir, &record).unwrap();
            assert_eq!(
                fixture
                    .restart()
                    .get_task(&fixture.task_id)
                    .unwrap()
                    .error
                    .as_deref(),
                Some("orphaned across restart"),
                "mismatch {mismatch}"
            );
        }
    }

    #[test]
    fn should_settle_closed_peer_only_for_exact_durable_lifetime() {
        for exact in [true, false] {
            let fixture = Fixture::new();
            peer_io::write_peer_file_atomic(&fixture.dir, "closed", "closed").unwrap();
            if !exact {
                let mut record = read_lifetime(&fixture.dir).unwrap();
                record.task_id = "another-task".to_owned();
                write_lifetime(&fixture.dir, &record).unwrap();
            }
            let row = fixture.restart().get_task(&fixture.task_id).unwrap();
            assert_eq!(row.status == octos_agent::TaskStatus::Completed, exact);
            assert_eq!(row.error.is_none(), exact);
        }
    }
}
