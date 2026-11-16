//! Deterministic same-cycle aggregation. The coordinator authors no judgments.
use crate::{Context, string};
use anyhow::{Context as _, Result, ensure};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub const SCHEMA: &str = "pptx-review-merge-v1";

fn insert(rows: &mut BTreeMap<String, Value>, key: String, row: &Value) -> Result<()> {
    if let Some(old) = rows.get(&key) {
        ensure!(
            old == row,
            "conflicting review observations for {key}; resolve them through the independent review route before merging"
        );
    } else {
        rows.insert(key, row.clone());
    }
    Ok(())
}

/// Derive every authoritative field from independently attested inputs. Only
/// direct review_submit receipts are accepted, so merge chains cannot recurse.
pub(crate) fn derive(
    ctx: &Context,
    policy: &Value,
    cycle: &Value,
    sources: &Value,
) -> Result<(Value, Option<Value>)> {
    let sources = sources
        .as_array()
        .filter(|v| !v.is_empty() && v.len() <= 512)
        .context("merge requires 1..512 bound native reviews")?;
    let mut fields = BTreeMap::<String, BTreeMap<String, Value>>::new();
    let mut inspected = BTreeMap::<String, BTreeMap<String, Value>>::new();
    let mut relation_pages = BTreeMap::new();
    let mut has_relations = false;
    let mut comparisons = None;
    for binding in sources {
        let review = ctx.json(ctx.verify(binding)?)?;
        let receipt = ctx.json(ctx.verify(&review["attestation"])?)?;
        ensure!(
            receipt["schema"] == "pptx-review-attestation-v1",
            "merge inputs must be direct review_submit receipts, not scores or other merges"
        );
        ensure!(
            review["cycle_sha256"] == cycle["sha256"]
                && crate::review_routes::authorized(ctx, policy, &review, cycle)?,
            "merge source is stale or not from the frozen independent reviewer"
        );
        for (field, keys) in [
            ("object_reviews", &["page", "object_id"][..]),
            ("comparisons", &["page"][..]),
            ("relation_reviews", &["id"][..]),
            ("measurement_reviews", &["id"][..]),
            ("findings", &["id"][..]),
        ] {
            if let Some(rows) = review.get(field) {
                for row in rows.as_array().context("review rows must be arrays")? {
                    let mut key = Vec::new();
                    for k in keys {
                        key.push(string(row, k)?);
                    }
                    insert(
                        fields.entry(field.into()).or_default(),
                        json!(key).to_string(),
                        row,
                    )?;
                }
            }
        }
        if let Some(coverage) = review.get("inspected") {
            for (domain, pages) in coverage.as_object().context("invalid inspected coverage")? {
                ensure!(
                    ["copy", "artwork", "readability"].contains(&domain.as_str()),
                    "unknown coverage domain"
                );
                for page in pages.as_array().context("coverage must be an array")? {
                    ensure!(page.is_string(), "coverage must name source page IDs");
                    insert(
                        inspected.entry(domain.clone()).or_default(),
                        page.to_string(),
                        page,
                    )?;
                }
            }
        }
        if let Some(binding) = review.get("comparison_manifest") {
            ctx.verify(binding)?;
            ensure!(
                comparisons.as_ref().is_none_or(|old| old == binding),
                "comparison manifests differ"
            );
            comparisons = Some(binding.clone());
        }
        if let Some(binding) = review.get("relations") {
            let report = ctx.json(ctx.verify(binding)?)?;
            ensure!(
                report["algorithm"] == crate::relations::ALGORITHM && report["cycle"] == *cycle,
                "relations belong to a different cycle"
            );
            has_relations = true;
            for page in report["pages"]
                .as_array()
                .context("invalid relation pages")?
            {
                insert(&mut relation_pages, string(page, "page")?.to_owned(), page)?;
            }
        }
    }
    let mut body = json!({"cycle_sha256":cycle["sha256"],
        "review_id":crate::object_review::review_id(string(cycle,"sha256")?, sources)?,
        "object_reviews":[],"comparisons":[],"measurement_reviews":[],"findings":[],"inspected":{}});
    for (field, rows) in fields {
        body[field] = json!(rows.into_values().collect::<Vec<_>>());
    }
    for (domain, pages) in inspected {
        body["inspected"][domain] = json!(pages.into_values().collect::<Vec<_>>());
    }
    if let Some(manifest) = comparisons {
        body["comparison_manifest"] = manifest;
    }
    let relations = has_relations.then(|| json!({"algorithm":crate::relations::ALGORITHM,
        "cycle":cycle,"pages":relation_pages.into_values().collect::<Vec<_>>(),"quality_approved":false}));
    Ok((body, relations))
}

