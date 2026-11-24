use super::{approve, fixture};
use octos_pptx::Context;
use serde_json::{Value, json};

fn setup(root: &std::path::Path) -> (Context, Value) {
    let mut ctx = fixture(root);
    ctx.continuation_owner = Some("session:coordinator".into());
    ctx.execute(json!({"action":"evidence_init","task_id":"assembled","stagnant_attempts":3,
        "deliverables":["final/deck.pptx","final/render/slide-002.png","final/qa.json","review/verdict.json","review/report.md"],
        "verification_mode":"independent_visual","reviewer_agent":"session:final-reviewer"})).unwrap();
    for id in ["alpha", "beta"] {
        approve(&ctx, id);
    }
    let result = ctx
        .execute(json!({"action":"page_assemble","output":"final/deck.pptx"}))
        .unwrap();
    ctx.execute(
        json!({"action":"render","input":"final/deck.pptx","output":"final/render","width":64}),
    )
    .unwrap();
    let row = json!({"status":"pass","basis":"synthetic regression evidence"});
    let per_page = json!({"copy":row,"artwork":row,"readability":row,"editability":row});
    let notes = json!({"assembly_sha256":result["receipt"]["sha256"],
        "render_receipt_sha256":ctx.binding("final/render/render.json").unwrap()["sha256"],
        "pages":{"alpha":per_page,"beta":per_page},"critical_defects":[],"open_required_findings":[]});
    ctx.write_json("review/notes.json", &notes).unwrap();
    ctx.write_new("review/report.md", b"Synthetic final review fixture")
        .unwrap();
    let req = json!({"action":"assembly_review_submit","assembly":result["receipt"]["path"],"render_receipt":"final/render/render.json",
        "observations":"review/notes.json","report":"review/report.md","output":"review/verdict.json"});
    (ctx, req)
}

fn collect(ctx: &Context) -> anyhow::Result<Value> {
    ctx.execute(
        json!({"action":"evidence_assembled","review":"review/verdict.json","qa":"final/qa.json"}),
    )
}

