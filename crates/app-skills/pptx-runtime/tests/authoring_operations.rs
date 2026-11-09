use octos_pptx::{Context, fs::sha_file, spec};
use serde_json::{Value, json};

fn fixture(root: &std::path::Path) -> Context {
    let ctx = Context::new(root).unwrap();
    image::RgbImage::from_pixel(800, 450, image::Rgb([245, 240, 230]))
        .save(root.join("art.png"))
        .unwrap();
    ctx.write_json("source.json", &json!({"schema_version":1,
        "canvas":{"width_px":800,"height_px":450,"width_in":8,"height_in":4.5},
        "styles":{"body":{"font":"Inter","size_pt":20}},
        "pages":[{"id":"scene","background":"art.png","copy":{"caption":"Before → After","other":"Review"},
            "texts":[{"id":"caption","text":"Before → After","style":"body","box":[100,100,500,80],
                "format":{"size_pt":17,"align":"center","color":"222222"}},
                {"id":"other","text":"Review","style":"body","box":[100,250,200,80]}]}]})).unwrap();
    ctx
}

#[test]
fn group_help_can_recover_a_missing_declaration_without_guessing_its_schema() {
    let temp = tempfile::tempdir().unwrap();
    let ctx = fixture(temp.path());
    let help = ctx.execute(json!({"action":"help"})).unwrap();
    let example = &help["fit"];
    let mut source = ctx.json("source.json").unwrap();
    let page = &mut source["pages"][0];
    page["id"] = example["group_source_page_fragment"]["id"].clone();
    page["texts"][0]["id"] = json!("label-a");
    page["texts"][1]["id"] = json!("label-b");
    page["copy"] = json!({"label-a":"Before → After","label-b":"Review"});
    std::fs::write(temp.path().join("source.json"), source.to_string()).unwrap();
    let request = example["group_request"].clone();
    let error = ctx.execute(request.clone()).unwrap_err().to_string();
    assert!(error.contains("group_source_page_fragment"));
    assert!(error.contains("declare the same-role typography group"));

    source["pages"][0]["typography_groups"] =
        example["group_source_page_fragment"]["typography_groups"].clone();
    std::fs::write(temp.path().join("source.json"), source.to_string()).unwrap();
    let result = ctx.execute(request).unwrap();
    assert_eq!(result["group"]["status"], "proposed");
    assert_eq!(result["placement_proposal_count"], 2);
    for row in result["proposals"].as_array().unwrap() {
        assert_eq!(row["set"]["format"]["size_pt"], 18.);
    }
}

#[test]
fn partial_format_patch_preserves_unmentioned_properties_and_reports_effective_changes() {
    let temp = tempfile::tempdir().unwrap();
    let ctx = fixture(temp.path());
    let r = ctx
        .execute(
            json!({"action":"patch","spec":"source.json","output":"changed.json",
        "edits":[{"page":"scene","id":"caption","set":{"format":{"color":"333333"}}}]}),
        )
        .unwrap();
    let s = spec::load(&ctx, &ctx.path("changed.json").unwrap(), None).unwrap();
    let style = s.style(&s.pages[0].texts[0]).unwrap();
    assert_eq!(style.size_pt, 17.);
    assert_eq!(style.align, "center");
    assert_eq!(style.color, "333333");
    assert_eq!(r["changes"][0]["format_mode"], "merge");
    assert_eq!(r["changes"][0]["effective_format_before"]["size_pt"], 17.);
    assert_eq!(r["changes"][0]["effective_format_after"]["size_pt"], 17.);
    assert_eq!(r["changes"][0]["changed_format_fields"], json!(["color"]));
}

#[test]
fn explicit_replacement_can_reset_overrides_but_invalid_or_stale_edits_write_nothing() {
    let temp = tempfile::tempdir().unwrap();
    let ctx = fixture(temp.path());
    ctx.execute(
        json!({"action":"patch","spec":"source.json","output":"reset.json",
        "edits":[{"page":"scene","id":"caption","format_mode":"replace","set":{"format":{}}}]}),
    )
    .unwrap();
    let s = spec::load(&ctx, &ctx.path("reset.json").unwrap(), None).unwrap();
    assert_eq!(s.style(&s.pages[0].texts[0]).unwrap().size_pt, 20.);
    for extra in [
        json!({"format_mode":"typo","set":{"format":{"color":"444444"}}}),
        json!({"set":{"format":null}}),
        json!({"set":{"format":{"size_pt":-1}}}),
    ] {
        let mut edit = json!({"page":"scene","id":"caption"});
        edit.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        assert!(
            ctx.execute(
                json!({"action":"patch","spec":"source.json","output":"bad.json","edits":[edit]})
            )
            .is_err()
        );
        assert!(!temp.path().join("bad.json").exists());
    }
    assert!(ctx.execute(json!({"action":"patch","spec":"source.json","expected_sha256":"stale","output":"bad.json","edits":[]})).is_err());
}

