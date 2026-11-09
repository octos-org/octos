//! Reference fidelity is separate from legibility. Facts are model observations;
//! Rust binds their pixel evidence and rejects contradictory or incomplete passes.
use crate::{Context, object_review, spec::Spec};
use anyhow::Context as _;
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::collections::BTreeSet;

pub const CONTRACT: &str = "reference-fidelity-v1";

pub fn enabled(plan: &Value) -> bool {
    plan["fidelity_contract"] == CONTRACT
}

pub fn evidence(item: &Value) -> Value {
    json!({"schema":CONTRACT,"object_evidence_hash":item["evidence_hash"],
        "viewport_normalized_xyxy":item["viewport_normalized_xyxy"],"sources":item["sources"]})
}

pub fn scaffold(item: &Value) -> Value {
    json!({"evidence":evidence(item),"comparison_reliable":null,
        "layout_relation":"unknown","reference_interference":"unknown",
        "current_interference":"unknown","interference_change":"unknown","basis":""})
}

/// Add metadata before the receipt is frozen. Old tasks keep their old contract.
pub fn attach(ctx: &Context, template: &mut Value, items: &[Value]) -> Result<()> {
    if !enabled(&crate::review_routes::load_plan(ctx)?) {
        return Ok(());
    }
    for row in template["object_reviews"]
        .as_array_mut()
        .context("fidelity::attach: missing required value")?
    {
        let item = items
            .iter()
            .find(|i| i["page"] == row["page"] && i["object_id"] == row["object_id"])
            .context("fidelity::attach: missing required value")?;
        if item["design_available"] == true {
            row["fidelity"] = scaffold(item);
        }
    }
    template["fidelity_contract"] = json!(CONTRACT);
    template["review_id"] = json!(object_review::review_id(
        template["cycle_sha256"]
            .as_str()
            .context("fidelity::attach: missing required value")?,
        &[
            template["review_id"].clone(),
            template["object_reviews"].clone(),
            json!(CONTRACT)
        ],
    )?);
    Ok(())
}

pub fn bind(source: &Value, row: &mut Value) -> Result<()> {
    if !source["fidelity"].is_object() {
        return Ok(());
    }
    if row.get("fidelity").is_none() {
        row["fidelity"] = source["fidelity"].clone(); // Unknown, never inferred pass.
    }
    let f = &mut row["fidelity"];
    ensure!(f.is_object(), "fidelity must be an observation object");
    ensure!(
        f.get("evidence")
            .is_none_or(|e| *e == source["fidelity"]["evidence"]),
        "fidelity evidence is stale or altered; use the current pack"
    );
    f["evidence"] = source["fidelity"]["evidence"].clone();
    ensure!(
        f["comparison_reliable"].is_null() || f["comparison_reliable"].is_boolean(),
        "comparison_reliable must be true, false or null"
    );
    for (field, choices) in [
        ("layout_relation", &["preserved", "changed", "unknown"][..]),
        (
            "reference_interference",
            &["absent", "present", "unknown"][..],
        ),
        (
            "current_interference",
            &["absent", "present", "unknown"][..],
        ),
        (
            "interference_change",
            &["equivalent", "reduced", "increased", "unknown"][..],
        ),
    ] {
        ensure!(
            f[field].is_null() || f[field].as_str().is_some_and(|v| choices.contains(&v)),
            "invalid fidelity {field}"
        );
    }
    Ok(())
}

/// No keyword parsing of prose, page coordinates, or global pixel-diff cutoff.
/// Deliberate slant/texture can pass when preserved; new interference cannot be
/// waived merely because foreground glyphs remain readable.
pub fn verdict(f: &Value) -> (&'static str, &'static str) {
    if f["comparison_reliable"] != true {
        return ("unknown", "REFERENCE_COMPARISON_UNCERTAIN");
    }
    if !f["basis"].as_str().is_some_and(|b| !b.trim().is_empty()) {
        return ("unknown", "MISSING_REFERENCE_OBSERVATION");
    }
    if f["layout_relation"] == "changed" {
        return ("defect", "REFERENCE_RELATION_CHANGED");
    }
    if (f["reference_interference"] == "absent" && f["current_interference"] == "present")
        || f["interference_change"] == "increased"
    {
        return ("defect", "ADDED_REFERENCE_INTERFERENCE");
    }
    let before = f["reference_interference"].as_str();
    let after = f["current_interference"].as_str();
    if f["layout_relation"] != "preserved"
        || !matches!(before, Some("absent" | "present"))
        || !matches!(after, Some("absent" | "present"))
    {
        return ("unknown", "INCOMPLETE_REFERENCE_OBSERVATIONS");
    }
    let change = f["interference_change"].as_str();
    let coherent = matches!(
        (before, after, change),
        (Some("absent"), Some("absent"), Some("equivalent"))
            | (
                Some("present"),
                Some("present"),
                Some("equivalent" | "reduced")
            )
            | (Some("present"), Some("absent"), Some("reduced"))
    );
    if !coherent {
        return ("unknown", "INCONSISTENT_REFERENCE_OBSERVATIONS");
    }
    ("pass", "REFERENCE_RELATIONS_REVIEWED")
}