#[test]
fn final_assembly_requires_independent_current_complete_evidence_and_replays_without_work() {
    let t = tempfile::tempdir().unwrap();
    let (mut ctx, req) = setup(t.path());
    for user in [
        None,
        Some("session:coordinator"),
        Some("maker-beta"),
        Some("other peer"),
    ] {
        ctx.continuation_owner = user.map(str::to_owned);
        assert!(ctx.execute(req.clone()).is_err());
    }
    assert!(!t.path().join("review/verdict.json").exists());
    ctx.continuation_owner = Some("final-reviewer".into());
    let mut notes = ctx.json("review/notes.json").unwrap();
    let original = notes.clone();
    notes["pages"].as_object_mut().unwrap().remove("alpha");
    ctx.replace_json("review/notes.json", &notes).unwrap();
    assert!(ctx.execute(req.clone()).is_err());
    notes = original.clone();
    notes["open_required_findings"] = json!(["missing label"]);
    ctx.replace_json("review/notes.json", &notes).unwrap();
    assert!(ctx.execute(req.clone()).is_err());
    notes = original.clone();
    notes["render_receipt_sha256"] = json!("previous revision");
    ctx.replace_json("review/notes.json", &notes).unwrap();
    assert!(ctx.execute(req.clone()).is_err());
    ctx.replace_json("review/notes.json", &original).unwrap();
    let submitted = ctx.execute(req.clone()).unwrap();
    assert_eq!(ctx.execute(req.clone()).unwrap(), submitted);
    assert!(
        collect(&ctx).is_err(),
        "the reviewer cannot record an owner attempt"
    );
    ctx.continuation_owner = Some("session:coordinator".into());
    let verdict = ctx.json("review/verdict.json").unwrap();
    let mut fabricated = verdict.clone();
    fabricated["reviewer_session"] = json!("coordinator");
    ctx.replace_json("review/verdict.json", &fabricated)
        .unwrap();
    assert!(collect(&ctx).is_err());
    fabricated = verdict.clone();
    fabricated["report"] = ctx.binding("review/notes.json").unwrap();
    ctx.replace_json("review/verdict.json", &fabricated)
        .unwrap();
    assert!(collect(&ctx).is_err());
    ctx.replace_json("review/verdict.json", &verdict).unwrap();
    // An arbitrary file at a declared delivery path cannot be certified by
    // the independent verdict. Retargeting paths preserves the checked task.
    let deliveries = ctx.json(".octos/continuation.json").unwrap()["deliverables"].clone();
    let mut extended = deliveries.clone();
    extended
        .as_array_mut()
        .unwrap()
        .push(json!("unrelated.json"));
    ctx.write_json("unrelated.json", &json!({"status":"pass"}))
        .unwrap();
    ctx.execute(json!({"action":"evidence_retarget","deliverables":extended,"reason":"test unrelated delivery"})).unwrap();
    assert!(
        collect(&ctx)
            .unwrap_err()
            .to_string()
            .contains("no current assembly")
    );
    assert_eq!(
        ctx.json(".octos/iteration-evidence.json").unwrap()["attempt"],
        0
    );
    assert!(!t.path().join("final/qa.json").exists());
    ctx.execute(json!({"action":"evidence_retarget","deliverables":deliveries,"reason":"restore original delivery"})).unwrap();
    let first = collect(&ctx).unwrap();
    assert_eq!(first["all_checks_pass"], true);
    assert_eq!(first["attempt"], 1);
    assert_eq!(collect(&ctx).unwrap(), first);
    let snapshot = ctx.json(".octos/iteration-evidence.json").unwrap();
    assert_eq!(snapshot["phase"], "checked");
    assert_eq!(snapshot["scope"], json!(["alpha", "beta"]));
    assert_eq!(snapshot["checks"].as_array().unwrap().len(), 4);
    for binding in snapshot["artifacts"].as_array().unwrap() {
        ctx.verify(binding).unwrap();
    }
    let status = ctx.execute(json!({"action":"evidence_status"})).unwrap();
    assert_eq!(status["delivery_bindings_current"], true);
    assert_eq!(
        status["runtime_consumed_current_evidence"], false,
        "no host in this test"
    );
    std::fs::write(
        t.path().join("final/render/slide-002.png"),
        b"mutated render",
    )
    .unwrap();
    assert!(collect(&ctx).is_err());
    assert_eq!(
        ctx.json(".octos/iteration-evidence.json").unwrap(),
        snapshot
    );
}

#[test]
fn unknown_editability_remains_incomplete_until_reviewer_publishes_a_bound_revision() {
    let t = tempfile::tempdir().unwrap();
    let (mut ctx, mut req) = setup(t.path());
    let original = ctx.json("review/notes.json").unwrap();
    let mut notes = original.clone();
    notes["pages"]["beta"]["editability"]["status"] = json!("unknown");
    ctx.replace_json("review/notes.json", &notes).unwrap();
    ctx.continuation_owner = Some("final-reviewer".into());
    let first = ctx.execute(req.clone()).unwrap();
    ctx.continuation_owner = Some("coordinator".into());
    let partial = collect(&ctx).unwrap();
    assert_eq!(partial["all_checks_pass"], false);
    assert_eq!(partial["checks"][1]["status"], "unknown");
    ctx.replace_json("review/notes.json", &original).unwrap();
    assert!(
        collect(&ctx).is_err(),
        "changed observations alone cannot become a pass"
    );
    ctx.continuation_owner = Some("final-reviewer".into());
    assert!(
        ctx.execute(req.clone()).is_err(),
        "revision needs old review digest"
    );
    req["previous_review"] = first["review"]["sha256"].clone();
    ctx.execute(req).unwrap();
    ctx.continuation_owner = Some("session:coordinator".into());
    let complete = collect(&ctx).unwrap();
    assert_eq!(complete["all_checks_pass"], true);
    assert_eq!(complete["attempt"], 2);
    assert_eq!(collect(&ctx).unwrap(), complete);
}

