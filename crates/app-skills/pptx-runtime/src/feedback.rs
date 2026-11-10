//! Readable review evidence, not a second reviewer or a visual acceptance score.
use crate::{
    Context, geometry, review,
    spec::{Page, Spec, Text},
    string,
};
use anyhow::Context as _;
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

pub const ALGORITHM: &str = "review-feedback-v3";

/// Several labels can share one owner/context crop. Read that group once while
/// retaining separate judgments; identical pixels at different locations differ.
pub fn reading_groups(batches: &[Value]) -> Vec<Value> {
    let mut positions = BTreeMap::new();
    let mut groups: Vec<Value> = Vec::new();
    for b in batches {
        let images: Vec<_> = b["images"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|im| json!({"role":im["role"],"image":im["image"]}))
            .collect();
        let hashes: Vec<_> = images
            .iter()
            .map(|im| json!([im["role"], im["image"]["sha256"]]))
            .collect();
        let key = json!([b["kind"], b["page"], b["viewport_normalized_xyxy"], hashes]).to_string();
        let index = *positions.entry(key).or_insert_with(|| {
            groups.push(json!({"kind":b["kind"],"page":b["page"],"images":images,"objects":[]}));
            groups.len() - 1
        });
        if b["kind"] == "object" {
            groups[index]["objects"]
                .as_array_mut()
                .expect("reading groups are initialized with an objects array")
                .push(json!({
                "object_id":b["object_id"],"evidence_hash":b["evidence_hash"]}));
        }
    }
    groups
}

/// Disappearing measurements are only comparable with unchanged criteria and targets.
pub fn finding_changes(before: &Value, after: &Value) -> Value {
    let same = before["algorithm"].is_string()
        && before["algorithm"] == after["algorithm"]
        && before["engine_revision"] == after["engine_revision"]
        && before["contract"].is_object()
        && before["contract"] == after["contract"]
        && before["findings"].is_array()
        && after["findings"].is_array()
        && before["inventory"] == after["inventory"];
    if !same {
        return json!({"comparable":false,"no_longer_detected":[],
            "reason":"Measurement criteria, target/coverage or evidence are different or missing; disappearance cannot be counted as repair.","quality_proven":false});
    }
    fn keyed(v: &Value) -> BTreeMap<String, Value> {
        v["findings"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|f| {
                let key = json!([f["page"], f["object_id"], f["code"]]);
                (
                    key.to_string(),
                    json!({"key":key,"status":f["status"],"observed":f["observed"]}),
                )
            })
            .collect()
    }
    let a = keyed(before);
    let b = keyed(after);
    json!({"comparable":true,
        "persisting":b.iter().filter_map(|(k,v)|a.get(k).map(|old|json!({"key":v["key"],"before":old,"after":v}))).collect::<Vec<_>>(),
        "new":b.iter().filter(|(k,_)|!a.contains_key(*k)).map(|(_,v)|v).collect::<Vec<_>>(),
        "no_longer_detected":a.iter().filter(|(k,_)|!b.contains_key(*k)).map(|(_,v)|v).collect::<Vec<_>>(),
        "quality_proven":false,"note":"No longer detected is a scoped measurement change, not proof of visual repair."})
}

fn crop(ctx: &Context, source: &Value, area: [f64; 4], output: &Path) -> Result<Value> {
    let image = review::load_image(ctx, &ctx.verify(source)?)?.to_rgb8();
    crop_pixels(ctx, source, &image, area, output)
}

fn crop_pixels(
    ctx: &Context,
    source: &Value,
    image: &image::RgbImage,
    area: [f64; 4],
    output: &Path,
) -> Result<Value> {
    let (w, h) = (image.width(), image.height());
    let x = (area[0] * w as f64).floor().max(0.) as u32;
    let y = (area[1] * h as f64).floor().max(0.) as u32;
    let right = (area[2] * w as f64).ceil().min(w as f64) as u32;
    let bottom = (area[3] * h as f64).ceil().min(h as f64) as u32;
    ensure!(right > x && bottom > y, "empty review viewport");
    let cropped = image::imageops::crop_imm(image, x, y, right - x, bottom - y).to_image();
    review::save_image(ctx, output, image::DynamicImage::ImageRgb8(cropped))?;
    Ok(
        json!({"image":ctx.binding(output)?,"source":source,"source_size":[w,h],
        "viewport_normalized_xyxy":area,"crop_px_xywh":[x,y,right-x,bottom-y],
        "resized":false,"inspected":false}),
    )
}

