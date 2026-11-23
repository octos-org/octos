//! Deliver native page batches to already attached, shared-workspace peers.
use super::{PeerCheckpointHandoff, peer_send_input_authorized, staged_peer_dir};
use crate::{autonomy::agent_orchestrator::InProcessAgentOrchestrator, peers};
use eyre::{Result, ensure, eyre};
use octos_core::SessionKey;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Deserialize)]
struct Job {
    page: String,
    phase: String,
    peer: String,
    candidate_sha256: Option<String>,
    review_sha256: Option<String>,
    message: String,
    resume_after_round: Option<u32>,
}

pub(super) fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn required(dir: &Path, leaf: &str) -> Result<String> {
    peers::peer_io::read_peer_control_file(dir, leaf, 4 * 1024 * 1024)?
        .ok_or_else(|| eyre!("missing {leaf}; prepare the frozen page assignments first"))
}

// Head paths come from the frozen roster. Read bytes only within this workspace.
fn head_bytes(workspace: &Path, name: &Value) -> Result<Option<Vec<u8>>> {
    let path = Path::new(
        name.as_str()
            .ok_or_else(|| eyre!("missing page head path"))?,
    );
    match octos_llm::vision::read_workspace_bytes_no_follow(workspace, path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error)
            if error.chain().any(|e| {
                e.downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
            }) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn current_head(workspace: &Path, name: &Value) -> Result<Option<String>> {
    Ok(head_bytes(workspace, name)?.map(|bytes| digest(&bytes)))
}

pub(super) fn deliver(
    scheduler: &InProcessAgentOrchestrator,
    workspace: &Path,
    root: &Path,
    profile: &str,
    master: &SessionKey,
    expected: &str,
) -> Result<Value> {
    let dir = workspace.join(".octos");
    let raw = required(&dir, "pptx-peer-batch.json")?;
    ensure!(
        digest(raw.as_bytes()) == expected,
        "batch has changed; prepare current page assignments"
    );
    let batch: Value = serde_json::from_str(&raw)?;
    let (plan, binding, route) = frozen_plan(workspace, master)?;
    ensure!(
        matches!(
            batch["kind"].as_str(),
            Some("pptx-peer-batch-v1" | "pptx-peer-resume-v1")
        ) && batch["plan"] == binding
            && batch["coordinator"] == plan["coordinator_session"],
        "frozen plan or coordinator mismatch"
    );
    let roster: BTreeMap<&str, &Value> = plan["pages"]
        .as_array()
        .ok_or_else(|| eyre!("missing roster"))?
        .iter()
        .map(|row| {
            row["id"]
                .as_str()
                .map(|id| (id, row))
                .ok_or_else(|| eyre!("missing page id"))
        })
        .collect::<Result<_>>()?;
    let queue = AssignmentQueue {
        scheduler,
        workspace,
        root,
        profile,
        master,
        binding: &binding,
        plan: &plan,
        route: &route,
    };
    let jobs: Vec<Job> = serde_json::from_value(batch["jobs"].clone())?;
    ensure!(jobs.len() <= 2 * roster.len(), "invalid page batch size");
    let resume = batch["kind"] == "pptx-peer-resume-v1";
    ensure!(
        (!resume || jobs.len() <= 1)
            && jobs
                .iter()
                .all(|j| j.resume_after_round.is_some() == resume),
        "inconsistent page batch recovery envelope"
    );
    let mut results = Vec::new();
    let mut failed = 0;
    for job in jobs {
        let outcome = roster
            .get(job.page.as_str())
            .ok_or_else(|| eyre!("page outside frozen roster"))
            .and_then(|row| queue.deliver_job(row, &job, None));
        results.push(match outcome {
            Ok(status)=>json!({"page":job.page,"phase":job.phase,"peer":job.peer,"status":status}),
            Err(error)=>{failed+=1;json!({"page":job.page,"phase":job.phase,"peer":job.peer,"status":"failed","error":error.to_string()})}
        });
    }
    Ok(json!({"failed":failed,"assignments":results}))
}

fn frozen_plan(workspace: &Path, master: &SessionKey) -> Result<(Value, Value, Value)> {
    let dir = workspace.join(".octos");
    let raw_plan = required(&dir, "pptx-page-plan.json")?;
    let binding = json!({"path":".octos/pptx-page-plan.json","sha256":digest(raw_plan.as_bytes())});
    let route: Value = serde_json::from_str(&required(&dir, "pptx-verification-plan.json")?)?;
    let plan: Value = serde_json::from_str(&raw_plan)?;
    let owner = plan["coordinator_session"]
        .as_str()
        .ok_or_else(|| eyre!("missing coordinator"))?;
    ensure!(
        plan["schema_version"] == 2
            && route["mode"] == "per_page"
            && route["page_plan"] == binding
            && identity(owner) == master.0,
        "frozen plan or coordinator mismatch"
    );
    Ok((plan, binding, route))
}

fn identity(session: &str) -> &str {
    session.strip_prefix("session:").unwrap_or(session)
}

// Called with digests recomputed from the current files.
fn revision_id(kind: &str, hash: &str) -> String {
    let parts: Vec<_> = (0..8).map(|i| &hash[8 * i..8 * i + 8]).collect();
    format!("{kind}-{}", parts.join("-"))
}

fn occurrence(
    stage: &PeerCheckpointHandoff,
    binding: &Value,
    page: &str,
    phase: &str,
    candidate: Option<&str>,
    review: Option<&str>,
) -> String {
    let key = json!([
        stage.stage_id,
        stage.master.0,
        binding,
        page,
        phase,
        candidate,
        review
    ]);
    format!("pptx/{}", digest(key.to_string().as_bytes()))
}

fn review_grant(
    page: &str,
    plan: &Value,
    maker: &PeerCheckpointHandoff,
    reviewer: &PeerCheckpointHandoff,
) -> String {
    digest(
        json!([page, plan, maker.master, maker.stage_id, reviewer.stage_id])
            .to_string()
            .as_bytes(),
    )
}

struct AssignmentQueue<'a> {
    scheduler: &'a InProcessAgentOrchestrator,
    workspace: &'a Path,
    root: &'a Path,
    profile: &'a str,
    master: &'a SessionKey,
    binding: &'a Value,
    plan: &'a Value,
    route: &'a Value,
}

