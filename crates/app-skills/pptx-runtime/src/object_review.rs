//! Object-level visual observations bound to real page pixels, not detector hits.
//! This validates coverage and provenance; it cannot prove a model saw an image.
use crate::{Context, fs::digest, geometry, spec::Spec};
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

pub const ALGORITHM: &str = "object-visual-evidence-v1";
pub const CHECKS: [&str; 5] = [
    "ownership",
    "containment",
    "alignment",
    "direction",
    "typography",
];
pub const SYMBOL_CHECKS: [&str; 3] = ["representation", "uniqueness", "visibility"];

/// An opaque, printable receipt, not a secret or a model-copied artifact digest.
/// Segmenting preserves every digest bit without resembling a bearer key.
pub fn review_id(cycle_digest: &str, objects: &[Value]) -> Result<String> {
    let hash = digest(&serde_json::to_vec(&json!([cycle_digest, objects]))?);
    Ok(format!(
        "review-{}",
        hash.as_bytes()
            .chunks(8)
            .map(|p| std::str::from_utf8(p).unwrap())
            .collect::<Vec<_>>()
            .join("-")
    ))
}

pub fn attach_relations(ctx: &Context, template: &mut Value, binding: &Value) -> Result<()> {
    let report = ctx.json(ctx.verify(binding)?)?;
    ensure!(
        report["algorithm"] == crate::relations::ALGORITHM
            && report["cycle"]["sha256"] == template["cycle_sha256"],
        "stale relation report"
    );
    let signals: Vec<Value> = report["pages"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|p| p["signals"].as_array().into_iter().flatten())
        .cloned()
        .collect();
    template["review_id"] = json!(review_id(
        template["cycle_sha256"].as_str().unwrap(),
        &[
            template["object_reviews"].clone(),
            binding["sha256"].clone()
        ]
    )?);
    template["relations"] = binding.clone();
    template["relation_reviews"]=json!(signals.iter().map(|s|json!({"id":s["id"],"status":"unknown","basis":"Awaiting visual assessment of measured candidate"})).collect::<Vec<_>>());
    Ok(())
}

/// Include the actual difference reports in the receipt. Reviewers name pages
/// and findings; Rust supplies digests that a model-facing redactor may hide.
pub fn attach_comparisons(
    ctx: &Context,
    template: &mut Value,
    cycle: &Value,
    spec: &Spec,
    binding: &Value,
) -> Result<()> {
    let path = ctx.verify(binding)?;
    crate::comparison_review::assess(ctx, cycle, spec, &json!({}), &json!({}), path.to_str())?;
    let manifest = ctx.json(&path)?;
    let mut rows = Vec::new();
    for entry in manifest["pages"].as_array().unwrap() {
        if entry["status"] != "ready" {
            continue; // Missing/failed comparisons remain unknown in assess.
        }
        let report = ctx.json(ctx.verify(&entry["report"])?)?;
        rows.push(json!({"page":entry["page"],"comparison_key":entry["comparison_key"],
            "inspected":false,"note":"Awaiting independent comparison review",
            "regions":report["regions"].as_array().unwrap().iter().map(|r|
                json!({"ids":[r["id"]],"classification":"unresolved","note":"Awaiting visual assessment"})).collect::<Vec<_>>(),
            "label_findings":report["label_evidence"]["findings"].as_array().unwrap().iter().map(|f|
                json!({"id":f["id"],"evidence_sha256":f["evidence"]["sha256"],
                    "classification":"unresolved","note":"Awaiting context panel assessment"})).collect::<Vec<_>>()
        }));
    }
    template["review_id"] = json!(review_id(
        template["cycle_sha256"].as_str().unwrap(),
        &[
            template["review_id"].clone(),
            binding["sha256"].clone(),
            json!(rows)
        ],
    )?);
    template["comparison_manifest"] = binding.clone();
    template["comparisons"] = json!(rows);
    Ok(())
}

