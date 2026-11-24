//! Exercise the real page publication and independent submission boundary.
use octos_pptx::Context;
use serde_json::{Value, json};

fn spec(angle: f64, italic: bool) -> Value {
    json!({"schema_version":1,"canvas":{"width_px":200,"height_px":100,"width_in":4,"height_in":2},
           "styles":{"base":{"font":"Arial","size_pt":15,"italic":italic}},
           "pages":[{"id":"example","background":"plate.png","copy":{"mark":"Label"},
                     "texts":[{"id":"mark","text":"Label","style":"base","rotation":angle,"box":[40,25,120,50]}]}]})
}

fn page(ctx: &mut Context, folder: &str, angle: f64, italic: bool) -> Value {
    ctx.continuation_owner = Some("maker".into());
    ctx.replace_json("page.json", &spec(angle, italic)).unwrap();
    let cycle = ctx
        .execute(
            json!({"action":"cycle","spec":"page.json","page":"example","output":folder,"width":200,
                                 "freeze_inputs":true,"publish_candidate":true}),
        )
        .unwrap();
    ctx.continuation_owner = Some("reviewer".into());
    let mut req = cycle["peer_review_handoff"]["review_request_template"].clone();
    let v = &mut req["verdict"];
    v["accepted"] = json!(true);
    v["score"] = json!(100);
    v["component_scores"] = json!({"quality":100});
    for check in v["delivery_checks"].as_object_mut().unwrap().values_mut() {
        *check = json!({"status":"pass","basis":"synthetic page comparison"});
    }
    let region = &mut v["source_detail_review"]["regions"]["mark"];
    region["source_glyph"] = json!("italic");
    region["rendered_glyph"] = json!("italic");
    region["rendered_baseline_basis"] =
        json!("Both baselines tilt and letter stems lean in the paired crop.");
    region["representation"] = json!("native");
    region["object_id"] = json!("mark");
    region["basis"] = json!("Compared the source label with the native label.");
    v["source_detail_review"]["scope"] = json!("whole_page");
    req
}

fn setup() -> (tempfile::TempDir, Context) {
    let tmp = tempfile::tempdir().unwrap();
    let mut ctx = Context::new(tmp.path()).unwrap();
    let mut plate = image::RgbImage::from_pixel(200, 100, image::Rgb([255, 255, 255]));
    for y in 75..95 {
        for x in 0..24 {
            plate.put_pixel(x, y, image::Rgb([220 + x as u8; 3]));
        }
    }
    plate.save(tmp.path().join("plate.png")).unwrap();
    for y in [81, 85, 89] {
        for x in 4..20 {
            plate.put_pixel(x, y, image::Rgb([55; 3]));
        }
    }
    plate.save(tmp.path().join("original-art.png")).unwrap();
    let mut source_spec = spec(20., true);
    source_spec["pages"][0]["background"] = json!("original-art.png");
    ctx.write_json("source.json", &source_spec).unwrap();
    let source = ctx
        .execute(json!({"action":"cycle","spec":"source.json","output":"source","width":200}))
        .unwrap();
    let original = &source["renders"]["editable"]["pages"][0];
    ctx.write_json("measurements.json",&json!({"canvas":[200,100],"sourceSha256":original["sha256"],"lines":[
        {"id":"mark","text":"Label","box":[30,10,140,80],"polygon":[[56,19],[150,53],[143,72],[49,38]]}]})).unwrap();
    let mut bound = ctx.binding("measurements.json").unwrap();
    bound["role"] = json!("measurements");
    ctx.write_json("page.json", &spec(0., false)).unwrap();
    ctx.write_json(".octos/pptx-page-plan.json",&json!({"schema_version":2,"coordinator_session":"lead",
        "rubric":{"quality":100},"pages":[{"id":"example","spec":"page.json","maker_session":"maker","reviewer_session":"reviewer",
        "editable_copy":{"mark":"Label"},"source_typography":{"mark":{"source_glyph":"italic"}},"reference":original,"inputs":[bound],"candidate":"candidate.json","review":"review.json"}]})).unwrap();
    ctx.continuation_owner = Some("lead".into());
    ctx.execute(json!({"action":"evidence_init","task_id":"reference-check","verification_mode":"per_page","stagnant_attempts":2,"deliverables":["final.pptx"]})).unwrap();
    (tmp, ctx)
}

