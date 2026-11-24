//! Decide whether a signed page rejection authorizes changing the frozen candidate.
use crate::{Context, page_gate, page_verdict, review_routes, string};
use anyhow::{Context as _, Result, bail, ensure};
use serde_json::{Value, json};
use std::collections::BTreeSet;

pub(crate) const VERSION: &str = "verified-findings-v1";

pub(crate) fn guarded(ctx: &Context) -> Result<bool> {
    Ok(review_routes::load_plan(ctx)?["repair_gate"] == VERSION)
}

fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    let s = string(value, key)?;
    ensure!(!s.trim().is_empty(), "{key} must contain text");
    Ok(s)
}

pub(crate) fn validate_rules(plan: &Value) -> Result<()> {
    if let Some(rules) = plan.get("repair_requirements") {
        let rules = rules
            .as_object()
            .context("repair_requirements must map IDs to frozen requirements")?;
        ensure!(
            rules.iter().all(|(id, rule)| !id.trim().is_empty()
                && rule.as_str().is_some_and(|s| !s.trim().is_empty())),
            "each repair requirement needs an ID and a description"
        );
    }
    Ok(())
}

pub(crate) fn findings(review: &Value) -> impl Iterator<Item = &Value> {
    ["critical_defects", "open_required_findings"]
        .into_iter()
        .filter_map(|key| review[key].as_array())
        .flatten()
}

fn manual_ids(review: &Value) -> impl Iterator<Item = &str> {
    findings(review)
        .filter(|f| f["kind"] != "native_qa")
        .filter_map(|f| f["id"].as_str())
}

fn native_failure(report: &Value, candidate: &Value, id: &str) -> bool {
    report["algorithm"] == crate::qa::ALGORITHM
        && candidate["artifact"]["sha256"].is_string()
        && report["input"]["sha256"] == candidate["artifact"]["sha256"]
        && report["findings"].as_array().is_some_and(|rows| {
            rows.iter().any(|r| {
                r["id"] == id
                    && (r["page"] == 1 || r["page"] == candidate["page_id"])
                    && r["status"] == "fail"
            })
        })
}

pub(crate) fn validate(
    ctx: &Context,
    plan: &Value,
    candidate: &Value,
    review: &Value,
) -> Result<()> {
    if review["accepted"] != false || page_verdict::disposition(review).is_some() {
        return Ok(());
    }
    let mut ids = BTreeSet::new();
    let mut native_reports = None;
    let mut dimensions = None;
    for f in findings(review) {
        ensure!(
            f.is_object(),
            "required repair findings must be objects with id and kind (native_qa, visible or requirement); record measurements alone as notes"
        );
        let id = text(f, "id")?;
        ensure!(ids.insert(id), "duplicate required finding {id}");
        match text(f, "kind")? {
            "native_qa" => {
                if native_reports.is_none() {
                    let cycle = ctx.json(ctx.verify(&candidate["build_receipt"])?)?;
                    let original = ctx.json(ctx.verify(&cycle["native_qa"]["report"])?)?;
                    ensure!(
                        cycle["pages"] == json!([candidate["page_id"]])
                            && cycle["editable"]["sha256"] == candidate["artifact"]["sha256"],
                        "native QA must measure the current published PPTX"
                    );
                    let measured = crate::qa::evaluate(
                        ctx,
                        string(&candidate["build_receipt"], "path")?,
                        &Value::Null,
                    )?;
                    native_reports = Some((original, measured));
                }
                let (original, measured) = native_reports.as_ref().unwrap();
                ensure!(
                    native_failure(original, candidate, id)
                        && native_failure(measured, candidate, id),
                    "{id} must be a failed native check for this exact page and PPTX"
                );
            }
            "visible" => {
                text(f, "impact")?;
                if dimensions.is_none() {
                    dimensions = Some(image::image_dimensions(ctx.verify(&candidate["render"])?)?);
                }
                let (w, h) = dimensions.unwrap();
                let rect = f["box"]
                    .as_array()
                    .context("visible finding needs box [x,y,width,height] in rendered pixels")?;
                let nums = rect
                    .iter()
                    .map(Value::as_u64)
                    .collect::<Option<Vec<_>>>()
                    .context("box coordinates must be nonnegative integers")?;
                ensure!(
                    matches!(nums.as_slice(), [x,y,a,b] if *a>0 && *b>0 && x.checked_add(*a).is_some_and(|n| n<=u64::from(w)) && y.checked_add(*b).is_some_and(|n| n<=u64::from(h))),
                    "finding box must fit the current render"
                );
            }
            "requirement" => {
                text(f, "impact")?;
                let rule = text(f, "rule")?;
                ensure!(
                    plan["repair_requirements"].get(rule).is_some(),
                    "requirement {rule} was not frozen before production"
                );
            }
            _ => bail!("unknown finding kind; use native_qa, visible or requirement"),
        }
    }
    Ok(())
}