/// Bind authored observations to the *same* immutable pack they were read from.
/// This never manufactures observations, fills passes, or retargets stale work.
pub fn bind_observations(template: &Value, observations: &Value) -> Result<Value> {
    ensure!(
        template["review_id"].is_string() && observations["review_id"] == template["review_id"],
        "missing/stale review_id; use the receipt from the pack that was actually reviewed"
    );
    let required = template["object_reviews"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("invalid object review template"))?;
    let rows = observations["object_reviews"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("object_reviews must be an array"))?;
    let mut result = observations.clone();
    let mut bound = Vec::new();
    let mut seen = BTreeSet::new();
    for row in rows {
        let key = (row["page"].to_string(), row["object_id"].to_string());
        ensure!(seen.insert(key), "duplicate object observation");
        let source = required
            .iter()
            .find(|v| v["page"] == row["page"] && v["object_id"] == row["object_id"])
            .ok_or_else(|| anyhow::anyhow!("observation names an unknown page/object"))?;
        let checks = row["checks"]
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("checks must be an object"))?;
        ensure!(
            checks.keys().all(|k| source["checks"].get(k).is_some()),
            "unknown object check"
        );
        for value in checks.values() {
            let status = value["status"].as_str().unwrap_or("");
            ensure!(
                matches!(status, "pass" | "defect" | "unknown"),
                "invalid check status"
            );
            ensure!(
                status == "unknown"
                    || value["observation"]
                        .as_str()
                        .is_some_and(|s| !s.trim().is_empty()),
                "pass/defect requires an observation"
            );
        }
        let mut row = row.clone();
        row["evidence_hash"] = source["evidence_hash"].clone();
        crate::fidelity::bind(source, &mut row)?;
        bound.push(row);
    }
    result["cycle_sha256"] = template["cycle_sha256"].clone();
    if template["relations"].is_object() {
        result["relations"] = template["relations"].clone();
    }
    result["object_reviews"] = json!(bound);
    if let Some(rows) = observations.get("comparisons") {
        let required = template["comparisons"].as_array().ok_or_else(|| {
            anyhow::anyhow!("comparison observations require a pack with comparisons")
        })?;
        let mut seen = BTreeSet::new();
        let mut bound = Vec::new();
        for row in rows
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("comparisons must be an array"))?
        {
            ensure!(
                seen.insert(row["page"].to_string()),
                "duplicate comparison observation"
            );
            let source = required
                .iter()
                .find(|s| s["page"] == row["page"])
                .ok_or_else(|| anyhow::anyhow!("comparison names an unknown/unavailable page"))?;
            let mut row = row.clone();
            row["comparison_key"] = source["comparison_key"].clone();
            if let Some(findings) = row.get_mut("label_findings") {
                let mut seen = BTreeSet::new();
                for finding in findings
                    .as_array_mut()
                    .ok_or_else(|| anyhow::anyhow!("label_findings must be an array"))?
                {
                    ensure!(
                        seen.insert(finding["id"].to_string()),
                        "duplicate label finding"
                    );
                    let source = source["label_findings"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|f| f["id"] == finding["id"])
                        .ok_or_else(|| anyhow::anyhow!("unknown label finding"))?;
                    finding["evidence_sha256"] = source["evidence_sha256"].clone();
                }
            }
            bound.push(row);
        }
        result["comparisons"] = json!(bound);
    }
    Ok(result)
}

