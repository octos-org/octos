//! Deterministic QA derived from the actual PPTX, never from model verdicts.
//! This is an application gate, not a security boundary against arbitrary host writes.
use crate::{Context, geometry, spec::Spec, string};
use anyhow::{Result, ensure};
use deckcraft_engine::model::{Shape, ShapeKind};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

pub const ALGORITHM: &str = "native-qa-v3.6";
type Boundaries = BTreeMap<(String, String), Value>;

/// Group evidence by what it can establish. This is diagnostic routing, never
/// a substitute for the original findings or a relaxation of acceptance.
pub fn diagnosis(findings: &[Value]) -> Value {
    let mut violations = 0;
    let mut questions = 0;
    let mut gaps = 0;
    let mut affected = BTreeSet::new();
    let mut groups: BTreeMap<(String, String), (usize, BTreeSet<String>)> = BTreeMap::new();
    for f in findings {
        let code = f["code"].as_str().unwrap_or("UNCLASSIFIED_FINDING");
        let kind = if f["status"] == "fail" {
            violations += 1;
            "measured_constraint_violation"
        } else if matches!(
            code,
            "ARTWORK_CONTACT_UNVERIFIED" | "COMPOSITE_INK_UNVERIFIED"
        ) {
            questions += 1;
            "visual_question"
        } else {
            gaps += 1;
            "measurement_gap"
        };
        let object = json!([f["page"], f["object_id"]]).to_string();
        affected.insert(object.clone());
        let entry = groups.entry((kind.into(), code.into())).or_default();
        entry.0 += 1;
        entry.1.insert(object);
    }
    let by_code: Vec<_> = groups.into_iter().map(|((kind, code), (count, objects))| {
        json!({"kind":kind,"code":code,"occurrences":count,"distinct_objects":objects.len(),
            "example_objects":objects.iter().take(3).map(|s|serde_json::from_str::<Value>(s).unwrap()).collect::<Vec<_>>(),
            "examples_truncated":objects.len()>3})
    }).collect();
    json!({"algorithm":"qa-diagnosis-v1",
        "counts":{"measured_constraint_violations":violations,"visual_questions":questions,"measurement_gaps":gaps},
        "distinct_affected_objects":affected.len(),"by_code":by_code,
        "next_step":if violations>0 {"repair_measured_constraints_then_review"}
            else if questions>0 {"independent_visual_adjudication"}
            else if gaps>0 {"resolve_measurement_scope_or_report_blocker"}
            else {"complete_independent_visual_review"},
        "routing":{
            "measured_constraint_violation":"A measured requirement is violated. Read its observed values and declared target, repair the artifact, and rebuild. A declared owner still needs independent semantic validation.",
            "visual_question":"Measured contact or ambiguous visibility needs original/blank/actual local pixel comparison. It is neither an automatic defect nor an intentional design. Preserve unresolved evidence; do not blanket-dismiss it.",
            "measurement_gap":"Input, coverage or supported geometry is insufficient. Inspect the particular code and supply genuinely missing evidence or report a tool limitation. Unknown alone is not a reason to move text, enlarge owners, add borders or start another whole-deck repair cycle."},
        "counts_are":"check occurrences, not independent visual defects; an object may appear in multiple groups",
        "progress_warning":"Fewer unknowns or changed review classifications do not prove visual repair. Compare the same object's actual local pixels and unchanged acceptance criterion; stop a stagnant method according to the existing exit policy.",
        "changes_acceptance":false,"quality_approved":false})
}

// The arguments correspond directly to the public finding fields.
#[allow(clippy::too_many_arguments)]
fn issue(
    out: &mut Vec<Value>,
    code: &str,
    domain: &str,
    state: &str,
    page: &Value,
    object: &Value,
    observed: Value,
    remedy: &str,
) {
    let id = crate::fs::digest(&serde_json::to_vec(&json!([code, page, object])).unwrap());
    out.push(
        json!({"id":format!("qa-{}", &id[..20]),"code":code,"check":domain,
        "status":state,"page":page,"object_id":object,"observed":observed,"remedy":remedy,
        "model_disposition_allowed":false}),
    );
}

