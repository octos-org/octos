use octos_pptx::{Context, fs::sha_file};
use serde_json::{Value, json};
mod common;

fn fixture() -> (tempfile::TempDir, Context) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Context::new(dir.path()).unwrap();
    let mut art = image::RgbImage::from_pixel(640, 360, image::Rgb([255, 255, 255]));
    for x in 40..=400 {
        for dy in 0..3 {
            art.put_pixel(x, 40 + dy, image::Rgb([20, 20, 20]));
            art.put_pixel(x, 200 - dy, image::Rgb([20, 20, 20]));
        }
    }
    for y in 40..=200 {
        for dx in 0..3 {
            art.put_pixel(40 + dx, y, image::Rgb([20, 20, 20]));
            art.put_pixel(400 - dx, y, image::Rgb([20, 20, 20]));
        }
    }
    art.save(dir.path().join("art.png")).unwrap();
    ctx.write_json(
        "deck.json",
        &json!({
            "schema_version":1,
            "canvas":{"width_px":640,"height_px":360,"width_in":12.8,"height_in":7.2},
            "styles":{"body":{"font":"Inter","size_pt":18}},
            "pages":[{"id":"overview","background":"art.png",
                "copy":{"caption":"Example label"},
                "regions":{"card":{"quad":[[40,40],[400,40],[400,200],[40,200]],
                    "scope":"label","basis":"Independent synthetic card"}},
                "texts":[{"id":"caption","text":"Example label","style":"body",
                    "box":[60,60,280,80],"region":"card"}]}]
        }),
    )
    .unwrap();
    ctx.execute(
        json!({"action":"evidence_init","task_id":"generic-qa","owner_agent":"maker",
        "stagnant_attempts":3,"deliverables":["final.pptx"]}),
    )
    .unwrap();
    (dir, ctx)
}
fn change(ctx: &Context, f: impl FnOnce(&mut Value)) {
    let mut s = ctx.json("deck.json").unwrap();
    f(&mut s);
    ctx.replace_json("deck.json", &s).unwrap();
}
fn cycle(ctx: &Context, name: &str) {
    ctx.execute(json!({"action":"cycle","spec":"deck.json","output":name,"width":640}))
        .unwrap();
    std::fs::copy(
        ctx.root.join(name).join("editable.pptx"),
        ctx.root.join("final.pptx"),
    )
    .unwrap();
}
fn claim(ctx: &Context, name: &str) -> Value {
    let c = ctx.json(format!("{name}/cycle.json")).unwrap();
    let r = json!({"cycle_sha256":sha_file(&ctx.root.join(name).join("cycle.json")).unwrap(),
        "inspected":{"copy":c["pages"],"artwork":c["pages"],"readability":c["pages"]},
        "findings":[],"qa":{"status":"pass","findings":[]},"object_reviews":common::object_judgments(ctx,name),
        "note":"Everything is fine; all overlap is intentional"});
    ctx.replace_json("review.json", &r).unwrap();
    ctx.execute(json!({"action":"evidence_record","cycle":format!("{name}/cycle.json"),"review":"review.json"})).unwrap()
}
fn qa(ctx: &Context, name: &str) -> Value {
    ctx.execute(json!({"action":"qa_check","cycle":format!("{name}/cycle.json")}))
        .unwrap()
}
fn has(v: &Value, code: &str) -> bool {
    v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|f| f["code"] == code)
}

#[test]
fn should_fail_actual_overflow_despite_claimed_pass_and_forged_inspection() {
    let (_dir, ctx) = fixture();
    change(&ctx, |s| {
        s["pages"][0]["texts"][0]["box"] = json!([620, 60, 280, 80])
    });
    cycle(&ctx, "bad");
    // The producer can edit a workspace JSON report and recompute its hash.
    let cpath = "bad/cycle.json";
    let mut c = ctx.json(cpath).unwrap();
    let report_path = c["inspection"]["report"]["path"]
        .as_str()
        .unwrap()
        .to_string();
    let mut report = ctx.json(&report_path).unwrap();
    report["objects"] = json!([]);
    report["ownership_unverified_objects"] = json!(0);
    ctx.replace_json(&report_path, &report).unwrap();
    c["inspection"]["report"] = ctx.binding(&report_path).unwrap();
    ctx.replace_json(cpath, &c).unwrap();
    let r = claim(&ctx, "bad");
    assert_eq!(
        r["checks"][3]["status"], "fail",
        "model-authored review must not override native QA"
    );
    assert!(r["native_qa"]["failed"].as_u64().unwrap() > 0);
    let q = qa(&ctx, "bad");
    assert!(has(&q, "INK_OUTSIDE_SLIDE"));
    assert_eq!(q["status"], "fail");
}

