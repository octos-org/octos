//! OCR supplies measured candidates, never authoritative copy or design approval.
use crate::{Context, fs::digest, string};
use anyhow::{Result, ensure};
use serde_json::{Value, json};

pub fn capabilities() -> Value {
    json!({"available":cfg!(target_os="macos"),"backend":if cfg!(target_os="macos") {"apple_vision"} else {"unavailable"},"python_required":false})
}

pub fn recognize(ctx: &Context, req: &Value) -> Result<Value> {
    ensure!(
        cfg!(target_os = "macos"),
        "native OCR currently requires macOS Vision; use an external OCR backend on this host"
    );
    let input = string(req, "input")?;
    let bytes = ctx.read(input)?;
    let reader = image::ImageReader::new(std::io::Cursor::new(&bytes)).with_guessed_format()?;
    let (width, height) = reader.into_dimensions()?;
    ensure!(
        u64::from(width) * u64::from(height) <= 80_000_000,
        "OCR image exceeds 80 million pixels; crop first"
    );
    let languages: Vec<String> = match req.get("languages") {
        Some(v) => serde_json::from_value(v.clone())?,
        None => vec!["zh-Hans".into(), "en-US".into()],
    };
    ensure!(
        languages.len() <= 16 && languages.iter().all(|s| !s.is_empty() && s.len() <= 64),
        "invalid OCR languages"
    );
    // Re-encode a private snapshot. Vision and the returned coordinates both
    // refer to these raw pixels (no EXIF rotation or mutable-path race).
    let temp = tempfile::tempdir()?;
    let png = temp.path().join("pixels.png");
    image::load_from_memory(&bytes)?.save(&png)?;
    #[cfg(target_os = "macos")]
    let detections = macos::recognize(&png, width, height, &languages)?;
    #[cfg(not(target_os = "macos"))]
    let detections: Vec<Value> = Vec::new();
    let result = json!({"schema_version":1,"backend":"apple_vision","quality_approved":false,
        "image":{"path":ctx.path(input)?.strip_prefix(&ctx.root)?.to_string_lossy(),"sha256":digest(&bytes),"width_px":width,"height_px":height},
        "coordinate_space":{"origin":"top_left","units":"pixels","x_axis":"right","y_axis":"down","quad_order":"TL,TR,BR,BL"},
        "languages":languages,"detections":detections,
        "limitations":"OCR may miss, merge or misread stylized text. Reconcile against canonical copy and inspect both design and blank; confidence is recognition confidence, not layout accuracy."});
    if let Some(out) = req.get("output").and_then(Value::as_str) {
        ctx.write_json(out, &result)?;
    }
    Ok(result)
}

#[cfg(any(target_os = "macos", test))]
fn pixel_quad(points: [[f64; 2]; 4], width: u32, height: u32) -> Result<([[f64; 2]; 4], [f64; 4])> {
    ensure!(
        points.iter().flatten().all(|n| n.is_finite()),
        "invalid OCR geometry"
    );
    let q = points.map(|[x, y]| [x * f64::from(width), (1.0 - y) * f64::from(height)]);
    let minx = q.iter().map(|p| p[0]).fold(f64::INFINITY, f64::min);
    let miny = q.iter().map(|p| p[1]).fold(f64::INFINITY, f64::min);
    let maxx = q.iter().map(|p| p[0]).fold(f64::NEG_INFINITY, f64::max);
    let maxy = q.iter().map(|p| p[1]).fold(f64::NEG_INFINITY, f64::max);
    Ok((q, [minx, miny, maxx - minx, maxy - miny]))
}

// The only unsafe code is the typed Objective-C framework boundary. No raw
// pointers escape and all returned objects stay retained for the sync request.
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
mod macos {
    use super::*;
    use objc2::AnyThread;
    use objc2_foundation::{NSArray, NSDictionary, NSString, NSURL};
    use objc2_vision::{
        VNImageRequestHandler, VNRecognizeTextRequest, VNRequest, VNRequestTextRecognitionLevel,
    };

    pub(super) fn recognize(
        path: &std::path::Path,
        width: u32,
        height: u32,
        languages: &[String],
    ) -> Result<Vec<Value>> {
        objc2::rc::autoreleasepool(|_| {
            let url = NSURL::fileURLWithPath(&NSString::from_str(
                path.to_str()
                    .ok_or_else(|| anyhow::anyhow!("non-UTF8 OCR snapshot path"))?,
            ));
            // SAFETY: empty options contain no incorrectly typed values; the
            // private PNG remains immutable until synchronous perform returns.
            let handler = unsafe {
                VNImageRequestHandler::initWithURL_options(
                    VNImageRequestHandler::alloc(),
                    &url,
                    &NSDictionary::new(),
                )
            };
            let request = VNRecognizeTextRequest::new();
            request.setRecognitionLevel(VNRequestTextRecognitionLevel::Accurate);
            request.setUsesLanguageCorrection(false);
            let langs: Vec<_> = languages.iter().map(|s| NSString::from_str(s)).collect();
            request.setRecognitionLanguages(&NSArray::from_retained_slice(&langs));
            let base: &VNRequest = &request;
            handler
                .performRequests_error(&NSArray::from_slice(&[base]))
                .map_err(|e| anyhow::anyhow!("Vision OCR: {e}"))?;
            let mut found = Vec::new();
            if let Some(observations) = request.results() {
                for i in 0..observations.count() {
                    let observation = observations.objectAtIndex(i);
                    let candidates = observation.topCandidates(1);
                    let Some(candidate) = candidates.firstObject() else {
                        continue;
                    };
                    // SAFETY: these are value getters on a retained rectangle
                    // observation returned by Vision for this request.
                    let points = unsafe {
                        [
                            observation.topLeft(),
                            observation.topRight(),
                            observation.bottomRight(),
                            observation.bottomLeft(),
                        ]
                    };
                    let (quad, bbox) = pixel_quad(points.map(|p| [p.x, p.y]), width, height)?;
                    found.push(json!({"text":candidate.string().to_string(),"confidence":candidate.confidence(),"box":bbox,"quad":quad,"geometry_source":"vision_text_observation"}));
                }
            }
            Ok(found)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn converts_bottom_left_normalized_quads_without_losing_rotation() {
        let (q, b) = pixel_quad(
            [[0.1, 0.9], [0.6, 0.8], [0.58, 0.6], [0.08, 0.7]],
            1000,
            500,
        )
        .unwrap();
        assert!((q[0][1] - 50.0).abs() < 0.001);
        assert!((q[1][1] - 100.0).abs() < 0.001);
        assert!((b[0] - 80.0).abs() < 0.001);
        assert!(pixel_quad([[f64::NAN, 0.0]; 4], 100, 100).is_err());
    }
}
