//! Conservative, artwork-aware text placement. No deck-specific coordinates.
//! A feasible envelope is not semantic or aesthetic approval. Contracts are
//! application gates, not protection against arbitrary workspace writes.
use crate::{
    Context, geometry,
    spec::{Spec, Text},
    string,
};
use anyhow::Context as _;
use anyhow::{Result, ensure};
use deckcraft_engine::model::Presentation;
use image::{RgbImage, RgbaImage};
use serde_json::{Value, json};
use std::{collections::BTreeSet, sync::Arc};

const STORE: &str = ".octos/pptx-placement-contracts";
const CELL: f64 = 3.;
const EDGE: u8 = 40;
// PPTX coordinates are quantized to integer EMUs. A round trip can turn an
// exact grid edge into e.g. 101.99995 canvas pixels. Snap only this subpixel
// numerical noise; do not count the entire adjacent occupied cell as a hit.
const GRID_ROUNDOFF_PX: f64 = 0.001;

fn grid_coordinate(px: f64) -> f64 {
    let cells = px / CELL;
    let nearest = cells.round();
    if (cells - nearest).abs() * CELL <= GRID_ROUNDOFF_PX {
        nearest
    } else {
        cells
    }
}

pub fn usage() -> Value {
    json!({"required_top_level":["action","spec","page","id","output"],
        "first_search_also_requires":"area = [x,y,width,height] (not four corners)",
        "example":{"action":"layout_search","spec":"source.json","page":"scene","id":"caption",
            "area":[30,40,500,260],"forbidden":[[30,40,160,210]],"register":false,"output":"new-search"},
        "note":"id is a top-level text object ID, not object or targets[].id. No targets/ink_align fields. Coordinates in the example are synthetic; measure the actual local area."})
}

