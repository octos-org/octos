//! Joint proposals for authored label regions, using existing geometry and fit.
//! Region semantics and final visual choice stay with the agent and its reviewer.
use crate::{Context, geometry, spec, string};
use anyhow::Context as _;
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::collections::BTreeSet;

pub fn usage() -> Value {
    json!({"request":{"action":"layout_region","spec":"source.json","page":"overview",
        "group_id":"labels","sizes_pt":[18,16],"padding_px":4,"clearance_px":4,
        "max_lines":1,"output":"proposals/labels"},
        "prerequisites":"Declare a typography group with size_pt in fields and a local artwork region for every member. Source regions provide geometry/forbidden areas; model supplies semantics. sizes_pt are explicit alternatives, never guessed.",
        "result":"Immutable candidate source files and actual PPTX previews; whole-group edits are withheld on conflict. Read candidate previews, choose one source, then run the complete cycle and independent peer review. No source is overwritten."})
}

fn gap(a: [f64; 4], b: [f64; 4]) -> f64 {
    let x = (a[0] - b[0] - b[2]).max(b[0] - a[0] - a[2]).max(0.);
    let y = (a[1] - b[1] - b[3]).max(b[1] - a[1] - a[3]).max(0.);
    x.hypot(y)
}
fn intersects(a: [f64; 4], b: [f64; 4]) -> bool {
    a[0] < b[0] + b[2] && b[0] < a[0] + a[2] && a[1] < b[1] + b[3] && b[1] < a[1] + a[3]
}
fn envelope(q: geometry::Quad) -> [f64; 4] {
    let l = q.iter().map(|p| p[0]).fold(f64::INFINITY, f64::min);
    let t = q.iter().map(|p| p[1]).fold(f64::INFINITY, f64::min);
    [
        l,
        t,
        q.iter().map(|p| p[0]).fold(f64::NEG_INFINITY, f64::max) - l,
        q.iter().map(|p| p[1]).fold(f64::NEG_INFINITY, f64::max) - t,
    ]
}

fn contextual_crop(
    ctx: &Context,
    image: &Value,
    area: [f64; 4],
    output: &std::path::Path,
) -> Result<Value> {
    let path = ctx.verify(image)?;
    let (w, h) = image::image_dimensions(&path)?;
    let left = (area[0] * w as f64).floor().max(0.) as u32;
    let top = (area[1] * h as f64).floor().max(0.) as u32;
    let right = (area[2] * w as f64).ceil().min(w as f64) as u32;
    let bottom = (area[3] * h as f64).ceil().min(h as f64) as u32;
    crate::review::crop(
        ctx,
        &json!({"action":"crop","input":path,
        "box":[left,top,right-left,bottom-top],"output":output}),
    )
}

