//! Read-only, background-subtracted comparison aids. Never a visual verdict.
use crate::{Context, geometry, review, string};
use anyhow::{Result, ensure};
use image::{Rgb, RgbImage, imageops};
use serde_json::{Value, json};

pub const ALGORITHM: &str = "background-subtracted-labels-v3";

/// Group adjacent marked cells, keeping every marked pixel. These are review
/// viewports, not recognized labels or defect judgments.
fn regions(marks: &[u8], w: usize, h: usize, left: u32, top: u32) -> Vec<Value> {
    let cell = 64;
    let (cols, rows) = (w.div_ceil(cell), h.div_ceil(cell));
    let mut counts = vec![[0u64; 2]; cols * rows];
    for (i, mark) in marks.iter().enumerate() {
        if *mark > 0 {
            counts[(i / w / cell) * cols + (i % w / cell)][(*mark - 1) as usize] += 1;
        }
    }
    let mut seen = vec![false; counts.len()];
    let mut result = Vec::new();
    for start in 0..counts.len() {
        if seen[start] || counts[start] == [0, 0] {
            continue;
        }
        let mut stack = vec![start];
        seen[start] = true;
        let (mut l, mut t, mut r, mut b) = (cols, rows, 0, 0);
        let mut total = [0u64; 2];
        while let Some(i) = stack.pop() {
            let (x, y) = (i % cols, i / cols);
            l = l.min(x);
            t = t.min(y);
            r = r.max(x + 1);
            b = b.max(y + 1);
            total[0] += counts[i][0];
            total[1] += counts[i][1];
            for ny in y.saturating_sub(1)..=(y + 1).min(rows - 1) {
                for nx in x.saturating_sub(1)..=(x + 1).min(cols - 1) {
                    let next = ny * cols + nx;
                    if !seen[next] && counts[next] != [0, 0] {
                        seen[next] = true;
                        stack.push(next);
                    }
                }
            }
        }
        let (x, y) = ((l * cell).saturating_sub(16), (t * cell).saturating_sub(16));
        result.push(json!({"id":format!("region-{:03}",result.len()+1),
            "box":[left as usize+x,top as usize+y,(r*cell+16).min(w)-x,(b*cell+16).min(h)-y],
            "reference_only_pixels":total[0],"candidate_only_pixels":total[1]}));
    }
    result
}

pub(crate) fn normalize(image: RgbImage, dimensions: (u32, u32)) -> Result<RgbImage> {
    let aspect = image.width() as f64 / image.height() as f64;
    let target = dimensions.0 as f64 / dimensions.1 as f64;
    ensure!(
        (aspect / target - 1.).abs() < 0.005,
        "comparison aspect ratios differ; provide corresponding full-page views"
    );
    Ok(if image.dimensions() == dimensions {
        image
    } else {
        imageops::resize(
            &image,
            dimensions.0,
            dimensions.1,
            imageops::FilterType::Triangle,
        )
    })
}

fn mask(image: &RgbImage, blank: &RgbImage, threshold: u8) -> Vec<bool> {
    image
        .pixels()
        .zip(blank.pixels())
        .map(|(a, b)| (0..3).any(|i| a[i].abs_diff(b[i]) > threshold))
        .collect()
}

// Integral image makes the tolerance independent of glyph count and avoids
// quadratic neighborhood work on a high-resolution page.
fn dilate(mask: &[bool], w: usize, h: usize, radius: usize) -> Vec<bool> {
    let stride = w + 1;
    let mut integral = vec![0u32; stride * (h + 1)];
    for y in 0..h {
        let mut row = 0;
        for x in 0..w {
            row += u32::from(mask[y * w + x]);
            integral[(y + 1) * stride + x + 1] = integral[y * stride + x + 1] + row;
        }
    }
    (0..w * h)
        .map(|i| {
            let (x, y) = (i % w, i / w);
            let (l, t) = (x.saturating_sub(radius), y.saturating_sub(radius));
            let (r, b) = ((x + radius + 1).min(w), (y + radius + 1).min(h));
            integral[b * stride + r] + integral[t * stride + l]
                > integral[b * stride + l] + integral[t * stride + r]
        })
        .collect()
}

