use crate::{Context, geometry, spec::Spec, string};
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    io::{Cursor, Read},
    path::Path,
};

pub fn preflight(ctx: &Context, path: &Path) -> Result<Value> {
    let data = ctx.read(path)?;
    let mut zip = zip::ZipArchive::new(Cursor::new(data))?;
    ensure!(zip.len() <= 20000, "package has too many parts");
    let mut names = BTreeSet::new();
    let mut issues = Vec::new();
    let mut size = 0u64;
    for i in 0..zip.len() {
        let f = zip.by_index(i)?;
        size = size.saturating_add(f.size());
        ensure!(
            size <= 512 * 1024 * 1024,
            "expanded package exceeds 512 MiB"
        );
        if !names.insert(f.name().to_string()) {
            issues.push(format!("duplicate part {}", f.name()))
        }
    }
    for needed in ["[Content_Types].xml", "_rels/.rels", "ppt/presentation.xml"] {
        if !names.contains(needed) {
            issues.push(format!("missing {needed}"))
        }
    }
    for name in &names {
        if !(name.ends_with(".xml") || name.ends_with(".rels")) {
            continue;
        }
        let mut f = zip.by_name(name)?;
        ensure!(f.size() <= 32 * 1024 * 1024, "XML part exceeds 32 MiB");
        let mut s = String::new();
        f.read_to_string(&mut s)?;
        if let Err(e) = deckcraft_pptx::parse_xml(s.as_bytes()) {
            issues.push(format!("invalid XML {name}: {e}"));
        }
        shape_xml_issues(name, &s, &mut issues)?;
        if name.ends_with(".rels") {
            let mut reader = quick_xml::Reader::from_str(&s);
            let mut ids = BTreeSet::new();
            loop {
                match reader.read_event() {
                    Ok(
                        quick_xml::events::Event::Empty(el) | quick_xml::events::Event::Start(el),
                    ) if el.local_name().as_ref() == b"Relationship" => {
                        let mut target = String::new();
                        let mut external = false;
                        for a in el.attributes() {
                            let a = a?;
                            let value = a
                                .decoded_and_normalized_value(
                                    quick_xml::XmlVersion::Implicit1_0,
                                    reader.decoder(),
                                )?
                                .into_owned();
                            match a.key.as_ref() {
                                b"Id" if !ids.insert(value.clone()) => {
                                    issues
                                        .push(format!("duplicate relationship id {name}: {value}"));
                                }
                                b"Target" => target = value,
                                b"TargetMode" => external = value == "External",
                                _ => {}
                            }
                        }
                        if !external && !target.is_empty() {
                            let base = if name == "_rels/.rels" {
                                ""
                            } else {
                                name.rsplit_once("/_rels/").map(|(a, _)| a).unwrap_or("")
                            };
                            let raw = if target.starts_with('/') {
                                target.trim_start_matches('/').to_string()
                            } else {
                                format!("{base}/{target}")
                            };
                            let mut parts = Vec::new();
                            let mut escaped = false;
                            for x in raw.split('/') {
                                match x {
                                    "" | "." => {}
                                    ".." => {
                                        if parts.pop().is_none() {
                                            escaped = true
                                        }
                                    }
                                    _ => parts.push(x),
                                }
                            }
                            let resolved = parts.join("/");
                            if escaped || !names.contains(&resolved) {
                                issues
                                    .push(format!("missing relationship target {name}: {target}"));
                            }
                        }
                    }
                    Ok(quick_xml::events::Event::Eof) => break,
                    Err(_) => break,
                    _ => {}
                }
            }
        }
    }
    Ok(
        json!({"issue_count":issues.len(),"issues":issues,"parts":names.len(),"scope":"bounded XML and package relationships; not PowerPoint application certification"}),
    )
}
fn shape_xml_issues(name: &str, s: &str, issues: &mut Vec<String>) -> Result<()> {
    let mut reader = quick_xml::Reader::from_str(s);
    let mut shape_ids = BTreeSet::new();
    let mut autofit: Option<usize> = None;
    loop {
        match reader.read_event() {
            Ok(quick_xml::events::Event::Start(el) | quick_xml::events::Event::Empty(el)) => {
                let local = el.local_name();
                match local.as_ref() {
                    b"cNvPr" => {
                        for a in el.attributes() {
                            let a = a?;
                            if a.key.as_ref() == b"id" {
                                let id = a
                                    .decoded_and_normalized_value(
                                        quick_xml::XmlVersion::Implicit1_0,
                                        reader.decoder(),
                                    )?
                                    .into_owned();
                                if !shape_ids.insert(id.clone()) {
                                    issues.push(format!("duplicate shape id {name}: {id}"));
                                }
                            }
                        }
                    }
                    b"bodyPr" => autofit = Some(0),
                    b"noAutofit" | b"normAutofit" | b"spAutoFit" => {
                        if let Some(n) = &mut autofit {
                            *n += 1;
                            if *n > 1 {
                                issues.push(format!("multiple text autofit modes in {name}"));
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(quick_xml::events::Event::End(el)) if el.local_name().as_ref() == b"bodyPr" => {
                autofit = None
            }
            Ok(quick_xml::events::Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    Ok(())
}
pub fn bound_cycle(ctx: &Context, path: &Path) -> Result<(Value, Spec)> {
    let v = ctx.json(path)?;
    for key in ["spec", "editable", "blank"] {
        ctx.verify(&v[key])?;
    }
    let spec: Spec = serde_json::from_slice(&ctx.read(ctx.verify(&v["spec"])?)?)?;
    spec.validate()?;
    ensure!(
        v["pages"] == json!(spec.pages.iter().map(|p| &p.id).collect::<Vec<_>>()),
        "page order changed"
    );
    for p in &spec.pages {
        ctx.verify(&p.background)?;
        for layer in &p.artwork_layers {
            ctx.verify(&layer.image)?;
        }
        if let Some(design) = &p.design {
            ctx.verify(design)?;
        }
    }
    for kind in ["editable", "blank"] {
        let r = &v["renders"][kind];
        ensure!(r["input"] == v[kind], "render input differs from {kind}");
        let pages = r["pages"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("cycle lacks checked renders"))?;
        ensure!(
            pages.len() == spec.pages.len(),
            "render page count mismatch"
        );
        for (i, p) in pages.iter().enumerate() {
            let file = ctx.verify(p)?;
            ensure!(p["page"] == json!(i + 1), "render page order mismatch");
            let size = image::image_dimensions(&file)?;
            ensure!(
                p["width"] == size.0 && p["height"] == size.1,
                "render dimensions changed"
            );
        }
    }
    Ok((v, spec))
}
pub fn inspect(ctx: &Context, r: &Value) -> Result<Value> {
    let (path, spec, source_spec, cycle) = if let Some(cycle) = r["cycle"].as_str() {
        let (v, s) = bound_cycle(ctx, &ctx.path(cycle)?)?;
        (
            ctx.verify(&v["editable"])?,
            Some(s),
            v["spec"].clone(),
            Some(v),
        )
    } else {
        (ctx.path(string(r, "input")?)?, None, Value::Null, None)
    };
    let doc = crate::deck::open(ctx, &path)?;
    let package = preflight(ctx, &path)?;
    let fonts = deckcraft_engine::fonts::inspect_fonts(&doc);
    let font_rows = fonts
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("font report must be an array"))?;
    let font_summary = json!({"runs":font_rows.len(),"fallback_runs":font_rows.iter().filter(|f|f["reason"]!="requested").count(),"missing_glyph_runs":font_rows.iter().filter(|f|f["missingGlyph"]==true).count(),"synthetic_style_runs":font_rows.iter().filter(|f|f["syntheticBold"]==true||f["syntheticItalic"]==true).count()});
    let mut rows = Vec::new();
    for (i, slide) in doc.slides.iter().enumerate() {
        let page_scale = spec
            .as_ref()
            .map(|s| s.canvas.width_px / (s.canvas.width_in * 72.))
            .unwrap_or(1.);
        let artwork = cycle
            .as_ref()
            .map(|c| {
                let blank =
                    load_image(ctx, &ctx.verify(&c["renders"]["blank"]["pages"][i])?)?.to_rgb8();
                Ok::<_, anyhow::Error>(crate::artwork_contact::EdgeMap::new(
                    &blank,
                    [
                        doc.slide_size.width * page_scale,
                        doc.slide_size.height * page_scale,
                    ],
                ))
            })
            .transpose()?;
        let context = deckcraft_engine::model::resolve::Ctx::for_slide(&doc, slide)
            .ok_or_else(|| anyhow::anyhow!("missing slide layout"))?;
        for sh in &slide.shapes {
            let Some(body) = &sh.text else { continue };
            let xf = deckcraft_engine::model::resolve::xfrm(&context, sh);
            let textrect = deckcraft_engine::render::shape_geometry(sh, xf.w, xf.h).text_rect;
            let layout = deckcraft_text::layout(
                &context,
                sh,
                body,
                &deckcraft_text::Opts {
                    rect: textrect,
                    fields: &deckcraft_text::NoFields,
                    prompt_color: None,
                    no_shrink: false,
                },
            );
            let mut findings = Vec::new();
            let actual_fonts: Vec<_> = font_rows
                .iter()
                .filter(|f| f["slide"] == json!(i + 1) && f["shape"] == json!(sh.id.0))
                .collect();
            if actual_fonts.iter().any(|f| f["reason"] != "requested") {
                findings.push("font_fallback");
            }
            if actual_fonts.iter().any(|f| f["missingGlyph"] == true) {
                findings.push("missing_glyph");
            }
            if actual_fonts
                .iter()
                .any(|f| f["syntheticBold"] == true || f["syntheticItalic"] == true)
            {
                findings.push("synthetic_font_style");
            }
            if layout.overflow {
                findings.push("text_layout_overflow");
            }
            let scale = spec
                .as_ref()
                .map(|s| s.canvas.width_px / (s.canvas.width_in * 72.))
                .unwrap_or(1.);
            let bounds = [xf.x, xf.y, xf.w, xf.h].map(|n| n * scale);
            let q = geometry::frame(bounds, xf.rot);
            let canvas = [doc.slide_size.width * scale, doc.slide_size.height * scale];
            if q.iter()
                .any(|p| p[0] < 0. || p[1] < 0. || p[0] > canvas[0] || p[1] > canvas[1])
            {
                findings.push("frame_outside_slide");
            }
            let id = sh.name.strip_prefix("octos:text:");
            let p = spec.as_ref().and_then(|s| s.pages.get(i));
            let t = p.and_then(|p| p.texts.iter().find(|t| Some(t.id.as_str()) == id));
            let font_runs = crate::typography::font_runs(&context, sh);
            let declared_style = t
                .and_then(|t| spec.as_ref().map(|s| s.style(t)))
                .transpose()?;
            if declared_style
                .as_ref()
                .is_some_and(|s| crate::typography::declared_mismatch(&font_runs, s))
            {
                findings.push("declared_font_mismatch");
            }
            let mut baseline_measurement = Value::Null;
            if let Some(t) = t {
                if crate::spec::normalized(&body.text()) != crate::spec::normalized(&t.text) {
                    findings.push("roundtrip_copy_mismatch");
                }
                if let Some(owner) = t
                    .region
                    .as_ref()
                    .and_then(|key| p.and_then(|p| p.regions.get(key)))
                {
                    let region = geometry::quad(owner)?;
                    if q.iter()
                        .any(|point| !geometry::contains(region, *point, 0.1))
                    {
                        findings.push("frame_outside_owner");
                    }
                    let baseline = owner["baseline_deg"]
                        .as_f64()
                        .unwrap_or(geometry::plane(region).1);
                    let tolerance = owner["angle_tolerance_deg"].as_f64().unwrap_or(2.);
                    let delta = geometry::angle(xf.rot, baseline);
                    baseline_measurement = json!({
                        "unit":"degrees", "actual_deg":geometry::angle(xf.rot, 0.),
                        "expected_deg":geometry::angle(baseline, 0.), "delta_deg":delta,
                        "tolerance_deg":tolerance,
                        "target_source":if owner["baseline_deg"].is_number(){"owner.baseline_deg"}else{"owner.quad"},
                        "owner_region":t.region
                    });
                    if delta.abs() > tolerance {
                        findings.push("label_baseline_mismatch");
                    }
                    if owner["scope"] != "label" {
                        findings.push("local_label_ownership_unverified");
                    }
                } else {
                    findings.push("local_label_ownership_unverified");
                }
            }
            let owner = t
                .and_then(|t| t.region.as_ref())
                .and_then(|key| p.and_then(|p| p.regions.get(key)))
                .map(geometry::quad)
                .transpose()?;
            let owner_value = t
                .and_then(|t| t.region.as_ref())
                .and_then(|key| p.and_then(|p| p.regions.get(key)));
            let forbidden: Vec<_> = owner_value
                .and_then(|v| v["forbidden"].as_array())
                .into_iter()
                .flatten()
                .map(geometry::quad)
                .collect::<Result<_>>()?;
            let ink = measure_ink(
                &doc,
                i,
                sh,
                xf,
                &layout,
                scale,
                (owner, &forbidden, artwork.as_ref()),
            )?;
            if ink["outside_owner_pixels"].as_u64().is_some_and(|n| n > 0) {
                findings.push("ink_outside_owner");
            }
            if ink["clipped_by_measurement_window"] == true {
                findings.push("ink_measurement_incomplete");
            }
            let mut corners = owner.unwrap_or(q).to_vec();
            corners.extend(q);
            if let Ok(b) = geometry::rect(&ink["bounds"]) {
                corners.extend(geometry::frame(b, 0.));
            }
            let left = (corners.iter().map(|p| p[0]).fold(f64::INFINITY, f64::min) - 16.)
                .clamp(0., canvas[0] - 1.);
            let top = (corners.iter().map(|p| p[1]).fold(f64::INFINITY, f64::min) - 16.)
                .clamp(0., canvas[1] - 1.);
            let right = (corners
                .iter()
                .map(|p| p[0])
                .fold(f64::NEG_INFINITY, f64::max)
                + 16.)
                .clamp(left + 1., canvas[0]);
            let bottom = (corners
                .iter()
                .map(|p| p[1])
                .fold(f64::NEG_INFINITY, f64::max)
                + 16.)
                .clamp(top + 1., canvas[1]);
            rows.push(json!({"page":i+1,"page_id":p.map(|p|&p.id),"object_id":id,"shape_id":sh.id.0,"text":body.text(),"box":bounds,"rotation":xf.rot,"baseline":baseline_measurement,"region":t.and_then(|t|t.region.as_ref()),"context_viewport":[left/canvas[0],top/canvas[1],right/canvas[0],bottom/canvas[1]],"layout":{"content_width_pt":layout.content_width,"content_height_pt":layout.content_height,"lines":layout.lines.len(),"overflow":layout.overflow},"ink":ink,"findings":findings}));
            let row = rows.last_mut().expect("object was just pushed");
            row["font_runs"] = json!(font_runs);
            row["declared_style"] = json!(declared_style);
            row["rendered_font_sizes_pt"] =
                json!(layout.runs.iter().map(|r| r.size).collect::<Vec<_>>());
            row["font_scale"] = json!(layout.font_scale);
        }
    }
    let composite = cycle
        .as_ref()
        .map(|c| crate::composite_ink::audit(ctx, c, &doc))
        .transpose()?;
    if let Some(composite) = &composite {
        for row in &mut rows {
            row["composite_ink"] = composite["objects"]
                .as_array()
                .and_then(|objects| {
                    objects
                        .iter()
                        .find(|o| o["page"] == row["page"] && o["shape_id"] == row["shape_id"])
                })
                .cloned()
                .unwrap_or_else(
                    || json!({"status":"unknown","reasons":["composite page unavailable"]}),
                );
            if row["composite_ink"]["status"] != "attributed" {
                row["findings"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!("composite_ink_unverified"));
            }
        }
    }
    let typography_groups = spec
        .as_ref()
        .map(|s| crate::typography::assess(s, &rows))
        .unwrap_or_default();
    for group in &typography_groups {
        if group["status"] == "consistent" {
            continue;
        }
        for row in rows.iter_mut().filter(|r| {
            r["page_id"] == group["page_id"]
                && group["members"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|m| m["object_id"] == r["object_id"])
        }) {
            row["findings"]
                .as_array_mut()
                .unwrap()
                .push(json!("typography_group_mismatch"));
        }
    }
    let queue = visual_review_queue(&rows);
    let ownership_unverified = rows
        .iter()
        .filter(|o| {
            o["findings"]
                .as_array()
                .is_some_and(|a| a.iter().any(|v| v == "local_label_ownership_unverified"))
        })
        .count();
    let result = json!({"input":ctx.binding(&path)?,"source_spec":source_spec,"coordinate_space":if spec.is_some(){"canvas_pixels"}else{"points"},"package":package,"objects":rows,"composite_ink":composite,"typography_groups":typography_groups,"visual_review_queue":queue,"ownership_unverified_objects":ownership_unverified,"fonts":fonts,"font_summary":font_summary,"slides":doc.slides.iter().enumerate().map(|(i,s)|json!({"page":i+1,"notes":s.notes_text(),"shapes":s.shapes})).collect::<Vec<_>>(),"quality_approved":false,"limitations":["Isolated ink alone does not prove clearance from artwork or later objects; cycle inspection also attributes full-minus-blank composite ink. Attribution is not semantic ownership or a visual verdict.","Typography groups check only explicitly declared same-role members/fields; consistent formatting does not prove correct membership or the intended design size.","No OCR was run. Inspect full pages and dense crops for raster text, omissions, ownership and occlusion.","Detailed object layout checks currently cover top-level text frames; grouped text and chart labels still require visual review."]});
    if let Some(out) = r["output"].as_str() {
        ctx.write_json(out, &result)?;
        Ok(
            json!({"report":ctx.binding(out)?,"font_summary":font_summary,"objects":rows.len(),"flagged_objects":rows.iter().filter(|v|v["findings"].as_array().is_some_and(|a|!a.is_empty())).count(),"ownership_unverified_objects":ownership_unverified,"visual_review_groups":queue.len(),"next":"Inspect visual_review_queue in the report; an unverified owner is not a pass. Use matched design/blank/render crops when available.","quality_approved":false}),
        )
    } else {
        Ok(result)
    }
}

fn visual_review_queue(rows: &[Value]) -> Vec<Value> {
    let mut groups = std::collections::BTreeMap::<String, Value>::new();
    for row in rows {
        let Some(reasons) = row["findings"].as_array().filter(|v| !v.is_empty()) else {
            continue;
        };
        let owner = row["region"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| format!("object-{}", row["shape_id"]));
        let key = format!("{}:{owner}", row["page"]);
        let group=groups.entry(key).or_insert_with(||json!({"page":row["page"],"page_id":row["page_id"],"region":row["region"],"viewport":row["context_viewport"],"objects":[],"reasons":[],"status":"unreviewed"}));
        group["objects"].as_array_mut().unwrap().push(
            json!({"object_id":row["object_id"],"shape_id":row["shape_id"],"text":row["text"]}),
        );
        for reason in reasons {
            let list = group["reasons"].as_array_mut().unwrap();
            if !list.contains(reason) {
                list.push(reason.clone());
            }
        }
        for i in 0..4 {
            let a = group["viewport"][i].as_f64().unwrap();
            let b = row["context_viewport"][i].as_f64().unwrap();
            group["viewport"][i] = json!(if i < 2 { a.min(b) } else { a.max(b) });
        }
    }
    groups.into_values().collect()
}

pub(crate) fn measure_ink(
    doc: &deckcraft_engine::model::Presentation,
    index: usize,
    shape: &deckcraft_engine::model::Shape,
    xf: deckcraft_engine::model::Xfrm,
    layout: &deckcraft_text::TextLayout,
    canvas_scale: f64,
    constraints: (
        Option<geometry::Quad>,
        &[geometry::Quad],
        Option<&crate::artwork_contact::EdgeMap>,
    ),
) -> Result<Value> {
    let (owner, forbidden, artwork) = constraints;
    use deckcraft_engine::{model, render};
    use std::sync::Arc;
    // Only the text of this object is drawn. Context/master font inheritance stays intact.
    // This is independent of its authored frame, so overflow is not cropped away to pass.
    let corners = geometry::frame([xf.x, xf.y, xf.w, xf.h], xf.rot);
    let extra = (layout.content_width - xf.w)
        .max(layout.content_height - xf.h)
        .max(0.)
        + 32.;
    let left = corners.iter().map(|p| p[0]).fold(f64::INFINITY, f64::min) - extra;
    let top = corners.iter().map(|p| p[1]).fold(f64::INFINITY, f64::min) - extra;
    let right = corners
        .iter()
        .map(|p| p[0])
        .fold(f64::NEG_INFINITY, f64::max)
        + extra;
    let bottom = corners
        .iter()
        .map(|p| p[1])
        .fold(f64::NEG_INFINITY, f64::max)
        + extra;
    let px_per_pt = 2.;
    let w = ((right - left) * px_per_pt).ceil();
    let h = ((bottom - top) * px_per_pt).ceil();
    if !w.is_finite()
        || !h.is_finite()
        || w <= 0.
        || h <= 0.
        || w * h > 16_000_000.
        || w > 8192.
        || h > 8192.
    {
        return Ok(
            json!({"status":"unknown","reason":"isolated ink window exceeds bounded image budget"}),
        );
    }
    let mut isolated = doc.clone();
    let slide = Arc::make_mut(&mut isolated.slides[index]);
    let mut text = shape.clone();
    text.fill = Some(model::Fill::None);
    text.line = Some(model::Line {
        fill: Some(model::Fill::None),
        ..Default::default()
    });
    text.effects = None;
    slide.shapes = vec![text];
    slide.show_master_shapes = false;
    let img = render::render_slide(
        &isolated,
        index,
        &render::RenderOpts {
            scale: px_per_pt,
            offset: (-left * px_per_pt, -top * px_per_pt),
            size: Some((w as u32, h as u32)),
            transparent_background: true,
            ..Default::default()
        },
    );
    let mut minx = img.width;
    let mut miny = img.height;
    let mut maxx = 0;
    let mut maxy = 0;
    let mut count = 0u64;
    let mut outside = 0u64;
    let frame_quad = geometry::frame([xf.x, xf.y, xf.w, xf.h].map(|x| x * canvas_scale), xf.rot);
    let mut outside_frame = 0u64;
    let mut forbidden_hits = vec![0u64; forbidden.len()];
    let mut contact = crate::artwork_contact::Contact::default();
    for y in 0..img.height {
        for x in 0..img.width {
            let idx = (y as usize * img.width as usize + x as usize) * 4;
            if img.pixels.get(idx + 3).copied().unwrap_or(0) <= 8 {
                continue;
            }
            minx = minx.min(x);
            miny = miny.min(y);
            maxx = maxx.max(x);
            maxy = maxy.max(y);
            count += 1;
            let point = [
                (left + (x as f64 + 0.5) / px_per_pt) * canvas_scale,
                (top + (y as f64 + 0.5) / px_per_pt) * canvas_scale,
            ];
            if let Some(artwork) = artwork.filter(|_| img.pixels[idx + 3] >= 32) {
                contact.sample(artwork, point);
            }
            if owner.is_some_and(|q| !geometry::contains(q, point, 0.25)) {
                outside += 1;
            }
            if !geometry::contains(frame_quad, point, 0.5 * canvas_scale) {
                outside_frame += 1;
            }
            for (q, count) in forbidden.iter().zip(&mut forbidden_hits) {
                if geometry::contains(*q, point, -0.25) {
                    *count += 1;
                }
            }
        }
    }
    let bounds = if count == 0 {
        Value::Null
    } else {
        json!([
            (left + minx as f64 / px_per_pt) * canvas_scale,
            (top + miny as f64 / px_per_pt) * canvas_scale,
            (maxx - minx + 1) as f64 / px_per_pt * canvas_scale,
            (maxy - miny + 1) as f64 / px_per_pt * canvas_scale
        ])
    };
    Ok(
        json!({"status":if count==0{"no_visible_ink"}else{"measured"},"bounds":bounds,"pixels":count,"pixels_per_point":px_per_pt,"outside_frame_pixels":outside_frame,"outside_owner_pixels":owner.map(|_|outside),"forbidden_hits":forbidden_hits.iter().enumerate().map(|(i,n)|json!({"region_index":i,"pixels":n})).collect::<Vec<_>>(),"artwork_contact":artwork.map(|a|contact.finish(a)).unwrap_or_else(||json!({"status":"unknown","reason":"actual derived-blank raster not provided"})),"clipped_by_measurement_window":count>0&&(minx==0||miny==0||maxx+1==img.width||maxy+1==img.height),"method":"isolated native glyph render with optional independently sampled actual blank artwork edges; composite visibility is checked separately"}),
    )
}
pub fn compare(ctx: &Context, r: &Value) -> Result<Value> {
    if r.get("background").is_some() {
        return crate::visual_diff::compare(ctx, r);
    }
    let a_path = ctx.path(string(r, "before")?)?;
    let b_path = ctx.path(string(r, "after")?)?;
    let a = load_image(ctx, &a_path)?.to_rgb8();
    let b = load_image(ctx, &b_path)?.to_rgb8();
    ensure!(
        a.dimensions() == b.dimensions(),
        "images have different dimensions"
    );
    let threshold = r["threshold"].as_u64().unwrap_or(8);
    ensure!(threshold <= 255, "threshold must be 0..255");
    let mut diff = image::RgbImage::new(a.width(), a.height());
    let mut changed = 0u64;
    let mut total = 0u64;
    for ((pa, pb), out) in a.pixels().zip(b.pixels()).zip(diff.pixels_mut()) {
        let delta = [
            pa[0].abs_diff(pb[0]),
            pa[1].abs_diff(pb[1]),
            pa[2].abs_diff(pb[2]),
        ];
        total += delta.iter().map(|x| u64::from(*x)).sum::<u64>();
        if delta.iter().any(|x| u64::from(*x) > threshold) {
            changed += 1;
            *out = image::Rgb([220, 35, 70]);
        } else {
            *out = image::Rgb([245, 245, 245]);
        }
    }
    let mut panel = image::RgbImage::new(a.width() * 3, a.height());
    image::imageops::replace(&mut panel, &a, 0, 0);
    image::imageops::replace(&mut panel, &b, i64::from(a.width()), 0);
    image::imageops::replace(&mut panel, &diff, i64::from(a.width()) * 2, 0);
    let output = save_image(
        ctx,
        string(r, "output")?,
        image::DynamicImage::ImageRgb8(panel),
    )?;
    Ok(
        json!({"before":ctx.binding(a_path)?,"after":ctx.binding(b_path)?,"panel":ctx.binding(output)?,"changed_pixels":changed,"mean_channel_error":total as f64/(a.width()as f64*a.height()as f64*3.),"quality_approved":false,"note":"Pixel change does not establish improvement. Panel: before, after, difference."}),
    )
}
pub fn load_image(ctx: &Context, path: &Path) -> Result<image::DynamicImage> {
    let data = ctx.read(path)?;
    let reader = image::ImageReader::new(Cursor::new(&data)).with_guessed_format()?;
    let dims = reader.into_dimensions()?;
    ensure!(
        u64::from(dims.0) * u64::from(dims.1) <= 64 * 1024 * 1024,
        "image exceeds 64 megapixels"
    );
    Ok(image::load_from_memory(&data)?)
}
pub fn save_image(
    ctx: &Context,
    path: impl AsRef<Path>,
    image: image::DynamicImage,
) -> Result<std::path::PathBuf> {
    let mut bytes = Cursor::new(Vec::new());
    image.write_to(&mut bytes, image::ImageFormat::Png)?;
    ctx.write_new(path, bytes.get_ref())
}
pub fn crop(ctx: &Context, r: &Value) -> Result<Value> {
    let src = ctx.path(string(r, "input")?)?;
    let binding = ctx.binding(&src)?;
    let b = geometry::rect(&r["box"])?;
    // Reuse is opt-in and bound to a prior machine receipt. It reuses computed
    // pixels only, never a visual judgment. Image bytes or viewport changes
    // invalidate the hit; a corrupted cached artifact is rejected.
    if let Some(reuse) = r.get("reuse") {
        let previous = ctx.json(ctx.verify(reuse)?)?;
        if previous["algorithm"] == "crop-native-v1"
            && previous["input"]["sha256"] == binding["sha256"]
            && previous["requested_box"] == json!(b)
        {
            let cached = ctx.verify(&previous["crop"])?;
            let out = ctx.write_new(string(r, "output")?, &ctx.read(cached)?)?;
            let result = json!({"algorithm":"crop-native-v1","input":binding,"crop":ctx.binding(out)?,
                "requested_box":b,"box":previous["box"],"resampled":false,"processing_reused":true,"quality_approved":false});
            return crop_receipt(ctx, r, result);
        }
    }
    let img = load_image(ctx, &src)?;
    ensure!(
        b[0] >= 0.
            && b[1] >= 0.
            && b[0] + b[2] <= img.width() as f64
            && b[1] + b[3] <= img.height() as f64,
        "crop is outside image"
    );
    let left = b[0].floor() as u32;
    let top = b[1].floor() as u32;
    let right = (b[0] + b[2]).ceil() as u32;
    let bottom = (b[1] + b[3]).ceil() as u32;
    let result = img.crop_imm(left, top, right - left, bottom - top);
    let out = save_image(ctx, string(r, "output")?, result)?;
    crop_receipt(
        ctx,
        r,
        json!({"algorithm":"crop-native-v1","input":binding,"crop":ctx.binding(out)?,"requested_box":b,"box":[left,top,right-left,bottom-top],"resampled":false,"processing_reused":false,"quality_approved":false}),
    )
}
fn crop_receipt(ctx: &Context, r: &Value, mut result: Value) -> Result<Value> {
    if let Some(path) = r["receipt"].as_str() {
        let file = ctx.write_json(path, &result)?;
        result["receipt"] = ctx.binding(file)?;
    }
    Ok(result)
}
pub fn image_info(ctx: &Context, r: &Value) -> Result<Value> {
    let path = ctx.path(string(r, "input")?)?;
    let data = ctx.read(&path)?;
    let size = image::ImageReader::new(Cursor::new(data))
        .with_guessed_format()?
        .into_dimensions()?;
    Ok(
        json!({"image":ctx.binding(path)?,"width":size.0,"height":size.1,"native_generation_resolution_verified":false}),
    )
}