pub fn submit(ctx: &Context, request: &Value) -> Result<Value> {
    let pack = ctx.json(crate::string(request, "pack")?)?;
    let cycle_path = ctx.verify(&pack["cycle"])?;
    let (cycle, spec) = crate::review::bound_cycle(ctx, &cycle_path)?;
    for artifact in pack["artifacts"].as_array().into_iter().flatten() {
        if artifact["image"].is_object() {
            ctx.verify(&artifact["image"])?;
        }
    }
    let saved = ctx.json(ctx.verify(&pack["object_review_template"])?)?;
    let all = inventory(&cycle, &spec)?;
    let selected: Vec<_> = all
        .iter()
        .filter(|item| {
            pack["object_evidence"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|v| v["page"] == item["page"] && v["object_id"] == item["object_id"])
        })
        .cloned()
        .collect();
    let mut fresh = template(ctx, &cycle_path, &selected)?;
    crate::fidelity::attach(ctx, &mut fresh, &selected)?;
    if pack["relations"].is_object() {
        attach_relations(ctx, &mut fresh, &pack["relations"])?;
    }
    if pack["comparisons"].is_object() {
        attach_comparisons(ctx, &mut fresh, &cycle, &spec, &pack["comparisons"])?;
    }
    ensure!(
        saved == fresh,
        "review pack is stale or its inventory was modified"
    );
    let observation_path = crate::string(request, "observations")?;
    let mut observations = ctx.json(observation_path)?;
    if pack["reuse"]["previous_review"].is_object() {
        let previous_review = ctx.verify(&pack["reuse"]["previous_review"])?;
        let previous_cycle = ctx.verify(&pack["reuse"]["previous_cycle"])?;
        let signals = if pack["relations"].is_object() {
            let rel = ctx.json(ctx.verify(&pack["relations"])?)?;
            rel["pages"]
                .as_array()
                .into_iter()
                .flatten()
                .flat_map(|p| p["signals"].as_array().into_iter().flatten())
                .cloned()
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let reuse = crate::review_reuse::collect(
            ctx,
            &cycle_path,
            previous_cycle.to_str().unwrap(),
            previous_review.to_str().unwrap(),
            &signals,
        )?;
        let mut comparable = reuse.clone();
        // Older packs omitted defect metadata. Recompute it from their verified
        // predecessor instead of requiring the model to reconstruct receipts.
        if pack["reuse"].get("findings").is_none() {
            comparable.as_object_mut().unwrap().remove("findings");
        }
        ensure!(
            comparable == pack["reuse"],
            "reused evidence changed; generate a fresh pack"
        );
        crate::review_reuse::merge(&mut observations, &reuse)?;
    }
    if pack["relations"].is_object() {
        let report = ctx.json(ctx.verify(&pack["relations"])?)?;
        let signals: Vec<_> = report["pages"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|p| p["signals"].as_array().into_iter().flatten())
            .cloned()
            .collect();
        crate::relations::validate_responses_with_route(
            &signals,
            &observations,
            crate::review_routes::permitted(ctx)?,
        )?;
    }
    let mut bound = bind_observations(&fresh, &observations)?;
    let mut fidelity = crate::fidelity::assess(
        &cycle,
        &spec,
        &bound,
        &crate::review_routes::load_plan(ctx)?,
    )?;
    crate::fidelity::require_independent(
        &mut fidelity,
        crate::review_routes::permitted(ctx)?,
        &crate::review_routes::load_plan(ctx)?,
    );
    crate::fidelity::register_defects(ctx, &cycle_path, &fidelity.summary, &mut bound)?;
    if let Some(list) = bound["findings"].as_array().cloned() {
        let mut registered = Vec::new();
        for mut f in list {
            if f.get("evidence").is_none()
                && let Some(local) =
                    crate::finding_guard::scope(ctx, &f, &json!({}), &cycle, &spec)?
            {
                f["evidence"] = local["image"]["path"].clone();
            }
            if f["state"] == "open" && !f["registration"].is_object() {
                let r = crate::finding_guard::register(
                    ctx,
                    &json!({"cycle":cycle_path,"findings":[f]}),
                )?;
                registered.push(r["findings"][0].clone());
            } else {
                registered.push(f);
            }
        }
        bound["findings"] = json!(registered);
    }
    let native = crate::qa::evaluate(ctx, cycle_path.to_str().unwrap(), &Value::Null)?;
    crate::review_routes::adjudicated(
        &native,
        &bound,
        crate::review_routes::permitted(ctx)?,
        &spec,
        &crate::review_routes::load_plan(ctx)?,
    )?;
    let assessment = assess(&cycle, &spec, &bound, &json!({}))?;
    let comparison_path = pack["comparisons"]["path"].as_str();
    let comparisons =
        crate::comparison_review::assess(ctx, &cycle, &spec, &bound, &json!({}), comparison_path)?;
    // The model already explicitly inspected/classified each bound whole-page
    // comparison. Do not require it to transcribe the same source page IDs into
    // three additional coverage arrays. Defects and unknown observations remain
    // in their own gates; this records inspection, never approval.
    let reviewed: Vec<_> = comparisons.summary["pages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|p| p["status"] == "reviewed")
        .map(|p| p["page"].clone())
        .collect();
    ensure!(
        bound.get("inspected").is_none_or(Value::is_object),
        "inspected must be an object of page-coverage arrays"
    );
    for domain in ["copy", "artwork", "readability"] {
        if bound["inspected"].get(domain).is_none() && !reviewed.is_empty() {
            bound["inspected"][domain] = json!(reviewed);
        }
    }
    crate::review_routes::attest(ctx, &ctx.binding(&cycle_path)?, &mut bound)?;
    let path = ctx.write_json(crate::string(request, "output")?, &bound)?;
    Ok(
        json!({"review":ctx.binding(path)?,"observations":ctx.binding(observation_path)?,
        "object_coverage":assessment.summary,"reference_fidelity":fidelity.summary,"comparison_coverage":comparisons.summary,"quality_approved":false,
        "note":"Only metadata was bound. Visual observations are unchanged; missing checks remain unknown. Submit this file through evidence_record for the existing complete QA gates."}),
    )
}

/// Include the intended owner and surroundings, even when no detector fired.
pub fn inventory(cycle: &Value, spec: &Spec) -> Result<Vec<Value>> {
    let mut rows = Vec::new();
    for (i, page) in spec.pages.iter().enumerate() {
        let mut sources = vec![
            json!({"role":"current","image":cycle["renders"]["editable"]["pages"][i]}),
            json!({"role":"blank","image":cycle["renders"]["blank"]["pages"][i]}),
        ];
        if let Some(design) = &page.design {
            sources.insert(0, json!({"role":"design","image":design}));
        }
        // Include all declarations: a changed neighbor, owner, copy or style can
        // invalidate a judgment even if this object's own coordinates did not move.
        let mut declaration = serde_json::to_value(page)?;
        declaration.as_object_mut().unwrap().remove("background");
        declaration.as_object_mut().unwrap().remove("design");
        let page_state = json!({"page":declaration,"styles":spec.styles,"canvas":spec.canvas});
        let source_hashes: Vec<_> = sources
            .iter()
            .map(|s| json!({"role":s["role"],"sha256":s["image"]["sha256"]}))
            .collect();
        for t in &page.texts {
            let mut points = Vec::from(geometry::frame(t.bounds, t.rotation));
            if let Some(region) = t.region.as_ref().and_then(|id| page.regions.get(id)) {
                points.extend(geometry::quad(region)?);
            }
            let mut area = [
                points.iter().map(|p| p[0]).fold(f64::INFINITY, f64::min) / spec.canvas.width_px,
                points.iter().map(|p| p[1]).fold(f64::INFINITY, f64::min) / spec.canvas.height_px,
                points
                    .iter()
                    .map(|p| p[0])
                    .fold(f64::NEG_INFINITY, f64::max)
                    / spec.canvas.width_px,
                points
                    .iter()
                    .map(|p| p[1])
                    .fold(f64::NEG_INFINITY, f64::max)
                    / spec.canvas.height_px,
            ];
            let dx = ((area[2] - area[0]) * 0.3).max(0.04);
            let dy = ((area[3] - area[1]) * 0.5).max(0.06);
            area = [
                (area[0] - dx).max(0.),
                (area[1] - dy).max(0.),
                (area[2] + dx).min(1.),
                (area[3] + dy).min(1.),
            ];
            // Fully off-page text still needs review; do not hide it with an empty crop.
            if area[2] <= area[0] || area[3] <= area[1] {
                area = [0., 0., 1., 1.];
            }
            let key = digest(&serde_json::to_vec(&json!({"algorithm":ALGORITHM,
                "page_state":page_state,"sources":source_hashes,"object_id":t.id,"viewport":area}))?);
            rows.push(
                json!({"page":page.id,"page_number":i+1,"object_id":t.id,"text":t.text,
                "declared_owner":t.region,"declared_owner_is_semantically_verified":false,
                "box":t.bounds,"rotation":t.rotation,"style":spec.style(t)?,
                "viewport_normalized_xyxy":area,"sources":sources,"evidence_hash":key,
                "design_available":page.design.is_some(),"checks_required":CHECKS}),
            );
        }
        for symbol in &page.artwork_symbols {
            let [x, y, w, h] = symbol.bounds;
            let dx = (w * 0.5).max(spec.canvas.width_px * 0.04);
            let dy = (h * 0.5).max(spec.canvas.height_px * 0.06);
            let area = [
                (x - dx).max(0.) / spec.canvas.width_px,
                (y - dy).max(0.) / spec.canvas.height_px,
                (x + w + dx).min(spec.canvas.width_px) / spec.canvas.width_px,
                (y + h + dy).min(spec.canvas.height_px) / spec.canvas.height_px,
            ];
            let key = digest(&serde_json::to_vec(&json!({"algorithm":ALGORITHM,
                "page_state":page_state,"sources":source_hashes,"artwork_symbol":symbol,"viewport":area}))?);
            rows.push(json!({"page":page.id,"page_number":i+1,"object_id":symbol.id,
                "kind":"artwork_symbol","text":symbol.text,"copy_id":symbol.copy_id,"copy_range":symbol.copy_range,
                "box":symbol.bounds,"declared_basis":symbol.basis,"semantic_equivalence_verified":false,
                "viewport_normalized_xyxy":area,"sources":sources,"evidence_hash":key,
                "design_available":page.design.is_some(),"checks_required":SYMBOL_CHECKS}));
        }
    }
    Ok(rows)
}

pub struct Assessment {
    pub unknown: usize,
    pub defects: usize,
    pub summary: Value,
    pub reviews: Vec<Value>,
}

/// Missing/old evidence is unknown. No blanket page pass can replace observations.
pub fn assess(cycle: &Value, spec: &Spec, review: &Value, old: &Value) -> Result<Assessment> {
    let required = inventory(cycle, spec)?;
    let mut stored = BTreeMap::new();
    for r in old["object_reviews"].as_array().into_iter().flatten() {
        stored.insert(
            (r["page"].to_string(), r["object_id"].to_string()),
            r.clone(),
        );
    }
    let mut seen = BTreeSet::new();
    if let Some(list) = review.get("object_reviews") {
        for r in list
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("object_reviews must be an array"))?
        {
            let key = (r["page"].to_string(), r["object_id"].to_string());
            ensure!(seen.insert(key.clone()), "duplicate object visual review");
            ensure!(
                required
                    .iter()
                    .any(|v| v["page"] == r["page"] && v["object_id"] == r["object_id"]),
                "visual review names an unknown page/object"
            );
            stored.insert(key, r.clone());
        }
    }
    let mut unknown = 0;
    let mut defects = 0;
    let mut symbol_unknown = 0;
    let mut symbol_defects = 0;
    let mut details = Vec::new();
    let mut reviews = Vec::new();
    for item in &required {
        let checks: Vec<_> = item["checks_required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        let r = stored.get(&(item["page"].to_string(), item["object_id"].to_string()));
        let current = r.is_some_and(|r| r["evidence_hash"] == item["evidence_hash"]);
        let mut missing = Vec::new();
        let mut failed = Vec::new();
        if let Some(r) = r.filter(|_| current) {
            for check in &checks {
                let v = &r["checks"][check];
                let observation = v["observation"]
                    .as_str()
                    .is_some_and(|s| !s.trim().is_empty());
                match v["status"].as_str() {
                    Some("pass") if observation => {}
                    Some("defect") if observation => failed.push(*check),
                    _ => missing.push(*check),
                }
            }
            reviews.push(r.clone());
        } else {
            missing.extend(checks);
        }
        if !missing.is_empty() {
            unknown += 1;
            if item["kind"] == "artwork_symbol" {
                symbol_unknown += 1;
            }
        }
        if !failed.is_empty() {
            defects += 1;
            if item["kind"] == "artwork_symbol" {
                symbol_defects += 1;
            }
        }
        details.push(json!({"page":item["page"],"object_id":item["object_id"],
            "kind":item["kind"],
            "evidence_hash":item["evidence_hash"],"current_review":current,
            "unknown_checks":missing,"defect_checks":failed}));
    }
    Ok(Assessment {
        unknown,
        defects,
        reviews,
        summary: json!({"algorithm":ALGORITHM,
        "required_objects":required.len(),"unknown_objects":unknown,"defect_objects":defects,
        "symbol_unknown":symbol_unknown,"symbol_defects":symbol_defects,
        "objects":details,"visual_quality_verified_by_tool":false,
        "next":"Use review_pack; read each matched contextual image group with your own vision. Record the checks_required for each object. Artwork symbols require equivalence, one visible occurrence, and visibility, not typography. Missing or stale observations remain unknown; machine failures remain authoritative."}),
    })
}

/// Write editable, deliberately unreviewed answer scaffolding; never fabricate passes.
pub fn template(ctx: &Context, cycle: &std::path::Path, objects: &[Value]) -> Result<Value> {
    let cycle_digest = crate::fs::sha_file(cycle)?;
    let receipt_rows: Vec<_> = objects
        .iter()
        .map(|v| json!([v["page"], v["object_id"], v["evidence_hash"]]))
        .collect();
    Ok(
        json!({"cycle_sha256":cycle_digest,"review_id":review_id(&cycle_digest,&receipt_rows)?,"object_reviews":objects.iter().map(|v| {
        let checks:serde_json::Map<String,Value> = v["checks_required"].as_array().unwrap().iter().map(|k|(k.as_str().unwrap().to_string(),json!({"status":"unknown","observation":""}))).collect();
        json!({"page":v["page"],"object_id":v["object_id"],"evidence_hash":v["evidence_hash"],"checks":checks})
    }).collect::<Vec<_>>(),"note":"Merge these observations into the existing cycle review; keep comparison findings and full-page coverage. This is not a completed review.","workspace":ctx.root}),
    )
}
