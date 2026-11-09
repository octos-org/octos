//! Deterministic coverage and provenance, never a visual quality verdict.
use crate::{Context, fs::digest, spec::Spec, string, visual_diff};
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

fn inputs(ctx: &Context, cycle: &Value, spec: &Spec, i: usize) -> Result<Value> {
    let render = ctx.verify(&cycle["renders"]["editable"]["pages"][i])?;
    let (w, h) = image::image_dimensions(&render)?;
    let mut labels = crate::label_diff::labels_for_page(spec, &spec.pages[i], w, h)?;
    if !cycle["inspection"]["report"].is_null() {
        let report = ctx.json(ctx.verify(&cycle["inspection"]["report"])?)?;
        ensure!(
            report["input"]["sha256"] == cycle["editable"]["sha256"]
                && report["source_spec"]["sha256"] == cycle["spec"]["sha256"]
                && report["coordinate_space"] == "canvas_pixels",
            "native ink inspection is stale or uses different coordinates"
        );
        for label in &mut labels {
            if let Some(row) = report["objects"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|o| {
                    o["page"] == json!(i + 1)
                        && o["object_id"] == label.id
                        && o["text"] == label.text
                        && o["ink"]["status"] == "measured"
                        && o["ink"]["clipped_by_measurement_window"] == false
                })
            {
                let b = crate::geometry::rect(&row["ink"]["bounds"])?;
                let (sx, sy) = (
                    f64::from(w) / spec.canvas.width_px,
                    f64::from(h) / spec.canvas.height_px,
                );
                label.native_ink_box = Some([b[0] * sx, b[1] * sy, b[2] * sx, b[3] * sy]);
            }
        }
    }
    Ok(json!({
        "design":ctx.binding(ctx.verify(spec.pages[i].design.as_ref().unwrap())?)?,
        "background":ctx.binding(ctx.verify(&cycle["renders"]["blank"]["pages"][i])?)?,
        "render":ctx.binding(render)?,
        "labels":labels
    }))
}

fn key(inputs: &Value) -> String {
    digest(
        &serde_json::to_vec(&json!({"algorithm":visual_diff::ALGORITHM,"label_algorithm":crate::label_diff::ALGORITHM,
        "threshold":24,"tolerance":2,
        "design":inputs["design"]["sha256"],
        "background":inputs["background"]["sha256"],
        "render":inputs["render"]["sha256"],"labels":inputs["labels"]}))
        .expect("JSON serialization"),
    )
}

fn validate_report(ctx: &Context, binding: &Value, expected: &Value) -> Result<Value> {
    let report = ctx.json(ctx.verify(binding)?)?;
    ensure!(
        report["algorithm"] == visual_diff::ALGORITHM
            && report["mode"] == "background_subtracted"
            && report["threshold"] == 24
            && report["tolerance_px"] == 2,
        "comparison algorithm/settings differ; regenerate whole-page comparisons"
    );
    for kind in ["design", "background", "render"] {
        ensure!(
            report["inputs"][kind]["sha256"] == expected[kind]["sha256"],
            "comparison {kind} differs from current page"
        );
    }
    let (w, h) = image::image_dimensions(ctx.verify(&expected["render"])?)?;
    let pair: Vec<_> = ["design", "background"]
        .iter()
        .map(|kind| {
            let pixels = crate::review::load_image(ctx, &ctx.verify(&expected[*kind])?)?.to_rgb8();
            crate::visual_diff::normalize(pixels, (w, h))
        })
        .collect::<Result<_>>()?;
    ensure!(
        report["reference_alignment"]
            == crate::visual_proof::reference_alignment(&pair[0], &pair[1]),
        "reference alignment evidence differs from actual input pixels; regenerate comparisons"
    );
    ensure!(
        report["label_input"] == expected["labels"]
            && report["label_evidence"]["algorithm"] == crate::label_diff::ALGORITHM,
        "comparison label geometry differs; regenerate comparisons"
    );
    for label in report["label_evidence"]["labels"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("missing label context evidence"))?
    {
        if !label["panel"].is_null() {
            ctx.verify(&label["panel"])?;
        }
    }
    for finding in report["label_evidence"]["findings"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("missing label findings"))?
    {
        ctx.verify(&finding["evidence"])?;
    }
    ensure!(
        report["box"] == json!([0, 0, w, h]) && report["normalized_to"] == json!([w, h]),
        "comparison must cover the whole current page, not a selected crop"
    );
    ctx.verify(&report["panel"])?;
    for name in [
        "design",
        "background",
        "render",
        "reference-signal",
        "candidate-signal",
        "difference",
    ] {
        ctx.verify(&report["views"][name])?;
    }
    let mut seen = BTreeSet::new();
    let mut totals = [0u64; 2];
    for region in report["regions"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("missing comparison regions"))?
    {
        ensure!(
            seen.insert(string(region, "id")?),
            "duplicate comparison region"
        );
        let b = crate::geometry::rect(&region["box"])?;
        ensure!(
            b[0] >= 0. && b[1] >= 0. && b[0] + b[2] <= f64::from(w) && b[1] + b[3] <= f64::from(h),
            "comparison region outside page"
        );
        for (i, field) in ["reference_only_pixels", "candidate_only_pixels"]
            .iter()
            .enumerate()
        {
            totals[i] = totals[i]
                .checked_add(
                    region[*field]
                        .as_u64()
                        .ok_or_else(|| anyhow::anyhow!("invalid marked-pixel count"))?,
                )
                .ok_or_else(|| anyhow::anyhow!("marked-pixel count overflow"))?;
        }
    }
    ensure!(
        json!(totals[0]) == report["reference_only_pixels"]
            && json!(totals[1]) == report["candidate_only_pixels"],
        "comparison regions do not cover all marked pixels"
    );
    Ok(report)
}

