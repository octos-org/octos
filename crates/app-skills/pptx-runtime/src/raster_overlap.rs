//! Raster OCR/native frame collision candidates; judgments remain with the host peer.
use crate::{Context, geometry, string};
use anyhow::Context as _;
use anyhow::{Result, ensure};
use serde_json::{Value, json};

fn xyxy(b: [f64; 4]) -> [f64; 4] {
    [b[0], b[1], b[0] + b[2], b[1] + b[3]]
}

pub fn proposals(detections: &[Value], objects: &[Value], size: [f64; 2]) -> Result<Vec<Value>> {
    let mut candidates = Vec::new();
    for d in detections {
        if d["text"].as_str().is_none_or(|s| s.trim().is_empty()) {
            continue;
        }
        let b = xyxy(geometry::rect(&d["box"])?);
        let box_area = (b[2] - b[0]) * (b[3] - b[1]);
        let mut members = Vec::new();
        let mut area = b;
        for object in objects {
            let q = geometry::frame(
                geometry::rect(&object["box"])?,
                object["rotation"].as_f64().unwrap_or(0.),
            );
            let r = [
                q.iter().map(|p| p[0]).fold(f64::INFINITY, f64::min),
                q.iter().map(|p| p[1]).fold(f64::INFINITY, f64::min),
                q.iter().map(|p| p[0]).fold(f64::NEG_INFINITY, f64::max),
                q.iter().map(|p| p[1]).fold(f64::NEG_INFINITY, f64::max),
            ];
            let intersection = (b[2].min(r[2]) - b[0].max(r[0])).max(0.)
                * (b[3].min(r[3]) - b[1].max(r[1])).max(0.);
            let denominator = box_area.min((r[2] - r[0]) * (r[3] - r[1]));
            if denominator > 0. && intersection / denominator >= 0.15 {
                members.push(
                    json!({"object_id":object["object_id"],"text":object["text"],"frame_xyxy":r}),
                );
                area = [
                    area[0].min(r[0]),
                    area[1].min(r[1]),
                    area[2].max(r[2]),
                    area[3].max(r[3]),
                ];
            }
        }
        if !members.is_empty() {
            area = [
                (area[0] / size[0] - 0.015).clamp(0., 1.),
                (area[1] / size[1] - 0.015).clamp(0., 1.),
                (area[2] / size[0] + 0.015).clamp(0., 1.),
                (area[3] / size[1] + 0.015).clamp(0., 1.),
            ];
            candidates.push(
                json!({"id":format!("candidate-{}",candidates.len()+1),"raster_text":d["text"],
                "raster_box":d["box"],"ocr_confidence":d["confidence"],"native":members,
                "viewport_normalized_xyxy":area,"status":"unreviewed","quality_approved":false}),
            );
        }
    }
    Ok(candidates)
}

pub fn run(ctx: &Context, r: &Value) -> Result<Value> {
    let (cycle, spec) = crate::review::bound_cycle(ctx, &ctx.path(string(r, "cycle")?)?)?;
    let selected = r["page"].as_str();
    ensure!(
        selected.is_none_or(|id| spec.pages.iter().any(|p| p.id == id)),
        "unknown source page ID"
    );
    let out = ctx.path(string(r, "output")?)?;
    ensure!(!out.exists(), "use a fresh raster-overlap output directory");
    if !cfg!(target_os = "macos") {
        return Ok(
            json!({"status":"unknown","reason":"native OCR unavailable on this host", "quality_approved":false}),
        );
    }
    let inspection = crate::review::inspect(ctx, &json!({"cycle":string(r,"cycle")?}))?;
    let mut pages = Vec::new();
    for (i, p) in spec
        .pages
        .iter()
        .enumerate()
        .filter(|(_, p)| selected.is_none_or(|id| p.id == id))
    {
        let blank = &cycle["renders"]["blank"]["pages"][i];
        let full = &cycle["renders"]["editable"]["pages"][i];
        ensure!(
            !blank.is_null() && !full.is_null(),
            "raster overlap requires actual full/derived-blank renders"
        );
        let blank_path = ctx.verify(blank)?;
        let ocr = crate::ocr::recognize(ctx, &json!({"input":blank_path}))?;
        let size = [
            ocr["image"]["width_px"]
                .as_f64()
                .context("raster_overlap::run: missing required value")?,
            ocr["image"]["height_px"]
                .as_f64()
                .context("raster_overlap::run: missing required value")?,
        ];
        let mut objects: Vec<_> = inspection["objects"]
            .as_array()
            .context("raster_overlap::run: missing required value")?
            .iter()
            .filter(|v| v["page"] == i + 1)
            .cloned()
            .collect();
        for object in &mut objects {
            let b = geometry::rect(&object["box"])?;
            object["box"] = json!([
                b[0] * size[0] / spec.canvas.width_px,
                b[1] * size[1] / spec.canvas.height_px,
                b[2] * size[0] / spec.canvas.width_px,
                b[3] * size[1] / spec.canvas.height_px
            ]);
        }
        let mut candidates = proposals(
            ocr["detections"]
                .as_array()
                .context("raster_overlap::run: missing required value")?,
            &objects,
            size,
        )?;
        for (ci, candidate) in candidates.iter_mut().enumerate() {
            let viewport: [f64; 4] =
                serde_json::from_value(candidate["viewport_normalized_xyxy"].clone())?;
            let mut images = Vec::new();
            let mut sources = vec![("blank", blank.clone()), ("render", full.clone())];
            if let Some(design) = &p.design {
                sources.insert(0, ("design", design.clone()));
            }
            for (role, source) in sources {
                let path = ctx.verify(&source)?;
                let info = crate::review::image_info(ctx, &json!({"input":path}))?;
                let (w, h) = (
                    info["width"]
                        .as_f64()
                        .context("raster_overlap::run: missing required value")?,
                    info["height"]
                        .as_f64()
                        .context("raster_overlap::run: missing required value")?,
                );
                let crop = crate::review::crop(
                    ctx,
                    &json!({"input":path,
                    "box":[viewport[0]*w,viewport[1]*h,(viewport[2]-viewport[0])*w,(viewport[3]-viewport[1])*h],
                    "output":out.join(format!("page-{:03}-candidate-{:03}-{role}.png",i+1,ci+1))}),
                )?;
                images.push(json!({"role":role,"crop":crop}));
            }
            candidate["images"] = json!(images);
        }
        pages.push(json!({"page":i+1,"page_id":p.id,"ocr":ocr,"candidates":candidates}));
    }
    let result = json!({"algorithm":"raster-native-overlap-v1","cycle":ctx.binding(string(r,"cycle")?)?,
        "pages":pages,"quality_approved":false,"visual_review_required":true,
        "instruction":"Read matched original/blank/render crops. Classify each candidate as duplicate/residual text, an intentional separate label/symbol, or unknown. Confidence and frame overlap are proposals, not proof. Send the same evidence to the existing independent peer.",
        "limitations":"OCR can miss or hallucinate stylized text. Rotated frame envelopes are conservative. Zero candidates is not a residual-text clearance or whole-deck pass."});
    let path = ctx.write_json(out.join("report.json"), &result)?;
    Ok(json!({"report":ctx.binding(path)?,"pages":pages.len(),
        "candidate_count":pages.iter().map(|p|p["candidates"].as_array().expect("page reports created above contain candidate arrays").len()).sum::<usize>(),
        "quality_approved":false,"visual_review_required":true}))
}
