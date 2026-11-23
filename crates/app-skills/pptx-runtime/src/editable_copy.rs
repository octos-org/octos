//! Page-owned editable copy survives changes to the maker's spec and post-build edits.
use crate::{Context, deck, fs, page_gate, review_routes, spec};
use anyhow::{Context as _, Result, ensure};
use deckcraft_engine::model::{Presentation, Shape, ShapeKind};
use serde_json::Value;
use std::{collections::BTreeMap, path::PathBuf};
use unicode_segmentation::UnicodeSegmentation;

pub(crate) type Copy = BTreeMap<String, String>;

pub(crate) fn declared(row: &Value) -> Result<Option<Copy>> {
    let Some(value) = row.get("editable_copy") else {
        return Ok(None);
    };
    let copy: Copy = serde_json::from_value(value.clone())
        .context("editable_copy requires an occurrence ID and text for each entry")?;
    ensure!(
        copy.iter()
            .all(|(id, text)| !id.trim().is_empty() && !text.trim().is_empty()),
        "empty editable_copy id or text"
    );
    Ok(Some(copy))
}

pub(crate) fn measurements(row: &Value) -> Result<Option<&Value>> {
    if let Some(bound) = row["source_artwork"].get("measurements") {
        return Ok(Some(bound));
    }
    let mut inputs = row["inputs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|v| v["role"] == "measurements");
    let first = inputs.next();
    ensure!(
        inputs.next().is_none(),
        "ambiguous measurements for editable copy"
    );
    Ok(first)
}

fn baseline(ctx: &Context, row: &Value) -> Result<PathBuf> {
    let plan = review_routes::load_plan(ctx)?;
    let digest = fs::digest(&serde_json::to_vec(&(&plan["page_plan"], &row["id"]))?);
    ctx.path(format!(".octos/page-editable-copy/{digest}.json"))
}

/// Compatibility for existing image-reconstruction assignments. Pin only the
/// source occurrences independently corroborated by the assigned maker's first
/// native build. Unconfirmed OCR is not authoritative copy.
pub(crate) fn draft(ctx: &Context, pages: &[spec::Page], doc: &Presentation) -> Result<Copy> {
    let ([page], Some(owner)) = (pages, ctx.continuation_owner.as_deref()) else {
        return Ok(Copy::new());
    };
    if !review_routes::per_page(ctx)? {
        return Ok(Copy::new());
    }
    let plan = page_gate::plan(ctx)?;
    let Some(row) = plan["pages"]
        .as_array()
        .and_then(|rows| rows.iter().find(|r| r["id"] == page.id))
    else {
        return Ok(Copy::new());
    };
    if row["maker_session"].as_str().map(page_gate::identity) != Some(page_gate::identity(owner)) {
        return Ok(Copy::new());
    }
    if let Some(copy) = declared(row)? {
        return missing(&copy, doc, page);
    }
    let Some(bound) = measurements(row)? else {
        return Ok(Copy::new());
    };
    let path = baseline(ctx, row)?;
    if path.try_exists()? {
        return missing(&serde_json::from_value(ctx.json(path)?)?, doc, page);
    }
    let _lock = crate::page_verdict::lock_page(ctx, &page.id)?;
    if path.try_exists()? {
        return missing(&serde_json::from_value(ctx.json(path)?)?, doc, page);
    }
    // An already running older task may have a published page without a baseline.
    if ctx.path(crate::string(row, "candidate")?)?.try_exists()? {
        return Ok(Copy::new());
    }
    let copy = corroborated(ctx, bound, page)?;
    let absent = missing(&copy, doc, page)?;
    if absent.is_empty() {
        ctx.write_json(path, &serde_json::to_value(copy)?)?;
    }
    Ok(absent)
}

fn corroborated(ctx: &Context, bound: &Value, page: &spec::Page) -> Result<Copy> {
    let measured = ctx.json(ctx.verify(bound)?)?;
    let mut copy = Copy::new();
    for line in measured["lines"].as_array().into_iter().flatten() {
        let (Some(id), Some(source)) = (line["id"].as_str(), line["text"].as_str()) else {
            continue;
        };
        if let Some(text) = page.copy.get(id)
            && !text.trim().is_empty()
            && spec::normalized(text) == spec::normalized(source)
            && !page.artwork_symbols.iter().any(|s| s.copy_id == id)
        {
            copy.insert(id.to_owned(), text.clone());
        }
    }
    Ok(copy)
}

// Existing one-page artifacts can go directly to page_publish without a build
// in the assigned session. The validated publisher also pins that starting copy.
pub(crate) fn remember_publication(ctx: &Context, row: &Value, resolved: &Value) -> Result<()> {
    if row.get("editable_copy").is_some() || !review_routes::per_page(ctx)? {
        return Ok(());
    }
    let Some(bound) = measurements(row)? else {
        return Ok(());
    };
    let path = baseline(ctx, row)?;
    if !path.try_exists()? {
        let copy = corroborated(ctx, bound, &serde_json::from_value(resolved.clone())?)?;
        ctx.write_json(path, &serde_json::to_value(copy)?)?;
    }
    Ok(())
}

pub(crate) fn check(ctx: &Context, row: &Value, artifact: &Value, resolved: &Value) -> Result<()> {
    let copy = match declared(row)? {
        Some(copy) => copy,
        None => {
            let Some(bound) = measurements(row)? else {
                return Ok(());
            };
            if !review_routes::per_page(ctx)? {
                return Ok(());
            }
            let path = baseline(ctx, row)?;
            if path.try_exists()? {
                serde_json::from_value(ctx.json(path)?)?
            } else {
                corroborated(ctx, bound, &serde_json::from_value(resolved.clone())?)?
            }
        }
    };
    if copy.is_empty() {
        return Ok(());
    }
    let path = ctx.path(crate::string(artifact, "path")?)?;
    let bytes = ctx.read(&path)?;
    ensure!(
        artifact["sha256"] == fs::digest(&bytes),
        "editable candidate changed"
    );
    let page: spec::Page = serde_json::from_value(resolved.clone())?;
    require_complete(
        &page.id,
        &missing(&copy, &deck::open_bytes(&path, &bytes)?, &page)?,
    )
}

fn collect(shapes: &[Shape], blocks: &mut Vec<(String, String)>) {
    for shape in shapes {
        if shape.hidden
            || shape.locked
            || shape
                .partial_locks
                .iter()
                .any(|(k, v)| k == "noTextEdit" && *v)
        {
            continue;
        }
        if let Some(body) = &shape.text {
            blocks.push((shape.name.clone(), body.text()));
        }
        match &shape.kind {
            ShapeKind::Group { children, .. } => collect(children, blocks),
            ShapeKind::Table(table) => {
                for cell in table
                    .rows
                    .iter()
                    .flat_map(|r| &r.cells)
                    .filter(|c| !c.h_merge && !c.v_merge)
                {
                    blocks.push((String::new(), cell.text.text()));
                }
            }
            ShapeKind::Chart(chart) => {
                for title in [&chart.title, &chart.axis_titles.0, &chart.axis_titles.1]
                    .into_iter()
                    .flatten()
                {
                    blocks.push((String::new(), title.clone()));
                }
            }
            _ => {}
        }
    }
}

// Rejoin intentional split_text fragments by their source offsets, even when
// the native objects are painted in a different order. Count the bytes in the
// package only after comparing each fragment to the declared source span.
fn blocks(doc: &Presentation, page: &spec::Page) -> Result<Vec<String>> {
    let [slide] = doc.slides.as_slice() else {
        anyhow::bail!("editable copy requires a single page")
    };
    let mut actual = Vec::new();
    collect(&slide.shapes, &mut actual);
    let mut fragments = BTreeMap::<&str, Vec<&spec::Text>>::new();
    for t in &page.texts {
        if t.copy_range.is_some() {
            fragments
                .entry(t.copy_id.as_deref().unwrap_or(&t.id))
                .or_default()
                .push(t);
        }
    }
    let mut used = vec![false; actual.len()];
    let mut joined = Vec::new();
    for (id, mut parts) in fragments {
        let Some(canonical) = page.copy.get(id) else {
            continue;
        };
        let chars: Vec<_> = spec::normalized(canonical).chars().collect();
        parts.sort_by_key(|t| t.copy_range);
        let mut end = 0;
        let mut indexes = Vec::new();
        for t in parts {
            let [start, next] = t.copy_range.unwrap();
            if start != end || next <= start || next > chars.len() {
                break;
            }
            let expected = spec::normalized(&chars[start..next].iter().collect::<String>());
            let mut matches = actual.iter().enumerate().filter(|(i, (name, text))| {
                !used[*i]
                    && name.strip_prefix("octos:text:") == Some(t.id.as_str())
                    && spec::normalized(text) == expected
            });
            let Some((index, _)) = matches.next() else {
                break;
            };
            if matches.next().is_some() {
                break;
            }
            indexes.push(index);
            end = next;
        }
        if end == chars.len() && !indexes.is_empty() {
            for index in indexes {
                used[index] = true
            }
            joined.push(canonical.clone());
        }
    }
    joined.extend(
        actual
            .into_iter()
            .zip(used)
            .filter_map(|((_, text), used)| (!used).then_some(text)),
    );
    Ok(joined)
}

fn tokens(text: &str) -> Vec<String> {
    text.split_word_bounds()
        .filter(|w| !w.trim().is_empty())
        .map(str::to_owned)
        .collect()
}

fn missing(copy: &Copy, doc: &Presentation, page: &spec::Page) -> Result<Copy> {
    if copy.is_empty() {
        return Ok(Copy::new());
    }
    let mut available: Vec<_> = blocks(doc, page)?.iter().map(|s| tokens(s)).collect();
    // Pin complete objects first. For merged boxes, give the labels with the
    // fewest matching positions first choice of the remaining native words.
    let mut wanted = Vec::new();
    for (id, text) in copy {
        let words = tokens(text);
        if let Some(exact) = available.iter_mut().find(|b| **b == words) {
            exact.clear();
        } else {
            wanted.push((id, text, words));
        }
    }
    wanted.sort_by_cached_key(|(_, _, words)| {
        let places: usize = available
            .iter()
            .map(|b| b.windows(words.len()).filter(|w| *w == words).count())
            .sum();
        (places, std::cmp::Reverse(words.len()))
    });
    let mut missing = Copy::new();
    for (id, text, words) in wanted {
        let span = available.iter().enumerate().find_map(|(i, b)| {
            b.windows(words.len())
                .position(|w| w == words)
                .map(|j| (i, j))
        });
        if let Some((i, j)) = span {
            available[i][j..j + words.len()].fill(String::new());
        } else {
            missing.insert(id.clone(), text.clone());
        }
    }
    Ok(missing)
}

pub(crate) fn require_complete(id: &str, missing: &Copy) -> Result<()> {
    ensure!(
        missing.is_empty(),
        "page {id} is missing required editable text: {}. Restore these occurrences in the native PPTX before publishing.",
        serde_json::to_string(missing)?
    );
    Ok(())
}