/// All reference pages get a whole-page comparison. A failed comparison remains
/// explicit, allowing the built deck to be inspected without silently passing QA.
pub fn generate(
    ctx: &Context,
    cycle: &Value,
    spec: &Spec,
    output: &Path,
    reuse: Option<&str>,
) -> Result<Value> {
    let output = ctx.path(output)?;
    ensure!(!output.exists(), "use a fresh comparisons directory");
    std::fs::create_dir_all(&output)?;
    let old = reuse.map(|p| ctx.json(p)).transpose()?.unwrap_or(json!({}));
    let mut pages = Vec::new();
    for (i, page) in spec.pages.iter().enumerate() {
        if page.design.is_none() {
            pages.push(json!({"page":page.id,"status":"no_reference"}));
            continue;
        }
        let current = inputs(ctx, cycle, spec, i)?;
        let fingerprint = key(&current);
        let cached = old["pages"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|p| {
                p["page"] == page.id && p["comparison_key"] == fingerprint && p["status"] == "ready"
            })
            .filter(|p| validate_report(ctx, &p["report"], &current).is_ok());
        let mut entry = json!({"page":page.id,"comparison_key":fingerprint,"inputs":current});
        if let Some(cached) = cached {
            entry["status"] = json!("ready");
            entry["report"] = cached["report"].clone();
            entry["reused"] = json!(true);
        } else {
            // Index-derived file names cannot interpret a page ID as a path.
            let panel = output.join(format!("page-{:03}.png", i + 1));
            let generated = visual_diff::compare(
                ctx,
                &json!({"before":current["design"]["path"],
                "background":current["background"]["path"],"after":current["render"]["path"],
                "output":panel,"labels":current["labels"]}),
            );
            match generated {
                Ok(_) => {
                    entry["status"] = json!("ready");
                    entry["report"] = ctx.binding(panel.with_extension("json"))?;
                    entry["reused"] = json!(false);
                }
                Err(e) => {
                    entry["status"] = json!("unavailable");
                    entry["error"] = json!(e.to_string());
                }
            }
        }
        pages.push(entry);
    }
    let manifest = json!({"schema_version":1,"algorithm":visual_diff::ALGORITHM,"pages":pages,"inspection":cycle["inspection"],
        "quality_approved":false,"next":"Read actual pages and label context panels before producer verdicts. Classify regions and each label_evidence finding individually in review.comparisons; whole-page font-difference claims do not resolve label findings."});
    let path = ctx.write_json(output.join("manifest.json"), &manifest)?;
    ctx.binding(path)
}

