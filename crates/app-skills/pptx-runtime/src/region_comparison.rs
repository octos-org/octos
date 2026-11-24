use crate::{Context, fs, review, string};
use anyhow::{Context as _, Result, ensure};
use image::RgbImage;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeSet;

#[derive(Deserialize)]
struct Region {
    id: String,
    #[serde(rename = "box")]
    bounds: [u32; 4],
}

fn snapshot(ctx: &Context, input: &str) -> Result<(RgbImage, Value)> {
    let path = ctx.path(input)?;
    let bytes = ctx.read(&path)?;
    let binding = json!({"path": path.strip_prefix(&ctx.root)?, "sha256": fs::digest(&bytes)});
    Ok((review::decode_image(&bytes)?.to_rgb8(), binding))
}

pub(crate) fn compare(ctx: &Context, request: &Value) -> Result<Value> {
    ensure!(
        request.get("background").is_none() && request.get("output").is_none(),
        "region comparison returns measurements; use receipt for an optional JSON file"
    );
    ensure!(
        request.get("threshold").is_none() && request.get("tolerance").is_none(),
        "region measurements report raw colour differences without a threshold or tolerance"
    );
    let regions: Vec<Region> = serde_json::from_value(request["regions"].clone())?;
    ensure!(
        (1..=64).contains(&regions.len()),
        "provide 1 to 64 pixel regions"
    );
    let mut ids = BTreeSet::new();
    let mut remaining: u64 = 64 * 1024 * 1024;
    for region in &regions {
        let [_, _, w, h] = region.bounds;
        ensure!(
            !region.id.trim().is_empty() && ids.insert(&region.id),
            "region IDs must be nonempty and unique"
        );
        ensure!(w > 0 && h > 0, "region {} is empty", region.id);
        remaining = remaining
            .checked_sub(u64::from(w) * u64::from(h))
            .context("total region area exceeds 64 megapixels")?;
    }
    let (before, before_binding) = snapshot(ctx, string(request, "before")?)?;
    let (after, after_binding) = snapshot(ctx, string(request, "after")?)?;
    let xmax = before.width().min(after.width());
    let ymax = before.height().min(after.height());
    let mut rows = Vec::with_capacity(regions.len());
    for region in regions {
        let [x, y, w, h] = region.bounds;
        ensure!(
            w <= xmax && h <= ymax && x <= xmax - w && y <= ymax - h,
            "region {} is outside one or both images",
            region.id
        );
        let mut before_sum = [0_u64; 3];
        let mut after_sum = [0_u64; 3];
        let mut abs_sum = [0_u64; 3];
        for pixel_y in y..y + h {
            for pixel_x in x..x + w {
                let a = before.get_pixel(pixel_x, pixel_y);
                let b = after.get_pixel(pixel_x, pixel_y);
                for c in 0..3 {
                    before_sum[c] += u64::from(a[c]);
                    after_sum[c] += u64::from(b[c]);
                    abs_sum[c] += u64::from(a[c].abs_diff(b[c]));
                }
            }
        }
        let count = u64::from(w) * u64::from(h);
        let mean = |sum: [u64; 3]| sum.map(|v| v as f64 / count as f64);
        rows.push(json!({"id":region.id,"box":region.bounds,"pixels":count,
            "before_mean_rgb":mean(before_sum),"after_mean_rgb":mean(after_sum),
            "mean_absolute_rgb_error":mean(abs_sum),"mean_channel_error":abs_sum.iter().sum::<u64>() as f64 / (3.0 * count as f64)}));
    }
    let mut report = json!({"algorithm":"region-comparison-v1","before":before_binding,"after":after_binding,
        "before_size":[before.width(),before.height()],"after_size":[after.width(),after.height()],
        "coordinate_space":"original image pixels at a common origin and scale","resampled":false,"regions":rows,"quality_approved":false});
    if request.get("receipt").is_some() {
        let path = ctx.write_json(string(request, "receipt")?, &report)?;
        report["receipt"] = ctx.binding(path)?;
    }
    Ok(report)
}
