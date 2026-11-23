//! Carry explicitly identified reference illustrations across the planner / page-maker boundary.
use crate::{Context, fs, geometry, review, spec};
use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeSet, io::Cursor, path::Path};

/// Source-image pixel coordinates, including any native text to leave uncovered.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceCrop {
    pub bounds: [u32; 4],
    #[serde(default)]
    pub exclude: Vec<[u32; 4]>,
}

impl SourceCrop {
    pub(crate) fn extract(&self, source: &image::DynamicImage) -> Result<Vec<u8>> {
        let [x, y, w, h] = self.bounds;
        let right = x.checked_add(w).context("crop right edge overflow")?;
        let bottom = y.checked_add(h).context("crop bottom edge overflow")?;
        ensure!(
            w > 0 && h > 0 && right <= source.width() && bottom <= source.height(),
            "source artwork crop outside original image"
        );
        ensure!(
            w < source.width() || h < source.height(),
            "expected local artwork, got full page"
        );
        let mut area = source.crop_imm(x, y, w, h).into_rgba8();
        ensure!(self.exclude.len() <= 10000, "too many text exclusions");
        for &[left, top, width, height] in &self.exclude {
            let end_x = left.checked_add(width).context("invalid exclusion width")?;
            let end_y = top
                .checked_add(height)
                .context("invalid exclusion height")?;
            ensure!(
                end_x <= source.width() && end_y <= source.height(),
                "text exclusion outside reference"
            );
            for row in top.max(y)..end_y.min(bottom) {
                for col in left.max(x)..end_x.min(right) {
                    area.get_pixel_mut(col - x, row - y).0 = [0; 4];
                }
            }
        }
        let mut output = Cursor::new(Vec::new());
        area.write_to(&mut output, image::ImageFormat::Png)?;
        Ok(output.into_inner())
    }
}

pub(crate) fn pixels(rect: [f64; 4], canvas: [f64; 2], dimensions: (u32, u32)) -> Result<[u32; 4]> {
    let [x, y, w, h] = rect;
    ensure!(
        x >= 0. && y >= 0. && w > 0. && h > 0. && x + w <= canvas[0] && y + h <= canvas[1],
        "artwork measurements outside reference canvas"
    );
    let sx = f64::from(dimensions.0) / canvas[0];
    let sy = f64::from(dimensions.1) / canvas[1];
    let (left, top) = ((x * sx).floor() as u32, (y * sy).floor() as u32);
    Ok([
        left,
        top,
        ((x + w) * sx).ceil().min(dimensions.0.into()) as u32 - left,
        ((y + h) * sy).ceil().min(dimensions.1.into()) as u32 - top,
    ])
}

/// Derive the handoff from existing page-job inputs when both upstream outputs
/// are present. The maker need not repeat their paths or the individual regions.
pub(crate) fn assignment(row: &Value) -> Result<Option<Value>> {
    if let Some(declared) = row.get("source_artwork") {
        return Ok(Some(declared.clone()));
    }
    let Some(inputs) = row["inputs"].as_array() else {
        return Ok(None);
    };
    let by_role = |role: &str| -> Result<Option<&Value>> {
        let mut files = inputs.iter().filter(|file| file["role"] == role);
        let first = files.next();
        ensure!(
            files.next().is_none(),
            "ambiguous source artwork input role {role}"
        );
        Ok(first)
    };
    let (Some(preparation), Some(measurements)) =
        (by_role("preparation")?, by_role("measurements")?)
    else {
        return Ok(None);
    };
    let image = match row.get("reference").filter(|r| r["sha256"].is_string()) {
        Some(reference) => reference,
        None => {
            by_role("design")?.context("prepared source artwork needs the bound reference image")?
        }
    };
    Ok(Some(
        json!({"image":image,"preparation":preparation,"measurements":measurements}),
    ))
}

