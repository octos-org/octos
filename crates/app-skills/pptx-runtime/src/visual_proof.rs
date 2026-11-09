//! Conservative pixel evidence. Supported geometry is not semantic ownership.
use image::{RgbImage, imageops};
use serde_json::{Value, json};

pub const ALGORITHM: &str = "artwork-evidence-v1";

fn distance(a: image::Rgb<u8>, b: image::Rgb<u8>) -> u8 {
    (0..3).map(|i| a[i].abs_diff(b[i])).fold(0, u8::max)
}
fn edge(im: &RgbImage, x: u32, y: u32) -> bool {
    let p = *im.get_pixel(x, y);
    [
        (x.saturating_sub(2), y),
        ((x + 2).min(im.width() - 1), y),
        (x, y.saturating_sub(2)),
        (x, (y + 2).min(im.height() - 1)),
    ]
    .iter()
    .any(|&(a, b)| distance(p, *im.get_pixel(a, b)) >= 35)
}

/// A one-way test: artwork edges in the blank should still exist in the design.
/// Added reference text need not exist in the blank. Smooth/sparse images cannot
/// establish correspondence and remain unknown rather than receiving a pass.
pub fn reference_alignment(design: &RgbImage, blank: &RgbImage) -> Value {
    let w = blank.width().min(640);
    let h = (u64::from(blank.height()) * u64::from(w) / u64::from(blank.width())).max(1) as u32;
    let a = imageops::resize(design, w, h, imageops::FilterType::Triangle);
    let b = imageops::resize(blank, w, h, imageops::FilterType::Triangle);
    if a == b {
        return json!({"algorithm":ALGORITHM,"status":"supported","method":"identical_normalized_pixels","can_use_as_layout_reference":true});
    }
    let mut total = 0u64;
    let mut matched = 0u64;
    let mut tiles = std::collections::BTreeSet::new();
    for y in (2..h.saturating_sub(2)).step_by(2) {
        for x in (2..w.saturating_sub(2)).step_by(2) {
            if !edge(&b, x, y) {
                continue;
            }
            total += 1;
            tiles.insert((x * 8 / w, y * 8 / h));
            let p = *b.get_pixel(x, y);
            let found = (y.saturating_sub(2)..=(y + 2).min(h - 1)).any(|ny| {
                (x.saturating_sub(2)..=(x + 2).min(w - 1))
                    .any(|nx| distance(p, *a.get_pixel(nx, ny)) <= 40 && edge(&a, nx, ny))
            });
            matched += u64::from(found);
        }
    }
    let ratio = matched as f64 / total.max(1) as f64;
    let status = if total < 100 || tiles.len() < 4 {
        "unknown"
    } else if ratio < 0.5 {
        "incompatible"
    } else if ratio >= 0.8 {
        "supported"
    } else {
        "unknown"
    };
    json!({"algorithm":ALGORITHM,"status":status,"can_use_as_layout_reference":status=="supported",
      "sampled_artwork_edges":total,"matched_edges":matched,"match_fraction":ratio,"occupied_tiles":tiles.len(),
      "normalized_to":[w,h],"method":"blank-edge correspondence within two normalized pixels",
      "next":if status=="supported" {"Review local relations; correspondence does not prove text placement."}else{"Verify that design and blank derive from the same composition; restore the correct pair before using subtraction to close layout defects."}})
}

/// Check a proposed quad against the actual blank pixels, independently of text.
/// All four sides need sustained boundary evidence. Borderless or ambiguous
/// artwork remains unknown; metadata cannot promote it to pixel-supported.
pub fn owner_boundary(blank: &RgbImage, quad: [[f64; 2]; 4]) -> Value {
    let sample = |x: f64, y: f64| -> Option<image::Rgb<u8>> {
        if x < 0. || y < 0. || x >= f64::from(blank.width()) || y >= f64::from(blank.height()) {
            None
        } else {
            Some(*blank.get_pixel(
                x.round().min(f64::from(blank.width() - 1)) as u32,
                y.round().min(f64::from(blank.height() - 1)) as u32,
            ))
        }
    };
    let mut sides = Vec::new();
    for i in 0..4 {
        let a = quad[i];
        let b = quad[(i + 1) % 4];
        let dx = b[0] - a[0];
        let dy = b[1] - a[1];
        let len = dx.hypot(dy);
        let n = [-dy / len.max(1.), dx / len.max(1.)];
        let steps = (len / 2.).ceil().clamp(8., 1024.) as usize;
        let mut matched = 0usize;
        for step in 0..steps {
            let t = (step as f64 + 0.5) / steps as f64;
            let p = [a[0] + dx * t, a[1] + dy * t];
            let hit = (-3..=3).any(|shift| {
                let c = [
                    p[0] + n[0] * f64::from(shift),
                    p[1] + n[1] * f64::from(shift),
                ];
                let (Some(center), Some(out), Some(inside)) = (
                    sample(c[0], c[1]),
                    sample(c[0] + n[0] * 5., c[1] + n[1] * 5.),
                    sample(c[0] - n[0] * 5., c[1] - n[1] * 5.),
                ) else {
                    return false;
                };
                distance(out, inside) >= 40
                    || (distance(center, out) >= 40 && distance(center, inside) >= 40)
            });
            matched += usize::from(hit);
        }
        sides.push(json!({"edge":i,"length_px":len,"samples":steps,"matched":matched,"support":matched as f64/steps as f64}));
    }
    let supported = sides.iter().all(|v| {
        v["length_px"].as_f64().is_some_and(|n| n >= 12.)
            && v["support"].as_f64().is_some_and(|n| n >= 0.65)
    });
    json!({"algorithm":ALGORITHM,"status":if supported{"supported"}else{"unknown"},"sides":sides,
      "semantic_ownership_verified":false,"next":"Boundary support is only geometric. Review the text-to-artwork relationship; unsupported edges require a measured local quad, not a prose pass."})
}
