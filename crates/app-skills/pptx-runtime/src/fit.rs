//! Proposals using the SAME shaping/layout engine as export. Never mutate source.
use crate::{Context, string};
use anyhow::Context as _;
use anyhow::{Result, ensure};
use serde_json::{Value, json};

pub fn usage() -> Value {
    json!({
        "single_request":{"action":"fit","spec":"source.json","objects":["page-id/text-id"],
            "ink_align":"middle","targets":[{"page":"page-id","id":"text-id","box":[100,100,500,80],"rotation":0}]},
        "group_source_page_fragment":{"id":"page-id","typography_groups":[{
            "id":"labels","members":["label-a","label-b"],"fields":["size_pt"],
            "basis":"Two same-level labels identified from the artwork; heading excluded"}]},
        "group_request":{"action":"fit","spec":"source.json","reflow":true,"ink_align":"middle",
            "group":{"page":"page-id","id":"labels","size_pt":18},
            "targets":[{"page":"page-id","id":"label-a","box":[100,100,500,80]},
                {"page":"page-id","id":"label-b","box":[100,250,500,80]}]},
        "notes":"Send one request object, not this entire help document. Declare typography_groups on the source page first: it is an ARRAY, members are local text IDs, fields is an ARRAY of property NAMES (not a map of values), and basis is required. The numeric size belongs to request.group.size_pt, not the declaration. A group requires at least two members and every target; one failed member withholds all edits. Do not combine group with min/max. Targets are canvas pixels; rotation matches the text. Without group, omitted min/max preserve size. Returns proposals, never visual approval."})
}

