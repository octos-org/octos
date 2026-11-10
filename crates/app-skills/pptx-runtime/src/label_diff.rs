//! Label-sized evidence from pixels, not OCR rectangles or a quality verdict.
//! Geometry is only a search hint. Long thin artwork is kept in context views
//! but excluded from the glyph estimate. Ambiguous estimates stay explicit.
use crate::{Context, geometry, review};
use anyhow::Context as _;
use anyhow::{Result, ensure};
use image::{Rgb, RgbImage, imageops};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::VecDeque, path::Path};

pub const ALGORITHM: &str = "label-context-geometry-v1";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Label {
    pub id: String,
    pub text: String,
    #[serde(rename = "box")]
    pub bounds: [f64; 4],
    pub font_px: f64,
    #[serde(default)]
    pub rotation: f64,
    #[serde(default)]
    pub native_ink_box: Option<[f64; 4]>,
}

type Rect = [f64; 4];

fn clipped(b: Rect, w: u32, h: u32) -> Rect {
    let l = b[0].floor().clamp(0., f64::from(w));
    let t = b[1].floor().clamp(0., f64::from(h));
    let r = (b[0] + b[2]).ceil().clamp(l, f64::from(w));
    let b = (b[1] + b[3]).ceil().clamp(t, f64::from(h));
    [l, t, r - l, b - t]
}

fn expanded(b: Rect, x: f64, y: f64, w: u32, h: u32) -> Rect {
    clipped([b[0] - x, b[1] - y, b[2] + 2. * x, b[3] + 2. * y], w, h)
}

fn union(a: Rect, b: Rect) -> Rect {
    let l = a[0].min(b[0]);
    let t = a[1].min(b[1]);
    [
        l,
        t,
        (a[0] + a[2]).max(b[0] + b[2]) - l,
        (a[1] + a[3]).max(b[1] + b[3]) - t,
    ]
}

fn intersects(a: Rect, b: Rect) -> bool {
    a[0] < b[0] + b[2] && b[0] < a[0] + a[2] && a[1] < b[1] + b[3] && b[1] < a[1] + a[3]
}

/// Estimate visible glyph extent from connected subtraction components. Does
/// not transcribe or match words. Never feeds a local warp back into comparison.
fn ink(mask: &[bool], width: u32, height: u32, area: Rect, em: f64) -> Option<Rect> {
    let area = clipped(area, width, height);
    let (l, t, w, h) = (
        area[0] as usize,
        area[1] as usize,
        area[2] as usize,
        area[3] as usize,
    );
    let mut seen = vec![false; w * h];
    let mut result = None;
    for start in 0..w * h {
        if seen[start] || !mask[(t + start / w) * width as usize + l + start % w] {
            continue;
        }
        let mut queue = VecDeque::from([start]);
        seen[start] = true;
        let (mut x0, mut y0, mut x1, mut y1, mut count) = (w, h, 0, 0, 0);
        while let Some(i) = queue.pop_front() {
            let (x, y) = (i % w, i / w);
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
            count += 1;
            for ny in y.saturating_sub(1)..=(y + 1).min(h - 1) {
                for nx in x.saturating_sub(1)..=(x + 1).min(w - 1) {
                    let j = ny * w + nx;
                    if !seen[j] && mask[(t + ny) * width as usize + l + nx] {
                        seen[j] = true;
                        queue.push_back(j);
                    }
                }
            }
        }
        let (cw, ch) = ((x1 - x0 + 1) as f64, (y1 - y0 + 1) as f64);
        // Dust pixels and long thin lines cannot establish the start of a word.
        if count < (em * em * 0.025).max(2.) as usize
            || ch < em * 0.4
            || (count as f64) / (cw * ch) < 0.08
            || (cw > em * 1.2 && ch < em * 0.22)
        {
            continue;
        }
        let b = [(l + x0) as f64, (t + y0) as f64, cw, ch];
        result = Some(result.map_or(b, |old| union(old, b)));
    }
    result
}

#[derive(Clone, Copy)]
struct Line {
    a: f64,
    b: f64,
    cross: f64,
    vertical: bool,
}

