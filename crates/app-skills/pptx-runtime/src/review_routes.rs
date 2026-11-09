//! A frozen, independent visual verification route for explicitly unsupported
//! measurements. Native failures remain authoritative and are never overwritten.
use crate::{Context, string};
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::collections::BTreeSet;
pub const PLAN_PATH: &str = ".octos/pptx-verification-plan.json";
pub fn load_plan(ctx: &Context) -> Result<Value> {
    if ctx.path(PLAN_PATH)?.exists() {
        ctx.json(PLAN_PATH)
    } else {
        Ok(json!({"mode":"strict"}))
    }
}

fn same_identity(a: &str, b: &str) -> bool {
    a.strip_prefix("session:").unwrap_or(a) == b.strip_prefix("session:").unwrap_or(b)
}

pub fn plan(request: &Value, owner: &str) -> Result<Value> {
    match request["verification_mode"].as_str().unwrap_or("strict") {
        "strict" => Ok(json!({"mode":"strict","fidelity_contract":crate::fidelity::CONTRACT})),
        "independent_visual" => {
            let reviewer = string(request, "reviewer_agent")?;
            ensure!(
                !same_identity(reviewer, owner) && !reviewer.trim().is_empty(),
                "visual reviewer must be a different session from the Maker"
            );
            if let Some(arbiter) = request.get("arbiter_agent") {
                let arbiter = arbiter
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("arbiter_agent must be a session identity"))?;
                ensure!(
                    !arbiter.trim().is_empty()
                        && !same_identity(arbiter, owner)
                        && !same_identity(arbiter, reviewer),
                    "arbiter must be a different session from Maker and Reviewer"
                );
            }
            Ok(
                json!({"mode":"independent_visual","reviewer_agent":reviewer,"arbiter_agent":request["arbiter_agent"],"fidelity_contract":crate::fidelity::CONTRACT,
                "native_failures_overridable":false,"eligible_unknown_codes":["OWNER_UNVERIFIED","OWNER_BOUNDARY_UNVERIFIED","COMPOSITE_INK_UNVERIFIED","ARTWORK_CONTACT_UNVERIFIED"]}),
            )
        }
        _ => anyhow::bail!("verification_mode must be strict or independent_visual"),
    }
}
pub fn permitted(ctx: &Context) -> Result<bool> {
    if !ctx.path(".octos/continuation.json")?.exists() {
        return Ok(false);
    }
    let policy = ctx.json(".octos/continuation.json")?;
    let plan = load_plan(ctx)?;
    Ok(plan["mode"] == "independent_visual"
        && ctx.continuation_owner.as_deref().is_some_and(|id| {
            ["reviewer_agent", "arbiter_agent"]
                .iter()
                .any(|k| plan[k].as_str().is_some_and(|r| same_identity(r, id)))
                && !policy["owner_agent"]
                    .as_str()
                    .is_some_and(|o| same_identity(o, id))
        }))
}

/// The host identity is captured here, not accepted from authored observations.
/// This is a workflow provenance check, not security against arbitrary file writes.
pub fn attest(ctx: &Context, cycle: &Value, review: &mut Value) -> Result<()> {
    review.as_object_mut().unwrap().remove("attestation");
    let Some(identity) = &ctx.continuation_owner else {
        return Ok(());
    };
    let receipt = json!({"schema":"pptx-review-attestation-v1","reviewer":identity,
        "cycle":cycle,"review_sha256":crate::fs::digest(&serde_json::to_vec(review)?)});
    let key = crate::fs::digest(&serde_json::to_vec(&receipt)?);
    let path = format!(".octos/pptx-review-attestations/{key}.json");
    if ctx.path(&path)?.exists() {
        ensure!(
            ctx.json(&path)? == receipt,
            "review attestation was modified"
        );
    } else {
        ctx.write_json(&path, &receipt)?;
    }
    review["attestation"] = ctx.binding(path)?;
    Ok(())
}
pub fn authorized(ctx: &Context, policy: &Value, review: &Value, cycle: &Value) -> Result<bool> {
    let plan = load_plan(ctx)?;
    if plan["mode"] != "independent_visual" || !review["attestation"].is_object() {
        return Ok(false);
    }
    let receipt = ctx.json(ctx.verify(&review["attestation"])?)?;
    let mut body = review.clone();
    body.as_object_mut().unwrap().remove("attestation");
    ensure!(
        receipt["schema"] == "pptx-review-attestation-v1"
            && receipt["cycle"] == *cycle
            && receipt["review_sha256"] == crate::fs::digest(&serde_json::to_vec(&body)?),
        "review attestation is stale or belongs to different evidence"
    );
    let reviewer = string(&receipt, "reviewer")?;
    Ok(["reviewer_agent", "arbiter_agent"]
        .iter()
        .any(|k| plan[k].as_str().is_some_and(|r| same_identity(r, reviewer)))
        && !policy["owner_agent"]
            .as_str()
            .is_some_and(|o| same_identity(o, reviewer)))
}

pub fn arbiter_authorized(
    ctx: &Context,
    policy: &Value,
    review: &Value,
    cycle: &Value,
) -> Result<bool> {
    if !authorized(ctx, policy, review, cycle)? {
        return Ok(false);
    }
    let receipt = ctx.json(ctx.verify(&review["attestation"])?)?;
    let plan = load_plan(ctx)?;
    Ok(plan["arbiter_agent"].as_str().is_some_and(|id| {
        receipt["reviewer"]
            .as_str()
            .is_some_and(|r| same_identity(id, r))
    }))
}