pub(crate) fn receipt_path(binding: &Value) -> Result<String> {
    Ok(format!(
        ".octos/page-repair-decisions/{}.json",
        string(binding, "sha256")?
    ))
}

pub(crate) fn confirmation_peer<'a>(
    plan: &'a Value,
    row: &Value,
    route: &'a Value,
) -> Option<&'a str> {
    if let Some(peer) = route["arbiter_agent"].as_str() {
        return Some(peer);
    }
    let pages = plan["pages"].as_array()?;
    let index = pages.iter().position(|p| p["id"] == row["id"])?;
    (pages.len() > 1)
        .then(|| pages[(index + 1) % pages.len()]["reviewer_session"].as_str())
        .flatten()
}

fn check_decision(
    plan: &Value,
    row: &Value,
    route: &Value,
    review: &Value,
    binding: &Value,
    decision: &Value,
) -> Result<()> {
    let who = text(decision, "peer")?;
    ensure!(
        !page_gate::identity(who).trim().is_empty(),
        "host peer identity is empty"
    );
    page_gate::independent(plan, &Value::Null, who)?;
    ensure!(
        page_gate::identity(who) != page_gate::identity(string(row, "reviewer_session")?),
        "a reviewer cannot corroborate their own rejection"
    );
    if let Some(peer) = confirmation_peer(plan, row, route) {
        ensure!(
            page_gate::identity(who) == page_gate::identity(peer),
            "use the independent peer assigned to this referral"
        );
    }
    ensure!(
        decision["verdict_id"] == page_verdict::token("verdict", binding)?
            && decision["candidate_id"] == page_verdict::token("candidate", &review["candidate"])?
            && decision["page"] == row["id"],
        "stale candidate or verdict in repair decision"
    );
    ensure!(
        matches!(decision["decision"].as_str(), Some("confirm" | "dispute")),
        "decision must be confirm or dispute"
    );
    let notes = decision["finding_observations"]
        .as_object()
        .context("supply finding_observations keyed by the required manual finding IDs")?;
    let ids: BTreeSet<_> = manual_ids(review).collect();
    ensure!(
        !ids.is_empty()
            && notes.len() == ids.len()
            && ids.iter().all(|id| notes
                .get(*id)
                .and_then(Value::as_str)
                .is_some_and(|s| !s.trim().is_empty())),
        "record one independent observation for every manual finding in this verdict"
    );
    Ok(())
}

pub(crate) fn phase(
    ctx: &Context,
    plan: &Value,
    row: &Value,
    review: &Value,
    binding: &Value,
) -> Result<&'static str> {
    if review["repair_gate"] != VERSION {
        return Ok("review");
    }
    if manual_ids(review).next().is_none() {
        return Ok(if findings(review).next().is_some() {
            "repair"
        } else {
            "review"
        });
    }
    let path = receipt_path(binding)?;
    if !ctx.path(&path)?.try_exists()? {
        return Ok("arbitrate");
    }
    let decision = ctx.json(path)?;
    check_decision(
        plan,
        row,
        &review_routes::load_plan(ctx)?,
        review,
        binding,
        &decision,
    )?;
    Ok(if decision["decision"] == "confirm" {
        "repair"
    } else {
        "review"
    })
}

