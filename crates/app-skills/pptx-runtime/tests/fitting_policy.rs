use octos_pptx::{Context, fs::sha_file};
use serde_json::json;

fn fixture(root: &std::path::Path) -> Context {
    let ctx = Context::new(root).unwrap();
    image::RgbImage::from_pixel(800, 450, image::Rgb([245, 240, 230]))
        .save(root.join("art.png"))
        .unwrap();
    ctx.write_json("source.json", &json!({"schema_version":1,
        "canvas":{"width_px":800,"height_px":450,"width_in":8,"height_in":4.5},
        "styles":{"body":{"font":"Inter","size_pt":20}},
        "pages":[{"id":"scene","background":"art.png","copy":{"caption":"A short label"},
            "texts":[{"id":"caption","text":"A short label","style":"body","box":[100,120,500,200]}]}]})).unwrap();
    ctx
}

#[test]
fn explicit_size_interval_can_grow_and_fixed_size_is_exact() {
    let temp = tempfile::tempdir().unwrap();
    let ctx = fixture(temp.path());
    let hash = sha_file(&temp.path().join("source.json")).unwrap();
    for (lo, hi) in [(20., 28.), (24., 24.), (12., 12.)] {
        let result = ctx
            .execute(json!({"action":"fit","spec":"source.json",
            "min_size_pt":lo,"max_size_pt":hi}))
            .unwrap();
        let row = &result["proposals"][0];
        assert_eq!(row["state"], "proposal", "{result}");
        assert_eq!(row["proposed_size_pt"], hi, "{result}");
        assert_eq!(row["set"]["format"]["size_pt"], hi);
    }
    assert_eq!(hash, sha_file(&temp.path().join("source.json")).unwrap());
}