#[test]
fn repaired_page_keeps_the_independent_final_verdict_it_cited() {
    let t = tempfile::tempdir().unwrap();
    let (mut ctx, submit) = setup(t.path());
    ctx.continuation_owner = Some("final-reviewer".into());
    let first = ctx.execute(submit).unwrap();
    let signed = ctx.json(ctx.verify(&first["review"]).unwrap()).unwrap();
    let candidate = super::gate(&ctx)["pages"][0]["candidate_id"].clone();
    std::fs::remove_file(t.path().join("alpha-review.json")).unwrap();
    ctx.continuation_owner = Some("reviewer".into());
    let approved = ctx
        .execute(
            json!({"action":"page_review_submit","page":"alpha","candidate_id":candidate,
        "verdict":{"score":96,"accepted":true,"critical_defects":[],"open_required_findings":[],
        "evidence":[first["review"]]}}),
        )
        .unwrap();
    let page_review = ctx.json(ctx.verify(&approved["review"]).unwrap()).unwrap();
    assert_eq!(page_review["evidence"][0], signed["attestation"]);
    std::fs::write(ctx.verify(&first["review"]).unwrap(), b"next verdict").unwrap();
    ctx.verify(&page_review["evidence"][0]).unwrap();
    assert_eq!(super::gate(&ctx)["pages"][0]["work_stage"], "complete");
}