/// Retrofit older cycles without rebuilding or overwriting their PPTX files.
pub fn compare_cycle(ctx: &Context, r: &Value) -> Result<Value> {
    let (mut cycle, spec) = crate::review::bound_cycle(ctx, &ctx.path(string(r, "cycle")?)?)?;
    if let Some(path) = r["inspection"].as_str() {
        cycle["inspection"] = json!({"report":ctx.binding(path)?});
    }
    let manifest = generate(
        ctx,
        &cycle,
        &spec,
        &ctx.path(string(r, "output")?)?,
        r["reuse_comparisons"].as_str(),
    )?;
    let value = ctx.json(ctx.verify(&manifest)?)?;
    Ok(json!({"comparisons":manifest,"pages":value["pages"],"quality_approved":false}))
}

pub struct Coverage {
    pub artifacts: Vec<Value>,
    pub reviews: BTreeMap<String, Value>,
    pub required_pages: BTreeSet<String>,
    pub unknown: usize,
    pub defects: usize,
    pub summary: Value,
}

fn nonempty_note(value: &Value) -> Result<()> {
    ensure!(
        !string(value, "note")?.trim().is_empty(),
        "comparison review needs an observation/reason"
    );
    Ok(())
}

/// Reuse observations only for exactly unchanged reference/blank/render bytes.
/// The model still supplies judgments; missing/stale coverage never means pass.
pub fn assess(
    ctx: &Context,
    cycle: &Value,
    spec: &Spec,
    review: &Value,
    old: &Value,
    override_path: Option<&str>,
) -> Result<Coverage> {
    let mut out = Coverage {
        artifacts: vec![],
        reviews: BTreeMap::new(),
        required_pages: BTreeSet::new(),
        unknown: 0,
        defects: 0,
        summary: Value::Null,
    };
    let old_required: BTreeSet<String> = serde_json::from_value(
        old.get("comparison_required_pages")
            .cloned()
            .unwrap_or(json!([])),
    )?;
    let current_required: BTreeSet<String> = spec
        .pages
        .iter()
        .filter(|p| p.design.is_some())
        .map(|p| p.id.clone())
        .collect();
    ensure!(
        old_required.is_subset(&current_required),
        "previously supplied design reference was removed; preserve reference coverage"
    );
    out.required_pages = current_required.clone();
    let binding = override_path
        .map(|p| ctx.binding(p))
        .transpose()?
        .or_else(|| cycle.get("comparisons").cloned());
    let manifest = if let Some(binding) = binding {
        let manifest = ctx.json(ctx.verify(&binding)?)?;
        ensure!(
            manifest["schema_version"] == 1 && manifest["algorithm"] == visual_diff::ALGORITHM,
            "unsupported comparison manifest; regenerate with compare_cycle"
        );
        out.artifacts.push(binding);
        Some(manifest)
    } else {
        None
    };
    let mut entries = BTreeMap::new();
    let mut cycle = cycle.clone();
    if let Some(manifest) = &manifest {
        if cycle["inspection"].is_null() && !manifest["inspection"].is_null() {
            cycle["inspection"] = manifest["inspection"].clone();
        }
        for entry in manifest["pages"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("missing comparison pages"))?
        {
            let page = string(entry, "page")?;
            ensure!(
                spec.pages.iter().any(|p| p.id == page) && entries.insert(page, entry).is_none(),
                "unknown/duplicate comparison page"
            );
        }
        ensure!(
            entries.len() == spec.pages.len(),
            "comparison manifest omits pages"
        );
    }
    let mut submitted = BTreeMap::new();
    if let Some(rows) = review.get("comparisons") {
        for row in rows
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("review.comparisons must be an array"))?
        {
            let page = string(row, "page")?;
            ensure!(
                current_required.contains(page) && submitted.insert(page, row).is_none(),
                "unknown/duplicate comparison review page"
            );
        }
    }
    let mut summaries = Vec::new();
    for (i, page) in spec
        .pages
        .iter()
        .enumerate()
        .filter(|(_, p)| p.design.is_some())
    {
        let current = inputs(ctx, &cycle, spec, i)?;
        let fingerprint = key(&current);
        let Some(entry) = entries.get(page.id.as_str()) else {
            out.unknown += 1;
            summaries.push(json!({"page":page.id,"status":"missing_comparison","next":"Run compare_cycle and supply its manifest via comparisons"}));
            continue;
        };
        ensure!(
            entry["comparison_key"] == fingerprint,
            "stale comparison for {}; regenerate from the current cycle",
            page.id
        );
        if entry["status"] != "ready" {
            out.unknown += 1;
            summaries.push(
                json!({"page":page.id,"status":"comparison_unavailable","error":entry["error"]}),
            );
            continue;
        }
        let report = validate_report(ctx, &entry["report"], &current)?;
        // A model's region dispositions cannot make mismatched artwork a valid
        // subtraction baseline. Keep generating panels for diagnosis.
        let alignment_unknown = usize::from(report["reference_alignment"]["status"] != "supported");
        out.artifacts.push(entry["report"].clone());
        out.artifacts.push(report["panel"].clone());
        out.artifacts
            .extend(report["views"].as_object().unwrap().values().cloned());
        for label in report["label_evidence"]["labels"].as_array().unwrap() {
            if !label["panel"].is_null() {
                out.artifacts.push(label["panel"].clone());
            }
        }
        for finding in report["label_evidence"]["findings"].as_array().unwrap() {
            if !out.artifacts.contains(&finding["evidence"]) {
                out.artifacts.push(finding["evidence"].clone());
            }
        }
        // A supplied stale review is a caller error. A stale retained review is
        // simply invalidated, with only that page returned for fresh review.
        let explicit = submitted.get(page.id.as_str()).copied();
        if let Some(row) = explicit {
            ensure!(
                row["comparison_key"] == fingerprint,
                "stale comparison review for {}",
                page.id
            );
        }
        let retained = old["comparison_reviews"]
            .get(&page.id)
            .filter(|v| v["comparison_key"] == fingerprint);
        let Some(row) = explicit.or(retained) else {
            out.unknown += 1;
            summaries.push(json!({"page":page.id,"status":"unreviewed","comparison_key":fingerprint,"report":entry["report"]}));
            continue;
        };
        nonempty_note(row)?;
        let inspected = row["inspected"]
            .as_bool()
            .ok_or_else(|| anyhow::anyhow!("comparison inspected must be boolean"))?;
        let ids: BTreeSet<&str> = report["regions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| string(r, "id"))
            .collect::<Result<_>>()?;
        let mut dispositions = BTreeMap::new();
        for group in row["regions"].as_array().ok_or_else(|| {
            anyhow::anyhow!(
                "comparison region dispositions required (use [] when no marked regions)"
            )
        })? {
            nonempty_note(group)?;
            let state = string(group, "classification")?;
            ensure!(
                matches!(state, "defect" | "acceptable_difference" | "unresolved"),
                "invalid comparison classification"
            );
            let group_ids = group["ids"]
                .as_array()
                .filter(|a| !a.is_empty())
                .ok_or_else(|| anyhow::anyhow!("region ids required"))?;
            for id in group_ids {
                let id = id
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("region id must be string"))?;
                ensure!(
                    ids.contains(id) && dispositions.insert(id, state).is_none(),
                    "unknown/duplicate region disposition"
                );
            }
        }
        let missing: Vec<_> = ids
            .iter()
            .filter(|id| !dispositions.contains_key(**id))
            .copied()
            .collect();
        let (label_unknown, label_defects, label_pending) = assess_label_findings(&report, row)?;
        let unknown = alignment_unknown
            + label_unknown
            + usize::from(!inspected)
            + missing.len()
            + dispositions
                .values()
                .filter(|s| **s == "unresolved")
                .count();
        let defects = label_defects + dispositions.values().filter(|s| **s == "defect").count();
        out.unknown += unknown;
        out.defects += defects;
        out.reviews.insert(page.id.clone(), row.clone());
        summaries.push(json!({"page":page.id,"comparison_key":fingerprint,
            "status":if defects>0{"fail"}else if unknown>0{"unknown"}else{"reviewed"},
            "reference_alignment":report["reference_alignment"],"reused_review":explicit.is_none(),"unclassified_regions":missing,"unreviewed_label_findings":label_pending,"unresolved":unknown,"defects":defects}));
    }
    out.summary = json!({"required_pages":current_required,"pages":summaries,
        "unknown":out.unknown,"defects":out.defects,"visual_quality_verified_by_tool":false});
    Ok(out)
}

