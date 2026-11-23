use octos_pptx::Context;
use serde_json::{Value, json};
use std::path::Path;

const PLAN: &str = ".octos/pptx-page-plan.json";
fn setup(root: &Path, ids: &[&str]) -> Context {
    let mut ctx = Context::new(root).unwrap();
    image::RgbImage::new(120, 80)
        .save(root.join("original.png"))
        .unwrap();
    let reference = ctx.binding("original.png").unwrap();
    let rows: Vec<_>=ids.iter().map(|id|json!({"id":id,"maker_session":format!("make-{id}"),"reviewer_session":format!("check-{id}"),"reference":reference,"candidate":format!("{id}-candidate.json"),"review":format!("{id}-review.json"),"editable_copy":{"one":"low","unit":"mm","noise":"o"}})).collect();
    ctx.write_json(PLAN,&json!({"schema_version":2,"coordinator_session":"lead","rubric":{"source":100},"pages":rows})).unwrap();
    ctx.continuation_owner = Some("lead".into());
    ctx
}
fn peer(ctx: &Context, owner: &str) -> Context {
    let mut other = Context::new(&ctx.root).unwrap();
    other.continuation_owner = Some(owner.to_owned());
    other
}
fn init(ctx: &Context) -> anyhow::Result<Value> {
    ctx.execute(json!({"action":"evidence_init","task_id":"source-inventory","verification_mode":"per_page","stagnant_attempts":2,"deliverables":["final.pptx"]}))
}
fn open(ctx: &Context, id: &str) -> Value {
    ctx.execute(json!({"action":"source_copy_review","page":id}))
        .unwrap()
}
fn submit(ctx: &Context, id: &str, token: &Value, file: &str) -> anyhow::Result<Value> {
    ctx.execute(json!({"action":"source_copy_review","page":id,"expected_sha256":token["expected_sha256"],"report":file}))
}
fn report() -> Value {
    json!({"confirmed_ids":["one"],"regions":[{"location":"keys beneath chart","observation":"two separate low settings and one unit","labels":["low","low","µm"]}],"changes":[
        {"action":"add","id":"second","text":"low","reason":"Separate setting on the right."},
        {"action":"replace","id":"unit","text":"µm","reason":"Axis shows micro."},
        {"action":"exclude","id":"noise","reason":"The ring is a printed marker."}
    ],"unresolved":[]})
}

#[test]
fn reconstruction_starts_its_makers_without_a_source_review() {
    let tmp = tempfile::tempdir().unwrap();
    let ctx = setup(tmp.path(), &["chart", "photo"]);
    let mut plan = ctx.json(PLAN).unwrap();
    plan["pages"][1].as_object_mut().unwrap().remove("editable_copy");
    ctx.replace_json(PLAN, &plan).unwrap();
    init(&ctx).unwrap();
    ctx.execute(json!({"action":"page_assignments"})).unwrap();
    let batch = ctx.json(".octos/pptx-peer-batch.json").unwrap();
    let jobs = batch["jobs"].as_array().unwrap();
    assert_eq!(jobs.len(), 2);
    for (job, row) in jobs.iter().zip(plan["pages"].as_array().unwrap()) {
        assert_eq!(job["phase"], "make");
        assert_eq!(job["peer"], row["maker_session"]);
    }
    assert_eq!(ctx.json(PLAN).unwrap(), plan);
}