fn qa_report(ctx: &Context, cycle: &Value) -> Result<Value> {
    let Some(binding) = cycle.get("native_qa").and_then(|v| v.get("report")) else {
        return Ok(Value::Null);
    };
    let report = ctx.json(ctx.verify(binding)?)?;
    ensure!(
        report["input"] == cycle["editable"] && report["spec"] == cycle["spec"],
        "QA belongs to another artifact"
    );
    Ok(report)
}

fn context(spec: &Spec, page: &Page, text: Option<&Text>) -> Result<Option<[f64; 4]>> {
    let Some(t) = text else { return Ok(None) };
    let mut points = Vec::from(geometry::frame(t.bounds, t.rotation));
    if let Some(owner) = t.region.as_ref().and_then(|id| page.regions.get(id)) {
        points.extend(geometry::quad(owner)?);
    }
    Ok(Some([
        points.iter().map(|p| p[0]).fold(f64::INFINITY, f64::min) / spec.canvas.width_px,
        points.iter().map(|p| p[1]).fold(f64::INFINITY, f64::min) / spec.canvas.height_px,
        points
            .iter()
            .map(|p| p[0])
            .fold(f64::NEG_INFINITY, f64::max)
            / spec.canvas.width_px,
        points
            .iter()
            .map(|p| p[1])
            .fold(f64::NEG_INFINITY, f64::max)
            / spec.canvas.height_px,
    ]))
}

fn expanded_union(areas: impl Iterator<Item = [f64; 4]>) -> [f64; 4] {
    let mut b = [1., 1., 0., 0.];
    for a in areas {
        b[0] = f64::min(b[0], a[0]);
        b[1] = f64::min(b[1], a[1]);
        b[2] = f64::max(b[2], a[2]);
        b[3] = f64::max(b[3], a[3]);
    }
    let dx = f64::max(0.025, (b[2] - b[0]) * 0.2);
    let dy = f64::max(0.025, (b[3] - b[1]) * 0.2);
    [
        (b[0] - dx).max(0.),
        (b[1] - dy).max(0.),
        (b[2] + dx).min(1.),
        (b[3] + dy).min(1.),
    ]
}

