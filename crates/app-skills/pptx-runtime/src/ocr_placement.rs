//! Use registered source ink as the anchor for reconstructing single-line native text.
use crate::{
    Context, geometry, placement,
    spec::{self, Spec},
};
use anyhow::{Context as _, Result, ensure};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path};

pub(crate) fn bind(
    ctx: &Context,
    page: &mut Value,
    parent: &Path,
    sources: &mut Vec<Value>,
) -> Result<()> {
    let explicit = page.get("source_ocr").is_some_and(|x| !x.is_null());
    let Some(design_hash) = page["design"]["sha256"].as_str() else {
        ensure!(!explicit, "source_ocr requires a bound design");
        return Ok(());
    };
    let mut file = if explicit {
        page["source_ocr"].clone()
    } else {
        let path = parent.join("ocr.json");
        if !path.is_file() {
            return Ok(());
        }
        json!("ocr.json")
    };
    spec::asset(ctx, &mut file, parent)?;
    let data = ctx.read(ctx.verify(&file)?)?;
    let observed: Value = match serde_json::from_slice(&data) {
        Ok(v) => v,
        Err(_) if !explicit => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    if observed["source_sha256"].as_str() != Some(design_hash) {
        ensure!(!explicit, "OCR observation belongs to a different design");
        return Ok(());
    }
    ensure!(
        crate::fs::digest(&data) == file["sha256"],
        "OCR observation changed"
    );
    page["source_ocr"] = file.clone();
    sources.push(file);
    Ok(())
}

fn key(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}
fn center([x, y, w, h]: [f64; 4]) -> [f64; 2] {
    [x + w / 2., y + h / 2.]
}

struct Anchor {
    center: [f64; 2],
    angle: Option<f64>,
    lift: f64,
}

fn anchor(line: &Value, canvas: [f64; 2]) -> Option<Anchor> {
    let rect = geometry::rect(&line["box"]).ok()?;
    let [x, y, w, h] = rect;
    if x < 0. || y < 0. || x + w > canvas[0] + 0.01 || y + h > canvas[1] + 0.01 {
        return None;
    }
    let Some(polygon) = line.get("polygon").or_else(|| line.get("quad")) else {
        return Some(Anchor {
            center: center(rect),
            angle: None,
            lift: h,
        });
    };
    let q = geometry::quad(polygon).ok()?;
    if q.iter()
        .any(|p| p[0] < 0. || p[1] < 0. || p[0] > canvas[0] || p[1] > canvas[1])
    {
        return None;
    }
    let (frame, angle, _) = geometry::plane(q);
    let mid = center(frame);
    let box_mid = center(rect);
    if (mid[0] - box_mid[0]).abs() > w * 0.2 + 1. || (mid[1] - box_mid[1]).abs() > h * 0.2 + 1. {
        return None;
    }
    Some(Anchor {
        center: mid,
        angle: Some(angle),
        lift: frame[3],
    })
}

pub(crate) fn apply(ctx: &Context, spec: &mut Spec) -> Result<Vec<Value>> {
    let mut report = Vec::new();
    let canvas = [spec.canvas.width_px, spec.canvas.height_px];
    for pi in 0..spec.pages.len() {
        let Some(file) = &spec.pages[pi].source_ocr else {
            continue;
        };
        let data = ctx.read(ctx.verify(file)?)?;
        ensure!(
            crate::fs::digest(&data) == file["sha256"],
            "stale OCR observation"
        );
        let o: Value = serde_json::from_slice(&data)?;
        let source_canvas: [f64; 2] = serde_json::from_value(o["canvas"].clone())?;
        ensure!(
            source_canvas.iter().all(|n| n.is_finite() && *n > 0.),
            "invalid OCR canvas"
        );
        let scale = canvas[0] / source_canvas[0];
        ensure!(
            (source_canvas[1] * scale - canvas[1]).abs() <= 0.5,
            "OCR canvas aspect ratio differs from slide"
        );
        let lines = o["lines"].as_array().context("missing source OCR lines")?;
        ensure!(lines.len() <= 10000, "excessive OCR lines");
        let mut by_text: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
        for line in lines {
            if let Some(s) = line["text"].as_str() {
                by_text.entry(key(s)).or_default().push(line);
            }
        }
        let mut counts = BTreeMap::new();
        let mut occurrences = BTreeMap::new();
        for t in &spec.pages[pi].texts {
            *occurrences
                .entry(t.copy_id.as_ref().unwrap_or(&t.id).clone())
                .or_insert(0usize) += 1;
            *counts.entry(key(&t.text)).or_insert(0usize) += 1;
        }
        for i in 0..spec.pages[pi].texts.len() {
            let t = &spec.pages[pi].texts[i];
            if t.copy_range.is_some() || t.text.lines().count() != 1 {
                continue;
            }
            let s = key(&t.text);
            let Some(rows) = by_text.get(&s) else {
                continue;
            };
            let cid = t.copy_id.as_ref().unwrap_or(&t.id).as_str();
            let line = match rows.as_slice() {
                [one] if counts[&s] == 1 => *one,
                many => {
                    let mut found = many
                        .iter()
                        .copied()
                        .filter(|line| line["id"].as_str() == Some(cid));
                    match (found.next(), found.next()) {
                        (Some(one), None) if occurrences[cid] == 1 => one,
                        _ => continue,
                    }
                }
            };
            if line["geometry_status"] == "rejected"
                || line["confidence"]
                    .as_f64()
                    .is_some_and(|p| !(0.8..=1.).contains(&p))
            {
                continue;
            }
            let Some(mut a) = anchor(line, source_canvas) else {
                continue;
            };
            a.center = a.center.map(|coordinate| coordinate * scale);
            a.lift *= scale;
            let mut trial = t.clone();
            if let Some(angle) = a.angle {
                if geometry::angle(angle, trial.rotation).abs() > 3. {
                    trial.rotation = angle;
                }
            }
            let (_, _, measured) = placement::measure(spec, &trial)?;
            if measured["ink"]["status"] != "measured"
                || measured["lines"] != 1
                || measured["ink"]["clipped_by_measurement_window"] == true
            {
                continue;
            }
            let rect = geometry::rect(&measured["ink"]["bounds"])?;
            let at = center(rect);
            let offset = [a.center[0] - at[0], a.center[1] - at[1]];
            // Leave room for subpixel fitting and the difference between the OCR envelope
            // and visible ink. The tolerance scales with the cross-line dimension.
            let tolerance = (a.lift * 0.2).clamp(2., 6.);
            if offset[0].hypot(offset[1]) > tolerance {
                trial.bounds[0] += offset[0];
                trial.bounds[1] += offset[1];
            }
            if trial.bounds == t.bounds && trial.rotation == t.rotation {
                continue;
            }
            let delta = [trial.bounds[0] - t.bounds[0], trial.bounds[1] - t.bounds[1]];
            let [x, y, w, h] = rect;
            let ink = [[x, y], [x + w, y], [x + w, y + h], [x, y + h]]
                .map(|p| [p[0] + delta[0], p[1] + delta[1]]);
            if ink
                .iter()
                .any(|p| p[0] < 0. || p[1] < 0. || p[0] > canvas[0] || p[1] > canvas[1])
            {
                continue;
            }
            if let Some(region) = &trial.region {
                let owner = geometry::quad(&spec.pages[pi].regions[region])?;
                if !geometry::contains(owner, a.center, 0.)
                    || !geometry::frame(trial.bounds, trial.rotation)
                        .iter()
                        .all(|&p| geometry::contains(owner, p, 0.5))
                {
                    continue;
                }
            }
            report.push(json!({"page":spec.pages[pi].id,"text_id":t.id,"ocr_id":line["id"],"source_line":line,
                 "original_box":t.bounds,"resolved_box":trial.bounds,"original_rotation":t.rotation,"resolved_rotation":trial.rotation,
                 "source_ink_center":a.center,"measured_native_ink_center":at}));
            spec.pages[pi].texts[i] = trial;
        }
    }
    Ok(report)
}