pub fn eligible(f: &Value) -> bool {
    f["status"] == "unknown"
        && matches!(
            f["code"].as_str(),
            Some(
                "OWNER_UNVERIFIED"
                    | "OWNER_BOUNDARY_UNVERIFIED"
                    | "COMPOSITE_INK_UNVERIFIED"
                    | "ARTWORK_CONTACT_UNVERIFIED"
            )
        )
}
pub fn allowed_by_plan(f: &Value, plan: &Value) -> bool {
    eligible(f)
        && plan["mode"] == "independent_visual"
        && plan["eligible_unknown_codes"]
            .as_array()
            .is_some_and(|codes| codes.contains(&f["code"]))
}
pub fn candidates(qa: &Value, plan: &Value) -> Value {
    json!(qa["findings"].as_array().into_iter().flatten().filter(|f|allowed_by_plan(f, plan)).map(|f|
        json!({"id":f["id"],"page":f["page"],"object_id":f["object_id"],"code":f["code"],"observed":f["observed"],
            "decision":"unknown","basis":"","rule":"Independent reviewer must inspect original, blank and actual pixels. Native status stays unknown; this is a separate visual verification route."})).collect::<Vec<_>>())
}
pub fn adjudicated(
    qa: &Value,
    review: &Value,
    authorized: bool,
    spec: &crate::spec::Spec,
    plan: &Value,
) -> Result<BTreeSet<String>> {
    let rows = review["measurement_reviews"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    ensure!(
        authorized || rows.is_empty(),
        "measurement_reviews require the frozen independent reviewer's attestation"
    );
    let mut accepted = BTreeSet::new();
    let mut seen = BTreeSet::new();
    for row in rows {
        let id = string(&row, "id")?;
        ensure!(seen.insert(id.to_owned()), "duplicate measurement review");
        let f = qa["findings"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|f| f["id"] == id)
            .ok_or_else(|| anyhow::anyhow!("measurement review names an unknown finding"))?;
        ensure!(
            allowed_by_plan(f, plan),
            "native failures or categories outside the frozen verification plan cannot be overridden by visual review"
        );
        ensure!(
            matches!(
                row["decision"].as_str(),
                Some("accepted_visual" | "defect" | "unknown")
            ),
            "invalid measurement decision"
        );
        ensure!(
            !string(&row, "basis")?.trim().is_empty(),
            "measurement review needs a concrete visual basis"
        );
        if row["decision"] == "accepted_visual" {
            let page = f["page"]
                .as_u64()
                .and_then(|n| n.checked_sub(1))
                .and_then(|n| spec.pages.get(n as usize))
                .map(|p| json!(p.id))
                .unwrap_or_else(|| f["page"].clone());
            let object = review["object_reviews"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|o| o["page"] == page && o["object_id"] == f["object_id"])
                .ok_or_else(|| {
                    anyhow::anyhow!("measurement acceptance needs its object's observations")
                })?;
            ensure!(
                crate::object_review::CHECKS
                    .iter()
                    .all(|k| object["checks"][k]["status"] == "pass"),
                "unknown/defective object cannot receive visual measurement acceptance"
            );
            let has_reference = spec
                .pages
                .iter()
                .any(|p| json!(p.id) == page && p.design.is_some());
            if crate::fidelity::enabled(plan)
                && has_reference
                && crate::fidelity::verdict(&object["fidelity"]).0 != "pass"
            {
                // Preserve the author's claim but do not count it as acceptance.
                // Recording unknown/defect evidence must remain possible.
                continue;
            }
            accepted.insert(id.to_owned());
        }
    }
    Ok(accepted)
}

/// Explain what is missing without allowing a native failure or a frozen-plan
/// mismatch to be relabelled as visual success.
pub fn feedback(qa: &Value, review: &Value, accepted: &BTreeSet<String>, plan: &Value) -> Value {
    let remaining = qa["findings"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|f| !accepted.contains(f["id"].as_str().unwrap_or("")))
        .map(|f| {
            let decision = review["measurement_reviews"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|r| r["id"] == f["id"])
                .map(|r| r["decision"].clone());
            let next = if f["status"] == "fail" {
                "resolve_native_failure"
            } else if allowed_by_plan(f, plan)
                && decision.as_ref().is_some_and(|d| *d == "accepted_visual")
            {
                "resolve_reference_fidelity_before_visual_acceptance"
            } else if allowed_by_plan(f, plan) {
                "independent_review_required"
            } else {
                "measurement_or_contract_blocker"
            };
            json!({"id":f["id"],"page":f["page"],"object_id":f["object_id"],"code":f["code"],
                "native_status":f["status"],"decision":decision,"next_action":next})
        })
        .collect::<Vec<_>>();
    json!({"remaining":remaining,"accepted_count":accepted.len(),
        "instruction":"Reviewer uses measurement_reviews with each exact ID, decision and concrete visual basis. Object checks alone do not adjudicate native unknowns. Remaining fail/unknown checks are incomplete acceptance; Supervisor may report a blocker, never declare the deck accepted. Do not edit a correct slide or change the frozen plan to clear measurement gaps.",
        "native_status_unchanged":true,"quality_approved":false})
}
