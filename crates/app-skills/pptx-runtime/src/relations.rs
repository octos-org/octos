//! Pixel-backed relation candidates. These measure differences, not design intent.
use crate::{Context, geometry, spec::Spec};
use anyhow::Context as _;
use anyhow::{Result, ensure};
use image::RgbImage;
use serde_json::{Value, json};
use std::collections::VecDeque;

pub const ALGORITHM: &str = "visual-relations-v4";
fn center(b: [f64; 4]) -> [f64; 2] {
    [b[0] + b[2] / 2., b[1] + b[3] / 2.]
}
fn normalized(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace())
        // OCR commonly substitutes typographic hyphens/range dashes. This
        // matching key locates geometry only; it never verifies canonical copy.
        // Do not strip punctuation or conflate mathematical minus with a range.
        .map(|c| match c {
            '•' => '·',
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' => '-',
            _ => c,
        })
        .collect()
}

/// Flood a local colour surface, then fit its two horizontal edges. Reject open,
/// noisy, tiny and non-rectangular regions instead of inventing an owner.
pub fn surface(image: &RgbImage, ink: [f64; 4]) -> Option<Value> {
    let c = center(ink);
    let radius = [(ink[2] * 2.5).max(ink[3] * 6.), (ink[3] * 8.).max(40.)];
    let x0 = (c[0] - radius[0]).max(0.) as u32;
    let y0 = (c[1] - radius[1]).max(0.) as u32;
    let x1 = (c[0] + radius[0]).min(image.width() as f64 - 1.) as u32;
    let y1 = (c[1] + radius[1]).min(image.height() as f64 - 1.) as u32;
    if x1 <= x0
        || y1 <= y0
        || c[0] < 0.
        || c[1] < 0.
        || c[0] >= image.width() as f64
        || c[1] >= image.height() as f64
    {
        return None;
    }
    let (w, h) = (x1 - x0 + 1, y1 - y0 + 1);
    if u64::from(w) * u64::from(h) > 4_000_000 {
        return None;
    }
    let seed = image.get_pixel(c[0] as u32, c[1] as u32).0;
    let mut seen = vec![false; (w * h) as usize];
    let mut queue = VecDeque::from([(c[0] as u32 - x0, c[1] as u32 - y0)]);
    let mut columns = vec![(u32::MAX, 0u32, 0u32); w as usize];
    let mut count = 0usize;
    while let Some((x, y)) = queue.pop_front() {
        let i = (y * w + x) as usize;
        if seen[i] {
            continue;
        }
        seen[i] = true;
        let p = image.get_pixel(x + x0, y + y0).0;
        if p.iter().zip(seed).any(|(a, b)| a.abs_diff(b) > 32) {
            continue;
        }
        if x == 0 || y == 0 || x + 1 == w || y + 1 == h {
            return None;
        }
        count += 1;
        if count > 500_000 {
            return None;
        }
        let col = &mut columns[x as usize];
        col.0 = col.0.min(y);
        col.1 = col.1.max(y);
        col.2 += 1;
        queue.extend([(x - 1, y), (x + 1, y), (x, y - 1), (x, y + 1)]);
    }
    let left = columns.iter().position(|v| v.2 > 0)?;
    let right = columns.iter().rposition(|v| v.2 > 0)?;
    let width = (right - left + 1) as f64;
    if width < ink[2] * 0.8 || count < 100 || width < 20. {
        return None;
    }
    let trim = (width * 0.12).ceil() as usize;
    let samples: Vec<_> = (left + trim..right.saturating_sub(trim))
        .filter(|&x| columns[x].2 > 0)
        .collect();
    if samples.len() < 12 {
        return None;
    }
    let mx = samples.iter().map(|x| *x as f64).sum::<f64>() / samples.len() as f64;
    let fit = |bottom: bool| {
        let yy = |x: usize| {
            if bottom {
                columns[x].1 as f64
            } else {
                columns[x].0 as f64
            }
        };
        let my = samples.iter().map(|x| yy(*x)).sum::<f64>() / samples.len() as f64;
        let den = samples
            .iter()
            .map(|x| (*x as f64 - mx).powi(2))
            .sum::<f64>();
        let slope = samples
            .iter()
            .map(|x| (*x as f64 - mx) * (yy(*x) - my))
            .sum::<f64>()
            / den;
        let err = (samples
            .iter()
            .map(|x| (yy(*x) - my - slope * (*x as f64 - mx)).powi(2))
            .sum::<f64>()
            / samples.len() as f64)
            .sqrt();
        (my, slope, err)
    };
    let (top, st, et) = fit(false);
    let (bottom, sb, eb) = fit(true);
    let height = bottom - top + 1.;
    if height < ink[3] * 0.7
        || height > ink[3] * 8.
        || count as f64 / (width * height) < 0.7
        || et.max(eb) > 3.0f64.max(height * 0.04)
        || (st - sb).abs() > 0.06
    {
        return None;
    }
    let slope = (st + sb) / 2.;
    let quad = [
        [left as f64, top + slope * (left as f64 - mx)],
        [right as f64, top + slope * (right as f64 - mx)],
        [right as f64, bottom + slope * (right as f64 - mx)],
        [left as f64, bottom + slope * (left as f64 - mx)],
    ]
    .map(|p| [p[0] + x0 as f64, p[1] + y0 as f64]);
    Some(
        json!({"quad":quad,"angle_deg":slope.atan().to_degrees(),"edge_rms_px":et.max(eb),
        "pixels":count,"semantic_owner_verified":false,"method":"closed local colour component; parallel fitted top/bottom edges"}),
    )
}