#[test]
fn reviewer_can_reject_delivered_bytes_while_a_maker_repairs_its_source() {
    let t = tempfile::tempdir().unwrap();
    let (mut ctx, submit) = setup(t.path());
    ctx.continuation_owner = Some("final-reviewer".into());
    super::edit(&ctx, "beta.json", |p| {
        p["copy"]["label"] = json!("new draft")
    });
    assert!(
        ctx.execute(submit.clone()).is_err(),
        "a stale acceptance cannot pass"
    );
    let observed = submit["observations"].as_str().unwrap();
    super::edit(&ctx, observed, |v| {
        v["pages"]["beta"]["copy"] = json!({"status":"fail","basis":"Synthetic old label"});
        v["open_required_findings"] = json!(["beta label"]);
    });
    let result = ctx.execute(submit.clone()).unwrap();
    assert!(
        result["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["status"] == "fail")
    );
    let assembly = ctx.json(submit["assembly"].as_str().unwrap()).unwrap();
    std::fs::write(
        ctx.verify(&assembly["pages"][1]["artifact"]).unwrap(),
        b"foreign",
    )
    .unwrap();
    assert!(
        ctx.execute(submit).is_err(),
        "old slide bytes must still reproduce the package"
    );
}

#[test]
fn final_reviewer_template_is_bound_unanswered_and_cannot_replace_work() {
    let t = tempfile::tempdir().unwrap();
    let (mut ctx, submit) = setup(t.path());
    let mut ask = json!({"action":"assembly_review_submit","assembly":submit["assembly"],
        "render_receipt":submit["render_receipt"],"template_output":"review/template.json"});
    ctx.continuation_owner = Some("coordinator".into());
    assert!(ctx.execute(ask.clone()).is_err());
    ctx.continuation_owner = Some("final-reviewer".into());
    let first = ctx.execute(ask.clone()).unwrap();
    assert_eq!(ctx.execute(ask.clone()).unwrap(), first);
    let path = ctx.verify(&first["observations_template"]).unwrap();
    let template = ctx.json(&path).unwrap();
    assert_eq!(first["pages"], 2);
    assert_eq!(
        template["pages"]["beta"]["artwork"],
        json!({"status":"unknown","basis":""})
    );
    assert_eq!(
        template["assembly_sha256"],
        ctx.binding(submit["assembly"].as_str().unwrap()).unwrap()["sha256"]
    );
    assert!(!ctx.path("review/verdict.json").unwrap().exists());
    let mut invalid = submit;
    invalid["observations"] = ask["template_output"].clone();
    assert!(ctx.execute(invalid).is_err());
    std::fs::write(path, b"{}").unwrap();
    assert!(ctx.execute(ask.clone()).is_err());
    ask["template_output"] = json!("final/qa.json");
    assert!(ctx.execute(ask).is_err());
}

#[test]
fn failed_final_review_repairs_and_rechecks_the_same_canonical_delivery() {
    use super::{edit, gate};
    let t = tempfile::tempdir().unwrap();
    let (mut ctx, mut final_review) = setup(t.path());
    // Start this fixture's beta review using the native reviewer route.
    std::fs::remove_file(t.path().join("beta-review.json")).unwrap();
    ctx.continuation_owner = Some("reviewer".into());
    let mut page_review = json!({"action":"page_review_submit","page":"beta",
        "candidate_id":gate(&ctx)["pages"][1]["candidate_id"], "verdict":{"score":96,"accepted":true,
        "critical_defects":[],"open_required_findings":[],"evidence":["evidence.json"]}});
    let initial_page = ctx.execute(page_review.clone()).unwrap();
    let bare_assembly = json!({"action":"page_assemble","output":"final/deck.pptx"});
    ctx.continuation_owner = Some("coordinator".into());
    let old = ctx.execute(bare_assembly.clone()).unwrap();
    let old_deck = ctx.read("final/deck.pptx").unwrap();
    let old_renders = ctx.json("final/render/render.json").unwrap();
    let old_beta = ctx.read("final/render/slide-002.png").unwrap();
    let alpha_time = std::fs::metadata(t.path().join("final/render/slide-001.png"))
        .unwrap()
        .modified()
        .unwrap();
    let mut notes = ctx.json("review/notes.json").unwrap();
    notes["assembly_sha256"] = old["receipt"]["sha256"].clone();
    notes["pages"]["beta"]["copy"] = json!({"status":"fail","basis":"Synthetic label missing."});
    notes["open_required_findings"] = json!(["beta label"]);
    ctx.replace_json("review/notes.json", &notes).unwrap();
    ctx.continuation_owner = Some("final-reviewer".into());
    let failed = ctx.execute(final_review.clone()).unwrap();
    ctx.write_new(
        "review/old-alias.json",
        &ctx.read("review/verdict.json").unwrap(),
    )
    .unwrap();
    let mut alias_submit = final_review.clone();
    alias_submit["output"] = json!("review/forked-head.json");
    assert!(
        ctx.execute(alias_submit).is_err(),
        "fresh path cannot fork the current final verdict"
    );
    let mut repair = bare_assembly.clone();
    repair["repair_review"] = failed["review"].clone();
    ctx.continuation_owner = Some("coordinator".into());
    assert!(!collect(&ctx).unwrap()["all_checks_pass"].as_bool().unwrap());
    assert!(ctx.execute(repair.clone()).is_err(), "no changed page");
    ctx.continuation_owner = Some("reviewer".into());
    page_review["replaces_verdict_id"] = initial_page["verdict_id"].clone();
    page_review["verdict"]["accepted"] = json!(false);
    page_review["verdict"]["open_required_findings"] = json!(["beta label"]);
    let released = ctx.execute(page_review.clone()).unwrap();
    ctx.continuation_owner = Some("maker-beta".into());
    edit(&ctx, "beta.json", |v| {
        v["copy"]["label"] = json!("Fixed");
        v["texts"][0]["text"] = json!("Fixed");
    });
    let made=ctx.execute(json!({"action":"cycle","spec":"deck.json","page":"beta","width":64,"output":"new-beta","publish_candidate":true,"freeze_inputs":true})).unwrap();
    ctx.continuation_owner = Some("coordinator".into());
    assert!(ctx.execute(repair.clone()).is_err(), "unreviewed repair");
    ctx.continuation_owner = Some("reviewer".into());
    page_review["candidate_id"] = made["peer_review_handoff"]["candidate_id"].clone();
    page_review["replaces_verdict_id"] = released["verdict_id"].clone();
    page_review["verdict"]["accepted"] = json!(true);
    page_review["verdict"]["open_required_findings"] = json!([]);
    ctx.execute(page_review).unwrap();
    assert!(
        ctx.execute(repair.clone()).is_err(),
        "reviewer cannot replace"
    );
    ctx.continuation_owner = Some("coordinator".into());
    assert!(
        ctx.execute(bare_assembly).is_err(),
        "replacement requires prior review"
    );
    std::fs::write(
        t.path().join("final/render/slide-002.png"),
        b"externally changed",
    )
    .unwrap();
    assert!(
        ctx.execute(repair.clone()).is_err(),
        "external preview edit"
    );
    assert_eq!(ctx.read("final/deck.pptx").unwrap(), old_deck);
    std::fs::write(t.path().join("final/render/slide-002.png"), &old_beta).unwrap();
    let mut stage = repair.clone();
    stage["stage_repair_render"] = json!(true);
    let staged = ctx.execute(stage.clone()).unwrap();
    assert_eq!(ctx.execute(stage).unwrap(), staged);
    assert_eq!(staged["stage"], "awaiting_repair_render");
    let doctor = ctx
        .execute(json!({"action":"doctor","check_renderer":true}))
        .unwrap();
    if doctor["renderer_probe"]["passed"] == false {
        let auto = ctx.execute(repair.clone()).unwrap();
        assert_eq!(auto["render_request"], staged["render_request"]);
        assert_eq!(auto["local_renderer_probe"]["passed"], false);
        assert_eq!(ctx.read("final/deck.pptx").unwrap(), old_deck);
    }
    assert_ne!(staged["staged_pptx"]["sha256"], old["output"]["sha256"]);
    let separate = ctx.execute(staged["render_request"].clone()).unwrap();
    assert_eq!(separate["input"], staged["staged_pptx"]);
    assert_eq!(ctx.read("final/deck.pptx").unwrap(), old_deck);
    let staged_view = ctx.verify(&separate["pages"][1]).unwrap();
    let good = ctx.read(&staged_view).unwrap();
    std::fs::write(&staged_view, b"foreign view").unwrap();
    assert!(ctx.execute(repair.clone()).is_err());
    assert_eq!(ctx.read("final/deck.pptx").unwrap(), old_deck);
    std::fs::write(&staged_view, good).unwrap();
    let done = ctx.execute(repair.clone()).unwrap();
    assert_eq!(ctx.execute(repair.clone()).unwrap(), done);
    assert_ne!(done["output"]["sha256"], old["output"]["sha256"]);
    assert_eq!(
        std::fs::metadata(t.path().join("final/render/slide-001.png"))
            .unwrap()
            .modified()
            .unwrap(),
        alpha_time
    );
    assert_ne!(
        ctx.binding("final/render/slide-002.png").unwrap()["sha256"],
        old_renders["pages"][1]["sha256"]
    );
    let new_renders = ctx.json("final/render/render.json").unwrap();
    let journal = ctx
        .json(ctx.verify(&done["repair_history"]).unwrap())
        .unwrap();
    assert_eq!(journal["prior_render"], old_renders);
    assert_eq!(journal["render"], new_renders);
    assert_eq!(new_renders["input"], done["output"]);
    assert!(
        collect(&ctx).is_err(),
        "old final verdict cannot approve new delivery"
    );
    // Reproduce an interruption after promoting the PPTX but before the changed
    // image and receipt heads. The same failed-review binding completes it once.
    let stage_parent = ctx
        .verify(&done["repair_history"])
        .unwrap()
        .parent()
        .unwrap()
        .to_owned();
    let staged = std::fs::read_dir(stage_parent)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.is_dir())
        .unwrap();
    let beta = t.path().join("final/render/slide-002.png");
    std::fs::rename(&beta, staged.join("png/slide-002.png")).unwrap();
    std::fs::write(&beta, &old_beta).unwrap();
    ctx.replace_json("final/render/render.json", &old_renders)
        .unwrap();
    ctx.replace_json(
        done["receipt"]["path"].as_str().unwrap(),
        &journal["prior_assembly"],
    )
    .unwrap();
    assert_eq!(ctx.execute(repair.clone()).unwrap(), done);
    assert!(!staged.join("deck.pptx").exists());
    assert!(!staged.join("png/slide-002.png").exists());

    notes["assembly_sha256"] = done["receipt"]["sha256"].clone();
    notes["render_receipt_sha256"] = done["render_receipt"]["sha256"].clone();
    notes["pages"]["beta"]["copy"] =
        json!({"status":"pass","basis":"Synthetic replacement is visible."});
    notes["open_required_findings"] = json!([]);
    ctx.write_json("review/repaired-notes.json", &notes)
        .unwrap();
    final_review["observations"] = json!("review/repaired-notes.json");
    final_review["previous_review"] = failed["review"]["sha256"].clone();
    ctx.continuation_owner = Some("final-reviewer".into());
    let passed = ctx.execute(final_review).unwrap();
    ctx.continuation_owner = Some("coordinator".into());
    let closed = collect(&ctx).unwrap();
    assert_eq!(closed["all_checks_pass"], true);
    assert_eq!(closed["attempt"], 2);
    assert!(
        ctx.execute(repair.clone()).is_err(),
        "stale previous verdict"
    );
    repair["repair_review"] = ctx.binding("review/old-alias.json").unwrap();
    assert!(
        ctx.execute(repair.clone()).is_err(),
        "an archived alias cannot supersede the current final verdict"
    );
    repair["repair_review"] = passed["review"].clone();
    assert!(
        ctx.execute(repair).is_err(),
        "passing verdict does not authorize changes"
    );
}

