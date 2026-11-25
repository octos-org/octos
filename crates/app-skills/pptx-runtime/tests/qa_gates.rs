use octos_pptx::Context;
use serde_json::{Value, json};
mod common;

fn fixture() -> (tempfile::TempDir, Context) {
    common::labeled_fixture("generic-qa", "overview", "maker")
}
fn claim(ctx: &Context, name: &str) -> Value {
    common::record(ctx, name, json!([]), None, json!([]), true).unwrap()
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
    common::edit_spec(&ctx, |s| {
        s["pages"][0]["texts"][0]["box"] = json!([620, 60, 280, 80])
    });
    common::cycle(&ctx, "bad");
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
    common::cycle(&ctx, "clean");
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
    common::cycle(&ctx, "before");
    assert_eq!(qa(&ctx, "before")["status"], "pass");
    common::edit_spec(&ctx, |s| {
        s["pages"][0]["texts"][0]["box"] = json!([630, 40, 280, 80])
    });
    common::cycle(&ctx, "after");
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
    common::edit_spec(&ctx, |s| {
        s["pages"][0]["texts"][0]
            .as_object_mut()
            .unwrap()
            .remove("region")
            .map(drop)
            .unwrap_or(())
    });
    common::cycle(&ctx, "unknown");
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
    common::edit_spec(&ctx, |s| {
        s["pages"][0]["regions"]["card"] = json!([40, 40, 360, 160])
    });
    common::cycle(&ctx, "legacy-array");
    claim(&ctx, "legacy-array");
    // Exercise already-persisted contracts in the original array format too.
    let history = ".octos/pptx-evidence-history.json";
    let mut saved = ctx.json(history).unwrap();
    saved["qa_contract"]["pages"]["overview"]["regions"]["card"] = json!([40, 40, 360, 160]);
    ctx.replace_json(history, &saved).unwrap();
    let unchanged = qa(&ctx, "legacy-array");
    assert!(
        !has(&unchanged, "CONSTRAINT_CHANGED") && has(&unchanged, "OWNER_UNVERIFIED"),
        "equal geometry is not a revision, but representation equality is not ownership proof"
    );
    common::edit_spec(&ctx, |s| {
        s["pages"][0]["regions"]["card"] = json!({
            "quad":[[40,40],[400,40],[400,200],[40,200]],
            "scope":"label","basis":"Synthetic independently defined card"
        })
    });
    common::cycle(&ctx, "documented-array");
    let upgraded = claim(&ctx, "documented-array");
    assert_eq!(upgraded["native_qa"]["status"], "pass");
    common::edit_spec(&ctx, |s| {
        s["pages"][0]["regions"]["card"]["quad"] = json!([[0, 0], [640, 0], [640, 360], [0, 360]])
    });
    common::cycle(&ctx, "array-expanded");
    assert!(has(&qa(&ctx, "array-expanded"), "CONSTRAINT_CHANGED"));
}
