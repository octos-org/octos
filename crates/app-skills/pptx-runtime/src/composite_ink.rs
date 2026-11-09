//! Attribute actual full-minus-derived-blank components to rendered character cells.
//! This is visibility evidence, not semantic ownership, OCR or visual approval.
use crate::{Context, geometry};
use anyhow::{Result, ensure};
use deckcraft_engine::model::{Presentation, resolve};
use serde_json::{Value, json};

type Rect = [f64; 4]; // left, top, right, bottom in render pixels
fn bounds(q: geometry::Quad) -> Rect {
    [
        q.iter().map(|p| p[0]).fold(f64::INFINITY, f64::min),
        q.iter().map(|p| p[1]).fold(f64::INFINITY, f64::min),
        q.iter().map(|p| p[0]).fold(f64::NEG_INFINITY, f64::max),
        q.iter().map(|p| p[1]).fold(f64::NEG_INFINITY, f64::max),
    ]
}
fn overlap(a: Rect, b: Rect) -> bool {
    a[0] <= b[2] && a[2] >= b[0] && a[1] <= b[3] && a[3] >= b[1]
}
fn union(a: Rect, b: Rect) -> Rect {
    [
        a[0].min(b[0]),
        a[1].min(b[1]),
        a[2].max(b[2]),
        a[3].max(b[3]),
    ]
}
struct Part {
    pixels: Vec<(u32, u32)>,
    bounds: Rect,
}
struct Mapping {
    shape: u32,
    id: String,
    cells: Vec<geometry::Quad>,
    frame: Rect,
}

fn components(mut mask: Vec<bool>, width: usize, height: usize) -> Vec<Part> {
    let mut parts = Vec::new();
    for start in 0..mask.len() {
        if !mask[start] {
            continue;
        }
        mask[start] = false;
        let mut stack = vec![start];
        let mut pixels = Vec::new();
        let mut b = [width as f64, height as f64, 0., 0.];
        while let Some(i) = stack.pop() {
            let (x, y) = (i % width, i / width);
            pixels.push((x as u32, y as u32));
            b = union(b, [x as f64, y as f64, (x + 1) as f64, (y + 1) as f64]);
            for ny in y.saturating_sub(1)..=(y + 1).min(height - 1) {
                for nx in x.saturating_sub(1)..=(x + 1).min(width - 1) {
                    let j = ny * width + nx;
                    if mask[j] {
                        mask[j] = false;
                        stack.push(j);
                    }
                }
            }
        }
        parts.push(Part { pixels, bounds: b });
    }
    parts
}
fn touches(part: &Part, q: geometry::Quad) -> bool {
    overlap(part.bounds, bounds(q))
        && part
            .pixels
            .iter()
            .any(|(x, y)| geometry::contains(q, [*x as f64 + 0.5, *y as f64 + 0.5], 1.))
}

fn mappings(doc: &Presentation, page: usize, size: (u32, u32)) -> Result<Vec<Mapping>> {
    let slide = &doc.slides[page];
    let ctx = resolve::Ctx::for_slide(doc, slide)
        .ok_or_else(|| anyhow::anyhow!("missing slide context"))?;
    let sx = size.0 as f64 / doc.slide_size.width;
    let sy = size.1 as f64 / doc.slide_size.height;
    let mut out = Vec::new();
    for shape in &slide.shapes {
        let Some(body) = &shape.text else { continue };
        let xf = resolve::xfrm(&ctx, shape);
        let layout = deckcraft_text::layout(
            &ctx,
            shape,
            body,
            &deckcraft_text::Opts {
                rect: deckcraft_engine::render::shape_geometry(shape, xf.w, xf.h).text_rect,
                fields: &deckcraft_text::NoFields,
                prompt_color: None,
                no_shrink: false,
            },
        );
        let center = [xf.x + xf.w / 2., xf.y + xf.h / 2.];
        let mut cells = Vec::new();
        // Vertical/body-rotated text needs an additional transform; retain unknown.
        if layout.rotation.abs() < 0.001 {
            let chars: Vec<Vec<char>> = body
                .paragraphs
                .iter()
                .map(|p| p.text().chars().collect())
                .collect();
            for line in &layout.lines {
                for (i, (left, right)) in line.edges.iter().enumerate() {
                    if right - left <= 1e-9
                        || line.bottom - line.top <= 1e-9
                        || chars
                            .get(line.para)
                            .and_then(|c| c.get(line.start + i))
                            .is_none_or(|c| c.is_whitespace() || c.is_control())
                    {
                        continue;
                    }
                    let q = geometry::frame(
                        [
                            xf.x + left,
                            xf.y + line.top,
                            right - left,
                            line.bottom - line.top,
                        ],
                        0.,
                    )
                    .map(|p| {
                        let p = geometry::rotate(p, center, xf.rot);
                        [p[0] * sx, p[1] * sy]
                    });
                    cells.push(q);
                }
            }
        }
        let frame = bounds(
            geometry::frame([xf.x, xf.y, xf.w, xf.h], xf.rot).map(|p| [p[0] * sx, p[1] * sy]),
        );
        out.push(Mapping {
            shape: shape.id.0,
            id: shape
                .name
                .strip_prefix("octos:text:")
                .unwrap_or(&shape.name)
                .to_string(),
            cells,
            frame,
        });
    }
    Ok(out)
}