pub fn bindings(ctx: &Context) -> Result<Value> {
    let dir = ctx.path(STORE)?;
    if !dir.is_dir() {
        return Ok(json!([]));
    }
    let mut paths = std::fs::read_dir(dir)?
        .map(|e| e.map(|v| v.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    paths.sort();
    Ok(json!(
        paths
            .into_iter()
            .filter(|p| p.extension().is_some_and(|s| s == "json"))
            .map(|p| ctx.binding(p))
            .collect::<Result<Vec<_>>>()?
    ))
}

struct Grid {
    area: [f64; 4],
    w: usize,
    h: usize,
    occupied: Vec<bool>,
}
impl Grid {
    fn components(&self) -> Vec<Value> {
        let mut seen = vec![false; self.occupied.len()];
        let mut rows = Vec::new();
        for index in 0..self.occupied.len() {
            if seen[index] || !self.occupied[index] {
                continue;
            }
            seen[index] = true;
            let mut queue = vec![index];
            let mut head = 0;
            let (mut left, mut right, mut top, mut bottom) = (
                index % self.w,
                index % self.w,
                index / self.w,
                index / self.w,
            );
            while head < queue.len() {
                let at = queue[head];
                head += 1;
                let x = at % self.w;
                let y = at / self.w;
                left = left.min(x);
                right = right.max(x);
                top = top.min(y);
                bottom = bottom.max(y);
                for dy in -1..=1 {
                    for dx in -1..=1 {
                        let nx = x as isize + dx;
                        let ny = y as isize + dy;
                        if nx < 0 || ny < 0 || nx >= self.w as isize || ny >= self.h as isize {
                            continue;
                        }
                        let next = ny as usize * self.w + nx as usize;
                        if !seen[next] && self.occupied[next] {
                            seen[next] = true;
                            queue.push(next);
                        }
                    }
                }
            }
            if queue.len() >= 3 {
                rows.push(json!({"box":self.rect([left,top,right-left+1,bottom-top+1]),"occupied_cells":queue.len()}));
            }
        }
        rows.sort_by_key(|v| {
            std::cmp::Reverse(
                v["occupied_cells"]
                    .as_u64()
                    .expect("component rows contain the computed occupied-cell count"),
            )
        });
        rows.truncate(64);
        rows
    }
    fn rect(&self, r: [usize; 4]) -> [f64; 4] {
        [
            self.area[0] + r[0] as f64 * CELL,
            self.area[1] + r[1] as f64 * CELL,
            r[2] as f64 * CELL,
            r[3] as f64 * CELL,
        ]
    }
    fn collision(&self, b: [f64; 4]) -> usize {
        let x0 = grid_coordinate(b[0] - self.area[0]).floor().max(0.) as usize;
        let y0 = grid_coordinate(b[1] - self.area[1]).floor().max(0.) as usize;
        let x1 = grid_coordinate(b[0] + b[2] - self.area[0]).ceil().max(0.) as usize;
        let y1 = grid_coordinate(b[1] + b[3] - self.area[1]).ceil().max(0.) as usize;
        (y0..y1.min(self.h))
            .flat_map(|y| (x0..x1.min(self.w)).map(move |x| y * self.w + x))
            .filter(|i| self.occupied[*i])
            .count()
    }
    fn free_rectangles(&self) -> Vec<[usize; 4]> {
        // Maximal empty rectangles from row histograms; no random coordinate guesses.
        let mut heights = vec![0usize; self.w];
        let mut rectangles = BTreeSet::new();
        for y in 0..self.h {
            for (x, h) in heights.iter_mut().enumerate() {
                *h = if self.occupied[y * self.w + x] {
                    0
                } else {
                    *h + 1
                };
            }
            let mut stack: Vec<(usize, usize)> = Vec::new();
            for (x, height) in heights
                .iter()
                .copied()
                .chain(std::iter::once(0))
                .enumerate()
            {
                let mut start = x;
                while stack.last().is_some_and(|(_, h)| *h > height) {
                    let (left, h) = stack
                        .pop()
                        .expect("the while condition checked the same stack is nonempty");
                    if x - left >= 3 && h >= 3 {
                        rectangles.insert([left, y + 1 - h, x - left, h]);
                    }
                    start = left;
                }
                if height > 0 && stack.last().is_none_or(|(_, h)| *h < height) {
                    stack.push((start, height));
                }
            }
        }
        let mut result: Vec<_> = rectangles.into_iter().collect();
        result.sort_by_key(|r| std::cmp::Reverse(r[2] * r[3]));
        result.truncate(512); // reported resource bound, not a proof of infeasibility
        result
    }
}

fn inside(area: [f64; 4], b: [f64; 4]) -> bool {
    b[0] >= area[0] - 0.1
        && b[1] >= area[1] - 0.1
        && b[0] + b[2] <= area[0] + area[2] + 0.1
        && b[1] + b[3] <= area[1] + area[3] + 0.1
}

fn grid(
    ctx: &Context,
    spec: &Spec,
    page: usize,
    c: &Value,
    neighbors: &[[f64; 4]],
) -> Result<(Grid, RgbImage)> {
    let area = geometry::rect(&c["area"])?;
    let clearance = c["clearance_px"].as_f64().unwrap_or(3.);
    ensure!(
        clearance.is_finite() && (0. ..=32.).contains(&clearance),
        "invalid clearance"
    );
    ensure!(
        inside([0., 0., spec.canvas.width_px, spec.canvas.height_px], area),
        "area outside canvas"
    );
    let w = (area[2] / CELL).floor() as usize;
    let h = (area[3] / CELL).floor() as usize;
    ensure!(
        w > 0 && h > 0 && w * h <= 1_000_000,
        "placement area exceeds grid budget"
    );
    let art = crate::review::load_image(ctx, &ctx.verify(&spec.pages[page].background)?)?.to_rgb8();
    let sx = f64::from(art.width()) / spec.canvas.width_px;
    let sy = f64::from(art.height()) / spec.canvas.height_px;
    let forbidden: Vec<_> = c["forbidden"]
        .as_array()
        .into_iter()
        .flatten()
        .map(geometry::quad)
        .collect::<Result<_>>()?;
    let mut occupied = vec![false; w * h];
    for y in 0..h {
        for x in 0..w {
            let rect = [
                area[0] + x as f64 * CELL,
                area[1] + y as f64 * CELL,
                CELL,
                CELL,
            ];
            let center = [rect[0] + CELL / 2., rect[1] + CELL / 2.];
            if neighbors.iter().any(|b| {
                rect[0] < b[0] + b[2]
                    && rect[0] + CELL > b[0]
                    && rect[1] < b[1] + b[3]
                    && rect[1] + CELL > b[1]
            }) || forbidden
                .iter()
                .any(|q| geometry::contains(*q, center, CELL))
            {
                occupied[y * w + x] = true;
                continue;
            }
            if c["avoid_artwork"] == false {
                continue;
            }
            let x0 = (rect[0] * sx).floor().max(2.) as u32;
            let y0 = (rect[1] * sy).floor().max(2.) as u32;
            let x1 = ((rect[0] + CELL) * sx)
                .ceil()
                .min(f64::from(art.width().saturating_sub(2))) as u32;
            let y1 = ((rect[1] + CELL) * sy)
                .ceil()
                .min(f64::from(art.height().saturating_sub(2))) as u32;
            'pixels: for py in y0..y1 {
                for px in x0..x1 {
                    let a = art.get_pixel(px, py).0;
                    for (dx, dy) in [(2, 0), (-2, 0), (0, 2), (0, -2)] {
                        let b = art
                            .get_pixel((px as i64 + dx) as u32, (py as i64 + dy) as u32)
                            .0;
                        if (0..3).any(|k| a[k].abs_diff(b[k]) >= EDGE) {
                            occupied[y * w + x] = true;
                            break 'pixels;
                        }
                    }
                }
            }
        }
    }
    let before = occupied.clone();
    let radius = (clearance / CELL).ceil() as isize;
    for y in 0..h {
        for x in 0..w {
            if x as f64 * CELL < clearance
                || y as f64 * CELL < clearance
                || (w - 1 - x) as f64 * CELL < clearance
                || (h - 1 - y) as f64 * CELL < clearance
            {
                occupied[y * w + x] = true;
            }
            if !before[y * w + x] {
                continue;
            }
            for dy in -radius..=radius {
                for dx in -radius..=radius {
                    let nx = x as isize + dx;
                    let ny = y as isize + dy;
                    if nx >= 0 && ny >= 0 && nx < w as isize && ny < h as isize {
                        occupied[ny as usize * w + nx as usize] = true;
                    }
                }
            }
        }
    }
    Ok((
        Grid {
            area,
            w,
            h,
            occupied,
        },
        art,
    ))
}

pub(crate) fn measure(
    spec: &Spec,
    t: &Text,
) -> Result<(Presentation, deckcraft_engine::model::Shape, Value)> {
    let mut doc = Presentation {
        slide_size: deckcraft_engine::model::Size::new(
            spec.canvas.width_in * 72.,
            spec.canvas.height_in * 72.,
        ),
        ..Default::default()
    };
    let shape = crate::deck::text_shape(&mut doc, spec, t)?;
    let xf = shape
        .xfrm
        .context("placement::measure: missing required value")?;
    let context = deckcraft_engine::model::resolve::Ctx::for_slide(&doc, &doc.slides[0])
        .ok_or_else(|| anyhow::anyhow!("missing context"))?;
    let layout = deckcraft_text::layout(
        &context,
        &shape,
        shape
            .text
            .as_ref()
            .context("placement::measure: missing required value")?,
        &deckcraft_text::Opts {
            rect: deckcraft_engine::render::shape_geometry(&shape, xf.w, xf.h).text_rect,
            fields: &deckcraft_text::NoFields,
            prompt_color: None,
            no_shrink: true,
        },
    );
    let ink = crate::review::measure_ink(
        &doc,
        0,
        &shape,
        xf,
        &layout,
        spec.canvas.width_px / (spec.canvas.width_in * 72.),
        (None, &[], None),
    )?;
    Ok((
        doc,
        shape,
        json!({"ink":ink,"lines":layout.lines.len(),"overflow":layout.overflow,
        "needed_height_px":layout.content_height*spec.canvas.width_px/(spec.canvas.width_in*72.)}),
    ))
}

fn neighbors(spec: &Spec, p: usize, id: &str) -> Result<Vec<[f64; 4]>> {
    spec.pages[p]
        .texts
        .iter()
        .filter(|t| t.id != id)
        .map(|t| {
            let (_, _, m) = measure(spec, t)?;
            geometry::rect(&m["ink"]["bounds"])
        })
        .collect()
}

fn save_image(ctx: &Context, path: &str, img: &RgbImage) -> Result<Value> {
    let mut bytes = std::io::Cursor::new(Vec::new());
    img.write_to(&mut bytes, image::ImageFormat::Png)?;
    ctx.write_new(path, &bytes.into_inner())?;
    ctx.binding(path)
}

pub fn search(ctx: &Context, r: &Value) -> Result<Value> {
    for key in r
        .as_object()
        .expect("search candidates include their computed distance")
        .keys()
    {
        ensure!(
            [
                "action",
                "spec",
                "page",
                "id",
                "area",
                "forbidden",
                "clearance_px",
                "avoid_artwork",
                "register",
                "output"
            ]
            .contains(&key.as_str()),
            "unknown layout_search field {key}"
        );
    }
    let source = ctx.path(string(r, "spec")?)?;
    let spec = crate::spec::load(ctx, &source, None)?;
    let page = string(r, "page")?;
    let id = string(r, "id")?;
    let pi = spec
        .pages
        .iter()
        .position(|p| p.id == page)
        .ok_or_else(|| anyhow::anyhow!("unknown page"))?;
    let t = spec.pages[pi]
        .texts
        .iter()
        .find(|t| t.id == id)
        .ok_or_else(|| anyhow::anyhow!("unknown text object"))?;
    ensure!(
        t.rotation.abs() < 0.001,
        "layout_search currently supports axis-aligned text only"
    );
    let style = spec.style(t)?;
    ensure!(
        style.wrap,
        "layout_search requires existing wrap=true; it never changes typography silently"
    );
    let key = crate::fs::digest(format!("{page}/{id}").as_bytes());
    let record = format!("{STORE}/{key}.json");
    let mut c = if ctx.path(&record)?.is_file() {
        ctx.json(&record)?
    } else {
        let area = geometry::rect(&r["area"])?;
        let mut forbidden = r.get("forbidden").cloned().unwrap_or(json!([]));
        ensure!(
            forbidden.is_array(),
            "forbidden must be an array of quads/rectangles"
        );
        if let Some(owner) = t
            .region
            .as_ref()
            .and_then(|v| spec.pages[pi].regions.get(v))
            && let Some(known) = owner["forbidden"].as_array()
        {
            forbidden
                .as_array_mut()
                .expect("search candidates include their computed distance")
                .extend(known.clone());
        }
        json!({"page":page,"id":id,"area":area,"forbidden":forbidden,
            "clearance_px":r.get("clearance_px").cloned().unwrap_or(json!(3.)),
            "avoid_artwork":r.get("avoid_artwork").cloned().unwrap_or(json!(true)),
            "background":spec.pages[pi].background,"canvas":spec.canvas,"copy":t.text,"style":style,
            "registered":false})
    };
    for field in ["area", "forbidden", "clearance_px", "avoid_artwork"] {
        if c["registered"] == true
            && let Some(value) = r.get(field)
        {
            ensure!(
                *value == c[field],
                "registered {field} cannot be changed; reuse the saved constraint"
            );
        }
    }
    ensure!(
        c["background"] == spec.pages[pi].background && c["canvas"] == json!(spec.canvas),
        "registered reference changed"
    );
    ensure!(
        crate::spec::normalized(string(&c, "copy")?) == crate::spec::normalized(&t.text)
            && c["style"] == json!(style),
        "registered copy/font/style changed"
    );
    let other = neighbors(&spec, pi, id)?;
    let (mask, art) = grid(ctx, &spec, pi, &c, &other)?;
    // Measure the artwork independently of producer exclusions. Otherwise an
    // oversized forbidden rectangle would hide the evidence needed to correct it.
    let mut probe = c.clone();
    probe["forbidden"] = json!([]);
    probe["clearance_px"] = json!(0.);
    probe["avoid_artwork"] = json!(true);
    let (art_grid, _) = grid(ctx, &spec, pi, &probe, &[])?;
    let components = art_grid.components();
    if r["register"] == true && c["registered"] != true {
        c["registered"] = json!(true);
        ctx.write_json(&record, &c)?;
    }
    let out = string(r, "output")?;
    let rects = mask.free_rectangles();
    let mut proposals = Vec::new();
    let mut tried = 0;
    for rect in &rects {
        let mut trial = t.clone();
        trial.bounds = mask.rect(*rect);
        let (_, _, m) = measure(&spec, &trial)?;
        tried += 1;
        if m["overflow"] == true
            || m["ink"]["status"] != "measured"
            || m["ink"]["clipped_by_measurement_window"] == true
        {
            continue;
        }
        let ink = geometry::rect(&m["ink"]["bounds"])?;
        if !inside(mask.area, ink) || mask.collision(ink) > 0 {
            continue;
        }
        let distance = (ink[0] - t.bounds[0]).hypot(ink[1] - t.bounds[1]);
        proposals.push(
            json!({"set":{"box":trial.bounds},"measurement":m,"distance_px":distance,
            "status":"feasible_under_constraints","quality_approved":false}),
        );
    }
    proposals.sort_by(|a, b| {
        a["measurement"]["lines"]
            .as_u64()
            .cmp(&b["measurement"]["lines"].as_u64())
            .then_with(|| {
                a["distance_px"]
                    .as_f64()
                    .expect("search candidates include their computed distance")
                    .total_cmp(
                        &b["distance_px"]
                            .as_f64()
                            .expect("search candidates include their computed distance"),
                    )
            })
    });
    proposals.truncate(8);
    let sx = f64::from(art.width()) / spec.canvas.width_px;
    let sy = f64::from(art.height()) / spec.canvas.height_px;
    let crop = [
        (mask.area[0] * sx) as u32,
        (mask.area[1] * sy) as u32,
        (mask.area[2] * sx).floor() as u32,
        (mask.area[3] * sy).floor() as u32,
    ];
    let mut overlay = art.clone();
    for y in 0..mask.h {
        for x in 0..mask.w {
            if mask.occupied[y * mask.w + x] {
                let b = mask.rect([x, y, 1, 1]);
                for py in (b[1] * sy) as u32
                    ..((b[1] + CELL) * sy).ceil().min(f64::from(art.height())) as u32
                {
                    for px in (b[0] * sx) as u32
                        ..((b[0] + CELL) * sx).ceil().min(f64::from(art.width())) as u32
                    {
                        let old = overlay.get_pixel(px, py).0;
                        overlay.put_pixel(
                            px,
                            py,
                            image::Rgb([
                                255,
                                (u16::from(old[1]) / 2) as u8,
                                (u16::from(old[2]) / 2) as u8,
                            ]),
                        );
                    }
                }
            }
        }
    }
    let overlay =
        image::imageops::crop_imm(&overlay, crop[0], crop[1], crop[2], crop[3]).to_image();
    let proof = save_image(ctx, &format!("{out}/obstacles.png"), &overlay)?;
    for (i, p) in proposals.iter_mut().enumerate() {
        let mut trial = t.clone();
        trial.bounds = geometry::rect(&p["set"]["box"])?;
        let (mut doc, shape, _) = measure(&spec, &trial)?;
        Arc::make_mut(&mut doc.slides[0]).shapes = vec![shape];
        Arc::make_mut(&mut doc.slides[0]).show_master_shapes = false;
        let rendered = deckcraft_engine::render::render_slide(
            &doc,
            0,
            &deckcraft_engine::render::RenderOpts {
                scale: f64::from(art.width()) / (spec.canvas.width_in * 72.),
                transparent_background: true,
                ..Default::default()
            },
        );
        let foreground = RgbaImage::from_raw(rendered.width, rendered.height, rendered.pixels)
            .ok_or_else(|| anyhow::anyhow!("invalid render"))?;
        let mut preview = image::DynamicImage::ImageRgb8(art.clone()).to_rgba8();
        image::imageops::overlay(&mut preview, &foreground, 0, 0);
        let preview = image::DynamicImage::ImageRgba8(preview).to_rgb8();
        let preview =
            image::imageops::crop_imm(&preview, crop[0], crop[1], crop[2], crop[3]).to_image();
        p["preview"] = save_image(ctx, &format!("{out}/candidate-{}.png", i + 1), &preview)?;
    }
    let result = json!({"source":ctx.binding(source)?,"page":page,"id":id,"constraints":c,
        "artwork_components":{"method":"8-connected contrast-edge cells, independent of supplied forbidden regions and native text","units":"canvas_pixels","cell_size_px":CELL,"regions":components,"semantic_labels_verified":false},
        "obstacles_preview":proof,"proposals":proposals,"tested_rectangles":tried,"search_limit":512,
        "grid_cell_px":CELL,"status":if proposals.is_empty(){"no_candidate_found"}else{"candidates_found"},
        "source_modified":false,"quality_approved":false,
        "limitations":"Conservative envelope/edge screening, not semantic segmentation. Faint edges, solid illustration interiors and ambiguous artwork require independently measured forbidden regions. No candidate found is not proof that every layout is impossible. Previews show target text over artwork; actual rebuilt PPTX and neighbors still require review."});
    ctx.write_json(format!("{out}/report.json"), &result)?;
    Ok(result)
}

/// Automatically called by native QA and evidence_record, not an optional model verdict.
pub fn assess(ctx: &Context, spec: &Spec, inspection: &Value) -> Result<Vec<Value>> {
    let dir = ctx.path(STORE)?;
    if !dir.is_dir() {
        return Ok(vec![]);
    }
    let mut findings = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let c = ctx.json(path)?;
        let page = string(&c, "page")?;
        let id = string(&c, "id")?;
        let mut reasons = Vec::new();
        let pi = spec.pages.iter().position(|p| p.id == page);
        let row = inspection["objects"].as_array().and_then(|rows| {
            rows.iter()
                .find(|v| v["page_id"] == page && v["object_id"] == id)
        });
        if let (Some(pi), Some(row)) = (pi, row) {
            let t = spec.pages[pi]
                .texts
                .iter()
                .find(|t| t.id == id)
                .ok_or_else(|| anyhow::anyhow!("missing object"))?;
            if c["background"] != spec.pages[pi].background || c["canvas"] != json!(spec.canvas) {
                reasons.push(json!("reference_changed"));
            }
            if c["style"] != json!(spec.style(t)?)
                || crate::spec::normalized(string(&c, "copy")?)
                    != crate::spec::normalized(row["text"].as_str().unwrap_or(""))
            {
                reasons.push(json!("copy_or_style_changed"));
            }
            let mut other = Vec::new();
            for n in inspection["objects"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|v| v["page_id"] == page && v["object_id"] != id)
            {
                other.push(geometry::rect(&n["ink"]["bounds"])?);
            }
            let (g, _) = grid(ctx, spec, pi, &c, &other)?;
            if let Ok(b) = geometry::rect(&row["ink"]["bounds"]) {
                if !inside(g.area, b) {
                    reasons.push(json!("ink_outside_registered_area"));
                }
                let hits = g.collision(b);
                if hits > 0 {
                    reasons.push(json!({"obstacle_cells":hits,"ink_bounds":b,
                        "grid_cell_px":CELL,"grid_roundoff_tolerance_px":GRID_ROUNDOFF_PX}));
                }
            } else {
                reasons.push(json!("ink_measurement_missing"));
            }
            if row["layout"]["overflow"] == true
                || row["ink"]["clipped_by_measurement_window"] == true
            {
                reasons.push(json!("overflow_or_incomplete_measurement"));
            }
        } else {
            reasons.push(json!("registered_object_missing"));
        }
        if !reasons.is_empty() {
            findings.push(json!({"id":format!("placement-{}",crate::fs::digest(format!("{page}/{id}").as_bytes())),
                "code":"PLACEMENT_CONSTRAINT_FAILED","check":"readability","status":"fail","page":page,"object_id":id,
                "observed":reasons,"model_disposition_allowed":false,
                "remedy":"Use layout_search with the original registered constraints, rebuild and inspect the actual local render. A prose pass cannot clear this measurement."}));
        }
    }
    Ok(findings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emu_roundtrip_noise_does_not_expand_ink_into_adjacent_grid_cell() {
        let grid = Grid {
            area: [10., 20., 15., 15.],
            w: 5,
            h: 5,
            occupied: (0..25)
                .map(|i| i % 5 == 0 || i / 5 == 0 || i % 5 == 4 || i / 5 == 4)
                .collect(),
        };
        let exact = [13., 23., 9., 9.];
        assert_eq!(grid.collision(exact), 0);
        // The same envelope after an integer-EMU export/import, at all edges.
        assert_eq!(grid.collision([12.99995, 22.99995, 9.0001, 9.0001]), 0);
        // Real subpixel encroachment and full-cell overlaps remain failures.
        for bad in [
            [12.99, 23., 9., 9.],
            [13., 22.99, 9., 9.],
            [13., 23., 9.01, 9.],
            [13., 23., 9., 9.01],
            [10., 23., 9., 9.],
        ] {
            assert!(grid.collision(bad) > 0, "{bad:?}");
        }
    }
}
