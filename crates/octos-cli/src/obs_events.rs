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