pub fn audit(ctx: &Context, cycle: &Value, doc: &Presentation) -> Result<Value> {
    let mut pages = Vec::new();
    let mut objects = Vec::new();
    for page in 0..doc.slides.len() {
        let full = &cycle["renders"]["editable"]["pages"][page];
        let blank = &cycle["renders"]["blank"]["pages"][page];
        if full.is_null() || blank.is_null() {
            pages.push(json!({"page":page+1,"status":"unknown","reason":"actual full/derived-blank pair unavailable"}));
            continue;
        }
        ensure!(
            cycle["renders"]["editable"]["input"] == cycle["editable"]
                && cycle["renders"]["blank"]["input"] == cycle["blank"],
            "composite evidence must bind both actual PPTX files"
        );
        let a = crate::review::load_image(ctx, &ctx.verify(full)?)?.to_rgb8();
        let b = crate::review::load_image(ctx, &ctx.verify(blank)?)?.to_rgb8();
        ensure!(
            a.dimensions() == b.dimensions(),
            "composite full/blank dimensions differ"
        );
        let (w, h) = a.dimensions();
        ensure!(
            (w as f64 / h as f64 - doc.slide_size.width / doc.slide_size.height).abs() < 0.002,
            "composite images must be full-page renders"
        );
        if w as u64 * h as u64 > 40_000_000 {
            pages.push(json!({"page":page+1,"status":"unknown","reason":"composite pixel budget exceeded"}));
            continue;
        }
        let mask = a
            .pixels()
            .zip(b.pixels())
            .map(|(a, b)| a.0.iter().zip(b.0).any(|(a, b)| a.abs_diff(b) > 16))
            .collect();
        let parts = components(mask, w as usize, h as usize);
        let mapping = mappings(doc, page, (w, h))?;
        let mut assigned = vec![Vec::new(); mapping.len()];
        let mut ambiguous = vec![0; mapping.len()];
        let mut unassigned = Vec::new();
        let mut ambiguous_count = 0;
        for (index, part) in parts.iter().enumerate() {
            let owners: Vec<_> = mapping
                .iter()
                .enumerate()
                .filter(|(_, m)| {
                    if m.cells.is_empty() {
                        return overlap(part.bounds, m.frame);
                    }
                    m.cells.iter().any(|q| touches(part, *q))
                })
                .map(|(i, _)| i)
                .collect();
            if owners.len() == 1 && !mapping[owners[0]].cells.is_empty() {
                assigned[owners[0]].push(index);
            } else if owners.is_empty() {
                unassigned.push(index);
            } else {
                ambiguous_count += 1;
                for i in owners {
                    ambiguous[i] += 1;
                }
            }
        }
        for (i, m) in mapping.iter().enumerate() {
            let missing = m
                .cells
                .iter()
                .filter(|q| !assigned[i].iter().any(|j| touches(&parts[*j], **q)))
                .count();
            let area = m
                .cells
                .iter()
                .map(|q| bounds(*q))
                .reduce(union)
                .unwrap_or(m.frame);
            let nearby = [area[0] - 2., area[1] - 2., area[2] + 2., area[3] + 2.];
            let nearby_unassigned = unassigned.iter().any(|j| overlap(parts[*j].bounds, nearby));
            let mut reasons = Vec::new();
            if m.cells.is_empty() {
                reasons.push("character cells unavailable or transform unsupported");
            }
            if assigned[i].is_empty() {
                reasons.push("no uniquely attributable visible ink above threshold");
            }
            if missing > 0 {
                reasons.push("character cells have missing, hidden or low-contrast ink");
            }
            if ambiguous[i] > 0 {
                reasons.push("visible components also intersect another text object");
            }
            if nearby_unassigned {
                reasons.push("unassigned ink touches this attribution boundary");
            }
            objects.push(json!({"page":page+1,"object_id":m.id,"shape_id":m.shape,
                "status":if reasons.is_empty(){"attributed"}else{"unknown"},"reasons":reasons,
                "character_cells":m.cells.len(),"missing_character_cells":missing,"ambiguous_components":ambiguous[i],
                "assigned_components":assigned[i].len(),"pixels":assigned[i].iter().map(|j|parts[*j].pixels.len()).sum::<usize>(),
                "ink_bounds_render_px":assigned[i].iter().map(|j|parts[*j].bounds).reduce(union),
                "quality_approved":false}));
        }
        pages.push(json!({"page":page+1,"status":"measured","full":full,"blank":blank,
            "components":parts.len(),"ambiguous_components":ambiguous_count,
            "unassigned_components":unassigned.len(),"unassigned_pixels":unassigned.iter().map(|i|parts[*i].pixels.len()).sum::<usize>()}));
    }
    Ok(
        json!({"algorithm":"composite-character-attribution-v1","threshold":16,"objects":objects,"pages":pages,
        "quality_approved":false,"basis":"actual final render minus its derived textless PPTX render; connected components matched to reopened native character cells",
        "limitations":["Missing signal can mean occlusion, low contrast or uncertain mapping; it is not proof of missing text.",
            "Attribution is not semantic ownership, accurate copy, collision-free placement, or visual approval.",
            "Vertical/body-rotated text and unsupported containers remain unknown. Character geometry comes from DeckCraft, not an independent Office renderer."]}),
    )
}