pub fn create(ctx: &Context, r: &Value) -> Result<Value> {
    let cycle_path = ctx.path(string(r, "cycle")?)?;
    let (cycle, spec) = review::bound_cycle(ctx, &cycle_path)?;
    let previous = r["previous_cycle"]
        .as_str()
        .map(|p| review::bound_cycle(ctx, &ctx.path(p)?))
        .transpose()?;
    if let Some((old, old_spec)) = &previous {
        ensure!(
            cycle["pages"] == old["pages"]
                && serde_json::to_value(&spec.canvas)? == serde_json::to_value(&old_spec.canvas)?,
            "before/after must use the same canvas and page order"
        );
    }
    ensure!(
        r.get("pages").is_none() || r.get("page_numbers").is_none(),
        "select pages by source IDs or page_numbers, not both"
    );
    let pages: BTreeSet<String> = if let Some(values) = r.get("page_numbers") {
        let values = values.as_array().filter(|v| !v.is_empty()).ok_or_else(|| {
            anyhow::anyhow!("page_numbers must be a nonempty array of 1-based display pages")
        })?;
        values
            .iter()
            .map(|v| {
                let i = v
                    .as_u64()
                    .and_then(|n| n.checked_sub(1))
                    .and_then(|n| usize::try_from(n).ok())
                    .filter(|&n| n < spec.pages.len())
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "page_number must be a 1-based display page inside this cycle"
                        )
                    })?;
                Ok(spec.pages[i].id.clone())
            })
            .collect::<Result<_>>()?
    } else if let Some(values) = r.get("pages") {
        let values = values
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("pages must be an array"))?;
        ensure!(!values.is_empty(), "pages cannot be empty");
        values
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| anyhow::anyhow!("page ID must be a string"))
            })
            .collect::<Result<_>>()?
    } else {
        spec.pages.iter().map(|p| p.id.clone()).collect()
    };
    ensure!(
        pages
            .iter()
            .all(|id| spec.pages.iter().any(|p| &p.id == id)),
        "unknown selected page"
    );
    let output = ctx.path(string(r, "output")?)?;
    ensure!(!output.exists(), "use a fresh review output directory");
    let qa = qa_report(ctx, &cycle)?;
    let delta = if let Some((old, _)) = &previous {
        finding_changes(&qa_report(ctx, old)?, &qa)
    } else {
        json!({"comparable":false,"reason":"no previous cycle","no_longer_detected":[],"quality_proven":false})
    };
    let mut artifacts = Vec::new();
    let mut coverage = Vec::new();
    let mut revisions = Vec::new();
    let inventory = crate::object_review::inventory(&cycle, &spec)?;
    let mut object_evidence = Vec::new();
    let mut view_batches = Vec::new();
    let mut relations = Vec::new();
    let page_map: Vec<_> = spec
        .pages
        .iter()
        .enumerate()
        .map(|(i, p)| {
            json!({
                "page_number":i+1,"page_id":p.id,"selected":pages.contains(&p.id),
                "render":cycle["renders"]["editable"]["pages"][i],
                "blank":cycle["renders"]["blank"]["pages"][i]
            })
        })
        .collect();
    for (i, page) in spec
        .pages
        .iter()
        .enumerate()
        .filter(|(_, p)| pages.contains(&p.id))
    {
        relations.push(crate::relations::page(ctx, &cycle, &spec, &qa, i)?);
        let mut sources = vec![
            ("current", cycle["renders"]["editable"]["pages"][i].clone()),
            ("blank", cycle["renders"]["blank"]["pages"][i].clone()),
        ];
        if let Some(design) = &page.design {
            sources.push(("design", design.clone()));
        }
        view_batches.push(json!({"kind":"overview","page":page.id,
            "images":sources.iter().map(|(role,source)|json!({"role":role,"image":source})).collect::<Vec<_>>(),
            "instruction":"Read the complete page for intended relationships before detailed judgments. Do not attach every page to a single model request."}));
        // Decode each source once for all object crops. No resampling or contact
        // sheet shrinking: the model receives a small group of native-pixel images.
        let pixels = sources
            .iter()
            .map(|(_, source)| review::load_image(ctx, &ctx.verify(source)?).map(|im| im.to_rgb8()))
            .collect::<Result<Vec<_>>>()?;
        for item in inventory.iter().filter(|v| v["page"] == page.id) {
            let area: [f64; 4] = serde_json::from_value(item["viewport_normalized_xyxy"].clone())?;
            let mut row = item.clone();
            let mut pictures = Vec::new();
            for ((role, source), pixels) in sources.iter().zip(&pixels) {
                let path = output.join(format!(
                    "object-{:05}-{role}.png",
                    object_evidence.len() + 1
                ));
                let mut picture = crop_pixels(ctx, source, pixels, area, &path)?;
                picture["role"] = json!(role);
                picture["kind"] = json!("object");
                picture["page"] = json!(page.id);
                picture["page_number"] = json!(i + 1);
                picture["object_id"] = item["object_id"].clone();
                picture["bytes"] = json!(std::fs::metadata(&path)?.len());
                pictures.push(picture.clone());
                artifacts.push(picture);
            }
            row["images"] = json!(pictures);
            row["native_findings"] = json!(
                qa["findings"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|f| (f["page"] == page.id
                        || f["page"].as_u64() == Some((i + 1) as u64))
                        && f["object_id"] == item["object_id"])
                    .collect::<Vec<_>>()
            );
            view_batches.push(json!({"kind":"object","page":page.id,"object_id":item["object_id"],
                "viewport_normalized_xyxy":item["viewport_normalized_xyxy"],
                "evidence_hash":item["evidence_hash"],"images":pictures,
                "checks_required":item["checks_required"],
                "instruction":if item["kind"] == "artwork_symbol" {
                    "Compare original, blank and current pixels. Verify that the declared symbol has the correct meaning/direction, appears once, remains visible and does not hide missing editable text. A hash or declared basis is not semantic proof."
                } else {
                    "Read these matched views together. Judge the target in its owner and neighboring context. Unknown ownership stays unknown. If context or text is unclear, read a wider/native crop before judging; do not infer from OCR alone."
                }}));
            object_evidence.push(row);
        }
        // Detector-independent coverage: do not let a false-negative detector hide a region.
        let viewports = [
            [0., 0., 0.55, 0.55],
            [0.45, 0., 1., 0.55],
            [0., 0.45, 0.55, 1.],
            [0.45, 0.45, 1., 1.],
        ];
        for (j, area) in viewports.iter().enumerate() {
            for (role, source) in &sources {
                let mut row = crop(
                    ctx,
                    source,
                    *area,
                    &output.join(format!("page-{:03}-sweep-{:02}-{role}.png", i + 1, j + 1)),
                )?;
                row["kind"] = json!("sweep");
                row["page"] = json!(page.id);
                row["page_number"] = json!(i + 1);
                row["role"] = json!(role);
                row["viewport"] = json!(j + 1);
                artifacts.push(row);
            }
        }
        coverage.push(
            json!({"page":page.id,"page_number":i+1,"sweep_viewports":4,"native_objects":page.texts.len(),
            "inspected":false,"independent_of_detector_findings":true}),
        );
        if let Some((old, old_spec)) = &previous {
            let old_page = &old_spec.pages[i];
            let ids: BTreeSet<_> = old_page
                .texts
                .iter()
                .chain(page.texts.iter())
                .map(|t| &t.id)
                .collect();
            for id in ids {
                let a = old_page.texts.iter().find(|t| &t.id == id);
                let b = page.texts.iter().find(|t| &t.id == id);
                let same_style = match (a, b) {
                    (Some(a), Some(b)) => {
                        serde_json::to_value(old_spec.style(a)?)?
                            == serde_json::to_value(spec.style(b)?)?
                    }
                    _ => false,
                };
                if serde_json::to_value(a)? == serde_json::to_value(b)?
                    && same_style
                    && old_page.background == page.background
                    && old_page.regions == page.regions
                {
                    continue;
                }
                let area = expanded_union(
                    [context(old_spec, old_page, a)?, context(&spec, page, b)?]
                        .into_iter()
                        .flatten(),
                );
                let index = revisions.len() + 1;
                let mut pictures = Vec::new();
                let mut local = sources.clone();
                local.push(("before", old["renders"]["editable"]["pages"][i].clone()));
                for (role, source) in local {
                    let mut row = crop(
                        ctx,
                        &source,
                        area,
                        &output.join(format!("revision-{index:04}-{role}.png")),
                    )?;
                    row["kind"] = json!("revision");
                    row["page"] = json!(page.id);
                    row["page_number"] = json!(i + 1);
                    row["object_id"] = json!(id);
                    row["role"] = json!(role);
                    pictures.push(row["image"].clone());
                    artifacts.push(row);
                }
                revisions.push(json!({"page":page.id,"page_number":i+1,"object_id":id,"before":a,"after":b,
                    "artwork_changed":old_page.background!=page.background,"owner_declarations_changed":old_page.regions!=page.regions,
                    "viewport_normalized_xyxy":area,"images":pictures,"quality_proven":false}));
            }
        }
    }
    let mut selected_findings: Vec<_> = qa["findings"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|f| {
            if let Some(index) = f["page"].as_u64() {
                return index
                    .checked_sub(1)
                    .and_then(|n| spec.pages.get(n as usize))
                    .is_none_or(|p| pages.contains(&p.id));
            }
            // Unknown/global scopes stay visible instead of being silently dropped.
            f["page"].as_str().is_none_or(|p| pages.contains(p))
        })
        .cloned()
        .collect();
    selected_findings.sort_by_key(|f| if f["status"] == "fail" { 0 } else { 1 });
    let mut by_code = BTreeMap::<String, usize>::new();
    for f in &selected_findings {
        *by_code
            .entry(f["code"].as_str().unwrap_or("unknown").to_owned())
            .or_default() += 1;
    }
    let summary = json!({"scope":"selected pages plus deck-wide findings", "by_code":by_code,
        "diagnosis":crate::qa::diagnosis(&selected_findings),
        "failed":selected_findings.iter().filter(|f|f["status"]=="fail").count(),
        "unknown":selected_findings.iter().filter(|f|f["status"]=="unknown").count(),
        "first_findings":selected_findings.iter().take(8).collect::<Vec<_>>(),
        "omitted_findings":selected_findings.len().saturating_sub(8),
        "note":"Unknowns are missing measurement/verification, not confirmed visual defects."});
    let relations_path = ctx.write_json(
        output.join("relations.json"),
        &json!({"algorithm":crate::relations::ALGORITHM,
        "cycle":ctx.binding(&cycle_path)?,"pages":relations,"quality_approved":false}),
    )?;
    let mut template = crate::object_review::template(ctx, &cycle_path, &object_evidence)?;
    crate::fidelity::attach(ctx, &mut template, &object_evidence)?;
    crate::object_review::attach_relations(ctx, &mut template, &ctx.binding(&relations_path)?)?;
    let comparisons = r["comparisons"]
        .as_str()
        .map(|p| ctx.binding(p))
        .transpose()?
        .or_else(|| cycle.get("comparisons").cloned());
    if let Some(binding) = &comparisons {
        crate::object_review::attach_comparisons(ctx, &mut template, &cycle, &spec, binding)?;
    }
    let template_path = ctx.write_json(output.join("object-review-template.json"), &template)?;
    let signals: Vec<_> = relations
        .iter()
        .flat_map(|p| p["signals"].as_array().into_iter().flatten())
        .cloned()
        .collect();
    let reuse = if let Some(prior) = r["previous_review"].as_str() {
        crate::review_reuse::collect(
            ctx,
            &cycle_path,
            string(r, "previous_cycle")?,
            prior,
            &signals,
        )?
    } else {
        json!({})
    };
    let reused = |page: &Value, id: &Value| {
        reuse["object_reviews"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|o| o["page"] == *page && o["object_id"] == *id)
    };
    // A small reading index prevents dumping the large diagnostic report into
    // model context just to discover which three images to open next.
    let mut groups = reading_groups(&view_batches);
    for group in &mut groups {
        let ids: Vec<_> = group["objects"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|o| o["object_id"].clone())
            .collect();
        group["relation_signals"] = json!(
            relations
                .iter()
                .filter(|p| p["page"] == group["page"])
                .flat_map(|p| p["signals"].as_array().into_iter().flatten())
                .filter(|s| ids.contains(&s["object_id"]))
                .collect::<Vec<_>>()
        );
        group["object_measurements"]=json!(relations.iter().filter(|p|p["page"]==group["page"])
            .flat_map(|p|p["objects"].as_array().into_iter().flatten())
            .filter(|o|ids.contains(&o["object_id"]))
            .map(|o|json!({"object_id":o["object_id"],"status":o["status"],"ink":o["ink"],"rotation":o["rotation"],
                "surface_candidate":o["surface"],"reference_candidate":o["reference"],
                "candidate_is_semantic_owner":false,"reference_center_relations":o["reference_center_relations"]})).collect::<Vec<_>>());
        group["objects_requiring_review"] = json!(
            group["objects"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|o| !reused(&group["page"], &o["object_id"]))
                .collect::<Vec<_>>()
        );
    }
    groups.retain(|g| {
        g["kind"] != "object"
            || g["objects_requiring_review"]
                .as_array()
                .is_some_and(|v| !v.is_empty())
    });
    let mut draft = template.clone();
    draft["object_reviews"] = json!(
        template["object_reviews"]
            .as_array()
            .context("feedback::create: missing required value")?
            .iter()
            .filter(|o| !reused(&o["page"], &o["object_id"]))
            .collect::<Vec<_>>()
    );
    draft["relation_reviews"] = json!(
        template["relation_reviews"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|row| !reuse["relation_reviews"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|r| r["id"] == row["id"]))
            .collect::<Vec<_>>()
    );
    let native_candidates =
        crate::review_routes::candidates(&qa, &crate::review_routes::load_plan(ctx)?);
    // Keep native-review IDs alongside the other editable observations. Asking
    // the model to transcribe them from a second report loses rows and wastes
    // calls; Rust supplies identity, never the visual decision or basis.
    draft["measurement_reviews"] = native_candidates.clone();
    // Model authors facts only; submit binds these immutable source receipts
    // from object-review-template.json. Do not ask it to retranscribe hashes.
    for row in draft["object_reviews"]
        .as_array_mut()
        .context("feedback::create: missing required value")?
    {
        if let Some(f) = row["fidelity"].as_object_mut() {
            f.remove("evidence");
        }
    }
    let draft_path = ctx.write_json(output.join("observations-template.json"), &draft)?;
    let index_path = ctx.write_json(output.join("view-index.json"), &json!({
        "cycle":ctx.binding(&cycle_path)?,"template":ctx.binding(&template_path)?,"review_id":template["review_id"],
        "relations":ctx.binding(&relations_path)?,"observations_template":ctx.binding(&draft_path)?,"reused_objects":reuse["object_reviews"].as_array().map_or(0,Vec::len),
        "native_review_candidates":native_candidates,
        "batches":groups,"note":"Read overview and sweep views, then only objects_requiring_review. Unchanged local evidence is reused by Rust, not re-authored. Surface/reference candidates are geometric evidence, not owner decisions."}))?;
    let report = json!({"algorithm":ALGORITHM,"cycle":ctx.binding(&cycle_path)?,
        "previous_cycle":r["previous_cycle"].as_str().map(|p|ctx.binding(p)).transpose()?,
        "page_map":page_map,"coverage":coverage,"revisions":revisions,"finding_changes":delta,"artifacts":artifacts,
        "finding_changes_scope":"all cycle objects; cropped page selection does not alter measurement comparison",
        "finding_summary":summary,
        "object_evidence":object_evidence,"view_batches":view_batches,"object_review_template":ctx.binding(template_path)?,
        "view_index":ctx.binding(&index_path)?,"relations":ctx.binding(&relations_path)?,"comparisons":comparisons,
        "reuse":reuse,"observations_template":ctx.binding(draft_path)?,
        "quality_approved":false,"source_modified":false,"visual_review_required":true,
        "limitations":["Crops are unresized actual pixels, not evidence the model viewed or understood them.",
            "Normalized comparison regions do not establish registration of design and blank; consult reference_alignment.",
            "Finding changes compare bound QA reports, not a fresh detector run. No longer detected does not close a visual defect.",
            "Changed text/artwork may create new neighbor defects; use all sweep views, not only revision crops."]});
    let path = ctx.write_json(output.join("report.json"), &report)?;
    Ok(
        json!({"report":ctx.binding(path)?,"page_map":page_map,"pages":coverage.len(),"revision_objects":revisions.len(),"objects":object_evidence.len(),"images":artifacts.len(),
        "object_review_template":report["object_review_template"],"review_id":template["review_id"],"view_batch_count":view_batches.len(),
        "view_index":ctx.binding(index_path)?,
        "reading_group_count":groups.len(),"reused_objects":reuse["object_reviews"].as_array().map_or(0,Vec::len),"observations_template":report["observations_template"],
        "finding_summary":summary,"quality_approved":false,"next":"Read actual sweep images for all selected pages, then before/current local images. Evaluate persistent and new findings; record each defect separately. This action performs no visual review."}),
    )
}
