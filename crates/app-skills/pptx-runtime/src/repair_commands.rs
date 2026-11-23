//! Page-local repair intents executed inside the normal no-clobber batch transaction.
use crate::{Context, deck, fit};
use anyhow::{Context as _, Result, ensure};
use deckcraft_engine::{
    Session, media_refs,
    model::{Presentation, Shape, ShapeId, ShapeKind, resolve},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::Arc,
};

pub const COMMANDS: &[(&str, &str)] = &[
    (
        "shape.matchStroke",
        "{id,reference}: match another shape's explicit stroke on this page",
    ),
    (
        "text.fitSingleLine",
        "{id,box:[x,y,w,h],min_pt}: fit uniform-size native text in a point rectangle, preserving all words and run styles",
    ),
    (
        "shape.replaceWithLayout",
        "{id,path,prior_copies?}: replace a full-page picture with a same-canvas layout. Optional source-ID to destination-ID map reuses verified prior copies.",
    ),
];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Stroke {
    id: u32,
    reference: u32,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Fit {
    id: u32,
    r#box: [f64; 4],
    min_pt: f64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Layout {
    id: u32,
    path: String,
    #[serde(default)]
    prior_copies: BTreeMap<u32, u32>,
}

fn index(doc: &Presentation, id: u32) -> Result<usize> {
    doc.slides[0]
        .shapes
        .iter()
        .position(|s| s.id == ShapeId(id))
        .with_context(|| format!("no top-level shape with ID {id}"))
}

fn uniform_text_size(shape: &Shape) -> Result<f64> {
    let body = shape.text.as_ref().context("target has no native text")?;
    ensure!(!body.text().trim().is_empty(), "empty native text");
    let mut sizes = Vec::new();
    for para in &body.paragraphs {
        for run in &para.runs {
            sizes.push(
                run.props
                    .size
                    .or(para.def_props.size)
                    .context("text size must be explicit")?,
            );
        }
    }
    let original = *sizes.first().context("empty text runs")?;
    ensure!(
        sizes.iter().all(|v| (*v - original).abs() < 0.001),
        "mixed-size text needs an explicit typography decision"
    );
    Ok(original)
}

fn check_floor(shape: &Shape, current: f64, minimum: f64) -> Result<()> {
    ensure!(
        minimum > 0. && minimum <= current && current <= 1000.,
        "text.fitSingleLine: {} (shape {}) has font size {current} pt and requested minimum {minimum} pt; require 0 < minimum <= current <= 1000 pt",
        shape.name,
        shape.id.0
    );
    Ok(())
}

/// Collect independent size-floor conflicts before laying out any text in this
/// run. Other command types can change shape IDs or fonts, so never look past them.
pub(crate) fn preflight_fit_run(doc: &Presentation, commands: &[Value]) -> Result<()> {
    ensure!(doc.slides.len() == 1, "page-local repair needs one slide");
    let mut conflicts = Vec::new();
    for command in commands {
        let request: Fit = serde_json::from_value(command["params"].clone())?;
        let shape = &doc.slides[0].shapes[index(doc, request.id)?];
        let size = uniform_text_size(shape)?;
        if let Err(error) = check_floor(shape, size, request.min_pt) {
            conflicts.push(error.to_string());
        }
    }
    ensure!(conflicts.is_empty(), "{}", conflicts.join("\n"));
    Ok(())
}

fn single_line(doc: &Presentation, req: Fit) -> Result<(usize, Shape, Value)> {
    let i = index(doc, req.id)?;
    let mut shape = doc.slides[0].shapes[i].clone();
    let ctx = resolve::Ctx::for_slide(doc, &doc.slides[0]).context("missing slide layout")?;
    let [x, y, w, h] = req.r#box;
    ensure!(
        x >= 0.
            && y >= 0.
            && w > 0.
            && h > 0.
            && x + w <= doc.slide_size.width + 0.001
            && y + h <= doc.slide_size.height + 0.001,
        "text rectangle outside the slide"
    );
    ensure!(
        !shape.locked && matches!(shape.kind, ShapeKind::Shape),
        "target must be an unlocked native text shape"
    );
    let original = uniform_text_size(&shape)?;
    check_floor(&shape, original, req.min_pt)?;
    let xf = shape
        .xfrm
        .as_mut()
        .context("text needs an explicit frame")?;
    xf.x = x;
    xf.y = y;
    xf.w = w;
    xf.h = h;
    let geometry = deckcraft_engine::render::shape_geometry(&shape, w, h);
    let fits = |shape: &Shape| -> bool {
        let body = shape.text.as_ref().expect("validated text");
        let layout = deckcraft_text::layout(
            &ctx,
            shape,
            body,
            &deckcraft_text::Opts {
                rect: geometry.text_rect,
                fields: &deckcraft_text::NoFields,
                prompt_color: None,
                no_shrink: true,
            },
        );
        !layout.overflow
            && layout.lines.len() == 1
            && layout.forced_line_breaks(body) == 0
            && layout.content_width <= layout.inner.width() + 0.01
            && layout.content_height <= layout.inner.height() + 0.01
    };
    // Search the same hundredth-point grid that PowerPoint can serialize.
    let mut low = (req.min_pt * 100. - 1e-7).ceil() as i64;
    let mut high = (original * 100. + 1e-7).floor() as i64;
    ensure!(
        low <= high,
        "size interval is smaller than PowerPoint precision"
    );
    fit::set_size(&mut shape, low as f64 / 100.);
    ensure!(
        fits(&shape),
        "text {} does not fit one line at its allowed minimum",
        req.id
    );
    while low < high {
        let mid = (low + high + 1) / 2;
        fit::set_size(&mut shape, mid as f64 / 100.);
        if fits(&shape) {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    let size = low as f64 / 100.;
    fit::set_size(&mut shape, size);
    Ok((
        i,
        shape,
        json!({"id":req.id,"original_pt":original,"size_pt":size,"lines":1,"box":req.r#box}),
    ))
}

fn replace(ctx: &Context, doc: &mut Presentation, req: Layout) -> Result<Value> {
    let idx = index(doc, req.id)?;
    let slide = &doc.slides[0];
    let old = &slide.shapes[idx];
    ensure!(
        !old.locked
            && matches!(old.kind, ShapeKind::Picture { .. })
            && old.click.is_none()
            && old.hover.is_none(),
        "replace target must be an unlocked static picture"
    );
    let xf = old.xfrm.context("missing picture frame")?;
    ensure!(
        xf.x.abs() < 0.01
            && xf.y.abs() < 0.01
            && (xf.w - doc.slide_size.width).abs() < 0.01
            && (xf.h - doc.slide_size.height).abs() < 0.01
            && xf.rot.abs() < 0.01,
        "replace target must cover the full slide"
    );
    ensure!(
        slide.animations.is_empty(),
        "animated slides need an explicit replacement plan"
    );
    let mut referenced = false;
    deckcraft_engine::model::walk(&slide.shapes, &mut |s, _| {
        if let ShapeKind::Connector { start, end } = &s.kind {
            referenced |= [start, end]
                .iter()
                .any(|p| p.is_some_and(|(id, _)| id == old.id));
        }
    });
    ensure!(!referenced, "a connector references the old picture");
    let source = deck::open(ctx, &ctx.path(&req.path)?)?;
    ensure!(
        source.slides.len() == 1 && source.slide_size == doc.slide_size,
        "layout must have one slide of the same size"
    );
    let donor = &source.slides[0];
    let source_ctx = resolve::Ctx::for_slide(&source, donor).context("missing source layout")?;
    let dest_ctx = resolve::Ctx::for_slide(doc, slide).context("missing destination layout")?;
    ensure!(
        source_ctx.master.theme == dest_ctx.master.theme && donor.color_map == slide.color_map,
        "source and destination themes must match"
    );
    ensure!(
        donor.animations.is_empty()
            && donor.shapes.iter().all(|s| matches!(
                s.kind,
                ShapeKind::Picture { .. } | ShapeKind::Shape
            ) && s.ph.is_none()
                && s.xfrm.is_some()
                && s.text.as_ref().is_none_or(|v| v.text().is_empty())
                && s.click.is_none()
                && s.hover.is_none()),
        "supply a static layout asset containing only positioned pictures and empty native shapes"
    );
    ensure!(
        donor
            .shapes
            .iter()
            .any(|s| matches!(s.kind, ShapeKind::Shape)),
        "layout has no editable geometry"
    );
    let mut layers = donor.shapes.clone();
    let mut media = HashMap::new();
    for id in media_refs::shapes_media(&layers) {
        let item = source.media(id).context("layout media missing")?;
        ensure!(item.link.is_none(), "external layout media is unsupported");
        let duplicate = doc
            .media
            .iter()
            .find(|m| {
                m.content_type == item.content_type && m.link.is_none() && m.data == item.data
            })
            .map(|m| m.id);
        let new = match duplicate {
            Some(id) => id,
            None => doc
                .add_media(&item.name, &item.content_type, item.data.as_ref().clone())
                .context("media ID capacity exhausted")?,
        };
        media.insert(id, new);
    }
    media_refs::visit_mut::shapes(&mut layers, &mut |m| *m = media[m]);
    let mut inserted = Vec::new();
    let mut reused = HashSet::new();
    for sh in &mut layers {
        let id = if let Some(&id) = req.prior_copies.get(&sh.id.0) {
            ensure!(
                id != req.id && reused.insert(id),
                "conflicting prior copy IDs"
            );
            let existing = &doc.slides[0].shapes[index(doc, id)?];
            sh.id = ShapeId(id);
            ensure!(
                existing == sh && !existing.locked,
                "prior copy {id} does not match the source shape"
            );
            id
        } else {
            doc.alloc_id().context("shape ID capacity exhausted")?
        };
        sh.id = ShapeId(id);
        inserted.push(id);
    }
    ensure!(
        reused.len() == req.prior_copies.len(),
        "unknown source IDs in prior_copies"
    );
    let shapes = &mut Arc::make_mut(&mut doc.slides[0]).shapes;
    shapes.retain(|s| !reused.contains(&s.id.0));
    let position = shapes
        .iter()
        .position(|s| s.id == ShapeId(req.id))
        .context("lost layout anchor")?;
    shapes.splice(position..=position, layers);
    Ok(
        json!({"removed":req.id,"layout_shape_ids":inserted,"reused_by_source_id":req.prior_copies,"z_index":position}),
    )
}

pub fn apply(ctx: &Context, session: &mut Session, name: &str, params: &Value) -> Result<Value> {
    let doc = Arc::make_mut(&mut session.doc_mut()?.doc);
    ensure!(
        doc.slides.len() == 1,
        "repair commands require an individual page PPTX"
    );
    match name {
        "shape.matchStroke" => {
            let r: Stroke = serde_json::from_value(params.clone())?;
            let (i, j) = (index(doc, r.id)?, index(doc, r.reference)?);
            let slide = Arc::make_mut(&mut doc.slides[0]);
            ensure!(!slide.shapes[i].locked, "stroke target is locked");
            let line = slide.shapes[j]
                .line
                .clone()
                .context("reference has no explicit stroke")?;
            ensure!(
                line.width.is_some() && line.fill.is_some(),
                "reference needs an explicit width and fill"
            );
            slide.shapes[i].line = Some(line);
            Ok(json!({"id":r.id,"reference":r.reference}))
        }
        "text.fitSingleLine" => {
            let (i, shape, result) = single_line(doc, serde_json::from_value(params.clone())?)?;
            Arc::make_mut(&mut doc.slides[0]).shapes[i] = shape;
            Ok(result)
        }
        "shape.replaceWithLayout" => replace(ctx, doc, serde_json::from_value(params.clone())?),
        _ => anyhow::bail!("unknown repair command {name}"),
    }
}
