use crate::{
    Context,
    spec::{Spec, Text},
    string,
};
use anyhow::{Context as _, Result, ensure};
use deckcraft_engine::{
    Session,
    cmd::file,
    model::{self, ColorRef, Fill, Presentation, Shape, ShapeId, Slide, SlideId, TextBody, Xfrm},
};
use model::text::{Align, Anchor, AutoFit, BodyProps, Bullet, RunProps, Spacing};
use serde_json::{Value, json};
use std::{path::Path, sync::Arc};

pub fn open(ctx: &Context, path: &Path) -> Result<Presentation> {
    open_bytes(path, &ctx.read(path)?)
}
pub(crate) fn open_bytes(path: &Path, bytes: &[u8]) -> Result<Presentation> {
    ensure!(
        bytes.len() as u64 <= crate::fs::MAX_FILE_BYTES,
        "file exceeds 256 MiB"
    );
    let (doc, damage) = file::open_checked(&path.to_string_lossy(), bytes)?;
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
        // All inherited regions normally come from one high-resolution reference.
        let mut reference_image = None;
        for layer in &p.artwork_layers {
            let bytes = if let Some(crop) = &layer.source_crop {
                if reference_image.as_ref().map(|(binding, _)| *binding) != Some(&layer.image) {
                    let image = crate::review::load_image(ctx, &ctx.verify(&layer.image)?)?;
                    reference_image = Some((&layer.image, image));
                }
                let (_, image) = reference_image
                    .as_ref()
                    .context("missing bound artwork image")?;
                crop.extract(image)?
            } else {
                ctx.read(ctx.verify(&layer.image)?)?
            };
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
    let (mut spec, mut sources) =
        crate::spec::load_with_sources(ctx, &spec_path, r["page"].as_str())?;
    ensure!(
        r["publish_candidate"] != true || (r["action"] == "cycle" && spec.pages.len() == 1),
        "publish_candidate requires a one-page cycle"
    );
    let freeze_inputs = r["freeze_inputs"] == true
        || (spec.pages.len() == 1
            && crate::page_assignments::should_snapshot_build(
                ctx,
                &spec.pages[0].id,
                &spec_path,
                r["publish_candidate"] == true,
            )?);
    if r["publish_candidate"] == true || freeze_inputs {
        crate::page_canvas::check(ctx, spec.canvas.physical_emu())?;
    }
    crate::page_gate::require_ready(ctx, spec.pages.len(), Some((&spec, &sources)))?;
    let out = ctx.path(string(r, "output")?)?;
    ensure!(!out.exists(), "use a fresh attempt directory");
    if freeze_inputs {
        ensure!(
            spec.pages.len() == 1,
            "freeze_inputs requires one page owned by the calling maker"
        );
        let plan = crate::page_gate::plan(ctx)?;
        let row = crate::page_gate::maker_row(ctx, &plan, &spec.pages[0].id)?;
        if crate::repair_gate::guarded(ctx)? {
            crate::page_gate::admit_maker_change(ctx, &plan, row)?;
        }
        sources = crate::page_snapshot::freeze(ctx, &spec)?;
    }
    let ocr_placement = crate::ocr_placement::apply(ctx, &mut spec)?;
    let doc = from_spec(ctx, &spec)?;
    let full = out.join("editable.pptx");
    let full_bytes = file::save_bytes(&doc, "pptx")?;
    let full_doc = open_bytes(&full, &full_bytes)?;
    let missing = crate::editable_copy::draft(ctx, &spec.pages, &full_doc)?;
    if r["publish_candidate"] == true {
        crate::editable_copy::require_complete(&spec.pages[0].id, &missing)?;
    }
    let render_width = (r["action"] == "cycle")
        .then(|| cycle_width(ctx, r, &spec, &full_doc))
        .transpose()?;
    ctx.write_new(&full, &full_bytes)?;
    drop(full_bytes);
    let blank = ctx.write_new(
        out.join("blank.pptx"),
        &file::save_bytes(&blank(&doc), "pptx")?,
    )?;
    let resolved = ctx.write_json(
        out.join("resolved-spec.json"),
        &serde_json::to_value(&spec)?,
    )?;
    let blank_doc = open(ctx, &blank)?;
    ensure!(
        full_doc.slides.len() == spec.pages.len() && blank_doc.slides.len() == spec.pages.len(),
        "round-trip lost slides"
    );
    let mut result = json!({"schema_version":1,"engine_revision":crate::ENGINE_REV,"runtime":"rust","spec":ctx.binding(resolved)?,"editable":ctx.binding(&full)?,"blank":ctx.binding(&blank)?,"pages":spec.pages.iter().map(|p|&p.id).collect::<Vec<_>>(),"quality_approved":false});
    result["sources"] = json!(sources);
    if !missing.is_empty() {
        result["missing_editable_copy"] = json!(missing);
    }
    let lines = crate::source_lines::inventory(ctx, &spec)?;
    if !lines.is_empty() {
        result["source_lines"] = json!(lines);
    }
    if !ocr_placement.is_empty() {
        result["ocr_placement"] = json!(ocr_placement);
    }
    if let Some(width) = render_width {
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
    let publication = if r["publish_candidate"] == true {
        crate::page_gate::publish(ctx, &json!({"page":spec.pages[0].id,"cycle":receipt}))?
    } else {
        Value::Null
    };
    let mut handoff = if r["action"] == "cycle"
        && missing.is_empty()
        && publication["ready_for_independent_review"] != false
    {
        json!({"kind":"review_data_not_a_tool","cycle":ctx.binding(&receipt)?,"editable":result["editable"],
            "renders":result["renders"]["editable"]["pages"],"ready_for_independent_review":true,
            "quality_approved":false,"notification_sent":false,
            "next":"This is data in the cycle response, not a tool named peer_review_handoff. Send this first readable cycle through existing peer messaging now; keep issue IDs and compare repairs on the same criteria. No notification has been sent automatically."})
    } else {
        Value::Null
    };
    if publication["candidate"].is_object() {
        handoff["candidate"] = publication["candidate"].clone();
        handoff["candidate_id"] = publication["candidate_id"].clone();
        handoff["page_id"] = json!(spec.pages[0].id);
        if let Some(template) = publication["handoff"].get("review_request_template") {
            handoff["review_request_template"] = template.clone();
        }
    }
    Ok(
        json!({"receipt":ctx.binding(receipt)?,"editable":result["editable"],"blank":result["blank"],"renders":result["renders"],"comparisons":result["comparisons"],"inspection":result["inspection"],"native_qa":result["native_qa"],"page_publication":publication,"missing_editable_copy":missing,"peer_review_handoff":handoff,"quality_approved":false}),
    )
}
fn cycle_width(ctx: &Context, r: &Value, spec: &Spec, doc: &Presentation) -> Result<u32> {
    if r.get("width").is_some() || !ctx.path(crate::page_gate::PLAN)?.exists() {
        return width(r);
    }
    let [w, h] = [spec.canvas.width_px, spec.canvas.height_px].map(f64::round);
    ensure!(
        (1.0..=8192.0).contains(&w) && (1.0..=8192.0).contains(&h),
        "page-first canvas {w}x{h} is outside the native render range (1..8192 pixels per side); specify a supported preview width"
    );
    let requested_scale = w / doc.slide_size.width;
    let scale = file::image_scale(doc, requested_scale);
    ensure!(
        requested_scale * doc.slide_size.height <= 8192.
            && (doc.slide_size.width * scale).round() == w
            && (doc.slide_size.height * scale).round() == h,
        "page-first canvas {w}x{h} cannot be rendered at the declared physical slide size; check the canvas or specify a supported preview width"
    );
    Ok(w as u32)
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
    render_bound(ctx, doc, ctx.binding(source)?, out, w)
}

fn render_bound(
    ctx: &Context,
    doc: &Presentation,
    input: Value,
    out: &Path,
    w: u32,
) -> Result<Value> {
    if std::env::var("OCTOS_PPTX_REQUIRE_CLEAN_RENDER").as_deref() == Ok("1") {
        let probe = crate::renderer_probe::check(ctx, &json!({}))?;
        ensure!(
            probe["passed"] == true,
            "this deployment failed its scaled-picture renderer check; render the same PPTX with the approved DeckCraft helper"
        );
    }
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
        let png = ctx.path(out.join(format!("slide-{:03}.png", i + 1)))?;
        let mut row = if png.exists() {
            // An interrupted render may have published a prefix without a receipt.
            // Only adopt those files after reproducing their exact bytes.
            let found = ctx.binding(png)?;
            ensure!(
                found["sha256"] == crate::fs::digest(&bytes),
                "existing preview differs at slide {}; use a fresh render directory",
                i + 1
            );
            found
        } else {
            ctx.binding(ctx.write_new(png, &bytes)?)?
        };
        row["width"] = json!(width);
        row["height"] = json!(height);
        row["page"] = json!(i + 1);
        pages.push(row);
    }
    ctx.verify(&input)?;
    let result = json!({"input":input,"engine_revision":crate::ENGINE_REV,"requested_width":w,"pages":pages,"fonts":deckcraft_engine::fonts::inspect_fonts(doc),"quality_approved":false});
    ctx.write_json(out.join("render.json"), &result)?;
    Ok(result)
}
pub(crate) fn lock_render(ctx: &Context, out: &Path) -> Result<crate::fs::WorkspaceLock> {
    let key = crate::fs::digest(out.strip_prefix(&ctx.root)?.as_os_str().as_encoded_bytes());
    ctx.lock_file(&format!(".octos/pptx-render-locks/{key}.lock"))
}
pub fn render(ctx: &Context, r: &Value) -> Result<Value> {
    let src = ctx.path(string(r, "input")?)?;
    let out = ctx.path(string(r, "output")?)?;
    let w = width(r)?;
    let _lock = lock_render(ctx, &out)?;
    let bytes = ctx.read(&src)?;
    let input = json!({"path":src.strip_prefix(&ctx.root)?.to_string_lossy(),"sha256":crate::fs::digest(&bytes)});
    let doc = open_bytes(&src, &bytes)?;
    drop(bytes);
    ensure!(
        doc.slides.len() <= 1 || !crate::review_routes::per_page(ctx)?,
        "per_page renders the individual candidate before compilation"
    );
    let receipt = ctx.path(out.join("render.json"))?;
    if !receipt.exists() {
        return render_bound(ctx, &doc, input, &out, w);
    }
    let result = ctx.json(receipt)?;
    ensure!(
        result["input"] == input
            && result["engine_revision"] == crate::ENGINE_REV
            && result.get("requested_width").is_none_or(|width| width == w)
            && result["fonts"] == deckcraft_engine::fonts::inspect_fonts(&doc)
            && result["quality_approved"] == false,
        "existing render has different input, renderer, fonts or width; use a fresh render directory"
    );
    let scale = file::image_scale(&doc, f64::from(w) / doc.slide_size.width);
    let expected = [doc.slide_size.width, doc.slide_size.height]
        .map(|side| (side * scale).round().max(1.0) as u32);
    let pages = result["pages"]
        .as_array()
        .context("invalid render receipt")?;
    ensure!(pages.len() == doc.slides.len(), "incomplete render receipt");
    for (index, page) in pages.iter().enumerate() {
        let n = index + 1;
        let path = ctx.verify(page)?;
        let (width, height) = image::image_dimensions(&path)?;
        ensure!(
            path == ctx.path(out.join(format!("slide-{n:03}.png")))?
                && page["page"] == n
                && page["width"] == width
                && page["height"] == height
                && [width, height] == expected,
            "existing preview at slide {n} differs from the requested render"
        );
    }
    ctx.verify(&input)?;
    Ok(result)
}
pub fn commands(r: &Value) -> Result<Value> {
    let filter = r["filter"].as_str().unwrap_or("");
    Ok(json!(
        deckcraft_engine::command_specs()
            .iter()
            .filter(|c| c.id.contains(filter))
            .map(|c| json!({"command":c.id,"description":c.params}))
            .chain(
                crate::repair_commands::COMMANDS
                    .iter()
                    .filter(|(id, _)| id.contains(filter))
                    .map(|(id, description)| json!({"command":id,"description":description}))
            )
            .collect::<Vec<_>>()
    ))
}

fn validate_guarded_text_edit(name: &str, params: &Value) -> Result<()> {
    if name == "text.set" {
        anyhow::bail!(
            "guarded_text_edits refuses text.set: it keeps only the first run's formatting"
        );
    }
    if name == "text.replace" {
        let target = params.get("at").and_then(Value::as_object).ok_or_else(|| {
            anyhow::anyhow!(
                "guarded_text_edits requires an explicit text.find match for text.replace"
            )
        })?;
        for field in ["slide", "shape", "para", "start", "end"] {
            ensure!(
                target.get(field).and_then(Value::as_u64).is_some(),
                "guarded_text_edits requires a slide text match with {field}"
            );
        }
        ensure!(
            matches!(
                (target.get("start").and_then(Value::as_u64), target.get("end").and_then(Value::as_u64)),
                (Some(start), Some(end)) if start < end
            ),
            "guarded_text_edits requires a non-empty text match"
        );
    }
    Ok(())
}

pub fn batch(ctx: &Context, r: &Value) -> Result<Value> {
    let mut s = Session::new();
    let mut input_binding = Value::Null;
    if let Some(input) = r["input"].as_str() {
        let p = ctx.path(input)?;
        let doc = open(ctx, &p)?;
        file::add_opened(&mut s, "input", None, doc);
        input_binding = ctx.binding(&p)?;
    }
    let cmds = r["commands"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("commands must be an array"))?;
    let guarded_text_edits = r
        .get("guarded_text_edits")
        .map(|value| {
            value
                .as_bool()
                .ok_or_else(|| anyhow::anyhow!("guarded_text_edits must be a boolean"))
        })
        .transpose()?
        .unwrap_or(false);
    ensure!(cmds.len() <= 10000, "too many commands");
    let roots = deckcraft_engine::file_access::AutomationRoots::new(
        std::slice::from_ref(&ctx.root),
        std::slice::from_ref(&ctx.root),
    )
    .map_err(anyhow::Error::msg)?;
    let mut results = Vec::new();
    deckcraft_engine::file_access::confine(roots.as_ref(), || -> Result<()> {
        for (position, cmd) in cmds.iter().enumerate() {
            let name = string(cmd, "command")?;
            if name == "text.fitSingleLine"
                && (position == 0 || cmds[position - 1]["command"] != name)
            {
                let group = &cmds[position..];
                let count = group.iter().take_while(|c| c["command"] == name).count();
                if count > 1 {
                    crate::repair_commands::preflight_fit_run(&s.doc()?.doc, &group[..count])?;
                }
            }
            // All file writes go through this wrapper's final no-clobber save.
            ensure!(
                (!name.starts_with("file.") || name == "file.new")
                    && !name.starts_with("app.")
                    && !name.starts_with("window."),
                "file/app commands are not available in batch"
            );
            let mut params = cmd.get("params").cloned().unwrap_or_else(|| json!({}));
            // The engine resolves reuse paths against the process cwd. Keep
            // this wrapper's workspace-relative path contract without changing
            // the process cwd, which would race concurrent page workers.
            if matches!(name, "slide.reuse" | "slide.reuseList")
                && let Some(path) = params.get("path").and_then(Value::as_str)
            {
                params["path"] = json!(ctx.path(path)?);
            }
            if guarded_text_edits {
                validate_guarded_text_edit(name, &params)?;
            }
            let value = if crate::repair_commands::COMMANDS
                .iter()
                .any(|(id, _)| *id == name)
            {
                crate::repair_commands::apply(ctx, &mut s, name, &params)?
            } else {
                s.execute(name, &params)?
            };
            results.push(value);
        }
        Ok(())
    })?;
    let mut result = json!({"results":results,"input":input_binding,
        "commands_sha256":crate::fs::digest(&serde_json::to_vec(cmds)?),"guarded_text_edits":guarded_text_edits,
        "quality_approved":false});
    if let Some(output) = r["output"].as_str() {
        ensure!(output.ends_with(".pptx"), "output must end in .pptx");
        crate::page_gate::require_ready(ctx, s.doc()?.doc.slides.len(), None)?;
        let p = ctx.write_new(output, &file::save_bytes(&s.doc()?.doc, "pptx")?)?;
        open(ctx, &p)?;
        result["output"] = ctx.binding(p)?;
    }
    Ok(result)
}

#[cfg(test)]
mod guarded_batch_tests {
    use super::*;

    #[test]
    fn should_require_a_specific_slide_text_match_for_guarded_replacements() {
        assert!(validate_guarded_text_edit("text.set", &json!({"text":"new"})).is_err());
        assert!(
            validate_guarded_text_edit("text.replace", &json!({"text":"old","with":"new"}))
                .is_err()
        );
        assert!(validate_guarded_text_edit("text.replace", &json!({"at":{"slide":0}})).is_err());
        assert!(
            validate_guarded_text_edit(
                "text.replace",
                &json!({"at":{
                    "slide":0,"shape":null,"para":0,"start":0,"end":3
                }})
            )
            .is_err()
        );
        assert!(
            validate_guarded_text_edit(
                "text.replace",
                &json!({"at":{
                    "slide":"bad","shape":1,"para":0,"start":0,"end":3
                }})
            )
            .is_err()
        );
        assert!(
            validate_guarded_text_edit(
                "text.replace",
                &json!({"at":{
                    "slide":0,"shape":1,"para":0,"start":3,"end":3
                }})
            )
            .is_err()
        );
        assert!(
            validate_guarded_text_edit(
                "text.replace",
                &json!({"at":{
                    "slide":0,"shape":1,"para":0,"start":0,"end":3
                }})
            )
            .is_ok()
        );
    }
}