/// Evidence completeness only. A model can recognize an intentional underline,
/// but cannot acknowledge every page region and silently skip the label warning.
fn assess_label_findings(report: &Value, row: &Value) -> Result<(usize, usize, Vec<String>)> {
    let findings = report["label_evidence"]["findings"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("missing label findings"))?;
    let mut submitted = BTreeMap::new();
    if let Some(values) = row.get("label_findings") {
        for disposition in values
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("label_findings must be an array"))?
        {
            let id = string(disposition, "id")?;
            let finding = findings
                .iter()
                .find(|f| f["id"] == id)
                .ok_or_else(|| anyhow::anyhow!("unknown label finding {id}"))?;
            ensure!(
                submitted.insert(id, disposition).is_none(),
                "duplicate label finding {id}"
            );
            nonempty_note(disposition)?;
            ensure!(
                disposition["evidence_sha256"] == finding["evidence"]["sha256"]
                    && finding["evidence"]["sha256"].is_string(),
                "label finding needs its current context panel hash"
            );
            let state = string(disposition, "classification")?;
            ensure!(
                matches!(state, "defect" | "acceptable_difference" | "unresolved"),
                "invalid label finding classification"
            );
            if state == "acceptable_difference" {
                let reason = string(disposition, "reason_code")?;
                ensure!(
                    matches!(
                        reason,
                        "intentional_change"
                            | "measurement_artifact"
                            | "reference_relation"
                            | "font_metrics"
                    ),
                    "unsupported label acceptance reason"
                );
                ensure!(
                    reason != "font_metrics"
                        || matches!(
                            finding["kind"].as_str(),
                            Some("ink_extent_changed" | "ink_position_changed")
                        ),
                    "font metrics cannot dismiss artwork clearance, peer alignment or uncertain reference geometry"
                );
                if reason == "reference_relation" && finding["kind"] == "artwork_line_clearance" {
                    let m = &finding["measurements"];
                    ensure!(
                        m["reference_geometry_reliable"] == true
                            && m["reference_clearance_px"]
                                .as_f64()
                                .zip(m["clearance_tolerance_px"].as_f64())
                                .is_some_and(|(gap, tolerance)| gap <= tolerance),
                        "reference_relation contradicts missing/unreliable/clear reference clearance; inspect the context and document a measurement artifact or intentional change instead"
                    );
                }
            }
        }
    }
    let mut pending = Vec::new();
    let mut defects = 0;
    for finding in findings {
        let id = string(finding, "id")?;
        match submitted.get(id).map(|d| d["classification"].as_str()) {
            Some(Some("acceptable_difference")) => (),
            Some(Some("defect")) => defects += 1,
            _ => pending.push(id.to_owned()),
        }
    }
    Ok((pending.len(), defects, pending))
}