/// Emit the authorized next step while the page lock still pins both heads.
pub(crate) fn followup(
    ctx: &Context,
    plan: &Value,
    row: &Value,
    review: &Value,
    binding: &Value,
) -> Result<Option<Value>> {
    if review["repair_gate"] != VERSION
        || review["accepted"] != false
        || page_verdict::disposition(review).is_some()
    {
        return Ok(None);
    }
    let stage = phase(ctx, plan, row, review, binding)?;
    let path = receipt_path(binding)?;
    let decision = if ctx.path(&path)?.try_exists()? {
        Some(ctx.binding(path)?)
    } else {
        None
    };
    if stage == "review" && decision.is_none() {
        return Ok(None);
    }
    Ok(Some(
        json!({"kind":"page_repair_transition","page":row["id"],"phase":stage,
        "candidate":review["candidate"],"review":binding,
        "candidate_id":page_verdict::token("candidate",&review["candidate"])?,
        "verdict_id":page_verdict::token("verdict",binding)?, "decision":decision}),
    ))
}

/// A revised score or a new optimistic-concurrency token is not new evidence.
pub(crate) fn validate_revision(
    ctx: &Context,
    binding: &Value,
    prior: &Value,
    revised: &Value,
) -> Result<()> {
    if prior["repair_gate"] == VERSION
        && revised["repair_gate"] == VERSION
        && page_verdict::disposition(revised).is_none()
        && [
            "candidate",
            "evidence",
            "critical_defects",
            "open_required_findings",
        ]
        .iter()
        .all(|key| prior[*key] == revised[*key])
    {
        let receipt = receipt_path(binding)?;
        ensure!(
            !ctx.path(&receipt)?.try_exists()? || ctx.json(receipt)?["decision"] != "dispute",
            "these findings were disputed on the same evidence; revise the finding, supply new evidence, or resolve the review"
        );
    }
    Ok(())
}

pub(crate) fn decide(ctx: &Context, request: &Value) -> Result<Value> {
    ensure!(guarded(ctx)?, "no verified repair gate on this workflow");
    let plan = page_gate::plan(ctx)?;
    let id = text(request, "page")?;
    let row = plan["pages"]
        .as_array()
        .context("missing roster")?
        .iter()
        .find(|r| r["id"] == id)
        .context("unknown page")?;
    let _lock = page_verdict::lock_page(ctx, id)?;
    let (candidate, head) = page_gate::load_candidate(ctx, row)?;
    page_gate::validate_reviewed_package(ctx, row, &candidate)?;
    let (review, binding) = page_gate::current_review(ctx, &plan, row, &head)?;
    ensure!(
        review["repair_gate"] == VERSION
            && review["accepted"] == false
            && page_verdict::disposition(&review).is_none(),
        "no actionable rejection"
    );
    let peer = ctx
        .continuation_owner
        .as_deref()
        .context("requires host peer identity")?;
    let decision = json!({"page":id,"peer":peer,"candidate_id":request["candidate_id"],"verdict_id":request["verdict_id"],"decision":request["decision"],"finding_observations":request["finding_observations"]});
    check_decision(
        &plan,
        row,
        &review_routes::load_plan(ctx)?,
        &review,
        &binding,
        &decision,
    )?;
    let path = receipt_path(&binding)?;
    if ctx.path(&path)?.try_exists()? {
        ensure!(
            ctx.json(&path)? == decision,
            "decision already recorded; refer the current verdict back to its reviewer"
        );
    } else {
        ctx.write_json(&path, &decision)?;
    }
    let event = followup(ctx, &plan, row, &review, &binding)?
        .context("missing repair decision transition")?;
    Ok(
        json!({"receipt":event["decision"],"page":id,"verdict_id":event["verdict_id"],
        "work_stage":event["phase"],"repair_event":event,"quality_approved":false}),
    )
}
