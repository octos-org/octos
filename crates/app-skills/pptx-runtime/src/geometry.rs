use anyhow::{Result, ensure};
use serde_json::{Value, json};
pub type Quad = [[f64; 2]; 4];
pub fn usage() -> Value {
    json!({"action":"geometry","spec":"deck/source/deck.json","image":"full-page.png",
        "quad":[[100,100],[400,120],[396,180],[96,160]],"padding":4,
        "notes":"spec is a FILE PATH string, never an array or region object. quad is TL,TR,BR,BL in the full-page image pixels. With image+spec, box and rotation are converted to canvas coordinates. Read owner_boundary: the supplied quad may not match the actual artwork. Geometry does not align text. Use fit with ink_align and explicit targets to obtain glyph-placement proposals."})
}
pub fn rect(v: &Value) -> Result<[f64; 4]> {
    let a: [f64; 4] = serde_json::from_value(v.clone())?;
    ensure!(
        a.iter().all(|n| n.is_finite()) && a[2] > 0. && a[3] > 0.,
        "invalid rectangle"
    );
    Ok(a)
}
pub fn rotate(p: [f64; 2], c: [f64; 2], angle: f64) -> [f64; 2] {
    let (s, t) = angle.to_radians().sin_cos();
    let x = p[0] - c[0];
    let y = p[1] - c[1];
    [c[0] + t * x - s * y, c[1] + s * x + t * y]
}
pub fn frame(b: [f64; 4], angle: f64) -> Quad {
    let [x, y, w, h] = b;
    let c = [x + w / 2., y + h / 2.];
    [[x, y], [x + w, y], [x + w, y + h], [x, y + h]].map(|p| rotate(p, c, angle))
}
pub fn cross(a: [f64; 2], b: [f64; 2], p: [f64; 2]) -> f64 {
    (b[0] - a[0]) * (p[1] - a[1]) - (b[1] - a[1]) * (p[0] - a[0])
}
pub fn quad(v: &Value) -> Result<Quad> {
    if v.is_array() && v[0].is_number() {
        return Ok(frame(rect(v)?, 0.));
    }
    let q: Quad = serde_json::from_value(v.get("quad").unwrap_or(v).clone())?;
    ensure!(
        q.iter().flatten().all(|x| x.is_finite())
            && (0..4).all(|i| cross(q[i], q[(i + 1) % 4], q[(i + 2) % 4]) > 1e-8),
        "quad must be convex TL,TR,BR,BL"
    );
    Ok(q)
}
pub fn contains(q: Quad, p: [f64; 2], tol: f64) -> bool {
    (0..4).all(|i| {
        let a = q[i];
        let b = q[(i + 1) % 4];
        cross(a, b, p) >= -tol * (b[0] - a[0]).hypot(b[1] - a[1]) - 1e-8
    })
}
pub fn angle(a: f64, b: f64) -> f64 {
    (a - b + 180.).rem_euclid(360.) - 180.
}
pub fn plane(q: Quad) -> ([f64; 4], f64, f64) {
    let u = [
        (q[1][0] - q[0][0] + q[2][0] - q[3][0]) / 2.,
        (q[1][1] - q[0][1] + q[2][1] - q[3][1]) / 2.,
    ];
    let v = [
        (q[3][0] - q[0][0] + q[2][0] - q[1][0]) / 2.,
        (q[3][1] - q[0][1] + q[2][1] - q[1][1]) / 2.,
    ];
    let w = u[0].hypot(u[1]);
    let h = v[0].hypot(v[1]);
    let a = u[1].atan2(u[0]).to_degrees();
    let cx = q.iter().map(|p| p[0]).sum::<f64>() / 4.;
    let cy = q.iter().map(|p| p[1]).sum::<f64>() / 4.;
    let b = [cx - w / 2., cy - h / 2., w, h];
    let error = q
        .iter()
        .zip(frame(b, a))
        .map(|(x, y)| (x[0] - y[0]).hypot(x[1] - y[1]))
        .fold(0., f64::max);
    (b, a, error)
}
pub fn proposal(v: &Value) -> Result<Value> {
    let q = quad(
        v.get("region")
            .or(v.get("quad"))
            .ok_or_else(|| anyhow::anyhow!("missing region"))?,
    )?;
    let pad = v.get("padding").and_then(Value::as_f64).unwrap_or(4.);
    ensure!(pad.is_finite() && pad >= 0., "invalid padding");
    let (b, a, e) = plane(q);
    let c = [b[0] + b[2] / 2., b[1] + b[3] / 2.];
    ensure!(contains(q, c, -pad), "padding consumes region");
    let (mut lo, mut hi) = (0., 1.);
    for _ in 0..50 {
        let s = (lo + hi) / 2.;
        let r = [
            c[0] - b[2] * s / 2.,
            c[1] - b[3] * s / 2.,
            b[2] * s,
            b[3] * s,
        ];
        if frame(r, a).iter().all(|p| contains(q, *p, -pad)) {
            lo = s
        } else {
            hi = s
        }
    }
    Ok(
        json!({"box":[c[0]-b[2]*lo/2.,c[1]-b[3]*lo/2.,b[2]*lo,b[3]*lo],"rotation":a,"rigid_fit_error_px":e,"perspective_or_shear":e/b[2].min(b[3])>0.03,"visual_review_required":true}),
    )
}

/// A measured full-page raster and the editable canvas can have different
/// resolutions. Never silently interpret the former as the latter.
pub fn image_to_canvas_proposal(v: &Value, image: [u32; 2], canvas: [f64; 2]) -> Result<Value> {
    ensure!(image.iter().all(|x| *x > 0), "invalid image dimensions");
    let scale = [canvas[0] / image[0] as f64, canvas[1] / image[1] as f64];
    ensure!(
        (scale[0] / scale[1] - 1.).abs() < 0.005,
        "image and canvas aspect ratios differ; use the full-page image, not a crop or stretched preview"
    );
    let input = quad(
        v.get("region")
            .or(v.get("quad"))
            .ok_or_else(|| anyhow::anyhow!("missing region"))?,
    )?;
    let image_frame = frame([0., 0., image[0] as f64, image[1] as f64], 0.);
    ensure!(
        input.iter().all(|p| contains(image_frame, *p, 0.)),
        "measured region outside source image"
    );
    let output = input.map(|p| [p[0] * scale[0], p[1] * scale[1]]);
    let mut mapped = v.clone();
    mapped["region"] = json!(output);
    mapped["padding"] =
        json!(v.get("padding").and_then(Value::as_f64).unwrap_or(4.) * scale[0].min(scale[1]));
    let mut result = proposal(&mapped)?;
    result["region"] = json!({"quad":output});
    result["coordinate_transform"] = json!({"input":"full_page_image_pixels","output":"canvas_pixels",
        "image_dimensions":image,"canvas_dimensions":canvas,"scale_xy":scale,
        "input_region":input,"padding_input_units":"image_pixels"});
    Ok(result)
}
