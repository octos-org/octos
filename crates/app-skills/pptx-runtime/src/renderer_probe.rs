use anyhow::{Context as _, Result, ensure};
use deckcraft_engine::{
    cmd::file,
    model::{self, Presentation, Shape, ShapeId, Xfrm},
};
use serde_json::{Value, json};
use std::{io::Cursor, path::Path, sync::Arc};

/// Exercise a scaled raster picture, including PPTX round-trip and PNG export.
/// A native shape alone does not exercise the picture sampling path.
pub(crate) fn check(ctx: &crate::Context, request: &Value) -> Result<Value> {
    let color = [42u8, 83, 129];
    let picture = image::RgbImage::from_pixel(117, 65, image::Rgb(color));
    let mut png = Cursor::new(Vec::new());
    picture.write_to(&mut png, image::ImageFormat::Png)?;
    let mut doc = Presentation {
        slide_size: model::Size::new(160., 90.),
        ..Default::default()
    };
    let media = doc
        .add_media("probe", "image/png", png.into_inner())
        .context("could not allocate probe picture")?;
    let picture = Shape {
        id: ShapeId(doc.alloc_id().context("could not allocate probe shape")?),
        xfrm: Some(Xfrm::new(0., 0., 160., 90.)),
        kind: model::ShapeKind::Picture {
            fill: model::style::PictureFill {
                media,
                ..Default::default()
            },
        },
        ..Default::default()
    };
    let slide = Arc::make_mut(doc.slides.first_mut().context("missing probe slide")?);
    slide.show_master_shapes = false;
    slide.shapes = vec![picture];
    let bytes = file::save_bytes(&doc, "pptx")?;
    let fixture_hash = crate::fs::digest(&bytes);
    if let Some(path) = request["probe_fixture_output"].as_str() {
        ctx.write_new(path, &bytes)?;
    }
    let (render, w, h) = if let Some(receipt) = request["probe_render_receipt"].as_str() {
        let receipt = ctx.json(receipt)?;
        let pages = receipt["pages"].as_array().context("missing probe pages")?;
        ensure!(
            receipt["engine_revision"] == crate::ENGINE_REV
                && pages.len() == 1
                && receipt["input"]["sha256"] == fixture_hash,
            "receipt must bind the same renderer probe and engine"
        );
        ctx.verify(&receipt["input"])?;
        let path = ctx.verify(&pages[0])?;
        ensure!(
            image::image_dimensions(&path)? == (320, 180),
            "render the probe at width 320"
        );
        let png = ctx.read(path)?;
        (png, 320, 180)
    } else {
        let reopened = crate::deck::open_bytes(Path::new("renderer-probe.pptx"), &bytes)?;
        file::render_png(&reopened, 0, 2., false)
    };
    let pixels = image::load_from_memory(&render)?.into_rgb8();
    ensure!(
        (w, h) == (320, 180) && pixels.dimensions() == (w, h),
        "wrong probe dimensions"
    );
    let mut max_error = 0;
    let mut incorrect_samples = 0;
    for y in 8..h - 8 {
        for x in [w / 4, w / 2, 3 * w / 4] {
            let diff = pixels
                .get_pixel(x, y)
                .0
                .into_iter()
                .zip(color)
                .map(|(a, b)| a.abs_diff(b))
                .max()
                .unwrap_or_default();
            max_error = max_error.max(diff);
            incorrect_samples += u32::from(diff > 2);
        }
    }
    Ok(
        json!({"passed":incorrect_samples == 0, "check":"scaled_opaque_picture",
        "samples":3*(h-16), "incorrect_samples":incorrect_samples, "maximum_channel_error":max_error,
        "tolerance":2, "engine_revision":crate::ENGINE_REV, "fixture_sha256":fixture_hash}),
    )
}
