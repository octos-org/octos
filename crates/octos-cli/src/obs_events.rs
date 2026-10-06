//! Structured observability event stream (OLP L1, slice 5).
//!
//! Contract: task-req-olp-obs-cli.spec.md — serve appends ONE single-line
//! JSON object per event to `<data_dir>/events.jsonl`:
//! `{ts, kind, goal_id?, slug?, session?, model_lane?, detail}`. Append
//! only (rotation/deletion is a later proposal's problem). Best-effort:
//! a write failure is logged at debug and NEVER affects the main flow
//! (scenario "事件写失败不影响主流程").
//!
//! `kind` coverage required by the contract: `peer_staged`,
//! `finding_recorded`, `escalation`, `goal_transition`, `steer_consumed`,
//! `turn_error`; stage 2 (#48) adds `fallback_switch` (model-lane failover,
//! gateway + serve/UI forwarder paths) and `malformed_exhausted` (malformed
//! tool-call self-correction budget exhausted, replaces that terminal's
//! turn_error row).

use std::path::{Path, PathBuf};

use serde::Serialize;

/// One event line. `ts` is RFC3339 UTC. Optional fields are omitted from
/// the JSON when absent so consumers can rely on key-presence semantics.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ObsEvent<'a> {
    pub ts: String,
    pub kind: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub goal_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slug: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_lane: Option<&'a str>,
    pub detail: &'a str,
}

impl<'a> ObsEvent<'a> {
    pub(crate) fn new(kind: &'a str, detail: &'a str) -> Self {
        Self {
            ts: chrono::Utc::now().to_rfc3339(),
            kind,
            goal_id: None,
            slug: None,
            session: None,
            model_lane: None,
            detail,
        }
    }

    pub(crate) fn goal_id(mut self, goal_id: Option<&'a str>) -> Self {
        self.goal_id = goal_id;
        self
    }

    pub(crate) fn slug(mut self, slug: Option<&'a str>) -> Self {
        self.slug = slug;
        self
    }

    pub(crate) fn session(mut self, session: Option<&'a str>) -> Self {
        self.session = session;
        self
    }

    pub(crate) fn model_lane(mut self, model_lane: Option<&'a str>) -> Self {
        self.model_lane = model_lane;
        self
    }
}

pub(crate) fn events_log_path(data_dir: &Path) -> PathBuf {
    data_dir.join("events.jsonl")
}

/// Append one event line. NEVER fails the caller: every error path is a
/// debug log and a silent return (the goal ledger remains the durable
/// source of truth; this stream is observability, not correctness).
pub(crate) fn append_obs_event(data_dir: &Path, event: &ObsEvent<'_>) {
    let line = match serde_json::to_string(event) {
        Ok(line) => line,
        Err(error) => {
            tracing::debug!(%error, "obs event serialization failed; event dropped");
            return;
        }
    };
    if let Err(error) = std::fs::create_dir_all(data_dir) {
        tracing::debug!(%error, "obs event: failed to create data dir; event dropped");
        return;
    }
    let path = events_log_path(data_dir);
    let result = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut file| {
            use std::io::Write;
            file.write_all(line.as_bytes())?;
            file.write_all(b"\n")
        });
    if let Err(error) = result {
        tracing::debug!(%error, path = %path.display(), "obs event append failed; event dropped");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_lines(data_dir: &Path) -> Vec<serde_json::Value> {
        std::fs::read_to_string(events_log_path(data_dir))
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("each line is valid JSON"))
            .collect()
    }

    #[test]
    fn olp_obs_event_line_shape_matches_contract() {
        let temp = tempfile::tempdir().expect("tempdir");
        append_obs_event(
            temp.path(),
            &ObsEvent::new("finding_recorded", "f-1 recorded")
                .goal_id(Some("goal_01"))
                .slug(Some("edison"))
                .model_lane(Some("primary")),
        );
        let lines = read_lines(temp.path());
        assert_eq!(lines.len(), 1);
        let line = &lines[0];
        assert!(line.get("ts").is_some());
        assert_eq!(line["kind"], "finding_recorded");
        assert_eq!(line["goal_id"], "goal_01");
        assert_eq!(line["slug"], "edison");
        assert_eq!(line["model_lane"], "primary");
        assert_eq!(line["detail"], "f-1 recorded");
        // Absent optionals are OMITTED, not null.
        assert!(line.get("session").is_none());
    }

    /// Contract scenario "事件写失败不影响主流程": when the events file
    /// cannot be written, `append_obs_event` returns normally (no panic,
    /// no error propagation).
    #[test]
    fn olp_obs_event_write_failure_is_nonfatal() {
        let temp = tempfile::tempdir().expect("tempdir");
        // Point data_dir at a path where events.jsonl's PARENT cannot be
        // created: a FILE where a directory is required.
        let blocker = temp.path().join("blocker");
        std::fs::write(&blocker, "not a dir").expect("blocker file");
        let bad_dir = blocker.join("nested");
        // Must not panic and must return ().
        append_obs_event(&bad_dir, &ObsEvent::new("turn_error", "boom"));
        append_obs_event(
            &bad_dir,
            &ObsEvent::new("peer_staged", "x").model_lane(Some("primary")),
        );
        assert!(!events_log_path(&bad_dir).exists());
    }
}
