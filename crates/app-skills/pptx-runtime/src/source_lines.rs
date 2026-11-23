//! Source-line identities belong to the original page, independently of any candidate.
use crate::{Context, fs::digest, geometry, spec::Page, string};
use anyhow::{Context as _, Result, ensure};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

fn text_key(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

fn index(rows: &[Value]) -> Result<BTreeMap<String, Value>> {
    let mut result = BTreeMap::new();
    for row in rows {
        let id = if let Some(id) = row.get("id") {
            id.as_str()
                .filter(|s| !s.trim().is_empty())
                .context("invalid source line ID")?
                .to_owned()
        } else {
            // IDs must survive transcript corrections, array order and object renames.
            let bounds = geometry::rect(&row["box"])?;
            let key = digest(&serde_json::to_vec(&bounds)?);
            format!("line-{}", &key[..20])
        };
        ensure!(
            id.len() <= 256 && row["text"].is_string(),
            "source line {id} needs a transcript (empty if unreadable)"
        );
        let mut line = row.clone();
        line["id"] = json!(id);
        ensure!(
            result.insert(id.clone(), line).is_none(),
            "duplicate source line {id}; distinguish coincident detections explicitly"
        );
    }
    Ok(result)
}

/// Serialize admission per original page. A reader receives an immutable snapshot,
/// so adding a missed line never changes the evidence for an already rendered deck.
pub(crate) fn register(
    ctx: &Context,
    page: &str,
    original: &Value,
    observed: Option<&Value>,
    additions: &[Value],
) -> Result<Option<Value>> {
    let hash = string(original, "sha256")?;
    let key = digest(&serde_json::to_vec(&(page, hash))?);
    let dir = ctx.path(format!(".octos/pptx-source-lines/{key}"))?;
    let latest = dir.join("current.json");
    if observed.is_none() && additions.is_empty() {
        if !latest.try_exists()? {
            return Ok(None);
        }
        // The writer atomically replaces this pointer after committing the immutable snapshot.
        let binding = ctx.json(latest)?;
        ctx.verify(&binding)?;
        return Ok(Some(binding));
    }
    std::fs::create_dir_all(&dir)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(ctx.path(dir.join("lock"))?)?;
    fs2::FileExt::lock_exclusive(&lock)?;
    let previous = if latest.exists() {
        let binding = ctx.json(&latest)?;
        Some(ctx.json(ctx.verify(&binding)?)?)
    } else {
        None
    };
    let seed = previous
        .as_ref()
        .or(observed)
        .context("source lines require a registered input with a canvas")?;
    let canvas: [f64; 2] = serde_json::from_value(seed["canvas"].clone())?;
    ensure!(
        canvas.iter().all(|n| n.is_finite() && *n > 0.),
        "invalid source canvas"
    );
    let mut rows = BTreeMap::new();
    for data in previous.iter().chain(observed) {
        ensure!(
            data["source_sha256"] == hash
                && serde_json::from_value::<[f64; 2]>(data["canvas"].clone())? == canvas,
            "source line register design or canvas mismatch"
        );
        for (id, row) in index(data["lines"].as_array().context("missing source lines")?)? {
            if let Some(old) = rows.insert(id.clone(), row.clone()) {
                ensure!(
                    old == row,
                    "conflicting evidence for source line {id}; retain its original reading and correct the authored text"
                );
            }
        }
    }
    for (id, row) in index(additions)? {
        let [x, y, w, h] = geometry::rect(&row["box"])?;
        ensure!(
            x >= 0. && y >= 0. && x + w <= canvas[0] + 0.01 && y + h <= canvas[1] + 0.01,
            "added source line outside original"
        );
        if let Some(old) = rows.insert(id.clone(), row.clone()) {
            ensure!(old == row, "conflicting additional source line {id}");
        }
    }
    ensure!(rows.len() <= 10000, "too many source lines");
    let snapshot = json!({"schema":"octos-source-lines-v1","source_sha256":hash,"canvas":canvas,"lines":rows.into_values().collect::<Vec<_>>()});
    let bytes = serde_json::to_vec(&snapshot)?;
    let path = dir.join(format!("{}.json", digest(&bytes)));
    if !path.exists() {
        ctx.write_new(&path, &bytes)?;
    }
    let binding = ctx.binding(&path)?;
    ensure!(
        binding["sha256"] == digest(&bytes),
        "source line snapshot changed"
    );
    if previous.as_ref() != Some(&snapshot) {
        ctx.replace_json(latest, &binding)?;
    }
    Ok(Some(binding))
}

fn page_pin(page: &str) -> String {
    format!(
        ".octos/pptx-source-lines/pages/{}.json",
        digest(page.as_bytes())
    )
}

/// A peer may omit the design in a repair draft. Restore the original admitted
/// by intake, and reject an attempted substitution for the same task page.
pub(crate) fn original(
    ctx: &Context,
    page: &str,
    declared: Option<&Value>,
) -> Result<Option<Value>> {
    let path = ctx.path(page_pin(page))?;
    if !path.try_exists()? {
        return Ok(None);
    }
    let mut bound = ctx.json(path)?;
    if let Some(design) = declared {
        ensure!(
            design["sha256"] == bound["sha256"],
            "different original for registered page {page}"
        );
    } else {
        bound["path"] = json!(ctx.verify(&bound)?);
    }
    Ok(Some(bound))
}

pub(crate) fn intake(ctx: &Context, r: &Value) -> Result<Value> {
    let design = ctx.binding(string(r, "design")?)?;
    let page = string(r, "page")?;
    let pinned = original(ctx, page, Some(&design))?.is_some();
    let mut data = r
        .get("ocr")
        .map(|v| ctx.json(v.as_str().context("ocr must be a workspace file")?))
        .transpose()?;
    let additions = r
        .get("additional_lines")
        .map(|v| v.as_array().context("additional_lines must be an array"))
        .transpose()?;
    // A reviewer may discover a line on a source for which OCR found nothing.
    if data.is_none()
        && additions.is_some_and(|rows| !rows.is_empty())
        && register(ctx, string(r, "page")?, &design, None, &[])?.is_none()
    {
        let (w, h) = image::image_dimensions(ctx.verify(&design)?)?;
        data = Some(json!({"source_sha256":design["sha256"],"canvas":[w,h],"lines":[]}));
    }
    let binding = register(
        ctx,
        string(r, "page")?,
        &design,
        data.as_ref(),
        additions.map(Vec::as_slice).unwrap_or_default(),
    )?
    .context("supply the source OCR file on first admission")?;
    if !pinned && let Err(error) = ctx.write_json(page_pin(page), &design) {
        if original(ctx, page, Some(&design))?.is_none() {
            return Err(error);
        }
    }
    Ok(
        json!({"source_ocr":binding,"lines":ctx.json(ctx.verify(&binding)?)?["lines"],"quality_approved":false}),
    )
}

/// Make implicit legacy matches explicit before a patch, split or build changes the text.
pub(crate) fn link(ctx: &Context, page: &mut Page) -> Result<()> {
    let Some(binding) = &page.source_ocr else {
        ensure!(
            page.texts.iter().all(|t| t.source_line_ids.is_empty()),
            "source_line_ids require a registered original"
        );
        return Ok(());
    };
    let data = ctx.json(ctx.verify(binding)?)?;
    let rows = index(
        data["lines"]
            .as_array()
            .context("missing registered lines")?,
    )?;
    let mut by_text: BTreeMap<String, Vec<&String>> = BTreeMap::new();
    for (id, r) in &rows {
        by_text
            .entry(text_key(string(r, "text").unwrap_or("")))
            .or_default()
            .push(id);
    }
    let mut counts = BTreeMap::new();
    let mut reserved = BTreeSet::new();
    let mut named = BTreeSet::new();
    for t in &page.texts {
        *counts.entry(text_key(&t.text)).or_insert(0usize) += 1;
        reserved.extend(t.source_line_ids.iter().cloned());
        if t.source_line_ids.is_empty() {
            named.insert(t.copy_id.as_ref().unwrap_or(&t.id).clone());
        }
    }
    let mut owners = BTreeMap::new();
    for t in &mut page.texts {
        let copy = t.copy_id.as_ref().unwrap_or(&t.id);
        if t.source_line_ids.is_empty() {
            if rows.contains_key(copy) && !reserved.contains(copy) {
                t.source_line_ids.push(copy.clone());
            } else if t.copy_range.is_none() {
                let k = text_key(&t.text);
                if let Some([id]) = by_text.get(&k).map(Vec::as_slice)
                    && counts[&k] == 1
                    && !reserved.contains(*id)
                    && !named.contains(*id)
                {
                    t.source_line_ids.push((*id).clone());
                }
            }
        }
        let mut seen = BTreeSet::new();
        for id in &t.source_line_ids {
            ensure!(
                rows.contains_key(id) && seen.insert(id),
                "unknown or duplicate source line {id} on {}",
                t.id
            );
            if let Some((prev, split)) =
                owners.insert(id.clone(), (copy.clone(), t.copy_range.is_some()))
            {
                ensure!(
                    prev == *copy && split && t.copy_range.is_some(),
                    "multiple independent objects claim source line {id}"
                );
            }
        }
    }
    Ok(())
}

pub(crate) fn inventory(ctx: &Context, spec: &crate::spec::Spec) -> Result<Vec<Value>> {
    let mut result = Vec::new();
    for p in &spec.pages {
        let Some(binding) = &p.source_ocr else {
            continue;
        };
        let data = ctx.json(ctx.verify(binding)?)?;
        let mut objects: BTreeMap<&str, Vec<Value>> = BTreeMap::new();
        for t in &p.texts {
            for id in &t.source_line_ids {
                objects
                    .entry(id)
                    .or_default()
                    .push(json!({"id":t.id,"text":t.text}));
            }
        }
        for line in data["lines"]
            .as_array()
            .context("missing registered lines")?
        {
            let id = string(line, "id")?;
            result.push(json!({"page":p.id,"source_sha256":data["source_sha256"],"source_line_id":id,
               "source":line,"canvas":data["canvas"],"objects":objects.remove(id).unwrap_or_default()}));
        }
    }
    Ok(result)
}

/// Assessment is scoped to this cycle. A previous candidate's answer never closes a line.
pub(crate) fn coverage(required: &[Value], review: &Value, independent: bool) -> Result<Value> {
    let mut remaining: BTreeMap<_, _> = required
        .iter()
        .map(|r| {
            (
                (
                    r["page"].as_str().unwrap_or(""),
                    r["source_line_id"].as_str().unwrap_or(""),
                ),
                r,
            )
        })
        .collect();
    let mut unresolved = Vec::new();
    let mut defects = 0;
    if let Some(entries) = review.get("source_line_reviews") {
        for row in entries
            .as_array()
            .context("source_line_reviews must be an array")?
        {
            let key = (string(row, "page")?, string(row, "source_line_id")?);
            let line = remaining
                .remove(&key)
                .with_context(|| format!("unknown or duplicate source line review {key:?}"))?;
            let status = string(row, "status")?;
            if status != "unknown" {
                ensure!(
                    !string(row, "observation")?.trim().is_empty(),
                    "describe this line in the original and current render"
                );
            }
            let linked = !line["objects"]
                .as_array()
                .context("invalid source line objects")?
                .is_empty();
            match status {
                "native" => {
                    ensure!(linked, "source line {key:?} has no linked editable text");
                    ensure!(
                        line["missing_objects"].as_array().is_none_or(Vec::is_empty),
                        "source line {key:?} is missing native fragments"
                    );
                    if !independent {
                        unresolved.push(key);
                    }
                }
                "not_text" | "illegible_artwork" => {
                    ensure!(
                        !linked,
                        "source line {key:?} still has a native transcription"
                    );
                    if !independent {
                        unresolved.push(key);
                    }
                }
                "defect" => {
                    defects += 1;
                    unresolved.push(key);
                }
                "unknown" => unresolved.push(key),
                _ => anyhow::bail!("invalid source line review status {status}"),
            }
        }
    }
    unresolved.extend(remaining.into_keys());
    Ok(
        json!({"total":required.len(),"unknown":unresolved.len()-defects,"defects":defects,"pending_ids":unresolved}),
    )
}

pub(crate) fn current_scope(
    ctx: &Context,
    spec: &crate::spec::Spec,
    rows: &[Value],
    previous: &Value,
) -> Result<Value> {
    let mut scope = BTreeSet::new();
    for row in rows {
        scope.insert((
            string(row, "page")?.to_owned(),
            string(row, "source_sha256")?.to_owned(),
            string(row, "source_line_id")?.to_owned(),
        ));
    }
    if !previous.is_null() {
        let old: BTreeSet<(String, String, String)> = serde_json::from_value(previous.clone())?;
        ensure!(
            old.is_subset(&scope),
            "repair lost registered source lines from the earlier attempt"
        );
    }
    for page in &spec.pages {
        let admitted = original(ctx, &page.id, page.design.as_ref())?;
        if let Some(original) = page.design.as_ref().or(admitted.as_ref())
            && let Some(latest) = register(ctx, &page.id, original, None, &[])?
        {
            let data = ctx.json(ctx.verify(&latest)?)?;
            for row in data["lines"]
                .as_array()
                .context("missing current source lines")?
            {
                ensure!(
                    scope.contains(&(
                        page.id.clone(),
                        string(original, "sha256")?.to_owned(),
                        string(row, "id")?.to_owned()
                    )),
                    "source line {} is absent from this candidate; rebuild with the current inventory",
                    row["id"]
                );
            }
        }
    }
    Ok(json!(scope))
}
