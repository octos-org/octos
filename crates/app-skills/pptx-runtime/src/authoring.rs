//! Content-preserving authoring operations, separate from visual judgments.
use crate::{Context, spec, string};
use anyhow::Context as _;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// A declared graphical symbol, not permission to rasterize arbitrary copy.
/// Pixel binding and complete copy coverage are mechanical; equivalence needs review.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtworkSymbol {
    pub id: String,
    pub copy_id: String,
    pub copy_range: [usize; 2],
    pub text: String,
    #[serde(rename = "box")]
    pub bounds: [f64; 4],
    pub background_sha256: String,
    pub basis: String,
}

impl ArtworkSymbol {
    pub fn validate(&self, page: &spec::Page, canvas: &spec::Canvas) -> Result<()> {
        let text = spec::normalized(&self.text);
        let mut chars = text.chars();
        let c = chars.next();
        ensure!(
            chars.next().is_none()
                && c.is_some_and(
                    |c| ('\u{2190}'..='\u{21ff}').contains(&c) || "+−×÷=±≠≤≥".contains(c)
                ),
            "artwork symbols permit one arrow/operator only; ordinary text, numbers and labels must remain native"
        );
        ensure!(
            !self.id.is_empty() && !self.basis.trim().is_empty() && self.basis.len() <= 4096,
            "artwork symbol needs an id and an evidence-based explanation"
        );
        ensure!(
            page.background["sha256"].as_str() == Some(self.background_sha256.as_str()),
            "artwork symbol refers to stale background pixels"
        );
        let canonical: Vec<_> = spec::normalized(
            page.copy
                .get(&self.copy_id)
                .ok_or_else(|| anyhow::anyhow!("missing artwork symbol copy occurrence"))?,
        )
        .chars()
        .collect();
        let [start, end] = self.copy_range;
        ensure!(
            start < end
                && end <= canonical.len()
                && spec::normalized(&canonical[start..end].iter().collect::<String>()) == text,
            "artwork symbol must cover exactly its authoritative copy span"
        );
        let [x, y, w, h] = crate::geometry::rect(&json!(self.bounds))?;
        ensure!(
            x >= 0. && y >= 0. && x + w <= canvas.width_px && y + h <= canvas.height_px,
            "artwork symbol area must be inside the canvas"
        );
        Ok(())
    }
}

pub fn represent_symbol(ctx: &Context, r: &Value) -> Result<Value> {
    ensure!(
        r.as_object()
            .context("authoring::represent_symbol: missing required value")?
            .keys()
            .all(|k| [
                "action",
                "spec",
                "output",
                "page",
                "id",
                "box",
                "basis",
                "expected_sha256"
            ]
            .contains(&k.as_str())),
        "unsupported represent_symbol option"
    );
    let source = ctx.path(string(r, "spec")?)?;
    let binding = ctx.binding(&source)?;
    if let Some(hash) = r.get("expected_sha256") {
        ensure!(hash == &binding["sha256"], "stale spec");
    }
    let mut spec = spec::load(ctx, &source, None)?;
    let page_id = string(r, "page")?;
    let id = string(r, "id")?;
    let page = spec
        .pages
        .iter_mut()
        .find(|p| p.id == page_id)
        .ok_or_else(|| anyhow::anyhow!("unknown page"))?;
    let index = page.texts.iter().position(|t| t.id == id).ok_or_else(|| {
        anyhow::anyhow!("unknown native text; split a mixed text/symbol occurrence first")
    })?;
    let t = &page.texts[index];
    let copy_id = t.copy_id.clone().unwrap_or_else(|| t.id.clone());
    let symbol = ArtworkSymbol {
        id: t.id.clone(),
        copy_range: t
            .copy_range
            .unwrap_or([0, spec::normalized(&page.copy[&copy_id]).chars().count()]),
        copy_id,
        text: t.text.clone(),
        bounds: crate::geometry::rect(&r["box"])?,
        background_sha256: page.background["sha256"]
            .as_str()
            .context("authoring::represent_symbol: missing required value")?
            .into(),
        basis: string(r, "basis")?.into(),
    };
    symbol.validate(page, &spec.canvas)?;
    // Do not silently alter a previously recorded content/layout contract.
    ensure!(
        !ctx.path(".octos/pptx-evidence-history.json")?.exists(),
        "content representation is already frozen; preserve history and report the contract mismatch, do not reset it"
    );
    for group in &mut page.typography_groups {
        group.members.retain(|m| m != id);
        ensure!(
            group.members.len() >= 2,
            "conversion would leave a one-member typography group; plan native groups before freezing the contract"
        );
    }
    page.texts.remove(index);
    page.artwork_symbols.push(symbol.clone());
    spec.validate()?;
    ctx.verify(&binding)?;
    let output = ctx.write_json(string(r, "output")?, &serde_json::to_value(&spec)?)?;
    Ok(
        json!({"source":binding,"spec":ctx.binding(output)?,"symbol":symbol,
        "canonical_copy_unchanged":true,"native_symbol_removed":true,"semantic_equivalence_verified":false,
        "quality_approved":false,"next":"Build and review the bound design/blank/current symbol crops. Representation, uniqueness and visibility remain unknown until explicitly reviewed. This symbol remains raster artwork, not editable text."}),
    )
}

