use crate::{
    Context,
    spec::{Spec, Text},
    string,
};
use anyhow::{Result, ensure};
use deckcraft_engine::{
    Session,
    cmd::file,
    model::{self, ColorRef, Fill, Presentation, Shape, ShapeId, Slide, SlideId, TextBody, Xfrm},
};
use model::text::{Align, Anchor, AutoFit, BodyProps, Bullet, RunProps, Spacing};
use serde_json::{Value, json};
use std::{path::Path, sync::Arc};

pub fn open(ctx: &Context, path: &Path) -> Result<Presentation> {
    let bytes = ctx.read(path)?;
    let (doc, damage) = file::open_checked(&path.to_string_lossy(), &bytes)?;
    ensure!(
        damage.is_empty(),
        "input contains unreadable parts; refusing a partial import: {damage:?}"
    );
    ensure!(!doc.slides.is_empty(), "presentation has no slides");
    deckcraft_engine::fonts::install_embedded_fonts(&doc);
    Ok(doc)
}
fn alloc(doc: &mut Presentation) -> Result<u32> {
    doc.alloc_id()
        .ok_or_else(|| anyhow::anyhow!("document id capacity exhausted"))
}
pub fn text_shape(doc: &mut Presentation, spec: &Spec, t: &Text) -> Result<Shape> {
    let s = spec.style(t)?;
    let scale = spec.canvas.width_in * 72. / spec.canvas.width_px;
    let [x, y, w, h] = t.bounds.map(|v| v * scale);
    let mut xf = Xfrm::new(x, y, w, h);
    xf.rot = t.rotation;
    let mut body = TextBody::from_text(&t.text);
    let [l, top, r, b] = s.margins_px.map(|v| v * scale);
    body.body = BodyProps {
        inset_l: Some(l),
        inset_t: Some(top),
        inset_r: Some(r),
        inset_b: Some(b),
        anchor: Some(match s.anchor.as_str() {
            "middle" => Anchor::Middle,
            "bottom" => Anchor::Bottom,
            _ => Anchor::Top,
        }),
        wrap: Some(s.wrap),
        autofit: Some(AutoFit::None),
        ..Default::default()
    };
    let color = model::Rgba::from_hex(&s.color).ok_or_else(|| anyhow::anyhow!("invalid color"))?;
    let props = RunProps {
        font: Some(s.font.clone()),
        font_ea: Some(s.font.clone()),
        font_cs: Some(s.font),
        size: Some(s.size_pt),
        bold: Some(s.bold),
        italic: Some(s.italic),
        fill: Some(Fill::solid(ColorRef::rgb(color))),
        ..Default::default()
    };
    for p in &mut body.paragraphs {
        p.props.align = Some(match s.align.as_str() {
            "center" => Align::Center,
            "right" => Align::Right,
            "justify" => Align::Justify,
            _ => Align::Left,
        });
        p.props.bullet = Some(Bullet::None);
        p.props.line_spacing = Some(Spacing::Pct(s.line_spacing));
        p.props.space_before = Some(Spacing::Pts(s.space_before_pt));
        p.props.space_after = Some(Spacing::Pts(s.space_after_pt));
        p.def_props = props.clone();
        p.end_props = props.clone();
        for run in &mut p.runs {
            run.props = props.clone();
        }
    }
    Ok(Shape {
        id: ShapeId(alloc(doc)?),
        name: format!("octos:text:{}", t.id),
        xfrm: Some(xf),
        text_box: true,
        text: Some(body),
        fill: Some(Fill::None),
        line: Some(model::Line {
            fill: Some(Fill::None),
            ..Default::default()
        }),
        ..Default::default()
    })
}
fn from_spec(ctx: &Context, spec: &Spec) -> Result<Presentation> {
    let mut doc = Presentation::default();
    doc.slides.clear();
    doc.slide_size = model::Size::new(spec.canvas.width_in * 72., spec.canvas.height_in * 72.);
    let layout = doc
        .masters
        .iter()
        .flat_map(|m| m.layouts.iter())
        .find(|l| l.kind == model::LayoutType::Blank)
        .map(|l| l.id)
        .ok_or_else(|| anyhow::anyhow!("missing blank layout"))?;
    for p in &spec.pages {
        let path = ctx.verify(&p.background)?;
        let bytes = ctx.read(&path)?;
        let format = image::guess_format(&bytes)?;
        let (image_w, image_h) = image::ImageReader::new(std::io::Cursor::new(&bytes))
            .with_guessed_format()?
            .into_dimensions()?;
        ensure!(
            image_w > 0
                && image_h > 0
                && u64::from(image_w) * u64::from(image_h) <= 64 * 1024 * 1024,
            "background exceeds bounded image dimensions"
        );
        ensure!(
            matches!(
                format,
                image::ImageFormat::Png | image::ImageFormat::Jpeg | image::ImageFormat::WebP
            ),
            "background must be PNG/JPEG/WebP"
        );
        let media = doc
            .add_media(
                &format!("{}-background", p.id),
                format.to_mime_type(),
                bytes,
            )
            .ok_or_else(|| anyhow::anyhow!("media id exhausted"))?;
        let bg = Shape {
            id: ShapeId(alloc(&mut doc)?),
            name: "octos:artwork".into(),
            xfrm: Some(Xfrm::new(
                0.,
                0.,
                doc.slide_size.width,
                doc.slide_size.height,
            )),
            kind: model::ShapeKind::Picture {
                fill: model::style::PictureFill {
                    media,
                    ..Default::default()
                },
            },
            ..Default::default()
        };
        let mut slide = Slide {
            id: SlideId(alloc(&mut doc)?),
            layout,
            name: p.id.clone(),
            show_master_shapes: false,
            shapes: vec![bg],
            ..Default::default()
        };
        // The same artwork stack is retained in editable and derived-blank decks.
        // Authoring a local repair no longer requires post-build z-order surgery.
        let scale = spec.canvas.width_in * 72. / spec.canvas.width_px;
        for layer in &p.artwork_layers {
            let bytes = ctx.read(ctx.verify(&layer.image)?)?;
            let format = image::guess_format(&bytes)?;
            ensure!(
                matches!(
                    format,
                    image::ImageFormat::Png | image::ImageFormat::Jpeg | image::ImageFormat::WebP
                ),
                "artwork layer must be PNG/JPEG/WebP"
            );
            let (w, h) = image::ImageReader::new(std::io::Cursor::new(&bytes))
                .with_guessed_format()?
                .into_dimensions()?;
            ensure!(
                w > 0 && h > 0 && u64::from(w) * u64::from(h) <= 64 * 1024 * 1024,
                "artwork layer exceeds bounded image dimensions"
            );
            let media = doc
                .add_media(
                    &format!("{}-layer-{}", p.id, layer.id),
                    format.to_mime_type(),
                    bytes,
                )
                .ok_or_else(|| anyhow::anyhow!("media id exhausted"))?;
            let [x, y, w, h] = layer.bounds.map(|v| v * scale);
            let mut xfrm = Xfrm::new(x, y, w, h);
            xfrm.rot = layer.rotation;
            slide.shapes.push(Shape {
                id: ShapeId(alloc(&mut doc)?),
                name: format!("octos:artwork-layer:{}", layer.id),
                xfrm: Some(xfrm),
                kind: model::ShapeKind::Picture {
                    fill: model::style::PictureFill {
                        media,
                        ..Default::default()
                    },
                },
                ..Default::default()
            });
        }
        for t in &p.texts {
            slide.shapes.push(text_shape(&mut doc, spec, t)?);
        }
        if let Some(notes) = &p.notes {
            slide.notes = TextBody::from_text(notes)
        }
        doc.slides.push(Arc::new(slide));
    }
    Ok(doc)
}
pub fn blank(doc: &Presentation) -> Presentation {
    let mut b = doc.clone();
    for sl in &mut b.slides {
        Arc::make_mut(sl)
            .shapes
            .retain(|s| !s.name.starts_with("octos:text:"));
    }
    b
}
pub fn build(ctx: &Context, r: &Value) -> Result<Value> {
    let spec_path = ctx.path(string(r, "spec")?)?;
    let (spec, sources) = crate::spec::load_with_sources(ctx, &spec_path, r["page"].as_str())?;
    let out = ctx.path(string(r, "output")?)?;
    ensure!(!out.exists(), "use a fresh attempt directory");
    std::fs::create_dir_all(&out)?;
    let doc = from_spec(ctx, &spec)?;
    let full = ctx.write_new(out.join("editable.pptx"), &file::save_bytes(&doc, "pptx")?)?;
    let blank = ctx.write_new(
        out.join("blank.pptx"),
        &file::save_bytes(&blank(&doc), "pptx")?,
    )?;
    let resolved = ctx.write_json(
        out.join("resolved-spec.json"),
        &serde_json::to_value(&spec)?,
    )?;
    let full_doc = open(ctx, &full)?;
    let blank_doc = open(ctx, &blank)?;
    ensure!(
        full_doc.slides.len() == spec.pages.len() && blank_doc.slides.len() == spec.pages.len(),
        "round-trip lost slides"
    );
    let mut result = json!({"schema_version":1,"engine_revision":crate::ENGINE_REV,"runtime":"rust","spec":ctx.binding(resolved)?,"editable":ctx.binding(&full)?,"blank":ctx.binding(&blank)?,"pages":spec.pages.iter().map(|p|&p.id).collect::<Vec<_>>(),"quality_approved":false});
    result["sources"] = json!(sources);
    if r["action"] == "cycle" {
        let width = width(r)?;
        result["renders"] = json!({"editable":render_to(ctx,&full_doc,&full,&out.join("rendered"),width)?,"blank":render_to(ctx,&blank_doc,&blank,&out.join("blank-rendered"),width)?});
        let draft = ctx.write_json(out.join("render-cycle.json"), &result)?;
        result["inspection"] = crate::review::inspect(
            ctx,
            &json!({"cycle":draft,"output":out.join("inspection.json")}),
        )?;
        let inspection = ctx.json(ctx.verify(&result["inspection"]["report"])?)?;
        let qa = crate::qa::assess(ctx, &result, &spec, &inspection, &Value::Null)?;
        let qa_path = ctx.write_json(out.join("native-qa.json"), &qa)?;
        result["native_qa"] = json!({"report":ctx.binding(qa_path)?,"status":qa["status"],
            "failed":qa["failed"],"unknown":qa["unknown"],"diagnosis":qa["diagnosis"],"quality_approved":false});
        result["comparisons"] = crate::comparison_review::generate(
            ctx,
            &result,
            &spec,
            &out.join("comparisons"),
            r["reuse_comparisons"].as_str(),
        )?;
    }
    let receipt = ctx.write_json(out.join("cycle.json"), &result)?;
    let handoff = if r["action"] == "cycle" {
        json!({"kind":"review_data_not_a_tool","cycle":ctx.binding(&receipt)?,"editable":result["editable"],
            "renders":result["renders"]["editable"]["pages"],"ready_for_independent_review":true,
            "quality_approved":false,"notification_sent":false,
            "next":"This is data in the cycle response, not a tool named peer_review_handoff. Send this first readable cycle through existing peer messaging now; keep issue IDs and compare repairs on the same criteria. No notification has been sent automatically."})
    } else {
        Value::Null
    };
    Ok(
        json!({"receipt":ctx.binding(receipt)?,"editable":result["editable"],"blank":result["blank"],"renders":result["renders"],"comparisons":result["comparisons"],"inspection":result["inspection"],"native_qa":result["native_qa"],"peer_review_handoff":handoff,"quality_approved":false}),
    )
}
pub fn width(r: &Value) -> Result<u32> {
    let w = r
        .get("width")
        .map(|v| {
            v.as_u64()
                .ok_or_else(|| anyhow::anyhow!("width must be an integer"))
        })
        .transpose()?
        .unwrap_or(1920);
    ensure!((64..=8192).contains(&w), "width must be 64..8192");
    Ok(w as u32)
}
pub fn render_to(
    ctx: &Context,
    doc: &Presentation,
    source: &Path,
    out: &Path,
    w: u32,
) -> Result<Value> {
    let scale = f64::from(w) / doc.slide_size.width;
    ensure!(
        doc.slide_size.width > 0.
            && doc.slide_size.height > 0.
            && scale.is_finite()
            && scale * doc.slide_size.height <= 8192.,
        "invalid render dimensions"
    );
    let mut pages = Vec::new();
    for i in 0..doc.slides.len() {
        let (bytes, width, height) = file::render_png(doc, i, scale, false);
        ensure!(
            !bytes.is_empty(),
            "renderer returned no pixels for slide {}",
            i + 1
        );
        let p = ctx.write_new(out.join(format!("slide-{:03}.png", i + 1)), &bytes)?;
        let mut row = ctx.binding(p)?;
        row["width"] = json!(width);
        row["height"] = json!(height);
        row["page"] = json!(i + 1);
        pages.push(row);
    }
    let result = json!({"input":ctx.binding(source)?,"engine_revision":crate::ENGINE_REV,"pages":pages,"fonts":deckcraft_engine::fonts::inspect_fonts(doc),"quality_approved":false});
    ctx.write_json(out.join("render.json"), &result)?;
    Ok(result)
}
pub fn render(ctx: &Context, r: &Value) -> Result<Value> {
    let src = ctx.path(string(r, "input")?)?;
    let doc = open(ctx, &src)?;
    render_to(ctx, &doc, &src, &ctx.path(string(r, "output")?)?, width(r)?)
}
pub fn commands(r: &Value) -> Result<Value> {
    let filter = r["filter"].as_str().unwrap_or("");
    Ok(json!(
        deckcraft_engine::command_specs()
            .iter()
            .filter(|c| c.id.contains(filter))
            .map(|c| json!({"command":c.id,"description":c.params}))
            .collect::<Vec<_>>()
    ))
}
pub fn batch(ctx: &Context, r: &Value) -> Result<Value> {
    let mut s = Session::new();
    if let Some(input) = r["input"].as_str() {
        let p = ctx.path(input)?;
        let doc = open(ctx, &p)?;
        file::add_opened(&mut s, "input", None, doc);
    }
    let cmds = r["commands"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("commands must be an array"))?;
    ensure!(cmds.len() <= 10000, "too many commands");
    let roots = deckcraft_engine::file_access::AutomationRoots::new(
        std::slice::from_ref(&ctx.root),
        std::slice::from_ref(&ctx.root),
    )
    .map_err(anyhow::Error::msg)?;
    let mut results = Vec::new();
    deckcraft_engine::file_access::confine(roots.as_ref(), || -> Result<()> {
        for cmd in cmds {
            let name = string(cmd, "command")?;
            // All file writes go through this wrapper's final no-clobber save.
            ensure!(
                (!name.starts_with("file.") || name == "file.new")
                    && !name.starts_with("app.")
                    && !name.starts_with("window."),
                "file/app commands are not available in batch"
            );
            results.push(s.execute(name, cmd.get("params").unwrap_or(&json!({})))?);
        }
        Ok(())
    })?;
    let mut result = json!({"results":results,"quality_approved":false});
    if let Some(output) = r["output"].as_str() {
        ensure!(output.ends_with(".pptx"), "output must end in .pptx");
        let p = ctx.write_new(output, &file::save_bytes(&s.doc()?.doc, "pptx")?)?;
        open(ctx, &p)?;
        result["output"] = ctx.binding(p)?;
    }
    Ok(result)
}