#[cfg(test)]
mod label_review_tests {
    use super::*;

    #[test]
    fn reference_relation_cannot_relabel_unreliable_or_clear_source_geometry() {
        let mut report = json!({"label_evidence":{"findings":[{"id":"line","kind":"artwork_line_clearance",
            "evidence":{"sha256":"context"},"measurements":{"reference_geometry_reliable":false,
            "reference_clearance_px":-20,"candidate_clearance_px":-20,"clearance_tolerance_px":5}}]}});
        let mut row = json!({"label_findings":[{"id":"line","evidence_sha256":"context","classification":"acceptable_difference",
            "reason_code":"reference_relation","note":"Claimed same relation"}]});
        assert!(assess_label_findings(&report, &row).is_err());
        report["label_evidence"]["findings"][0]["measurements"]["reference_geometry_reliable"] =
            json!(true);
        report["label_evidence"]["findings"][0]["measurements"]["reference_clearance_px"] =
            json!(25);
        assert!(assess_label_findings(&report, &row).is_err());
        report["label_evidence"]["findings"][0]["measurements"]["reference_clearance_px"] =
            json!(-20);
        assert_eq!(assess_label_findings(&report, &row).unwrap().0, 0);
        row["label_findings"][0]["reason_code"] = json!("measurement_artifact");
        row["label_findings"][0]["note"] = json!(
            "Context shows an intentional underline; envelope intersection is not a label crossing a connector"
        );
        report["label_evidence"]["findings"][0]["measurements"]["reference_geometry_reliable"] =
            json!(false);
        assert_eq!(assess_label_findings(&report, &row).unwrap().0, 0);
    }
}