#[test]
fn resolved_source_changes_reach_assignment_and_real_candidate_checks() {
    let tmp = tempfile::tempdir().unwrap();
    let ctx = setup(tmp.path(), &["chart"]);
    let reviewer = peer(&ctx, "session:check-chart");
    let token = open(&reviewer, "chart");
    ctx.write_json("source.json", &report()).unwrap();
    let signed = submit(&reviewer, "chart", &token, "source.json").unwrap();
    assert_eq!(signed["ready_to_freeze"], true);
    assert_eq!(
        ctx.json(PLAN).unwrap()["pages"][0]["editable_copy"],
        json!({"one":"low","second":"low","unit":"µm"})
    );
    assert_eq!(
        submit(&reviewer, "chart", &token, "source.json").unwrap(),
        signed
    );
    init(&ctx).unwrap();
    assert_eq!(
        submit(&reviewer, "chart", &token, "source.json").unwrap(),
        signed
    );
    ctx.execute(json!({"action":"page_assignments"})).unwrap();
    let batch = ctx.json(".octos/pptx-peer-batch.json").unwrap();
    for job in batch["jobs"].as_array().unwrap() {
        let data: Value = serde_json::from_str(
            job["message"]
                .as_str()
                .unwrap()
                .split_once("Page assignment\n")
                .unwrap()
                .1,
        )
        .unwrap();
        assert_eq!(data["editable_copy"]["second"], "low");
    }
    let maker = peer(&ctx, "make-chart");
    ctx.write_json("chart.json",&json!({"id":"chart","background":"original.png","copy":{"one":"low","unit":"µm"},"texts":[
        {"id":"one","text":"low","style":"t","box":[0,0,50,20]},{"id":"unit","text":"µm","style":"t","box":[55,10,50,20]}]})).unwrap();
    ctx.write_json("deck.json",&json!({"schema_version":1,"canvas":ctx.json(".octos/pptx-verification-plan.json").unwrap()["canvas"],"styles":{"t":{"font":"Inter","size_pt":10}},"pages":[{"path":"chart.json"}]})).unwrap();
    let e=maker.execute(json!({"action":"cycle","publish_candidate":true,"spec":"deck.json","output":"bad","width":120})).unwrap_err().to_string();
    assert!(
        e.contains("required editable text") && e.contains("low"),
        "{e}"
    );
    assert!(!ctx.path("chart-candidate.json").unwrap().exists());
    let mut repaired = ctx.json("chart.json").unwrap();
    repaired["copy"]["second"] = json!("low");
    repaired["texts"]
        .as_array_mut()
        .unwrap()
        .push(json!({"id":"second","text":"low","style":"t","box":[5,35,50,20]}));
    ctx.replace_json("chart.json", &repaired).unwrap();
    let submitted=maker.execute(json!({"action":"cycle","publish_candidate":true,"spec":"deck.json","output":"restored","width":120})).unwrap();
    assert_eq!(
        submitted["page_publication"]["ready_for_independent_review"],
        true
    );
    assert_eq!(submitted["quality_approved"], false);
}

#[test]
fn incomplete_reports_block_freeze_cannot_be_replayed_over_new_evidence() {
    let tmp = tempfile::tempdir().unwrap();
    let ctx = setup(tmp.path(), &["legend"]);
    let reviewer = peer(&ctx, "check-legend");
    let first = open(&reviewer, "legend");
    let mut pending = report();
    pending["changes"] = json!([]);
    pending["unresolved"] = json!(["At right edge the letter is blurred"]);
    ctx.write_json("pending.json", &pending).unwrap();
    let missing = submit(&reviewer, "legend", &first, "pending.json").unwrap();
    assert_eq!(missing["ready_to_freeze"], false);
    assert_eq!(missing["automatic_additions"].as_array().unwrap().len(), 2);
    assert_eq!(missing["unresolved"].as_array().unwrap().len(), 1);
    let e = init(&ctx).unwrap_err().to_string();
    assert!(e.contains("blurred") && e.contains("unit"), "{e}");
    ctx.write_json("resolved.json", &report()).unwrap();
    assert!(
        submit(&reviewer, "legend", &first, "resolved.json")
            .unwrap_err()
            .to_string()
            .contains("stale")
    );
    let next = open(&reviewer, "legend");
    submit(&reviewer, "legend", &next, "resolved.json").unwrap();
    assert!(
        submit(&reviewer, "legend", &first, "pending.json")
            .unwrap_err()
            .to_string()
            .contains("stale")
    );
    let mut row = ctx.json(PLAN).unwrap();
    row["pages"][0]["editable_copy"]["second"] = json!("changed");
    ctx.replace_json(PLAN, &row).unwrap();
    assert!(init(&ctx).unwrap_err().to_string().contains("changed"));
}

#[test]
fn only_assigned_reviewer_can_amend_and_reference_and_receipt_are_pinned() {
    let tmp = tempfile::tempdir().unwrap();
    let mut ctx = setup(tmp.path(), &["plot", "key"]);
    let reviewer = peer(&ctx, "check-plot");
    let token = open(&reviewer, "plot");
    ctx.write_json("observed.json", &report()).unwrap();
    for role in ["lead", "make-plot", "check-key"] {
        assert!(submit(&peer(&ctx, role), "plot", &token, "observed.json").is_err());
    }
    ctx.continuation_owner = None;
    assert!(submit(&ctx, "plot", &token, "observed.json").is_err());
    let signed = submit(&reviewer, "plot", &token, "observed.json").unwrap();
    let archive = ctx.verify(&signed["receipt"]).unwrap();
    let bytes = ctx.read(&archive).unwrap();
    ctx.replace_json(&archive, &json!({"fake":true})).unwrap();
    let lead = peer(&ctx, "lead");
    assert!(
        init(&lead)
            .unwrap_err()
            .to_string()
            .contains("receipt changed")
    );
    std::fs::write(archive, bytes).unwrap();
    image::RgbImage::from_pixel(120, 80, image::Rgb([255, 255, 255]))
        .save(tmp.path().join("original.png"))
        .unwrap();
    assert!(
        init(&lead)
            .unwrap_err()
            .to_string()
            .contains("source image changed")
    );
}