impl AssignmentQueue<'_> {
    fn stage_for(&self, name: &str) -> Option<PeerCheckpointHandoff> {
        let peer = SessionKey(identity(name).to_owned());
        PeerCheckpointHandoff::capture(self.root, self.profile, &peer, self.workspace)
            .filter(|stage| stage.master == *self.master)
    }

    // Same frozen selection as the native repair gate: named arbiter, or the
    // next page's reviewer for a multi-page roster. Never use a name from a prompt.
    fn confirmation_peer(&self, row: &Value) -> Option<&str> {
        if self.route["repair_gate"] != "verified-findings-v1" {
            return None;
        }
        let pages = self.plan["pages"].as_array()?;
        let pos = pages.iter().position(|p| p["id"] == row["id"])?;
        let peer = self.route["arbiter_agent"].as_str().or_else(|| {
            (pages.len() > 1)
                .then(|| pages[(pos + 1) % pages.len()]["reviewer_session"].as_str())
                .flatten()
        })?;
        let who = identity(peer);
        (who != self.master.0
            && row["reviewer_session"].as_str().map(identity) != Some(who)
            && pages
                .iter()
                .all(|p| p["maker_session"].as_str().map(identity) != Some(who)))
        .then_some(peer)
    }

    fn confirmation_grant(&self, pair: &str, stage: &PeerCheckpointHandoff) -> String {
        digest(
            json!(["confirmation-v1", pair, self.route, stage.stage_id])
                .to_string()
                .as_bytes(),
        )
    }

    fn grants(&self, row: &Value) -> Option<(String, Option<String>)> {
        let maker = self.stage_for(row["maker_session"].as_str()?)?;
        let reviewer = self.stage_for(row["reviewer_session"].as_str()?)?;
        if maker == reviewer {
            return None;
        }
        let pair = review_grant(row["id"].as_str()?, self.binding, &maker, &reviewer);
        let confirmation = self
            .confirmation_peer(row)
            .and_then(|name| self.stage_for(name))
            .map(|stage| self.confirmation_grant(&pair, &stage));
        Some((pair, confirmation))
    }

    fn activity_contract(
        &self,
        row: &Value,
        job: &Job,
        stage: &PeerCheckpointHandoff,
        id: &str,
        pair: &str,
        coordinator_issued: bool,
    ) -> Result<String> {
        let Self {
            workspace,
            root,
            binding,
            ..
        } = *self;
        use super::workflow_activity::PeerActivity;
        let data_dir = root
            .parent()
            .ok_or_else(|| eyre!("missing workflow data directory"))?;
        let kind = match job.phase.as_str() {
            "make" | "repair" => "candidate",
            "review" => "verdict",
            "arbitrate" => "decision",
            _ => unreachable!(),
        };
        let admission = if coordinator_issued && job.candidate_sha256.is_some() {
            // An existing deck may be adopted before all native receipts have
            // been replayed. Anchor the authorized input heads to today's cursor.
            super::cover_workflow::advance(root, pair, &job.page, "start", "", "")?;
            octos_workflows::activity::Admission::AdoptExisting
        } else {
            octos_workflows::activity::Admission::Awaited
        };
        let stream = std::path::PathBuf::from("pptx-cover-workflow").join(format!("{pair}.json"));
        let plan_path = Path::new(".octos/pptx-page-plan.json");
        let route_path = Path::new(".octos/pptx-verification-plan.json");
        let route_bytes =
            head_bytes(workspace, &json!(route_path))?.ok_or_else(|| eyre!("missing route"))?;
        ensure!(
            serde_json::from_slice::<Value>(&route_bytes)? == *self.route,
            "verification plan changed"
        );
        let route_hash = digest(&route_bytes);
        let plan = BTreeMap::from([
            (
                plan_path.into(),
                Some(
                    binding["sha256"]
                        .as_str()
                        .ok_or_else(|| eyre!("missing plan digest"))?
                        .to_owned(),
                ),
            ),
            (route_path.into(), Some(route_hash)),
        ]);
        let mut heads = BTreeMap::new();
        for (name, head) in [
            ("candidate", &job.candidate_sha256),
            ("review", &job.review_sha256),
        ] {
            heads.insert(
                row[name]
                    .as_str()
                    .ok_or_else(|| eyre!("missing {name} path"))?
                    .into(),
                head.clone(),
            );
        }
        if job.phase == "arbitrate" {
            let review = job
                .review_sha256
                .as_deref()
                .ok_or_else(|| eyre!("missing verdict"))?;
            heads.insert(
                format!(".octos/page-repair-decisions/{review}.json").into(),
                None,
            );
        }
        let run = octos_workflows::activity::ActivityAttempt {
            operation: id.to_owned(),
            attempt: 0,
            max_resumes: 1,
        };
        serde_json::to_string(
            &PeerActivity::bind(
                data_dir, stage, run, stream, &job.phase, kind, admission, plan, heads,
            )?
            .with_label(format!("page {} ({})", job.page, job.phase)),
        )
        .map_err(Into::into)
    }

    fn deliver_job(
        &self,
        row: &Value,
        job: &Job,
        armed_stage: Option<&PeerCheckpointHandoff>,
    ) -> Result<&'static str> {
        let Self {
            scheduler,
            workspace,
            root,
            profile,
            master,
            binding,
            ..
        } = *self;
        let assigned = match job.phase.as_str() {
            "make" | "repair" => row["maker_session"].as_str(),
            "prepare_review" | "review" => row["reviewer_session"].as_str(),
            "arbitrate" => self.confirmation_peer(row),
            _ => eyre::bail!("unknown assignment phase"),
        };
        ensure!(
            assigned == Some(job.peer.as_str())
                && !job.message.trim().is_empty()
                && job.message.len() <= 64 * 1024,
            "assignment differs from roster or message exceeds limit"
        );
        ensure!(
            current_head(workspace, &row["candidate"])? == job.candidate_sha256
                && current_head(workspace, &row["review"])? == job.review_sha256,
            "page changed; prepare the current assignment"
        );
        if job.phase == "arbitrate" {
            let review = job
                .review_sha256
                .as_deref()
                .ok_or_else(|| eyre!("missing verdict"))?;
            ensure!(
                job.candidate_sha256.is_some()
                    && current_head(
                        workspace,
                        &json!(format!(".octos/page-repair-decisions/{review}.json"))
                    )?
                    .is_none(),
                "independent decision already exists or candidate is missing"
            );
        }
        let peer = SessionKey(
            job.peer
                .strip_prefix("session:")
                .unwrap_or(&job.peer)
                .to_owned(),
        );
        let stage = PeerCheckpointHandoff::capture(root, profile, &peer, workspace)
            .ok_or_else(|| eyre!("peer must be attached in the shared workspace"))?;
        ensure!(
            stage.master == *master && armed_stage.is_none_or(|armed| armed == &stage),
            "different peer originator or stage"
        );
        peer_send_input_authorized(root, &stage.slug, &master.0).map_err(|e| eyre!("{e}"))?;
        let stage_dir = staged_peer_dir(root, &stage.slug).ok_or_else(|| eyre!("peer retired"))?;
        let occurrence = occurrence(
            &stage,
            binding,
            &job.page,
            &job.phase,
            job.candidate_sha256.as_deref(),
            job.review_sha256.as_deref(),
        );
        let id = if let Some(round) = job.resume_after_round {
            ensure!(
                round > 0 && job.phase != "prepare_review",
                "invalid recovery phase or round"
            );
            ensure!(
                scheduler.pptx_assignment_completed(&peer, &occurrence)?,
                "the original assignment for this phase and evidence has not finished"
            );
            if scheduler.peer_occurrence_recorded(&peer, &format!("{occurrence}/returned")) {
                // The durable return event already owns reconciliation of these heads.
                return Ok("handled_by_workflow");
            }
            format!("{occurrence}/resume/{round}")
        } else {
            occurrence
        };
        // Only coordinator-issued assignments arm an automatic route. A peer
        // cannot mint a new grant when another peer is resumed or re-staged.
        let stage_grants = self.grants(row);
        let grants = stage_grants.as_ref().filter(|_| armed_stage.is_none());
        if job.phase == "make" && armed_stage.is_none() && let Some((pair,_))=&grants {
            let intent=super::cover_workflow::advance(root,pair,&job.page,"start","","")?;
            ensure!(intent=="make","cover page workflow already expects {intent}");
        }
        let activity = if job.phase == "prepare_review" {
            None
        } else {
            let (pair, _) = stage_grants.as_ref().ok_or_else(|| {
                eyre!("attach both assigned peers before admitting a receipt-bound page task")
            })?;
            Some(self.activity_contract(row, job, &stage, &id, pair, armed_stage.is_none())?)
        };
        let outcome = peers::with_peer_input_lifetime(&stage_dir, |advance| -> Result<_> {
            let wire = peers::peer_wire_registry()
                .by_key
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            ensure!(
                wire.get(&peers::peer_wire_key(profile, &stage.slug)) == Some(&peer)
                    && PeerCheckpointHandoff::read(&stage_dir, profile, &stage.slug, None).as_ref()
                        == Some(&stage),
                "peer binding changed before delivery"
            );
            if let Some(round) = job.resume_after_round {
                ensure!(
                    peers::read_last_terminal_evidence(&stage_dir, &stage.slug)
                        .is_some_and(|(current, _)| current == round),
                    "peer terminal round differs; inspect its current native result"
                );
            }
            Ok(scheduler.enqueue_peer_assignment(
                crate::autonomy::agent_orchestrator::PPTX_ASSIGNMENT_GROUP,
                &peer,
                profile,
                &stage.slug,
                &id,
                &job.message,
                grants.iter().flat_map(|(pair, confirmation)| {
                    std::iter::once(("pptx_review_route", pair.clone())).chain(
                        confirmation
                            .iter()
                            .map(|value| ("pptx_confirmation_route", value.clone())),
                    )
                }),
                activity,
                advance,
            ))
        })?;
        match outcome
            .into_callback_result(&stage.slug)
            .map_err(|e| eyre!("{e}"))?
        {
            octos_agent::PeerSendInputDelivery::Queued => {
                peers::record_peer_brief(&stage_dir, &job.message);
                Ok("queued")
            }
            octos_agent::PeerSendInputDelivery::AlreadyQueued => Ok("already_dispatched"),
        }
    }
}

