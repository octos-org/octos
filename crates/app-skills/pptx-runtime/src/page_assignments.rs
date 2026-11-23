//! Prepare page-scoped messages from one frozen roster. The host owns delivery.
use crate::{Context, fs, page_gate, review_routes, string};
use anyhow::{Context as _, Result, ensure};
use serde_json::{Value, json};

const BATCH: &str = ".octos/pptx-peer-batch.json";
const MAKER: &str = include_str!("../references/assignment-maker.md");
const REVIEWER: &str = include_str!("../references/assignment-reviewer.md");

/// Only the current template-dispatched plan owns the pending review window.
pub(crate) fn uses_frozen_roster(ctx: &Context) -> Result<bool> {
    if !ctx.path(BATCH)?.try_exists()? {
        return Ok(false);
    }
    Ok(ctx.json(BATCH)?["plan"] == ctx.binding(page_gate::PLAN)?)
}

/// Keep an assigned maker's editable draft separate from its published review source,
/// including when it builds first and publishes after native post-build operations.
pub(crate) fn should_snapshot_build(
    ctx: &Context,
    id: &str,
    spec: &std::path::Path,
    publishing: bool,
) -> Result<bool> {
    let Some(owner) = ctx.continuation_owner.as_deref() else {
        return Ok(false);
    };
    if !uses_frozen_roster(ctx)? && !crate::repair_gate::guarded(ctx)? {
        return Ok(false);
    }
    let plan = page_gate::plan(ctx)?;
    let Some(row) = plan["pages"].as_array().and_then(|rows| rows.iter().find(|row| row["id"] == id)) else {
        return Ok(false);
    };
    if row["maker_session"].as_str().map(page_gate::identity) != Some(page_gate::identity(owner)) {
        return Ok(false);
    }
    let planned_spec = match row["spec"].as_str() {
        Some(declared) => ctx.path(declared)? == spec,
        None => true,
    };
    Ok(publishing || planned_spec)
}

fn head(ctx: &Context, row: &Value, key: &str) -> Result<Option<String>> {
    let path = ctx.path(string(row, key)?)?;
    if path.try_exists()? {
        Ok(Some(fs::sha_file(&path)?))
    } else {
        Ok(None)
    }
}

