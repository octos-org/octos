//! `octos steer` — external reviewer steer channel (OLP P2, slice 1).
//!
//! Contract: task-req-olp-ctrl-steer.spec.md — writes a durable steer to
//! the session's `.reviewer-notes` sidecar (SAME flock+append protocol
//! as `.monitor-notes`, same 64KiB batch cap; a single oversize steer is
//! REJECTED at enqueue time). Injection level is user-message data —
//! NEVER a system instruction (operator 拍板, 两次实测).
//!
//! Scenario bindings: "steer 目标 session 不存在时报错" (unknown session
//! → non-zero exit, NO queue file created) and "超限 steer 在入队时被
//! 拒绝" (>64KiB → non-zero exit, queue untouched).

use std::path::{Path, PathBuf};

use clap::Args;
use eyre::Result;

use super::Executable;

/// Reader cap shared with the monitor-notes channel — a single steer
/// larger than this can never be consumed, so it is refused at enqueue.
pub(crate) const STEER_MAX_BYTES: usize = 64 * 1024;

#[derive(Debug, Args)]
pub struct SteerCommand {
    /// Target session key (e.g. the master session).
    #[arg(long)]
    pub session: String,
    /// Steer text (the reviewer instruction, injected as DATA).
    #[arg(long)]
    pub text: String,
    /// Data-dir override (defaults to the standard resolution).
    #[arg(long, value_name = "DIR")]
    pub data_dir: Option<PathBuf>,
}

/// `.reviewer-notes` sidecar paths, mirroring `monitor_notes_paths`
/// (same inbox naming scheme, same hash).
pub(crate) fn reviewer_notes_paths(data_dir: &Path, session_id: &str) -> (PathBuf, PathBuf) {
    let safe_session = crate::autonomy::hash_session_for_inbox(session_id);
    let inbox = data_dir.join("inbox");
    (
        inbox.join(format!("{safe_session}.reviewer-notes")),
        inbox.join(format!("{safe_session}.reviewer-notes.lock")),
    )
}

/// Whether a session has ANY persistent state we can steer into. The
/// contract's "session 不存在" check uses the RUNTIME's real session
/// store (外环整改: NOT the instance `data/sessions/` dir, which is
/// empty in real deployments): the per-project store at
/// `<cwd>/.octos/<profile>/sessions/…` that the running session actually
/// writes (jsonl transcripts). Resolved through the SAME path helpers
/// the runtime uses (`runtime::session::project_sessions_root` +
/// `octos_bus::session::encode_path_component`) — never a hand-built
/// encoding. Also accepts inbox state (a session that has notes/locks
/// from goal or monitor activity is equally real).
pub(crate) fn session_has_persistent_state(
    instance_data_dir: &Path,
    cwd: &Path,
    profile_id: &str,
    session_id: &str,
) -> bool {
    // Primary source: the runtime session store (jsonl transcripts).
    let sessions_root = crate::runtime::session::project_sessions_root(cwd, profile_id);
    let key = octos_core::SessionKey(session_id.to_owned());
    let base_key = key.base_key();
    let encoded_base = octos_bus::session::encode_path_component(base_key);
    let topic = key.topic().unwrap_or("default");
    let encoded_topic = octos_bus::session::encode_path_component(topic);
    // Flat layout: <root>/sessions/<key>.jsonl
    let flat = sessions_root.join("sessions").join(format!(
        "{}.jsonl",
        octos_bus::session::encode_path_component(session_id)
    ));
    if flat.exists() {
        return true;
    }
    // Per-user layout: <root>/users/<base>/sessions/<topic>.jsonl AND the
    // observed real layout <root>/sessions/<base>/sessions/<topic>.jsonl
    // (project store nests per-user under sessions/).
    let per_user_a = sessions_root
        .join("users")
        .join(&encoded_base)
        .join("sessions")
        .join(format!("{encoded_topic}.jsonl"));
    if per_user_a.exists() {
        return true;
    }
    let per_user_b = sessions_root
        .join("sessions")
        .join(&encoded_base)
        .join("sessions")
        .join(format!("{encoded_topic}.jsonl"));
    if per_user_b.exists() {
        return true;
    }
    // Fallback: inbox state (goal/monitor notes for this session).
    let safe_session = crate::autonomy::hash_session_for_inbox(session_id);
    let inbox = instance_data_dir.join("inbox");
    if let Ok(entries) = std::fs::read_dir(&inbox) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(&safe_session) {
                return true;
            }
        }
    }
    false
}