/// Axis-aligned thin contrast runs in the blank artwork. This intentionally
/// reports a geometric relationship, not whether an underline is intentional.
fn lines(blank: &RgbImage, area: Rect, em: f64) -> Vec<Line> {
    let mut out: Vec<Line> = Vec::new();
    let (w, h) = blank.dimensions();
    let area = clipped(area, w, h);
    let offset = (em * 0.13).ceil().clamp(2., 7.) as u32;
    for vertical in [false, true] {
        let (axis, cross, axis_len, cross_len, limit) = if vertical {
            (
                area[1] as u32,
                area[0] as u32,
                area[3] as u32,
                area[2] as u32,
                w,
            )
        } else {
            (
                area[0] as u32,
                area[1] as u32,
                area[2] as u32,
                area[3] as u32,
                h,
            )
        };
        let pixel = |a, c| {
            if vertical {
                *blank.get_pixel(c, a)
            } else {
                *blank.get_pixel(a, c)
            }
        };
        for c in cross.max(offset)..(cross + cross_len).min(limit.saturating_sub(offset)) {
            let mut run = None;
            let mut last = axis;
            for a in axis..=axis + axis_len {
                let marked = a < axis + axis_len && {
                    let p = pixel(a, c);
                    let n = pixel(a, c - offset);
                    let s = pixel(a, c + offset);
                    (0..3).any(|k| {
                        let d1 = i16::from(p[k]) - i16::from(n[k]);
                        let d2 = i16::from(p[k]) - i16::from(s[k]);
                        d1.abs() >= 32 && d2.abs() >= 32 && d1.signum() == d2.signum()
                    })
                };
                if marked {
                    run.get_or_insert(a);
                    last = a;
                }
                if let Some(start) = run
                    && (a == axis + axis_len || a.saturating_sub(last) > 2)
                {
                    if f64::from(last - start + 1) >= (em * 0.8).max(16.) {
                        let next = Line {
                            a: f64::from(start),
                            b: f64::from(last),
                            cross: f64::from(c),
                            vertical,
                        };
                        if !out.iter().any(|p| {
                            p.vertical == vertical
                                && (p.cross - next.cross).abs() <= f64::from(offset)
                                && p.a <= next.b
                                && next.a <= p.b
                        }) {
                            out.push(next);
                        }
                    }
                    run = None;
                }
            }
        }
    }
    out
}

fn clearance(line: Line, b: Rect) -> Option<f64> {
    let (start, end, top, bottom) = if line.vertical {
        (b[1], b[1] + b[3], b[0], b[0] + b[2])
    } else {
        (b[0], b[0] + b[2], b[1], b[1] + b[3])
    };
    let along = (line.a - end).max(start - line.b).max(0.);
    let across = (top - line.cross).max(line.cross - bottom).max(0.);
    Some(if along > 0. || across > 0. {
        along.hypot(across)
    } else {
        -(end.min(line.b) - start.max(line.a))
    })
}

fn panel(ctx: &Context, path: &Path, area: Rect, views: &[RgbImage]) -> Result<Value> {
    let (cw, ch) = (area[2] as u32, area[3] as u32);
    let scale = (640. / f64::from(cw)).min(1.);
    let (pw, ph) = (
        (f64::from(cw) * scale).max(1.) as u32,
        (f64::from(ch) * scale).max(1.) as u32,
    );
    let mut canvas = RgbImage::from_pixel(pw * 3, ph, Rgb([255, 255, 255]));
    for (i, view) in [0, 2, 5].into_iter().enumerate() {
        let crop =
            imageops::crop_imm(&views[view], area[0] as u32, area[1] as u32, cw, ch).to_image();
        let crop = imageops::resize(&crop, pw, ph, imageops::FilterType::Triangle);
        imageops::replace(&mut canvas, &crop, i64::from(pw) * i as i64, 0);
    }
    review::save_image(ctx, path, image::DynamicImage::ImageRgb8(canvas))?;
    ctx.binding(path)
}

pub fn labels_for_page(
    spec: &crate::spec::Spec,
    page: &crate::spec::Page,
    w: u32,
    h: u32,
) -> Result<Vec<Label>> {
    let (sx, sy) = (
        f64::from(w) / spec.canvas.width_px,
        f64::from(h) / spec.canvas.height_px,
    );
    page.texts
        .iter()
        .map(|t| {
            Ok(Label {
                id: t.id.clone(),
                text: t.text.clone(),
                bounds: [
                    t.bounds[0] * sx,
                    t.bounds[1] * sy,
                    t.bounds[2] * sx,
                    t.bounds[3] * sy,
                ],
                font_px: spec.style(t)?.size_pt * f64::from(w) / (spec.canvas.width_in * 72.),
                rotation: t.rotation,
                native_ink_box: None,
            })
        })
        .collect()
}

