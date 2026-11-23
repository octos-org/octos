use octos_pptx::{Context, fs::sha_file};
use serde_json::{Value, json};

#[test]
fn build_anchor_survives_repairs_but_ignores_ambiguous_or_rejected_ocr() {
    let root = tempfile::tempdir().unwrap();
    let ctx = Context::new(root.path()).unwrap();
    image::RgbImage::from_pixel(640, 320, image::Rgb([224, 223, 220]))
        .save(root.path().join("source.png"))
        .unwrap();
    let source_hash = sha_file(&root.path().join("source.png")).unwrap();
    let make_line = |id: &str, text: &str, x: f64| json!({"id":id,"text":text,"box":[x,100,56,24]});
    let mut rejected = make_line("t04", "Draft", 315.);
    rejected["geometry_status"] = json!("rejected");
    let mut weak = make_line("t05", "Faint", 375.);
    weak["confidence"] = json!(0.5);
    let ocr = json!({"canvas":[640,320],"source_sha256":source_hash,"lines":[
        {"id":"t01","text":"Lens","box":[310,110,54,44],"polygon":[[310,134],[350,110],[364,130],[324,154]]},
        make_line("t02","Part",100.),make_line("t03","Part",200.),rejected,weak]});
    ctx.write_json("ocr.json", &ocr).unwrap();
    let names = [
        ("unique", "Lens"),
        ("dup", "Part"),
        ("rejected", "Draft"),
        ("weak", "Faint"),
    ];
    let mut copy = json!({});
    let mut texts = vec![];
    for (i, (id, s)) in names.into_iter().enumerate() {
        copy[id] = json!(s);
        texts.push(json!({"id":id,"text":s,"style":"body","box":[30,15+i*65,160,40]}));
    }
    let spec = json!({"schema_version":1,"canvas":{"width_px":640,"height_px":320,"width_in":6.4,"height_in":3.2},
        "styles":{"body":{"font":"Inter","size_pt":12}},"pages":[{"id":"p","design":"source.png","background":"source.png","copy":copy,"texts":texts}]});
    ctx.write_json("spec.json", &spec).unwrap();
    let build = |spec: &str, output: &str| {
        ctx.execute(json!({"action":"build","spec":spec,"output":output}))
    };
    build("spec.json", "first").unwrap();
    let receipt = ctx.json("first/cycle.json").unwrap();
    let changes = receipt["ocr_placement"].as_array().unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0]["text_id"], "unique");
    assert_eq!(changes[0]["source_ink_center"], json!([337., 132.]));
    assert_eq!(
        changes[0]["source_line"]["polygon"],
        ocr["lines"][0]["polygon"]
    );
    let resolved = ctx.json("first/resolved-spec.json").unwrap();
    let before = &spec["pages"][0]["texts"][0];
    let after = &resolved["pages"][0]["texts"][0];
    assert_eq!(before["text"], after["text"]);
    assert_eq!(before["box"][2].as_f64(), after["box"][2].as_f64());
    assert_eq!(before["box"][3].as_f64(), after["box"][3].as_f64());
    assert!((after["rotation"].as_f64().unwrap() + 30.96).abs() < 0.1);
    build("first/resolved-spec.json", "repeat").unwrap();
    assert!(
        ctx.json("repeat/cycle.json")
            .unwrap()
            .get("ocr_placement")
            .is_none()
    );
    let mut stale = ocr.clone();
    stale["source_sha256"] = Value::String("wrong-source".into());
    ctx.replace_json("ocr.json", &stale).unwrap();
    build("spec.json", "stale-auto").unwrap();
    assert!(
        ctx.json("stale-auto/cycle.json")
            .unwrap()
            .get("ocr_placement")
            .is_none()
    );
    let mut explicit = spec;
    explicit["pages"][0]["source_ocr"] = json!("ocr.json");
    ctx.write_json("explicit.json", &explicit).unwrap();
    assert!(build("explicit.json", "stale-explicit").is_err());
}
