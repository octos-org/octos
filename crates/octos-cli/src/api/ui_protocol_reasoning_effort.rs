//! Disk-backed per-session reasoning/thinking effort store.
//!
//! The octos-tui sets a per-session reasoning effort (`/thinking
//! low|medium|high|max`) and attaches it to every `turn/start` via
//! [`TurnStartParams::reasoning_effort`]. The serve must persist that choice
//! **server-side and on disk** so it survives a full serve/TUI restart: in
//! `--stdio` mode the serve is a child of the TUI, so a TUI restart respawns
//! the serve and only a disk-backed value reloads.
//!
//! Persistence shape mirrors the per-session task ledger
//! ([`super::ui_protocol_task_output::task_state_path`]): a small file keyed by
//! the full [`SessionKey`] (base + topic) under
//! `<data_dir>/users/<encoded base>/sessions/<encoded topic>.reasoning_effort.json`.
//! Because the key embeds the topic, the path is deterministic regardless of
//! which per-profile [`octos_bus::SessionManager`] instance computes it — the
//! persist-on-turn site (`run_standalone_turn`) and the surface-on-open site
//! (`open_session_result`) resolve to the same `data_dir` root and therefore
//! the same file.
//!
//! The on-disk format is a tiny JSON object (`{"reasoning_effort":"high"}`)
//! written atomically (write-temp-then-rename) so a crash mid-write never
//! leaves a torn file. Reads tolerate a missing/corrupt file by returning
//! `None` (treated as "no override / default").

use std::io::Write;
use std::path::{Path, PathBuf};

use octos_core::SessionKey;
use octos_core::ui_protocol::ReasoningEffortLevel;
use serde::{Deserialize, Serialize};

/// On-disk record. A struct (rather than a bare enum) so future per-session
/// reasoning knobs can be added as additive `serde(default)` fields without a
/// format break.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReasoningEffortRecord {
    reasoning_effort: ReasoningEffortLevel,
}

/// Resolve the per-session reasoning-effort file path.
///
/// Sibling to [`super::ui_protocol_task_output::task_state_path`]; keyed
/// identically (encoded base key + encoded topic, defaulting the empty topic to
/// `"default"`).
pub(crate) fn reasoning_effort_path(data_dir: &Path, session_id: &SessionKey) -> PathBuf {
    let encoded_base = octos_bus::session::encode_path_component(session_id.base_key());
    let topic = session_id
        .topic()
        .filter(|topic| !topic.is_empty())
        .unwrap_or("default");
    let encoded_topic = octos_bus::session::encode_path_component(topic);

    data_dir
        .join("users")
        .join(encoded_base)
        .join("sessions")
        .join(format!("{encoded_topic}.reasoning_effort.json"))
}

/// Read the persisted reasoning effort for a session, if any. **Blocking** —
/// performs synchronous disk IO. Async callers must reach it via
/// [`read_reasoning_effort_async`] (which offloads it onto a blocking thread)
/// rather than calling it directly on a Tokio worker.
///
/// Returns `None` when no file exists or the file is unreadable/corrupt — the
/// caller treats that as "no override".
pub(crate) fn read_reasoning_effort(
    data_dir: &Path,
    session_id: &SessionKey,
) -> Option<ReasoningEffortLevel> {
    let path = reasoning_effort_path(data_dir, session_id);
    let contents = std::fs::read_to_string(&path).ok()?;
    let record: ReasoningEffortRecord = serde_json::from_str(&contents).ok()?;
    Some(record.reasoning_effort)
}

/// Read the persisted reasoning effort, offloading the blocking disk read onto a
/// Tokio blocking thread so it never stalls an async executor worker.
///
/// `None` on missing/corrupt/cancelled — identical "no override" semantics to
/// the sync [`read_reasoning_effort`].
pub(crate) async fn read_reasoning_effort_async(
    data_dir: &Path,
    session_id: &SessionKey,
) -> Option<ReasoningEffortLevel> {
    let data_dir = data_dir.to_path_buf();
    let session_id = session_id.clone();
    tokio::task::spawn_blocking(move || read_reasoning_effort(&data_dir, &session_id))
        .await
        .ok()
        .flatten()
}

/// Persist the reasoning effort for a session (atomic write-then-rename).
/// **Blocking** — performs synchronous disk IO including an `fsync`. Async
/// callers must offload it (see [`resolve_and_persist_reasoning_effort`]).
///
/// Best-effort: returns the IO error to the caller, which logs and continues —
/// a failed persist must never abort a turn.
pub(crate) fn write_reasoning_effort(
    data_dir: &Path,
    session_id: &SessionKey,
    level: ReasoningEffortLevel,
) -> std::io::Result<()> {
    let path = reasoning_effort_path(data_dir, session_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let record = ReasoningEffortRecord {
        reasoning_effort: level,
    };
    let serialized = serde_json::to_vec(&record)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;

    // Atomic: write a uniquely-named temp sibling, fsync, then rename over the
    // target. The temp name embeds the pid so concurrent writers from the same
    // data_dir don't clobber each other's temp file before the rename.
    let tmp_path = path.with_extension(format!("json.tmp.{}", std::process::id()));
    // Write + fsync the temp file. On ANY failure along the way (create,
    // write_all, sync_all, or the rename) we must remove the temp sibling so we
    // never leak `*.reasoning_effort.json.tmp.<pid>` files on disk.
    let write_result = (|| {
        let mut file = std::fs::File::create(&tmp_path)?;
        file.write_all(&serialized)?;
        file.sync_all()
    })();
    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(error);
    }
    if let Err(error) = std::fs::rename(&tmp_path, &path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(error);
    }
    Ok(())
}