/// Append one steer line (flock+append, monitor-notes idiom). Returns
/// Err on any IO failure; the caller maps that to a non-zero exit.
pub(crate) fn append_reviewer_steer(
    data_dir: &Path,
    session_id: &str,
    text: &str,
) -> Result<(), String> {
    let (note_path, lock_path) = reviewer_notes_paths(data_dir, session_id);
    let inbox_dir = note_path.parent().expect("notes path has inbox parent");
    std::fs::create_dir_all(inbox_dir)
        .map_err(|e| format!("failed to create inbox dir {}: {e}", inbox_dir.display()))?;
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // One steer = one line; control chars stay data.
    let line = format!("{timestamp} {}\n", text.replace('\n', " "));
    // fs2::FileExt provides the MSRV-1.85-compatible lock/unlock (std's
    // inherent methods are 1.89+). #8c ③: an EXCLUSIVE lock — a shared
    // lock lets two concurrent `octos steer` processes interleave appends
    // (torn lines), which the per-line content-hash dedupe cannot
    // reconstruct; the exclusive lock serializes writers.
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&lock_path)
        .map_err(|e| format!("failed to open reviewer lock {}: {e}", lock_path.display()))?;
    fs2::FileExt::lock_exclusive(&lock_file)
        .map_err(|e| format!("failed to lock reviewer notes {}: {e}", lock_path.display()))?;
    let result = (|| {
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&note_path)
            .map_err(|e| format!("failed to open reviewer notes {}: {e}", note_path.display()))?;
        file.write_all(line.as_bytes())
            .map_err(|e| format!("failed to append reviewer note: {e}"))?;
        file.sync_all()
            .map_err(|e| format!("failed to fsync reviewer note: {e}"))
    })();
    let _ = fs2::FileExt::unlock(&lock_file);
    // #8c ④: the session marker is NOT best-effort — the cross-process
    // wake is unaddressable without it, so a write failure must FAIL the
    // CLI (non-zero exit) instead of reporting `queued` for a steer no
    // sweep can ever route.
    if result.is_ok() {
        let session_marker = note_path.with_extension("reviewer-session");
        if let Err(error) = std::fs::write(&session_marker, session_id) {
            return Err(format!(
                "steer notes written but the session marker {} failed: {error} — \
                 the steer is NOT addressable; refusing to report queued",
                session_marker.display()
            ));
        }
    }
    result
}

impl Executable for SteerCommand {
    fn execute(self) -> Result<()> {
        // Contract scenario "超限 steer 在入队时被拒绝": reject BEFORE any
        // filesystem side effect.
        if self.text.len() > STEER_MAX_BYTES {
            eprintln!(
                "error: steer text is {} bytes, over the {}-byte limit; rejected at enqueue",
                self.text.len(),
                STEER_MAX_BYTES
            );
            std::process::exit(1);
        }
        let state_home = super::resolve_data_dir(self.data_dir.clone())?;
        let cwd = std::env::current_dir()?;
        let data_dir = super::obs::resolve_profile_data_root(
            &state_home,
            &cwd,
            super::obs::DEFAULT_PROFILE_ID,
        );
        // Contract scenario "steer 目标 session 不存在时报错": refuse and
        // create NOTHING. Failure semantics keep the resolved path visible
        // (整改: now the runtime session store root, not the empty
        // instance data/sessions/).
        let sessions_root =
            crate::runtime::session::project_sessions_root(&cwd, super::obs::DEFAULT_PROFILE_ID);
        if !session_has_persistent_state(
            &data_dir,
            &cwd,
            super::obs::DEFAULT_PROFILE_ID,
            &self.session,
        ) {
            eprintln!(
                "error: unknown session `{}` (no session transcript under {})",
                self.session,
                sessions_root.display()
            );
            std::process::exit(1);
        }
        match append_reviewer_steer(&data_dir, &self.session, &self.text) {
            Ok(()) => {
                let (note_path, _) = reviewer_notes_paths(&data_dir, &self.session);
                println!("steer queued: {}", note_path.display());
                // OLP-CTRL slice 2: wake the steered session through the
                // SAME continuation mechanism the goal-progress wake uses.
                // In-process only: when octos runs the serve/scheduler, the
                // enqueue lands on the live orchestrator; from a cold CLI
                // (serve in another process) the durable .reviewer-notes
                // sidecar is the doorbell and the session's next tick /
                // turn consumes it — the continuation enqueue is a
                // best-effort accelerator, never the correctness path.
                crate::autonomy::agent_orchestrator::default_agent_orchestrator()
                    .enqueue_steer_continuation(
                        &octos_core::SessionKey(self.session.clone()),
                        super::obs::DEFAULT_PROFILE_ID,
                    );
                Ok(())
            }
            Err(error) => {
                eprintln!("error: failed to queue steer: {error}");
                std::process::exit(1);
            }
        }
    }
}