#[test]
fn implicit_multi_page_route_rejects_a_shared_reviewer_before_it_freezes() {
    let root = tempfile::tempdir().unwrap();
    let mut ctx = Context::new(root.path()).unwrap();
    ctx.continuation_owner = Some("coordinator".into());
    let path = ".octos/pptx-page-plan.json";
    let pages: Vec<_> = ["alpha", "beta"]
        .iter()
        .map(|id| {
            json!({"id":id,"maker_session":format!("maker-{id}"),
            "reviewer_session":"reviewer-alpha","candidate":format!("{id}/candidate.json"),
            "review":format!("{id}/review.json")})
        })
        .collect();
    let mut plan = json!({"schema_version":2,"coordinator_session":"coordinator",
        "rubric":{"quality":100},"pages":pages});
    ctx.write_json(path, &plan).unwrap();
    let init = json!({"action":"evidence_init","task_id":"default-roster","stagnant_attempts":3,
        "deliverables":["deck.pptx","qa.json"]});
    let error = ctx.execute(init.clone()).unwrap_err();
    assert!(error.to_string().contains("distinct independent reviewer"));
    assert!(
        !root
            .path()
            .join(".octos/pptx-verification-plan.json")
            .exists()
    );
    assert!(!root.path().join(".octos/continuation.json").exists());
    plan["pages"][1]["reviewer_session"] = json!("reviewer-beta");
    ctx.replace_json(path, &plan).unwrap();
    ctx.execute(init).unwrap();
    let route = ctx.json(".octos/pptx-verification-plan.json").unwrap();
    assert_eq!(route["mode"], "per_page");
    assert_eq!(route["page_plan"], ctx.binding(path).unwrap());
}

