//! Preserve source observations across revisions of an image reconstruction.
use crate::{
    Context, deck, editable_copy, geometry, repair_gate, review, source_artwork,
    source_copy_review, string, typography,
};
use anyhow::{Context as _, Result, ensure};
use deckcraft_engine::model::{Shape, ShapeKind, resolve};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

const FIELD: &str = "source_detail_review";

fn bound_bytes(ctx: &Context, file: &Value) -> Result<Vec<u8>> {
    let bytes = ctx.read(string(file, "path")?)?;
    ensure!(
        file["sha256"] == crate::fs::digest(&bytes),
        "source detail evidence changed"
    );
    Ok(bytes)
}

/// Only newly frozen image reconstruction assignments acquire this contract.
pub(crate) fn freeze(ctx: &Context, roster: &Value, route: &mut Value) -> Result<()> {
    let mut pages = Map::new();
    for row in roster["pages"].as_array().context("missing pages")? {
        let matched = source_copy_review::reference(row)?;
        let fallback = row.get("reference").filter(|r| {
            (row.get("source_typography").is_some()
                || !row["inputs"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|i| i["role"] == "canonical-copy"))
                && r.as_str()
                    .or(r["path"].as_str())
                    .is_some_and(|p| image::ImageFormat::from_path(p).is_ok())
        });
        let Some(source) = matched.or(fallback) else {
            ensure!(
                row.get("source_typography").is_none(),
                "source_typography needs the original image"
            );
            continue;
        };
        let image = source_copy_review::source_binding(ctx, source)?;
        let size = review::image_dimensions(&ctx.verify(&image)?)?;
        let mut regions = Map::new();
        if let Some(file) = editable_copy::measurements(row)? {
            let measured: Value = serde_json::from_slice(&bound_bytes(ctx, file)?)?;
            if let Some(digest) = measured
                .get("sourceSha256")
                .or(measured.get("source_sha256"))
            {
                ensure!(
                    digest == &image["sha256"],
                    "source measurements belong to another image"
                );
            }
            if let Some(canvas) = measured.get("canvas") {
                let canvas: [f64; 2] = serde_json::from_value(canvas.clone())?;
                ensure!(
                    canvas.iter().all(|d| d.is_finite() && *d > 0.),
                    "invalid measurement canvas"
                );
                for line in measured["lines"].as_array().into_iter().flatten() {
                    let (Some(id), Some(rect)) = (line["id"].as_str(), line.get("box")) else {
                        continue;
                    };
                    let bounds = source_artwork::pixels(geometry::rect(rect)?, canvas, size)?;
                    let mut item = json!({"box":bounds});
                    if let Some(points) = line.get("polygon").or(line.get("quad"))
                        && let Ok(mut quad) = geometry::quad(points)
                    {
                        for p in &mut quad {
                            p[0] *= f64::from(size.0) / canvas[0];
                            p[1] *= f64::from(size.1) / canvas[1];
                        }
                        let angle = geometry::plane(quad).1;
                        // Quantization of an OCR box is not evidence of italic glyphs.
                        if angle.abs() >= 5. {
                            item["source_angle_deg"] = json!(angle);
                        }
                    }
                    ensure!(
                        !id.trim().is_empty() && regions.insert(id.into(), item).is_none(),
                        "duplicate or empty source mark {id}"
                    );
                }
            }
        }
        if let Some(expected) = row.get("source_typography") {
            for (id, style) in expected
                .as_object()
                .context("source_typography must be keyed by occurrence ID")?
            {
                let item = regions.entry(id).or_insert_with(|| json!({}));
                if let Some(area) = style.get("box") {
                    source_artwork::pixels(
                        geometry::rect(area)?,
                        [size.0.into(), size.1.into()],
                        size,
                    )?;
                    item["box"] = area.clone();
                }
                ensure!(
                    item["box"].is_array() && !id.trim().is_empty(),
                    "give the original-image box for {id}"
                );
                ensure!(
                    style.get("source_glyph").is_some() || style.get("source_angle_deg").is_some(),
                    "give the required glyph style or baseline for {id}"
                );
                if let Some(glyph) = style.get("source_glyph") {
                    ensure!(
                        matches!(glyph.as_str(), Some("upright" | "italic")),
                        "invalid source_glyph for {id}"
                    );
                    item["source_glyph"] = glyph.clone();
                }
                if let Some(angle) = style.get("source_angle_deg") {
                    ensure!(
                        angle.as_f64().is_some_and(|a| a.abs() <= 360.),
                        "invalid source_angle_deg for {id}"
                    );
                    item["source_angle_deg"] = angle.clone();
                }
                item["native_required"] = json!(true);
            }
        }
        pages.insert(
            string(row, "id")?.into(),
            json!({"original":image,"size":size,"regions":regions}),
        );
    }
    if !pages.is_empty() {
        route[FIELD] = Value::Object(pages);
    }
    Ok(())
}