fn parts() -> Value {
    json!([
        {"id":"left","text":"Before","box":[100,100,180,80]},
        {"id":"arrow","text":"→","box":[300,100,60,80]},
        {"id":"right","text":"After","box":[400,100,180,80]}
    ])
}

#[test]
fn split_keeps_canonical_copy_and_tracks_every_unicode_character_through_render() {
    let temp = tempfile::tempdir().unwrap();
    let ctx = fixture(temp.path());
    let hash = sha_file(&temp.path().join("source.json")).unwrap();
    let r = ctx
        .execute(
            json!({"action":"split_text","spec":"source.json","output":"split.json",
        "page":"scene","id":"caption","parts":parts()}),
        )
        .unwrap();
    let s = ctx.json("split.json").unwrap();
    assert_eq!(
        s["pages"][0]["copy"],
        ctx.json("source.json").unwrap()["pages"][0]["copy"]
    );
    assert_eq!(s["pages"][0]["texts"][0]["copy_id"], "caption");
    assert_eq!(s["pages"][0]["texts"][0]["copy_range"], json!([0, 6]));
    assert_eq!(s["pages"][0]["texts"][1]["copy_range"], json!([6, 8]));
    assert_eq!(s["pages"][0]["texts"][2]["copy_range"], json!([8, 14]));
    assert_eq!(r["content_preserved"], true);
    assert_eq!(r["quality_approved"], false);
    assert_eq!(hash, sha_file(&temp.path().join("source.json")).unwrap());
    ctx.execute(json!({"action":"cycle","spec":"split.json","output":"cycle","width":800}))
        .unwrap();
    let inspection = ctx.json("cycle/inspection.json").unwrap();
    assert_eq!(inspection["objects"].as_array().unwrap().len(), 4);
}

#[test]
fn split_rejects_missing_repeated_or_changed_content_without_output() {
    let temp = tempfile::tempdir().unwrap();
    let ctx = fixture(temp.path());
    for bad in [
        json!([{"id":"a","text":"Before","box":[100,100,180,80]},
        {"id":"b","text":"After","box":[400,100,180,80]}]),
        json!([{"id":"a","text":"Before Before","box":[100,100,180,80]},
        {"id":"b","text":"→ After","box":[400,100,180,80]}]),
        json!([{"id":"a","text":"Before","box":[100,100,180,80]},
        {"id":"b","text":"→ Changed","box":[400,100,180,80]}]),
    ] {
        assert!(
            ctx.execute(
                json!({"action":"split_text","spec":"source.json","output":"bad.json",
            "page":"scene","id":"caption","parts":bad})
            )
            .is_err()
        );
        assert!(!temp.path().join("bad.json").exists());
    }
}

#[test]
fn manually_forged_fragment_gaps_overlaps_and_mixed_whole_occurrences_are_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let ctx = fixture(temp.path());
    ctx.execute(
        json!({"action":"split_text","spec":"source.json","output":"split.json",
        "page":"scene","id":"caption","parts":parts()}),
    )
    .unwrap();
    for range in [json!([7, 8]), json!([5, 8]), Value::Null] {
        let mut s = ctx.json("split.json").unwrap();
        s["pages"][0]["texts"][1]["copy_range"] = range;
        let parsed: spec::Spec = serde_json::from_value(s).unwrap();
        assert!(parsed.validate().is_err());
    }
}

#[test]
fn grouped_fit_preserves_selected_size_and_withholds_partial_success() {
    let temp = tempfile::tempdir().unwrap();
    let ctx = fixture(temp.path());
    let mut s = ctx.json("source.json").unwrap();
    s["pages"][0]["typography_groups"] = json!([{"id":"labels","members":["caption","other"],"fields":["size_pt"],"basis":"Two equal-level labels in the source artwork."}]);
    ctx.replace_json("source.json", &s).unwrap();
    let mut r = json!({"action":"fit","spec":"source.json","group":{"page":"scene","id":"labels","size_pt":18},
        "ink_align":"middle","reflow":true,"targets":[
            {"page":"scene","id":"caption","box":[100,100,500,80]},
            {"page":"scene","id":"other","box":[100,250,200,80]}]});
    let out = ctx.execute(r.clone()).unwrap();
    assert_eq!(out["group"]["status"], "proposed");
    assert!(
        out["proposals"]
            .as_array()
            .unwrap()
            .iter()
            .all(|v| v["proposed_size_pt"] == 18.)
    );
    r["targets"][1]["box"] = json!([100, 250, 8, 8]);
    let out = ctx.execute(r).unwrap();
    assert_eq!(out["group"]["status"], "infeasible");
    assert!(
        out["proposals"]
            .as_array()
            .unwrap()
            .iter()
            .all(|v| v["set"] == json!({}))
    );
    assert_eq!(out["group"]["blocking_objects"], json!(["scene/other"]));
}