/// Called from the captured peer turn's concrete PPTX plugin. An earlier
/// coordinator dispatch must already have admitted both peers for this roster.
pub(super) struct PublicationRouter {
    pub root: std::path::PathBuf,
    pub profile: String,
    pub caller: SessionKey,
    pub stage: PeerCheckpointHandoff,
    pub turn: String,
}

impl PublicationRouter {
    fn check_caller(&self) -> Result<()> {
        let workspace = &self.stage.workspace;
        ensure!(
            super::session_workspace_root_for_profile(Some(&self.profile), &self.stage.master)
                .as_ref()
                == Some(workspace),
            "supervisor workspace changed"
        );
        let current_stage =
            PeerCheckpointHandoff::capture(&self.root, &self.profile, &self.caller, workspace);
        ensure!(
            current_stage.as_ref() == Some(&self.stage),
            "publishing peer binding changed"
        );
        if self.stage.task_id.is_some() {
            let dir = staged_peer_dir(&self.root, &self.stage.slug)
                .ok_or_else(|| eyre!("publishing peer retired"))?;
            ensure!(
                super::trusted_lifetime_projection(&dir, &self.profile, &self.stage.slug)
                    .is_some_and(|l| l.turn_id.as_deref() == Some(&self.turn)
                        && matches!(
                            l.phase,
                            super::LifetimePhase::Running | super::LifetimePhase::Pending
                        )),
                "publishing turn changed"
            );
        }
        Ok(())
    }

