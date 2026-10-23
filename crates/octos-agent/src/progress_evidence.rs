//! Validate optional task check reports without claiming to judge task quality.
use crate::workspace_progress::ConfinedRoot;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

const MAX_REPORT: u64 = 256 * 1024;
const MAX_ARTIFACT: u64 = 256 * 1024 * 1024;
const MAX_HASH_BUDGET: u64 = 512 * 1024 * 1024;

#[derive(Default, Debug, Serialize)]
pub struct EvidenceDelta {
    pub available: bool,
    pub current_passes: usize,
    pub current_failures: usize,
    pub failures: Vec<FailureEvidence>,
    pub new_failures: Vec<String>,
    pub stale: usize,
    pub improved: bool,
    /// A new, hash-bound tool operation finished. This is activity, not a
    /// content/quality check pass, and only suppresses periodic reminders.
    pub activity_advanced: bool,
    pub completed_operations: Vec<String>,
    pub newly_passed: Vec<String>,
    pub resolved: Vec<String>,
    pub open_findings: Vec<String>,
    pub invalid_checks: Vec<String>,
    pub removed_checks: Vec<String>,
    pub scope: &'static str,
}

/// Provenance for a failing report; operation failures are not artifact defects.
#[derive(Debug, Serialize)]
pub struct FailureEvidence {
    pub id: String,
    pub kind: String,
    pub report: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Index {
    schema_version: u32,
    checks: Vec<CheckRef>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckRef {
    id: String,
    report: String,
    sha256: String,
}
#[derive(Deserialize)]
struct Report {
    schema_version: u32,
    check_id: String,
    status: String,
    #[serde(default = "default_kind")]
    kind: String,
    artifacts: Vec<Artifact>,
    #[serde(default)]
    findings: Vec<Finding>,
}
#[derive(Deserialize)]
struct Artifact {
    path: String,
    sha256: String,
    #[serde(default = "default_role")]
    role: String,
}
#[derive(Deserialize)]
struct Finding {
    id: String,
    status: String,
}
#[derive(Clone)]
struct CheckState {
    status: String,
    kind: String,
    artifact_fingerprint: String,
    findings: BTreeMap<String, String>,
}

fn default_kind() -> String {
    "check".into()
}
fn default_role() -> String {
    "input".into()
}

pub struct ProgressEvidence {
    root: ConfinedRoot,
    previous: BTreeMap<String, CheckState>,
    cache: HashMap<PathBuf, (String, String)>,
    had_index: bool,
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_alphanumeric() || "-_/.:".contains(c))
}
fn valid_hash(hash: &str) -> bool {
    hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit())
}

fn change_stamp(meta: &std::fs::Metadata) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        format!(
            "{}:{}:{}:{}:{}:{}:{}",
            meta.dev(),
            meta.ino(),
            meta.len(),
            meta.mtime(),
            meta.mtime_nsec(),
            meta.ctime(),
            meta.ctime_nsec()
        )
    }
    #[cfg(not(unix))]
    {
        // An empty stamp disables caching where a reliable stamp is unavailable.
        let _ = meta;
        String::new()
    }
}

impl ProgressEvidence {
    pub fn new(root: &Path) -> io::Result<Self> {
        Ok(Self {
            root: ConfinedRoot::new(root)?,
            previous: BTreeMap::new(),
            cache: HashMap::new(),
            had_index: false,
        })
    }

    fn artifact_hash(&mut self, path: &Path, budget: &mut u64) -> io::Result<String> {
        let file = self.root.open(path, MAX_ARTIFACT)?;
        let meta = file.metadata()?;
        let stamp = change_stamp(&meta);
        if !stamp.is_empty() {
            if let Some((old_stamp, hash)) = self.cache.get(path) {
                if *old_stamp == stamp {
                    return Ok(hash.clone());
                }
            }
        }
        if meta.len() > *budget {
            return Err(io::Error::other("checkpoint hash budget exceeded"));
        }
        let mut digest = Sha256::new();
        let mut reader = file.take(meta.len() + 1);
        let mut buf = [0u8; 65536];
        let mut bytes = 0;
        loop {
            let count = reader.read(&mut buf)?;
            if count == 0 {
                break;
            }
            bytes += count as u64;
            if bytes > meta.len() {
                return Err(io::Error::other("artifact changed while hashing"));
            }
            digest.update(&buf[..count]);
        }
        if bytes != meta.len() || change_stamp(&reader.get_ref().metadata()?) != stamp {
            return Err(io::Error::other("artifact changed while hashing"));
        }
        *budget = budget.saturating_sub(bytes);
        let hash = format!("{:x}", digest.finalize());
        if self.cache.len() >= 512 {
            self.cache.clear();
        }
        self.cache.insert(path.to_path_buf(), (stamp, hash.clone()));
        Ok(hash)
    }