pub(crate) fn inherit(
    ctx: &Context,
    page: &mut Value,
    handoff: &Value,
    base: &Path,
    expected: &spec::Canvas,
    sources: &mut Vec<Value>,
) -> Result<()> {
    let bound = |key: &str| -> Result<Value> {
        let mut value = handoff[key].clone();
        spec::asset(ctx, &mut value, base).with_context(|| format!("source_artwork.{key}"))?;
        Ok(value)
    };
    let image = bound("image")?;
    let prepared = bound("preparation")?;
    let measured = bound("measurements")?;
    let read_bound = |v: &Value| -> Result<Value> {
        let bytes = ctx.read(ctx.verify(v)?)?;
        ensure!(
            fs::digest(&bytes) == v["sha256"],
            "source artwork input changed"
        );
        Ok(serde_json::from_slice(&bytes)?)
    };
    let measurements = read_bound(&measured)?;
    let plan = read_bound(&prepared)?;
    let canvas: [f64; 2] = serde_json::from_value(measurements["canvas"].clone())?;
    ensure!(
        canvas == [expected.width_px, expected.height_px]
            && measurements["sourceSha256"] == image["sha256"],
        "source artwork measurements belong to a different image or canvas"
    );
    let dimensions = review::image_dimensions(&ctx.verify(&image)?)?;
    ensure!(
        (f64::from(dimensions.0) / f64::from(dimensions.1) - canvas[0] / canvas[1]).abs() < 0.001,
        "source artwork image aspect ratio differs from canvas"
    );
    let native_text: BTreeSet<_> = page["texts"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|t| [t["id"].as_str(), t["copy_id"].as_str()])
        .flatten()
        .map(str::to_owned)
        .collect();
    let layers = page
        .as_object_mut()
        .context("invalid page")?
        .entry("artwork_layers")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .context("invalid artwork_layers")?;
    let ids: BTreeSet<String> = layers
        .iter()
        .filter_map(|v| v["id"].as_str().map(str::to_owned))
        .collect();
    let graphics = plan["graphics"]
        .as_array()
        .context("missing prepared graphics")?;
    let lines = measurements["lines"]
        .as_array()
        .context("missing source text measurements")?;
    ensure!(
        graphics.len() <= 10000 && lines.len() <= 10000,
        "excessive source artwork input"
    );
    let text: Vec<_> = lines
        .iter()
        .filter(|line| {
            line["id"]
                .as_str()
                .is_some_and(|id| native_text.contains(id))
        })
        .map(|v| geometry::rect(&v["box"]))
        .collect::<Result<_>>()?;
    let mut additions = Vec::new();
    let mut declared_ids = BTreeSet::new();
    for (i, region) in graphics
        .iter()
        .enumerate()
        .filter(|(_, r)| r["treatment"] == "crop")
    {
        let b = geometry::rect(&region["box"])?;
        let measured_region = measurements["graphicRegions"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|v| v["box"] == region["box"]);
        let id = region["id"]
            .as_str()
            .or_else(|| measured_region.and_then(|r| r["id"].as_str()))
            .map(str::to_owned)
            .unwrap_or_else(|| format!("source-figure-{}", i + 1));
        ensure!(
            declared_ids.insert(id.clone()),
            "duplicate prepared illustration ID {id}"
        );
        if ids.contains(&id) {
            continue;
        }
        let bounds = pixels(b, canvas, dimensions)?;
        ensure!(
            bounds[2] < dimensions.0 || bounds[3] < dimensions.1,
            "cannot inherit a full-page picture"
        );
        let exclude = text
            .iter()
            .filter(|t| {
                t[0] < b[0] + b[2] && b[0] < t[0] + t[2] && t[1] < b[1] + b[3] && b[1] < t[1] + t[3]
            })
            .map(|t| pixels(*t, canvas, dimensions))
            .collect::<Result<_>>()?;
        additions.push(json!({"id":id,"image":image,"box":b,"basis":"Declared reference illustration from page preparation",
            "source_crop":SourceCrop{bounds,exclude}}));
    }
    layers.splice(0..0, additions);
    sources.extend([prepared, measured]);
    Ok(())
}