pub(crate) fn template(
    ctx: &Context,
    row: &Value,
    candidate: &Value,
    previous: Option<&Value>,
    verdict: &mut Value,
) -> Result<()> {
    let route = crate::review_routes::load_plan(ctx)?;
    let contract = &route[FIELD][string(row, "id")?];
    if contract.is_null() {
        return Ok(());
    }
    let mut regions = contract["regions"].clone();
    if let Some(old) = previous.and_then(|v| v[FIELD]["regions"].as_object()) {
        for (id, entry) in old {
            let mut item = json!({"box":entry["box"]});
            for key in ["source_angle_deg", "source_glyph", "native_required"] {
                if let Some(v) = entry.get(key) {
                    item[key] = v.clone();
                }
            }
            regions[id] = item;
        }
    }
    verdict[FIELD] = json!({"scope":null,"regions":regions,"resolved_findings":{}});
    // The reviewer receives both files; including them in an unanswered template is not signoff.
    verdict["evidence"] = json!([contract["original"], candidate["render"]]);
    if let Some(prior) = previous {
        for f in repair_gate::findings(prior) {
            if let Some(id) = f["id"].as_str() {
                verdict[FIELD]["resolved_findings"][id] = Value::Null;
            }
        }
    }
    Ok(())
}

struct Native {
    runs: Vec<Value>,
    rotation: f64,
    simple: bool,
    bounds: [f64; 4],
}

fn native_shapes(ctx: &Context, artifact: &Value) -> Result<BTreeMap<String, Vec<Native>>> {
    fn collect(
        ctx: &resolve::Ctx,
        shapes: &[Shape],
        group: bool,
        scale: [f64; 2],
        result: &mut BTreeMap<String, Vec<Native>>,
    ) {
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
            if let ShapeKind::Group { children, .. } = &shape.kind {
                collect(ctx, children, true, scale, result);
            }
            let Some(body) = &shape.text else { continue };
            let runs = typography::font_runs(ctx, shape);
            if runs.is_empty() {
                continue;
            }
            let key = shape
                .name
                .strip_prefix("octos:text:")
                .unwrap_or(&shape.name);
            let xf = resolve::xfrm(ctx, shape);
            let frame = geometry::frame([xf.x, xf.y, xf.w, xf.h], xf.rot);
            let (bounds_x, bounds_y) = (
                frame.map(|p| p[0] * scale[0]),
                frame.map(|p| p[1] * scale[1]),
            );
            result.entry(key.into()).or_default().push(Native {
                runs,
                bounds: [
                    bounds_x.into_iter().fold(f64::INFINITY, f64::min),
                    bounds_y.into_iter().fold(f64::INFINITY, f64::min),
                    bounds_x.into_iter().fold(f64::NEG_INFINITY, f64::max),
                    bounds_y.into_iter().fold(f64::NEG_INFINITY, f64::max),
                ],
                rotation: xf.rot + body.body.rot.unwrap_or(0.),
                simple: !group
                    && body.body.warp.as_deref().is_none_or(|w| w == "textNoShape")
                    && body.body.vert.is_none_or(|v| v.xml() == "horz")
                    && body.body.upright != Some(true)
                    && shape.effects.as_ref().is_none_or(|e| e.raw3d.is_none()),
            });
        }
    }
    let path = ctx.path(string(artifact, "path")?)?;
    let doc = deck::open_bytes(&path, &bound_bytes(ctx, artifact)?)?;
    let [slide] = doc.slides.as_slice() else {
        anyhow::bail!("expected a single editable page")
    };
    let rc = resolve::Ctx::for_slide(&doc, slide).context("missing slide context")?;
    let mut out = BTreeMap::new();
    collect(
        &rc,
        &slide.shapes,
        false,
        [1. / doc.slide_size.width, 1. / doc.slide_size.height],
        &mut out,
    );
    Ok(out)
}

fn has_detail(render: &image::RgbImage, area: [u32; 4]) -> bool {
    let [x, y, w, h] = area;
    let first = render.get_pixel(x, y);
    (y..y + h).any(|j| (x..x + w).any(|i| render.get_pixel(i, j) != first))
}

// A one-sided contradiction for a claim that reference pixels survived. A
// textured result still requires visual review. Never infer literal text here.
fn edge_count(image: &image::RgbImage, [x, y, w, h]: [u32; 4], threshold: u8) -> u64 {
    let mut count = 0;
    // The floor/ceil mapping can bring the neighboring card edge into the crop.
    // Sample only the interior on both images.
    for j in y + 1..(y + h).saturating_sub(2) {
        for i in x + 1..(x + w).saturating_sub(2) {
            let a = image.get_pixel(i, j);
            let right = image.get_pixel(i + 1, j);
            let below = image.get_pixel(i, j + 1);
            if (0..3).any(|c| a[c].abs_diff(right[c]).max(a[c].abs_diff(below[c])) >= threshold) {
                count += 1;
            }
        }
    }
    count
}