    fn inspect(&mut self, check: &CheckRef, budget: &mut u64) -> io::Result<CheckState> {
        if !valid_id(&check.id) || !valid_hash(&check.sha256) {
            return Err(io::Error::other("invalid check identity"));
        }
        let bytes = self.root.read(Path::new(&check.report), MAX_REPORT)?;
        if format!("{:x}", Sha256::digest(&bytes)) != check.sha256 {
            return Err(io::Error::other("report changed"));
        }
        let report: Report = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
        if report.schema_version != 1
            || report.check_id != check.id
            || !matches!(report.status.as_str(), "pass" | "fail" | "unknown")
            || !matches!(report.kind.as_str(), "check" | "operation")
            || report.artifacts.is_empty()
            || report.artifacts.len() > 16
            || report.findings.len() > 128
        {
            return Err(io::Error::other("invalid check report"));
        }
        let mut paths = BTreeSet::new();
        let mut bindings = BTreeSet::new();
        for artifact in report.artifacts {
            if !matches!(artifact.role.as_str(), "input" | "output")
                || !valid_hash(&artifact.sha256)
                || !paths.insert(artifact.path.clone())
                || self.artifact_hash(Path::new(&artifact.path), budget)? != artifact.sha256
            {
                return Err(io::Error::other("stale artifact binding"));
            }
            if artifact.role == "input" {
                bindings.insert((artifact.path, artifact.sha256));
            }
        }
        if bindings.is_empty() {
            return Err(io::Error::other("report requires an input binding"));
        }
        let mut findings = BTreeMap::new();
        for finding in report.findings {
            if !valid_id(&finding.id)
                || !matches!(finding.status.as_str(), "open" | "closed")
                || findings.insert(finding.id, finding.status).is_some()
            {
                return Err(io::Error::other("invalid finding identity"));
            }
        }
        if report.status == "pass" && findings.values().any(|s| s == "open") {
            return Err(io::Error::other("pass contradicts open findings"));
        }
        Ok(CheckState {
            status: report.status,
            kind: report.kind,
            artifact_fingerprint: format!(
                "{:x}",
                Sha256::digest(serde_json::to_vec(&bindings).map_err(io::Error::other)?)
            ),
            findings,
        })
    }

    /// Adapters publish reports at the tool boundary; no model-authored index
    /// is needed. Every discovered report still passes the same confined
    /// identity, size, artifact hash and freshness validation as an index ref.
    fn automatic_checks(&self) -> io::Result<Vec<CheckRef>> {
        for relative in [".octos", ".octos/tool-progress"] {
            let meta = match self.root.path.join(relative).symlink_metadata() {
                Ok(meta) => meta,
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(vec![]),
                Err(e) => return Err(e),
            };
            if !meta.is_dir() || meta.is_symlink() {
                return Err(io::Error::other(
                    "automatic report directory must not be a symlink",
                ));
            }
        }
        let mut checks = Vec::new();
        for entry in std::fs::read_dir(self.root.path.join(".octos/tool-progress"))?.take(65) {
            let entry = entry?;
            if checks.len() >= 64 {
                return Err(io::Error::other("automatic report bound exceeded"));
            }
            let relative = Path::new(".octos/tool-progress").join(entry.file_name());
            if relative.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let bytes = self.root.read(&relative, MAX_REPORT)?;
            let report: Report = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
            checks.push(CheckRef {
                id: report.check_id,
                report: relative.to_string_lossy().into_owned(),
                sha256: format!("{:x}", Sha256::digest(bytes)),
            });
        }
        Ok(checks)
    }

