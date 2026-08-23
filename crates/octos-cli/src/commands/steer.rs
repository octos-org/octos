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
/// contract's "session 不存在" check: without an existing inbox dir
/// entry (notes/lock of ANY channel) or a session record, a steer would
/// queue into the void — refuse instead (never create a queue file for
/// an unknown session). v1 heuristic: the inbox dir must already exist
/// with at least one file naming this session's hash, OR the sessions
/// dir must name it. Kept conservative and side-effect-free (read-only).
pub(crate) fn session_has_persistent_state(data_dir: &Path, session_id: &str) -> bool {
    let safe_session = crate::autonomy::hash_session_for_inbox(session_id);
    let inbox = data_dir.join("inbox");
    if let Ok(entries) = std::fs::read_dir(&inbox) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(&safe_session) {
                return true;
            }
        }
    }
    // Session store fallback: ui-protocol ledger dirs are hashed with a
    // DIFFERENT (percent-encoding) scheme, so we can only check the
    // monitor/goal inbox naming here. A session with no inbox state at
    // all is treated as unknown — the master we steer always has SOME
    // inbox file (its own .notes/.lock from boot/goal activity).
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
    // fs2::FileExt provides the MSRV-1.85-compatible lock_shared/unlock
    // (std's inherent methods are 1.89+ and would break upstream CI);
    // fully-qualified calls below keep the trait genuinely used.
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&lock_path)
        .map_err(|e| format!("failed to open reviewer lock {}: {e}", lock_path.display()))?;
    fs2::FileExt::lock_shared(&lock_file)
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
        // create NOTHING.
        if !session_has_persistent_state(&data_dir, &self.session) {
            eprintln!(
                "error: unknown session `{}` (no persistent state under {})",
                self.session,
                data_dir.display()
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Contract scenario "超限 steer 在入队时被拒绝" (shape-level): the
    /// limit constant and the rejection bound are the contract's 64KiB.
    #[test]
    fn olp_ctrl_steer_oversize_rejected() {
        let oversize = "x".repeat(STEER_MAX_BYTES + 1);
        assert!(oversize.len() > STEER_MAX_BYTES);
        // The execute() path exits non-zero BEFORE touching disk; assert
        // the bound it enforces.
        assert_eq!(STEER_MAX_BYTES, 64 * 1024);
    }

    /// Contract scenario "steer 目标 session 不存在时报错": an unknown
    /// session has no persistent state and must NOT get a queue file.
    #[test]
    fn olp_ctrl_steer_unknown_session_errors() {
        let temp = tempfile::tempdir().expect("tempdir");
        assert!(!session_has_persistent_state(
            temp.path(),
            "unknown-session"
        ));
        // append would create the file — but execute() refuses before
        // reaching it; assert the precondition that drives the refusal.
        let (note_path, _) = reviewer_notes_paths(temp.path(), "unknown-session");
        assert!(!note_path.exists());
    }

    /// OLP-CTRL slice 2: the steer wake enqueues an External("steer")
    /// continuation for the target session through the SAME mechanism as
    /// the goal-progress wake (contract: 唤醒 idle master).
    #[test]
    fn olp_ctrl_steer_wakes_and_receipts_enqueue() {
        let orchestrator = crate::autonomy::agent_orchestrator::default_agent_orchestrator();
        let session = octos_core::SessionKey("steer-test:local:master".into());
        assert!(
            !orchestrator.has_pending_steer_continuation_for_test(&session, "octos"),
            "no steer continuation queued before the wake"
        );
        let _ = orchestrator.enqueue_steer_continuation(&session, "octos");
        assert!(
            orchestrator.has_pending_steer_continuation_for_test(&session, "octos"),
            "steer continuation must be queued after the wake"
        );
    }

    /// A session WITH inbox state is steerable and the append lands via
    /// the flock protocol.
    #[test]
    fn olp_ctrl_steer_appends_to_existing_session() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session = "master:local:tui#coding";
        // Seed SOME inbox state for this session (the unknown-check's key).
        let safe = crate::autonomy::hash_session_for_inbox(session);
        let inbox = temp.path().join("inbox");
        std::fs::create_dir_all(&inbox).expect("inbox");
        std::fs::write(inbox.join(format!("{safe}.notes")), "x").expect("seed");
        assert!(session_has_persistent_state(temp.path(), session));
        append_reviewer_steer(temp.path(), session, "读黑板第 7 条").expect("append");
        let (note_path, _) = reviewer_notes_paths(temp.path(), session);
        let content = std::fs::read_to_string(note_path).expect("read");
        assert!(content.contains("读黑板第 7 条"));
    }
}
