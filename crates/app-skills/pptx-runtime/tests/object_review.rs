use octos_pptx::{
    Context,
    object_review::{self, CHECKS},
    spec::Spec,
};
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

// Synthetic fixture answers only: these never run as production review code.
fn answers(cycle: &Value, spec: &Spec) -> Value {
    json!({"object_reviews":object_review::inventory(cycle,spec).unwrap().iter().map(|v| {
        let checks:serde_json::Map<String,Value> = CHECKS.iter().map(|k|(k.to_string(),json!({"status":"pass","observation":"Synthetic fixture observation"}))).collect();
        json!({"page":v["page"],"object_id":v["object_id"],"evidence_hash":v["evidence_hash"],"checks":checks})
    }).collect::<Vec<_>>()})
}

#[test]
fn no_flags_or_blanket_pass_does_not_skip_an_object() {
    let (c, s) = fixture();
    let a = object_review::assess(
        &c,
        &s,
        &json!({"inspected":{"readability":["scene"]}}),
        &json!({}),
    )
    .unwrap();
    assert_eq!(a.unknown, 2);
    let mut r = answers(&c, &s);
    r["object_reviews"][0]["checks"]["ownership"]["status"] = json!("unknown");
    r["object_reviews"][1]["checks"]["alignment"]["observation"] = json!("");
    assert_eq!(
        object_review::assess(&c, &s, &r, &json!({}))
            .unwrap()
            .unknown,
        2
    );
    r["object_reviews"][0]["checks"]["containment"]["status"] = json!("defect");
    let a = object_review::assess(&c, &s, &r, &json!({})).unwrap();
    assert_eq!(a.defects, 1);
    // Omitting the defect on the next call cannot close it.
    assert_eq!(
        object_review::assess(&c, &s, &json!({}), &r)
            .unwrap()
            .defects,
        1
    );
}

#[test]
fn changes_to_neighbor_pixels_owner_or_style_invalidate_old_review() {
    let (c, s) = fixture();
    let old = answers(&c, &s);
    assert_eq!(
        object_review::assess(&c, &s, &json!({}), &old)
            .unwrap()
            .unknown,
        0
    );
    let mut renamed = c.clone();
    renamed["renders"]["editable"]["pages"][0]["path"] = json!("new-name.png");
    assert_eq!(
        object_review::assess(&renamed, &s, &json!({}), &old)
            .unwrap()
            .unknown,
        0
    );
    renamed["renders"]["editable"]["pages"][0]["sha256"] = json!("new-pixels");
    assert_eq!(
        object_review::assess(&renamed, &s, &old, &old)
            .unwrap()
            .unknown,
        2
    );
    let mut moved = s.clone();
    moved.pages[0].texts[1].bounds[0] += 30.;
    assert_eq!(
        object_review::assess(&c, &moved, &old, &old)
            .unwrap()
            .unknown,
        2
    );
    let mut font = s.clone();
    font.styles.get_mut("body").unwrap()["size_pt"] = json!(17);
    assert_eq!(
        object_review::assess(&c, &font, &old, &old)
            .unwrap()
            .unknown,
        2
    );
    let mut owner = s.clone();
    owner.pages[0].regions.get_mut("card").unwrap()["quad"][0][0] = json!(90);
    assert_eq!(
        object_review::assess(&c, &owner, &old, &old)
            .unwrap()
            .unknown,
        2
    );
}

#[test]
fn duplicate_unknown_and_malformed_reviews_are_rejected() {
    let (c, s) = fixture();
    let r = answers(&c, &s);
    let mut duplicate = r.clone();
    duplicate["object_reviews"]
        .as_array_mut()
        .unwrap()
        .push(r["object_reviews"][0].clone());
    assert!(object_review::assess(&c, &s, &duplicate, &json!({})).is_err());
    let mut wrong = r;
    wrong["object_reviews"][0]["object_id"] = json!("nonexistent");
    assert!(object_review::assess(&c, &s, &wrong, &json!({})).is_err());
    assert!(object_review::assess(&c, &s, &json!({"object_reviews":true}), &json!({})).is_err());
}

#[test]
fn submission_binds_hashes_without_inventing_or_retargeting_observations() {
    let template = json!({"review_id":"review-one","cycle_sha256":"real-cycle",
        "object_reviews":[{"page":"scene","object_id":"a","evidence_hash":"real-object",
        "checks":{"direction":{},"ownership":{},"alignment":{},"containment":{},"typography":{}}}]});
    let authored = json!({"review_id":"review-one","cycle_sha256":"[hex-redacted]",
        "object_reviews":[{"page":"scene","object_id":"a","evidence_hash":"[hex-redacted]",
        "checks":{"direction":{"status":"defect","observation":"Text and label axes differ"},
        "ownership":{"status":"unknown","observation":""}}}],"findings":[{"id":"one"}]});
    let bound = object_review::bind_observations(&template, &authored).unwrap();
    assert_eq!(bound["cycle_sha256"], "real-cycle");
    assert_eq!(bound["object_reviews"][0]["evidence_hash"], "real-object");
    assert_eq!(
        bound["object_reviews"][0]["checks"],
        authored["object_reviews"][0]["checks"]
    );
    assert_eq!(bound["findings"], authored["findings"]);
    assert!(bound["object_reviews"][0]["checks"]["alignment"].is_null());
    let mut stale = authored.clone();
    stale["review_id"] = json!("review-old");
    assert!(object_review::bind_observations(&template, &stale).is_err());
    let mut unknown = authored.clone();
    unknown["object_reviews"][0]["object_id"] = json!("other");
    assert!(object_review::bind_observations(&template, &unknown).is_err());
    let mut dup = authored.clone();
    dup["object_reviews"]
        .as_array_mut()
        .unwrap()
        .push(authored["object_reviews"][0].clone());
    assert!(object_review::bind_observations(&template, &dup).is_err());
    let mut empty = authored;
    empty["object_reviews"][0]["checks"]["direction"]["observation"] = json!("");
    assert!(object_review::bind_observations(&template, &empty).is_err());
}

