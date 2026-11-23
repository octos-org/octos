//! Exercise the frozen roster and the real publish boundary, including post-build artifacts.
use octos_pptx::Context;
use serde_json::{Value, json};

fn setup(root: &std::path::Path, explicit: Option<Value>) -> Context {
    let mut ctx = Context::new(root).unwrap();
    image::RgbImage::from_pixel(400, 220, image::Rgb([230, 230, 230]))
        .save(root.join("art.png"))
        .unwrap();
    let mut row = json!({"id":"instrument","maker_session":"maker","reviewer_session":"peer","candidate":"candidate.json","review":"review.json","spec":"deck.json"});
    if let Some(copy) = explicit {
        row["editable_copy"] = copy;
    } else {
        ctx.write_json(
            "lines.json",
            &json!({"lines":[{"id":"reading","text":"Carbon 14"},{"id":"noise","text":"I0O"}]}),
        )
        .unwrap();
        let mut bound = ctx.binding("lines.json").unwrap();
        bound["role"] = json!("measurements");
        row["inputs"] = json!([bound]);
    }
    ctx.write_json(".octos/pptx-page-plan.json", &json!({"schema_version":2,"coordinator_session":"lead","rubric":{"content":100},"pages":[row]})).unwrap();
    ctx.continuation_owner = Some("lead".into());
    ctx.execute(json!({"action":"evidence_init","verification_mode":"per_page","task_id":"test-copy","stagnant_attempts":2,"deliverables":["final.pptx"]})).unwrap();
    ctx.write_json("deck.json",&json!({"schema_version":1,"canvas":{"width_px":400,"height_px":220,"width_in":4,"height_in":2.2},"styles":{"regular":{"font":"Inter","size_pt":12}},"pages":[{"path":"instrument.json"}]})).unwrap();
    ctx.continuation_owner = Some("maker".into());
    ctx
}

fn draft(ctx: &Context, words: &[(&str, &str)]) {
    let mut copy = json!({});
    let mut texts = vec![];
    for (i, (id, text)) in words.iter().enumerate() {
        copy[id] = json!(text);
        texts.push(json!({"id":id,"text":text,"style":"regular","box":[5,5+i*40,380,34]}));
    }
    ctx.replace_json("instrument.json",&json!({"id":"instrument","background":"art.png","notes":"Carbon 14","copy":copy,"texts":texts})).unwrap();
}

fn cycle(ctx: &Context, output: &str) -> Value {
    ctx.execute(json!({"action":"cycle","spec":"deck.json","width":400,"output":output}))
        .unwrap()
}
fn publish(ctx: &Context, output: &str) -> Value {
    json!({"action":"page_publish","page":"instrument","cycle":ctx.path(format!("{output}/cycle.json")).unwrap()})
}

#[test]
fn original_copy_handles_renames_merged_boxes_and_repeated_occurrences() {
    let temp = tempfile::tempdir().unwrap();
    let ctx = setup(
        temp.path(),
        Some(json!({"sample":"Carbon 14","first":"温度","second":"温度"})),
    );
    draft(&ctx, &[("new-shape", "Carbon 14 / 温度")]);
    let direct = ctx.execute(json!({"action":"cycle","publish_candidate":true,"spec":"deck.json","output":"early","width":400})).unwrap_err();
    assert!(direct.to_string().contains("温度"));
    assert!(!ctx.path("early").unwrap().exists());
    let result = cycle(&ctx, "one");
    assert!(result["peer_review_handoff"].is_null());
    assert_eq!(result["missing_editable_copy"], json!({"second":"温度"}));
    let error = ctx.execute(publish(&ctx, "one")).unwrap_err().to_string();
    assert!(
        error.contains("second") && !ctx.path("candidate.json").unwrap().exists(),
        "{error}"
    );
    draft(&ctx, &[("joined", "Carbon\n14 / 温度 / 温度")]);
    cycle(&ctx, "two");
    assert_eq!(
        ctx.execute(publish(&ctx, "two")).unwrap()["ready_for_independent_review"],
        true
    );
}

#[test]
fn confirmed_measurements_survive_deleted_copy_and_notes_cannot_replace_a_shape() {
    let temp = tempfile::tempdir().unwrap();
    let ctx = setup(temp.path(), None);
    draft(&ctx, &[("reading", "Carbon 14"), ("noise", "100")]);
    ctx.execute(json!({"action":"build","spec":"deck.json","output":"private-draft"}))
        .unwrap();
    draft(&ctx, &[("other", "Carbon 140")]);
    cycle(&ctx, "dropped");
    let error = ctx
        .execute(publish(&ctx, "dropped"))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("reading")
            && !error.contains("noise")
            && !ctx.path("candidate.json").unwrap().exists(),
        "{error}"
    );
    draft(&ctx, &[("renamed", "Carbon 14")]);
    cycle(&ctx, "restored");
    assert_eq!(
        ctx.execute(publish(&ctx, "restored")).unwrap()["ready_for_independent_review"],
        true
    );
}