    fn enqueue(
        &self,
        scheduler: &InProcessAgentOrchestrator,
        publication: &Value,
    ) -> Result<Value> {
        let workspace = &self.stage.workspace;
        let master = &self.stage.master;
        self.check_caller()?;
        let (plan, binding, route) = frozen_plan(workspace, master)?;
        let handoff = &publication["handoff"];
        let page = handoff["page_id"]
            .as_str()
            .ok_or_else(|| eyre!("missing page id"))?;
        let row = plan["pages"]
            .as_array()
            .and_then(|rows| rows.iter().find(|row| row["id"] == page))
            .ok_or_else(|| eyre!("page outside frozen roster"))?;
        let reviewer_name = row["reviewer_session"]
            .as_str()
            .ok_or_else(|| eyre!("missing reviewer"))?;
        let reviewer = SessionKey(identity(reviewer_name).to_owned());
        ensure!(
            row["maker_session"].as_str().map(identity) == Some(self.caller.0.as_str())
                && handoff["maker_session"] == row["maker_session"]
                && reviewer != self.caller,
            "publication must come from the assigned maker"
        );
        let stage = PeerCheckpointHandoff::capture(&self.root, &self.profile, &reviewer, workspace)
            .ok_or_else(|| eyre!("reviewer must be attached in the shared workspace"))?;
        ensure!(stage.master == *master, "different reviewer originator");
        ensure!(
            scheduler.pptx_review_route_admitted(&review_grant(
                page,
                &binding,
                &self.stage,
                &stage
            ))?,
            "coordinator has not admitted this maker and reviewer for the frozen page"
        );
        let hash = current_head(workspace, &row["candidate"])?
            .ok_or_else(|| eyre!("missing candidate"))?;
        let id = publication["candidate_id"]
            .as_str()
            .ok_or_else(|| eyre!("missing candidate id"))?;
        let published = &publication["candidate"];
        ensure!(
            published["path"] == row["candidate"]
                && handoff["candidate"] == *published
                && handoff["candidate_id"] == id
                && published["sha256"].is_string(),
            "unbound candidate"
        );
        if published["sha256"] != hash {
            return Ok(json!({"status":"superseded","peer":reviewer_name}));
        }
        ensure!(
            id == revision_id("candidate", &hash),
            "unbound candidate id"
        );
        let review_bytes = head_bytes(workspace, &row["review"])?;
        let review = review_bytes.as_ref().map(|bytes| digest(bytes));
        if let (Some(bytes), Some(previous)) = (review_bytes, &review) {
            let verdict: Value = serde_json::from_slice(&bytes)?;
            let archive = json!(format!(".octos/page-verdicts/{previous}.json"));
            ensure!(
                verdict["kind"] == "page-review-v1"
                    && verdict["page_id"] == page
                    && verdict["reviewer_session"].as_str().map(identity)
                        == Some(reviewer.0.as_str())
                    && current_head(workspace, &archive)?.as_ref() == Some(previous),
                "unbound review head"
            );
            if verdict["candidate_sha256"] == hash {
                return Ok(json!({"status":"already_reviewed","peer":reviewer_name}));
            }
        }
        let pair=review_grant(page,&binding,&self.stage,&stage);
        let phase=super::cover_workflow::advance(&self.root,&pair,page,"candidate",id,"published")?;
        ensure!(phase=="review","unexpected candidate continuation: {phase}");
        let message = format!(
            "Apply the PPTX skill, shared brief, page instructions and frozen rubric from the bound plan. Use page_gate(only_page) and verify the candidate_id. Inspect this exact editable PPTX and its render. For reconstruction, compare the render directly with the original reference. Record material discrepancies with their location, expected result and actual result, including items absent from the production checklist. On repair, verify the previous findings and check for regressions. Sign using the review_request_template. Return if the candidate has been superseded.\n\nPage assignment\n{}",
            json!({"page":page,"phase":"review","candidate_id":id,"plan":binding})
        );
        let job = Job {
            page: page.into(),
            phase,
            peer: reviewer_name.into(),
            candidate_sha256: Some(hash),
            review_sha256: review,
            message,
            resume_after_round: None,
        };
        let queue = AssignmentQueue {
            scheduler,
            workspace,
            root: &self.root,
            profile: &self.profile,
            master,
            binding: &binding,
            plan: &plan,
            route: &route,
        };
        let status = queue.deliver_job(row, &job, Some(&stage))?;
        let notice =
            format!("Page {page} candidate {id} is queued for its assigned independent reviewer.");
        let supervisor =
            self.stage
                .publish(scheduler, &self.root, &self.caller, &self.turn, &notice, id);
        Ok(json!({"status":status,"peer":reviewer_name,"supervisor_notified":supervisor.is_ok()}))
    }