pub(crate) fn validate(
    ctx: &Context,
    row: &Value,
    candidate: &Value,
    prior: Option<&Value>,
    verdict: &Value,
) -> Result<()> {
    let plan = crate::review_routes::load_plan(ctx)?;
    let contract = &plan[FIELD][string(row, "id")?];
    if contract.is_null() {
        return Ok(());
    }
    let pass = verdict["accepted"] == true;
    let data = &verdict[FIELD];
    let old = prior.map(|v| &v[FIELD]);
    // A rejection can be submitted early; previously recorded source facts still travel with it.
    if pass {
        ensure!(
            data["scope"] == "whole_page",
            "a local repair review cannot approve a page; inspect the whole original and final render"
        );
        for file in [&contract["original"], &candidate["render"]] {
            ensure!(
                verdict["evidence"].as_array().is_some_and(|v| v
                    .iter()
                    .any(|b| b["path"] == file["path"] && b["sha256"] == file["sha256"])),
                "whole-page approval needs the bound original and current render in evidence"
            );
        }
    }
    let empty = Map::new();
    let entries = data["regions"].as_object().unwrap_or(&empty);
    if pass {
        ensure!(
            data["regions"].is_object(),
            "record source regions, including visible marks with unreadable words"
        );
    }
    for (id, requirement) in contract["regions"]
        .as_object()
        .context("invalid frozen regions")?
    {
        if pass || entries.contains_key(id) {
            ensure!(
                entries
                    .get(id)
                    .is_some_and(|entry| requirement.as_object().is_some_and(|fields| fields
                        .iter()
                        .all(|(key, value)| entry.get(key) == Some(value)))),
                "missing or changed source region {id}"
            );
        }
    }
    if let Some(old) = old.and_then(|d| d["regions"].as_object()) {
        for (id, item) in old {
            let Some(now) = entries.get(id) else {
                anyhow::bail!("previous source region {id} must be accounted for")
            };
            if matches!(
                item["representation"].as_str(),
                Some("missing" | "native" | "artwork")
            ) {
                ensure!(
                    matches!(
                        now["representation"].as_str(),
                        Some("missing" | "native" | "artwork")
                    ),
                    "previously confirmed source mark {id} must stay accounted for"
                );
            }
            for key in ["box", "source_angle_deg", "source_glyph", "native_required"] {
                if let Some(value) = item
                    .get(key)
                    .filter(|v| !v.is_null() && **v != "indistinct")
                {
                    ensure!(
                        now.get(key) == Some(value),
                        "do not drop source evidence {key} for {id}"
                    );
                }
            }
        }
    }
    if let Some(prior) = prior {
        for f in repair_gate::findings(prior) {
            let Some(id) = f["id"].as_str() else { continue };
            ensure!(
                repair_gate::findings(verdict).any(|n| n["id"] == id)
                    || data["resolved_findings"][id]
                        .as_str()
                        .is_some_and(|s| !s.trim().is_empty()),
                "previous required finding {id} needs a current observation or must stay open"
            );
        }
    }
    let size: (u32, u32) = serde_json::from_value(contract["size"].clone())?;
    let mut render = None;
    let mut original = None;
    let mut objects = None;
    let mut remaining = 64 * 1024 * 1024u64;
    for (id, item) in entries {
        ensure!(!id.trim().is_empty(), "empty source detail ID");
        let rect = geometry::rect(&item["box"])?;
        let canvas = [f64::from(size.0), f64::from(size.1)];
        source_artwork::pixels(rect, canvas, size)?;
        if !pass {
            continue;
        }
        ensure!(
            item["basis"].as_str().is_some_and(|s| !s.trim().is_empty()),
            "inspect original and rendered marks at {id}"
        );
        let required_native =
            row["editable_copy"].get(id).is_some() || item["native_required"] == true;
        let seen = string(item, "representation")?;
        if seen == "not_source_content" {
            ensure!(
                !required_native,
                "confirmed source mark {id} cannot be dismissed"
            );
            continue;
        }
        ensure!(
            matches!(seen, "artwork" | "native"),
            "source mark {id} is missing or unresolved"
        );
        ensure!(
            !required_native || seen == "native",
            "source mark {id} requires editable text"
        );
        let glyph = string(item, "source_glyph")?;
        ensure!(
            matches!(glyph, "italic" | "upright" | "indistinct"),
            "inspect glyph inclination separately at {id}"
        );
        if glyph == "italic" || item.get("source_angle_deg").is_some() {
            ensure!(
                item["rendered_glyph"] == glyph
                    && item["rendered_baseline_basis"]
                        .as_str()
                        .is_some_and(|s| !s.trim().is_empty()),
                "compare the rendered glyph inclination and baseline independently with the original at {id}"
            );
        }
        if render.is_none() {
            render =
                Some(review::decode_image(&bound_bytes(ctx, &candidate["render"])?)?.to_rgb8());
        }
        let image = render.as_ref().unwrap();
        let area = source_artwork::pixels(rect, canvas, image.dimensions())?;
        ensure!(
            area[2] > 0 && area[3] > 0,
            "source region {id} is subpixel on the rendered page"
        );
        remaining = remaining
            .checked_sub(u64::from(area[2]) * u64::from(area[3]))
            .context("source detail areas exceed 64 megapixels")?;
        ensure!(
            has_detail(image, area),
            "current rendered region {id} is blank"
        );
        if seen == "artwork" {
            if original.is_none() {
                original = Some(
                    review::decode_image(&bound_bytes(ctx, &contract["original"])?)?.to_rgb8(),
                );
            }
            let [x, y, w, h] = source_artwork::pixels(rect, canvas, size)?;
            let [_, _, ow, oh] = area;
            let crop = image::imageops::crop_imm(original.as_ref().unwrap(), x, y, w, h);
            let matched =
                image::imageops::resize(&*crop, ow, oh, image::imageops::FilterType::Triangle);
            let before = edge_count(&matched, [0, 0, ow, oh], 24);
            let after = edge_count(image, area, 8);
            ensure!(
                before < 12 || after * 50 >= before,
                "source strokes are missing from the claimed artwork at {id}: {before} strong reference edges, {after} visible candidate edges"
            );
            continue;
        }
        if objects.is_none() {
            objects = Some(native_shapes(ctx, &candidate["artifact"])?);
        }
        let object_id = string(item, "object_id")?;
        let values = objects.as_ref().unwrap().get(object_id).map(Vec::as_slice);
        let Some([native]) = values else {
            anyhow::bail!("no unique visible editable text object {object_id} for {id}")
        };
        if native.simple {
            let [x, y, w, h] = rect;
            let [l, t, r, b] = native.bounds;
            ensure!(
                l < (x + w) / canvas[0]
                    && x / canvas[0] < r
                    && t < (y + h) / canvas[1]
                    && y / canvas[1] < b,
                "editable text object {object_id} does not intersect source region {id}"
            );
        }
        let text = native
            .runs
            .iter()
            .filter_map(|r| r["text"].as_str())
            .collect::<String>();
        if let Some(copy) = row["editable_copy"][id].as_str() {
            ensure!(
                crate::spec::normalized(&text).contains(&crate::spec::normalized(copy)),
                "editable text object {object_id} does not contain source mark {id}"
            );
        }
        if glyph != "indistinct" {
            let span = item
                .get("glyph_span")
                .and_then(Value::as_str)
                .or_else(|| row["editable_copy"][id].as_str())
                .unwrap_or(&text);
            if let Some(expected) = row["editable_copy"][id].as_str()
                && contract["regions"][id]["source_glyph"].is_string()
            {
                ensure!(
                    crate::spec::normalized(span) == crate::spec::normalized(expected),
                    "glyph_span for {id} must cover the required source label"
                );
            }
            let mut matches = text.match_indices(span);
            let start = matches
                .next()
                .map(|m| m.0)
                .context("glyph_span is absent from the editable text")?;
            ensure!(
                !span.is_empty() && matches.next().is_none(),
                "glyph_span must identify one nonempty occurrence in the object"
            );
            let mut offset = 0;
            for run in &native.runs {
                let next = offset + string(run, "text")?.len();
                if offset < start + span.len() && start < next {
                    let italic = run["italic"] == true
                        || run["font"].as_str().is_some_and(|name| {
                            let name = name.to_lowercase();
                            name.contains("italic") || name.contains("oblique")
                        });
                    ensure!(
                        italic == (glyph == "italic"),
                        "source mark {id} requires {glyph} glyphs on its native text"
                    );
                }
                offset = next;
            }
        }
        if let Some(angle) = item.get("source_angle_deg") {
            let target = angle
                .as_f64()
                .filter(|a| a.is_finite() && a.abs() <= 360.)
                .context("invalid source baseline angle")?;
            if native.simple {
                let tolerance = 2.0_f64.atan2(rect[2].max(1.)).to_degrees().max(3.);
                ensure!(
                    geometry::angle(native.rotation, target).abs() <= tolerance,
                    "native baseline for {id} is {} degrees; source {target} degrees",
                    native.rotation
                );
            }
        }
    }
    Ok(())
}
