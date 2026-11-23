//! Read-only coverage of current page work and coordinator-declared assignments.
//! This is neither a scheduler nor an attestation of live peer execution.
use crate::{Context, page_gate};
use anyhow::{Context as _, Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Assignment {
    page: String,
    phase: String,
    peer: String,
    state: String,
    #[serde(default)]
    candidate_sha256: Option<String>,
    #[serde(default)]
    dependency: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Peer {
    peer: String,
    state: String,
    #[serde(default)]
    authorized: bool,
    #[serde(default)]
    attached: bool,
}

fn identity(s: &str) -> &str {
    page_gate::identity(s)
}

fn name(s: &str) -> bool {
    !s.is_empty() && s.trim() == s && s.len() <= 512
}

pub fn check(ctx: &Context, request: &Value) -> Result<Value> {
    let mut gate = page_gate::check(ctx)?;
    ensure!(
        gate["active"] == true,
        "page_dispatch requires an active page plan"
    );
    if gate["pages"]
        .as_array()
        .is_some_and(|pages| pages.iter().any(|p| p["work_stage"] == "arbitrate"))
    {
        gate["arbiter_agent"] = crate::review_routes::load_plan(ctx)?["arbiter_agent"].clone();
    }
    audit(&gate, request)
}

fn audit(gate: &Value, request: &Value) -> Result<Value> {
    let assignments: Vec<Assignment> = serde_json::from_value(
        request
            .get("assignments")
            .cloned()
            .unwrap_or_else(|| json!([])),
    )
    .context("invalid page_dispatch assignments")?;
    let peers: Vec<Peer> =
        serde_json::from_value(request.get("peers").cloned().unwrap_or_else(|| json!([])))
            .context("invalid page_dispatch peers")?;
    ensure!(
        assignments.len() <= 1024 && peers.len() <= 1024,
        "dispatch input exceeds 1024 rows"
    );
    let pages = gate["pages"].as_array().context("page gate has no pages")?;
    let makers: BTreeSet<&str> = pages
        .iter()
        .filter_map(|p| p["maker_session"].as_str())
        .map(identity)
        .collect();
    let coordinator = identity(gate["coordinator_session"].as_str().unwrap_or_default());
    let mut roster = BTreeMap::new();
    for peer in &peers {
        ensure!(
            name(&peer.peer),
            "peer identity must be a nonempty trimmed string"
        );
        ensure!(
            ["idle", "running", "queued", "unknown", "unavailable"].contains(&peer.state.as_str()),
            "peer state must be idle, running, queued, unknown or unavailable"
        );
        ensure!(
            roster.insert(identity(&peer.peer), peer).is_none(),
            "duplicate peer identity"
        );
    }
    let mut invalid = Vec::new();
    let mut completed_work = Vec::new();
    let mut dependency_claims = Vec::new();
    let mut covered = BTreeSet::new();
    let mut used = BTreeSet::new();
    let mut review_queues: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    let mut seen = BTreeSet::new();
    let mut running_peers = BTreeSet::new();
    for assignment in &assignments {
        ensure!(
            name(&assignment.page) && name(&assignment.peer),
            "assignment page/peer must be nonempty trimmed strings"
        );
        ensure!(
            ["make", "review", "repair", "arbitrate", "diagnose"]
                .contains(&assignment.phase.as_str()),
            "assignment phase must be make, review, repair, arbitrate or diagnose"
        );
        ensure!(
            ["running", "queued", "waiting"].contains(&assignment.state.as_str()),
            "assignment state must be running, queued or waiting"
        );
        let error = (|| -> Result<()> {
            let row = pages
                .iter()
                .find(|p| p["id"] == assignment.page)
                .context("page is outside the plan")?;
            if row["work_stage"] == "complete" {
                completed_work.push(json!({"page":assignment.page,"peer":assignment.peer}));
                return Ok(());
            }
            ensure!(
                row["work_stage"] != "blocked",
                "blocked candidate already has an evidenced peer outcome"
            );
            ensure!(
                seen.insert((&assignment.page, &assignment.phase)),
                "duplicate page/phase assignment"
            );
            let peer_id = identity(&assignment.peer);
            let peer = roster
                .get(peer_id)
                .context("peer has no declared roster entry")?;
            ensure!(
                peer.authorized && peer.attached,
                "peer authorization/attachment is not declared available"
            );
            ensure!(
                !["unknown", "unavailable"].contains(&peer.state.as_str()),
                "peer execution state is unknown/unavailable"
            );
            if matches!(
                assignment.phase.as_str(),
                "review" | "arbitrate" | "diagnose"
            ) {
                ensure!(
                    peer_id != coordinator && !makers.contains(peer_id),
                    "resolution and review peers must be independent of coordinator and all makers"
                );
                let reviewer = row["reviewer_session"].as_str().map(identity);
                if assignment.phase == "review"
                    && let Some(assigned) = row["assigned_reviewer_session"].as_str()
                {
                    ensure!(
                        peer_id == identity(assigned),
                        "review belongs to the assigned independent page reviewer"
                    );
                }
                if matches!(assignment.phase.as_str(), "arbitrate" | "diagnose") {
                    ensure!(
                        Some(peer_id) != reviewer,
                        "use a separate peer to resolve the reviewer's referral"
                    );
                }
                if assignment.phase == "arbitrate" {
                    if let Some(arbiter) = row["confirmation_peer"].as_str().or(gate["arbiter_agent"].as_str()) {
                        ensure!(
                            peer_id == identity(arbiter),
                            "use the frozen arbiter session"
                        );
                    }
                } else if assignment.phase == "review" && row["review_current"] == true {
                    ensure!(
                        Some(peer_id) == reviewer,
                        "clarification belongs to the bound reviewer"
                    );
                }
            } else {
                ensure!(
                    Some(peer_id) == row["maker_session"].as_str().map(identity),
                    "production must stay with the declared page maker"
                );
            }
            if assignment.state == "waiting" {
                ensure!(
                    assignment.dependency.as_deref().is_some_and(name),
                    "waiting work needs a specific dependency"
                );
                dependency_claims.push(json!({"page":assignment.page,"phase":assignment.phase,
                    "peer":assignment.peer,"dependency":assignment.dependency,"verified":false}));
                return Ok(());
            }
            ensure!(
                row["work_stage"] == assignment.phase,
                "assignment phase does not match current bound page work"
            );
            if assignment.phase != "make" {
                ensure!(
                    assignment.candidate_sha256.as_deref() == row["candidate"]["sha256"].as_str(),
                    "assignment must name the exact current candidate SHA-256"
                );
            }
            if assignment.state == "running" {
                ensure!(
                    peer.state == "running",
                    "running assignment contradicts the declared peer execution state"
                );
                running_peers.insert(peer_id);
            } else {
                ensure!(
                    ["running", "queued"].contains(&peer.state.as_str()),
                    "queued assignment contradicts the declared peer execution state"
                );
            }
            covered.insert(assignment.page.as_str());
            used.insert(peer_id);
            if assignment.phase == "review" {
                review_queues
                    .entry(peer_id)
                    .or_default()
                    .insert(&assignment.page);
            }
            Ok(())
        })();
        if let Err(error) = error {
            invalid.push(json!({"page":assignment.page,"peer":assignment.peer,"reason":format!("{error:#}")}));
        }
    }
    let idle: Vec<_> = roster
        .iter()
        .filter(|(id, peer)| {
            peer.authorized
                && peer.attached
                && peer.state == "idle"
                && !makers.contains(**id)
                && **id != coordinator
                && !used.contains(**id)
        })
        .map(|(_, peer)| peer.peer.as_str())
        .collect();
    let mut serialization = Vec::new();
    let mut capacity_limited = Vec::new();
    for (peer, queue) in review_queues {
        if queue.len() < 2 {
            continue;
        }
        let item = json!({"peer":peer,"pages":queue,"eligible_idle_reviewers":idle});
        if idle.is_empty() {
            capacity_limited.push(item);
        } else {
            serialization.push(item);
        }
    }
    let dispositions: Vec<_> = pages
        .iter()
        .filter(|p| p["disposition"].is_object() && p["work_stage"] != "blocked")
        .map(|p| json!({"page":p["id"],"phase":p["work_stage"],"candidate":p["candidate"],"disposition":p["disposition"]}))
        .collect();
    let unassigned: Vec<_> = pages
        .iter()
        .filter(|p| {
            p["work_stage"] != "complete"
                && p["work_stage"] != "blocked"
                && !covered.contains(p["id"].as_str().unwrap_or_default())
        })
        .map(|p| {
            json!({"page":p["id"],"phase":p["work_stage"],"candidate":p["candidate"],
            "maker_session":p["maker_session"],"reason":p["reason"]})
        })
        .collect();
    let complete = invalid.is_empty()
        && unassigned.is_empty()
        && serialization.is_empty()
        && capacity_limited.is_empty();
    let blocked: Vec<_> = pages
        .iter()
        .filter(|p| p["work_stage"] == "blocked")
        .map(|p| json!({"page":p["id"],"candidate":p["candidate"],"disposition":p["disposition"]}))
        .collect();
    let blocked_only = !blocked.is_empty()
        && pages
            .iter()
            .all(|p| p["work_stage"] == "blocked" || p["work_stage"] == "complete");
    let handoff = blocked_only.then(|| json!({"outcome":"blocked","automatic_retry_allowed":false,
        "next":"Record the verified page blockers with evidence_blocked when the workspace has a continuation policy. Reconcile queued peer checkpoints with peer_checkpoint_inbox on supported hosts and acknowledge the exact handled receipts. Stop consumed task monitors and end this attempt as blocked. Resume on changed evidence or capability."}));
    Ok(
        json!({"plan":gate["plan"],"page_count":pages.len(),"accepted_pages":gate["accepted_pages"],
        "pages":pages,"needs_disposition":dispositions,"blocked_pages":blocked,"only_blocked_pages_remain":blocked_only,"handoff":handoff,"ready_unassigned":unassigned,"avoidable_serialization":serialization,
        "capacity_limited_serialization":capacity_limited,"invalid_assignments":invalid,
        "eligible_idle_reviewers":idle,"completed_page_assignments":completed_work,
        "unverified_dependency_claims":dependency_claims,"declared_running_count":running_peers.len(),
        "declared_coverage_complete":complete,
        "all_work_waiting":if blocked_only || !unassigned.is_empty() || !serialization.is_empty() || !capacity_limited.is_empty() {json!(false)} else {Value::Null},
        "host_verified":false,"agent_execution_verified":false,"quality_approved":false,
        "limitations":"Page readiness is checked against bound files. Assignments, peer availability, authorization, attachment and dependencies are coordinator declarations, not host attestations. This check does not dispatch peers, grant permissions, judge quality or enforce EndTurn. A complete plan does not prove simultaneous execution; verify native current-turn events."}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(count: usize) -> (Value, Value) {
        let mut pages = Vec::new();
        let mut peers = Vec::new();
        let mut assignments = Vec::new();
        for i in 0..count {
            let page = format!("page-{i}");
            let peer = format!("reviewer-{i}");
            let hash = format!("{i:064x}");
            pages.push(json!({"id":page,"maker_session":format!("maker-{i}"),
                "work_stage":"review","ready":false,"candidate_current":true,
                "candidate":{"path":format!("{page}.json"),"sha256":hash}}));
            peers.push(json!({"peer":peer,"state":"running","authorized":true,"attached":true}));
            assignments.push(json!({"page":page,"phase":"review","peer":peer,
                "state":"running","candidate_sha256":hash}));
        }
        (
            json!({"pages":pages,"coordinator_session":"coordinator","accepted_pages":0}),
            json!({"assignments":assignments,"peers":peers}),
        )
    }

    #[test]
    fn covers_all_twenty_ready_pages_without_manufacturing_execution_proof() {
        let (gate, request) = fixture(20);
        let result = audit(&gate, &request).unwrap();
        assert_eq!(result["declared_coverage_complete"], true);
        assert_eq!(result["declared_running_count"], 20);
        assert_eq!(result["ready_unassigned"], json!([]));
        assert_eq!(result["host_verified"], false);
        assert_eq!(result["agent_execution_verified"], false);
        assert_eq!(result["quality_approved"], false);
        assert!(result["all_work_waiting"].is_null());
    }

    #[test]
    fn catches_twenty_ready_pages_serialized_behind_one_reviewer() {
        let (gate, mut request) = fixture(20);
        for (i, assignment) in request["assignments"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .enumerate()
        {
            assignment["peer"] = json!("reviewer-0");
            assignment["state"] = json!(if i == 0 { "running" } else { "queued" });
        }
        for peer in request["peers"].as_array_mut().unwrap().iter_mut().skip(1) {
            peer["state"] = json!("idle");
        }
        let result = audit(&gate, &request).unwrap();
        assert_eq!(result["declared_coverage_complete"], false);
        assert_eq!(result["declared_running_count"], 1);
        assert_eq!(
            result["avoidable_serialization"][0]["pages"]
                .as_array()
                .unwrap()
                .len(),
            20
        );
        assert_eq!(
            result["eligible_idle_reviewers"].as_array().unwrap().len(),
            19
        );
        assert_eq!(result["all_work_waiting"], false);
    }

    #[test]
    fn initial_review_dispatch_respects_the_planned_page_reviewer() {
        let (mut gate, mut request) = fixture(2);
        gate["pages"][1]["assigned_reviewer_session"] = json!("session:reviewer-1");
        assert_eq!(
            audit(&gate, &request).unwrap()["declared_coverage_complete"],
            true
        );
        request["assignments"][1]["peer"] = json!("reviewer-0");
        let result = audit(&gate, &request).unwrap();
        assert_eq!(result["declared_coverage_complete"], false);
        assert_eq!(result["invalid_assignments"][0]["page"], "page-1");
    }

    #[test]
    fn queued_unknown_unattached_and_unauthorized_peers_are_not_idle_capacity() {
        let (gate, mut request) = fixture(2);
        request["assignments"][1]["peer"] = json!("reviewer-0");
        request["assignments"][1]["state"] = json!("queued");
        for state in ["queued", "running", "unknown", "unavailable"] {
            request["peers"][1]["state"] = json!(state);
            let result = audit(&gate, &request).unwrap();
            assert_eq!(result["eligible_idle_reviewers"], json!([]));
            assert_eq!(result["avoidable_serialization"], json!([]));
            assert_eq!(
                result["capacity_limited_serialization"]
                    .as_array()
                    .unwrap()
                    .len(),
                1
            );
            assert_eq!(result["declared_coverage_complete"], false);
        }
        request["peers"][1]["state"] = json!("idle");
        for field in ["authorized", "attached"] {
            request["peers"][1][field] = json!(false);
            assert_eq!(
                audit(&gate, &request).unwrap()["eligible_idle_reviewers"],
                json!([])
            );
            request["peers"][1][field] = json!(true);
        }
    }

    #[test]
    fn ready_unassigned_and_unverified_waiting_cannot_be_all_external_work() {
        let (gate, mut request) = fixture(2);
        request["assignments"][1]["state"] = json!("waiting");
        request["assignments"][1]["dependency"] = json!("another page review");
        let result = audit(&gate, &request).unwrap();
        assert_eq!(result["ready_unassigned"][0]["page"], "page-1");
        assert_eq!(result["unverified_dependency_claims"][0]["verified"], false);
        assert_eq!(result["declared_coverage_complete"], false);
        assert_eq!(result["all_work_waiting"], false);
    }

    #[test]
    fn finished_pages_do_not_create_work_and_stale_or_self_review_cannot_cover_ready_work() {
        let (mut gate, mut request) = fixture(2);
        gate["pages"][0]["work_stage"] = json!("complete");
        request["assignments"].as_array_mut().unwrap().remove(0);
        assert_eq!(
            audit(&gate, &request).unwrap()["declared_coverage_complete"],
            true
        );
        request["assignments"][0]["candidate_sha256"] = json!("stale");
        assert_eq!(
            audit(&gate, &request).unwrap()["invalid_assignments"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        request["assignments"][0]["candidate_sha256"] =
            gate["pages"][1]["candidate"]["sha256"].clone();
        request["assignments"][0]["peer"] = json!("session:maker-1");
        request["peers"][1]["peer"] = json!("maker-1");
        assert_eq!(
            audit(&gate, &request).unwrap()["declared_coverage_complete"],
            false
        );
    }

    #[test]
    fn repairs_keep_original_owner_and_running_is_not_a_plan_label() {
        let (mut gate, mut request) = fixture(1);
        gate["pages"][0]["work_stage"] = json!("repair");
        request["assignments"][0]["phase"] = json!("repair");
        assert_eq!(
            audit(&gate, &request).unwrap()["declared_coverage_complete"],
            false
        );
        request["assignments"][0]["peer"] = json!("maker-0");
        request["peers"][0]["peer"] = json!("maker-0");
        request["peers"][0]["state"] = json!("idle");
        assert_eq!(
            audit(&gate, &request).unwrap()["declared_coverage_complete"],
            false
        );
        request["peers"][0]["state"] = json!("running");
        assert_eq!(
            audit(&gate, &request).unwrap()["declared_coverage_complete"],
            true
        );
    }

    #[test]
    fn planned_queue_on_an_idle_peer_is_not_current_dispatch_coverage() {
        let (gate, mut request) = fixture(1);
        request["assignments"][0]["state"] = json!("queued");
        request["peers"][0]["state"] = json!("idle");
        let result = audit(&gate, &request).unwrap();
        assert_eq!(result["declared_coverage_complete"], false);
        assert_eq!(result["ready_unassigned"].as_array().unwrap().len(), 1);
        request["peers"][0]["state"] = json!("queued");
        assert_eq!(
            audit(&gate, &request).unwrap()["declared_coverage_complete"],
            true
        );
    }
}
