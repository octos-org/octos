//! Rebind existing observations only after comparing the actual local pixels,
//! declarations and neighbours. Never generate a new visual verdict.
use crate::{Context, geometry, object_review, review};
use anyhow::Context as _;
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::collections::BTreeMap;

fn signal_body(v: &Value) -> Option<Value> {
    let mut s = v.clone();
    s.as_object_mut()?.remove("id");
    if let Some(o) = s["observed"].as_object_mut() {
        o.remove("blank");
    }
    Some(s)
}
fn context_signature(
    ctx: &Context,
    item: &Value,
    inventory: &[Value],
    spec: &crate::spec::Spec,
    pixels: &mut BTreeMap<String, image::RgbImage>,
) -> Result<Value> {
    let a: [f64; 4] = serde_json::from_value(item["viewport_normalized_xyxy"].clone())?;
    let mut sources = Vec::new();
    for src in item["sources"]
        .as_array()
        .context("review_reuse::context_signature: missing required value")?
    {
        let id = src["image"].to_string();
        if !pixels.contains_key(&id) {
            pixels.insert(
                id.clone(),
                crate::review::load_image(ctx, &ctx.verify(&src["image"])?)?.to_rgb8(),
            );
        }
        let im = &pixels[&id];
        let (w, h) = im.dimensions();
        let box_ = [
            a[0] * w as f64,
            a[1] * h as f64,
            (a[2] - a[0]) * w as f64,
            (a[3] - a[1]) * h as f64,
        ];
        let key = crate::finding_guard::pixel_digest(im, box_)?;
        sources.push(json!([src["role"], key]));
    }
    let mut neighbours = Vec::new();
    for n in inventory.iter().filter(|n| n["page"] == item["page"]) {
        let b = geometry::rect(&n["box"])?;
        if b[0] / spec.canvas.width_px >= a[2]
            || (b[0] + b[2]) / spec.canvas.width_px <= a[0]
            || b[1] / spec.canvas.height_px >= a[3]
            || (b[1] + b[3]) / spec.canvas.height_px <= a[1]
        {
            continue;
        }
        let mut n = n.clone();
        for key in ["sources", "evidence_hash", "page_number"] {
            n.as_object_mut()
                .context("review_reuse::context_signature: missing required value")?
                .remove(key);
        }
        let page = spec
            .pages
            .iter()
            .find(|p| n["page"] == p.id)
            .context("review_reuse::context_signature: missing required value")?;
        n["owner_declaration"] = n["declared_owner"]
            .as_str()
            .and_then(|id| page.regions.get(id))
            .cloned()
            .unwrap_or(Value::Null);
        neighbours.push(n);
    }
    Ok(json!({"viewport":a,"sources":sources,"neighbours":neighbours}))
}