pub fn object_result(item: &Value, row: &Value) -> (&'static str, &'static str) {
    if row["evidence_hash"] != item["evidence_hash"]
        || row["fidelity"]["evidence"] != evidence(item)
    {
        return ("unknown", "MISSING_OR_STALE_FIDELITY_EVIDENCE");
    }
    verdict(&row["fidelity"])
}

pub struct Assessment {
    pub unknown: usize,
    pub defects: usize,
    pub summary: Value,
}

pub fn require_independent(assessment: &mut Assessment, independent: bool, plan: &Value) {
    if !enabled(plan) || plan["mode"] != "independent_visual" || independent {
        return;
    }
    for row in assessment.summary["objects"]
        .as_array_mut()
        .expect("Assessment summaries are constructed with object rows")
    {
        if row["status"] == "pass" {
            row["status"] = json!("unknown");
            row["code"] = json!("INDEPENDENT_FIDELITY_REVIEW_REQUIRED");
            assessment.unknown += 1;
        }
    }
    assessment.summary["unknown_objects"] = json!(assessment.unknown);
}

/// Stable IDs enter the existing immutable-before/repair lifecycle; changing a
/// later observation to pass or omitting it cannot silently close the defect.
pub fn register_defects(
    ctx: &Context,
    cycle_path: &std::path::Path,
    summary: &Value,
    review: &mut Value,
) -> Result<()> {
    if !summary["objects"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|i| i["status"] == "defect")
    {
        return Ok(());
    }
    let (cycle, spec) = crate::review::bound_cycle(ctx, cycle_path)?;
    let mut findings = review["findings"].as_array().cloned().unwrap_or_default();
    for item in summary["objects"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|i| i["status"] == "defect")
    {
        let id = format!(
            "fidelity-{}",
            crate::fs::digest(&serde_json::to_vec(&json!([
                item["page"],
                item["object_id"]
            ]))?)
        );
        let pos = findings.iter().position(|f| f["id"] == id);
        let mut f = pos.map(|i| findings[i].clone()).unwrap_or(
            json!({"id":id,"page":item["page"],"object_id":item["object_id"],"check":"artwork"}),
        );
        ensure!(
            f["page"] == item["page"]
                && f["object_id"] == item["object_id"]
                && f["check"] == "artwork",
            "registered fidelity identity cannot be relabeled"
        );
        f["state"] = json!("open");
        f["note"] = json!(format!(
            "{}: independently compare original, blank and current; readable text alone does not establish fidelity",
            item["code"]
                .as_str()
                .context("fidelity::register_defects: missing required value")?
        ));
        if !f["registration"].is_object() {
            f = crate::finding_guard::register(ctx, &json!({"cycle":cycle_path,"findings":[f]}))?["findings"][0].clone();
        }
        // A carried registration is the immutable before-state; the evidence
        // path for this observation must still name the current render.
        f.as_object_mut()
            .context("fidelity::register_defects: missing required value")?
            .remove("evidence");
        if let Some(local) = crate::finding_guard::scope(ctx, &f, &json!({}), &cycle, &spec)? {
            f["evidence"] = local["image"]["path"].clone();
        }
        if let Some(i) = pos {
            findings[i] = f;
        } else {
            findings.push(f);
        }
    }
    review["findings"] = json!(findings);
    Ok(())
}

pub fn assess(cycle: &Value, spec: &Spec, review: &Value, plan: &Value) -> Result<Assessment> {
    let mut details = Vec::new();
    let mut unknown = 0;
    let mut defects = 0;
    if enabled(plan) {
        let mut seen = BTreeSet::new();
        for row in review["object_reviews"].as_array().into_iter().flatten() {
            ensure!(
                seen.insert((row["page"].to_string(), row["object_id"].to_string())),
                "duplicate fidelity object observation"
            );
        }
        for item in object_review::inventory(cycle, spec)?
            .iter()
            .filter(|i| i["design_available"] == true)
        {
            let row = review["object_reviews"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|r| r["page"] == item["page"] && r["object_id"] == item["object_id"])
                .unwrap_or(&Value::Null);
            let (status, code) = object_result(item, row);
            unknown += usize::from(status == "unknown");
            defects += usize::from(status == "defect");
            details.push(json!({"page":item["page"],"object_id":item["object_id"],"status":status,"code":code}));
        }
    }
    Ok(Assessment {
        unknown,
        defects,
        summary: json!({"contract":plan["fidelity_contract"],
        "required_objects":details.len(),"unknown_objects":unknown,"defect_objects":defects,"objects":details,
        "next":"Read matched original/blank/current regions and neighbors. Readability is independent of reference fidelity. New interference or changed layout requires repair; uncertainty requires review, never a prose waiver. Keep the last accepted output while testing candidates.",
        "visual_quality_verified_by_tool":false}),
    })
}