pub fn merge(ctx: &Context, request: &Value) -> Result<Value> {
    let cycle_path = ctx.path(string(request, "cycle")?)?;
    crate::review::bound_cycle(ctx, &cycle_path)?;
    let cycle = ctx.binding(&cycle_path)?;
    let policy = ctx.json(".octos/continuation.json")?;
    let sources: Vec<_> = request["reviews"]
        .as_array()
        .filter(|paths| !paths.is_empty() && paths.len() <= 512)
        .context("reviews must contain 1..512 paths")?
        .iter()
        .map(|p| ctx.binding(p.as_str().context("review must be a path")?))
        .collect::<Result<_>>()?;
    let sources = json!(sources);
    let (mut body, relations) = derive(ctx, &policy, &cycle, &sources)?;
    let output = ctx.path(string(request, "output")?)?;
    ensure!(!output.exists(), "use a fresh merged review output");
    if let Some(report) = relations {
        let key = crate::fs::digest(&serde_json::to_vec(&report)?);
        let path = format!(".octos/pptx-merged-relations/{key}.json");
        if ctx.path(&path)?.exists() {
            ensure!(
                ctx.json(&path)? == report,
                "merged relation report was modified"
            );
        } else {
            ctx.write_json(&path, &report)?;
        }
        body["relations"] = ctx.binding(path)?;
    }
    let receipt = json!({"schema":SCHEMA,"cycle":cycle,"source_reviews":sources,
        "review_sha256":crate::fs::digest(&serde_json::to_vec(&body)?)});
    let key = crate::fs::digest(&serde_json::to_vec(&receipt)?);
    let receipt_path = format!(".octos/pptx-review-attestations/{key}.json");
    if ctx.path(&receipt_path)?.exists() {
        ensure!(
            ctx.json(&receipt_path)? == receipt,
            "merge receipt was modified"
        );
    } else {
        ctx.write_json(&receipt_path, &receipt)?;
    }
    body["attestation"] = ctx.binding(receipt_path)?;
    let path = ctx.write_json(output, &body)?;
    Ok(
        json!({"review":ctx.binding(path)?,"sources":sources,"quality_approved":false,
        "next":"Record this merged evidence with evidence_record. Missing or unknown observations remain incomplete; this operation performs no visual judgment."}),
    )
}

/// Recompute the merge on every authorization check; a copied coordinator
/// receipt cannot authorize added passes, dropped defects, or changed sources.
pub(crate) fn validate(
    ctx: &Context,
    policy: &Value,
    body: &Value,
    receipt: &Value,
    cycle: &Value,
) -> Result<()> {
    let (mut expected, relations) = derive(ctx, policy, cycle, &receipt["source_reviews"])?;
    if let Some(report) = relations {
        ensure!(
            ctx.json(ctx.verify(&body["relations"])?)? == report,
            "merged relations were changed"
        );
        expected["relations"] = body["relations"].clone();
    }
    ensure!(
        expected == *body,
        "merged observations differ from their attested sources"
    );
    Ok(())
}

/// Include the source provenance in the final continuation snapshot so edits
/// after evidence_record are caught by the host as stale bindings too.
pub(crate) fn dependencies(ctx: &Context, review: &Value) -> Result<Vec<Value>> {
    if !review["attestation"].is_object() {
        return Ok(Vec::new());
    }
    let receipt = ctx.json(ctx.verify(&review["attestation"])?)?;
    let mut result = Vec::new();
    if receipt["schema"] == SCHEMA {
        for binding in receipt["source_reviews"]
            .as_array()
            .context("missing merge sources")?
        {
            let source = ctx.json(ctx.verify(binding)?)?;
            result.push(binding.clone());
            for key in ["attestation", "relations", "comparison_manifest"] {
                if source[key].is_object() {
                    ctx.verify(&source[key])?;
                    result.push(source[key].clone());
                }
            }
        }
    }
    Ok(result)
}
