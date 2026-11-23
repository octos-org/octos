use octos_pptx::Context;
use serde_json::json;

#[test]
fn line_identity_survives_repair_and_unrepresented_lines_reach_independent_review() {
    let dir = tempfile::tempdir().unwrap();
    let mut maker = Context::new(dir.path()).unwrap();
    maker.continuation_owner = Some("maker".into());
    let mut reviewer = Context::new(dir.path()).unwrap();
    reviewer.continuation_owner = Some("reviewer".into());
    image::RgbImage::from_pixel(320, 160, image::Rgb([236, 236, 236]))
        .save(dir.path().join("cover.png"))
        .unwrap();
    let digest = maker.binding("cover.png").unwrap()["sha256"].clone();
    let source = json!({"source_sha256":digest,"canvas":[320,160],"lines":[
        {"id":"t07","text":"Agent0S","box":[10,10,58,16]},
        {"id":"t08","text":"UI","box":[100,20,22,14]},
        {"id":"t09","text":"UI","box":[160,20,22,14]},
        {"text":"Lost","box":[90,110,25,12]}]});
    maker.write_json("raw.json", &source).unwrap();
    let intake = json!({"action":"source_lines","page":"p","design":"cover.png","ocr":"raw.json"});
    let first = maker.execute(intake.clone()).unwrap();
    let orphan = first["lines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["text"] == "Lost")
        .unwrap()["id"]
        .clone();
    let mut reordered = source.clone();
    reordered["lines"].as_array_mut().unwrap().reverse();
    maker.replace_json("raw.json", &reordered).unwrap();
    assert_eq!(first, maker.execute(intake.clone()).unwrap());
    let spec = json!({"schema_version":1,"canvas":{"width_px":320,"height_px":160,"width_in":6.4,"height_in":3.2},
        "styles":{"b":{"font":"Inter","size_pt":12}},"pages":[{"id":"p","design":"cover.png","background":"cover.png",
        "copy":{"a":"Agent0S","x":"UI","y":"UI"},"texts":[{"id":"a","text":"Agent0S","style":"b","box":[20,15,85,20]},
        {"id":"x","text":"UI","source_line_ids":["t08"],"style":"b","box":[100,20,30,18]},
        {"id":"y","text":"UI","source_line_ids":["t09"],"style":"b","box":[160,20,30,18]}]}]});
    maker.write_json("raw-spec.json", &spec).unwrap();
    let mut draft = spec.clone();
    draft["pages"][0].as_object_mut().unwrap().remove("design");
    maker.write_json("no-design.json", &draft).unwrap();
    maker
        .execute(json!({"action":"build","spec":"no-design.json","output":"restored"}))
        .unwrap();
    assert_eq!(
        maker.json("restored/cycle.json").unwrap()["source_lines"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
    let cycle =
        |spec: &str, out: &str| json!({"action":"cycle","spec":spec,"output":out,"width":640});
    maker.execute(cycle("raw-spec.json", "before")).unwrap();
    maker.execute(json!({"action":"patch","spec":"raw-spec.json","edits":[{"page":"p","id":"a","copy":"AgentOS","set":{"text":"AgentOS"}}],"output":"corrected.json"})).unwrap();
    maker.execute(json!({"action":"split_text","spec":"corrected.json","output":"split.json","page":"p","id":"a","parts":[
        {"id":"left","text":"Agent","box":[20,15,45,20]},{"id":"right","text":"OS","box":[65,15,40,20]}]})).unwrap();
    maker
        .execute(json!({"action":"build","spec":"split.json","output":"fragments"}))
        .unwrap();
    let fragments = maker.json("fragments/cycle.json").unwrap();
    assert_eq!(
        fragments["source_lines"]
            .as_array()
            .unwrap()
            .iter()
            .find(|line| line["source_line_id"] == "t07")
            .unwrap()["objects"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let mut corrected = maker.json("corrected.json").unwrap();
    assert_eq!(
        corrected["pages"][0]["texts"][0]["source_line_ids"],
        json!(["t07"])
    );
    corrected["pages"][0]["texts"][0]["id"] = json!("renamed");
    corrected["pages"][0]["texts"][0]["copy_id"] = json!("a");
    maker.write_json("renamed.json", &corrected).unwrap();
    maker.execute(cycle("renamed.json", "fixed")).unwrap();
    let receipt = maker.json("fixed/cycle.json").unwrap();
    let lines = receipt["source_lines"].as_array().unwrap();
    assert_eq!(lines.len(), 4);
    assert_eq!(
        lines.iter().find(|l| l["source_line_id"] == "t07").unwrap()["objects"],
        json!([{"id":"renamed","text":"AgentOS"}])
    );
    let ui = |key: &str| {
        lines.iter().find(|l| l["source_line_id"] == key).unwrap()["objects"][0]["id"].clone()
    };
    assert_eq!(ui("t08"), "x");
    assert_eq!(ui("t09"), "y");
    let mut task = json!({"action":"evidence_init","stagnant_attempts":3,"task_id":"fixture","deliverables":["fixed/editable.pptx"],"verification_mode":"independent_visual","reviewer_agent":"reviewer"});
    task["owner_agent"] = json!("maker");
    maker.execute(task).unwrap();
    let pack = reviewer
        .execute(json!({"action":"review_pack","cycle":"fixed/cycle.json","output":"pack"}))
        .unwrap();
    let mut obs = reviewer
        .json(reviewer.verify(&pack["object_review_template"]).unwrap())
        .unwrap();
    let submit = |name: &str| json!({"action":"review_submit","pack":"pack/report.json","observations":"obs.json","output":name});
    for row in obs["source_line_reviews"].as_array_mut().unwrap() {
        row["status"] = json!("native");
        row["observation"] = json!("Synthetic fixture observation");
    }
    maker.write_json("obs.json", &obs).unwrap();
    let invalid = reviewer.execute(submit("invalid.json")).unwrap_err();
    assert!(
        invalid.to_string().contains("no linked editable text"),
        "{invalid:#}"
    );
    let absent = obs["source_line_reviews"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|r| r["source_line_id"] == orphan)
        .unwrap();
    absent["status"] = json!("defect");
    maker.replace_json("obs.json", &obs).unwrap();
    reviewer.execute(submit("signed.json")).unwrap();
    let result = maker
        .execute(
            json!({"action":"evidence_record","cycle":"fixed/cycle.json","review":"signed.json"}),
        )
        .unwrap();
    assert_eq!(
        result["source_line_coverage"],
        json!({"total":4,"defects":1,"unknown":0,"pending_ids":[["p",orphan]]})
    );
    assert_eq!(
        result["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == "copy")
            .unwrap()["status"],
        "fail"
    );
    // Even a fully populated draft cannot sign itself as the independent peer.
    maker.execute(submit("self.json")).unwrap();
    let self_review = maker
        .execute(
            json!({"action":"evidence_record","cycle":"fixed/cycle.json","review":"self.json"}),
        )
        .unwrap();
    assert_eq!(self_review["source_line_coverage"]["unknown"], 3);
    let mut extra = intake.clone();
    extra["additional_lines"] = json!([{"text":"Missed by OCR","box":[180,80,50,14]}]);
    let five = reviewer.execute(extra).unwrap();
    assert_eq!(five["lines"].as_array().unwrap().len(), 5);
    let stale = maker.execute(
        json!({"action":"evidence_record","cycle":"fixed/cycle.json","review":"signed.json"}),
    );
    assert!(
        stale
            .unwrap_err()
            .to_string()
            .contains("absent from this candidate")
    );
    let mut different = source.clone();
    different["lines"][0]["text"] = json!("replace source observation");
    maker.replace_json("raw.json", &different).unwrap();
    assert!(maker.execute(intake).is_err());
    let mut missing = corrected;
    missing["pages"][0]["texts"][1]["source_line_ids"] = json!(["no-such-id"]);
    maker.write_json("bad.json", &missing).unwrap();
    assert!(maker.execute(cycle("bad.json", "bad")).is_err());
}
