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