/// A background edit is a repair only within previously registered defect
/// viewports, with an unchanged design/canvas and fresh independent observations.
/// Every other constraint stays frozen. No source image is modified here.
pub fn repair_contract(
    ctx: &Context,
    cycle: &Value,
    spec: &Spec,
    review: &Value,
    old: &Value,
    independent: bool,
    plan: &Value,
) -> Result<(Value, Vec<Value>)> {
    let mut contract = old["qa_contract"].clone();
    let mut proofs = Vec::new();
    if !independent || !enabled(plan) || contract.is_null() {
        return Ok((contract, proofs));
    }
    let inventory = object_review::inventory(cycle, spec)?;
    for (i, page) in spec.pages.iter().enumerate() {
        let old_page = &old["qa_contract"]["pages"][&page.id];
        if old_page["background"].is_null() || old_page["background"] == page.background["sha256"] {
            continue;
        }
        let observations_complete = inventory
            .iter()
            .filter(|o| o["page"] == page.id)
            .all(|item| {
                review["object_reviews"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|r| r["page"] == item["page"] && r["object_id"] == item["object_id"])
                    .is_some_and(|r| {
                        object_result(item, r).0 == "pass"
                            && item["checks_required"].as_array().is_some_and(|checks| {
                                !checks.is_empty()
                                    && checks.iter().all(|k| {
                                        k.as_str().is_some_and(|name| {
                                            r["checks"][name]["status"] == "pass"
                                        })
                                    })
                            })
                    })
            });
        if !observations_complete || page.design.is_none() {
            proofs.push(json!({"page":page.id,"status":"blocked","code":"MISSING_INDEPENDENT_REPAIR_OBSERVATIONS"}));
            continue;
        }
        let mut before_image = None;
        let mut boxes = Vec::new();
        let mut receipts = Vec::new();
        for finding in old["findings"]
            .as_object()
            .into_iter()
            .flat_map(|m| m.values())
        {
            if finding["page"] != page.id
                || !matches!(finding["state"].as_str(), Some("open" | "resolved"))
                || !finding["id"]
                    .as_str()
                    .is_some_and(|id| id.starts_with("fidelity-"))
            {
                continue;
            }
            if !review["findings"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|f| f["id"] == finding["id"] && f["state"] == "resolved")
            {
                continue;
            }
            let receipt = ctx.json(ctx.verify(&finding["registration"])?)?;
            let (before, before_spec) =
                crate::review::bound_cycle(ctx, &ctx.verify(&receipt["cycle"])?)?;
            let Some(j) = before_spec.pages.iter().position(|p| p.id == page.id) else {
                continue;
            };
            let p = &before_spec.pages[j];
            if receipt["id"] != finding["id"]
                || receipt["before"]["page"] != page.id
                || p.background["sha256"] != old_page["background"]
                || serde_json::to_value(&before_spec.canvas)? != serde_json::to_value(&spec.canvas)?
                || p.design.as_ref().map(|b| &b["sha256"])
                    != page.design.as_ref().map(|b| &b["sha256"])
                || page.design.as_ref().map(|b| &b["sha256"]) != Some(&old_page["design"])
            {
                continue;
            }
            before_image = Some(before["renders"]["blank"]["pages"][j].clone());
            boxes.push(crate::geometry::rect(&receipt["before"]["box"])?);
            receipts.push(finding["registration"].clone());
        }
        let Some(before) = before_image else {
            proofs.push(
                json!({"page":page.id,"status":"blocked","code":"NO_MATCHING_REGISTERED_BASELINE"}),
            );
            continue;
        };
        let after = &cycle["renders"]["blank"]["pages"][i];
        let a = crate::review::load_image(ctx, &ctx.verify(&before)?)?.to_rgba8();
        let b = crate::review::load_image(ctx, &ctx.verify(after)?)?.to_rgba8();
        if a.dimensions() != b.dimensions() {
            proofs.push(json!({"page":page.id,"status":"blocked","code":"REPAIR_RENDER_DIMENSIONS_CHANGED"}));
            continue;
        }
        let mut changed = 0usize;
        let mut outside = 0usize;
        for (x, y, pixel) in a.enumerate_pixels() {
            if pixel != b.get_pixel(x, y) {
                changed += 1;
                if !boxes.iter().any(|r| {
                    f64::from(x) >= r[0].floor()
                        && f64::from(y) >= r[1].floor()
                        && f64::from(x) < (r[0] + r[2]).ceil()
                        && f64::from(y) < (r[1] + r[3]).ceil()
                }) {
                    outside += 1;
                }
            }
        }
        if changed == 0 || outside != 0 {
            proofs.push(json!({"page":page.id,"status":"blocked","code":if outside != 0 {"BACKGROUND_CHANGED_OUTSIDE_DEFECT"} else {"NO_VISIBLE_BACKGROUND_REPAIR"},
                "changed_pixels":changed,"changed_outside_registered_regions":outside,"before":before,"after":after}));
            continue;
        }
        proofs.push(
            json!({"page":page.id,"status":"scope_verified","before":before,"after":after,"registrations":receipts,
            "changed_pixels":changed,"changed_outside_registered_regions":outside,
            "original_design_unchanged":true,"visual_quality_proven":false}),
        );
        contract["pages"][&page.id]["background"] = page.background["sha256"].clone();
    }
    Ok((contract, proofs))
}