pub fn propose(ctx: &Context, r: &Value) -> Result<Value> {
    for key in r.as_object().into_iter().flat_map(|v| v.keys()) {
        ensure!(
            [
                "action",
                "spec",
                "page",
                "objects",
                "reflow",
                "ink_align",
                "targets",
                "min_size_pt",
                "max_size_pt",
                "max_lines",
                "group",
                "output"
            ]
            .contains(&key.as_str()),
            "unsupported fit option {key}; no request constraints were silently ignored"
        );
    }
    let source = ctx.path(string(r, "spec")?)?;
    let spec = crate::spec::load(ctx, &source, r["page"].as_str())?;
    let group = r
        .get("group")
        .map(|g| prepare_group(&spec, r, g))
        .transpose()?;
    let mut doc = deckcraft_engine::model::Presentation {
        slide_size: deckcraft_engine::model::Size::new(
            spec.canvas.width_in * 72.,
            spec.canvas.height_in * 72.,
        ),
        ..Default::default()
    };
    let mut rows = Vec::new();
    let reflow = r
        .get("reflow")
        .map(|v| {
            v.as_bool()
                .ok_or_else(|| anyhow::anyhow!("reflow must be boolean"))
        })
        .transpose()?
        .unwrap_or(false);
    let selection = group
        .as_ref()
        .map(|g| json!(g.members))
        .or_else(|| r.get("objects").cloned());
    let requested = selection
        .as_ref()
        .map(|v| {
            v.as_array().ok_or_else(|| {
                anyhow::anyhow!("objects must be an array of page/object string IDs")
            })
        })
        .transpose()?;
    let ids: std::collections::BTreeSet<_> = spec
        .pages
        .iter()
        .flat_map(|p| p.texts.iter().map(move |t| format!("{}/{}", p.id, t.id)))
        .collect();
    if let Some(requested) = requested {
        ensure!(
            !requested.is_empty()
                && requested
                    .iter()
                    .all(|v| v.as_str().is_some_and(|s| ids.contains(s))),
            "objects must select known page/object STRING IDs, e.g. {} (not {{page, object}}); source IDs are not display page numbers",
            serde_json::to_string(&ids.iter().take(5).collect::<Vec<_>>())?
        );
    }
    let alignment = r
        .get("ink_align")
        .map(|v| {
            v.as_str()
                .ok_or_else(|| anyhow::anyhow!("ink_align must be a string"))
        })
        .transpose()?;
    ensure!(
        alignment.is_none_or(|a| matches!(a, "top" | "middle" | "bottom")),
        "ink_align must be top, middle or bottom"
    );
    let mut targets = std::collections::BTreeMap::new();
    let mut target_rotations = std::collections::BTreeMap::new();
    if let Some(values) = r.get("targets") {
        ensure!(alignment.is_some(), "targets requires ink_align");
        for v in values
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("targets must be an array"))?
        {
            let key = format!("{}/{}", string(v, "page")?, string(v, "id")?);
            ensure!(
                ids.contains(&key) && requested.is_none_or(|a| a.iter().any(|v| v == &key)),
                "target must select a known requested object"
            );
            let rect = crate::geometry::rect(&v["box"])?;
            let rotation = match v.get("rotation") {
                Some(value) => value
                    .as_f64()
                    .filter(|n| n.is_finite())
                    .ok_or_else(|| anyhow::anyhow!("target rotation must be finite degrees"))?,
                None => 0.,
            };
            let canvas =
                crate::geometry::frame([0., 0., spec.canvas.width_px, spec.canvas.height_px], 0.);
            ensure!(
                crate::geometry::frame(rect, rotation)
                    .iter()
                    .all(|p| crate::geometry::contains(canvas, *p, 0.)),
                "ink target outside canvas"
            );
            target_rotations.insert(key.clone(), rotation);
            ensure!(targets.insert(key, rect).is_none(), "duplicate ink target");
        }
    }
    ensure!(
        !reflow || !targets.is_empty(),
        "reflow requires explicit targets and ink_align"
    );
    if let Some(g) = &group {
        ensure!(
            reflow && alignment.is_some() && g.members.iter().all(|id| targets.contains_key(id)),
            "group fit requires reflow:true, ink_align and an explicit local target for every member"
        );
    }
    let minimum = r
        .get("min_size_pt")
        .map(|n| {
            n.as_f64()
                .ok_or_else(|| anyhow::anyhow!("min_size_pt must be numeric"))
        })
        .transpose()?
        .or_else(|| group.as_ref().map(|g| g.size));
    if let Some(min) = minimum {
        ensure!(
            min > 0. && min <= 1000. && min.is_finite(),
            "invalid min_size_pt"
        )
    }
    let maximum = r
        .get("max_size_pt")
        .map(|n| {
            n.as_f64()
                .filter(|n| n.is_finite() && *n > 0. && *n <= 1000.)
                .ok_or_else(|| anyhow::anyhow!("max_size_pt must be positive and at most 1000"))
        })
        .transpose()?
        .or_else(|| group.as_ref().map(|g| g.size));
    let max_lines = r
        .get("max_lines")
        .map(|n| {
            n.as_u64().filter(|n| *n > 0 && *n <= 10000).ok_or_else(|| {
                anyhow::anyhow!("max_lines must be a positive integer at most 10000")
            })
        })
        .transpose()?;
    for p in &spec.pages {
        for t in &p.texts {
            let identity = format!("{}/{}", p.id, t.id);
            if requested.is_some_and(|a| !a.iter().any(|id| id == &identity)) {
                continue;
            }
            // A target was previously only an ink-translation destination. Opt-in
            // reflow must rebuild the actual shape BEFORE asking the layout engine.
            let mut trial = t.clone();
            if reflow && let Some(target) = targets.get(&identity) {
                trial.bounds = *target;
            }
            let mut shape = crate::deck::text_shape(&mut doc, &spec, &trial)?;
            let original = spec.style(t)?;
            let context = deckcraft_engine::model::resolve::Ctx::for_slide(&doc, &doc.slides[0])
                .ok_or_else(|| anyhow::anyhow!("missing context"))?;
            let fonts = deckcraft_text::font_usage(
                &context,
                &shape,
                shape
                    .text
                    .as_ref()
                    .context("fit::propose: missing required value")?,
            );
            let font_provenance: Vec<_> = fonts.iter().map(|f| json!({
                "requested":f.requested,"actual_family":f.actual.family,"actual_style":f.actual.style,
                "font_sha256":f.actual.sha256(),"face_index":f.actual.index(),
                "variation_coordinates":f.actual.coords,"reason":f.reason,"missing_glyph":f.missing_glyph,
                "synthetic_bold":f.synthetic_bold,"synthetic_italic":f.synthetic_italic})).collect();
            if fonts.is_empty()
                || fonts
                    .iter()
                    .any(|f| f.reason != "requested" || f.missing_glyph)
            {
                rows.push(json!({"page":p.id,"id":t.id,"state":"font_unresolved",
                    "font_provenance":font_provenance,"set":{},"visual_review_required":true,
                    "reason":"The requested face does not cover the text without fallback. Choose an explicit installed font with the required glyphs before fitting; fallback metrics cannot silently drive a placement proposal."}));
                continue;
            }
            let layout =
                |sh: &deckcraft_engine::model::Shape| -> Result<deckcraft_text::TextLayout> {
                    let xf = sh
                        .xfrm
                        .ok_or_else(|| anyhow::anyhow!("missing transform"))?;
                    let geo = deckcraft_engine::render::shape_geometry(sh, xf.w, xf.h);
                    let body = sh
                        .text
                        .as_ref()
                        .ok_or_else(|| anyhow::anyhow!("missing text"))?;
                    Ok(deckcraft_text::layout(
                        &context,
                        sh,
                        body,
                        &deckcraft_text::Opts {
                            rect: geo.text_rect,
                            fields: &deckcraft_text::NoFields,
                            prompt_color: None,
                            no_shrink: true,
                        },
                    ))
                };
            let before = layout(&shape)?;
            let mut low = minimum.unwrap_or(original.size_pt);
            let mut high = maximum.unwrap_or(original.size_pt);
            ensure!(low <= high, "minimum font exceeds maximum for {identity}");
            if original.locked {
                ensure!(
                    low <= original.size_pt && original.size_pt <= high,
                    "size interval conflicts with locked style for {identity}"
                );
                low = original.size_pt;
                high = original.size_pt;
            }
            let owner_line_limit = t
                .region
                .as_ref()
                .and_then(|id| p.regions.get(id))
                .and_then(|v| v["max_lines"].as_u64());
            let line_limit = max_lines.into_iter().chain(owner_line_limit).min();
            let fits = |candidate: &deckcraft_text::TextLayout,
                        sh: &deckcraft_engine::model::Shape| {
                !candidate.overflow
                    && line_limit.is_none_or(|n| candidate.lines.len() as u64 <= n)
                    && sh
                        .text
                        .as_ref()
                        .is_some_and(|body| candidate.forced_line_breaks(body) == 0)
            };
            set_size(&mut shape, low);
            let smallest = layout(&shape)?;
            if !fits(&smallest, &shape) {
                rows.push(json!({"page":p.id,"id":t.id,"state":"no_fit_within_bounds",
                    "current_size_pt":original.size_pt,"allowed_size_pt":[low,high],"max_lines":line_limit,
                    "overflow_before":before.overflow,"overflow_at_minimum":smallest.overflow,
                    "lines_at_minimum":smallest.lines.len(),
                    "forced_breaks_at_minimum":smallest.forced_line_breaks(shape.text.as_ref().context("fit::propose: missing required value")?),
                    "layout_box":trial.bounds,"set":{},"visual_review_required":true,
                    "reason":"No layout at the explicit size floor satisfies frame, line limit and unbroken-word constraints. Reconsider the local layout; no partial shrink, copy change or constraint relaxation was applied."}));
                continue;
            }
            set_size(&mut shape, high);
            let mut size = high;
            if !fits(&layout(&shape)?, &shape) {
                let (mut lo, mut hi) = (low, high);
                for _ in 0..18 {
                    let mid = (lo + hi) / 2.;
                    set_size(&mut shape, mid);
                    if fits(&layout(&shape)?, &shape) {
                        lo = mid;
                    } else {
                        hi = mid;
                    }
                }
                size = low.max((lo * 100.).floor() / 100.);
            }
            set_size(&mut shape, size);
            let after = layout(&shape)?;
            ensure!(
                fits(&after, &shape),
                "selected font failed its own layout constraints for {identity}"
            );
            let mut format = t.format.clone().unwrap_or_else(|| json!({}));
            format["size_pt"] = json!(size);
            let mut row = json!({"page":p.id,"id":t.id,"state":"proposal","current_size_pt":original.size_pt,"proposed_size_pt":size,"allowed_size_pt":[low,high],"max_lines":line_limit,"overflow_before":before.overflow,"overflow_after":after.overflow,"needed_height_pt":after.content_height,"needed_width_pt":after.content_width,"lines":after.lines.len(),"forced_line_breaks":after.forced_line_breaks(shape.text.as_ref().context("fit::propose: missing required value")?),"set":if size!=original.size_pt{json!({"format":format})}else{json!({})},"visual_review_required":true});
            row["reflowed"] = json!(reflow && targets.contains_key(&identity));
            row["font_provenance"] = json!(font_provenance);
            row["layout_box"] = json!(trial.bounds);
            if let Some(vertical) = alignment {
                let target = *targets.get(&identity).unwrap_or(&t.bounds);
                let owner = t
                    .region
                    .as_ref()
                    .and_then(|id| p.regions.get(id))
                    .map(crate::geometry::quad)
                    .transpose()?;
                let (mut measurement, mut proposed) = align_ink(
                    &doc,
                    &shape,
                    &after,
                    &spec,
                    &original,
                    target,
                    target_rotations.get(&identity).copied().unwrap_or(0.),
                    vertical,
                    owner,
                )?;
                // Like native_fit.py's line-box-room calculation, remove only
                // unused frame height when it alone prevents placement. Do not
                // change the font, wrap width, owner, or complete line layout.
                if proposed.is_none()
                    && measurement["reason_code"] == "frame_outside_owner_or_canvas"
                {
                    let xf = shape.xfrm.expect("text shape has a transform");
                    let body = &shape.text.as_ref().expect("text shape has a body").body;
                    let minimum_height = after.content_height
                        + body.inset_t.unwrap_or(0.)
                        + body.inset_b.unwrap_or(0.)
                        + 0.25;
                    if minimum_height < xf.h {
                        let mut compact = shape.clone();
                        compact
                            .xfrm
                            .as_mut()
                            .context("fit::propose: missing required value")?
                            .h = minimum_height;
                        let compact_layout = layout(&compact)?;
                        let same_lines = !compact_layout.overflow
                            && compact_layout.lines.len() == after.lines.len()
                            && (compact_layout.content_width - after.content_width).abs() < 0.001
                            && (compact_layout.content_height - after.content_height).abs() < 0.001;
                        if same_lines {
                            let (mut candidate, mut bounds) = align_ink(
                                &doc,
                                &compact,
                                &compact_layout,
                                &spec,
                                &original,
                                target,
                                target_rotations.get(&identity).copied().unwrap_or(0.),
                                vertical,
                                owner,
                            )?;
                            if bounds.is_some() {
                                let (mut low, mut high) = (minimum_height, xf.h);
                                for _ in 0..10 {
                                    let middle = (low + high) / 2.;
                                    compact
                                        .xfrm
                                        .as_mut()
                                        .context("fit::propose: missing required value")?
                                        .h = middle;
                                    let compact_layout = layout(&compact)?;
                                    let (checked, next) = align_ink(
                                        &doc,
                                        &compact,
                                        &compact_layout,
                                        &spec,
                                        &original,
                                        target,
                                        target_rotations.get(&identity).copied().unwrap_or(0.),
                                        vertical,
                                        owner,
                                    )?;
                                    if next.is_some() {
                                        low = middle;
                                        candidate = checked;
                                        bounds = next;
                                    } else {
                                        high = middle;
                                    }
                                }
                                candidate["frame_height_adjustment"] = json!({
                                    "from_pt":xf.h,"to_pt":low,"minimum_line_box_pt":minimum_height,
                                    "font_changed":false,"wrap_width_changed":false,"line_layout_preserved":true,
                                    "method":"retain the largest feasible height after removing unused line-box space; original owner/canvas constraints remain"});
                                measurement = candidate;
                                proposed = bounds;
                            } else {
                                measurement["compact_frame_attempt"] = candidate;
                            }
                        }
                    }
                }
                row["ink_alignment"] = measurement;
                if let Some(bounds) = proposed {
                    row["set"]["box"] = json!(bounds);
                } else {
                    // Do not apply partial shrink edits as if they satisfied the requested target.
                    row["set"] = json!({});
                    row["state"] = json!("placement_infeasible");
                }
            }
            rows.push(row);
        }
    }
    let group_result = group.map(|g| -> Result<Value> {
        let blocking: Vec<_> = rows.iter().filter(|row| row["state"] != "proposal")
            .map(|row| Ok(format!("{}/{}", row["page"].as_str().context("fit result requires page")?, row["id"].as_str().context("fit result requires id")?))).collect::<Result<Vec<_>>>()?;
        if !blocking.is_empty() {
            for row in &mut rows {
                row["set"] = json!({});
                if row["state"] == "proposal" {
                    row["state"] = json!("group_blocked");
                    row["reason"] = json!("Another member cannot fit the chosen common size. No partial group edits; reconsider that local target or explicitly choose another size for the entire group.");
                }
            }
        }
        Ok(json!({"page":g.page,"id":g.id,"size_pt":g.size,"members":g.members,
            "status":if blocking.is_empty(){"proposed"}else{"infeasible"},
            "blocking_objects":blocking,"semantics_verified":false,"quality_approved":false}))
    }).transpose()?;
    let placements = rows
        .iter()
        .filter(|row| row["set"]["box"].is_array())
        .count();
    let result = json!({"source":ctx.binding(source)?,"proposals":rows,"source_modified":false,"quality_approved":false,"group":group_result,
        "mode":if alignment.is_some(){"ink_alignment"}else{"size_measurement"},
        "placement_proposal_count":placements,
        "placement_note":if alignment.is_none(){"No text placement was calculated. Size measurement alone does not align text. For positioning, use ink_align plus explicit targets; apply only a proposed set.box after checking the target against the artwork."}else{"Only rows with ink_alignment.status=proposed and set.box contain a placement proposal. Infeasible/unsupported rows must not be reported as aligned."},
        "note":"reflow=true lays out text at each explicit target width; otherwise wrapping width is preserved. Size changes require explicit bounds. The largest fitting size is proposed, respecting max_lines and unsplit words; infeasible rows have no edits. Explicit newlines and owner constraints remain. Actual composite render and artwork relationships still require review."});
    if let Some(output) = r["output"].as_str() {
        ctx.write_json(output, &result)?;
    }
    Ok(result)
}