#[test]
fn an_italic_run_and_its_rotated_baseline_are_checked_independently() {
    let (_rotated_dir, mut ctx) = setup();
    let rotated = page(&mut ctx, "rotated", 20., false);
    assert!(
        ctx.execute(rotated)
            .unwrap_err()
            .to_string()
            .contains("italic glyphs")
    );
    let (_italic_dir, mut ctx) = setup();
    let italic = page(&mut ctx, "italic", 0., true);
    assert!(
        ctx.execute(italic)
            .unwrap_err()
            .to_string()
            .contains("native baseline")
    );
}

#[test]
fn whole_page_approval_retains_known_source_details_and_requires_current_evidence() {
    let (_dir, mut ctx) = setup();
    let correct = page(&mut ctx, "correct", 20., true);
    let test = |request: &Value, expected: &str| {
        let error = ctx.execute(request.clone()).unwrap_err().to_string();
        assert!(error.contains(expected), "{error}");
    };
    let mut incomplete = correct.clone();
    incomplete["verdict"]["source_detail_review"]["scope"] = json!("local");
    test(&incomplete, "local repair");
    let mut missing = correct.clone();
    missing["verdict"]["source_detail_review"]["regions"] = json!({});
    test(&missing, "source region mark");
    let mut image = correct.clone();
    image["verdict"]["source_detail_review"]["regions"]["mark"]["representation"] =
        json!("artwork");
    test(&image, "requires editable text");
    let mut downgraded = correct.clone();
    downgraded["verdict"]["source_detail_review"]["regions"]["mark"]["source_glyph"] =
        json!("upright");
    test(&downgraded, "changed source region mark");
    let mut unbound = correct.clone();
    unbound["verdict"]["evidence"]
        .as_array_mut()
        .unwrap()
        .remove(0);
    test(&unbound, "bound original");
    let mut bad_style = correct.clone();
    bad_style["verdict"]["source_detail_review"]["regions"]["mark"]["rendered_glyph"] =
        json!("upright");
    test(&bad_style, "rendered glyph");
    let mut blank = correct.clone();
    blank["verdict"]["source_detail_review"]["regions"]["unreadable-row"] = json!({"box":[0,0,10,10],
        "representation":"artwork","source_glyph":"indistinct","basis":"An unreadable line was reported here."});
    test(&blank, "is blank");
    blank["verdict"]["source_detail_review"]["regions"]["unreadable-row"]["box"] =
        json!([3, 78, 18, 14]);
    test(&blank, "source strokes are missing");
    let approved = ctx.execute(correct.clone()).unwrap();
    assert_eq!(approved["accepted"], true);
    assert_eq!(
        ctx.execute(correct.clone()).unwrap()["verdict_id"],
        approved["verdict_id"]
    );
    let mut rejected = correct.clone();
    rejected["replaces_verdict_id"] = approved["verdict_id"].clone();
    let v = &mut rejected["verdict"];
    v["accepted"] = json!(false);
    v["source_detail_review"]["regions"]["unreadable-row"] =
        json!({"box":[0,0,10,10],"representation":"missing"});
    v["open_required_findings"] = json!([{"id":"review-row","kind":"visible","impact":"source row is absent","box":[0,0,10,10]}]);
    let rejection = ctx.execute(rejected).unwrap();
    let mut forget = correct;
    forget["replaces_verdict_id"] = rejection["verdict_id"].clone();
    test(&forget, "previous source region unreadable-row");
    forget["verdict"]["source_detail_review"]["regions"]["unreadable-row"] =
        json!({"box":[0,0,10,10],"representation":"missing"});
    test(&forget, "previous required finding review-row");
    forget["verdict"]["source_detail_review"]["resolved_findings"]["review-row"] =
        json!("Checked again");
    test(&forget, "inspect original and rendered marks");
    assert_eq!(
        ctx.execute(json!({"action":"page_gate","only_page":"example"}))
            .unwrap()["pages"][0]["ready"],
        false
    );
}