// Rectangle arrays and quadrilateral objects are equivalent wire formats.
// Normalize geometry, preserving every declared semantic constraint, before
// comparing or retaining a contract. This does not infer label ownership.
fn canonical_region(value: &Value) -> Value {
    let Ok(quad) = geometry::quad(value) else {
        return value.clone();
    };
    let mut result = if value.is_object() {
        value.clone()
    } else {
        json!({})
    };
    result["quad"] = json!(quad);
    result
}

fn canonical_contract(value: &Value) -> Value {
    let mut result = value.clone();
    for page in result["pages"]
        .as_object_mut()
        .into_iter()
        .flat_map(|p| p.values_mut())
    {
        for region in page["regions"]
            .as_object_mut()
            .into_iter()
            .flat_map(|r| r.values_mut())
        {
            *region = canonical_region(region);
        }
    }
    result
}

fn constraints(spec: &Spec) -> Value {
    let pages: BTreeMap<_, _> = spec
        .pages
        .iter()
        .map(|p| {
            let owners: BTreeMap<_, _> = p.texts.iter().map(|t| (&t.id, &t.region)).collect();
            (
                p.id.clone(),
                json!({"copy":p.copy,"owners":owners,"regions":p.regions,"typography_groups":p.typography_groups,"artwork_symbols":p.artwork_symbols,
            "background":p.background["sha256"],"design":p.design.as_ref().map(|v|&v["sha256"])}),
            )
        })
        .collect();
    canonical_contract(&json!({"canvas":spec.canvas,"pages":pages}))
}

// A legacy panel is an envelope, not a verified local label. Let the producer
// add a separately measured child inside it without discarding the envelope.
// Verified targets and explicit semantic constraints still cannot be relaxed.
fn local_refinement(
    old: &Value,
    new: &Value,
    owner: &Value,
    target: &Value,
    parent_supported: bool,
    child_supported: bool,
) -> bool {
    let (Some(parent_id), Some(child_id)) = (owner.as_str(), target.as_str()) else {
        return false;
    };
    let parent = &old["regions"][parent_id];
    let child = &new["regions"][child_id];
    if parent_id == child_id
        || child["parent"] != *owner
        || child["scope"] != "label"
        || child["basis"].as_str().is_none_or(|s| s.trim().is_empty())
    {
        return false;
    }
    // A prose label declaration is not measurement. Permit correction of an
    // unsupported envelope only to a pixel-supported child; never reinterpret a
    // supported label or an explicit positional target as a generic container.
    let declared_label = parent["scope"] == "label"
        && parent["basis"]
            .as_str()
            .is_some_and(|s| !s.trim().is_empty());
    if declared_label && (parent_supported || !child_supported)
        || ["ink_align", "ink_valign", "baseline_deg", "source_quad"]
            .iter()
            .any(|field| parent.get(field).is_some())
        || old["background"] != new["background"]
    {
        return false;
    }
    let (Ok(envelope), Ok(local)) = (geometry::quad(parent), geometry::quad(child)) else {
        return false;
    };
    local.iter().all(|p| geometry::contains(envelope, *p, 1e-6))
        && parent.as_object().is_some_and(|fields| {
            fields
                .iter()
                .all(|(k, v)| matches!(k.as_str(), "quad" | "scope" | "basis") || child[k] == *v)
        })
}