#[test]
fn per_page_freezes_distinct_reviewer_sessions_and_coordinator_before_production() {
    let root = tempfile::tempdir().unwrap();
    let mut ctx = fixture(root.path());
    ctx.continuation_owner = Some("session:coordinator".into());
    let path = ".octos/pptx-page-plan.json";
    let mut plan = ctx.json(path).unwrap();
    let init = json!({"action":"evidence_init","task_id":"scoped","verification_mode":"per_page",
        "stagnant_attempts":3,"deliverables":["deck.pptx","qa.json"]});
    plan["schema_version"] = json!(2);
    plan["rubric"] = json!({"quality":100});
    plan["pages"][0]["reviewer_session"] = json!("reviewer-a");
    for wrong in [
        Value::Null,
        json!("session:reviewer-a"),
        json!("session:coordinator"),
        json!("maker-alpha"),
    ] {
        plan["pages"][1]["reviewer_session"] = wrong;
        ctx.replace_json(path, &plan).unwrap();
        assert!(ctx.execute(init.clone()).is_err());
        assert!(!root.path().join(".octos/continuation.json").exists());
    }
    plan["pages"][1]["reviewer_session"] = json!("reviewer-b");
    ctx.replace_json(path, &plan).unwrap();
    let mut route = init;
    route["arbiter_agent"] = json!("session:reviewer-b");
    assert!(ctx.execute(route.clone()).is_err());
    route["arbiter_agent"] = json!("arbiter");
    ctx.execute(route).unwrap();
    assert_eq!(
        ctx.json(".octos/pptx-verification-plan.json").unwrap()["arbiter_agent"],
        "arbiter"
    );
    plan["pages"].as_array_mut().unwrap().reverse();
    ctx.replace_json(path, &plan).unwrap();
    assert!(
        ctx.execute(json!({"action":"page_gate"}))
            .unwrap_err()
            .to_string()
            .contains("changed after")
    );
}

