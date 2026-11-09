use octos_pptx::raster_overlap::proposals;
use serde_json::json;

#[test]
fn joins_only_overlapping_ocr_candidates_with_native_text_and_keeps_uncertainty() {
    let detections = json!([
        {"text":"Old label","box":[100,100,100,30],"confidence":0.99},
        {"text":"Separate annotation","box":[600,300,150,40],"confidence":0.8},
        {"text":"→","box":[110,110,15,20],"confidence":0.4}]);
    let objects = json!([{"object_id":"a","text":"New label","box":[95,95,200,60],"rotation":0}]);
    let r = proposals(
        detections.as_array().unwrap(),
        objects.as_array().unwrap(),
        [800., 450.],
    )
    .unwrap();
    assert_eq!(r.len(), 2);
    assert_eq!(r[0]["native"][0]["object_id"], "a");
    assert_eq!(r[0]["status"], "unreviewed");
    assert_eq!(r[1]["raster_text"], "→"); // possible OCR-on-icon, not an automatic defect
    assert_eq!(r[1]["quality_approved"], false);
}

#[cfg(target_os = "macos")]
#[test]
fn actual_raster_text_overlap_creates_bound_context_crops() {
    use octos_pptx::Context;

    let temp = tempfile::tempdir().unwrap();
    let ctx = Context::new(temp.path()).unwrap();
    image::RgbImage::from_pixel(800, 450, image::Rgb([250, 250, 250]))
        .save(temp.path().join("art.png"))
        .unwrap();
    let mut source = json!({"schema_version":1,
        "canvas":{"width_px":800,"height_px":450,"width_in":8,"height_in":4.5},
        "styles":{"body":{"font":"Inter","size_pt":32}},
        "pages":[{"id":"slide","background":"art.png","copy":{"a":"HELLO WORLD"},
        "texts":[{"id":"a","text":"HELLO WORLD","style":"body","box":[100,100,550,100]}]}]});
    ctx.write_json("source.json", &source).unwrap();
    ctx.execute(json!({"action":"cycle","spec":"source.json","output":"old","width":800}))
        .unwrap();
    source["pages"][0]["background"] = json!("old/rendered/slide-001.png");
    source["pages"][0]["copy"]["a"] = json!("NEW LABEL");
    source["pages"][0]["texts"][0]["text"] = json!("NEW LABEL");
    ctx.write_json("overlap.json", &source).unwrap();
    ctx.execute(json!({"action":"cycle","spec":"overlap.json","output":"new","width":800}))
        .unwrap();
    let result = ctx
        .execute(json!({"action":"raster_overlap","cycle":"new/cycle.json","output":"review"}))
        .unwrap();
    assert!(result["candidate_count"].as_u64().unwrap() > 0, "{result}");
    let report = ctx.json(ctx.verify(&result["report"]).unwrap()).unwrap();
    let candidate = &report["pages"][0]["candidates"][0];
    assert_eq!(candidate["images"].as_array().unwrap().len(), 2);
    assert_eq!(candidate["status"], "unreviewed");
    assert_eq!(report["quality_approved"], false);
    assert!(
        temp.path()
            .join("review/page-001-candidate-001-render.png")
            .is_file()
    );
}