#[test]
fn missing_font_cannot_silently_fit_using_another_face() {
    let temp = tempfile::tempdir().unwrap();
    let ctx = fixture(temp.path());
    let mut spec = ctx.json("source.json").unwrap();
    spec["styles"]["body"]["font"] = json!("Missing-Font-For-Parity-Test");
    ctx.replace_json("source.json", &spec).unwrap();
    let r = ctx
        .execute(json!({"action":"fit","spec":"source.json","min_size_pt":8,"max_size_pt":28}))
        .unwrap();
    assert_eq!(r["proposals"][0]["state"], "font_unresolved");
    assert_eq!(r["proposals"][0]["set"], json!({}));
    assert!(
        !r["proposals"][0]["font_provenance"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn line_limit_applies_even_when_the_original_frame_does_not_overflow() {
    let temp = tempfile::tempdir().unwrap();
    let ctx = fixture(temp.path());
    let mut spec = ctx.json("source.json").unwrap();
    spec["pages"][0]["copy"]["caption"] = json!("Alpha beta gamma delta");
    spec["pages"][0]["texts"][0]["text"] = json!("Alpha beta gamma delta");
    spec["pages"][0]["texts"][0]["box"] = json!([100, 120, 220, 200]);
    ctx.replace_json("source.json", &spec).unwrap();
    let result = ctx
        .execute(json!({"action":"fit","spec":"source.json",
        "min_size_pt":8,"max_size_pt":20,"max_lines":1}))
        .unwrap();
    let row = &result["proposals"][0];
    assert_eq!(row["overflow_before"], false, "{result}");
    assert_eq!(row["state"], "proposal", "{result}");
    assert_eq!(row["lines"], 1, "{result}");
    assert!(row["proposed_size_pt"].as_f64().unwrap() < 20.);
    let fixed = ctx
        .execute(json!({"action":"fit","spec":"source.json","max_lines":1}))
        .unwrap();
    assert_eq!(fixed["proposals"][0]["state"], "no_fit_within_bounds");
    assert_eq!(fixed["proposals"][0]["set"], json!({}));
}

#[test]
fn explicit_newlines_are_not_removed_to_satisfy_line_limit() {
    let temp = tempfile::tempdir().unwrap();
    let ctx = fixture(temp.path());
    let mut spec = ctx.json("source.json").unwrap();
    for field in ["copy", "texts"] {
        if field == "copy" {
            spec["pages"][0][field]["caption"] = json!("first\nsecond");
        } else {
            spec["pages"][0][field][0]["text"] = json!("first\nsecond");
        }
    }
    ctx.replace_json("source.json", &spec).unwrap();
    let result = ctx
        .execute(json!({"action":"fit","spec":"source.json",
        "min_size_pt":6,"max_size_pt":24,"max_lines":1}))
        .unwrap();
    assert_eq!(result["proposals"][0]["state"], "no_fit_within_bounds");
    assert_eq!(result["proposals"][0]["set"], json!({}));
}

#[test]
fn unknown_options_and_invalid_ranges_cannot_silently_succeed() {
    let temp = tempfile::tempdir().unwrap();
    let ctx = fixture(temp.path());
    for extra in [
        json!({"max_size_pt":0}),
        json!({"max_lines":0}),
        json!({"max_lines":1.5}),
        json!({"min_size_pt":20,"max_size_pt":10}),
        json!({"unknown_max_lines":1}),
    ] {
        let mut request = json!({"action":"fit","spec":"source.json"});
        for (k, v) in extra.as_object().unwrap() {
            request[k] = v.clone();
        }
        assert!(ctx.execute(request.clone()).is_err(), "{request}");
    }
}

#[test]
fn target_size_policy_uses_the_target_not_the_old_large_frame() {
    let temp = tempfile::tempdir().unwrap();
    let ctx = fixture(temp.path());
    let result = ctx
        .execute(json!({"action":"fit","spec":"source.json",
        "ink_align":"middle","reflow":true,"min_size_pt":8,"max_size_pt":20,"max_lines":1,
        "targets":[{"page":"scene","id":"caption","box":[150,200,120,60]}]}))
        .unwrap();
    let row = &result["proposals"][0];
    assert_eq!(row["state"], "proposal", "{result}");
    assert_eq!(row["ink_alignment"]["status"], "proposed", "{result}");
    assert_eq!(row["lines"], 1);
    assert!(row["proposed_size_pt"].as_f64().unwrap() < 20.);
    // Reopen the actual exported artifact and verify the selected size/line count.
    let mut spec = ctx.json("source.json").unwrap();
    for (k, v) in row["set"].as_object().unwrap() {
        spec["pages"][0]["texts"][0][k] = v.clone();
    }
    ctx.write_json("fitted.json", &spec).unwrap();
    let build = ctx
        .execute(json!({"action":"build","spec":"fitted.json","output":"fitted"}))
        .unwrap();
    let inspected = ctx
        .execute(json!({"action":"inspect","input":ctx.verify(&build["editable"]).unwrap()}))
        .unwrap();
    assert_eq!(inspected["objects"][0]["layout"]["lines"], 1);
    assert_eq!(inspected["objects"][0]["layout"]["overflow"], false);
}

#[test]
fn impossible_tiny_frame_returns_no_partial_shrink() {
    let temp = tempfile::tempdir().unwrap();
    let ctx = fixture(temp.path());
    let mut spec = ctx.json("source.json").unwrap();
    spec["pages"][0]["texts"][0]["box"] = json!([100, 120, 60, 12]);
    spec["pages"][0]["texts"][0]["format"] = json!({"italic":true,"color":"BB0000"});
    ctx.replace_json("source.json", &spec).unwrap();
    let r = ctx
        .execute(json!({"action":"fit","spec":"source.json","min_size_pt":8}))
        .unwrap();
    assert_eq!(r["proposals"][0]["state"], "no_fit_within_bounds");
    assert_eq!(r["proposals"][0]["set"], json!({}));
}

#[test]
fn emergency_identifier_breaks_are_not_accepted_as_fit_and_locked_size_is_preserved() {
    let temp = tempfile::tempdir().unwrap();
    let ctx = fixture(temp.path());
    let mut spec = ctx.json("source.json").unwrap();
    spec["pages"][0]["texts"][0]["box"] = json!([100, 120, 80, 280]);
    spec["pages"][0]["texts"][0]["text"] = json!("LongIdentifierWithoutSpaces");
    spec["pages"][0]["copy"]["caption"] = json!("LongIdentifierWithoutSpaces");
    ctx.replace_json("source.json", &spec).unwrap();
    let r = ctx
        .execute(json!({"action":"fit","spec":"source.json"}))
        .unwrap();
    assert_eq!(r["proposals"][0]["state"], "no_fit_within_bounds");
    spec["pages"][0]["texts"][0]["box"] = json!([100, 120, 600, 200]);
    spec["styles"]["body"]["locked"] = json!(true);
    ctx.replace_json("source.json", &spec).unwrap();
    let r = ctx
        .execute(json!({"action":"fit","spec":"source.json","min_size_pt":10,"max_size_pt":28}))
        .unwrap();
    assert_eq!(r["proposals"][0]["proposed_size_pt"], 20.);
    assert_eq!(r["proposals"][0]["set"], json!({}));
    assert!(
        ctx.execute(json!({"action":"fit","spec":"source.json","min_size_pt":24,"max_size_pt":24}))
            .is_err()
    );
}