#[test]
fn comparisons_bind_by_receipt_without_changing_judgments_or_filling_coverage() {
    let template = json!({"review_id":"review-current","cycle_sha256":"cycle",
        "object_reviews":[],"comparisons":[{"page":"scene","comparison_key":"actual-key",
        "label_findings":[{"id":"tilt","evidence_sha256":"actual-panel"}]}]});
    let observations = json!({"review_id":"review-current","object_reviews":[],
        "comparisons":[{"page":"scene","comparison_key":"[hex-redacted]","inspected":true,
        "note":"Read the current page","regions":[],"label_findings":[{
            "id":"tilt","evidence_sha256":"[hex-redacted]","classification":"defect",
            "note":"Text crosses the label edge"}]}]});
    let bound = object_review::bind_observations(&template, &observations).unwrap();
    assert_eq!(bound["comparisons"][0]["comparison_key"], "actual-key");
    assert_eq!(
        bound["comparisons"][0]["label_findings"][0]["evidence_sha256"],
        "actual-panel"
    );
    assert_eq!(
        bound["comparisons"][0]["label_findings"][0]["classification"],
        "defect"
    );
    assert_eq!(bound["comparisons"][0]["regions"], json!([]));
    for (field, value) in [("review_id", json!("old")), ("comparisons", json!(true))] {
        let mut bad = observations.clone();
        bad[field] = value;
        assert!(object_review::bind_observations(&template, &bad).is_err());
    }
    let mut bad = observations.clone();
    bad["comparisons"][0]["label_findings"][0]["id"] = json!("not-in-pack");
    assert!(object_review::bind_observations(&template, &bad).is_err());
    let mut duplicate = observations.clone();
    duplicate["comparisons"]
        .as_array_mut()
        .unwrap()
        .push(observations["comparisons"][0].clone());
    assert!(object_review::bind_observations(&template, &duplicate).is_err());
    let omitted = object_review::bind_observations(
        &template,
        &json!({"review_id":"review-current","object_reviews":[]}),
    )
    .unwrap();
    assert!(omitted.get("comparisons").is_none());
}

#[test]
fn context_contains_owner_and_off_slide_objects_are_not_hidden() {
    let (c, mut s) = fixture();
    let rows = object_review::inventory(&c, &s).unwrap();
    let v: [f64; 4] = serde_json::from_value(rows[0]["viewport_normalized_xyxy"].clone()).unwrap();
    assert!(v[0] < 100. / 800. && v[1] < 65. / 450. && v[2] > 410. / 800. && v[3] > 220. / 450.);
    assert_eq!(rows[0]["declared_owner_is_semantically_verified"], false);
    s.pages[0].texts[1].bounds = [2000., 3000., 100., 100.];
    s.pages[0].design = None;
    let rows = object_review::inventory(&c, &s).unwrap();
    assert_eq!(rows[1]["viewport_normalized_xyxy"], json!([0., 0., 1., 1.]));
    assert_eq!(rows[1]["design_available"], false);
    assert_eq!(rows[1]["sources"].as_array().unwrap().len(), 2);
}

#[test]
fn reading_index_reuses_shared_context_but_keeps_location_and_object_identity() {
    let a = json!({"kind":"object","page":"scene","object_id":"a","evidence_hash":"a",
        "viewport_normalized_xyxy":[0.1,0.1,0.5,0.5],"images":[{"role":"current","image":{"path":"a.png","sha256":"pixels"}}]});
    let mut b = a.clone();
    b["object_id"] = json!("b");
    b["evidence_hash"] = json!("b");
    b["images"][0]["image"]["path"] = json!("same-pixels.png");
    let mut c = b.clone();
    c["object_id"] = json!("c");
    c["viewport_normalized_xyxy"] = json!([0.5, 0.5, 0.9, 0.9]);
    let groups = octos_pptx::feedback::reading_groups(&[a, b, c]);
    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0]["objects"].as_array().unwrap().len(), 2);
    assert_eq!(groups[0]["objects"][1]["evidence_hash"], "b");
    assert_eq!(groups[1]["objects"][0]["object_id"], "c");
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