/// Canonical text corroborates OCR identity, not confidence in its geometry.
/// Partial spans support direction only, never a whole block's centre.
pub fn reference(text: &str, ink: [f64; 4], ocr: &Value, scale: [f64; 2]) -> Option<Value> {
    let text = normalized(text);
    if text.chars().count() < 2 {
        return None;
    }
    let mut choices = Vec::new();
    for d in ocr["detections"].as_array().into_iter().flatten() {
        let recognized = normalized(d["text"].as_str().unwrap_or(""));
        let exact = recognized == text;
        let partial = recognized.chars().count() >= 4 && text.contains(&recognized);
        if !(exact || partial) || d["confidence"].as_f64().unwrap_or(0.) <= 0. {
            continue;
        }
        let Ok(mut q) = geometry::quad(&d["quad"]) else {
            continue;
        };
        for p in &mut q {
            p[0] *= scale[0];
            p[1] *= scale[1]
        }
        let (b, a, e) = geometry::plane(q);
        let c = center(b);
        let target = center(ink);
        let distance = (c[0] - target[0]).hypot(c[1] - target[1]);
        if distance > ink[2].max(ink[3] * 3.) || e > b[3] * 0.2 {
            continue;
        }
        choices.push((distance,json!({"quad":q,"angle_deg":a,"center":c,"confidence":d["confidence"],"matched_text":recognized,
            "partial":!exact,"recognition_is_not_layout_validation":true,
            "copy_verified":false,"match_basis":"whitespace_and_typographic_dash_normalized"})));
    }
    if choices.iter().any(|c| c.1["partial"] == false) {
        choices.retain(|c| c.1["partial"] == false);
    }
    choices.sort_by(|a, b| a.0.total_cmp(&b.0));
    if choices.len() > 1 && choices[1].0 < choices[0].0 * 2. + ink[3] {
        let distinct_consistent = choices[0].1["partial"] == true
            && choices.iter().enumerate().all(|(i, c)| {
                choices[..i]
                    .iter()
                    .all(|v| v.1["matched_text"] != c.1["matched_text"])
                    && geometry::angle(
                        c.1["angle_deg"]
                            .as_f64()
                            .expect("geometry candidates include the calculated angle"),
                        choices[0].1["angle_deg"]
                            .as_f64()
                            .expect("geometry candidates include the calculated angle"),
                    )
                    .abs()
                        < 2.
            });
        if !distinct_consistent {
            return None;
        }
    }
    choices.into_iter().next().map(|v| v.1)
}