pub fn compare(ctx: &Context, request: &Value) -> Result<Value> {
    let paths = [
        ctx.path(string(request, "before")?)?,
        ctx.path(string(request, "background")?)?,
        ctx.path(string(request, "after")?)?,
    ];
    let bindings: Vec<_> = paths
        .iter()
        .map(|p| ctx.binding(p))
        .collect::<Result<_>>()?;
    let mut loaded = Vec::new();
    for p in &paths {
        loaded.push(review::load_image(ctx, p)?.to_rgb8());
    }
    let sizes: Vec<_> = loaded.iter().map(|im| im.dimensions()).collect();
    let dimensions = sizes[2];
    ensure!(
        u64::from(dimensions.0) * u64::from(dimensions.1) <= 16_000_000,
        "comparison canvas exceeds 16 megapixels"
    );
    let mut views: Vec<_> = loaded
        .into_iter()
        .map(|im| normalize(im, dimensions))
        .collect::<Result<_>>()?;
    let reference_alignment = crate::visual_proof::reference_alignment(&views[0], &views[1]);
    let b = if request.get("box").is_some() {
        geometry::rect(&request["box"])?
    } else {
        [0., 0., dimensions.0 as f64, dimensions.1 as f64]
    };
    ensure!(
        b[0] >= 0.
            && b[1] >= 0.
            && b[0] + b[2] <= dimensions.0 as f64
            && b[1] + b[3] <= dimensions.1 as f64,
        "comparison crop is outside the after image"
    );
    let (left, top) = (b[0].floor() as u32, b[1].floor() as u32);
    let (w, h) = (
        (b[0] + b[2]).ceil() as u32 - left,
        (b[1] + b[3]).ceil() as u32 - top,
    );
    for im in &mut views {
        *im = imageops::crop_imm(im, left, top, w, h).to_image();
    }
    let threshold = request
        .get("threshold")
        .map(|v| {
            v.as_u64()
                .filter(|n| *n <= 255)
                .ok_or_else(|| anyhow::anyhow!("threshold must be 0..255"))
        })
        .transpose()?
        .unwrap_or(24) as u8;
    let tolerance = request
        .get("tolerance")
        .map(|v| {
            v.as_u64()
                .filter(|n| *n <= 8)
                .ok_or_else(|| anyhow::anyhow!("tolerance must be 0..8 pixels"))
        })
        .transpose()?
        .unwrap_or(2) as usize;
    let reference = mask(&views[0], &views[1], threshold);
    let candidate = mask(&views[2], &views[1], threshold);
    let near_reference = dilate(&reference, w as usize, h as usize, tolerance);
    let near_candidate = dilate(&candidate, w as usize, h as usize, tolerance);
    let mut reference_signal = RgbImage::from_pixel(w, h, Rgb([255, 255, 255]));
    let mut candidate_signal = reference_signal.clone();
    let mut difference = views[1].clone();
    let mut marks = vec![0u8; (w as usize) * (h as usize)];
    let (mut reference_only, mut candidate_only) = (0u64, 0u64);
    for (i, p) in difference.pixels_mut().enumerate() {
        for v in &mut p.0 {
            *v = (u16::from(*v) / 4 + 191).min(255) as u8;
        }
        if reference[i] {
            reference_signal.put_pixel(i as u32 % w, i as u32 / w, Rgb([35, 110, 220]));
        }
        if candidate[i] {
            candidate_signal.put_pixel(i as u32 % w, i as u32 / w, Rgb([225, 45, 65]));
        }
        if reference[i] || candidate[i] {
            *p = Rgb([110, 110, 110]);
        }
        if reference[i] && !near_candidate[i] {
            *p = Rgb([35, 110, 220]);
            reference_only += 1;
            marks[i] = 1;
        }
        if candidate[i] && !near_reference[i] {
            *p = Rgb([225, 45, 65]);
            candidate_only += 1;
            marks[i] = 2;
        }
    }
    views.extend([reference_signal, candidate_signal, difference]);
    let names = [
        "design",
        "background",
        "render",
        "reference-signal",
        "candidate-signal",
        "difference",
    ];
    let output = ctx.path(string(request, "output")?)?;
    let labels: Vec<crate::label_diff::Label> =
        serde_json::from_value(request.get("labels").cloned().unwrap_or(json!([])))?;
    ensure!(
        labels.is_empty() || (left == 0 && top == 0 && (w, h) == dimensions),
        "label evidence requires the whole page; crop the resulting label panels instead"
    );
    let label_evidence =
        crate::label_diff::analyze(ctx, &output, &labels, &views, &reference, &candidate)?;
    let stem = output
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow::anyhow!("output needs a file name"))?;
    let mut files = serde_json::Map::new();
    // Full-resolution signals stay separate. Bound only the overview panel so
    // a 4K page does not require a 50-megapixel contact sheet.
    let scale = (1_000_000. / (f64::from(w) * f64::from(h)))
        .sqrt()
        .min(1280. / f64::from(w))
        .min(1.);
    let (pw, ph) = (
        (f64::from(w) * scale).floor().max(1.) as u32,
        (f64::from(h) * scale).floor().max(1.) as u32,
    );
    let mut panel = RgbImage::from_pixel(pw * 3, ph * 2, Rgb([255, 255, 255]));
    for (i, (name, im)) in names.iter().zip(views).enumerate() {
        let thumbnail = imageops::resize(&im, pw, ph, imageops::FilterType::Triangle);
        imageops::replace(
            &mut panel,
            &thumbnail,
            i64::from(pw) * (i % 3) as i64,
            i64::from(ph) * (i / 3) as i64,
        );
        let path = output.with_file_name(format!("{stem}-{name}.png"));
        review::save_image(ctx, &path, image::DynamicImage::ImageRgb8(im))?;
        files.insert((*name).into(), ctx.binding(path)?);
    }
    review::save_image(ctx, &output, image::DynamicImage::ImageRgb8(panel))?;
    // Catch concurrent input replacement; never certify a panel against bytes
    // that changed while it was being prepared.
    for binding in &bindings {
        ctx.verify(binding)?;
    }
    let report = json!({"algorithm":ALGORITHM,"mode":"background_subtracted","inputs":{"design":bindings[0],"background":bindings[1],"render":bindings[2]},"label_input":labels,"label_evidence":label_evidence,"input_sizes":sizes,"normalized_to":dimensions,"box":[left,top,w,h],"views":files,"panel":ctx.binding(&output)?,"panel_view_size":[pw,ph],"regions":regions(&marks,w as usize,h as usize,left,top),"panel_order":[["design","background","render"],["reference-signal","candidate-signal","difference"]],"threshold":threshold,"tolerance_px":tolerance,"reference_only_pixels":reference_only,"candidate_only_pixels":candidate_only,"reference_signal_pixels":reference.iter().filter(|v|**v).count(),"candidate_signal_pixels":candidate.iter().filter(|v|**v).count(),"quality_approved":false,"visual_review_required":true,"legend":{"blue":"reference-only change from blank","red":"render-only change from blank","gray":"both signals have nearby pixels"},"limitations":["Regions group marked pixels, not semantic labels; inspect their surroundings and unflagged areas too.","Signals include artwork drift from text removal, font differences and intentional edits; they are not OCR or automatic defect verdicts.","Only full-canvas scaling is applied. No local warp or per-label alignment hides displacement.","Subtraction can miss dark text on a dark icon. Inspect the actual render and complete container even when the difference looks small."]});
    let mut report = report;
    report["reference_alignment"] = reference_alignment;
    ctx.write_json(output.with_extension("json"), &report)?;
    Ok(report)
}
