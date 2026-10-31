//! Opt-in observation of completed, artifact-bound check cycles. No model calls.
//!
//! A producer's check result remains a claim by that checker. This observer does
//! not judge visual quality, execute validators, or authorize more resources.
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use eyre::{Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const MAX_RECORD_BYTES: u64 = 256 * 1024;
const POLICY: &str = ".octos/continuation.json";
const STATE: &str = ".octos/continuation-state.json";

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Decision {
    Continue,
    Notice(String),
    Yield(String),
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Policy {
    schema_version: u32,
    task_id: String,
    owner_agent: String,
    evidence_file: String,
    stagnant_attempts: u32,
    required_checks: Vec<Check>,
    deliverables: Vec<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Check {
    id: String,
    #[serde(default)]
    epsilon: f64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    schema_version: u32,
    task_id: String,
    attempt: u64,
    phase: Phase,
    artifacts: Vec<Binding>,
    checks: Vec<Observation>,
}
#[derive(Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Pending,
    Checked,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    path: String,
    sha256: String,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Observation {
    id: String,
    status: Status,
    /// A checker-defined nonnegative residual, where lower is better.
    /// It is not an arbitrary producer quality score.
    #[serde(default)]
    remaining: Option<f64>,
}
#[derive(Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
enum Status {
    #[default]
    Unknown,
    Fail,
    Pass,
}
#[derive(Default, Deserialize, Serialize)]
struct State {
    policy_digest: String,
    attempt: u64,
    snapshot_digest: String,
    best: BTreeMap<String, Observation>,
    previous: BTreeMap<String, Observation>,
    flat_attempts: u32,
    warned: bool,
    final_notice: Option<String>,
}

pub(super) struct Observer {
    root: PathBuf,
    owner_agent: String,
}
impl Observer {
    pub(super) fn new(root: &Path, owner_agent: &str) -> Self {
        Self {
            root: root.to_path_buf(),
            owner_agent: owner_agent.to_owned(),
        }
    }

    pub(super) fn poll(&self, final_answer: bool) -> Result<Decision> {
        if !self.root.join(POLICY).try_exists()? {
            return Ok(Decision::Continue);
        }
        let root = self.root.canonicalize()?;
        let policy_bytes = record(&confined(&root, POLICY)?)?;
        let policy: Policy = serde_json::from_slice(&policy_bytes)?;
        if policy.owner_agent != self.owner_agent {
            return Ok(Decision::Continue);
        }
        ensure!(
            policy.schema_version == 1,
            "unsupported continuation policy version"
        );
        ensure!(
            !policy.task_id.is_empty() && policy.task_id.len() <= 256,
            "invalid task id"
        );
        ensure!(policy.stagnant_attempts > 0, "invalid stagnation window");
        ensure!(
            !policy.required_checks.is_empty() && policy.required_checks.len() <= 512,
            "declare 1..512 required checks"
        );
        ensure!(
            !policy.deliverables.is_empty() && policy.deliverables.len() <= 512,
            "declare the task's delivery paths"
        );
        let mut ids = std::collections::BTreeSet::new();
        for c in &policy.required_checks {
            ensure!(
                !c.id.is_empty() && c.id.len() <= 256 && ids.insert(&c.id),
                "invalid/duplicate check id"
            );
            ensure!(
                c.epsilon.is_finite() && c.epsilon >= 0.,
                "invalid residual tolerance"
            );
        }
        let state_path = confined(&root, STATE)?;
        let mut state: State = if state_path.try_exists()? {
            serde_json::from_slice(&record(&state_path)?)?
        } else {
            State::default()
        };
        let policy_digest = digest(&policy_bytes);
        ensure!(
            state.policy_digest.is_empty() || state.policy_digest == policy_digest,
            "continuation policy changed during this task; preserve its evidence and explicitly start a new task policy"
        );
        state.policy_digest = policy_digest;

        let loaded = (|| -> Result<(Snapshot, String)> {
            let bytes = record(&confined(&root, &policy.evidence_file)?)?;
            let snapshot: Snapshot = serde_json::from_slice(&bytes)?;
            ensure!(
                snapshot.schema_version == 1 && snapshot.task_id == policy.task_id,
                "evidence belongs to another task/version"
            );
            ensure!(snapshot.attempt > 0, "attempt must be positive");
            Ok((snapshot, digest(&bytes)))
        })();
        let (snapshot, fingerprint) = match loaded {
            Ok(v) => v,
            Err(_) if !final_answer => return Ok(Decision::Continue),
            Err(e) => {
                return finish_gap(
                    &state_path,
                    &mut state,
                    "missing_or_invalid",
                    &format!("Check evidence unavailable: {e}"),
                );
            }
        };
        if snapshot.phase == Phase::Pending {
            if !final_answer {
                return Ok(Decision::Continue);
            }
            return finish_gap(
                &state_path,
                &mut state,
                &fingerprint,
                "External work/checking is pending. Preserve its job handle and wait for actual completion; do not claim completion.",
            );
        }
        // Polls and old attempts never consume a repair round. On EndTurn still
        // revalidate bytes: the artifact can have changed without a new report.
        if !final_answer && snapshot.attempt <= state.attempt {
            return Ok(Decision::Continue);
        }
        let checked = (|| -> Result<BTreeMap<String, Observation>> {
            ensure!(snapshot.attempt >= state.attempt, "replayed older evidence");
            ensure!(
                snapshot.attempt != state.attempt || fingerprint == state.snapshot_digest,
                "completed attempt was rewritten; publish a fresh attempt"
            );
            ensure!(
                !snapshot.artifacts.is_empty() && snapshot.artifacts.len() <= 1024,
                "checked evidence must bind actual artifacts"
            );
            for binding in &snapshot.artifacts {
                let path = confined(&root, &binding.path)?;
                ensure!(
                    binding.sha256.len() == 64 && file_digest(&path)? == binding.sha256,
                    "stale artifact binding: {}",
                    binding.path
                );
            }
            if final_answer {
                for delivery in &policy.deliverables {
                    let path = confined(&root, delivery)?;
                    ensure!(
                        snapshot
                            .artifacts
                            .iter()
                            .any(|b| confined(&root, &b.path).ok().as_ref() == Some(&path)),
                        "delivery path has no current check binding: {}",
                        delivery
                    );
                }
            }
            let mut observed = BTreeMap::new();
            for c in &snapshot.checks {
                ensure!(
                    c.id.len() <= 256 && !observed.contains_key(&c.id),
                    "invalid/duplicate observation"
                );
                ensure!(
                    c.remaining.is_none_or(|v| v.is_finite() && v >= 0.),
                    "invalid residual"
                );
                observed.insert(c.id.clone(), c.clone());
            }
            Ok(policy
                .required_checks
                .iter()
                .map(|c| {
                    let value = observed.remove(&c.id).unwrap_or(Observation {
                        id: c.id.clone(),
                        status: Status::Unknown,
                        remaining: None,
                    });
                    (c.id.clone(), value)
                })
                .collect())
        })();
        let current = match checked {
            Ok(v) => v,
            Err(_) if !final_answer => return Ok(Decision::Continue),
            Err(e) => {
                return finish_gap(
                    &state_path,
                    &mut state,
                    &fingerprint,
                    &format!("Check evidence is not current: {e}"),
                );
            }
        };
        let ready = current.values().all(|o| o.status == Status::Pass);
        if snapshot.attempt > state.attempt {
            let improved = policy.required_checks.iter().any(|c| {
                state
                    .best
                    .get(&c.id)
                    .is_some_and(|b| better(&current[&c.id], b, c.epsilon))
            });
            let regressed = policy.required_checks.iter().any(|c| {
                state
                    .previous
                    .get(&c.id)
                    .is_some_and(|p| better(p, &current[&c.id], c.epsilon))
            });
            if state.attempt == 0 || (improved && !regressed) || ready {
                state.flat_attempts = 0;
                state.warned = false;
            } else {
                state.flat_attempts = state.flat_attempts.saturating_add(1);
            }
            for c in &policy.required_checks {
                if !regressed
                    && state
                        .best
                        .get(&c.id)
                        .is_none_or(|b| better(&current[&c.id], b, c.epsilon))
                {
                    state.best.insert(c.id.clone(), current[&c.id].clone());
                }
            }
            state.previous = current.clone();
            state.attempt = snapshot.attempt;
            state.snapshot_digest = fingerprint.clone();
            let was_warned = state.warned;
            if !ready && state.flat_attempts >= policy.stagnant_attempts {
                state.warned = true;
                save(&state_path, &state)?;
                let message = format!(
                    "[needs_replan] {} checked attempts without a new best result without regression. Unresolved checks: {}. Preserve artifacts and evidence. Diagnose a different cause or hand back to the coordinator; do not restart the same attempt or claim completion.",
                    state.flat_attempts,
                    unresolved(&current)
                );
                return Ok(if was_warned {
                    Decision::Yield(message)
                } else {
                    Decision::Notice(message)
                });
            }
            save(&state_path, &state)?;
        }
        if final_answer && !ready {
            return finish_gap(
                &state_path,
                &mut state,
                &fingerprint,
                &format!(
                    "Required checks remain fail/unknown: {}. Check or repair the actual artifact, or report a concrete blocker. A repeated unchanged completion claim will return control with needs_replan.",
                    unresolved(&current)
                ),
            );
        }
        Ok(Decision::Continue)
    }
}
fn better(a: &Observation, b: &Observation, epsilon: f64) -> bool {
    a.status > b.status
        || (a.status == b.status
            && matches!((a.remaining, b.remaining), (Some(a), Some(b)) if a + epsilon < b))
}
fn unresolved(checks: &BTreeMap<String, Observation>) -> String {
    let ids: Vec<_> = checks
        .values()
        .filter(|o| o.status != Status::Pass)
        .map(|o| &o.id)
        .take(16)
        .collect();
    serde_json::to_string(&ids).unwrap_or_default()
}
fn finish_gap(path: &Path, state: &mut State, key: &str, message: &str) -> Result<Decision> {
    let repeated = state.final_notice.as_deref() == Some(key);
    state.final_notice = Some(key.to_string());
    save(path, state)?;
    Ok(if repeated {
        Decision::Yield(format!("[needs_replan] {message}"))
    } else {
        Decision::Notice(format!("[continuation evidence] {message}"))
    })
}
fn confined(root: &Path, relative: &str) -> Result<PathBuf> {
    let p = Path::new(relative);
    ensure!(
        !relative.is_empty()
            && p.components()
                .all(|c| matches!(c, Component::Normal(_) | Component::CurDir)),
        "evidence path must stay in its workspace"
    );
    let joined = root.join(p);
    let resolved = if joined.try_exists()? {
        joined.canonicalize()?
    } else {
        joined
            .parent()
            .ok_or_else(|| eyre::eyre!("missing parent"))?
            .canonicalize()?
            .join(
                joined
                    .file_name()
                    .ok_or_else(|| eyre::eyre!("evidence path has no filename"))?,
            )
    };
    ensure!(
        resolved.starts_with(root),
        "evidence path escapes workspace"
    );
    Ok(resolved)
}
fn record(path: &Path) -> Result<Vec<u8>> {
    let f = fs::File::open(path)?;
    ensure!(f.metadata()?.is_file(), "evidence must be a regular file");
    let mut bytes = Vec::new();
    f.take(MAX_RECORD_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_RECORD_BYTES,
        "evidence record too large"
    );
    Ok(bytes)
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn file_digest(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    ensure!(
        file.metadata()?.is_file(),
        "bound artifact must be a regular file"
    );
    let mut h = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        h.update(&buffer[..n]);
    }
    Ok(format!("{:x}", h.finalize()))
}
fn save(path: &Path, state: &State) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| eyre::eyre!("continuation state path has no parent"))?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(&serde_json::to_vec(state)?)?;
    file.persist(path).map_err(|e| eyre::eyre!(e.error))?;
    Ok(())
}

#[cfg(test)]
#[path = "iteration_evidence_tests.rs"]
mod tests;
