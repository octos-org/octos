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

/// Read the persisted reasoning effort for a session, if any.
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

/// Persist the reasoning effort for a session (atomic write-then-rename).
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
    {
        let mut file = std::fs::File::create(&tmp_path)?;
        file.write_all(&serialized)?;
        file.sync_all()?;
    }
    match std::fs::rename(&tmp_path, &path) {
        Ok(()) => Ok(()),
        Err(error) => {
            // Clean up the temp file on a failed rename so we don't leak it.
            let _ = std::fs::remove_file(&tmp_path);
            Err(error)
        }
    }
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
pub(crate) fn resolve_and_persist_reasoning_effort(
    data_dir: &Path,
    session_id: &SessionKey,
    turn_param: Option<ReasoningEffortLevel>,
) -> Option<ReasoningEffortLevel> {
    match turn_param {
        Some(level) => {
            if let Err(error) = write_reasoning_effort(data_dir, session_id, level) {
                tracing::warn!(
                    session_id = %session_id.0,
                    %error,
                    "failed to persist per-session reasoning_effort; applying for this turn only"
                );
            }
            Some(level)
        }
        None => read_reasoning_effort(data_dir, session_id),
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

    #[test]
    fn should_let_turn_param_win_and_persist_it() {
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
        );
        assert_eq!(resolved, Some(ReasoningEffortLevel::Max));
        // Persisted, so a subsequent turn that omits the param observes Max.
        assert_eq!(
            read_reasoning_effort(data_dir, &session),
            Some(ReasoningEffortLevel::Max)
        );
    }

    #[test]
    fn should_fall_back_to_stored_when_turn_omits_effort() {
        // A turn that omits reasoning_effort falls back to the persisted value —
        // the stored choice survives a restart even before the client re-sends.
        let tmp = tempfile::tempdir().expect("tempdir");
        let data_dir = tmp.path();
        let session = SessionKey("api:abc".into());

        write_reasoning_effort(data_dir, &session, ReasoningEffortLevel::High).expect("seed high");

        let resolved = resolve_and_persist_reasoning_effort(data_dir, &session, None);
        assert_eq!(resolved, Some(ReasoningEffortLevel::High));
    }

    #[test]
    fn should_resolve_none_when_no_param_and_nothing_stored() {
        // Nothing stored + turn omits → no override; caller keeps the default.
        let tmp = tempfile::tempdir().expect("tempdir");
        let data_dir = tmp.path();
        let session = SessionKey("api:fresh".into());

        assert_eq!(
            resolve_and_persist_reasoning_effort(data_dir, &session, None),
            None
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