pub fn split_text(ctx: &Context, r: &Value) -> Result<Value> {
    for key in r.as_object().into_iter().flat_map(|v| v.keys()) {
        ensure!(
            [
                "action",
                "spec",
                "output",
                "expected_sha256",
                "page",
                "id",
                "parts"
            ]
            .contains(&key.as_str()),
            "unsupported split_text option {key}"
        );
    }
    let source = ctx.path(string(r, "spec")?)?;
    let binding = ctx.binding(&source)?;
    if let Some(hash) = r.get("expected_sha256") {
        ensure!(hash == &binding["sha256"], "stale spec");
    }
    let mut spec = spec::load(ctx, &source, None)?;
    let page_id = string(r, "page")?;
    let object_id = string(r, "id")?;
    let page = spec
        .pages
        .iter_mut()
        .find(|p| p.id == page_id)
        .ok_or_else(|| anyhow::anyhow!("unknown page {page_id}"))?;
    let index = page
        .texts
        .iter()
        .position(|t| t.id == object_id)
        .ok_or_else(|| anyhow::anyhow!("unknown object {object_id}"))?;
    let old = &page.texts[index];
    let key = old.copy_id.as_ref().unwrap_or(&old.id).clone();
    let chars: Vec<_> = spec::normalized(&page.copy[&key]).chars().collect();
    let [start, end] = old.copy_range.unwrap_or([0, chars.len()]);
    let parts = r["parts"]
        .as_array()
        .filter(|p| (2..=1024).contains(&p.len()))
        .ok_or_else(|| anyhow::anyhow!("parts must contain 2 to 1024 text fragments"))?;
    let original = serde_json::to_value(old)?;
    let mut cursor = start;
    let mut fragments = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        let fields = part
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("part must be an object"))?;
        ensure!(
            fields.keys().all(
                |k| ["id", "text", "box", "style", "rotation", "format", "region"]
                    .contains(&k.as_str())
            ),
            "parts support id, text, box, style, rotation, format and region only"
        );
        string(part, "id")?;
        crate::geometry::rect(&part["box"])?;
        let text = spec::normalized(string(part, "text")?);
        ensure!(!text.is_empty(), "empty fragment");
        let span_start = cursor;
        while cursor < end && chars[cursor].is_whitespace() {
            cursor += 1;
        }
        let proposed: Vec<_> = text.chars().collect();
        ensure!(
            cursor + proposed.len() <= end && chars[cursor..cursor + proposed.len()] == proposed,
            "fragment {} does not match the next authoritative content; no characters or graphical symbols may be dropped, duplicated or rewritten",
            part["id"]
        );
        cursor += proposed.len();
        if i + 1 == parts.len() {
            while cursor < end && chars[cursor].is_whitespace() {
                cursor += 1;
            }
        }
        let mut fragment = original.clone();
        for (k, value) in fields {
            if k == "format" {
                spec::merge_format(&mut fragment, value, "merge")?;
            } else {
                fragment[k] = value.clone();
            }
        }
        fragment["copy_id"] = json!(key);
        fragment["copy_range"] = json!([span_start, cursor]);
        fragments.push(serde_json::from_value::<spec::Text>(fragment)?);
    }
    ensure!(
        cursor == end,
        "split leaves authoritative content unrepresented"
    );
    let mapping: Vec<_> = fragments
        .iter()
        .map(|t| {
            json!({"id":t.id,"text":t.text,
        "copy_id":t.copy_id,"copy_range":t.copy_range})
        })
        .collect();
    for group in &mut page.typography_groups {
        if let Some(i) = group.members.iter().position(|id| id == object_id) {
            group
                .members
                .splice(i..=i, fragments.iter().map(|t| t.id.clone()));
        }
    }
    page.texts.splice(index..=index, fragments);
    spec.validate()?;
    ctx.verify(&binding)?;
    let output = ctx.write_json(string(r, "output")?, &serde_json::to_value(&spec)?)?;
    Ok(
        json!({"source":binding,"spec":ctx.binding(output)?,"page":page_id,"original_object":object_id,
        "copy_id":key,"fragments":mapping,"range_units":"Unicode scalar offsets in normalized authoritative copy; end exclusive",
        "content_preserved":true,"quality_approved":false,
        "note":"Canonical copy is unchanged. All fragments remain native text. This does not certify artwork ownership, typography or accept a raster symbol in place of text. Rebuild and review; existing evidence is not rewritten."}),
    )
}
