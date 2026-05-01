//! Append-only JSON-Lines audit log for approval decisions.
//!
//! Compliance / forensic record. One line per decision (manual or
//! auto-resolved). Carries identifiers + decision metadata only — never
//! command bodies or diff content (see PII rule in M9-FIX-07 spec).
//!
//! Rotation: by size (10 MiB default), matching the rolling-file pattern
//! used by `tracing-appender` elsewhere in the cli. Retention: 90 days
//! default (configurable). Sweeps older `approvals-*.log` files in the
//! audit directory on every rotation.

use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use octos_core::ui_protocol::ApprovalDecidedEvent;
use serde::{Deserialize, Serialize};
use serde_json::json;

/// Default rotation threshold: 10 MiB.
pub const DEFAULT_ROTATE_BYTES: u64 = 10 * 1024 * 1024;

/// Default retention window: 90 days.
pub const DEFAULT_RETENTION_DAYS: i64 = 90;

const ENV_ENABLED: &str = "OCTOS_APPROVALS_AUDIT_ENABLED";
const ENV_DIR: &str = "OCTOS_APPROVALS_AUDIT_DIR";
const ENV_ROTATE_BYTES: &str = "OCTOS_APPROVALS_AUDIT_ROTATE_BYTES";
const ENV_RETENTION_DAYS: &str = "OCTOS_APPROVALS_AUDIT_RETENTION_DAYS";

const AUDIT_RECORD_SCHEMA_VERSION: u32 = 1;

/// Configuration for the approvals audit log. When the cli grows a profile
/// config wiring, this is the struct to plumb through; until then,
/// [`ApprovalsAuditConfig::from_env`] reads from env vars.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalsAuditConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub directory: Option<PathBuf>,
    #[serde(default = "default_rotate_bytes")]
    pub rotate_bytes: u64,
    #[serde(default = "default_retention_days")]
    pub retention_days: i64,
}

fn default_enabled() -> bool {
    true
}
fn default_rotate_bytes() -> u64 {
    DEFAULT_ROTATE_BYTES
}
fn default_retention_days() -> i64 {
    DEFAULT_RETENTION_DAYS
}

impl Default for ApprovalsAuditConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            directory: None,
            rotate_bytes: DEFAULT_ROTATE_BYTES,
            retention_days: DEFAULT_RETENTION_DAYS,
        }
    }
}

impl ApprovalsAuditConfig {
    pub fn from_env() -> Self {
        let mut cfg = Self::default();
        if let Ok(v) = std::env::var(ENV_ENABLED) {
            cfg.enabled = parse_bool(&v).unwrap_or(cfg.enabled);
        }
        if let Ok(v) = std::env::var(ENV_DIR) {
            if !v.is_empty() {
                cfg.directory = Some(PathBuf::from(v));
            }
        }
        if let Ok(v) = std::env::var(ENV_ROTATE_BYTES) {
            if let Ok(parsed) = v.parse::<u64>() {
                cfg.rotate_bytes = parsed;
            }
        }
        if let Ok(v) = std::env::var(ENV_RETENTION_DAYS) {
            if let Ok(parsed) = v.parse::<i64>() {
                cfg.retention_days = parsed;
            }
        }
        cfg
    }
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// Pluggable clock so size-rotation tests can pin the file epoch suffix.
pub trait AuditClock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

#[derive(Debug, Default)]
pub struct SystemAuditClock;

impl AuditClock for SystemAuditClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// Append-only JSON-Lines audit log writer with size-based rotation.
pub struct ApprovalsAuditLog {
    config: ApprovalsAuditConfig,
    base_dir: PathBuf,
    clock: Box<dyn AuditClock>,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    path: Option<PathBuf>,
    writer: Option<BufWriter<File>>,
    bytes_written: u64,
}

impl ApprovalsAuditLog {
    /// Construct a writer rooted at `<data_dir>/audit/` unless
    /// `config.directory` overrides.
    pub fn new(data_dir: impl AsRef<Path>, config: ApprovalsAuditConfig) -> Self {
        Self::with_clock(data_dir, config, Box::new(SystemAuditClock))
    }