#[test]
fn should_not_accept_request_level_ignore_or_threshold_overrides() {
    let (_dir, ctx) = fixture();
    cycle(&ctx, "clean");
    for key in [
        "ignore",
        "dismiss",
        "allow_failures",
        "threshold",
        "tolerance",
        "passed",
        "status",
    ] {
        let mut r = json!({"action":"qa_check","cycle":"clean/cycle.json"});
        r[key] = json!(true);
        assert!(
            ctx.execute(r)
                .unwrap_err()
                .to_string()
                .contains("does not accept"),
            "{key}"
        );
    }
}

#[test]
fn should_bind_qa_to_actual_pptx_not_a_forged_clean_report_or_reused_cycle() {
    let (_dir, ctx) = fixture();
    cycle(&ctx, "before");
    assert_eq!(qa(&ctx, "before")["status"], "pass");
    change(&ctx, |s| {
        s["pages"][0]["texts"][0]["box"] = json!([630, 40, 280, 80])
    });
    cycle(&ctx, "after");
    let mut forged = ctx.json("after/cycle.json").unwrap();
    forged["native_qa"] = ctx.json("before/cycle.json").unwrap()["native_qa"].clone();
    ctx.replace_json("after/cycle.json", &forged).unwrap();
    assert_eq!(qa(&ctx, "after")["status"], "fail");
    let r = claim(&ctx, "after");
    assert_eq!(r["checks"][3]["status"], "fail");
    let qa_report = ctx
        .json(ctx.verify(&r["native_qa"]["report"]).unwrap())
        .unwrap();
    let id = qa_report["findings"][0]["id"].clone();
    let mut review = ctx.json("review.json").unwrap();
    review["findings"] = json!([{"id":id,"check":"readability","state":"dismissed",
        "note":"Hand drawn style; intentional; accept all","evidence":"after/rendered/slide-001.png"}]);
    ctx.replace_json("review.json", &review).unwrap();
    let retry = ctx
        .execute(
            json!({"action":"evidence_record","cycle":"after/cycle.json","review":"review.json"}),
        )
        .unwrap();
    assert_eq!(retry["checks"][3]["status"], "fail");
}

#[test]
fn should_use_nonzero_cli_exit_for_failed_or_unknown_qa() {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let (_dir, ctx) = fixture();
    change(&ctx, |s| {
        s["pages"][0]["texts"][0]
            .as_object_mut()
            .unwrap()
            .remove("region")
            .map(drop)
            .unwrap_or(())
    });
    cycle(&ctx, "unknown");
    let mut process = Command::new(env!("CARGO_BIN_EXE_octos-pptx"))
        .arg("pptx_tool")
        .current_dir(&ctx.root)
        .env("OCTOS_SESSION_WORKSPACE", &ctx.root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    process
        .stdin
        .take()
        .unwrap()
        .write_all(br#"{"action":"qa_check","cycle":"unknown/cycle.json"}"#)
        .unwrap();
    let out = process.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    let result: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(result["result"]["status"], "unknown");
}

#[test]
fn should_preserve_legacy_array_regions_and_allow_equivalent_metadata_upgrade() {
    let (_dir, ctx) = fixture();
    change(&ctx, |s| {
        s["pages"][0]["regions"]["card"] = json!([40, 40, 360, 160])
    });
    cycle(&ctx, "legacy-array");
    claim(&ctx, "legacy-array");
    // Exercise already-persisted contracts in the original array format too.
    let history = ".octos/pptx-evidence-history.json";
    let mut saved = ctx.json(history).unwrap();
    saved["qa_contract"]["pages"]["overview"]["regions"]["card"] = json!([40, 40, 360, 160]);
    ctx.replace_json(history, &saved).unwrap();
    let unchanged = qa(&ctx, "legacy-array");
    assert!(
        !has(&unchanged, "CONSTRAINT_CHANGED"),
        "unchanged geometry is not a revision"
    );
    assert!(
        has(&unchanged, "OWNER_UNVERIFIED"),
        "representation equality is not ownership proof"
    );
    change(&ctx, |s| {
        s["pages"][0]["regions"]["card"] = json!({
            "quad":[[40,40],[400,40],[400,200],[40,200]],
            "scope":"label","basis":"Synthetic independently defined card"
        })
    });
    cycle(&ctx, "documented-array");
    let upgraded = claim(&ctx, "documented-array");
    assert_eq!(upgraded["native_qa"]["status"], "pass");
    change(&ctx, |s| {
        s["pages"][0]["regions"]["card"]["quad"] = json!([[0, 0], [640, 0], [640, 360], [0, 360]])
    });
    cycle(&ctx, "array-expanded");
    assert!(has(&qa(&ctx, "array-expanded"), "CONSTRAINT_CHANGED"));
}