pub fn collect(
    ctx: &Context,
    cycle_path: &std::path::Path,
    previous_cycle: &str,
    previous_review: &str,
    signals: &[Value],
) -> Result<Value> {
    let (cycle, spec) = review::bound_cycle(ctx, cycle_path)?;
    let (old, old_spec) = review::bound_cycle(ctx, &ctx.path(previous_cycle)?)?;
    let prior = ctx.json(previous_review)?;
    ensure!(
        prior["cycle_sha256"] == ctx.binding(previous_cycle)?["sha256"],
        "previous review belongs to a different cycle"
    );
    let prior_independent = if ctx.path(".octos/continuation.json")?.exists() {
        crate::review_routes::authorized(
            ctx,
            &ctx.json(".octos/continuation.json")?,
            &prior,
            &ctx.binding(previous_cycle)?,
        )?
    } else {
        false
    };
    ensure!(
        serde_json::to_value(&spec.canvas)? == serde_json::to_value(&old_spec.canvas)?,
        "review reuse requires unchanged canvas"
    );
    let old_items = object_review::inventory(&old, &old_spec)?;
    let items = object_review::inventory(&cycle, &spec)?;
    let old_signals = if prior["relations"].is_object() {
        let rel = ctx.json(ctx.verify(&prior["relations"])?)?;
        ensure!(
            rel["cycle"]["sha256"] == prior["cycle_sha256"],
            "stale previous relation evidence"
        );
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
    let mut reused = Vec::new();
    let mut relations = Vec::new();
    let mut pixels = BTreeMap::new();
    let mut cache_page = Value::Null;
    let fidelity_enabled = crate::fidelity::enabled(&crate::review_routes::load_plan(ctx)?);
    for item in &items {
        if cache_page != item["page"] {
            pixels.clear();
            cache_page = item["page"].clone();
        }
        let Some(previous) = old_items
            .iter()
            .find(|v| v["page"] == item["page"] && v["object_id"] == item["object_id"])
        else {
            continue;
        };
        let Some(answer) = prior["object_reviews"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|v| {
                v["page"] == item["page"]
                    && v["object_id"] == item["object_id"]
                    && v["evidence_hash"] == previous["evidence_hash"]
            })
        else {
            continue;
        };
        if !item["checks_required"]
            .as_array()
            .context("review_reuse::collect: missing required value")?
            .iter()
            .all(|c| {
                c.as_str()
                    .is_some_and(|name| answer["checks"][name]["status"] == "pass")
            })
        {
            continue;
        }
        let fidelity_required = item["design_available"] == true && fidelity_enabled;
        if fidelity_required && crate::fidelity::object_result(previous, answer).0 != "pass" {
            continue; // Old all-green legibility is not new-contract fidelity.
        }
        if item["viewport_normalized_xyxy"] != previous["viewport_normalized_xyxy"]
            || context_signature(ctx, item, &items, &spec, &mut pixels)?
                != context_signature(ctx, previous, &old_items, &old_spec, &mut pixels)?
        {
            continue;
        }
        let mut local = Vec::new();
        let mut valid = true;
        for s in signals
            .iter()
            .filter(|s| s["page"] == item["page"] && s["object_id"] == item["object_id"])
        {
            let response = old_signals
                .iter()
                .find(|o| signal_body(o).is_some_and(|body| signal_body(s).as_ref() == Some(&body)))
                .and_then(|o| {
                    prior["relation_reviews"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .find(|r| {
                            r["id"] == o["id"]
                                && (r["status"] == "intentional"
                                    || (prior_independent && r["status"] == "visual_acceptance"))
                        })
                });
            if let Some(r) = response {
                let mut r = r.clone();
                r["id"] = s["id"].clone();
                local.push(r);
            } else {
                valid = false;
                break;
            }
        }
        if valid {
            let mut row = answer.clone();
            row["evidence_hash"] = item["evidence_hash"].clone();
            if fidelity_required {
                // Only reached after exact local pixels and neighbours match.
                row["fidelity"]["evidence"] = crate::fidelity::evidence(item);
            }
            reused.push(row);
            relations.extend(local);
        }
    }
    let mut comparisons = Vec::new();
    for p in prior["comparisons"].as_array().into_iter().flatten() {
        let i = spec.pages.iter().position(|s| p["page"] == s.id);
        let j = old_spec.pages.iter().position(|s| p["page"] == s.id);
        if let (Some(i), Some(j)) = (i, j)
            && cycle["renders"]["editable"]["pages"][i]["sha256"]
                == old["renders"]["editable"]["pages"][j]["sha256"]
            && spec.pages[i].background["sha256"] == old_spec.pages[j].background["sha256"]
            && spec.pages[i].design == old_spec.pages[j].design
        {
            comparisons.push(p.clone());
        }
    }
    let findings: Vec<_> = prior["findings"]
        .as_array()
        .into_iter()
        .flatten()
        .cloned()
        .map(|mut f| {
            // The registration binds before pixels. A fresh observation without an
            // explicit render path must resolve against the current cycle.
            f.as_object_mut()
                .context("review_reuse::collect: missing required value")?
                .remove("evidence");
            Ok(f)
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(
        json!({"previous_cycle":ctx.binding(previous_cycle)?,"previous_review":ctx.binding(previous_review)?,
        "object_reviews":reused,"relation_reviews":relations,"comparisons":comparisons,
        "findings":findings,
        "quality_approved":false,"basis":"Identical original/blank/current local pixels, viewport, local declarations, neighbours and relation candidates; no new verdict generated."}),
    )
}
pub fn merge(observations: &mut Value, reuse: &Value) -> Result<()> {
    for (field, keys) in [
        ("object_reviews", vec!["page", "object_id"]),
        ("relation_reviews", vec!["id"]),
        ("comparisons", vec!["page"]),
    ] {
        let key = |r: &Value| {
            keys.iter()
                .map(|k| r[*k].to_string())
                .collect::<Vec<_>>()
                .join("|")
        };
        let mut map = BTreeMap::new();
        for row in reuse[field].as_array().into_iter().flatten() {
            map.insert(key(row), row.clone());
        }
        for row in observations[field].as_array().into_iter().flatten() {
            map.insert(key(row), row.clone());
        }
        observations[field] = json!(map.into_values().collect::<Vec<_>>());
    }
    // Carry metadata, not a new verdict. A resolved row does not need to
    // transcribe receipt hashes or the original viewport from its predecessor.
    let mut findings = BTreeMap::new();
    for old in reuse["findings"].as_array().into_iter().flatten() {
        findings.insert(crate::string(old, "id")?.to_owned(), old.clone());
    }
    let mut seen = std::collections::BTreeSet::new();
    for row in observations["findings"].as_array().into_iter().flatten() {
        let id = crate::string(row, "id")?;
        ensure!(seen.insert(id), "duplicate finding disposition");
        let mut next = row.clone();
        if let Some(old) = findings.get(id) {
            for key in ["page", "object_id", "check", "box", "registration"] {
                if old.get(key).is_none() {
                    continue;
                }
                if let Some(value) = next.get(key) {
                    ensure!(
                        *value == old[key],
                        "finding {id} changed its {key}; preserve the original registered identity/viewport"
                    );
                } else {
                    next[key] = old[key].clone();
                }
            }
        }
        findings.insert(id.to_owned(), next);
    }
    observations["findings"] = json!(findings.into_values().collect::<Vec<_>>());
    Ok(())
}
