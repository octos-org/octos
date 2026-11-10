//! Evidence for layout exceptions. A reviewer may choose a measured explanation,
//! but may not manufacture one by switching axes or calling an offset intentional.
use crate::geometry;
use serde_json::{Value, json};

fn center(b: [f64; 4]) -> [f64; 2] {
    [b[0] + b[2] / 2., b[1] + b[3] / 2.]
}
fn extrema(q: geometry::Quad, axis: usize) -> [f64; 3] {
    let lo = q.iter().map(|p| p[axis]).fold(f64::INFINITY, f64::min);
    let hi = q.iter().map(|p| p[axis]).fold(f64::NEG_INFINITY, f64::max);
    [lo, (lo + hi) / 2., hi]
}

/// Only a complete reference and a pixel-supported design/blank registration can
/// establish a prior centre relation. It is still a candidate ownership relation.
pub fn centers(
    ink: [f64; 4],
    reference: &Value,
    reference_surface: &Value,
    registered: bool,
) -> Vec<Value> {
    if !registered || reference["partial"] != false {
        return Vec::new();
    }
    let (Ok(rq), Ok(sq)) = (
        geometry::quad(&reference["quad"]),
        geometry::quad(&reference_surface["quad"]),
    ) else {
        return Vec::new();
    };
    let (b, a, error) = geometry::plane(sq);
    if error > b[2].min(b[3]) * 0.05 {
        return Vec::new();
    }
    let c = center(b);
    let rc = rq
        .iter()
        .fold([0., 0.], |v, p| [v[0] + p[0] / 4., v[1] + p[1] / 4.]);
    // Do not assign an unrelated, remote OCR box to a local colour component.
    if !geometry::contains(sq, rc, 0.) {
        return Vec::new();
    }
    let rc = geometry::rotate(rc, c, -a);
    let ic = geometry::rotate(center(ink), c, -a);
    let mut rows = Vec::new();
    for axis in 0..2 {
        let ref_delta = rc[axis] - c[axis];
        let current_delta = ic[axis] - c[axis];
        let tolerance = (b[axis + 2] * 0.035).clamp(3., 12.);
        // Require appreciable whitespace; a label filling its owner does not
        // distinguish centre anchoring from fixed left/right edges.
        let local = rq.map(|p| geometry::rotate(p, c, -a));
        let e = extrema(local, axis);
        if ref_delta.abs() <= tolerance && (e[2] - e[0]) < b[axis + 2] * 0.9 {
            rows.push(json!({"axis":if axis==0{"x"}else{"y"},"reference_offset_px":ref_delta,
                "current_offset_px":current_delta,"tolerance_px":tolerance,"owner_quad":sq,
                "reference_quad":rq,"changed":current_delta.abs()>tolerance,
                "basis":"complete reference text centred within the same registered colour surface; ownership still requires visual judgment"}));
        }
    }
    rows
}

fn option(kind: &str, measured: Value) -> Value {
    let data = json!({"kind":kind,"measured":measured});
    let id = format!(
        "support-{}",
        &crate::fs::digest(data.to_string().as_bytes())[..20]
    );
    json!({"id":id,"kind":kind,"measured":data["measured"],"semantic_intent_proven":false})
}

fn anchor_support(
    ink: [f64; 4],
    reference: &Value,
    required_axes: &[usize],
    centers: &[Value],
) -> Option<Value> {
    if reference["partial"] != false {
        return None;
    }
    let q = geometry::quad(&reference["quad"]).ok()?;
    let current = geometry::frame(ink, 0.);
    let names = ["left/top", "center", "right/bottom"];
    let mut axes = Vec::new();
    for &axis in required_axes {
        let original = extrema(q, axis);
        let now = extrema(current, axis);
        // No left-edge excuse for a measured prior centre relationship.
        let centered = centers
            .iter()
            .any(|c| c["axis"] == if axis == 0 { "x" } else { "y" });
        let allowed = if centered { vec![1] } else { vec![0, 1, 2] };
        let best = allowed.into_iter().min_by(|&a, &b| {
            (now[a] - original[a])
                .abs()
                .total_cmp(&(now[b] - original[b]).abs())
        })?;
        let delta = now[best] - original[best];
        let tolerance = (ink[3] * 0.2).clamp(3., 7.);
        if delta.abs() > tolerance {
            return None;
        }
        axes.push(json!({"axis":if axis==0{"x"}else{"y"},"anchor":names[best],"reference_px":original[best],
            "current_px":now[best],"delta_px":delta,"tolerance_px":tolerance}));
    }
    Some(option(
        "preserved_reference_anchors",
        json!({"axes":axes,"reference_quad":q}),
    ))
}

pub fn options(
    code: &str,
    observed: &Value,
    current: ([f64; 4], f64),
    surface: &Value,
    reference: &Value,
    centers: &[Value],
    registered: bool,
) -> Vec<Value> {
    let (ink, rotation) = current;
    match code {
        "SURFACE_DIRECTION_DIFFERS" if registered => {
            if let Some(a) = reference["angle_deg"].as_f64() {
                let delta = geometry::angle(rotation, a);
                if delta.abs() <= 2.5 {
                    return vec![option(
                        "preserved_reference_direction",
                        json!({"reference_degrees":a,"current_degrees":rotation,"delta_degrees":delta,"reference_quad":reference["quad"]}),
                    )];
                }
            }
        }
        "REFERENCE_DIRECTION_DIFFERS" => {
            if let Some(a) = surface["angle_deg"].as_f64() {
                let delta = geometry::angle(rotation, a);
                if delta.abs() <= 2.5 {
                    return vec![option(
                        "follows_measured_surface",
                        json!({"surface_degrees":a,"current_degrees":rotation,"delta_degrees":delta,"surface_quad":surface["quad"]}),
                    )];
                }
            }
        }
        "REFERENCE_ANCHOR_DIFFERS" if registered => {
            let axes: Vec<_> = (0..2)
                .filter(|&i| {
                    observed["delta_px"][i].as_f64().is_some_and(|d| {
                        d.abs()
                            > if i == 0 {
                                ink[3].max(8.)
                            } else {
                                (ink[3] * 0.5).max(5.)
                            }
                    })
                })
                .collect();
            if !axes.is_empty() {
                return anchor_support(ink, reference, &axes, centers)
                    .into_iter()
                    .collect();
            }
        }
        "SURFACE_VERTICAL_OFFSET" if registered => {
            return anchor_support(ink, reference, &[1], centers)
                .into_iter()
                .collect();
        }
        _ => {}
    }
    Vec::new()
}

/// The selected support is an ID from this signal's bound report. A quoted
/// measurement for a different property/object/report is not evidence here.
pub fn supports(signal: &Value, response: &Value) -> bool {
    response["support_id"].as_str().is_some_and(|id| {
        signal["intentional_options"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|o| o["id"] == id)
    })
}