pub fn analyze(
    ctx: &Context,
    output: &Path,
    labels: &[Label],
    views: &[RgbImage],
    reference: &[bool],
    candidate: &[bool],
) -> Result<Value> {
    let (w, h) = views[0].dimensions();
    ensure!(labels.len() <= 10000, "too many comparison labels");
    let mut ids = std::collections::BTreeSet::new();
    let mut rows = Vec::new();
    let mut findings = Vec::new();
    for (index, label) in labels.iter().enumerate() {
        ensure!(
            !label.id.is_empty()
                && ids.insert(&label.id)
                && label.font_px.is_finite()
                && label.font_px > 0.
                && label.rotation.is_finite(),
            "invalid/duplicate comparison label"
        );
        geometry::rect(&json!(label.bounds))?;
        if let Some(b) = label.native_ink_box {
            geometry::rect(&json!(b))?;
        }
        let em = label.font_px;
        let mut context = expanded(label.bounds, em * 3., em * 1.5, w, h);
        // Out-of-canvas labels still need evidence so reviewers can record a
        // defect rather than getting trapped behind a missing-crop hash.
        if context[2] == 0. || context[3] == 0. {
            context = [0., 0., f64::from(w), f64::from(h)];
        }
        let candidate_area = expanded(label.bounds, em * 0.15, em * 0.15, w, h);
        let actual = label
            .native_ink_box
            .or_else(|| ink(candidate, w, h, candidate_area, em));
        let reference_area = expanded(
            label.native_ink_box.unwrap_or(label.bounds),
            em * 3.,
            em * 0.3,
            w,
            h,
        );
        let expected = ink(reference, w, h, reference_area, em);
        let ambiguous = labels.iter().any(|other| {
            other.id != label.id
                && intersects(reference_area, other.native_ink_box.unwrap_or(other.bounds))
        });
        let nearby_lines = lines(&views[1], context, em);
        // Subtraction may retain a shifted connector/dot from blank creation.
        // Do not use that contaminated reference envelope to excuse a collision.
        let reference_touches_artwork = expected.is_some_and(|b| {
            nearby_lines
                .iter()
                .any(|line| clearance(*line, b).is_some_and(|gap| gap <= 0.))
        });
        let reliable = !ambiguous
            && !reference_touches_artwork
            && label.rotation.abs() < 1.
            && expected.is_some()
            && actual.is_some();
        let stem = output
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("comparison");
        let panel_path = output.with_file_name(format!("{stem}-label-{:03}.png", index + 1));
        let panel_binding = panel(ctx, &panel_path, context, views)?;
        let mut metrics = json!({"reference_ink_box":expected,"candidate_ink_box":actual,"candidate_ink_method":if label.native_ink_box.is_some(){"isolated_native_render"}else{"pixel_component_estimate"},
            "reference_geometry_reliable":reliable,"nearby_labels_make_reference_ambiguous":ambiguous,
            "reference_signal_may_include_artwork":reference_touches_artwork});
        let mut label_findings = Vec::new();
        let mut add = |kind: &str, measurements: Value| {
            let id = format!("label-{:03}-{:03}", index + 1, label_findings.len() + 1);
            label_findings.push(json!({"id":id,"kind":kind,"measurements":measurements}));
        };
        if !reliable {
            add(
                "reference_geometry_uncertain",
                json!({"rotation":label.rotation,"ambiguous":ambiguous,"reference_signal_may_include_artwork":reference_touches_artwork,
            "reference_ink_found":expected.is_some(),"candidate_ink_found":actual.is_some()}),
            );
        }
        if let (Some(a), Some(b)) = (expected, actual) {
            let delta = [b[0] - a[0], b[1] - a[1], b[2] - a[2], b[3] - a[3]];
            metrics["ink_delta_xywh_px"] = json!(delta);
            if reliable && (delta[0].abs() > em * 0.35 || delta[1].abs() > em * 0.35) {
                add(
                    "ink_position_changed",
                    json!({"delta_xy_px":[delta[0],delta[1]],"tolerance_px":em*0.35}),
                );
            }
            if reliable && (delta[2].abs() > em || delta[3].abs() > em * 0.5) {
                add(
                    "ink_extent_changed",
                    json!({"delta_wh_px":[delta[2],delta[3]],"font_px":em}),
                );
            }
        }
        let mut line_metrics = Vec::new();
        if let Some(actual) = actual {
            for line in nearby_lines {
                let ac = clearance(line, actual);
                let rc = expected.and_then(|b| clearance(line, b));
                if ac.is_some_and(|gap| gap < em * 0.2) {
                    let m = json!({"orientation":if line.vertical{"vertical"}else{"horizontal"},
                        "axis_start_px":line.a,"axis_end_px":line.b,"cross_px":line.cross,
                        "candidate_clearance_px":ac,"reference_clearance_px":rc,
                        "reference_geometry_reliable":reliable,"clearance_tolerance_px":em*0.2});
                    line_metrics.push(m.clone());
                    add("artwork_line_clearance", m);
                }
            }
        }
        metrics["nearby_artwork_lines"] = json!(line_metrics);
        for f in &label_findings {
            let mut f = f.clone();
            f["object_id"] = json!(label.id);
            f["text"] = json!(label.text);
            f["evidence"] = panel_binding.clone();
            findings.push(f);
        }
        rows.push(json!({"object_id":label.id,"text":label.text,"declared_box":label.bounds,"font_px":em,
            "context_box":context,"panel":panel_binding,"metrics":metrics,"findings":label_findings}));
    }
    // A repeated vertical label group is inferred only from stable reference
    // ink left edges, similar type size and nearby rows, never candidate owners.
    for i in 0..rows.len() {
        if rows[i]["metrics"]["reference_geometry_reliable"] != true {
            continue;
        }
        let r: Rect = serde_json::from_value(rows[i]["metrics"]["reference_ink_box"].clone())?;
        let em = labels[i].font_px;
        let mut group = Vec::new();
        for j in 0..rows.len() {
            if rows[j]["metrics"]["reference_geometry_reliable"] != true
                || (labels[j].font_px / em - 1.).abs() >= 0.35
            {
                continue;
            }
            let reference = geometry::rect(&rows[j]["metrics"]["reference_ink_box"])?;
            if (reference[0] - r[0]).abs() < em * 0.25 && (reference[1] - r[1]).abs() < em * 15. {
                group.push(j);
            }
        }
        if group.len() < 3 || group[0] != i {
            continue;
        }
        let xs: Vec<_> = group
            .iter()
            .map(|&j| {
                rows[j]["metrics"]["candidate_ink_box"][0]
                    .as_f64()
                    .context("alignment candidate requires an x coordinate")
            })
            .collect::<Result<Vec<_>>>()?;
        let spread = xs.iter().copied().fold(f64::NEG_INFINITY, f64::max)
            - xs.iter().copied().fold(f64::INFINITY, f64::min);
        if spread > em * 0.5 {
            let area = group
                .iter()
                .map(|&j| geometry::rect(&rows[j]["context_box"]))
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .reduce(union)
                .context("label_diff::analyze: missing required value")?;
            let stem = output
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("comparison");
            let evidence = panel(
                ctx,
                &output.with_file_name(format!("{stem}-alignment-{:03}.png", i + 1)),
                area,
                views,
            )?;
            findings.push(json!({"id":format!("alignment-{:03}",i+1),"kind":"peer_left_alignment_changed",
                "object_ids":group.iter().map(|&j|labels[j].id.clone()).collect::<Vec<_>>(),
                "measurements":{"candidate_left_edges_px":xs,"spread_px":spread,"tolerance_px":em*0.5},
                "evidence":evidence}));
        }
    }
    // Native isolated ink avoids confusing a tall text frame with visible
    // letters, or accidentally measuring a neighbor as part of this label.
    for (i, a) in labels.iter().enumerate() {
        let Some(ab) = a.native_ink_box else { continue };
        for (j, b) in labels.iter().enumerate().skip(i + 1) {
            let Some(bb) = b.native_ink_box else { continue };
            let dx = (ab[0].max(bb[0]) - (ab[0] + ab[2]).min(bb[0] + bb[2])).max(0.);
            let dy = (ab[1].max(bb[1]) - (ab[1] + ab[3]).min(bb[1] + bb[3])).max(0.);
            let threshold = a.font_px.min(b.font_px) * 0.15;
            if dx.hypot(dy) >= threshold {
                continue;
            }
            let area = expanded(
                union(ab, bb),
                a.font_px.max(b.font_px),
                a.font_px.max(b.font_px),
                w,
                h,
            );
            if area[2] == 0. || area[3] == 0. {
                continue;
            }
            let stem = output
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("comparison");
            let evidence = panel(
                ctx,
                &output.with_file_name(format!("{stem}-neighbors-{:03}-{:03}.png", i + 1, j + 1)),
                area,
                views,
            )?;
            findings.push(json!({"id":format!("neighbors-{:03}-{:03}",i+1,j+1),"kind":"neighbor_label_clearance",
                "object_ids":[a.id,b.id],"measurements":{"candidate_ink_boxes":[ab,bb],"gap_px":dx.hypot(dy),
                "envelopes_intersect":intersects(ab,bb),"clearance_tolerance_px":threshold},"evidence":evidence}));
        }
    }
    Ok(
        json!({"algorithm":ALGORITHM,"labels":rows,"findings":findings,"quality_approved":false,
        "panel_order":["design","render","difference"],
        "limitations":["Pixel components estimate glyph extent; they do not transcribe or establish semantic ownership.",
        "Reference estimates become uncertain near other labels, missing signal or rotated text; use the context view and original.",
        "Line measurements cover axis-aligned thin contrast runs, not all curves, diagonal lines or perspective.",
        "Negative clearance means the detected line enters the estimated ink envelope; intentional underlines still need model judgment.",
        "A clean estimate does not establish visual quality. Review unflagged labels and the complete page too."]}),
    )
}