    fn dispatch_repair_event(
        &self,
        scheduler: &InProcessAgentOrchestrator,
        action: &str,
        event: &Value,
    ) -> Result<Value> {
        self.check_caller()?;
        let workspace = &self.stage.workspace;
        let master = &self.stage.master;
        let (plan, binding, route) = frozen_plan(workspace, master)?;
        ensure!(
            route["repair_gate"] == "verified-findings-v1",
            "verified repair gate required"
        );
        let queue = AssignmentQueue {
            scheduler,
            workspace,
            root: &self.root,
            profile: &self.profile,
            master,
            binding: &binding,
            plan: &plan,
            route: &route,
        };
        let page = event["page"]
            .as_str()
            .ok_or_else(|| eyre!("missing page id"))?;
        let row = plan["pages"]
            .as_array()
            .and_then(|rows| rows.iter().find(|r| r["id"] == page))
            .ok_or_else(|| eyre!("page outside frozen roster"))?;
        let maker_name = row["maker_session"]
            .as_str()
            .ok_or_else(|| eyre!("missing maker"))?;
        let reviewer_name = row["reviewer_session"]
            .as_str()
            .ok_or_else(|| eyre!("missing reviewer"))?;
        let maker = queue
            .stage_for(maker_name)
            .ok_or_else(|| eyre!("maker no longer attached"))?;
        let reviewer = queue
            .stage_for(reviewer_name)
            .ok_or_else(|| eyre!("reviewer no longer attached"))?;
        let pair = review_grant(page, &binding, &maker, &reviewer);
        ensure!(
            maker != reviewer && scheduler.pptx_review_route_admitted(&pair)?,
            "page pair has no coordinator assignment"
        );
        if action == "page_review_submit" {
            ensure!(
                self.stage == reviewer,
                "only the assigned reviewer can route its rejection"
            );
        }
        let phase = event["phase"]
            .as_str()
            .ok_or_else(|| eyre!("missing repair phase"))?;
        let arbiter = if phase == "arbitrate"
            || !event["decision"].is_null()
            || action == "page_repair_decide"
        {
            let name = queue
                .confirmation_peer(row)
                .ok_or_else(|| eyre!("no assigned confirmation peer"))?;
            let stage = queue
                .stage_for(name)
                .ok_or_else(|| eyre!("confirmation peer no longer attached"))?;
            ensure!(
                scheduler.pptx_review_route_admitted(&queue.confirmation_grant(&pair, &stage))?,
                "confirmation peer has no coordinator assignment for this route"
            );
            if action == "page_repair_decide" {
                ensure!(
                    self.stage == stage,
                    "only the assigned confirmation peer can route its decision"
                );
            }
            Some((name, stage))
        } else {
            None
        };
        let candidate = current_head(workspace, &row["candidate"])?
            .ok_or_else(|| eyre!("missing candidate"))?;
        let bytes =
            head_bytes(workspace, &row["review"])?.ok_or_else(|| eyre!("missing verdict"))?;
        let review = digest(&bytes);
        ensure!(
            event["candidate"]["path"] == row["candidate"]
                && event["review"]["path"] == row["review"],
            "publication paths differ from frozen page"
        );
        let superseded = || json!({"status":"superseded","phase":phase});
        if event["candidate"]["sha256"] != candidate || event["review"]["sha256"] != review {
            return Ok(superseded());
        }
        let verdict: Value = serde_json::from_slice(&bytes)?;
        let id = revision_id("verdict", &review);
        ensure!(
            event["candidate_id"] == revision_id("candidate", &candidate)
                && event["verdict_id"] == id
                && verdict["kind"] == "page-review-v1"
                && verdict["page_id"] == page
                && verdict["repair_gate"] == route["repair_gate"]
                && verdict["accepted"] == false
                && verdict["disposition"].is_null()
                && verdict["candidate"] == event["candidate"]
                && verdict["reviewer_session"].as_str().map(identity)
                    == Some(identity(reviewer_name))
                && current_head(
                    workspace,
                    &json!(format!(".octos/page-verdicts/{review}.json"))
                )?
                .as_ref()
                    == Some(&review),
            "repair event differs from the archived verdict"
        );
        let decision_path = json!(format!(".octos/page-repair-decisions/{review}.json"));
        let recorded = head_bytes(workspace, &decision_path)?;
        let decision = if let Some(bytes) = recorded {
            if event["decision"].is_null() {
                return Ok(superseded());
            }
            let expected = json!({"path":decision_path,"sha256":digest(&bytes)});
            let data: Value = serde_json::from_slice(&bytes)?;
            ensure!(
                event["decision"] == expected
                    && data["page"] == page
                    && data["candidate_id"] == event["candidate_id"]
                    && data["verdict_id"] == id
                    && data["peer"].as_str().map(identity)
                        == arbiter.as_ref().map(|(name, _)| identity(name)),
                "confirmation does not match this verdict and peer"
            );
            Some(data)
        } else {
            ensure!(
                event["decision"].is_null() && action == "page_review_submit",
                "missing independent decision"
            );
            None
        };
        let mut findings = ["critical_defects", "open_required_findings"]
            .iter()
            .filter_map(|key| verdict[*key].as_array())
            .flatten()
            .peekable();
        ensure!(
            findings.peek().is_some(),
            "rejection has no required findings"
        );
        let manual = findings.any(|finding| finding["kind"] != "native_qa");
        let decision = decision.as_ref().and_then(|d| d["decision"].as_str());
        let (kind,outcome,receipt_id) = if action == "page_repair_decide" {
            ("decision",decision.ok_or_else(||eyre!("missing decision"))?,event["decision"]["sha256"].as_str().ok_or_else(||eyre!("missing decision hash"))?)
        } else {
            ("verdict", if manual {"manual"} else {"native"}, id.as_str())
        };
        let required_phase=match (manual,decision) {
            (false,None) | (true,Some("confirm")) => "repair",
            (true,None) => "arbitrate",
            (true,Some("dispute")) => "review",
            _ => eyre::bail!("invalid verified decision"),
        };
        ensure!(phase==required_phase,"native disposition differs from signed evidence");
        let cid=revision_id("candidate",&candidate);
        let intent=super::cover_workflow::advance_bound(&self.root,&pair,page,kind,receipt_id,outcome,Some((&cid,Some(&id))))?;
        ensure!(intent==phase,"native gate and cover workflow disagree: {phase} vs {intent}");
        let phase=intent.as_str();
        let (name, target, instruction) = match (phase, manual, decision) {
            ("repair", false, None) | ("repair", true, Some("confirm")) => (
                maker_name,
                &maker,
                "Repair all authorized findings for this page together, then publish the new candidate.",
            ),
            ("arbitrate", true, None) => {
                let (name, stage) = arbiter
                    .as_ref()
                    .ok_or_else(|| eyre!("missing confirmation peer"))?;
                (
                    *name,
                    stage,
                    "Independently inspect the current candidate and every manual finding. Submit page_repair_decide with both revision IDs and observations for every manual finding.",
                )
            }
            ("review", true, Some("dispute")) => (
                reviewer_name,
                &reviewer,
                "Read the independent decision and revise the signed review on the evidence.",
            ),
            _ => eyre::bail!("requested phase differs from the signed findings and decision"),
        };
        let data = json!({"page":page,"phase":phase,"candidate_id":event["candidate_id"],
            "verdict_id":id,"decision":event["decision"],"plan":binding});
        let job = Job {
            page: page.into(),
            phase: phase.into(),
            peer: name.into(),
            candidate_sha256: Some(candidate),
            review_sha256: Some(review),
            resume_after_round: None,
            message: format!(
                "Check page_gate(only_page) and both revision IDs before working. Finish if superseded. {instruction} Follow the host dispatch receipt.\n\nPage assignment\n{data}"
            ),
        };
        let status = queue.deliver_job(row, &job, Some(target))?;
        let notice = format!("Page {page}, verdict {id}: {phase} assignment recorded for {name}.");
        let notified = self
            .stage
            .publish(
                scheduler,
                &self.root,
                &self.caller,
                &self.turn,
                &notice,
                &id,
            )
            .is_ok();
        Ok(json!({"status":status,"phase":phase,"peer":name,"supervisor_notified":notified}))
    }

