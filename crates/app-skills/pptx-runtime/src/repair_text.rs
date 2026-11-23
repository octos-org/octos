//! Page-local evidence for repairs to specifically identified editable text.
use crate::{Context, deck, fs, repair_gate};
use anyhow::{Context as _, Result, ensure};
use deckcraft_engine::model::{Shape, ShapeKind};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

type Targets = BTreeMap<String, String>;
type Actual = BTreeMap<String, Vec<String>>;

fn targets(review: &Value) -> Result<BTreeMap<String, Targets>> {
    let mut groups = BTreeMap::new();
    let mut expected = BTreeMap::new();
    for finding in repair_gate::findings(review) {
        let Some(labels) = finding.get("native_text") else {
            continue;
        };
        ensure!(
            finding["kind"] == "requirement",
            "native_text needs a frozen requirement"
        );
        let labels: Targets = serde_json::from_value(labels.clone()).context(
            "native_text must map stable text object IDs to their exact expected strings",
        )?;
        ensure!(!labels.is_empty(), "native_text has no expected objects");
        for (id, text) in &labels {
            ensure!(
                !id.trim().is_empty() && !text.trim().is_empty(),
                "native_text needs nonempty IDs and text"
            );
            ensure!(
                expected
                    .insert(id.clone(), text.clone())
                    .is_none_or(|prior| prior == *text),
                "conflicting required text for object {id}"
            );
        }
        groups.insert(crate::string(finding, "id")?.to_owned(), labels);
    }
    Ok(groups)
}

fn read_objects(ctx: &Context, artifact: &Value, ids: BTreeSet<&String>) -> Result<Actual> {
    fn collect(shapes: &[Shape], actual: &mut Actual) {
        for shape in shapes {
            if let Some(id) = shape.name.strip_prefix("octos:text:")
                && let Some(values) = actual.get_mut(id)
                && let Some(text) = &shape.text
            {
                values.push(text.text());
            }
            if let ShapeKind::Group { children, .. } = &shape.kind {
                collect(children, actual);
            }
        }
    }
    let path = ctx.path(crate::string(artifact, "path")?)?;
    let bytes = ctx.read(&path)?;
    ensure!(
        artifact["sha256"] == fs::digest(&bytes),
        "repair PPTX changed"
    );
    let pptx = deck::open_bytes(&path, &bytes)?;
    ensure!(
        pptx.slides.len() == 1,
        "repair inspection requires one page"
    );
    let mut actual: Actual = ids.into_iter().map(|id| (id.clone(), Vec::new())).collect();
    collect(&pptx.slides[0].shapes, &mut actual);
    for values in actual.values_mut() {
        values.sort();
    }
    Ok(actual)
}

fn matches(values: &[String], text: &str) -> bool {
    matches!(values, [one] if one == text)
}

/// Freeze the measured original alongside the independent rejection.
pub(crate) fn baseline(ctx: &Context, candidate: &Value, review: &Value) -> Result<Option<Value>> {
    let groups = targets(review)?;
    if groups.is_empty() {
        return Ok(None);
    }
    let actual = read_objects(
        ctx,
        &candidate["artifact"],
        groups.values().flat_map(|g| g.keys()).collect(),
    )?;
    for (finding, labels) in &groups {
        ensure!(
            labels.iter().any(|(id, text)| !matches(&actual[id], text)),
            "native text already meets finding {finding}"
        );
    }
    Ok(Some(json!(actual)))
}

pub(crate) fn directory(binding: &Value) -> Result<String> {
    Ok(format!(
        ".octos/page-repair-observations/{}",
        crate::string(binding, "sha256")?
    ))
}

/// One compact receipt per original verdict and proposed artifact. An identical
/// publication retry reuses its receipt. No deck-wide state or peer call.
pub(crate) fn observe(
    ctx: &Context,
    review: &Value,
    binding: &Value,
    candidate: &Value,
) -> Result<Value> {
    let Some(original) = review.get("native_text_baseline") else {
        return Ok(Value::Null);
    };
    let baseline: Actual = serde_json::from_value(original.clone())?;
    let dir = ctx.path(directory(binding)?)?;
    let sha = crate::string(&candidate["artifact"], "sha256")?;
    let mut receipts = Vec::new();
    if dir.try_exists()? {
        for entry in std::fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.extension().is_some_and(|ext| ext == "json") {
                receipts.push(path);
            }
        }
    }
    receipts.sort_unstable();
    let suffix = format!("-{sha}.json");
    let existing = receipts.iter().position(|p| {
        p.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.ends_with(&suffix))
    });
    let index = existing.unwrap_or(receipts.len());
    let last = index
        .checked_sub(1)
        .map(|i| ctx.json(&receipts[i]))
        .transpose()?;
    let previous: Actual = match last {
        Some(record) => serde_json::from_value(record["actual"].clone())?,
        None => baseline.clone(),
    };
    let groups = targets(review)?;
    let actual = read_objects(
        ctx,
        &candidate["artifact"],
        groups.values().flat_map(|g| g.keys()).collect(),
    )?;
    let mut findings = BTreeMap::new();
    let mut met = true;
    for (finding, labels) in groups {
        let mut changed = Vec::new();
        let mut this_attempt = Vec::new();
        let mut pending = Vec::new();
        for (id, text) in &labels {
            if baseline.get(id) != actual.get(id) {
                changed.push(id);
            }
            if previous.get(id) != actual.get(id) {
                this_attempt.push(id);
            }
            if !matches(&actual[id], text) {
                pending.push(id);
            }
        }
        met &= pending.is_empty();
        findings.insert(
            finding,
            json!({"changed_from_reviewed":changed,"changed_this_attempt":this_attempt,"remaining":pending}),
        );
    }
    let result = json!({"proposal":index + 1,"page":review["page_id"],"verdict_sha256":binding["sha256"],
        "original_artifact_sha256":review["artifact"]["sha256"],"proposed_artifact_sha256":candidate["artifact"]["sha256"],
        "actual":actual,"findings":findings,"native_text_met":met});
    let path = dir.join(format!("{:012}-{sha}.json", index + 1));
    if existing.is_some() {
        ensure!(
            ctx.json(&path)? == result,
            "recorded repair observation differs"
        );
    } else {
        ctx.replace_json(&path, &result)?;
    }
    Ok(json!({"receipt":ctx.binding(path)?,"native_text_met":met,"findings":result["findings"]}))
}
