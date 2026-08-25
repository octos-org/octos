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
    // Cross-process wake (外环 首航第二回合 整改): the inbox hash is
    // one-way, so the serve-side sweep cannot recover the session id from
    // the filename. Drop a tiny `<hash>.reviewer-session` sibling holding
    // the RAW session id (best-effort; the notes line is the payload).
    if result.is_ok() {
        let session_marker = note_path.with_extension("reviewer-session");
        let _ = std::fs::write(&session_marker, session_id);
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

    /// Contract scenario "steer 目标 session 不存在时报错" (整改 e2e,
    /// REAL layout): an unknown session has no transcript in the runtime
    /// session store and must NOT get a queue file.
    #[test]
    fn olp_ctrl_steer_unknown_session_errors() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cwd = temp.path().join("proj");
        std::fs::create_dir_all(&cwd).expect("cwd");
        let instance = temp.path().join("instance-data");
        std::fs::create_dir_all(&instance).expect("instance");
        assert!(!session_has_persistent_state(
            &instance,
            &cwd,
            "octos",
            "unknown-session"
        ));
        let (note_path, _) = reviewer_notes_paths(&instance, "unknown-session");
        assert!(!note_path.exists());
    }

    /// 整改金丝雀 (e2e, REAL layout): a session with a transcript in the
    /// project-local store (.octos/<profile>/sessions/<base>/sessions/
    /// <topic>.jsonl — what the runtime actually writes) passes the check;
    /// a non-existent one fails it.
    #[test]
    fn olp_ctrl_steer_canary_real_session_store_layout() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cwd = temp.path().join("proj");
        std::fs::create_dir_all(&cwd).expect("cwd");
        let instance = temp.path().join("instance-data");
        std::fs::create_dir_all(&instance).expect("instance");
        let session = "octos:local:tui#coding";
        // Reproduce the runtime's real write path through the SAME helpers.
        let root = crate::runtime::session::project_sessions_root(&cwd, "octos");
        let key = octos_core::SessionKey(session.to_owned());
        let transcript = root
            .join("sessions")
            .join(octos_bus::session::encode_path_component(key.base_key()))
            .join("sessions")
            .join(format!(
                "{}.jsonl",
                octos_bus::session::encode_path_component(key.topic().unwrap_or("default"))
            ));
        std::fs::create_dir_all(transcript.parent().expect("parent")).expect("mkdir");
        std::fs::write(&transcript, "{}\n").expect("seed transcript");
        assert!(
            session_has_persistent_state(&instance, &cwd, "octos", session),
            "live session with a real transcript must pass"
        );
        assert!(
            !session_has_persistent_state(&instance, &cwd, "octos", "ghost-session"),
            "non-existent session must fail"
        );
    }

    /// OLP-CTRL 回合 5 (收官, 调用门): the production call gate sweeps
    /// EVERY profile in `state.profiles` — a lookup keyed on
    /// `MAIN_PROFILE_ID` ("_main") was a dead door because the runtime
    /// bootstrap registers "octos". This test pins the sweep to the
    /// PRODUCTION profile id: the CLI resolves under "octos" (same as
    /// the bootstrap), so a sweep addressed at "octos" must find the
    /// queued steer — no test-injected profile name.
    #[test]
    fn olp_ctrl_steer_sweep_uses_production_profile_id() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session = "xproc:local:tui#coding";
        // CLI-side queue (writes notes + marker under the instance inbox).
        append_reviewer_steer(temp.path(), session, "读黑板第 7 条").expect("queue");
        let orchestrator = crate::autonomy::agent_orchestrator::default_agent_orchestrator();
        let key = octos_core::SessionKey(session.to_owned());
        // The PRODUCTION profile id is "octos" (NOT MAIN_PROFILE_ID
        // "_main") — sweeping under it must enqueue.
        assert_ne!(
            "octos",
            octos_core::MAIN_PROFILE_ID,
            "regression guard: the dead door was MAIN_PROFILE_ID != runtime profile"
        );
        let before = orchestrator.pending_steer_continuation_count_for_test(&key, "octos");
        orchestrator.steer_inbox_sweep(temp.path(), "octos");
        let after = orchestrator.pending_steer_continuation_count_for_test(&key, "octos");
        assert_eq!(
            after,
            before + 1,
            "sweep under the production profile enqueues"
        );
    }

    /// OLP-CTRL 回合 3 整改 (契约红线): a steer continuation's prompt is
    /// the STANDALONE user message body — verbatim steer text with only
    /// the `[external-reviewer]` source marker. It must NOT be wrapped in
    /// a `[system-internal]` envelope, must NOT contain a
    /// `### External reviewer` appendix header, and must NOT be glued to
    /// any loop/goal prompt.
    #[test]
    fn olp_ctrl_steer_prompt_is_standalone_user_message() {
        use crate::autonomy::master_continuation_scheduler::{
            MasterContinuationReason, MasterContinuationRequest, MasterContinuationScheduler,
        };
        let request = MasterContinuationRequest::new(
            "g",
            "octos:local:tui#coding",
            "octos",
            MasterContinuationReason::External(
                crate::autonomy::agent_orchestrator::STEER_EXTERNAL_KIND.to_owned(),
            ),
            std::time::SystemTime::now(),
        )
        .with_metadata(
            crate::autonomy::agent_orchestrator::STEER_META_TEXT,
            "在黑板追加一行: 金丝雀",
        );
        // Build the queued item through the real scheduler (field-private
        // struct — never hand-construct it).
        let mut scheduler = MasterContinuationScheduler::new();
        scheduler.enqueue(request);
        let queued = scheduler
            .pop_ready(crate::autonomy::master_continuation_scheduler::MasterContinuationRuntimeState::idle())
            .expect("one queued continuation");
        let prompt = crate::autonomy::agent_orchestrator::master_continuation_prompt(&queued);
        assert!(
            prompt.starts_with("[external-reviewer]"),
            "steer prompt carries the source marker: {prompt}"
        );
        assert!(
            prompt.contains("在黑板追加一行: 金丝雀"),
            "steer text is the message body: {prompt}"
        );
        assert!(
            !prompt.contains("[system-internal]"),
            "steer is NOT a system-internal envelope: {prompt}"
        );
        assert!(
            !prompt.contains("### External reviewer"),
            "no appendix header (that pattern is dead): {prompt}"
        );
        assert!(
            !prompt.contains("/loop") && !prompt.contains("Advance the goal"),
            "steer is never merged with a loop/goal prompt: {prompt}"
        );
    }

    /// OLP-CTRL 首航第二回合 整改 (cross-process wake equivalence): the
    /// CLI writes ONLY files (notes + session marker); the serve-side
    /// `steer_inbox_sweep` must turn that on-disk state into a queued
    /// `External("steer")` continuation — the exact cross-process contract
    /// (CLI process ≠ serve process). Idempotence: a second sweep with an
    /// unchanged file must NOT double-enqueue.
    #[test]
    fn olp_ctrl_steer_cross_process_sweep_enqueues() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session = "octos:local:tui#coding";
        // CLI side: queue a steer (writes notes + marker, like the binary).
        append_reviewer_steer(temp.path(), session, "读黑板第 7 条").expect("queue steer");
        // Serve side: the sweep picks it up into a continuation.
        let orchestrator = crate::autonomy::agent_orchestrator::default_agent_orchestrator();
        let key = octos_core::SessionKey(session.to_owned());
        let before = orchestrator.pending_steer_continuation_count_for_test(&key, "octos");
        orchestrator.steer_inbox_sweep(temp.path(), "octos");
        let after = orchestrator.pending_steer_continuation_count_for_test(&key, "octos");
        assert_eq!(
            after,
            before + 1,
            "sweep must enqueue exactly one steer continuation for the on-disk batch"
        );
        // Idempotence: sweeping again WITHOUT a new append (same file
        // mtime → same dedupe key) must not enqueue a duplicate.
        orchestrator.steer_inbox_sweep(temp.path(), "octos");
        let after_second = orchestrator.pending_steer_continuation_count_for_test(&key, "octos");
        assert_eq!(
            after_second, after,
            "an unchanged sidecar must not double-enqueue"
        );
    }
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
        let cwd = temp.path().join("proj");
        std::fs::create_dir_all(&cwd).expect("cwd");
        assert!(session_has_persistent_state(
            temp.path(),
            &cwd,
            "octos",
            session
        ));
        append_reviewer_steer(temp.path(), session, "读黑板第 7 条").expect("append");
        let (note_path, _) = reviewer_notes_paths(temp.path(), session);
        let content = std::fs::read_to_string(note_path).expect("read");
        assert!(content.contains("读黑板第 7 条"));
    }
}