pub fn page(ctx: &Context, cycle: &Value, spec: &Spec, qa: &Value, i: usize) -> Result<Value> {
    let p = &spec.pages[i];
    let blank =
        crate::review::load_image(ctx, &ctx.verify(&cycle["renders"]["blank"]["pages"][i])?)?
            .to_rgb8();
    let scale = [
        blank.width() as f64 / spec.canvas.width_px,
        blank.height() as f64 / spec.canvas.height_px,
    ];
    let design_pixels = p
        .design
        .as_ref()
        .map(|d| crate::review::load_image(ctx, &ctx.verify(d)?).map(|im| im.to_rgb8()))
        .transpose()?;
    let registration = design_pixels
        .as_ref()
        .map(|im| crate::visual_proof::reference_alignment(im, &blank))
        .unwrap_or_else(|| json!({"status":"unavailable"}));
    let registered = registration["status"] == "supported";
    // Raster text and graphical strokes are separate evidence channels. OCR may
    // miss an arrow completely; native glyph/blank-edge contact does not use OCR.
    let blank_ocr = match crate::ocr::recognize(
        ctx,
        &json!({"input":ctx.verify(&cycle["renders"]["blank"]["pages"][i])?}),
    ) {
        Ok(v) => v,
        Err(e) => json!({"status":"unavailable","reason":e.to_string()}),
    };
    // Vision failure is explicit missing evidence, never a no-defect conclusion.
    let ocr = if let Some(design) = &p.design {
        let path = ctx.verify(design)?;
        match crate::ocr::recognize(ctx, &json!({"input":path})) {
            Ok(v) => v,
            Err(e) => json!({"status":"unavailable","reason":e.to_string()}),
        }
    } else {
        json!({"status":"unavailable","reason":"no reference design"})
    };
    let oscale = [
        spec.canvas.width_px
            / ocr["image"]["width_px"]
                .as_f64()
                .unwrap_or(spec.canvas.width_px),
        spec.canvas.height_px
            / ocr["image"]["height_px"]
                .as_f64()
                .unwrap_or(spec.canvas.height_px),
    ];
    let mut objects = Vec::new();
    let mut signals = Vec::new();
    for t in &p.texts {
        let measured = qa["objects"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|r| r["page"] == i + 1 && r["object_id"] == t.id);
        let ink = measured.and_then(|v| geometry::rect(&v["ink"]["bounds"]).ok());
        let Some(ink) = ink else {
            objects.push(json!({"object_id":t.id,"status":"unknown","reason":"native ink measurement unavailable"}));
            continue;
        };
        let pixel_ink = [
            ink[0] * scale[0],
            ink[1] * scale[1],
            ink[2] * scale[0],
            ink[3] * scale[1],
        ];
        let mut surf = surface(&blank, pixel_ink);
        if let Some(s) = &mut surf {
            let mut q = geometry::quad(&s["quad"])?;
            for point in &mut q {
                point[0] /= scale[0];
                point[1] /= scale[1]
            }
            s["quad"] = json!(q);
            let (b, a, _) = geometry::plane(q);
            let c = center(b);
            let tc = geometry::rotate(center(ink), c, -a);
            s["angle_deg"] = json!(a);
            s["ink_center_delta_px"] = json!([tc[0] - c[0], tc[1] - c[1]]);
        }
        let orig = reference(&t.text, ink, &ocr, oscale);
        // Locate the reference surface from reference pixels, not from the new
        // text position. Moving text must not silently redefine its prior owner.
        let reference_surface = orig.as_ref().and_then(|o| {
            let q = geometry::quad(&o["quad"]).ok()?;
            let xmin = q.iter().map(|p| p[0]).fold(f64::INFINITY, f64::min);
            let ymin = q.iter().map(|p| p[1]).fold(f64::INFINITY, f64::min);
            let xmax = q.iter().map(|p| p[0]).fold(f64::NEG_INFINITY, f64::max);
            let ymax = q.iter().map(|p| p[1]).fold(f64::NEG_INFINITY, f64::max);
            let mut s = surface(
                &blank,
                [
                    xmin * scale[0],
                    ymin * scale[1],
                    (xmax - xmin) * scale[0],
                    (ymax - ymin) * scale[1],
                ],
            )?;
            let mut q = geometry::quad(&s["quad"]).ok()?;
            for p in &mut q {
                p[0] /= scale[0];
                p[1] /= scale[1]
            }
            s["quad"] = json!(q);
            s["angle_deg"] = json!(geometry::plane(q).1);
            Some(s)
        });
        let centers = crate::intent_evidence::centers(
            ink,
            orig.as_ref().unwrap_or(&Value::Null),
            reference_surface.as_ref().unwrap_or(&Value::Null),
            registered,
        );
        let mut push = |code: &str, check: &str, observed: Value| {
            let options = crate::intent_evidence::options(
                code,
                &observed,
                (ink, t.rotation),
                surf.as_ref().unwrap_or(&Value::Null),
                orig.as_ref().unwrap_or(&Value::Null),
                &centers,
                registered,
            );
            let id = format!(
                "relation-{}",
                &crate::fs::digest(
                    json!([ALGORITHM, p.id, t.id, code, &observed, &options])
                        .to_string()
                        .as_bytes()
                )[..20]
            );
            signals.push(json!({"id":id,"page":p.id,"object_id":t.id,"code":code,"check":check,"status":"candidate","observed":observed,
                "intentional_options":options,
                "required_response":"defect, unknown, or intentional with support_id from this signal's measured intentional_options plus a specific basis. No option means unknown/defect; prose alone is not support. Candidates do not prove semantic ownership."}));
        };
        if let Some(m) = measured {
            let contact = &m["ink"]["artwork_contact"];
            if contact["candidate"] == true {
                push(
                    "RASTER_STROKE_CONTACT",
                    "containment",
                    json!({"contact":contact,
                    "blank":cycle["renders"]["blank"]["pages"][i],
                    "interpretation":"Actual glyph pixels touch raster contrast edges. Inspect local original/blank/current images; this is not a semantic arrow classifier or an automatic defect."}),
                );
            }
        }
        let raster = crate::raster_overlap::proposals(
            blank_ocr["detections"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or(&[]),
            &[json!({"object_id":t.id,"text":t.text,"box":pixel_ink,"rotation":0})],
            [blank.width() as f64, blank.height() as f64],
        )?;
        if !raster.is_empty() {
            push(
                "RASTER_OCR_OVERLAP",
                "containment",
                json!({"candidates":raster,
                "bounds_basis":"actual glyph ink envelope, not authored text frame",
                "coordinate_units":"actual derived-blank render pixels",
                "blank":cycle["renders"]["blank"]["pages"][i]}),
            );
        }
        if let Some(s) = &surf {
            let angle = s["angle_deg"]
                .as_f64()
                .context("relations::page: missing required value")?;
            let delta = geometry::angle(t.rotation, angle);
            if delta.abs() > 2.5 {
                push(
                    "SURFACE_DIRECTION_DIFFERS",
                    "direction",
                    json!({"text_degrees":t.rotation,"surface_degrees":angle,"delta_degrees":delta,"surface":s}),
                );
            }
            let dy = s["ink_center_delta_px"][1]
                .as_f64()
                .context("relations::page: missing required value")?;
            if dy.abs() > 3.0f64.max(ink[3] * 0.3) {
                push(
                    "SURFACE_VERTICAL_OFFSET",
                    "alignment",
                    json!({"offset_px":dy,"ink_height_px":ink[3],"surface":s}),
                );
            }
        }
        if let Some(o) = &orig {
            let angle = o["angle_deg"]
                .as_f64()
                .context("relations::page: missing required value")?;
            let delta = geometry::angle(t.rotation, angle);
            if delta.abs() > 2.5 {
                push(
                    "REFERENCE_DIRECTION_DIFFERS",
                    "direction",
                    json!({"text_degrees":t.rotation,"reference_degrees":angle,"delta_degrees":delta,"reference":o}),
                );
            }
            // OCR is a reference anchor, not a mandate to restore old font size/copy.
            let c: [f64; 2] = serde_json::from_value(o["center"].clone())?;
            let current = center(ink);
            let d = [current[0] - c[0], current[1] - c[1]];
            if o["partial"] == false
                && (d[0].abs() > ink[3].max(8.) || d[1].abs() > (ink[3] * 0.5).max(5.))
            {
                push(
                    "REFERENCE_ANCHOR_DIFFERS",
                    "alignment",
                    json!({"delta_px":d,"reference":o,"current_ink":ink,"registration_verified":false}),
                );
            }
        }
        for relation in &centers {
            if relation["changed"] == true {
                push(
                    "REFERENCE_CENTER_RELATION_CHANGED",
                    "alignment",
                    relation.clone(),
                );
            }
        }
        objects.push(
            json!({"object_id":t.id,"status":"measured","ink":ink,"rotation":t.rotation,"surface":surf,"reference":orig,
                "reference_surface":reference_surface,"reference_center_relations":centers}),
        );
    }
    Ok(
        json!({"algorithm":ALGORITHM,"page":p.id,"objects":objects,"signals":signals,
        "ocr":ocr,"blank_ocr":blank_ocr,"reference_alignment":registration,"units":"canvas pixels; degrees clockwise; raster OCR candidates explicitly use render pixels","quality_approved":false,
        "limitations":["Colour surfaces are geometric candidates, not semantic owners; irregular/artistic boundaries may be unknown.",
            "Reference text may intentionally change position or style; OCR matching cannot prove design-to-blank registration.",
            "Raster contact and OCR overlap are automatically surfaced for review, not automatically classified as defects. Missing OCR or weak artwork edges can still miss interference.",
            "Absent signals do not prove correctness; duplicated semantic icons and occlusion still need image review."]}),
    )
}

pub fn validate_responses(signals: &[Value], observations: &Value) -> Result<()> {
    validate_responses_with_route(signals, observations, false)
}
pub fn validate_responses_with_route(
    signals: &[Value],
    observations: &Value,
    independent: bool,
) -> Result<()> {
    let replies = observations["relation_reviews"].as_array();
    let mut issues = Vec::new();
    for s in signals {
        let matching: Vec<_> = replies
            .into_iter()
            .flatten()
            .filter(|r| r["id"] == s["id"])
            .collect();
        let name = format!(
            "{} ({}/{}, {})",
            s["id"], s["page"], s["object_id"], s["code"]
        );
        if matching.len() != 1 {
            issues.push(format!("{name}: requires exactly one disposition"));
            continue;
        }
        let r = matching[0];
        if !matches!(
            r["status"].as_str(),
            Some("defect" | "intentional" | "unknown" | "visual_acceptance")
        ) || !r["basis"].as_str().is_some_and(|x| !x.trim().is_empty())
        {
            issues.push(format!(
                "{name}: requires status defect|intentional|unknown and a specific basis"
            ));
            continue;
        }
        if r["status"] == "intentional" && !crate::intent_evidence::supports(s, r) {
            issues.push(format!("{name}: intentional requires a support_id from this signal's intentional_options; current selection is unsupported. If no measured option applies, keep unknown or defect; another anchor/axis or prose alone does not explain this relation."));
        }
        if r["status"] == "visual_acceptance" && !independent {
            issues.push(format!("{name}: visual_acceptance requires the frozen independent reviewer; it is not measured intentional support"));
        }
        if r["status"] == "defect" || r["status"] == "unknown" {
            let check_name = s["check"]
                .as_str()
                .context("relation signal requires a check name")?;
            let check = observations["object_reviews"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|v| v["page"] == s["page"] && v["object_id"] == s["object_id"])
                .map(|v| &v["checks"][check_name]["status"]);
            if !check
                .is_some_and(|c| c == &r["status"] || (r["status"] == "unknown" && c == "defect"))
            {
                issues.push(format!(
                    "{name}: contradicts object's {} status",
                    s["check"]
                ));
            }
        }
    }
    if replies.is_some_and(|rows| {
        rows.iter()
            .any(|r| !signals.iter().any(|s| s["id"] == r["id"]))
    }) {
        issues.push("unknown/stale relation response".into());
    }
    ensure!(
        issues.is_empty(),
        "Relation review rejected (all issues):\n{}",
        issues.join("\n")
    );
    Ok(())
}