/// Keep previously declared constraints. New owners/reference data may tighten a
/// previously unspecified relation; removing/moving an existing target never fixes it.
fn contract(spec: &Spec, previous: &Value, boundaries: &Boundaries) -> (Value, Vec<Value>) {
    let current = constraints(spec);
    if previous.is_null() {
        return (current, vec![]);
    }
    let previous = canonical_contract(previous);
    let mut retained = previous.clone();
    let mut changes = Vec::new();
    if previous["canvas"] != current["canvas"] {
        changes.push(json!({"page":null,"field":"canvas"}));
    }
    for (id, old) in previous["pages"].as_object().into_iter().flatten() {
        let new = &current["pages"][id];
        let symbols = |p: &Value| {
            p.get("artwork_symbols")
                .cloned()
                .unwrap_or_else(|| json!([]))
        };
        if symbols(old) != symbols(new) {
            changes.push(json!({"page":id,"field":"artwork_symbols"}));
        }
        for field in ["copy", "background", "design"] {
            if !old[field].is_null() && old[field] != new[field] {
                changes.push(json!({"page":id,"field":field}));
            }
        }
        if old["typography_groups"]
            .as_array()
            .is_some_and(|v| !v.is_empty())
            && old["typography_groups"] != new["typography_groups"]
        {
            changes.push(json!({"page":id,"field":"typography_groups"}));
        }
        for (tid, owner) in old["owners"].as_object().into_iter().flatten() {
            if !owner.is_null() && new["owners"][tid] != *owner {
                let supported = |region: &Value| {
                    region.as_str().is_some_and(|r| {
                        boundaries
                            .get(&(id.clone(), r.to_string()))
                            .is_some_and(|b| b["status"] == "supported")
                    })
                };
                if local_refinement(
                    old,
                    new,
                    owner,
                    &new["owners"][tid],
                    supported(owner),
                    supported(&new["owners"][tid]),
                ) {
                    retained["pages"][id]["owners"][tid] = new["owners"][tid].clone();
                } else {
                    changes.push(json!({"page":id,"field":"owner","object_id":tid}));
                }
            }
        }
        for (rid, region) in old["regions"].as_object().into_iter().flatten() {
            // Missing metadata may be supplied later, but existing targets
            // and constraints cannot be replaced or removed as a repair.
            if region.as_object().is_none_or(|fields| {
                fields
                    .iter()
                    // Provenance prose is not a positional constraint or pixel
                    // evidence. Retain its original record; rewording it does
                    // not change the geometric acceptance target.
                    .any(|(key, value)| key != "basis" && new["regions"][rid][key] != *value)
            }) {
                changes.push(json!({"page":id,"field":"region","region":rid}));
            }
        }
    }
    // Only tighten a previously unspecified relation, or add a new page.
    for (id, new) in current["pages"].as_object().into_iter().flatten() {
        if retained["pages"].get(id).is_none() {
            retained["pages"][id] = new.clone();
        } else {
            for field in ["owners", "regions"] {
                for (key, v) in new[field].as_object().into_iter().flatten() {
                    if retained["pages"][id][field][key].is_null() {
                        retained["pages"][id][field][key] = v.clone();
                    } else if field == "regions" {
                        for (name, value) in v.as_object().into_iter().flatten() {
                            if retained["pages"][id][field][key].get(name).is_none() {
                                retained["pages"][id][field][key][name] = value.clone();
                            }
                        }
                    }
                }
            }
            if retained["pages"][id]["design"].is_null() {
                retained["pages"][id]["design"] = new["design"].clone();
            }
            if retained["pages"][id]["typography_groups"]
                .as_array()
                .is_none_or(|v| v.is_empty())
            {
                retained["pages"][id]["typography_groups"] = new["typography_groups"].clone();
            }
        }
    }
    (retained, changes)
}

fn unsupported(shapes: &[Shape], page: usize, depth: usize, out: &mut Vec<Value>) {
    for s in shapes {
        if depth > 0 && s.text.as_ref().is_some_and(|t| !t.text().trim().is_empty()) {
            out.push(json!({"page":page,"shape_id":s.id.0,"kind":"grouped_text"}));
        }
        match &s.kind {
            ShapeKind::Group { children, .. } => unsupported(children, page, depth + 1, out),
            ShapeKind::Table(_) | ShapeKind::Chart(_) | ShapeKind::Opaque { .. } => {
                out.push(json!({"page":page,"shape_id":s.id.0,"kind":"table_chart_or_opaque"}))
            }
            _ => {}
        }
        if s.diagram.is_some() {
            out.push(json!({"page":page,"shape_id":s.id.0,"kind":"diagram"}));
        }
    }
}

pub fn evaluate(ctx: &Context, cycle_path: &str, previous: &Value) -> Result<Value> {
    let (cycle, spec) = crate::review::bound_cycle(ctx, &ctx.path(cycle_path)?)?;
    // Deliberately ignore cycle.inspection and model-authored QA JSON: they are
    // writable workspace files. Reopen and measure the actual candidate.
    let inspection = crate::review::inspect(ctx, &json!({"cycle":cycle_path}))?;
    assess(ctx, &cycle, &spec, &inspection, previous)
}

