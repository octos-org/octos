//! Native glyph contact with raster edges. No OCR, owner declarations or page rules.
//! Contact is physical evidence; it does not establish that an overlay is unintended.
use image::RgbImage;
use serde_json::{Value, json};
use std::collections::BTreeSet;

pub struct EdgeMap {
    width: u32,
    height: u32,
    scale: [f64; 2],
    edges: Vec<bool>,
}

impl EdgeMap {
    pub fn new(image: &RgbImage, canvas: [f64; 2]) -> Self {
        let (w, h) = image.dimensions();
        let mut edges = vec![false; w as usize * h as usize];
        // Same contrast/neighbor scale used by artwork-aware placement. Flat
        // dark fills and low-contrast paper grain are not raster line edges.
        for y in 0..h {
            for x in 0..w {
                let a = image.get_pixel(x, y).0;
                edges[y as usize * w as usize + x as usize] =
                    [(2i64, 0i64), (-2, 0), (0, 2), (0, -2)]
                        .iter()
                        .any(|(dx, dy)| {
                            let (nx, ny) = (i64::from(x) + dx, i64::from(y) + dy);
                            if nx < 0 || ny < 0 || nx >= i64::from(w) || ny >= i64::from(h) {
                                return false;
                            }
                            let b = image.get_pixel(nx as u32, ny as u32).0;
                            a.iter().zip(b).any(|(a, b)| a.abs_diff(b) >= 40)
                        });
            }
        }
        Self {
            width: w,
            height: h,
            scale: [w as f64 / canvas[0], h as f64 / canvas[1]],
            edges,
        }
    }
}

#[derive(Default)]
pub struct Contact {
    pixels: BTreeSet<(u32, u32)>,
    sampled: u64,
}
impl Contact {
    pub fn sample(&mut self, edges: &EdgeMap, point: [f64; 2]) {
        self.sampled += 1;
        let p = [point[0] * edges.scale[0], point[1] * edges.scale[1]];
        if p.iter().any(|p| !p.is_finite())
            || p[0] < 0.
            || p[1] < 0.
            || p[0] >= edges.width as f64
            || p[1] >= edges.height as f64
        {
            return;
        }
        let (x, y) = (p[0] as u32, p[1] as u32);
        if edges.edges[y as usize * edges.width as usize + x as usize] {
            self.pixels.insert((x, y));
        }
    }
    pub fn finish(&self, edges: &EdgeMap) -> Value {
        let min_pixels = (6. * edges.scale[0] * edges.scale[1]).ceil().max(6.) as usize;
        let points: Vec<_> = self
            .pixels
            .iter()
            .step_by(self.pixels.len().div_ceil(64).max(1))
            .map(|(x, y)| {
                [
                    (*x as f64 + 0.5) / edges.scale[0],
                    (*y as f64 + 0.5) / edges.scale[1],
                ]
            })
            .collect();
        let bounds = if self.pixels.is_empty() {
            Value::Null
        } else {
            let x0 = self
                .pixels
                .iter()
                .map(|p| p.0)
                .min()
                .expect("the nonempty contact pixel set has coordinate extrema");
            let x1 = self
                .pixels
                .iter()
                .map(|p| p.0)
                .max()
                .expect("the nonempty contact pixel set has coordinate extrema");
            let y0 = self
                .pixels
                .iter()
                .map(|p| p.1)
                .min()
                .expect("the nonempty contact pixel set has coordinate extrema");
            let y1 = self
                .pixels
                .iter()
                .map(|p| p.1)
                .max()
                .expect("the nonempty contact pixel set has coordinate extrema");
            json!([
                x0 as f64 / edges.scale[0],
                y0 as f64 / edges.scale[1],
                (x1 - x0 + 1) as f64 / edges.scale[0],
                (y1 - y0 + 1) as f64 / edges.scale[1]
            ])
        };
        json!({"status":"measured","candidate":self.pixels.len()>=min_pixels,
            "contact_pixels":self.pixels.len(),"minimum_contact_pixels":min_pixels,
            "sampled_glyph_pixels":self.sampled,"bounds":bounds,"sample_points":points,
            "point_units":"canvas pixels","count_units":"unique actual blank-render pixels",
            "sampling_subject":"Nontransparent native glyph pixels (alpha >=32), never whitespace or gaps between letters.",
            "edge_probe_offset_render_px":2,
            "interpretation":"A positive count places glyph pixels in the measured contrast-edge neighborhood. Explaining all hits as gaps between letters contradicts the sampling subject. Inspect sample_points on the actual glyphs. The 2-render-pixel edge probe, antialiasing and artwork texture can create benign proximity; this does not itself prove occlusion or a visual defect.",
            "semantic_collision_proven":false,"quality_approved":false,
            "method":"isolated native glyph alpha >=32 touching >=40-channel contrast edges in the actual derived-blank render; deduplicated raster samples",
            "limitations":"Raster-edge contact may be intentional. No contact is not proof of clearance: low-contrast or thick uniform interiors may be missed. This is not OCR or arrow recognition."})
    }
}