#[test]
fn signed_pages_stream_in_plan_order_and_withdrawals_update_the_same_editable_file() {
    use std::io::Read;
    const LIVE: &str = ".octos/pptx-progress/ready.pptx";
    let root = tempfile::tempdir().unwrap();
    let mut ctx = fixture(root.path());
    ctx.continuation_owner = Some("coordinator".into());
    let path = ".octos/pptx-page-plan.json";
    let mut plan = ctx.json(path).unwrap();
    plan["schema_version"] = json!(2);
    plan["rubric"] = json!({"quality":100});
    for page in plan["pages"].as_array_mut().unwrap() {
        page["reviewer_session"] = json!(format!("reviewer-{}", page["id"].as_str().unwrap()));
    }
    ctx.replace_json(path, &plan).unwrap();
    ctx.execute(
        json!({"action":"evidence_init","task_id":"rolling-pages","stagnant_attempts":3,
        "deliverables":["final/deck.pptx","qa.json"]}),
    )
    .unwrap();
    let refresh = json!({"action":"page_progress"});
    assert_eq!(ctx.execute(refresh.clone()).unwrap()["pages"], json!([]));
    assert!(!ctx.path(LIVE).unwrap().exists());
    let gate = ctx.execute(json!({"action":"page_gate"})).unwrap();
    let decision = |i: usize| {
        let mut domains = json!({});
        for key in ["copy", "artwork", "readability", "editability"] {
            domains[key] = json!({"status":"pass","basis":"observed in the synthetic fixture"});
        }
        json!({"action":"page_review_submit","page":plan["pages"][i]["id"],
            "candidate_id":gate["pages"][i]["candidate_id"],
            "verdict":{"score":98,"component_scores":{"quality":98},"accepted":true,
                "critical_defects":[],"open_required_findings":[],"delivery_checks":domains,
                "evidence":["evidence.json"]}})
    };
    let scoped = ctx
        .execute(json!({"action":"page_gate","only_page":"beta"}))
        .unwrap();
    let unanswered = &scoped["review_request_template"];
    assert_eq!(unanswered["candidate_id"], gate["pages"][1]["candidate_id"]);
    assert_eq!(
        unanswered["verdict"]["delivery_checks"]["copy"],
        json!({"status":null,"basis":""})
    );
    let beta = decision(1);
    ctx.continuation_owner = Some("reviewer-beta".into());
    assert!(ctx.execute(refresh.clone()).is_err());
    let mut malformed = beta.clone();
    malformed["verdict"]["delivery_checks"] = json!({"copy":true});
    malformed["verdict"]["score"] = json!(101);
    let diagnostic = ctx.execute(malformed).unwrap_err().to_string();
    for required in [
        "copy",
        "artwork",
        "readability",
        "editability",
        "verdict.delivery_checks",
        "status",
        "basis",
        "score must be between",
    ] {
        assert!(diagnostic.contains(required), "{diagnostic}");
    }
    assert!(!ctx.path(LIVE).unwrap().exists());

    let b = ctx.execute(beta.clone()).unwrap();
    assert_eq!(b["progress_path"], LIVE, "{b}");
    assert_eq!(
        ctx.binding(LIVE).unwrap()["sha256"],
        ctx.binding("beta-postbuild.pptx").unwrap()["sha256"]
    );
    assert!(!ctx.path("final/deck.pptx").unwrap().exists());
    let mut alpha = decision(0);
    ctx.continuation_owner = Some("reviewer-alpha".into());
    let a = ctx.execute(alpha.clone()).unwrap();
    let merged = ctx.binding(LIVE).unwrap();
    let modified = std::fs::metadata(root.path().join(LIVE))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(ctx.execute(alpha.clone()).unwrap(), a);
    assert_eq!(
        std::fs::metadata(root.path().join(LIVE))
            .unwrap()
            .modified()
            .unwrap(),
        modified
    );
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(ctx.read(LIVE).unwrap())).unwrap();
    for (n, word) in [(1, "alpha"), (2, "beta")] {
        let mut xml = String::new();
        zip.by_name(&format!("ppt/slides/slide{n}.xml"))
            .unwrap()
            .read_to_string(&mut xml)
            .unwrap();
        assert!(
            xml.contains(&format!(">{word}</a:t>")),
            "{word} must remain native editable text"
        );
    }
    assert!(zip.by_name("ppt/slides/slide3.xml").is_err());
    ctx.continuation_owner = Some("coordinator".into());
    let result = ctx.execute(refresh.clone()).unwrap();
    assert_eq!(result["pages"], json!(["alpha", "beta"]));
    assert_eq!(result["output"], merged);
    assert_eq!(result["quality_approved"], false);
    assert!(
        ctx.execute(json!({"action":"page_assemble","output":LIVE}))
            .is_err()
    );
    let finalized = ctx
        .execute(json!({"action":"page_assemble","output":"final/deck.pptx"}))
        .unwrap();
    assert_eq!(finalized["output"]["sha256"], merged["sha256"]);

    let withdraw = |req: &mut Value, receipt: &Value| {
        req["replaces_verdict_id"] = receipt["verdict_id"].clone();
        req["verdict"]["accepted"] = json!(false);
        req["verdict"]["open_required_findings"] = json!([{"id":"F1","kind":"visible","impact":"Fixture withdraws this visual for re-review","box":[0,0,12,12]}]);
    };
    ctx.continuation_owner = Some("reviewer-alpha".into());
    withdraw(&mut alpha, &a);
    ctx.execute(alpha).unwrap();
    assert_eq!(
        ctx.binding(LIVE).unwrap()["sha256"],
        ctx.binding("beta-postbuild.pptx").unwrap()["sha256"]
    );
    let mut beta = beta;
    withdraw(&mut beta, &b);
    ctx.continuation_owner = Some("reviewer-beta".into());
    ctx.execute(beta).unwrap();
    assert!(!ctx.path(LIVE).unwrap().exists());

    // Fail publication independently of the signed verdict and recover without regrading.
    let mut again = decision(1);
    let current = ctx.execute(json!({"action":"page_gate"})).unwrap();
    again["replaces_verdict_id"] = current["pages"][1]["verdict_id"].clone();
    ctx.write_new(LIVE, b"preserve my file").unwrap();
    let saved = ctx.execute(again).unwrap();
    assert_eq!(saved["accepted"], true);
    assert!(
        saved["progress_error"]
            .as_str()
            .unwrap()
            .contains("externally")
    );
    assert_eq!(ctx.read(LIVE).unwrap(), b"preserve my file");
    std::fs::rename(ctx.path(LIVE).unwrap(), root.path().join("my-file")).unwrap();
    ctx.continuation_owner = Some("coordinator".into());
    assert_eq!(ctx.execute(refresh).unwrap()["pages"], json!(["beta"]));
}