pub fn propose(ctx: &Context, r: &Value) -> Result<Value> {
    ensure!(
        r.as_object()
            .context("region_layout::propose: missing required value")?
            .keys()
            .all(|k| [
                "action",
                "spec",
                "page",
                "group_id",
                "sizes_pt",
                "padding_px",
                "clearance_px",
                "max_lines",
                "output",
                "width"
            ]
            .contains(&k.as_str())),
        "unsupported layout_region option"
    );
    let path = ctx.path(string(r, "spec")?)?;
    let binding = ctx.binding(&path)?;
    let original = spec::load(ctx, &path, None)?;
    let page_id = string(r, "page")?;
    let pi = original
        .pages
        .iter()
        .position(|p| p.id == page_id)
        .ok_or_else(|| anyhow::anyhow!("unknown page"))?;
    let group = original.pages[pi]
        .typography_groups
        .iter()
        .find(|g| g.id == r["group_id"])
        .ok_or_else(|| anyhow::anyhow!("declare the same-role typography group first"))?;
    ensure!(
        group.fields.iter().any(|f| f == "size_pt"),
        "group must constrain size_pt"
    );
    let members: BTreeSet<_> = group.members.iter().cloned().collect();
    let sizes = r["sizes_pt"]
        .as_array()
        .filter(|a| !a.is_empty() && a.len() <= 8)
        .ok_or_else(|| anyhow::anyhow!("sizes_pt requires 1 to 8 explicit alternatives"))?;
    let mut numbers = Vec::new();
    for v in sizes {
        let s = v
            .as_f64()
            .filter(|s| s.is_finite() && *s > 0. && *s <= 1000.)
            .ok_or_else(|| anyhow::anyhow!("invalid chosen size"))?;
        ensure!(!numbers.contains(&s), "duplicate chosen size");
        numbers.push(s);
    }
    let padding = r["padding_px"]
        .as_f64()
        .filter(|v| v.is_finite() && (0. ..=100.).contains(v))
        .ok_or_else(|| anyhow::anyhow!("explicit padding_px required"))?;
    let clearance = r["clearance_px"]
        .as_f64()
        .filter(|v| v.is_finite() && (0. ..=32.).contains(v))
        .ok_or_else(|| anyhow::anyhow!("explicit clearance_px required"))?;
    let lines = r["max_lines"]
        .as_u64()
        .filter(|n| *n > 0 && *n <= 10000)
        .ok_or_else(|| anyhow::anyhow!("explicit max_lines required"))?;
    let out = ctx.path(string(r, "output")?)?;
    ensure!(!out.exists(), "use a fresh proposal directory");
    let mut base = original.clone();
    let mut targets = Vec::new();
    for t in base.pages[pi]
        .texts
        .iter_mut()
        .filter(|t| members.contains(&t.id))
    {
        let owner = t
            .region
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("{} needs a local artwork region", t.id))?;
        let region = &original.pages[pi].regions[owner];
        let plane = geometry::proposal(&json!({"region":region,"padding":padding}))?;
        ensure!(
            plane["perspective_or_shear"] == false,
            "{} needs a nonrigid transform; rigid region layout cannot certify it",
            t.id
        );
        t.rotation = plane["rotation"]
            .as_f64()
            .context("region_layout::propose: missing required value")?;
        targets.push(json!({"page":page_id,"id":t.id,"box":plane["box"],"rotation":t.rotation}));
    }
    for owner in original.pages[pi]
        .texts
        .iter()
        .filter(|t| members.contains(&t.id))
        .filter_map(|t| t.region.as_ref())
    {
        ensure!(
            !original.pages[pi]
                .texts
                .iter()
                .any(|t| !members.contains(&t.id) && t.region.as_ref() == Some(owner)),
            "region {owner} also owns an unchanged label; declare separate local regions before changing its line budget"
        );
        let r = base.pages[pi]
            .regions
            .get_mut(owner)
            .context("region_layout::propose: missing required value")?;
        let q = geometry::quad(r)?;
        if !r.is_object() {
            *r = json!({"quad":q});
        }
        r["max_lines"] = json!(
            r["max_lines"]
                .as_u64()
                .map(|n| n.min(lines))
                .unwrap_or(lines)
        );
    }
    base.validate()?;
    let base_path = ctx.write_json(out.join("planned-source.json"), &json!(base))?;
    let mut points = Vec::new();
    for target in &targets {
        points.extend(geometry::frame(
            geometry::rect(&target["box"])?,
            target["rotation"]
                .as_f64()
                .context("region_layout::propose: missing required value")?,
        ));
    }
    let cw = original.canvas.width_px;
    let ch = original.canvas.height_px;
    let l = points.iter().map(|p| p[0]).fold(f64::INFINITY, f64::min);
    let t = points.iter().map(|p| p[1]).fold(f64::INFINITY, f64::min);
    let rr = points
        .iter()
        .map(|p| p[0])
        .fold(f64::NEG_INFINITY, f64::max);
    let bb = points
        .iter()
        .map(|p| p[1])
        .fold(f64::NEG_INFINITY, f64::max);
    let dx = ((rr - l) * 0.15).max(cw * 0.025);
    let dy = ((bb - t) * 0.15).max(ch * 0.025);
    let area = [
        ((l - dx) / cw).max(0.),
        ((t - dy) / ch).max(0.),
        ((rr + dx) / cw).min(1.),
        ((bb + dy) / ch).min(1.),
    ];
    let mut media = Vec::new();
    let mut views = Vec::new();
    if let Some(design) = &original.pages[pi].design {
        let crop = contextual_crop(ctx, design, area, &out.join("reference-context.png"))?;
        media.push(crop["crop"]["path"].clone());
        views.push(json!({"role":"reference","evidence":crop}));
    }
    let mut obstacles = Vec::new();
    for t in original.pages[pi]
        .texts
        .iter()
        .filter(|t| !members.contains(&t.id))
    {
        let (_, _, m) = crate::placement::measure(&original, t)?;
        ensure!(
            m["ink"]["status"] == "measured" && m["ink"]["clipped_by_measurement_window"] != true,
            "unchanged neighbor {} could not be measured; resolve it before joint layout",
            t.id
        );
        obstacles.push((
            format!("native:{}", t.id),
            geometry::rect(&m["ink"]["bounds"])?,
        ));
    }
    for s in &original.pages[pi].artwork_symbols {
        obstacles.push((format!("artwork_symbol:{}", s.id), s.bounds));
    }
    let mut candidates = Vec::new();
    for (index, size) in numbers.iter().enumerate() {
        let fit = crate::fit::propose(
            ctx,
            &json!({"action":"fit","spec":base_path,"group":{"page":page_id,"id":group.id,"size_pt":size},
            "reflow":true,"ink_align":"middle","targets":targets,"max_lines":lines}),
        )?;
        let mut conflicts = Vec::new();
        let rows = fit["proposals"]
            .as_array()
            .context("region_layout::propose: missing required value")?;
        for row in rows.iter().filter(|row| row["state"] != "proposal") {
            conflicts.push(json!({"code":"FIT_CONSTRAINT","object":row["id"],"measurement":row,
                "next":"Choose another explicit common size or revise the intended local region/line budget; no partial group edits."}));
        }
        if conflicts.is_empty() {
            let mut occupied = obstacles.clone();
            for row in rows {
                let id = row["id"]
                    .as_str()
                    .context("region_layout::propose: missing required value")?;
                let ink = geometry::rect(&row["ink_alignment"]["after"]["bounds"])?;
                let t = base.pages[pi]
                    .texts
                    .iter()
                    .find(|t| t.id == id)
                    .context("region_layout::propose: missing required value")?;
                let owner = &base.pages[pi].regions[t
                    .region
                    .as_ref()
                    .context("region_layout::propose: missing required value")?];
                let mut local = occupied.clone();
                for (i, q) in owner["forbidden"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .enumerate()
                {
                    local.push((format!("forbidden:{i}"), envelope(geometry::quad(q)?)));
                }
                for (obstacle, area) in local {
                    let separation = gap(ink, area);
                    if intersects(ink, area) || separation + 0.001 < clearance {
                        conflicts.push(json!({"code":"INK_CLEARANCE","object":id,"obstacle":obstacle,
                            "actual_gap_px":separation,"required_gap_px":clearance,"ink":ink,"obstacle_box":area,
                            "next":"Reconsider the local allocation or an explicitly offered common size; preserve the obstacle."}));
                    }
                }
                occupied.push((format!("group:{id}"), ink));
            }
        }
        let mut result = json!({"candidate":index+1,"size_pt":size,"status":if conflicts.is_empty(){"candidate"}else{"infeasible_under_declared_constraints"},
            "conflicts":conflicts,"quality_approved":false,"edits":[]});
        if result["conflicts"]
            .as_array()
            .context("region_layout::propose: missing required value")?
            .is_empty()
        {
            let edits: Vec<_> = rows
                .iter()
                .map(|row| json!({"page":row["page"],"id":row["id"],"set":row["set"]}))
                .collect();
            let candidate = out.join(format!("candidate-{}.json", index + 1));
            crate::spec::patch(
                ctx,
                &json!({"action":"patch","spec":base_path,"output":candidate,"edits":edits}),
            )?;
            let build = crate::deck::build(
                ctx,
                &json!({"action":"build","spec":candidate,"page":page_id,"output":out.join(format!("candidate-{}",index+1))}),
            )?;
            let pptx = ctx.verify(&build["editable"])?;
            let doc = crate::deck::open(ctx, &pptx)?;
            let preview = crate::deck::render_to(
                ctx,
                &doc,
                &pptx,
                &out.join(format!("preview-{}", index + 1)),
                crate::deck::width(r)?,
            )?;
            result["spec"] = ctx.binding(&candidate)?;
            result["pptx"] = build["editable"].clone();
            result["previews"] = preview["pages"].clone();
            let crop = contextual_crop(
                ctx,
                &preview["pages"][0],
                area,
                &out.join(format!("candidate-{}-context.png", index + 1)),
            )?;
            media.push(crop["crop"]["path"].clone());
            views.push(
                json!({"role":"candidate","candidate":index+1,"size_pt":size,"evidence":crop}),
            );
            result["context_preview"] = crop;
            result["edits"] = json!(edits);
        }
        candidates.push(result);
    }
    ctx.verify(&binding)?;
    let result = json!({"source":binding,"page":page_id,"group_id":group.id,"targets":targets,"candidates":candidates,
        "model_media":media,"views":views,"viewport_normalized_xyxy":area,
        "source_modified":false,"quality_approved":false,
        "limits":"Joint clearance uses conservative axis-aligned glyph envelopes and declared obstacles. Decorative/unknown raster ink is not semantically segmented. Derived rotations assume rigid labels. Regions, meaning and aesthetics need independent review.",
        "next":"Read candidate PNGs; choose a complete candidate source, preserving all pages. Run cycle and hand the first readable version to the existing independent peer before further aesthetic refinement. Do not copy only the successful subset of an infeasible group."});
    ctx.write_json(out.join("report.json"), &result)?;
    Ok(result)
}