    pub fn with_clock(
        data_dir: impl AsRef<Path>,
        config: ApprovalsAuditConfig,
        clock: Box<dyn AuditClock>,
    ) -> Self {
        let base_dir = config
            .directory
            .clone()
            .unwrap_or_else(|| data_dir.as_ref().join("audit"));
        Self {
            config,
            base_dir,
            clock,
            inner: Mutex::new(Inner::default()),
        }
    }

    /// Append a line for `event`. Returns `Ok(None)` when the audit log is
    /// disabled, or `Ok(Some(path))` with the file the line landed in.
    pub fn record(
        &self,
        event: &ApprovalDecidedEvent,
        tool_name: Option<&str>,
    ) -> std::io::Result<Option<PathBuf>> {
        if !self.config.enabled {
            return Ok(None);
        }
        let line = serialize_record(event, tool_name)?;
        let mut inner = self
            .inner
            .lock()
            .map_err(|err| std::io::Error::other(format!("audit log poisoned: {err}")))?;
        let path = self.ensure_active(&mut inner, line.len() as u64)?;
        if let Some(writer) = inner.writer.as_mut() {
            writer.write_all(&line)?;
            writer.flush()?;
            inner.bytes_written += line.len() as u64;
        }
        Ok(Some(path))
    }

    fn ensure_active(&self, inner: &mut Inner, incoming: u64) -> std::io::Result<PathBuf> {
        let needs_rotate = inner
            .writer
            .as_ref()
            .is_some_and(|_| inner.bytes_written + incoming > self.config.rotate_bytes);
        if needs_rotate {
            inner.writer = None;
            inner.path = None;
            inner.bytes_written = 0;
            self.sweep_retained();
        }
        if inner.writer.is_none() {
            fs::create_dir_all(&self.base_dir)?;
            let path = self.next_file_path();
            let file = OpenOptions::new().create(true).append(true).open(&path)?;
            let bytes_written = file.metadata().map(|m| m.len()).unwrap_or(0);
            inner.writer = Some(BufWriter::new(file));
            inner.path = Some(path.clone());
            inner.bytes_written = bytes_written;
            return Ok(path);
        }
        Ok(inner
            .path
            .clone()
            .expect("path is Some when writer is Some"))
    }

    fn next_file_path(&self) -> PathBuf {
        let epoch_ms = self.clock.now().timestamp_millis();
        let candidate = self.base_dir.join(format!("approvals-{epoch_ms}.log"));
        if !candidate.exists() {
            return candidate;
        }
        for n in 1u32..u32::MAX {
            let suffixed = self.base_dir.join(format!("approvals-{epoch_ms}-{n}.log"));
            if !suffixed.exists() {
                return suffixed;
            }
        }
        candidate
    }

    fn sweep_retained(&self) {
        if self.config.retention_days < 0 {
            return;
        }
        let Ok(entries) = fs::read_dir(&self.base_dir) else {
            return;
        };
        let now_ms = self.clock.now().timestamp_millis();
        let retention_ms = self.config.retention_days.saturating_mul(86_400_000);
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if !stem.starts_with("approvals-") {
                continue;
            }
            let suffix = &stem["approvals-".len()..];
            let epoch_part: String = suffix.chars().take_while(|c| c.is_ascii_digit()).collect();
            let Ok(epoch_ms) = epoch_part.parse::<i64>() else {
                continue;
            };
            if now_ms.saturating_sub(epoch_ms) > retention_ms {
                let _ = fs::remove_file(&path);
            }
        }
    }
}

/// Serialize the JSON-Lines record. Separated so the test path can hit it
/// without a writer; also keeps the PII surface review-able in one place.
fn serialize_record(
    event: &ApprovalDecidedEvent,
    tool_name: Option<&str>,
) -> std::io::Result<Vec<u8>> {
    // Note: this intentionally does not include any payload-shaped fields
    // (command body, diff text). Only identifiers + decision metadata.
    let value = json!({
        "schema_version": AUDIT_RECORD_SCHEMA_VERSION,
        "session_id": event.session_id,
        "approval_id": event.approval_id.0.to_string(),
        "turn_id": event.turn_id.0.to_string(),
        "tool_name": tool_name,
        "decision": event.decision,
        "scope": event.scope,
        "decided_at": event.decided_at,
        "decided_by": event.decided_by,
        "auto_resolved": event.auto_resolved,
        "policy_id": event.policy_id,
        "client_note": event.client_note,
    });
    let mut line = serde_json::to_string(&value).map_err(|err| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("serialize audit record: {err}"),
        )
    })?;
    line.push('\n');
    Ok(line.into_bytes())
}