#[test]
fn validated_split_can_reorder_objects_but_actual_postbuild_text_must_survive() {
    let temp = tempfile::tempdir().unwrap();
    let ctx = setup(temp.path(), Some(json!({"term":"calibration"})));
    draft(&ctx, &[("term", "calibration")]);
    let mut page = ctx.json("instrument.json").unwrap();
    page["texts"] = json!([
        {"id":"tail","copy_id":"term","copy_range":[4,11],"text":"bration","style":"regular","box":[50,5,180,34]},
        {"id":"head","copy_id":"term","copy_range":[0,4],"text":"cali","style":"regular","box":[5,5,45,34]}
    ]);
    ctx.replace_json("instrument.json", &page).unwrap();
    cycle(&ctx, "split");
    let good = ctx.execute(publish(&ctx, "split")).unwrap();
    assert_eq!(good["ready_for_independent_review"], true);
    let path = ctx.path("split/editable.pptx").unwrap();
    let doc = octos_pptx::deck::open(&ctx, &path).unwrap();
    let tail = doc.slides[0]
        .shapes
        .iter()
        .find(|s| s.name == "octos:text:tail")
        .unwrap();
    let reply=ctx.execute(json!({"action":"batch","input":path,"output":"changed.pptx","commands":[{"command":"text.edit","params":{"id":tail.id.0}},{"command":"text.set","params":{"text":"X"}}]})).unwrap();
    ctx.write_json("batch.json", &reply).unwrap();
    ctx.execute(json!({"action":"render","input":"changed.pptx","output":"changed","width":400}))
        .unwrap();
    let mut request = publish(&ctx, "split");
    request["receipts"] = json!(["batch.json"]);
    request["artifact"] = json!("changed.pptx");
    request["render_receipt"] = json!("changed/render.json");
    let error = ctx.execute(request).unwrap_err().to_string();
    assert!(error.contains("calibration"), "{error}");
    assert_eq!(ctx.binding("candidate.json").unwrap(), good["candidate"]);
}

#[test]
fn postbuild_table_can_supply_copy_missing_from_the_draft() {
    let temp = tempfile::tempdir().unwrap();
    let ctx = setup(temp.path(), Some(json!({"unit":"µmol/L"})));
    draft(&ctx, &[("heading", "Concentration")]);
    let draft = cycle(&ctx, "base");
    assert_eq!(draft["missing_editable_copy"], json!({"unit":"µmol/L"}));
    let reply = ctx
        .execute(json!({
            "action":"batch","input":"base/editable.pptx","output":"table.pptx","commands":[
                {"command":"insert.table","params":{"rows":1,"cols":1,"rect":[10,30,120,45]}},

                {"command":"text.set","params":{"cell":[0,0],"text":"µmol/L"}}
            ]
        }))
        .unwrap();
    ctx.write_json("table-receipt.json", &reply).unwrap();
    ctx.execute(
        json!({"action":"render","input":"table.pptx","output":"table-render","width":400}),
    )
    .unwrap();
    let mut request = publish(&ctx, "base");
    request["receipts"] = json!(["table-receipt.json"]);
    request["artifact"] = json!("table.pptx");
    request["render_receipt"] = json!("table-render/render.json");
    assert_eq!(
        ctx.execute(request).unwrap()["ready_for_independent_review"],
        true
    );
}

#[test]
fn first_publication_from_a_prebuilt_page_also_pins_confirmed_copy() {
    let temp = tempfile::tempdir().unwrap();
    let mut ctx = setup(temp.path(), None);
    ctx.continuation_owner = None;
    draft(&ctx, &[("reading", "Carbon 14")]);
    cycle(&ctx, "prebuilt");
    assert!(!ctx.path(".octos/page-editable-copy").unwrap().exists());
    ctx.continuation_owner = Some("maker".into());
    let original = ctx.execute(publish(&ctx, "prebuilt")).unwrap();
    draft(&ctx, &[("notes-label", "Notes")]);
    let attempt = cycle(&ctx, "next");
    assert_eq!(
        attempt["missing_editable_copy"],
        json!({"reading":"Carbon 14"})
    );
    assert!(
        ctx.execute(publish(&ctx, "next"))
            .unwrap_err()
            .to_string()
            .contains("reading")
    );
    assert_eq!(
        ctx.binding("candidate.json").unwrap(),
        original["candidate"]
    );
}

#[test]
fn merging_labels_preserves_distinct_occurrences_with_overlapping_phrases() {
    let temp = tempfile::tempdir().unwrap();
    let ctx = setup(
        temp.path(),
        Some(json!({"a-material":"Copper foil","b-process":"foil heat"})),
    );
    draft(
        &ctx,
        &[
            ("label-one", "Copper foil heat"),
            ("label-two", "Copper foil sample"),
        ],
    );
    let result = cycle(&ctx, "merged");
    assert_eq!(result["missing_editable_copy"], json!({}));
    assert_eq!(
        ctx.execute(publish(&ctx, "merged")).unwrap()["ready_for_independent_review"],
        true
    );
}