#[test]
fn parallel_page_submissions_preserve_both_rows() {
    let tmp = tempfile::tempdir().unwrap();
    let ctx = setup(tmp.path(), &["east", "west"]);
    ctx.write_json("report.json", &report()).unwrap();
    let barrier = std::sync::Barrier::new(2);
    std::thread::scope(|s| {
        for id in ["east", "west"] {
            let who = peer(&ctx, &format!("check-{id}"));
            let b = &barrier;
            s.spawn(move || {
                let token = open(&who, id);
                b.wait();
                assert_eq!(
                    submit(&who, id, &token, "report.json").unwrap()["ready_to_freeze"],
                    true
                );
            });
        }
    });
    init(&ctx).unwrap();
    let rows = ctx.json(PLAN).unwrap();
    for row in rows["pages"].as_array().unwrap() {
        ctx.verify(&row["source_copy_review"]).unwrap();
        assert_eq!(row["editable_copy"]["second"], "low");
    }
}

#[test]
fn standalone_symbols_are_added_once_per_occurrence_without_a_second_instruction() {
    let tmp = tempfile::tempdir().unwrap();
    let ctx = setup(tmp.path(), &["map"]);
    let who = peer(&ctx, "check-map");
    let token = open(&who, "map");
    let q = json!({"regions":[{"location":"two compass roses","observation":"Each rose has a north label.","labels":["N","N"]}],
        "confirmed_ids":["one","unit","noise"],"changes":[],"unresolved":[],"notes":["Type size differs between the roses."]});
    ctx.write_json("map-source.json", &q).unwrap();
    let receipt = submit(&who, "map", &token, "map-source.json").unwrap();
    assert_eq!(receipt["ready_to_freeze"], true);
    assert_eq!(receipt["automatic_additions"].as_array().unwrap().len(), 2);
    assert_ne!(
        receipt["automatic_additions"][0]["id"],
        receipt["automatic_additions"][1]["id"]
    );
    assert_eq!(
        submit(&who, "map", &token, "map-source.json").unwrap(),
        receipt
    );
    let next = open(&who, "map");
    let verified = ctx.json(PLAN).unwrap()["pages"][0]["editable_copy"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    let mut all = q;
    all["confirmed_ids"] = json!(verified);
    ctx.write_json("second-review.json", &all).unwrap();
    assert_eq!(
        submit(&who, "map", &next, "second-review.json").unwrap()["automatic_additions"],
        json!([])
    );
    init(&ctx).unwrap();
}

#[test]
fn unverified_existing_entries_block_even_when_the_independent_list_appears_complete() {
    let tmp = tempfile::tempdir().unwrap();
    let ctx = setup(tmp.path(), &["floor"]);
    let who = peer(&ctx, "check-floor");
    let token = open(&who, "floor");
    ctx.write_json(
        "floor-source.json",
        &json!({"regions":[{"location":"top","observation":"One key","labels":["low"]}],
        "confirmed_ids":["one","unit"],"changes":[],"unresolved":[]}),
    )
    .unwrap();
    let result = submit(&who, "floor", &token, "floor-source.json").unwrap();
    assert_eq!(result["unconfirmed_ids"], json!(["noise"]));
    assert_eq!(result["ready_to_freeze"], false);
    assert!(init(&ctx).unwrap_err().to_string().contains("noise"));
}

#[test]
fn independently_authored_copy_can_use_a_design_image_for_style() {
    let tmp = tempfile::tempdir().unwrap();
    let ctx = setup(tmp.path(), &["story"]);
    std::fs::write(tmp.path().join("author.txt"), "low mm o").unwrap();
    let mut binding = ctx.binding("author.txt").unwrap();
    binding["role"] = json!("canonical-copy");
    let mut plan = ctx.json(PLAN).unwrap();
    plan["pages"][0]["inputs"] = json!([binding]);
    ctx.replace_json(PLAN, &plan).unwrap();
    init(&ctx).unwrap();
}

#[test]
fn a_text_document_reference_keeps_existing_authored_workflow() {
    let tmp = tempfile::tempdir().unwrap();
    let ctx = setup(tmp.path(), &["abstract"]);
    std::fs::write(tmp.path().join("source.md"), "low mm o").unwrap();
    let mut plan = ctx.json(PLAN).unwrap();
    plan["pages"][0]["reference"] = ctx.binding("source.md").unwrap();
    ctx.replace_json(PLAN, &plan).unwrap();
    init(&ctx).unwrap();
}
