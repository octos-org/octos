use serde_json::json;
mod common;

#[test]
fn legacy_cycles_need_real_reports_and_no_reference_decks_do_not_invent_them() {
    let (_dir, ctx) = common::labeled_fixture("synthetic-coverage", "page-0", "test");
    common::edit_spec(&ctx, |spec| {
        spec["pages"][0]["design"] = json!("blank.png");
    });
    common::cycle(&ctx, "a");
    let mut receipt = ctx.json("a/cycle.json").unwrap();
    receipt.as_object_mut().unwrap().remove("comparisons");
    // Emulate an old persisted cycle; never mutate a production receipt.
    ctx.replace_json("a/cycle.json", &receipt).unwrap();
    assert_eq!(
        common::record(&ctx, "a", json!([]), None, json!([]), false).unwrap()["comparison_coverage"]
            ["pages"][0]["status"],
        "missing_comparison"
    );
    ctx.execute(json!({"action":"compare_cycle","cycle":"a/cycle.json","output":"retrofit"}))
        .unwrap();
    let comparisons = common::accept(&ctx, "retrofit/manifest.json");
    let manifest = Some("retrofit/manifest.json");
    let result =
        common::record(&ctx, "a", comparisons.clone(), manifest, json!([]), false).unwrap();
    assert_eq!(result["checks"][3]["status"], "pass", "{result}");

    // Retrofitting does not make a subsequently changed report authoritative.
    let report = ctx.json("retrofit/manifest.json").unwrap()["pages"][0]["report"].clone();
    std::fs::write(ctx.verify(&report).unwrap(), b"changed report").unwrap();
    assert!(common::record(&ctx, "a", comparisons, manifest, json!([]), false).is_err());

    let (_dir, native) = common::labeled_fixture("native-coverage", "page-0", "test");
    common::cycle(&native, "native");
    let status =
        native.json("native/comparisons/manifest.json").unwrap()["pages"][0]["status"].clone();
    let recorded = common::record(&native, "native", json!([]), None, json!([]), false).unwrap();
    assert!(status == "no_reference" && recorded["checks"][3]["status"] == "pass");
}