    fn accept_cover(&self,scheduler:&InProcessAgentOrchestrator,result:&Value)->Result<Value>{
        self.check_caller()?;
        let workspace=&self.stage.workspace;let master=&self.stage.master;
        let (plan,binding,route)=frozen_plan(workspace,master)?;
        let queue=AssignmentQueue{scheduler,workspace,root:&self.root,profile:&self.profile,master,binding:&binding,plan:&plan,route:&route};
        let page=result["page"].as_str().ok_or_else(||eyre!("missing page"))?;
        let row=plan["pages"].as_array().and_then(|v|v.iter().find(|r|r["id"]==page)).ok_or_else(||eyre!("missing roster row"))?;
        let reviewer=queue.stage_for(row["reviewer_session"].as_str().ok_or_else(||eyre!("missing reviewer"))?).ok_or_else(||eyre!("missing reviewer stage"))?;
        ensure!(reviewer==self.stage,"wrong reviewer");
        let maker=queue.stage_for(row["maker_session"].as_str().ok_or_else(||eyre!("missing maker"))?).ok_or_else(||eyre!("missing maker stage"))?;
        let pair=review_grant(page,&binding,&maker,&reviewer);
        ensure!(maker!=reviewer && scheduler.pptx_review_route_admitted(&pair)?,"unarmed page");
        let bytes=head_bytes(workspace,&row["review"])?.ok_or_else(||eyre!("missing review"))?;
        let hash=digest(&bytes); let verdict:Value=serde_json::from_slice(&bytes)?;
        let candidate=current_head(workspace,&row["candidate"])?.ok_or_else(||eyre!("missing candidate"))?;
        let id=revision_id("verdict",&hash);
        ensure!(result["verdict_id"]==id && result["candidate_id"]==revision_id("candidate",&candidate)
            && verdict["accepted"]==true && verdict["page_id"]==page && verdict["candidate_sha256"]==candidate
            && verdict["reviewer_session"].as_str().map(identity)==Some(self.caller.0.as_str())
            && current_head(workspace,&json!(format!(".octos/page-verdicts/{hash}.json")))?.as_ref()==Some(&hash),"stale or unbound verdict");
        let cid=revision_id("candidate",&candidate);
        let next=super::cover_workflow::advance_bound(&self.root,&pair,page,"verdict",&id,"approved",Some((&cid,None)))?;
        ensure!(next=="complete","workflow expected {next}");
        let note=format!("OctoScript page workflow complete for {page}; independent native verdict {id}. Assemble and verify the requested delivery.");
        let notice=self.stage.publish(scheduler,&self.root,&self.caller,&self.turn,&note,&id);
        Ok(json!({"status":"complete","supervisor_notified":notice.is_ok()}))
    }