#[test]
fn grouped_fit_requires_all_targets_and_cannot_hide_conflicting_size_options() {
    let temp = tempfile::tempdir().unwrap();
    let ctx = fixture(temp.path());
    let mut s = ctx.json("source.json").unwrap();
    s["pages"][0]["typography_groups"] = json!([{"id":"labels","members":["caption","other"],"fields":["size_pt"],"basis":"Equal-level labels."}]);
    ctx.replace_json("source.json", &s).unwrap();
    let r = json!({"action":"fit","spec":"source.json","group":{"page":"scene","id":"labels","size_pt":18},
        "ink_align":"middle","reflow":true,"targets":[{"page":"scene","id":"caption","box":[100,100,500,80]}]});
    assert!(ctx.execute(r.clone()).is_err());
    let mut conflicting = r;
    conflicting["min_size_pt"] = json!(8);
    assert!(ctx.execute(conflicting).is_err());
}

#[test]
fn nested_unicode_splits_preserve_parent_ranges_and_group_membership() {
    let temp = tempfile::tempdir().unwrap();
    let ctx = fixture(temp.path());
    let mut s = ctx.json("source.json").unwrap();
    s["pages"][0]["copy"]["caption"] = json!("花朵 → 城市");
    s["pages"][0]["texts"][0]["text"] = json!("花朵 → 城市");
    s["pages"][0]["typography_groups"] = json!([{"id":"labels","members":["caption","other"],"fields":["size_pt"],"basis":"Equal-level labels."}]);
    ctx.replace_json("source.json", &s).unwrap();
    ctx.execute(json!({"action":"split_text","spec":"source.json","output":"split.json","page":"scene","id":"caption","parts":[
        {"id":"left","text":"花朵","box":[100,100,180,80]},
        {"id":"rest","text":"→ 城市","box":[300,100,280,80]}]})).unwrap();
    ctx.execute(json!({"action":"split_text","spec":"split.json","output":"nested.json","page":"scene","id":"rest","parts":[
        {"id":"arrow","text":"→","box":[300,100,60,80]},
        {"id":"right","text":"城市","box":[400,100,180,80]}]})).unwrap();
    let s = ctx.json("nested.json").unwrap();
    assert_eq!(s["pages"][0]["texts"][2]["copy_range"], json!([4, 7]));
    assert_eq!(
        s["pages"][0]["typography_groups"][0]["members"],
        json!(["left", "arrow", "right", "other"])
    );
    assert!(
        ctx.execute(
            json!({"action":"patch","spec":"nested.json","output":"bad.json","edits":[
        {"page":"scene","id":"left","set":{"text":"Changed"},"copy":"Changed"}]})
        )
        .is_err()
    );
    assert!(!temp.path().join("bad.json").exists());
}

#[test]
fn grouped_proposals_apply_and_export_uniform_sizes_without_losing_other_formatting() {
    let temp = tempfile::tempdir().unwrap();
    let ctx = fixture(temp.path());
    let mut s = ctx.json("source.json").unwrap();
    s["pages"][0]["typography_groups"] = json!([{"id":"labels","members":["caption","other"],"fields":["size_pt"],"basis":"Equal-level labels."}]);
    ctx.replace_json("source.json", &s).unwrap();
    let out=ctx.execute(json!({"action":"fit","spec":"source.json","group":{"page":"scene","id":"labels","size_pt":18},
        "ink_align":"middle","reflow":true,"targets":[
            {"page":"scene","id":"caption","box":[100,100,500,80]},
            {"page":"scene","id":"other","box":[100,250,200,80]}]})).unwrap();
    let edits: Vec<_> = out["proposals"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| json!({"page":row["page"],"id":row["id"],"set":row["set"]}))
        .collect();
    ctx.execute(json!({"action":"patch","spec":"source.json","output":"fit.json","edits":edits}))
        .unwrap();
    ctx.execute(json!({"action":"cycle","spec":"fit.json","output":"cycle","width":800}))
        .unwrap();
    let s = spec::load(&ctx, &ctx.path("fit.json").unwrap(), None).unwrap();
    let style = s.style(&s.pages[0].texts[0]).unwrap();
    assert_eq!(style.align, "center");
    assert_eq!(style.color, "222222");
    let report = ctx.json("cycle/inspection.json").unwrap();
    for object in report["objects"].as_array().unwrap() {
        for run in object["font_runs"].as_array().unwrap() {
            assert_eq!(run["size_pt"], 18.);
        }
    }
}