pub fn assess(
    ctx: &Context,
    cycle: &Value,
    spec: &Spec,
    inspection: &Value,
    previous: &Value,
) -> Result<Value> {
    ensure!(
        inspection["input"] == cycle["editable"] && inspection["source_spec"] == cycle["spec"],
        "native QA inspection must bind the current candidate and resolved spec"
    );
    let mut findings = Vec::new();
    let mut expected = BTreeMap::new();
    for (i, p) in spec.pages.iter().enumerate() {
        for t in &p.texts {
            expected.insert((i + 1, t.id.as_str()), t);
        }
    }
    let rows = inspection["objects"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("missing native inventory"))?;
    let mut seen = BTreeSet::new();
    let mut objects = Vec::new();
    let mut artwork_boundaries = BTreeMap::new();
    let mut owner_audit = Vec::new();
    for (i, page) in spec.pages.iter().enumerate() {
        let used: BTreeSet<_> = page
            .texts
            .iter()
            .filter_map(|t| t.region.as_ref())
            .collect();
        if used.is_empty() {
            continue;
        }
        let blank = crate::review::load_image(ctx, &ctx.verify(&page.background)?)?.to_rgb8();
        for (id, owner) in &page.regions {
            let quad = geometry::quad(owner)?.map(|p| {
                [
                    p[0] * f64::from(blank.width()) / spec.canvas.width_px,
                    p[1] * f64::from(blank.height()) / spec.canvas.height_px,
                ]
            });
            let mut proof = crate::visual_proof::owner_boundary(&blank, quad);
            proof["background"] = page.background.clone();
            proof["region_id"] = json!(id);
            if used.contains(id) {
                owner_audit.push(json!({"page":i+1,"page_id":page.id,"region_id":id,
                        "quad":geometry::quad(owner)?,"boundary":proof,
                        "object_ids":page.texts.iter().filter(|t|t.region.as_ref()==Some(id)).map(|t|&t.id).collect::<Vec<_>>(),
                        "semantic_ownership_verified":false,
                        "next":"Inspect each local card/ribbon and all linked text. A shared panel is not evidence of local alignment. Unsupported envelopes may be refined using measured children; preserve the parent and existing constraints."}));
            }
            artwork_boundaries.insert((page.id.clone(), id.clone()), proof);
        }
    }
    let (mut contract, changes) = contract(spec, previous, &artwork_boundaries);
    contract["placement_contracts"] = crate::placement::bindings(ctx)?;
    for c in &changes {
        issue(
            &mut findings,
            "CONSTRAINT_CHANGED",
            if c["field"] == "copy" || c["field"] == "artwork_symbols" {
                "copy"
            } else {
                "readability"
            },
            "unknown",
            &c["page"],
            &c["object_id"],
            c.clone(),
            "Preserve recorded constraints. An unsupported envelope can gain a contained, pixel-supported child with parent/scope/basis; retain the envelope and inherited constraints. Supported labels and explicit positional targets cannot be replaced to hide defects.",
        );
    }
    for row in rows {
        let page = row["page"].clone();
        let p = page.as_u64().unwrap_or(0) as usize;
        let id = row["object_id"].as_str().unwrap_or("");
        let key = (p, id);
        let object = if id.is_empty() {
            json!(format!("shape-{}", row["shape_id"]))
        } else {
            json!(id)
        };
        let owner = expected
            .get(&key)
            .and_then(|t| t.region.as_ref())
            .and_then(|r| spec.pages[p - 1].regions.get(r));
        let owner_verified = owner.is_some_and(|o| {
            o["scope"] == "label" && o["basis"].as_str().is_some_and(|s| !s.trim().is_empty())
        });
        let boundary = expected
            .get(&key)
            .and_then(|t| t.region.as_ref())
            .and_then(|id| artwork_boundaries.get(&(spec.pages[p - 1].id.clone(), id.clone())));
        let start = findings.len();
        if !expected.contains_key(&key) || !seen.insert(key) {
            issue(
                &mut findings,
                "UNEXPECTED_NATIVE_TEXT",
                "copy",
                "fail",
                &page,
                &object,
                row["text"].clone(),
                "Match every native text occurrence to exactly one source object; remove only proven duplicates.",
            );
        }
        let mut push = |code: &str, domain: &str, state: &str, value: Value, fix: &str| {
            issue(
                &mut findings,
                code,
                domain,
                state,
                &page,
                &object,
                value,
                fix,
            )
        };
        if row["ink"]["artwork_contact"]["candidate"] == true {
            push(
                "ARTWORK_CONTACT_UNVERIFIED",
                "readability",
                "unknown",
                row["ink"]["artwork_contact"].clone(),
                "Actual glyph pixels touch raster artwork edges. Inspect the measured local contact against blank/original/current; correct unintended interference and rerender. Contact is not automatically a defect. Preserve a genuinely intentional unresolved overlay as unknown; do not erase legitimate artwork or claim a prose pass.",
            );
        }
        for flag in row["findings"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            match flag {
                "composite_ink_unverified" => push(
                    "COMPOSITE_INK_UNVERIFIED",
                    "readability",
                    "unknown",
                    row["composite_ink"].clone(),
                    "Inspect the actual composite crop against its blank. Isolated glyphs exist, but final visible character ink is missing, ambiguous or low contrast; determine whether it is hidden or overlapping before closing the issue.",
                ),
                "declared_font_mismatch" => push(
                    "DECLARED_FONT_MISMATCH",
                    "readability",
                    "fail",
                    json!({"actual_runs":row["font_runs"],"expected":row["declared_style"]}),
                    "The reopened PPTX formatting differs from source. Restore the intended font/size/style and rebuild; do not accept source-only measurements.",
                ),
                "roundtrip_copy_mismatch" => push(
                    "COPY_MISMATCH",
                    "copy",
                    "fail",
                    row["text"].clone(),
                    "Restore the authoritative text, then rebuild.",
                ),
                "missing_glyph" => push(
                    "MISSING_GLYPH",
                    "readability",
                    "fail",
                    row["text"].clone(),
                    "Use a font containing the required glyphs; rerender.",
                ),
                "text_layout_overflow"
                    if row["ink"]["outside_frame_pixels"].as_u64().unwrap_or(0) > 0 =>
                {
                    push(
                        "TEXT_LAYOUT_OVERFLOW",
                        "readability",
                        "fail",
                        json!({"layout":row["layout"],"outside_frame_pixels":row["ink"]["outside_frame_pixels"]}),
                        "Correct actual glyph overflow or wrapping; font line-height alone is not an error.",
                    )
                }
                "ink_outside_owner" if owner_verified => push(
                    "INK_OUTSIDE_OWNER",
                    "readability",
                    "fail",
                    row["ink"]["outside_owner_pixels"].clone(),
                    "Move/fit the text within its recorded local owner; do not expand the owner to hide the error.",
                ),
                "label_baseline_mismatch" if owner_verified => push(
                    "BASELINE_MISMATCH",
                    "readability",
                    "fail",
                    row["baseline"].clone(),
                    "Compare actual_deg with expected_deg (degrees, not pixel positions). Translation does not change this error. Measure the artwork's local baseline and rotate the native text accordingly; do not infer the target from the candidate text or suppress the constraint.",
                ),
                "local_label_ownership_unverified" => push(
                    "OWNER_UNVERIFIED",
                    "readability",
                    "unknown",
                    row["region"].clone(),
                    "Identify the local owner from the artwork, or retain an explicit unverified ownership result.",
                ),
                "font_fallback" => push(
                    "FONT_FALLBACK",
                    "readability",
                    "unknown",
                    row["text"].clone(),
                    "Resolve the requested font or review a deliberate font substitution.",
                ),
                "ink_measurement_incomplete" => push(
                    "INK_MEASUREMENT_INCOMPLETE",
                    "readability",
                    "unknown",
                    row["ink"].clone(),
                    "Use a supported measurable layout; clipped measurement is not proof of containment.",
                ),
                _ => {}
            }
        }
        if !owner_verified
            && !row["findings"]
                .as_array()
                .is_some_and(|a| a.iter().any(|v| v == "local_label_ownership_unverified"))
        {
            push(
                "OWNER_UNVERIFIED",
                "readability",
                "unknown",
                row["region"].clone(),
                "A label owner needs an independent geometry basis; a scope flag alone is insufficient.",
            );
        }
        if owner_verified && boundary.is_none_or(|v| v["status"] != "supported") {
            push(
                "OWNER_BOUNDARY_UNVERIFIED",
                "readability",
                "unknown",
                boundary.cloned().unwrap_or(Value::Null),
                "Declared scope/basis is not pixel evidence. Proposed owner edges lack support in the actual blank. Inspect the local artwork and measure its boundary; borderless/ambiguous labels remain unverified rather than being accepted by prose.",
            );
        }
        if row["ink"]["status"] != "measured"
            && !row["text"].as_str().unwrap_or("").trim().is_empty()
        {
            push(
                "INK_UNAVAILABLE",
                "readability",
                "unknown",
                row["ink"].clone(),
                "Restore visible measurable glyphs; invisibility does not resolve an error.",
            );
        }
        if let Ok(b) = geometry::rect(&row["ink"]["bounds"]) {
            // Quantization allowance only; unlike the text frame this uses visible ink.
            let tolerance = 0.5 * spec.canvas.width_px / (spec.canvas.width_in * 72.);
            if b[0] < -tolerance
                || b[1] < -tolerance
                || b[0] + b[2] > spec.canvas.width_px + tolerance
                || b[1] + b[3] > spec.canvas.height_px + tolerance
            {
                push(
                    "INK_OUTSIDE_SLIDE",
                    "readability",
                    "fail",
                    json!(b),
                    "Move visible text inside the slide canvas and rerender.",
                );
            }
        }
        for hit in row["ink"]["forbidden_hits"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if owner_verified && hit["pixels"].as_u64().unwrap_or(0) > 0 {
                push(
                    "INK_IN_FORBIDDEN_REGION",
                    "readability",
                    "fail",
                    hit.clone(),
                    "Separate the actual glyphs from the recorded line/icon/neighbor exclusion area.",
                );
            }
        }
        if let Some(t) = expected.get(&key) {
            let owner = t
                .region
                .as_ref()
                .and_then(|r| spec.pages[p - 1].regions.get(r));
            if let Some(owner) = owner.filter(|_| owner_verified) {
                if let Some(max) = owner["max_lines"].as_u64()
                    && row["layout"]["lines"].as_u64().unwrap_or(u64::MAX) > max
                {
                    push(
                        "LINE_COUNT_EXCEEDED",
                        "readability",
                        "fail",
                        row["layout"]["lines"].clone(),
                        "Restore the declared line count without changing the acceptance constraint.",
                    );
                }
                // Opt-in relation checks: do not assume all text should be centred.
                if let Ok(b) = geometry::rect(&row["ink"]["bounds"]) {
                    let q = geometry::quad(owner)?;
                    let (r, angle, _) = geometry::plane(q);
                    let c = [r[0] + r[2] / 2., r[1] + r[3] / 2.];
                    let points = geometry::frame(b, 0.).map(|p| geometry::rotate(p, c, -angle));
                    let minx = points.iter().map(|p| p[0]).fold(f64::INFINITY, f64::min);
                    let maxx = points
                        .iter()
                        .map(|p| p[0])
                        .fold(f64::NEG_INFINITY, f64::max);
                    let miny = points.iter().map(|p| p[1]).fold(f64::INFINITY, f64::min);
                    let maxy = points
                        .iter()
                        .map(|p| p[1])
                        .fold(f64::NEG_INFINITY, f64::max);
                    let tolerance = 2. * spec.canvas.width_px / (spec.canvas.width_in * 72.);
                    for (field, delta) in [
                        ("ink_align", (minx + maxx) / 2. - c[0]),
                        ("ink_valign", (miny + maxy) / 2. - c[1]),
                    ] {
                        if owner[field] == "center" && delta.abs() > tolerance {
                            push(
                                "INK_NOT_CENTERED",
                                "readability",
                                "fail",
                                json!({"axis":field,"offset_px":delta,"tolerance_px":tolerance}),
                                "Align visible glyphs to the recorded owner centre; changing frame size alone is not proof of alignment.",
                            );
                        }
                    }
                }
            }
        }
        objects.push(
            json!({"page":page,"object_id":object,"text":row["text"],"region":row["region"],
            "context_viewport":row["context_viewport"],"ink":row["ink"],"owner_boundary":boundary,
            "finding_ids":findings[start..].iter().map(|f|&f["id"]).collect::<Vec<_>>(),
            "status":if findings[start..].iter().any(|f|f["status"]=="fail"){"fail"}
                else if findings.len()>start{"unknown"}else{"measured"}}),
        );
    }
    for ((p, id), t) in &expected {
        if !seen.contains(&(*p, *id)) {
            issue(
                &mut findings,
                "MISSING_NATIVE_TEXT",
                "copy",
                "fail",
                &json!(p),
                &json!(id),
                json!(t.text),
                "Restore the missing editable text occurrence; a raster copy does not replace it.",
            );
        }
    }
    let doc = crate::deck::open(ctx, &ctx.verify(&cycle["editable"])?)?;
    let mut omitted = Vec::new();
    for (i, s) in doc.slides.iter().enumerate() {
        unsupported(&s.shapes, i + 1, 0, &mut omitted);
    }
    for u in &omitted {
        issue(
            &mut findings,
            "UNSUPPORTED_TEXT_CONTAINER",
            "readability",
            "unknown",
            &u["page"],
            &u["shape_id"],
            u.clone(),
            "Grouped/table/chart/opaque content needs an independent supported check; do not claim complete native coverage.",
        );
    }
    let package = inspection["package"].clone();
    if package["issue_count"].as_u64().unwrap_or(1) > 0 {
        issue(
            &mut findings,
            "PACKAGE_INVALID",
            "package",
            "fail",
            &Value::Null,
            &Value::Null,
            package,
            "Repair package structure and reopen the actual PPTX.",
        );
    }
    findings.extend(crate::placement::assess(ctx, spec, inspection)?);
    for page in inspection["composite_ink"]["pages"]
        .as_array()
        .into_iter()
        .flatten()
    {
        if page["status"] == "unknown" || page["unassigned_pixels"].as_u64().unwrap_or(0) > 0 {
            issue(
                &mut findings,
                "UNATTRIBUTED_COMPOSITE_INK",
                "readability",
                "unknown",
                &page["page"],
                &Value::Null,
                page.clone(),
                "Review the full-minus-derived-blank difference. Some visible pixels could not be assigned to native character cells; do not count incomplete attribution as a pass.",
            );
        }
    }
    let typography = crate::typography::assess(spec, rows);
    for group in &typography {
        if group["status"] != "consistent" {
            issue(
                &mut findings,
                "TYPOGRAPHY_GROUP_MISMATCH",
                "readability",
                if group["status"] == "fail" {
                    "fail"
                } else {
                    "unknown"
                },
                &group["page"],
                &json!(format!(
                    "typography:{}",
                    group["group_id"].as_str().unwrap()
                )),
                group.clone(),
                "Use the intended common formatting for this same-role group, fit each local label, then inspect actual PPTX runs and pixels. Do not remove group members to hide inconsistency.",
            );
        }
    }
    let failed = findings.iter().filter(|f| f["status"] == "fail").count();
    let unknown = findings.iter().filter(|f| f["status"] == "unknown").count();
    let diagnosis = diagnosis(&findings);
    Ok(
        json!({"algorithm":ALGORITHM,"input":cycle["editable"],"spec":cycle["spec"],
        "engine_revision":crate::ENGINE_REV,"status":if failed>0{"fail"}else if unknown>0{"unknown"}else{"pass"},
        "failed":failed,"unknown":unknown,"diagnosis":diagnosis,"findings":findings,"objects":objects,"owner_audit":owner_audit,"typography_groups":typography,
        "inventory":{"expected_text_objects":expected.len(),"measured_native_text_objects":rows.len(),"unsupported":omitted},
        "constraint_changes":changes,"contract":contract,
        "quality_approved":false,"visual_review_required":true,
        "scope":"Native text identity, visible glyph bounds, declared local constraints and unsupported-container coverage. Raster semantics, occlusion and visual aesthetics still require independent review."}),
    )
}

pub fn check(ctx: &Context, r: &Value) -> Result<Value> {
    // There is no request-level pass override, tolerance override or ignore list.
    for key in [
        "ignore",
        "dismiss",
        "allow_failures",
        "threshold",
        "tolerance",
        "passed",
        "status",
    ] {
        ensure!(
            r.get(key).is_none(),
            "qa_check does not accept {key} overrides"
        );
    }
    let old = if ctx.path(".octos/pptx-evidence-history.json")?.is_file() {
        ctx.json(".octos/pptx-evidence-history.json")?["qa_contract"].clone()
    } else {
        Value::Null
    };
    let report = evaluate(ctx, string(r, "cycle")?, &old)?;
    if let Some(path) = r["output"].as_str() {
        ctx.write_json(path, &report)?;
        Ok(
            json!({"qa_command":true,"report":ctx.binding(path)?,"status":report["status"],
            "failed":report["failed"],"unknown":report["unknown"],"diagnosis":report["diagnosis"],"quality_approved":false,
            "visual_review_required":true}),
        )
    } else {
        let mut report = report;
        report["qa_command"] = json!(true);
        Ok(report)
    }
}