    pub fn refresh(&mut self) -> EvidenceDelta {
        let mut delta = EvidenceDelta {
            scope: "Report/input hash consistency only; does not certify test correctness, visual quality or complete coverage.",
            ..Default::default()
        };
        let bytes = match self
            .root
            .read(Path::new(".octos/progress.json"), MAX_REPORT)
        {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(_) => {
                delta.stale = 1;
                delta
                    .invalid_checks
                    .push("missing-or-unreadable-index".into());
                return delta;
            }
        };
        let mut index: Index = match bytes {
            Some(bytes) => {
                self.had_index = true;
                match serde_json::from_slice(&bytes) {
                    Ok(index) => index,
                    Err(_) => {
                        delta.stale = 1;
                        delta.invalid_checks.push("invalid-index".into());
                        return delta;
                    }
                }
            }
            None => {
                if self.had_index {
                    delta.stale += 1;
                    delta
                        .invalid_checks
                        .push("missing-or-unreadable-index".into());
                }
                Index {
                    schema_version: 1,
                    checks: vec![],
                }
            }
        };
        if index.schema_version != 1 || index.checks.len() > 64 {
            delta.stale = 1;
            delta.invalid_checks.push("invalid-index".into());
            return delta;
        }
        match self.automatic_checks() {
            Ok(checks) => index.checks.extend(checks),
            Err(_) => {
                delta.stale += 1;
                delta
                    .invalid_checks
                    .push("invalid-automatic-reports".into());
            }
        }
        delta.available = !index.checks.is_empty();
        let mut budget = MAX_HASH_BUDGET;
        let mut seen = BTreeSet::new();
        let mut current = BTreeSet::new();
        for check in index.checks {
            if !seen.insert(check.id.clone()) {
                delta.stale += 1;
                delta.invalid_checks.push("duplicate-check".into());
                continue;
            }
            match self.inspect(&check, &mut budget) {
                Ok(mut state) => {
                    current.insert(check.id.clone());
                    let previous = self.previous.get(&check.id);
                    // Dropping an old finding does not close it. Only an explicit
                    // closed finding in a current report can do that.
                    if let Some(old) = previous {
                        for (id, status) in &old.findings {
                            if status == "open" && !state.findings.contains_key(id) {
                                state.findings.insert(id.clone(), "open".into());
                            }
                        }
                    }
                    if state.status == "pass" && !state.findings.values().any(|v| v == "open") {
                        if state.kind == "operation" {
                            if previous.is_none_or(|p| {
                                p.status != "pass"
                                    || p.artifact_fingerprint != state.artifact_fingerprint
                            }) {
                                delta.completed_operations.push(check.id.clone());
                            }
                        } else {
                            delta.current_passes += 1;
                            if previous.is_none_or(|p| p.status != "pass") {
                                delta.newly_passed.push(check.id.clone());
                            }
                        }
                    } else if state.status == "fail" || state.findings.values().any(|v| v == "open")
                    {
                        delta.current_failures += 1;
                        delta.failures.push(FailureEvidence {
                            id: check.id.clone(),
                            kind: state.kind.clone(),
                            report: check.report.clone(),
                        });
                        if previous.is_none_or(|p| {
                            (p.status != "fail" && !p.findings.values().any(|v| v == "open"))
                                || p.kind != state.kind
                                || p.artifact_fingerprint != state.artifact_fingerprint
                                || p.findings != state.findings
                        }) {
                            delta.new_failures.push(check.id.clone());
                        }
                        if state.status == "pass" {
                            state.status = "fail".into();
                        }
                    }
                    for (id, status) in &state.findings {
                        let key = format!("{}:{id}", check.id);
                        if status == "open" {
                            delta.open_findings.push(key);
                        } else if state.kind == "check"
                            && previous
                                .and_then(|p| p.findings.get(id))
                                .is_some_and(|s| s == "open")
                        {
                            delta.resolved.push(key);
                        }
                    }
                    self.previous.insert(check.id, state);
                }
                Err(_) => {
                    delta.stale += 1;
                    delta.invalid_checks.push(if valid_id(&check.id) {
                        check.id
                    } else {
                        "invalid-id".into()
                    });
                }
            }
        }
        for (id, old) in &self.previous {
            if !seen.contains(id) {
                delta.removed_checks.push(id.clone());
            }
            if !current.contains(id) {
                for (finding, status) in &old.findings {
                    if status == "open" {
                        delta.open_findings.push(format!("{id}:{finding}"));
                    }
                }
            }
        }
        // Missing/stale checks never justify bypassing a reflection, even if
        // some other check passed. New discoveries are not called regressions.
        delta.improved = delta.stale == 0
            && delta.removed_checks.is_empty()
            && (!delta.newly_passed.is_empty() || !delta.resolved.is_empty());
        delta.activity_advanced = delta.stale == 0
            && delta.removed_checks.is_empty()
            && delta.new_failures.is_empty()
            && !delta.completed_operations.is_empty();
        delta
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use sha2::{Digest, Sha256};
    #[test]
    fn should_discover_adapter_report_without_model_index_and_not_call_it_quality() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".octos/tool-progress")).unwrap();
        std::fs::write(dir.path().join("source.txt"), "v1").unwrap();
        std::fs::write(dir.path().join("receipt.json"), "first").unwrap();
        let report = dir.path().join(".octos/tool-progress/render.json");
        let publish =
            |input: &str, output: &str| {
                std::fs::write(&report,serde_json::to_vec(&json!({
                "schema_version":1,"check_id":"tool-operation/render","kind":"operation",
                "status":"pass","artifacts":[
                    {"path":"source.txt","sha256":hash(input.as_bytes()),"role":"input"},
                    {"path":"receipt.json","sha256":hash(output.as_bytes()),"role":"output"}],
                "findings":[]
            })).unwrap()).unwrap();
            };
        publish("v1", "first");
        let mut evidence = ProgressEvidence::new(dir.path()).unwrap();
        let first = evidence.refresh();
        assert!(first.activity_advanced);
        assert!(!first.improved);
        assert_eq!(first.current_passes, 0);
        assert!(!evidence.refresh().activity_advanced);
        // Timestamp/id/output-only churn must not count as more progress.
        std::fs::write(dir.path().join("receipt.json"), "second").unwrap();
        publish("v1", "second");
        assert!(!evidence.refresh().activity_advanced);
        std::fs::write(dir.path().join("source.txt"), "v2").unwrap();
        assert!(
            !evidence.refresh().activity_advanced,
            "stale render is not progress"
        );
        publish("v2", "second");
        assert!(evidence.refresh().activity_advanced);
    }

    #[test]
    fn should_identify_failures_and_allow_unrelated_activity_after_observation() {
        let dir = fixture();
        publish(&dir, "fail", "original");
        let mut evidence = ProgressEvidence::new(dir.path()).unwrap();
        let first = evidence.refresh();
        assert_eq!(first.failures[0].id, "copy");
        assert_eq!(first.failures[0].report, "report.json");
        assert_eq!(first.failures[0].kind, "check");
        assert_eq!(first.new_failures, vec!["copy"]);
        std::fs::create_dir_all(dir.path().join(".octos/tool-progress")).unwrap();
        let bytes = serde_json::to_vec(&json!({"schema_version":1,
            "check_id":"tool-operation/build","kind":"operation","status":"pass",
            "artifacts":[{"path":"artifact.txt","sha256":hash(b"original")}],
            "findings":[]}))
        .unwrap();
        std::fs::write(dir.path().join(".octos/tool-progress/build.json"), bytes).unwrap();
        let next = evidence.refresh();
        assert_eq!(next.current_failures, 1, "the known failure is not erased");
        assert!(next.new_failures.is_empty());
        assert!(
            next.activity_advanced,
            "unrelated work can advance despite a known failure"
        );
        assert!(
            !next.improved,
            "operation completion does not certify quality"
        );
        assert!(
            !evidence.refresh().activity_advanced,
            "unchanged reports are not fresh work"
        );
    }

    #[test]
    fn should_not_rediscover_an_unchanged_open_finding_on_an_unknown_report() {
        let dir = fixture();
        publish(&dir, "unknown", "original");
        set_findings(&dir, json!([{"id":"pending", "status":"open"}]));
        let mut evidence = ProgressEvidence::new(dir.path()).unwrap();
        assert_eq!(evidence.refresh().new_failures, vec!["copy"]);
        let unchanged = evidence.refresh();
        assert_eq!(unchanged.current_failures, 1);
        assert!(unchanged.new_failures.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn should_reject_symlinked_automatic_report_directory() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".octos")).unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join(".octos/tool-progress"))
            .unwrap();
        let state = ProgressEvidence::new(dir.path()).unwrap().refresh();
        assert!(!state.activity_advanced);
        assert!(
            state
                .invalid_checks
                .contains(&"invalid-automatic-reports".to_string())
        );
    }
    fn hash(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }
    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".octos")).unwrap();
        std::fs::write(dir.path().join("artifact.txt"), "original").unwrap();
        publish(&dir, "pass", "original");
        dir
    }
    fn publish(dir: &tempfile::TempDir, status: &str, content: &str) {
        let report = json!({"schema_version":1,"check_id":"copy","status":status,
            "artifacts":[{"path":"artifact.txt","sha256":hash(content.as_bytes())}],
            "findings":[]});
        let bytes = serde_json::to_vec(&report).unwrap();
        std::fs::write(dir.path().join("report.json"), &bytes).unwrap();
        std::fs::write(dir.path().join(".octos/progress.json"), serde_json::to_vec(&json!({
            "schema_version":1,"checks":[{"id":"copy","report":"report.json","sha256":hash(&bytes)}]})).unwrap()).unwrap();
    }
    #[test]
    fn should_invalidate_pass_when_artifact_changes() {
        let dir = fixture();
        let mut evidence = ProgressEvidence::new(dir.path()).unwrap();
        assert_eq!(evidence.refresh().current_passes, 1);
        std::fs::write(dir.path().join("artifact.txt"), "modified").unwrap();
        let state = evidence.refresh();
        assert_eq!(state.current_passes, 0);
        assert_eq!(state.stale, 1);
        assert!(!state.improved);
    }
    #[test]
    fn should_count_new_pass_once_and_reject_changed_receipt() {
        let dir = fixture();
        let mut evidence = ProgressEvidence::new(dir.path()).unwrap();
        assert!(evidence.refresh().improved);
        assert!(!evidence.refresh().improved);
        std::fs::write(dir.path().join("report.json"), "{}").unwrap();
        assert_eq!(evidence.refresh().stale, 1);
    }
    #[test]
    fn should_recognize_failed_check_becoming_current_pass() {
        let dir = fixture();
        publish(&dir, "fail", "original");
        let mut evidence = ProgressEvidence::new(dir.path()).unwrap();
        assert!(!evidence.refresh().improved);
        publish(&dir, "pass", "original");
        assert!(evidence.refresh().improved);
    }

    fn set_findings(dir: &tempfile::TempDir, findings: serde_json::Value) {
        let path = dir.path().join("report.json");
        let mut report: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        report["findings"] = findings;
        let bytes = serde_json::to_vec(&report).unwrap();
        std::fs::write(path, &bytes).unwrap();
        let path = dir.path().join(".octos/progress.json");
        let mut index: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        index["checks"][0]["sha256"] = hash(&bytes).into();
        std::fs::write(path, serde_json::to_vec(&index).unwrap()).unwrap();
    }

    #[test]
    fn should_require_explicit_closure_and_preserve_removed_checks() {
        let dir = fixture();
        publish(&dir, "fail", "original");
        set_findings(&dir, json!([{"id":"overlap", "status":"open"}]));
        let mut evidence = ProgressEvidence::new(dir.path()).unwrap();
        assert_eq!(evidence.refresh().open_findings, vec!["copy:overlap"]);
        std::fs::write(dir.path().join("artifact.txt"), "temporarily stale").unwrap();
        let stale = evidence.refresh();
        assert_eq!(stale.stale, 1);
        assert_eq!(stale.open_findings, vec!["copy:overlap"]);
        std::fs::write(dir.path().join("artifact.txt"), "original").unwrap();
        publish(&dir, "pass", "original");
        let dropped = evidence.refresh();
        assert!(!dropped.improved);
        assert_eq!(dropped.open_findings, vec!["copy:overlap"]);
        std::fs::write(
            dir.path().join(".octos/progress.json"),
            r#"{"schema_version":1,"checks":[]}"#,
        )
        .unwrap();
        assert_eq!(evidence.refresh().removed_checks, vec!["copy"]);
        publish(&dir, "pass", "original");
        set_findings(&dir, json!([{"id":"overlap", "status":"closed"}]));
        let closed = evidence.refresh();
        assert!(closed.improved);
        assert_eq!(closed.resolved, vec!["copy:overlap"]);
    }

    #[test]
    fn should_reject_duplicate_findings_and_traversing_report_path() {
        let dir = fixture();
        let mut evidence = ProgressEvidence::new(dir.path()).unwrap();
        set_findings(
            &dir,
            json!([{"id":"same", "status":"closed"}, {"id":"same", "status":"closed"}]),
        );
        assert_eq!(evidence.refresh().stale, 1);
        publish(&dir, "pass", "original");
        let path = dir.path().join(".octos/progress.json");
        let mut index: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        index["checks"][0]["report"] = "../report.json".into();
        std::fs::write(path, serde_json::to_vec(&index).unwrap()).unwrap();
        assert_eq!(evidence.refresh().stale, 1);
    }
}
