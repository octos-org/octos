use octos_pptx::Context;
use serde_json::{Value, json};

fn fixture(root: &std::path::Path) -> Context {
    let ctx = Context::new(root).unwrap();
    image::RgbImage::from_pixel(800, 450, image::Rgb([250, 250, 245]))
        .save(root.join("art.png"))
        .unwrap();
    ctx.write_json("source.json", &json!({"schema_version":1,
        "canvas":{"width_px":800,"height_px":450,"width_in":8,"height_in":4.5},
        "styles":{"body":{"font":"Inter","size_pt":20},"small":{"font":"Inter","size_pt":15}},
        "pages":[{"id":"slide","background":"art.png","copy":{"a":"First row","b":"Second row","title":"Heading"},
        "typography_groups":[{"id":"list","members":["a","b"],"fields":["font","size_pt"],"basis":"two same-level list labels; heading is separate"}],
        "texts":[{"id":"a","text":"First row","style":"body","box":[100,100,350,60]},
            {"id":"b","text":"Second row","style":"small","box":[100,200,350,60]},
            {"id":"title","text":"Heading","style":"body","format":{"size_pt":32,"bold":true},"box":[80,20,600,60]}]}]})).unwrap();
    ctx
}

fn cycle(ctx: &Context, out: &str) -> Value {
    ctx.execute(json!({"action":"cycle","spec":"source.json","output":out,"width":800}))
        .unwrap();
    ctx.execute(json!({"action":"qa_check","cycle":format!("{out}/cycle.json")}))
        .unwrap()
}

#[test]
fn actual_exported_runs_expose_size_and_required_group_mismatch() {
    let tmp = tempfile::tempdir().unwrap();
    let ctx = fixture(tmp.path());
    let qa = cycle(&ctx, "before");
    let inspection = ctx
        .execute(json!({"action":"inspect","cycle":"before/cycle.json"}))
        .unwrap();
    let rows = inspection["objects"].as_array().unwrap();
    assert_eq!(rows[0]["font_runs"][0]["size_pt"], 20.);
    assert_eq!(rows[1]["font_runs"][0]["size_pt"], 15.);
    assert_eq!(rows[2]["font_runs"][0]["bold"], true);
    assert_eq!(inspection["typography_groups"][0]["status"], "fail");
    let findings = qa["findings"].as_array().unwrap();
    let mismatch = findings
        .iter()
        .find(|f| f["code"] == "TYPOGRAPHY_GROUP_MISMATCH")
        .unwrap();
    assert_eq!(mismatch["status"], "fail");
    assert_eq!(mismatch["observed"]["differences"][0]["actual"], 15.);
    assert_eq!(mismatch["model_disposition_allowed"], false);
    let mut source = ctx.json("source.json").unwrap();
    source["pages"][0]["texts"][1]["style"] = json!("body");
    ctx.replace_json("source.json", &source).unwrap();
    let fixed = cycle(&ctx, "after");
    assert!(
        !fixed["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["code"] == "TYPOGRAPHY_GROUP_MISMATCH")
    );
    assert_eq!(fixed["typography_groups"][0]["status"], "consistent");
    // Different heading is intentional and outside the authored same-role group.
    assert_eq!(fixed["quality_approved"], false);
}

#[test]
fn typography_constraints_cannot_disappear_during_a_repair() {
    let tmp = tempfile::tempdir().unwrap();
    let ctx = fixture(tmp.path());
    let before = cycle(&ctx, "before");
    let mut source = ctx.json("source.json").unwrap();
    source["pages"][0]["typography_groups"] = json!([]);
    ctx.replace_json("source.json", &source).unwrap();
    ctx.execute(json!({"action":"cycle","spec":"source.json","output":"after","width":800}))
        .unwrap();
    let (cy, spec) =
        octos_pptx::review::bound_cycle(&ctx, &ctx.path("after/cycle.json").unwrap()).unwrap();
    let inspect = ctx
        .execute(json!({"action":"inspect","cycle":"after/cycle.json"}))
        .unwrap();
    let qa = octos_pptx::qa::assess(&ctx, &cy, &spec, &inspect, &before["contract"]).unwrap();
    assert!(
        qa["constraint_changes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["field"] == "typography_groups")
    );
    assert_eq!(
        qa["contract"]["pages"]["slide"]["typography_groups"],
        before["contract"]["pages"]["slide"]["typography_groups"]
    );
    assert_ne!(qa["status"], "pass");
}

#[test]
fn invalid_groups_are_rejected_and_different_styles_need_not_be_equal() {
    let tmp = tempfile::tempdir().unwrap();
    let ctx = fixture(tmp.path());
    let source = ctx.json("source.json").unwrap();
    for members in [json!(["a", "a"]), json!(["a", "unknown"]), json!(["a"])] {
        let mut bad = source.clone();
        bad["pages"][0]["typography_groups"][0]["members"] = members;
        ctx.replace_json("source.json", &bad).unwrap();
        assert!(
            ctx.execute(json!({"action":"build","spec":"source.json","output":"bad"}))
                .is_err()
        );
    }
    let mut good = source;
    good["pages"][0]["typography_groups"][0]["fields"] = json!(["font"]);
    ctx.replace_json("source.json", &good).unwrap();
    let qa = cycle(&ctx, "different_sizes_intended");
    assert_eq!(qa["typography_groups"][0]["status"], "consistent");
}