pub(crate) fn prepare(ctx: &Context, request: &Value) -> Result<Value> {
    ensure!(
        request.as_object().is_some_and(|r| r
            .keys()
            .all(|k| matches!(k.as_str(), "action" | "only_page" | "resume_after_round"))),
        "page_assignments accepts only only_page and resume_after_round"
    );
    ensure!(
        review_routes::per_page(ctx)?,
        "freeze a per_page roster with evidence_init first"
    );
    let _lock = ctx.lock_file(".octos/pptx-peer-batch.lock")?;
    let plan = page_gate::plan(ctx)?;
    let coordinator = string(&plan, "coordinator_session")?;
    ensure!(
        ctx.continuation_owner.as_deref().map(page_gate::identity)
            == Some(page_gate::identity(coordinator)),
        "only the frozen coordinator can prepare assignments"
    );
    let only = request
        .get("only_page")
        .map(|v| v.as_str().context("only_page must name a page"))
        .transpose()?;
    let resume = request
        .get("resume_after_round")
        .map(|v| {
            v.as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .filter(|n| *n > 0)
                .context("resume_after_round must be a positive peer round")
        })
        .transpose()?;
    ensure!(
        resume.is_none() || only.is_some(),
        "resume_after_round requires only_page"
    );
    let canvas = crate::page_canvas::stored(ctx)?
        .map(serde_json::to_value)
        .transpose()?;
    let roster = plan["pages"].as_array().context("missing pages")?;
    ensure!(
        only.is_none_or(|id| roster.iter().any(|row| row["id"] == id)),
        "unknown page"
    );
    let mut jobs = Vec::new();
    let mut specialist = Vec::new();
    for row in roster
        .iter()
        .filter(|row| only.is_none_or(|id| row["id"] == id))
    {
        let id = string(row, "id")?;
        let candidate = head(ctx, row, "candidate")?;
        let review = head(ctx, row, "review")?;
        let state = page_gate::page_state(ctx, &plan, row);
        ensure!(
            candidate == head(ctx, row, "candidate")? && review == head(ctx, row, "review")?,
            "page {id} changed during preparation; retry"
        );
        let stage = string(&state, "work_stage")?;
        let phases: &[&str] = match stage {
            "make" => &["make"],
            "review" => &["review"],
            "repair" => &["repair"],
            "arbitrate" if state["confirmation_peer"].is_string() => &["arbitrate"],
            "complete" => &[],
            _ => {
                let mut referral = json!({"page":id,"stage":stage});
                if let Some(peer) = state.get("confirmation_peer") {
                    referral["peer"] = peer.clone();
                    referral["request"] = json!({"action":"page_repair_decide","page":id,"candidate_id":state["candidate_id"],"verdict_id":state["verdict_id"]});
                    referral["review_path"] = row["review"].clone();
                    referral["required"] = json!(["decision","finding_observations"]);
                }
                specialist.push(referral);
                continue;
            }
        };
        for phase in phases {
            let maker = matches!(*phase, "make" | "repair");
            let peer = match *phase {
                "arbitrate" => string(&state, "confirmation_peer")?,
                _ if maker => string(row, "maker_session")?,
                _ => string(row, "reviewer_session")?,
            };
            let mut data = json!({"page":id, "phase":phase, "candidate_path":row["candidate"], "review_path":row["review"],
                "min_score":plan["min_score"].as_f64().unwrap_or(95.0), "rubric":plan["rubric"]});
            for key in [
                "spec",
                "reference",
                "inputs",
                "instructions",
                "approved_background",
                "source_artwork",
                "editable_copy",
                "source_copy_review",
                "source_typography",
            ] {
                if let Some(v) = row.get(key) {
                    data[key] = v.clone();
                }
            }
            if let Some(value) = &canvas {
                data["canvas"] = value.clone();
            }
            if let Some(v) = plan.get("shared_brief") {
                data["shared_brief"] = v.clone();
            }
            if let Some(v) = state.get("candidate_id") {
                data["candidate_id"] = v.clone();
            }
            if let Some(v) = state.get("verdict_id").or(state.get("replaces_verdict_id")) {
                data["verdict_id"] = v.clone();
            }
            if let Some(receipt) = state.get("repair_decision") {
                data["repair_decision"] = receipt.clone();
            }
            if let Some(round) = resume {
                data["resume_after_round"] = json!(round);
            }
            let message = format!(
                "{}\n\nPage assignment\n{}",
                match *phase {
                    "arbitrate" => "Independently inspect every manual finding against the current candidate. Check page_gate(only_page) and sign page_repair_decide with both revision IDs and a finding_observations entry per manual finding. Follow the host dispatch receipt.",
                    _ if maker => MAKER.trim(),
                    _ => REVIEWER.trim(),
                },
                serde_json::to_string(&data)?
            );
            ensure!(
                message.len() <= 64 * 1024,
                "brief for {id} exceeds the peer input limit; reference shared material by path"
            );
            let mut job = json!({"page":id,"phase":phase,"peer":peer,"candidate_sha256":candidate,"review_sha256":review,"message":message});
            if let Some(round) = resume {
                job["resume_after_round"] = json!(round);
            }
            jobs.push(job);
        }
    }
    let plan_binding = ctx.binding(page_gate::PLAN)?;
    ensure!(
        review_routes::load_plan(ctx)?["page_plan"] == plan_binding,
        "frozen page plan changed"
    );
    // A host predating terminal-round receipts must reject recovery, not silently dedupe it as a fresh make.
    let kind = if resume.is_some() {
        "pptx-peer-resume-v1"
    } else {
        "pptx-peer-batch-v1"
    };
    let batch = json!({"kind":kind,"plan":plan_binding,"coordinator":coordinator,"jobs":jobs});
    let bytes = serde_json::to_vec_pretty(&batch)?;
    ensure!(
        bytes.len() <= 4 * 1024 * 1024,
        "batch exceeds 4 MiB; use only_page"
    );
    ctx.replace_bytes(BATCH, &bytes)?;
    let mut receipt = json!({"peer_send_input":{"pptx_batch":fs::digest(&bytes)},"assignments":jobs.len(),"specialist_pages":specialist,"dispatched":false});
    if let Some(id) = only {
        receipt["only_page"] = json!(id);
        receipt["before_wait"] = json!("Reconcile with page_assignments without only_page and dispatch its batch before waiting. This receipt covers one page.");
    }
    Ok(receipt)
}