    pub(super) fn observe(
        &self,
        scheduler: &InProcessAgentOrchestrator,
        owner: &str,
        request: &Value,
        result: &mut Value,
    ) -> bool {
        if identity(owner) != self.caller.0 {
            return false;
        }
        if request["action"] == "operations" {
            let Some(steps) = request["steps"].as_array() else {
                return false;
            };
            let completed = result["completed_steps"].as_u64().unwrap_or(0) as usize;
            let Some(entries) = result["results"].as_array_mut() else {
                return false;
            };
            let mut changed = false;
            for (i, (step, entry)) in steps.iter().zip(entries).take(completed).enumerate() {
                if entry["step"] == i && entry["action"] == step["action"] {
                    changed |= self.observe_one(scheduler, step, &mut entry["result"]);
                }
            }
            return changed;
        }
        self.observe_one(scheduler, request, result)
    }

    fn observe_one(
        &self,
        scheduler: &InProcessAgentOrchestrator,
        request: &Value,
        result: &mut Value,
    ) -> bool {
        if request["action"]=="page_review_submit" && result["accepted"]==true && result["verdict_id"].is_string() && result["candidate_id"].is_string() {
            let receipt=self.accept_cover(scheduler,result).unwrap_or_else(|e|json!({"status":"coordinator_required","reason":e.to_string()}));
            result["cover_workflow"]=receipt;
            return true;
        }
        if let Some(action @ ("page_review_submit" | "page_repair_decide")) =
            request["action"].as_str()
        {
            if result["repair_event"]["kind"] != "page_repair_transition" {
                return false;
            }
            let start = std::time::Instant::now();
            let observed = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            let mut receipt = self
                .dispatch_repair_event(scheduler, action, &result["repair_event"])
                .unwrap_or_else(
                    |error| json!({"status":"coordinator_required","reason":error.to_string()}),
                );
            receipt["observed_at_unix_ms"] = json!(observed);
            receipt["dispatch_duration_ms"] = json!(start.elapsed().as_millis() as u64);
            result["notification_sent"] = json!(matches!(
                receipt["status"].as_str(),
                Some("queued" | "already_dispatched")
            ));
            result["next"] = json!(match receipt["status"].as_str() {
                Some("superseded") =>
                    "A newer publication is current. Finish this obsolete assignment.",
                Some("queued" | "already_dispatched") if receipt["supervisor_notified"] == true =>
                    "Delivery recorded. Finish this assignment.",
                _ =>
                    "Relay the verdict_id and receipt to the coordinator with peer_publish. It can reconcile the native page batch.",
            });
            result["repair_dispatch"] = receipt;
            return true;
        }
        let cycle = request["action"] == "cycle" && request["publish_candidate"] == true;
        if !cycle && request["action"] != "page_publish" {
            return false;
        }
        let publication = if cycle {
            &mut result["page_publication"]
        } else {
            &mut *result
        };
        if publication["ready_for_independent_review"] != true
            || publication["handoff"]["kind"] != "page_candidate_ready"
            || !publication["handoff"]["review_request_template"].is_object()
        {
            return false;
        }
        let receipt = self.enqueue(scheduler, publication).unwrap_or_else(
            |err| json!({"status":"coordinator_required","reason":err.to_string()}),
        );
        let next = if receipt["status"] == "superseded" {
            "A newer candidate is current. Finish this obsolete assignment without relaying it."
        } else if receipt["status"] == "coordinator_required" {
            "Relay the bound candidate_id with peer_publish so the supervisor can reconcile this page."
        } else if receipt["supervisor_notified"] == false {
            "Reviewer delivery is recorded. Relay only the supervisor checkpoint with peer_publish(handoff_id=candidate_id), then finish."
        } else {
            "The host handled this revision. Finish this maker assignment."
        };
        let delivered = matches!(
            receipt["status"].as_str(),
            Some("queued" | "already_dispatched")
        );
        publication["review_dispatch"] = receipt;
        publication["notification_sent"] = json!(delivered);
        publication["next"] = json!(next);
        if cycle && result["peer_review_handoff"].is_object() {
            result["peer_review_handoff"]["notification_sent"] = json!(delivered);
            result["peer_review_handoff"]["next"] = json!(next);
        }
        true
    }
}