struct FitGroup {
    page: String,
    id: String,
    members: Vec<String>,
    size: f64,
}

fn prepare_group(spec: &crate::spec::Spec, request: &Value, value: &Value) -> Result<FitGroup> {
    let fields = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("group must be an object"))?;
    ensure!(
        fields
            .keys()
            .all(|k| ["page", "id", "size_pt"].contains(&k.as_str())),
        "unsupported fit group field"
    );
    ensure!(
        request.get("min_size_pt").is_none() && request.get("max_size_pt").is_none(),
        "group size is exact; do not combine it with min_size_pt or max_size_pt"
    );
    let page_id = string(value, "page")?;
    let id = string(value, "id")?;
    let page = spec
        .pages
        .iter()
        .find(|p| p.id == page_id)
        .ok_or_else(|| anyhow::anyhow!("unknown group page {page_id}"))?;
    let group = page
        .typography_groups
        .iter()
        .find(|g| g.id == id)
        .ok_or_else(|| {
            anyhow::anyhow!("declare the same-role typography group before fitting it")
        })?;
    ensure!(
        group.fields.iter().any(|f| f == "size_pt"),
        "fit group must declare size_pt consistency"
    );
    let size = value["size_pt"]
        .as_f64()
        .filter(|s| s.is_finite() && *s > 0. && *s <= 1000.)
        .ok_or_else(|| anyhow::anyhow!("group size_pt must be positive and at most 1000"))?;
    let members: Vec<_> = group
        .members
        .iter()
        .map(|id| format!("{page_id}/{id}"))
        .collect();
    if let Some(selection) = request.get("objects") {
        let selected = selection
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("objects must be an array"))?;
        ensure!(
            selected.len() == members.len()
                && members.iter().all(|id| selected.iter().any(|v| v == id)),
            "group fit must select every group member exactly once"
        );
    }
    Ok(FitGroup {
        page: page_id.into(),
        id: id.into(),
        members,
        size,
    })
}