#[cfg(test)]
pub(crate) fn read_audit_lines(path: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).expect("audit line is JSON"))
        .collect()
}

/// Emit the `octos.approvals.decision` tracing span. Kept here so the
/// approvals handler imports a single audit-related symbol.
pub fn log_decision_tracing(event: &ApprovalDecidedEvent, tool_name: Option<&str>) {
    tracing::info!(
        target: "octos.approvals.decision",
        approval_id = %event.approval_id.0,
        decision = ?event.decision,
        scope = ?event.scope,
        auto_resolved = event.auto_resolved,
        policy_id = ?event.policy_id,
        decided_by = %event.decided_by,
        decided_at = %event.decided_at,
        turn_id = %event.turn_id.0,
        session_id = %event.session_id.0,
        tool_name = ?tool_name,
        "approval decision recorded"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use octos_core::SessionKey;
    use octos_core::ui_protocol::{ApprovalDecision, ApprovalId, TurnId};
    use std::sync::atomic::{AtomicI64, Ordering};

    struct FixedClock {
        millis: AtomicI64,
        step_ms: i64,
    }

    impl AuditClock for FixedClock {
        fn now(&self) -> DateTime<Utc> {
            let v = self.millis.fetch_add(self.step_ms, Ordering::SeqCst);
            DateTime::<Utc>::from_timestamp_millis(v).unwrap_or_else(Utc::now)
        }
    }

    fn sample_event(decision: ApprovalDecision) -> ApprovalDecidedEvent {
        ApprovalDecidedEvent {
            session_id: SessionKey("local:test".into()),
            approval_id: ApprovalId::new(),
            turn_id: TurnId::new(),
            decision,
            scope: None,
            decided_at: Utc::now(),
            decided_by: "user:test".into(),
            auto_resolved: false,
            policy_id: None,
            client_note: None,
        }
    }

    #[test]
    fn audit_log_writes_one_line_per_decision() {
        let temp = tempfile::tempdir().expect("tempdir");
        let log = ApprovalsAuditLog::new(temp.path(), ApprovalsAuditConfig::default());
        let event = sample_event(ApprovalDecision::Approve);
        let path = log
            .record(&event, Some("shell"))
            .expect("write")
            .expect("enabled");
        let lines = read_audit_lines(&path);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["decision"], json!("approve"));
        assert_eq!(lines[0]["tool_name"], json!("shell"));
        assert_eq!(
            lines[0]["schema_version"],
            json!(AUDIT_RECORD_SCHEMA_VERSION)
        );
    }

    #[test]
    fn audit_log_rotates_on_size_threshold() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cfg = ApprovalsAuditConfig {
            rotate_bytes: 256,
            ..Default::default()
        };
        let clock = Box::new(FixedClock {
            millis: AtomicI64::new(1_700_000_000_000),
            step_ms: 1_000,
        });
        let log = ApprovalsAuditLog::with_clock(temp.path(), cfg, clock);
        let mut paths = std::collections::HashSet::new();
        for _ in 0..30 {
            let path = log
                .record(&sample_event(ApprovalDecision::Approve), Some("shell"))
                .expect("write")
                .expect("enabled");
            paths.insert(path);
        }
        assert!(paths.len() > 1, "expected rotation; saw {paths:?}");
    }

    #[test]
    fn audit_log_retention_sweeps_old_files() {
        let temp = tempfile::tempdir().expect("tempdir");
        let audit_dir = temp.path().join("audit");
        fs::create_dir_all(&audit_dir).unwrap();
        let stale_ms = Utc::now().timestamp_millis() - 100 * 86_400_000;
        let stale_path = audit_dir.join(format!("approvals-{stale_ms}.log"));
        std::fs::write(&stale_path, "{}\n").unwrap();
        let cfg = ApprovalsAuditConfig {
            rotate_bytes: 16,
            retention_days: 90,
            ..Default::default()
        };
        let log = ApprovalsAuditLog::new(temp.path(), cfg);
        for _ in 0..3 {
            log.record(&sample_event(ApprovalDecision::Approve), None)
                .expect("write");
        }
        assert!(!stale_path.exists(), "stale audit file should be swept");
    }
}