/// Resolve the effective per-session reasoning effort for a turn and persist
/// the turn-carried value when present. This is the single precedence point the
/// turn handler (`run_standalone_turn`) calls:
///
///   1. `turn_param = Some(level)` — the turn carries an explicit effort. It
///      WINS, and is persisted so a later restart (or a turn that omits it)
///      observes the same value. A failed persist is logged and the value is
///      still applied for this turn.
///   2. `turn_param = None` — fall back to the persisted stored value, so the
///      stored choice stays authoritative across a serve/TUI restart even
///      before the client re-sends it.
///   3. `turn_param = None` and nothing stored — return `None`; the caller
///      leaves the gateway/profile default untouched.
///
/// Async + write-on-change. The TUI attaches `reasoning_effort` to **every**
/// `turn/start`, so this runs on every turn once an effort is set. To keep the
/// hot path off the disk:
///   - the stored value is read first on a blocking thread, and
///   - the `fsync`ing write is issued **only when the incoming value actually
///     differs** from what is already stored, and even then it is offloaded onto
///     a blocking thread.
///
/// In the common case (effort unchanged turn-to-turn) no write — and thus no
/// `fsync` — happens at all, and the executor worker is never blocked.
pub(crate) async fn resolve_and_persist_reasoning_effort(
    data_dir: &Path,
    session_id: &SessionKey,
    turn_param: Option<ReasoningEffortLevel>,
) -> Option<ReasoningEffortLevel> {
    let stored = read_reasoning_effort_async(data_dir, session_id).await;
    match turn_param {
        // Turn carries an explicit effort: it wins. Persist only when it changed
        // the stored value (this is the per-turn write the TUI would otherwise
        // trigger on every turn). The write — and its fsync — runs on a blocking
        // thread so it never stalls the executor.
        Some(level) => {
            if stored != Some(level) {
                let data_dir = data_dir.to_path_buf();
                let session_id_owned = session_id.clone();
                let persist = tokio::task::spawn_blocking(move || {
                    write_reasoning_effort(&data_dir, &session_id_owned, level)
                })
                .await;
                match persist {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        tracing::warn!(
                            session_id = %session_id.0,
                            %error,
                            "failed to persist per-session reasoning_effort; applying for this turn only"
                        );
                    }
                    Err(join_error) => {
                        tracing::warn!(
                            session_id = %session_id.0,
                            %join_error,
                            "reasoning_effort persist task failed to join; applying for this turn only"
                        );
                    }
                }
            }
            Some(level)
        }
        // Turn omits the effort: fall back to the stored value (already read).
        None => stored,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_roundtrip_persisted_reasoning_effort_across_restart() {
        // Simulates persist-on-turn then a cold reload (fresh process / new
        // store call against the same data_dir).
        let tmp = tempfile::tempdir().expect("tempdir");
        let data_dir = tmp.path();
        let session = SessionKey("api:abc".into());

        assert_eq!(read_reasoning_effort(data_dir, &session), None);

        write_reasoning_effort(data_dir, &session, ReasoningEffortLevel::High)
            .expect("write high effort");
        assert_eq!(
            read_reasoning_effort(data_dir, &session),
            Some(ReasoningEffortLevel::High)
        );

        // Overwrite wins (last `/thinking` set is authoritative).
        write_reasoning_effort(data_dir, &session, ReasoningEffortLevel::Max)
            .expect("overwrite to max");
        assert_eq!(
            read_reasoning_effort(data_dir, &session),
            Some(ReasoningEffortLevel::Max)
        );
    }

    #[test]
    fn should_key_reasoning_effort_per_topic() {
        // Topic-suffixed sessions persist independently of the base session.
        let tmp = tempfile::tempdir().expect("tempdir");
        let data_dir = tmp.path();
        let base = SessionKey("api:abc".into());
        let topic = SessionKey("api:abc#slides".into());

        write_reasoning_effort(data_dir, &base, ReasoningEffortLevel::Low).expect("write base");
        write_reasoning_effort(data_dir, &topic, ReasoningEffortLevel::Max).expect("write topic");

        assert_eq!(
            read_reasoning_effort(data_dir, &base),
            Some(ReasoningEffortLevel::Low)
        );
        assert_eq!(
            read_reasoning_effort(data_dir, &topic),
            Some(ReasoningEffortLevel::Max)
        );
        assert_ne!(
            reasoning_effort_path(data_dir, &base),
            reasoning_effort_path(data_dir, &topic)
        );
    }

    #[tokio::test]
    async fn should_let_turn_param_win_and_persist_it() {
        // A turn that carries reasoning_effort wins over any stored value AND
        // is persisted so a later restart sees it.
        let tmp = tempfile::tempdir().expect("tempdir");
        let data_dir = tmp.path();
        let session = SessionKey("api:abc".into());

        write_reasoning_effort(data_dir, &session, ReasoningEffortLevel::Low).expect("seed low");

        let resolved = resolve_and_persist_reasoning_effort(
            data_dir,
            &session,
            Some(ReasoningEffortLevel::Max),
        )
        .await;
        assert_eq!(resolved, Some(ReasoningEffortLevel::Max));
        // Persisted, so a subsequent turn that omits the param observes Max.
        assert_eq!(
            read_reasoning_effort(data_dir, &session),
            Some(ReasoningEffortLevel::Max)
        );
    }

    #[tokio::test]
    async fn should_fall_back_to_stored_when_turn_omits_effort() {
        // A turn that omits reasoning_effort falls back to the persisted value —
        // the stored choice survives a restart even before the client re-sends.
        let tmp = tempfile::tempdir().expect("tempdir");
        let data_dir = tmp.path();
        let session = SessionKey("api:abc".into());

        write_reasoning_effort(data_dir, &session, ReasoningEffortLevel::High).expect("seed high");

        let resolved = resolve_and_persist_reasoning_effort(data_dir, &session, None).await;
        assert_eq!(resolved, Some(ReasoningEffortLevel::High));
    }

    #[tokio::test]
    async fn should_resolve_none_when_no_param_and_nothing_stored() {
        // Nothing stored + turn omits → no override; caller keeps the default.
        let tmp = tempfile::tempdir().expect("tempdir");
        let data_dir = tmp.path();
        let session = SessionKey("api:fresh".into());

        assert_eq!(
            resolve_and_persist_reasoning_effort(data_dir, &session, None).await,
            None
        );
    }

    #[tokio::test]
    async fn should_not_rewrite_when_turn_param_matches_stored() {
        // The hot path: the TUI re-attaches the SAME effort on every turn. When
        // the incoming value already equals the stored value we must NOT touch
        // the file (no write, no fsync) — the per-turn write is exactly what the
        // P2 fix eliminates.
        //
        // Detect the no-write deterministically (no clock/mtime-resolution
        // dependence): seed the file with a byte-distinct-but-equivalent JSON
        // encoding (extra whitespace) that still parses to `High`. A genuine
        // rewrite would normalize it to serde's compact form, so byte-equality
        // after the call proves no write happened.
        let tmp = tempfile::tempdir().expect("tempdir");
        let data_dir = tmp.path();
        let session = SessionKey("api:abc".into());

        let path = reasoning_effort_path(data_dir, &session);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let sentinel = b"{ \"reasoning_effort\" : \"high\" }";
        std::fs::write(&path, sentinel).expect("seed sentinel");
        // Sanity: the sentinel parses to High but is NOT byte-equal to a fresh
        // canonical write, so a rewrite is observable.
        assert_eq!(
            read_reasoning_effort(data_dir, &session),
            Some(ReasoningEffortLevel::High)
        );
        assert_ne!(
            std::fs::read(&path).unwrap(),
            serde_json::to_vec(&ReasoningEffortRecord {
                reasoning_effort: ReasoningEffortLevel::High
            })
            .unwrap()
        );

        let resolved = resolve_and_persist_reasoning_effort(
            data_dir,
            &session,
            Some(ReasoningEffortLevel::High),
        )
        .await;
        assert_eq!(resolved, Some(ReasoningEffortLevel::High));
        assert_eq!(
            std::fs::read(&path).unwrap(),
            sentinel,
            "unchanged effort must not rewrite (and re-fsync) the file"
        );

        // A genuinely different value, by contrast, DOES rewrite.
        let resolved_changed = resolve_and_persist_reasoning_effort(
            data_dir,
            &session,
            Some(ReasoningEffortLevel::Low),
        )
        .await;
        assert_eq!(resolved_changed, Some(ReasoningEffortLevel::Low));
        assert_ne!(
            std::fs::read(&path).unwrap(),
            sentinel,
            "a changed effort must rewrite the file"
        );
        assert_eq!(
            read_reasoning_effort(data_dir, &session),
            Some(ReasoningEffortLevel::Low)
        );
    }

    #[test]
    fn should_return_none_for_corrupt_record() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let data_dir = tmp.path();
        let session = SessionKey("api:abc".into());
        let path = reasoning_effort_path(data_dir, &session);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{not json").unwrap();

        assert_eq!(read_reasoning_effort(data_dir, &session), None);
    }
}