fn set_size(shape: &mut deckcraft_engine::model::Shape, size: f64) {
    if let Some(body) = &mut shape.text {
        for para in &mut body.paragraphs {
            para.def_props.size = Some(size);
            para.end_props.size = Some(size);
            for run in &mut para.runs {
                run.props.size = Some(size);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn align_ink(
    doc: &deckcraft_engine::model::Presentation,
    shape: &deckcraft_engine::model::Shape,
    layout: &deckcraft_text::TextLayout,
    spec: &crate::spec::Spec,
    style: &crate::spec::Style,
    target: [f64; 4],
    target_rotation: f64,
    vertical: &str,
    owner: Option<crate::geometry::Quad>,
) -> Result<(Value, Option<[f64; 4]>)> {
    use crate::geometry::{angle, contains, frame, rotate};
    let xf = shape
        .xfrm
        .ok_or_else(|| anyhow::anyhow!("missing text transform"))?;
    let mut evidence = json!({"status":"infeasible","target":target,"units":"canvas_pixels",
        "horizontal":style.align,"vertical":vertical,"quality_proven":false,
        "target_rotation":target_rotation,
        "method":"isolated native glyph alpha at 2 px/pt in the rigid label coordinate system; translation only; same layout engine as export"});
    if angle(xf.rot, target_rotation).abs() > 0.001 || style.align == "justify" {
        evidence["status"] = json!("unsupported");
        evidence["reason"] = json!(
            "The target and text must have the same explicit rigid rotation. Use geometry to establish the local frame first; perspective, shear and justified placement are not inferred."
        );
        return Ok((evidence, None));
    }
    let scale = spec.canvas.width_px / (spec.canvas.width_in * 72.);
    let ink = crate::review::measure_ink(doc, 0, shape, xf, layout, scale, (owner, &[], None))?;
    evidence["before"] = ink.clone();
    // Measure the glyphs in their own axes, not the bounding rectangle of the
    // rotated image. The latter grows with angle and does not describe margins.
    let local_ink = if xf.rot.abs() > 0.001 {
        let mut local_shape = shape.clone();
        let mut local_xf = xf;
        local_xf.rot = 0.;
        local_shape.xfrm = Some(local_xf);
        crate::review::measure_ink(
            doc,
            0,
            &local_shape,
            local_xf,
            layout,
            scale,
            (None, &[], None),
        )?
    } else {
        ink.clone()
    };
    if ink["status"] != "measured"
        || ink["clipped_by_measurement_window"] == true
        || local_ink["status"] != "measured"
        || local_ink["clipped_by_measurement_window"] == true
        || layout.overflow
    {
        evidence["reason"] =
            json!("A complete, nonoverflowing glyph measurement is required before placement.");
        return Ok((evidence, None));
    }
    let mut b: [f64; 4] = serde_json::from_value(local_ink["bounds"].clone())?;
    let current_center = [(xf.x + xf.w / 2.) * scale, (xf.y + xf.h / 2.) * scale];
    let target_center = [target[0] + target[2] / 2., target[1] + target[3] / 2.];
    let center_in_target_axes = rotate(current_center, target_center, -target_rotation);
    b[0] += center_in_target_axes[0] - current_center[0];
    b[1] += center_in_target_axes[1] - current_center[1];
    evidence["before_in_target_axes"] = json!(b);
    if b[2] > target[2] + 0.25 || b[3] > target[3] + 0.25 {
        evidence["reason"] = json!(
            "Visible glyphs exceed the requested target. Choose a suitable target or explicitly permit a font change; no automatic shrinking."
        );
        return Ok((evidence, None));
    }
    let ax = match style.align.as_str() {
        "center" => 0.5,
        "right" => 1.,
        _ => 0.,
    };
    let ay = match vertical {
        "middle" => 0.5,
        "bottom" => 1.,
        _ => 0.,
    };
    let local_delta = [
        target[0] + ax * (target[2] - b[2]) - b[0],
        target[1] + ay * (target[3] - b[3]) - b[1],
    ];
    let [dx, dy] = rotate(local_delta, [0., 0.], target_rotation);
    let bounds = [
        xf.x * scale + dx,
        xf.y * scale + dy,
        xf.w * scale,
        xf.h * scale,
    ];
    let canvas = frame([0., 0., spec.canvas.width_px, spec.canvas.height_px], 0.);
    if !frame(bounds, xf.rot)
        .iter()
        .all(|p| contains(canvas, *p, 0.) && owner.is_none_or(|q| contains(q, *p, 0.)))
    {
        evidence["reason_code"] = json!("frame_outside_owner_or_canvas");
        evidence["attempted_frame"] = json!(bounds);
        evidence["reason"] = json!(
            "Alignment would move the native frame outside the canvas or declared owner. Review geometry; constraints were not expanded."
        );
        return Ok((evidence, None));
    }
    let mut proposed = shape.clone();
    let mut moved = xf;
    moved.x += dx / scale;
    moved.y += dy / scale;
    proposed.xfrm = Some(moved);
    evidence["after"] =
        crate::review::measure_ink(doc, 0, &proposed, moved, layout, scale, (owner, &[], None))?;
    evidence["status"] = json!("proposed");
    evidence["delta_px"] = json!([dx, dy]);
    evidence["owner_semantics_verified"] = json!(false);
    Ok((evidence, Some(bounds)))
}
