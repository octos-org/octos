//! Resolve each original line against the artifact the page peer is signing.
use crate::{Context, deck, source_lines, spec::Spec};
use anyhow::{Context as _, Result, ensure};
use deckcraft_engine::model::{Shape, ShapeKind};
use serde_json::{Value, json};
use std::collections::BTreeMap;

fn editable_text(shapes: &[Shape], out: &mut BTreeMap<String, Vec<String>>) {
    for shape in shapes {
        if shape.hidden
            || shape.locked
            || shape
                .partial_locks
                .iter()
                .any(|(key, v)| key == "noTextEdit" && *v)
        {
            continue;
        }
        if let ShapeKind::Group { children, .. } = &shape.kind {
            editable_text(children, out);
        }
        if let Some(body) = &shape.text
            && let Some(id) = shape.name.strip_prefix("octos:text:")
        {
            out.entry(id.to_owned()).or_default().push(body.text());
        }
    }
}

fn source_inventory(
    ctx: &Context,
    candidate: &Value,
    enforce_scope: bool,
) -> Result<(Vec<Value>, Value)> {
    let receipt = ctx.json(ctx.verify(&candidate["build_receipt"])?)?;
    let spec: Spec = serde_json::from_value(ctx.json(ctx.verify(&receipt["spec"])?)?)?;
    let rows = receipt["source_lines"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if spec.pages.iter().any(|p| p.source_ocr.is_some()) {
        ensure!(
            source_lines::inventory(ctx, &spec)? == rows,
            "source line register differs from the published build"
        );
    }
    if enforce_scope {
        source_lines::current_scope(ctx, &spec, &rows, &Value::Null)?;
    }
    Ok((rows, receipt["editable"]["sha256"].clone()))
}

// Polling a signed page only rechecks the original-line inventory. Its validated
// candidate digest already covers all post-build edits to the PPTX.
pub(crate) fn check_accepted_scope(ctx: &Context, candidate: &Value) -> Result<usize> {
    Ok(source_inventory(ctx, candidate, true)?.0.len())
}

fn current(ctx: &Context, candidate: &Value, enforce_scope: bool) -> Result<Vec<Value>> {
    let (mut rows, editable_digest) = source_inventory(ctx, candidate, enforce_scope)?;
    if rows.is_empty() || candidate["artifact"]["sha256"] == editable_digest {
        return Ok(rows);
    }
    let doc = deck::open(ctx, &ctx.verify(&candidate["artifact"])?)?;
    let mut actual = BTreeMap::new();
    editable_text(
        &doc.slides
            .first()
            .context("missing candidate slide")?
            .shapes,
        &mut actual,
    );
    for row in &mut rows {
        let objects = row["objects"]
            .as_array_mut()
            .context("missing mapped line objects")?;
        let mut missing = Vec::new();
        objects.retain_mut(|object| {
            let id = object["id"].as_str().unwrap_or("");
            if let Some([text]) = actual.get(id).map(Vec::as_slice)
                && !text.trim().is_empty()
            {
                object["text"] = json!(text);
                true
            } else {
                missing.push(id.to_owned());
                false
            }
        });
        if !missing.is_empty() {
            row["missing_objects"] = json!(missing);
        }
    }
    Ok(rows)
}

pub(crate) fn template(ctx: &Context, candidate: &Value, request: &mut Value) -> Result<()> {
    let mut rows = current(ctx, candidate, false)?;
    if !rows.is_empty() {
        for r in &mut rows {
            r["status"] = json!("unknown");
            r["observation"] = json!("");
        }
        request["verdict"]["source_line_reviews"] = json!(rows);
    }
    Ok(())
}

pub(crate) fn assess(ctx: &Context, candidate: &Value, verdict: &Value) -> Result<Value> {
    let accepting = verdict["accepted"] == true;
    let rows = current(ctx, candidate, accepting)?;
    let coverage = source_lines::coverage(&rows, verdict, true)?;
    if accepting {
        ensure!(
            coverage["unknown"] == 0 && coverage["defects"] == 0,
            "independent review has pending source lines: {}",
            coverage["pending_ids"]
        );
    }
    Ok(coverage)
}
