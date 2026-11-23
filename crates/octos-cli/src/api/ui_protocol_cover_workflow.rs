//! Isolated cover trial. The host keeps only validated native receipt IDs and
//! outcomes. Replaying them re-creates VM waits, never external peer effects.
use super::pptx_dispatch::digest;
use crate::peers::peer_io;
use eyre::{Result, ensure, eyre};
use octoscript_capabilities::{CapabilityRuntime, ToolPolicy};
use octoscript_workflow::{WorkflowEngine, WorkflowError, WorkflowEvent, WorkflowStep};
use serde_json::{Value, json};
use std::{
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};
const SCRIPT: &str = include_str!("cover_page.octoscript");
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
fn follow(events: &[Value]) -> Result<String> {
    let mut runtime = CapabilityRuntime::default();
    for task in ["make", "review", "repair", "arbitrate"] {
        let mut policy = ToolPolicy::new(format!("page.{task}"));
        policy.max_calls = 512;
        runtime
            .register_external_tool(policy)
            .map_err(|e| eyre!("{e}"))?;
    }
    let mut engine = WorkflowEngine::new(runtime);
    let plan = engine
        .plan(vec![WorkflowStep::new("cover-lifecycle", SCRIPT)])
        .map_err(|e| eyre!("{e}"))?;
    let approval = engine.approve(&plan).map_err(|e| eyre!("{e}"))?;
    let mut state = engine.execute(&plan, approval);
    for e in events {
        ensure!(
            matches!(state, Err(WorkflowError::StepSuspended { .. })),
            "workflow did not suspend: {state:?}"
        );
        let call = engine
            .claim_next_external_tool()
            .ok_or_else(|| eyre!("no pending peer operation"))?;
        let role = call.name.strip_prefix("page.").unwrap_or("");
        let kind = e["kind"]
            .as_str()
            .ok_or_else(|| eyre!("missing event kind"))?;
        ensure!(
            matches!(
                (role, kind),
                ("make" | "repair", "candidate")
                    | ("review", "verdict")
                    | ("arbitrate", "decision")
            ),
            "workflow expects {role}, got {kind}"
        );
        state = engine.complete_external_tool(
            call.id,
            Ok(e["outcome"]
                .as_str()
                .ok_or_else(|| eyre!("missing outcome"))?
                .to_owned()),
        );
    }
    match state {
        Ok(())
            if matches!(
                engine.events().last(),
                Some(WorkflowEvent::Completed { .. })
            ) =>
        {
            Ok("complete".into())
        }
        Err(WorkflowError::StepSuspended { .. }) => {
            let call = engine
                .claim_next_external_tool()
                .ok_or_else(|| eyre!("missing next operation"))?;
            Ok(call.name.trim_start_matches("page.").to_string())
        }
        other => Err(eyre!("workflow execution error: {other:?}")),
    }
}
/// Only the native router can supply checked predecessor IDs. A recovery
/// checkpoint restarts the VM at that immutable evidence without replaying
/// external effects. Earlier observed events stay in the journal.
fn projection(events: &[Value]) -> Vec<Value> {
    if let Some(i) = events.iter().rposition(|e| e["replay_from"].is_array()) {
        let mut projected = events[i]["replay_from"].as_array().unwrap().clone();
        projected.extend_from_slice(&events[i..]);
        projected
    } else {
        events.to_vec()
    }
}

pub(super) fn advance(
    root: &Path,
    pair: &str,
    page: &str,
    kind: &str,
    id: &str,
    outcome: &str,
) -> Result<String> {
    advance_bound(root, pair, page, kind, id, outcome, None)
}
pub(super) fn advance_bound(
    root: &Path,
    pair: &str,
    page: &str,
    kind: &str,
    id: &str,
    outcome: &str,
    predecessor: Option<(&str, Option<&str>)>,
) -> Result<String> {
    let parent = root
        .parent()
        .ok_or_else(|| eyre!("missing supervisor directory"))?;
    let dir = parent.join("pptx-cover-workflow");
    std::fs::create_dir_all(&dir)?;
    ensure!(
        pair.len() == 64 && pair.bytes().all(|c| c.is_ascii_hexdigit()),
        "invalid page pair"
    );
    let leaf = format!("{pair}.json");
    let lock = peer_io::mutation_lock(&dir);
    let guard = lock.lock().unwrap_or_else(|e| e.into_inner());
    let fingerprint = digest(SCRIPT.as_bytes());
    let mut journal: Value = match peer_io::read_peer_control_file(&dir, &leaf, 2 * 1024 * 1024)? {
        Some(s) => serde_json::from_str(&s)?,
        None => {
            json!({"page":page,"workflow_sha256":fingerprint,"started_at_ms":now_ms(),"events":[]})
        }
    };
    ensure!(
        journal["page"] == page && journal["workflow_sha256"] == fingerprint,
        "workflow plan changed"
    );
    let waiting = journal["next"].as_str().unwrap_or("make").to_owned();
    let events = journal["events"]
        .as_array_mut()
        .ok_or_else(|| eyre!("missing journal events"))?;
    if kind != "start" {
        if let Some(old) = events.iter().find(|v| v["kind"] == kind && v["id"] == id) {
            ensure!(old["outcome"] == outcome, "conflicting receipt");
            return old["next"]
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| eyre!("missing intent"));
        }
        let mut incoming = json!({"kind":kind,"id":id,"outcome":outcome,"observed_at_ms":now_ms()});
        let consumes = matches!(
            (waiting.as_str(), kind),
            ("make" | "repair", "candidate") | ("review", "verdict") | ("arbitrate", "decision")
        );
        if !consumes {
            let mut prior = Vec::new();
            if kind != "candidate" {
                let (candidate, review) =
                    predecessor.ok_or_else(|| eyre!("missing verified evidence for recovery"))?;
                prior.push(json!({"kind":"candidate","id":candidate,"outcome":"published"}));
                if kind == "decision" {
                    prior.push(json!({"kind":"verdict","id":review.ok_or_else(||eyre!("missing verified rejection"))?,"outcome":"manual"}));
                }
            }
            incoming["replay_from"] = json!(prior);
            incoming["previous_wait"] = json!(waiting);
        }
        events.push(incoming);
    }
    let next = follow(&projection(events))?;
    if kind != "start" {
        events.last_mut().unwrap()["next"] = json!(next);
    }
    journal["next"] = json!(next);
    peer_io::write_peer_file_durable_locked(
        &dir,
        &leaf,
        &serde_json::to_string_pretty(&journal)?,
        &guard,
    )?;
    Ok(next)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cover_workflow_consumes_native_outcomes() {
        let mut e = vec![];
        assert_eq!(follow(&e).unwrap(), "make");
        for (kind, outcome, next) in [
            ("candidate", "published", "review"),
            ("verdict", "manual", "arbitrate"),
            ("decision", "dispute", "review"),
            ("verdict", "native", "repair"),
            ("candidate", "published", "review"),
            ("verdict", "approved", "complete"),
        ] {
            e.push(json!({"kind":kind,"outcome":outcome}));
            assert_eq!(follow(&e).unwrap(), next);
        }
    }
}
