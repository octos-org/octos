//! Freeze a single PowerPoint canvas before independently built pages fan out.
use crate::{Context, review_routes, spec::Canvas};
use anyhow::{Context as _, Result, ensure};
use serde_json::Value;

fn validated(value: &Value) -> Result<Canvas> {
    let canvas: Canvas =
        serde_json::from_value(value.clone()).context("invalid shared page canvas")?;
    canvas.validate()?;
    Ok(canvas)
}

pub(crate) fn freeze(ctx: &Context, roster: &Value) -> Result<Option<Canvas>> {
    if let Some(value) = roster.get("canvas") {
        return validated(value).map(Some);
    }
    let row = &roster["pages"][0];
    if let Some(path) = row["spec"].as_str() {
        let file = ctx.path(path)?;
        if file.is_file()
            && let Some(value) = ctx.json(file)?.get("canvas")
        {
            return validated(value).map(Some);
        }
    }
    let reference = &row["reference"];
    let path = match (
        reference.as_str(),
        reference.get("sha256"),
        reference["path"].as_str(),
    ) {
        (Some(name), _, _) => ctx.path(name)?,
        (_, Some(_), _) => ctx.verify(reference)?,
        (_, _, Some(name)) => ctx.path(name)?,
        _ => return Ok(None),
    };
    if !path.is_file() || image::ImageFormat::from_path(&path).is_err() {
        return Ok(None);
    }
    let (width, height) = crate::review::image_dimensions(&path)
        .context("read first page reference dimensions for shared canvas")?;
    let (w, h) = (f64::from(width), f64::from(height));
    let scale = (40. / 3.) / w.max(h);
    let canvas = Canvas {
        width_px: w,
        height_px: h,
        width_in: w * scale,
        height_in: h * scale,
    };
    canvas.validate()?;
    Ok(Some(canvas))
}

pub(crate) fn stored(ctx: &Context) -> Result<Option<Canvas>> {
    review_routes::load_plan(ctx)?
        .get("canvas")
        .map(validated)
        .transpose()
}

pub(crate) fn check(ctx: &Context, actual: [i64; 2]) -> Result<()> {
    if let Some(expected) = stored(ctx)? {
        ensure!(
            expected.physical_emu() == actual,
            "page physical size {actual:?} EMU differs from the frozen deck canvas {:?} EMU; use width_in {} and height_in {} from the assignment before submitting",
            expected.physical_emu(),
            expected.width_in,
            expected.height_in
        );
    }
    Ok(())
}
