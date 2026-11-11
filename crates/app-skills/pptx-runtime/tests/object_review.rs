use octos_pptx::{Context, spec::Spec};
use serde_json::{Value, json};

fn fixture() -> (Value, Spec) {
    let spec = serde_json::from_value(json!({"schema_version":1,
        "canvas":{"width_px":800,"height_px":450,"width_in":8,"height_in":4.5},
        "styles":{"body":{"font":"Inter","size_pt":20}},
        "pages":[{"id":"scene","background":{"path":"blank.png","sha256":"b"},
            "design":{"path":"design.png","sha256":"d"},"copy":{"a":"One","b":"Two"},
            "regions":{"card":{"quad":[[100,80],[400,65],[410,220],[100,200]]}},
            "texts":[{"id":"a","text":"One","style":"body","box":[130,100,180,50],"region":"card","rotation":-5},
            {"id":"b","text":"Two","style":"body","box":[460,100,180,50]}]}]})).unwrap();
    (
        json!({"renders":{"editable":{"pages":[{"path":"after.png","sha256":"a"}]},
        "blank":{"pages":[{"path":"blank.png","sha256":"b"}]}}}),
        spec,
    )
}

#[test]
fn native_crop_pixels_and_final_evidence_are_exercised_through_public_tools() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Context::new(dir.path()).unwrap();
    let (_, mut spec) = fixture();
    // Unequal design/render resolutions must use equivalent normalized viewports.
    let design = image::RgbImage::from_fn(1600, 900, |x, y| {
        image::Rgb([(x % 255) as u8, (y % 255) as u8, 210])
    });
    design.save(dir.path().join("design.png")).unwrap();
    image::RgbImage::from_pixel(800, 450, image::Rgb([250, 245, 235]))
        .save(dir.path().join("blank.png"))
        .unwrap();
    spec.pages[0].background = json!("blank.png");
    spec.pages[0].design = Some(json!("design.png"));
    ctx.write_json("deck.json", &serde_json::to_value(spec).unwrap())
        .unwrap();
    ctx.execute(json!({"action":"cycle","spec":"deck.json","output":"attempt","width":800}))
        .unwrap();
    ctx.execute(json!({"action":"evidence_init","task_id":"fixture","owner_agent":"test","stagnant_attempts":3,"deliverables":["attempt/editable.pptx"]})).unwrap();
    let out = ctx
        .execute(json!({"action":"review_pack","cycle":"attempt/cycle.json","output":"pack"}))
        .unwrap();
    let pack = ctx.json(ctx.verify(&out["report"]).unwrap()).unwrap();
    assert_eq!(out["objects"], 2);
    assert_eq!(pack["object_evidence"].as_array().unwrap().len(), 2);
    assert_eq!(pack["view_batches"].as_array().unwrap().len(), 3);
    let index = ctx.json(ctx.verify(&out["view_index"]).unwrap()).unwrap();
    assert_eq!(index["batches"].as_array().unwrap().len(), 3);
    assert_eq!(index["batches"][1]["images"].as_array().unwrap().len(), 3);
    let crop = &pack["object_evidence"][0]["images"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["role"] == "design")
        .unwrap();
    assert_eq!(crop["resized"], false);
    let bounds: [u32; 4] = serde_json::from_value(crop["crop_px_xywh"].clone()).unwrap();
    let actual = image::open(ctx.verify(&crop["image"]).unwrap())
        .unwrap()
        .to_rgb8();
    assert_eq!(
        actual,
        image::imageops::crop_imm(&design, bounds[0], bounds[1], bounds[2], bounds[3]).to_image()
    );
    let template = ctx
        .json(ctx.verify(&out["object_review_template"]).unwrap())
        .unwrap();
    assert!(
        template["object_reviews"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["checks"]["ownership"]["status"] == "unknown")
    );
    ctx.write_json("review.json", &template).unwrap();
    let recorded = ctx
        .execute(
            json!({"action":"evidence_record","cycle":"attempt/cycle.json","review":"review.json"}),
        )
        .unwrap();
    assert_eq!(recorded["object_coverage"]["unknown_objects"], 2);
    let status = ctx
        .execute(json!({"action":"evidence_status","owner_agent":"test"}))
        .unwrap();
    assert_eq!(status["object_coverage"]["unknown_objects"], 2);
    let mut observations = template.clone();
    observations["cycle_sha256"] = json!("[hex-redacted]");
    for row in observations["object_reviews"].as_array_mut().unwrap() {
        row["evidence_hash"] = json!("[hex-redacted]");
    }
    ctx.write_json("observations.json", &observations).unwrap();
    let submitted = ctx
        .execute(json!({"action":"review_submit","pack":"pack/report.json",
        "observations":"observations.json","output":"submitted.json"}))
        .unwrap();
    assert_eq!(submitted["object_coverage"]["unknown_objects"], 2);
    assert_eq!(ctx.json("submitted.json").unwrap(), template);
    let crop_path = ctx.verify(&crop["image"]).unwrap();
    std::fs::write(crop_path, b"changed image").unwrap();
    assert!(
        ctx.execute(json!({"action":"review_submit","pack":"pack/report.json",
        "observations":"observations.json","output":"changed.json"}))
            .is_err()
    );
}
