//! Synthetic workflow admission, not a visual-quality evaluation.
use octos_pptx::Context;
use serde_json::{Value, json};

fn fixture(root: &std::path::Path) -> Context {
    let ctx = Context::new(root).unwrap();
    image::RgbImage::from_pixel(40, 30, image::Rgb([20, 30, 40]))
        .save(root.join("art.png"))
        .unwrap();
    std::fs::copy(root.join("art.png"), root.join("design.png")).unwrap();
    for id in ["alpha", "beta"] {
        ctx.write_json(format!("{id}.json"), &json!({"id":id,"background":"art.png",
            "copy":{"label":id},"texts":[{"id":"label","text":id,"style":"body","box":[1,1,35,20]}]})).unwrap();
    }
    ctx.write_json(
        "deck.json",
        &json!({"schema_version":1,
        "canvas":{"width_px":40,"height_px":30,"width_in":4,"height_in":3},
        "styles":{"body":{"font":"Inter","size_pt":10}},
        "pages":[{"path":"alpha.json"},{"path":"beta.json"}]}),
    )
    .unwrap();
    ctx.write_json("evidence.json", &json!({"synthetic":true}))
        .unwrap();
    for id in ["alpha", "beta"] {
        let seed = if id == "alpha" { "seed" } else { "seed-beta" };
        let build = ctx
            .execute(json!({"action":"build","spec":"deck.json","page":id,"output":seed}))
            .unwrap();
        let output = if id == "alpha" {
            "postbuild.pptx"
        } else {
            "beta-postbuild.pptx"
        };
        let batch=ctx.execute(json!({"action":"batch","input":format!("{seed}/editable.pptx"),"output":output,"commands":[]})).unwrap();
        let post_path = format!("{id}-postbuild-receipt.json");
        ctx.write_json(&post_path, &batch).unwrap();
        let render=ctx.execute(json!({"action":"render","input":output,"output":format!("{id}-render"),"width":64})).unwrap();
        ctx.write_json(
            format!("{id}-candidate.json"),
            &json!({
            "page_id":id,"maker_session":format!("maker-{id}"),
            "source":ctx.binding(format!("{id}.json")).unwrap(),
            "build_receipt":build["receipt"],
            "artifact":ctx.binding(output).unwrap(),
            "render":render["pages"][0],
            "render_receipt":ctx.binding(format!("{id}-render/render.json")).unwrap(),
            "dependencies":[ctx.binding("design.png").unwrap()],
            "receipts":[build["receipt"],ctx.binding(&post_path).unwrap(),ctx.binding("evidence.json").unwrap()]}),
        )
        .unwrap();
    }
    ctx.write_json(".octos/pptx-page-plan.json", &json!({"schema_version":1,
        "coordinator_session":"coordinator","min_score":95,
        "pages":[
            {"id":"alpha","maker_session":"maker-alpha","candidate":"alpha-candidate.json","review":"alpha-review.json"},
            {"id":"beta","maker_session":"maker-beta","candidate":"beta-candidate.json","review":"beta-review.json"}]})).unwrap();
    ctx
}

fn approve(ctx: &Context, id: &str) {
    ctx.write_json(
        format!("{id}-review.json"),
        &json!({"page_id":id,
        "reviewer_session":"reviewer","accepted":true,"score":96,
        "critical_defects":[],"open_required_findings":[],
        "candidate_sha256":ctx.binding(format!("{id}-candidate.json")).unwrap()["sha256"],
        "evidence":[ctx.binding("evidence.json").unwrap()]}),
    )
    .unwrap();
}
fn gate(ctx: &Context) -> Value {
    ctx.execute(json!({"action":"page_gate"})).unwrap()
}

fn verdict_request(ctx: &Context) -> Value {
    let state = gate(ctx);
    json!({"action":"page_review_submit","page":"alpha",
        "candidate_id":state["pages"][0]["candidate_id"],
        "verdict":{"score":94, "accepted":false, "critical_defects":[],
            "open_required_findings":["F1"],"evidence":["evidence.json"]}})
}

#[test]
fn reviewer_can_publish_and_revise_one_bound_verdict_without_rebuilding() {
    let t = tempfile::tempdir().unwrap();
    let mut ctx = fixture(t.path());
    ctx.continuation_owner = Some("session:reviewer".into());
    let mut req = verdict_request(&ctx);
    let first = ctx.execute(req.clone()).unwrap();
    assert_eq!(first["score"], 94);
    assert_eq!(gate(&ctx)["pages"][0]["work_stage"], "repair");
    assert_eq!(ctx.execute(req.clone()).unwrap(), first);
    let before_candidate = ctx.binding("alpha-candidate.json").unwrap();
    req["verdict"]["score"] = json!(96);
    req["verdict"]["accepted"] = json!(true);
    req["verdict"]["open_required_findings"] = json!([]);
    assert!(ctx.execute(req.clone()).is_err(), "must compare revision");
    req["replaces_verdict_id"] = first["verdict_id"].clone();
    ctx.continuation_owner = Some("another reviewer".into());
    assert!(
        ctx.execute(req.clone()).is_err(),
        "reviewer cannot take over"
    );
    ctx.continuation_owner = Some("session:reviewer".into());
    let second = ctx.execute(req.clone()).unwrap();
    assert_ne!(first["archive"], second["archive"]);
    ctx.verify(&first["archive"]).unwrap();
    assert_eq!(ctx.execute(req.clone()).unwrap(), second);
    assert_eq!(gate(&ctx)["accepted_pages"], 1);
    assert_eq!(
        ctx.binding("alpha-candidate.json").unwrap(),
        before_candidate
    );
    req["candidate_id"] = json!("old");
    assert!(
        ctx.execute(req)
            .unwrap_err()
            .to_string()
            .contains("candidate")
    );
    assert_eq!(ctx.binding("alpha-review.json").unwrap(), second["review"]);
}

#[test]
fn submission_checks_all_fields_identity_frozen_rubric_and_evidence_before_writing() {
    let t = tempfile::tempdir().unwrap();
    let mut ctx = fixture(t.path());
    let mut p = ctx.json(".octos/pptx-page-plan.json").unwrap();
    p["rubric"] = json!({"design":60,"editable":40});
    p["pages"][0]["reviewer_session"] = json!("reviewer");
    ctx.replace_json(".octos/pptx-page-plan.json", &p).unwrap();
    let mut req = verdict_request(&ctx);
    for owner in [
        None,
        Some("session:maker-alpha"),
        Some("coordinator"),
        Some("other"),
    ] {
        ctx.continuation_owner = owner.map(str::to_owned);
        assert!(ctx.execute(req.clone()).is_err());
    }
    ctx.continuation_owner = Some("reviewer".into());
    req["verdict"]["evidence"] = json!({"bad":"shape"});
    req["verdict"]["critical_defects"] = Value::Null;
    let err = ctx.execute(req.clone()).unwrap_err().to_string();
    for key in ["evidence", "critical_defects", "component_scores"] {
        assert!(err.contains(key), "{err}");
    }
    assert!(!t.path().join("alpha-review.json").exists());
    req["verdict"]["critical_defects"] = json!([]);
    req["verdict"]["evidence"] = json!(["evidence.json"]);
    req["verdict"]["component_scores"] = json!({"design":55,"editable":37});
    assert!(
        ctx.execute(req.clone())
            .unwrap_err()
            .to_string()
            .contains("sum")
    );
    req["verdict"]["component_scores"]["design"] = json!(57);
    ctx.execute(req).unwrap();
    assert_eq!(gate(&ctx)["pages"][0]["review_current"], true);
    p["rubric"] = json!({"design":59,"editable":41});
    ctx.replace_json(".octos/pptx-page-plan.json", &p).unwrap();
    assert_eq!(gate(&ctx)["pages"][0]["review_current"], false);
}

#[test]
fn below_threshold_without_a_maker_fix_routes_to_a_specific_owner() {
    let t = tempfile::tempdir().unwrap();
    let mut ctx = fixture(t.path());
    ctx.continuation_owner = Some("reviewer".into());
    let mut req = verdict_request(&ctx);
    req["verdict"]["open_required_findings"] = json!([]);
    assert!(
        ctx.execute(req.clone())
            .unwrap_err()
            .to_string()
            .contains("disposition")
    );
    req["verdict"]["disposition"] = json!({"owner":"tool_maintainer","reason":"Requested face cannot be resolved.",
        "resume_when":"The installed face resolves in a rendered fixture."});
    ctx.execute(req).unwrap();
    let inventory = ctx.execute(json!({"action":"page_dispatch"})).unwrap();
    assert_eq!(inventory["needs_disposition"][0]["page"], "alpha");
    assert_eq!(inventory["ready_unassigned"].as_array().unwrap().len(), 2);
    assert_eq!(inventory["ready_unassigned"][0]["phase"], "diagnose");
    assert_eq!(inventory["ready_unassigned"][1]["page"], "beta");
    assert_eq!(inventory["declared_coverage_complete"], false);
    assert_eq!(gate(&ctx)["ready_for_assembly"], false);
}

#[test]
fn human_handoffs_resume_as_peer_work_without_changing_existing_verdicts() {
    let t = tempfile::tempdir().unwrap();
    let mut ctx = fixture(t.path());
    ctx.continuation_owner = Some("reviewer".into());
    approve(&ctx, "beta");
    let mut req = verdict_request(&ctx);
    req["verdict"]["open_required_findings"] = json!([]);
    req["verdict"]["disposition"] =
        json!({"owner":"task_owner","reason":"Scoring dispute","resume_when":"Owner decides"});
    assert!(
        ctx.execute(req.clone())
            .unwrap_err()
            .to_string()
            .contains("arbiter")
    );
    assert!(!t.path().join("alpha-review.json").exists());
    req["verdict"]["disposition"]["owner"] = json!("arbiter");
    ctx.execute(req).unwrap();
    let candidate = ctx.binding("alpha-candidate.json").unwrap();
    ctx.write_json(
        ".octos/pptx-verification-plan.json",
        &json!({"mode":"independent_visual","arbiter_agent":"session:arbiter-1"}),
    )
    .unwrap();
    let audit = |phase: &str, peer: &str| {
        ctx.execute(json!({"action":"page_dispatch","peers":[{"peer":peer,"state":"running","attached":true,"authorized":true}],
            "assignments":[{"phase":phase,"page":"alpha","peer":peer,"state":"running","candidate_sha256":candidate["sha256"]}]})).unwrap()
    };
    for peer in ["reviewer", "maker-beta", "coordinator", "other-arbiter"] {
        assert_eq!(
            audit("arbitrate", peer)["declared_coverage_complete"],
            false
        );
    }
    assert_eq!(
        audit("arbitrate", "arbiter-1")["declared_coverage_complete"],
        true
    );
    edit(&ctx, "alpha-review.json", |r| {
        r["disposition"]["owner"] = json!("task_owner")
    });
    let legacy = ctx.binding("alpha-review.json").unwrap();
    let result = audit("arbitrate", "arbiter-1");
    assert_eq!(result["needs_disposition"][0]["phase"], "arbitrate");
    assert_eq!(result["accepted_pages"], 1);
    assert_eq!(ctx.binding("alpha-review.json").unwrap(), legacy);
    edit(&ctx, "alpha-review.json", |r| {
        r["open_required_findings"] = json!(["F1"])
    });
    assert_eq!(
        audit("diagnose", "technical-peer")["declared_coverage_complete"],
        true
    );
    assert_eq!(
        audit("diagnose", "maker-alpha")["declared_coverage_complete"],
        false
    );
    assert_eq!(gate(&ctx)["ready_for_assembly"], false);
}

#[test]
fn exhausted_peer_investigation_ends_blocked_and_stale_evidence_reopens_work() {
    let t = tempfile::tempdir().unwrap();
    let mut ctx = fixture(t.path());
    ctx.continuation_owner = Some("reviewer".into());
    approve(&ctx, "beta");
    let mut req = verdict_request(&ctx);
    req["verdict"]["disposition"] = json!({"owner":"tool_maintainer","reason":"Font unavailable","resume_when":"Compatible font is available"});
    let referral = ctx.execute(req.clone()).unwrap();
    req["replaces_verdict_id"] = referral["verdict_id"].clone();
    req["verdict"]["disposition"]["blocked_report"] = json!("result.json");
    assert!(ctx.execute(req.clone()).is_err());
    req["verdict"]["evidence"] = json!(["evidence.json", "result.json"]);
    let mut report = json!({"outcome":"blocked","phase":"diagnose","page":"alpha","candidate_sha256":"obsolete",
        "peer":"technical-peer","attempted_remedy":"Tested authorized fonts","remaining_blocker":"No compatible font available"});
    ctx.write_json("result.json", &report).unwrap();
    assert!(ctx.execute(req.clone()).is_err());
    report["candidate_sha256"] = ctx.binding("alpha-candidate.json").unwrap()["sha256"].clone();
    for peer in ["session:reviewer", "session:coordinator", "maker-beta"] {
        report["peer"] = json!(peer);
        ctx.replace_json("result.json", &report).unwrap();
        assert!(ctx.execute(req.clone()).is_err());
    }
    report["peer"] = json!("technical-peer");
    ctx.replace_json("result.json", &report).unwrap();
    ctx.execute(req.clone()).unwrap();
    ctx.execute(req).unwrap();
    let result = ctx.execute(json!({"action":"page_dispatch"})).unwrap();
    assert_eq!(result["only_blocked_pages_remain"], true);
    assert_eq!(result["blocked_pages"][0]["page"], "alpha");
    assert_eq!(result["ready_unassigned"], json!([]));
    assert_eq!(gate(&ctx)["ready_for_assembly"], false);
    edit(&ctx, "result.json", |r| {
        r["remaining_blocker"] = json!("changed")
    });
    let result = ctx.execute(json!({"action":"page_dispatch"})).unwrap();
    assert_eq!(result["only_blocked_pages_remain"], false);
    assert_eq!(result["ready_unassigned"][0]["phase"], "review");
}

#[test]
fn bind_finished_report_once_on_the_original_candidate_and_review() {
    let t = tempfile::tempdir().unwrap();
    let mut ctx = fixture(t.path());
    ctx.continuation_owner = Some("reviewer".into());
    approve(&ctx, "beta");
    let mut req = verdict_request(&ctx);
    req["verdict"]["disposition"] = json!({"owner":"tool_maintainer","reason":"Font unresolved","resume_when":"Changed font capability"});
    let referral = ctx.execute(req).unwrap();
    let before = ctx.json("alpha-review.json").unwrap();
    let mut bind = json!({"action":"page_review_submit","page":"alpha","blocked_report":"specialist.json",
        "candidate_id":referral["candidate_id"],"replaces_verdict_id":referral["verdict_id"]});
    let mut report = json!({"page_id":"alpha","phase":"diagnose","outcome":"blocked","candidate_sha256":before["candidate_sha256"],
        "peer":"specialist","attempted_remedy":"PENDING — measurement in progress.","remaining_blocker":"PENDING","evidence":[]});
    ctx.write_json("specialist.json", &report).unwrap();
    assert!(ctx.execute(bind.clone()).is_err());
    report["attempted_remedy"] = json!("Measured available shapes");
    report["remaining_blocker"] = json!("Required shape unavailable");
    ctx.replace_json("specialist.json", &report).unwrap();
    assert!(
        ctx.execute(bind.clone()).is_err(),
        "must not attach empty inventory"
    );
    report["evidence"] = json!(["evidence.json"]);
    report["page"] = json!("beta");
    ctx.replace_json("specialist.json", &report).unwrap();
    assert!(ctx.execute(bind.clone()).is_err());
    report.as_object_mut().unwrap().remove("page");
    ctx.replace_json("specialist.json", &report).unwrap();
    ctx.continuation_owner = Some("unrelated reviewer".into());
    assert!(ctx.execute(bind.clone()).is_err());
    ctx.continuation_owner = Some("reviewer".into());
    let after = ctx.execute(bind.clone()).unwrap();
    assert_eq!(ctx.execute(bind.clone()).unwrap(), after);
    bind["replaces_verdict_id"] = gate(&ctx)["pages"][0]["verdict_id"].clone();
    assert_eq!(ctx.execute(bind.clone()).unwrap(), after);
    bind["replaces_verdict_id"] = json!("stale");
    assert!(ctx.execute(bind).is_err());
    let v = ctx.json("alpha-review.json").unwrap();
    for key in [
        "score",
        "open_required_findings",
        "accepted",
        "candidate_sha256",
    ] {
        assert_eq!(v[key], before[key]);
    }
    let dispatch = ctx.execute(json!({"action":"page_dispatch"})).unwrap();
    assert_eq!(dispatch["ready_unassigned"], json!([]));
    assert_eq!(dispatch["handoff"]["automatic_retry_allowed"], false);
    assert_eq!(dispatch["all_work_waiting"], false);
    assert_eq!(gate(&ctx)["ready_for_assembly"], false);
}

#[test]
fn native_submission_preserves_legacy_verdict_and_rejects_changed_sources() {
    let t = tempfile::tempdir().unwrap();
    let mut ctx = fixture(t.path());
    ctx.continuation_owner = Some("reviewer".into());
    let req = verdict_request(&ctx);
    approve(&ctx, "alpha");
    let before = ctx.binding("alpha-review.json").unwrap();
    assert!(
        ctx.execute(req.clone())
            .unwrap_err()
            .to_string()
            .contains("preserve existing")
    );
    assert_eq!(ctx.binding("alpha-review.json").unwrap(), before);
    edit(&ctx, "alpha.json", |p| {
        p["copy"]["label"] = json!("changed")
    });
    assert!(ctx.execute(req).is_err());
    assert_eq!(ctx.binding("alpha-review.json").unwrap(), before);
}

#[test]
fn dispatch_stages_follow_exact_bound_candidates_and_current_reviews() {
    let t = tempfile::tempdir().unwrap();
    let ctx = fixture(t.path());
    assert_eq!(gate(&ctx)["pages"][0]["work_stage"], "review");
    assert_eq!(gate(&ctx)["pages"][0]["candidate_current"], true);
    approve(&ctx, "alpha");
    assert_eq!(gate(&ctx)["pages"][0]["work_stage"], "complete");
    let mut review = ctx.json("alpha-review.json").unwrap();
    review["score"] = json!(90);
    review["accepted"] = json!(false);
    review["open_required_findings"] = json!(["finding-alpha"]);
    ctx.replace_json("alpha-review.json", &review).unwrap();
    assert_eq!(gate(&ctx)["pages"][0]["work_stage"], "repair");
    review["open_required_findings"] = json!([]);
    ctx.replace_json("alpha-review.json", &review).unwrap();
    assert_eq!(gate(&ctx)["pages"][0]["work_stage"], "review");
    assert_eq!(gate(&ctx)["pages"][0]["review_disposition_needed"], true);
    review["candidate_sha256"] = json!("stale");
    ctx.replace_json("alpha-review.json", &review).unwrap();
    assert_eq!(gate(&ctx)["pages"][0]["work_stage"], "review");
    let mut source = ctx.json("alpha.json").unwrap();
    source["copy"]["label"] = json!("changed draft");
    ctx.replace_json("alpha.json", &source).unwrap();
    assert_eq!(gate(&ctx)["pages"][0]["work_stage"], "make");
    assert_eq!(gate(&ctx)["pages"][0]["candidate_current"], false);
}

#[test]
fn native_dispatch_inventory_is_read_only_and_accepted_pages_need_no_assignment() {
    let t = tempfile::tempdir().unwrap();
    let ctx = fixture(t.path());
    approve(&ctx, "alpha");
    let before = ctx.binding("alpha-candidate.json").unwrap();
    let result = ctx.execute(json!({"action":"page_dispatch"})).unwrap();
    assert_eq!(result["ready_unassigned"].as_array().unwrap().len(), 1);
    assert_eq!(result["ready_unassigned"][0]["page"], "beta");
    assert_eq!(result["ready_unassigned"][0]["phase"], "review");
    assert_eq!(result["host_verified"], false);
    assert_eq!(result["declared_coverage_complete"], false);
    assert_eq!(ctx.binding("alpha-candidate.json").unwrap(), before);
    assert!(
        ctx.execute(json!({"action":"help","topic":"page_dispatch"}))
            .unwrap()["documentation"]
            .as_str()
            .unwrap()
            .contains("coordinator declarations")
    );
}

#[test]
fn session_prefix_cannot_disguise_a_maker_or_coordinator_as_an_independent_reviewer() {
    let t = tempfile::tempdir().unwrap();
    let ctx = fixture(t.path());
    approve(&ctx, "alpha");
    let mut review = ctx.json("alpha-review.json").unwrap();
    for disguised in ["session:maker-alpha", "session:coordinator"] {
        review["reviewer_session"] = json!(disguised);
        ctx.replace_json("alpha-review.json", &review).unwrap();
        let result = gate(&ctx);
        assert_eq!(result["accepted_pages"], 0);
        assert_eq!(result["pages"][0]["work_stage"], "review");
        assert!(
            result["pages"][0]["reason"]
                .as_str()
                .unwrap()
                .contains("independent")
        );
    }
    let mut plan = ctx.json(".octos/pptx-page-plan.json").unwrap();
    plan["pages"][1]["maker_session"] = json!("session:maker-alpha");
    ctx.replace_json(".octos/pptx-page-plan.json", &plan)
        .unwrap();
    assert!(
        ctx.execute(json!({"action":"page_gate"}))
            .unwrap_err()
            .to_string()
            .contains("duplicate maker_session")
    );
}

fn publish_alpha(ctx: &Context) -> anyhow::Result<Value> {
    ctx.execute(json!({"action":"page_publish","page":"alpha",
        "cycle":"seed/cycle.json","artifact":"postbuild.pptx",
        "render_receipt":"alpha-render/render.json",
        "receipts":["alpha-postbuild-receipt.json"]}))
}

#[test]
fn should_publish_bound_current_artifacts_for_independent_review() {
    let t = tempfile::tempdir().unwrap();
    let mut ctx = fixture(t.path());
    ctx.continuation_owner = Some("session:maker-alpha".into());
    let result = publish_alpha(&ctx).unwrap();
    assert_eq!(result["ready_for_independent_review"], true);
    assert_eq!(result["quality_approved"], false);
    assert_eq!(result["notification_sent"], false);
    let candidate = ctx.json("alpha-candidate.json").unwrap();
    assert!(
        candidate["dependencies"]
            .as_array()
            .unwrap()
            .contains(&ctx.binding("art.png").unwrap()),
        "native publication must use workspace-relative asset bindings for host handoff"
    );
    assert_eq!(
        candidate["artifact"],
        ctx.binding("postbuild.pptx").unwrap()
    );
    assert_eq!(
        result["handoff"]["candidate"],
        ctx.binding("alpha-candidate.json").unwrap()
    );
    assert!(!candidate.as_object().unwrap().contains_key("score"));
    assert_eq!(gate(&ctx)["accepted_pages"], 0);
}

#[test]
fn should_reject_wrong_owner_and_stale_assets_without_replacing_candidate() {
    let t = tempfile::tempdir().unwrap();
    let mut ctx = fixture(t.path());
    let before = ctx.binding("alpha-candidate.json").unwrap();
    ctx.continuation_owner = Some("session:maker-beta".into());
    assert!(
        publish_alpha(&ctx)
            .unwrap_err()
            .to_string()
            .contains("owner")
    );
    ctx.continuation_owner = Some("session:maker-alpha".into());
    let mut art = std::fs::read(t.path().join("art.png")).unwrap();
    art.push(b' ');
    std::fs::write(t.path().join("art.png"), art).unwrap();
    assert!(publish_alpha(&ctx).is_err());
    assert_eq!(ctx.binding("alpha-candidate.json").unwrap(), before);
}

#[test]
fn should_preserve_an_accepted_candidate_until_reviewer_release() {
    let t = tempfile::tempdir().unwrap();
    let mut ctx = fixture(t.path());
    ctx.continuation_owner = Some("session:maker-alpha".into());
    approve(&ctx, "alpha");
    let before = ctx.binding("alpha-candidate.json").unwrap();
    let error = publish_alpha(&ctx).unwrap_err();
    assert!(error.to_string().contains("accepted candidate"));
    assert_eq!(ctx.binding("alpha-candidate.json").unwrap(), before);
}

#[test]
fn bound_artwork_from_the_task_plan_cannot_be_substituted_by_the_maker() {
    let t = tempfile::tempdir().unwrap();
    let ctx = fixture(t.path());
    let mut p = ctx.json(".octos/pptx-page-plan.json").unwrap();
    p["pages"][0]["approved_background"] = ctx.binding("design.png").unwrap();
    ctx.replace_json(".octos/pptx-page-plan.json", &p).unwrap();
    assert_eq!(gate(&ctx)["pages"][0]["candidate_current"], true);
    image::RgbImage::from_pixel(40, 30, image::Rgb([85, 85, 85]))
        .save(t.path().join("other.png"))
        .unwrap();
    p["pages"][0]["approved_background"] = ctx.binding("other.png").unwrap();
    ctx.replace_json(".octos/pptx-page-plan.json", &p).unwrap();
    let result = gate(&ctx);
    assert_eq!(result["pages"][0]["candidate_current"], false);
    assert!(
        result["pages"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("approved artwork")
    );
}

#[test]
fn frozen_publication_survives_draft_edits_and_still_detects_snapshot_changes() {
    let t = tempfile::tempdir().unwrap();
    let mut ctx = fixture(t.path());
    ctx.continuation_owner = Some("maker-alpha".into());
    edit(&ctx, "deck.json", |root| {
        root["canvas"]["width_px"] = json!(320);
        root["canvas"]["height_px"] = json!(240);
    });
    let result = ctx
        .execute(
            json!({"action":"cycle","page":"alpha","spec":"deck.json","output":"pass",
        "publish_candidate":true,"freeze_inputs":true}),
        )
        .unwrap();
    let published = &result["page_publication"]["handoff"];
    let source = published["source"]["path"].as_str().unwrap();
    assert!(source.starts_with(".octos/page-sources/"));
    assert_eq!(
        result["peer_review_handoff"]["candidate_id"],
        published["candidate_id"]
    );
    approve(&ctx, "alpha");
    edit(&ctx, "alpha.json", |page| {
        page["notes"] = json!("draft notes")
    });
    assert_eq!(gate(&ctx)["accepted_pages"], 1);
    edit(&ctx, source, |page| {
        page["notes"] = json!("changed frozen input")
    });
    assert_eq!(gate(&ctx)["accepted_pages"], 0);
}

#[test]
fn should_publish_in_one_cycle_without_binding_unrelated_page_edits() {
    let t = tempfile::tempdir().unwrap();
    let mut ctx = fixture(t.path());
    ctx.continuation_owner = Some("session:maker-alpha".into());
    let result = ctx
        .execute(json!({"action":"cycle","spec":"deck.json","page":"alpha",
        "output":"published-cycle","width":64,"publish_candidate":true}))
        .unwrap();
    assert_eq!(
        result["page_publication"]["ready_for_independent_review"],
        true
    );
    assert_eq!(
        result["peer_review_handoff"]["candidate"],
        ctx.binding("alpha-candidate.json").unwrap()
    );
    approve(&ctx, "alpha");
    assert_eq!(gate(&ctx)["accepted_pages"], 1);
    edit(&ctx, "beta.json", |page| {
        page["copy"]["label"] = json!("changed elsewhere")
    });
    assert_eq!(
        gate(&ctx)["accepted_pages"],
        1,
        "another maker's page must not invalidate this accepted page"
    );
}

#[test]
fn should_reuse_an_unchanged_accepted_publication_without_replacing_it() {
    let t = tempfile::tempdir().unwrap();
    let mut ctx = fixture(t.path());
    ctx.continuation_owner = Some("session:maker-alpha".into());
    publish_alpha(&ctx).unwrap();
    approve(&ctx, "alpha");
    let before = ctx.binding("alpha-candidate.json").unwrap();
    let result = publish_alpha(&ctx).unwrap();
    assert_eq!(result["candidate"], before);
    assert_eq!(gate(&ctx)["accepted_pages"], 1);
}
fn edit(ctx: &Context, path: &str, f: impl FnOnce(&mut Value)) {
    let mut v = ctx.json(path).unwrap();
    f(&mut v);
    ctx.replace_json(path, &v).unwrap();
}

#[test]
fn missing_page_review_blocks_combined_build_but_allows_one_page() {
    let t = tempfile::tempdir().unwrap();
    let ctx = fixture(t.path());
    approve(&ctx, "alpha");
    let r = gate(&ctx);
    assert_eq!(r["ready_for_assembly"], false);
    assert_eq!(r["accepted_pages"], 1);
    let e = ctx
        .execute(json!({"action":"build","spec":"deck.json","output":"blocked"}))
        .unwrap_err();
    assert!(format!("{e:#}").contains("beta"));
    assert!(!t.path().join("blocked/editable.pptx").exists());
    ctx.execute(json!({"action":"build","spec":"deck.json","page":"beta","output":"one"}))
        .unwrap();
}

#[test]
fn duplicate_page_ownership_and_self_review_are_rejected() {
    let t = tempfile::tempdir().unwrap();
    let ctx = fixture(t.path());
    edit(&ctx, ".octos/pptx-page-plan.json", |p| {
        p["pages"][1]["maker_session"] = json!("maker-alpha")
    });
    assert!(
        ctx.execute(json!({"action":"page_gate"}))
            .unwrap_err()
            .to_string()
            .contains("one page")
    );
    edit(&ctx, ".octos/pptx-page-plan.json", |p| {
        p["pages"][1]["maker_session"] = json!("maker-beta")
    });
    approve(&ctx, "alpha");
    approve(&ctx, "beta");
    edit(&ctx, "alpha-review.json", |p| {
        p["reviewer_session"] = json!("maker-beta")
    });
    assert_eq!(gate(&ctx)["ready_for_assembly"], false);
}

#[test]
fn score_critical_and_required_findings_each_hold_the_gate() {
    let t = tempfile::tempdir().unwrap();
    let ctx = fixture(t.path());
    approve(&ctx, "alpha");
    approve(&ctx, "beta");
    for (key, bad, good) in [
        ("score", json!(94), json!(96)),
        ("critical_defects", json!(["clipped"]), json!([])),
        ("open_required_findings", json!(["wrong data"]), json!([])),
    ] {
        edit(&ctx, "alpha-review.json", |r| r[key] = bad);
        assert_eq!(gate(&ctx)["ready_for_assembly"], false);
        edit(&ctx, "alpha-review.json", |r| r[key] = good);
    }
    assert_eq!(gate(&ctx)["ready_for_assembly"], true);
}

#[test]
fn changed_source_image_receipt_or_candidate_requires_fresh_evidence() {
    let t = tempfile::tempdir().unwrap();
    let ctx = fixture(t.path());
    approve(&ctx, "alpha");
    approve(&ctx, "beta");
    for path in [
        "alpha.json",
        "art.png",
        "alpha-render/slide-001.png",
        "design.png",
        "evidence.json",
        "postbuild.pptx",
    ] {
        let p = t.path().join(path);
        let original = std::fs::read(&p).unwrap();
        let mut changed = original.clone();
        changed.push(b' ');
        std::fs::write(&p, &changed).unwrap();
        assert_eq!(gate(&ctx)["ready_for_assembly"], false);
        std::fs::write(&p, &original).unwrap();
    }
    edit(&ctx, "alpha-candidate.json", |p| {
        p["artifact"] = ctx.binding("seed/editable.pptx").unwrap()
    });
    assert_eq!(gate(&ctx)["ready_for_assembly"], false);
}

#[test]
fn a_ready_gate_does_not_award_visual_quality_and_checks_source_order() {
    let t = tempfile::tempdir().unwrap();
    let ctx = fixture(t.path());
    approve(&ctx, "alpha");
    approve(&ctx, "beta");
    let r = gate(&ctx);
    assert_eq!(r["ready_for_assembly"], true);
    assert_eq!(r["quality_approved"], false);
    assert_eq!(r["agent_execution_verified"], false);
    ctx.execute(json!({"action":"build","spec":"deck.json","output":"good"}))
        .unwrap();
    edit(&ctx, "deck.json", |p| {
        p["pages"].as_array_mut().unwrap().reverse()
    });
    assert!(
        ctx.execute(json!({"action":"build","spec":"deck.json","output":"wrong-order"}))
            .is_err()
    );
}

#[test]
fn batch_and_nested_operations_cannot_export_a_combined_unapproved_probe() {
    let t = tempfile::tempdir().unwrap();
    let ctx = fixture(t.path());
    let request = json!({"action":"batch","input":"seed/editable.pptx","output":"probe.pptx",
        "commands":[{"command":"slide.new","params":{}}]});
    assert!(ctx.execute(request.clone()).is_err());
    assert!(!t.path().join("probe.pptx").exists());
    let r = ctx
        .execute(json!({"action":"operations","steps":[request]}))
        .unwrap();
    assert_eq!(r["tool_success"], false);
    assert!(!t.path().join("probe.pptx").exists());
    approve(&ctx, "alpha");
    approve(&ctx, "beta");
    ctx.execute(
        json!({"action":"batch","input":"seed/editable.pptx","output":"combined.pptx",
        "commands":[{"command":"slide.new","params":{}}]}),
    )
    .unwrap();
}

#[test]
fn ordinary_workspaces_without_a_page_plan_keep_existing_behavior() {
    let t = tempfile::tempdir().unwrap();
    let ctx = fixture(t.path());
    std::fs::remove_file(t.path().join(".octos/pptx-page-plan.json")).unwrap();
    assert_eq!(gate(&ctx)["active"], false);
    ctx.execute(json!({"action":"build","spec":"deck.json","output":"ordinary"}))
        .unwrap();
}

#[test]
fn a_multislide_page_candidate_and_an_extra_assembly_page_are_rejected() {
    let t = tempfile::tempdir().unwrap();
    let ctx = fixture(t.path());
    approve(&ctx, "alpha");
    approve(&ctx, "beta");
    ctx.execute(json!({"action":"build","spec":"deck.json","output":"two"}))
        .unwrap();
    assert!(
        ctx.execute(
            json!({"action":"batch","input":"two/editable.pptx","output":"extra.pptx",
        "commands":[{"command":"slide.new","params":{}}]})
        )
        .is_err()
    );
    assert!(!t.path().join("extra.pptx").exists());
    edit(&ctx, "alpha-candidate.json", |c| {
        c["artifact"] = ctx.binding("two/editable.pptx").unwrap()
    });
    edit(&ctx, "alpha-review.json", |r| {
        r["candidate_sha256"] = ctx.binding("alpha-candidate.json").unwrap()["sha256"].clone()
    });
    let r = gate(&ctx);
    assert_eq!(r["ready_for_assembly"], false);
    assert!(
        r["pages"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("exactly one")
    );
}

#[test]
fn substituted_assembly_source_and_missing_review_evidence_are_rejected() {
    let t = tempfile::tempdir().unwrap();
    let ctx = fixture(t.path());
    approve(&ctx, "alpha");
    approve(&ctx, "beta");
    std::fs::copy(
        t.path().join("alpha.json"),
        t.path().join("other-alpha.json"),
    )
    .unwrap();
    edit(&ctx, "deck.json", |d| {
        d["pages"][0]["path"] = json!("other-alpha.json")
    });
    let e = ctx
        .execute(json!({"action":"build","spec":"deck.json","output":"wrong-source"}))
        .unwrap_err();
    assert!(format!("{e:#}").contains("source differs"));
    edit(&ctx, "alpha-review.json", |r| r["evidence"] = json!([]));
    assert_eq!(gate(&ctx)["ready_for_assembly"], false);
}

#[test]
fn changed_shared_styles_canvas_and_omitted_artwork_binding_do_not_reuse_acceptance() {
    let t = tempfile::tempdir().unwrap();
    let ctx = fixture(t.path());
    approve(&ctx, "alpha");
    approve(&ctx, "beta");
    let original = ctx.json("deck.json").unwrap();
    for (name, changed) in [
        ("styles", {
            let mut d = original.clone();
            d["styles"]["body"]["size_pt"] = json!(18);
            d
        }),
        ("canvas", {
            let mut d = original.clone();
            d["canvas"]["width_in"] = json!(8);
            d["canvas"]["height_in"] = json!(6);
            d
        }),
    ] {
        ctx.replace_json("deck.json", &changed).unwrap();
        let e = ctx
            .execute(
                json!({"action":"build","spec":"deck.json","output":format!("changed-{name}")}),
            )
            .unwrap_err();
        assert!(format!("{e:#}").contains("canvas/styles"), "{name}: {e:#}");
    }
    ctx.replace_json("deck.json", &original).unwrap();
    edit(&ctx, "alpha-candidate.json", |c| {
        c["dependencies"] = json!([])
    });
    edit(&ctx, "alpha-review.json", |r| {
        r["candidate_sha256"] = ctx.binding("alpha-candidate.json").unwrap()["sha256"].clone()
    });
    assert_eq!(gate(&ctx)["ready_for_assembly"], true);
    image::RgbImage::from_pixel(40, 30, image::Rgb([200, 10, 10]))
        .save(t.path().join("art.png"))
        .unwrap();
    assert_eq!(gate(&ctx)["ready_for_assembly"], false);
}

#[test]
fn a_render_receipt_for_the_base_cannot_attest_to_a_changed_postbuild_candidate() {
    let t = tempfile::tempdir().unwrap();
    let ctx = fixture(t.path());
    approve(&ctx, "alpha");
    approve(&ctx, "beta");
    let changed=ctx.execute(json!({"action":"batch","input":"postbuild.pptx","output":"repaired.pptx",
        "commands":[{"command":"slide.go","params":{"index":0}},
            {"command":"shape.insert","params":{"preset":"rect","rect":[1,1,10,10],"text":"patch"}}]})).unwrap();
    ctx.write_json("new-repair-receipt.json", &changed).unwrap();
    edit(&ctx, "alpha-candidate.json", |c| {
        c["artifact"] = ctx.binding("repaired.pptx").unwrap();
        c["receipts"]
            .as_array_mut()
            .unwrap()
            .push(ctx.binding("new-repair-receipt.json").unwrap());
    });
    edit(&ctx, "alpha-review.json", |r| {
        r["candidate_sha256"] = ctx.binding("alpha-candidate.json").unwrap()["sha256"].clone()
    });
    let r = gate(&ctx);
    assert_eq!(r["ready_for_assembly"], false);
    assert!(
        r["pages"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("render receipt belongs")
    );
}

#[test]
fn page_gate_help_and_capability_are_discoverable_in_the_public_schema() {
    let manifest: Value = serde_json::from_str(include_str!("../manifest.json")).unwrap();
    assert!(
        manifest["tools"][0]["input_schema"]["properties"]["topic"]["enum"]
            .as_array()
            .unwrap()
            .contains(&json!("page_gate"))
    );
    let t = tempfile::tempdir().unwrap();
    let ctx = fixture(t.path());
    assert_eq!(
        ctx.execute(json!({"action":"help","topic":"page_gate"}))
            .unwrap()["topic"],
        "page_gate"
    );
    let doctor = ctx.execute(json!({"action":"doctor"})).unwrap();
    assert_eq!(doctor["page_first_gate"]["supported"], true);
    assert_eq!(doctor["page_first_gate"]["active"], true);
}

#[test]
fn page_first_cycle_uses_the_declared_canvas_and_respects_explicit_width() {
    let t = tempfile::tempdir().unwrap();
    let ctx = fixture(t.path());
    // The native viewport comes from the declared design, even with 40x30 artwork.
    edit(&ctx, "deck.json", |root| {
        root["canvas"] = json!({"width_px":320,"height_px":80,"width_in":8,"height_in":2});
    });
    for (dir, width, expected) in [
        ("native", None, (320, 80)),
        ("preview", Some(160), (160, 40)),
        ("standalone", None, (1920, 480)),
    ] {
        if dir == "standalone" {
            std::fs::remove_file(t.path().join(".octos/pptx-page-plan.json")).unwrap();
        }
        let mut request = json!({"action":"cycle","spec":"deck.json","page":"alpha","output":dir});
        if let Some(width) = width {
            request["width"] = json!(width);
        }
        let cycle = ctx.execute(request).unwrap();
        if dir == "native" {
            let render = ctx
                .execute(json!({"action":"render","input":cycle["editable"]["path"],
                "output":"raw-render"}))
                .unwrap();
            assert_eq!(render["pages"][0]["width"], 1920);
        }
        for kind in ["editable", "blank"] {
            let page = &cycle["renders"][kind]["pages"][0];
            assert_eq!(
                (page["width"].as_u64(), page["height"].as_u64()),
                (Some(expected.0.into()), Some(expected.1.into()))
            );
            assert_eq!(
                image::image_dimensions(ctx.verify(page).unwrap()).unwrap(),
                expected
            );
        }
    }
}

#[test]
fn page_first_cycle_rejects_unrenderable_canvas_before_creating_an_attempt() {
    let t = tempfile::tempdir().unwrap();
    let ctx = fixture(t.path());
    for (name, width, height, inches) in [
        ("over-limit", 9000, 6750, 4.0),
        ("over-engine-scale", 6400, 4800, 4.0),
        // The PPTX writer clamps physical sides shorter than one inch.
        ("changed-roundtrip-aspect", 320, 240, 0.9),
    ] {
        edit(&ctx, "deck.json", |root| {
            root["canvas"] = json!({"width_px":width,"height_px":height,
                "width_in":inches,"height_in":inches * 0.75});
        });
        let error = ctx
            .execute(json!({"action":"cycle","spec":"deck.json","page":"beta","output":name}))
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("page-first canvas"),
            "{error:#}"
        );
        assert!(!t.path().join(name).exists());
    }
}

#[test]
fn source_assembly_rejects_postbuild_edits_and_artifact_reuse_keeps_the_approved_pages() {
    let t = tempfile::tempdir().unwrap();
    let ctx = fixture(t.path());
    let repair = ctx.execute(json!({"action":"batch","input":"beta-postbuild.pptx","output":"repair.pptx",
        "commands":[{"command":"slide.go","params":{"index":0}},
        {"command":"shape.insert","params":{"preset":"rect","rect":[20,120,160,55],"text":"Approved addition"}}]})).unwrap();
    ctx.write_json("repair.json", &repair).unwrap();
    let render = ctx
        .execute(
            json!({"action":"render","input":"repair.pptx","width":64,"output":"repair-render"}),
        )
        .unwrap();
    edit(&ctx, "beta-candidate.json", |candidate| {
        candidate["artifact"] = repair["output"].clone();
        candidate["render"] = render["pages"][0].clone();
        candidate["render_receipt"] = ctx.binding("repair-render/render.json").unwrap();
        candidate["receipts"]
            .as_array_mut()
            .unwrap()
            .push(ctx.binding("repair.json").unwrap());
    });
    approve(&ctx, "alpha");
    approve(&ctx, "beta");
    assert_eq!(gate(&ctx)["ready_for_assembly"], true);
    for action in ["build", "cycle"] {
        let error = ctx
            .execute(json!({"action":action,"spec":"deck.json","output":"lost-edits"}))
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("discard post-build edits on accepted page"),
            "{error:#}"
        );
        assert!(!t.path().join("lost-edits").exists());
    }
    let first = ctx.json("alpha-candidate.json").unwrap();
    let second = ctx.json("beta-candidate.json").unwrap();
    assert!(
        ctx.execute(json!({"action":"page_assemble","output":"wrong.pptx","input":"repair.pptx"}))
            .is_err()
    );
    let assembled = ctx
        .execute(json!({"action":"page_assemble","output":"assembled.pptx"}))
        .unwrap();
    assert_eq!(assembled["pages"][0]["page_id"], "alpha");
    assert_eq!(assembled["pages"][1]["artifact"], second["artifact"]);
    assert_eq!(assembled["requires_final_review"], true);
    assert_eq!(assembled["quality_approved"], false);
    assert!(
        ctx.execute(json!({"action":"page_assemble","output":"assembled.pptx"}))
            .is_err()
    );
    let combined = ctx.execute(json!({"action":"render","input":"assembled.pptx","width":64,"output":"assembled-render"})).unwrap();
    assert_eq!(combined["pages"].as_array().unwrap().len(), 2);
    for (page, accepted) in combined["pages"]
        .as_array()
        .unwrap()
        .iter()
        .zip([first, second])
    {
        assert_eq!(page["sha256"], accepted["render"]["sha256"]);
    }
    let inspected = ctx.execute(json!({"action":"batch","input":"assembled.pptx","commands":[
        {"command":"slide.inspect","params":{"index":0}},{"command":"slide.inspect","params":{"index":1}}]})).unwrap();
    let shapes = |i: usize| inspected["results"][i]["shapes"].as_array().unwrap();
    assert!(shapes(0).iter().any(|s| s["text"] == "alpha"));
    assert!(shapes(1).iter().any(|s| s["text"] == "beta"));
    assert!(shapes(1).iter().any(|s| s["text"] == "Approved addition"));
}

#[test]
fn twenty_independent_native_page_owners_can_publish_review_and_assemble_in_order() {
    use std::sync::Barrier;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let ctx = Context::new(root).unwrap();
    image::RgbImage::from_pixel(96, 72, image::Rgb([210, 222, 225]))
        .save(root.join("plate.png"))
        .unwrap();
    let mut pages = Vec::new();
    for i in 0..20 {
        let id = format!("slide-{i}");
        let spec = json!({"schema_version":1,"canvas":{"width_px":96,"height_px":72,"width_in":4,"height_in":3},
            "styles":{"title":{"font":"Inter","size_pt":10+i}},
            "pages":[{"id":id,"background":"plate.png","copy":{"title":id},
                "texts":[{"id":"title","text":id,"style":"title","box":[1,1,94,66]}]}]});
        ctx.write_json(format!("{id}.json"), &spec).unwrap();
        pages.push(json!({"id":id,"maker_session":format!("maker-{i}"),"reviewer_session":format!("reviewer-{i}"),
            "candidate":format!("{id}-candidate.json"),"review":format!("{id}-verdict.json")}));
    }
    ctx.write_json(
        ".octos/pptx-page-plan.json",
        &json!({"schema_version":1,"min_score":95,
        "coordinator_session":"outer","rubric":{"visual":100},"pages":pages}),
    )
    .unwrap();
    assert!(
        ctx.execute(json!({"action":"page_assemble","output":"early.pptx"}))
            .is_err()
    );
    assert!(!root.join("early.pptx").exists());
    let barrier = Barrier::new(20);
    std::thread::scope(|scope| {
        for i in 0..20 {
            let barrier = &barrier;
            scope.spawn(move || {
                let mut maker = Context::new(root).unwrap();
                maker.continuation_owner = Some(format!("maker-{i}"));
                let id = format!("slide-{i}");
                barrier.wait();
                let made = maker.execute(json!({"action":"cycle","spec":format!("{id}.json"),
                    "output":format!("{id}-build"),"page":id,"publish_candidate":true,"freeze_inputs":true})).unwrap();
                let mut reviewer = Context::new(root).unwrap();
                reviewer.continuation_owner = Some(format!("reviewer-{i}"));
                let decision = json!({"action":"page_review_submit","page":id,
                    "candidate_id":made["peer_review_handoff"]["candidate_id"],
                    "verdict":{"accepted":true,"score":96,"component_scores":{"visual":96},
                        "critical_defects":[],"open_required_findings":[],"evidence":[made["inspection"]["report"]]}});
                let receipt = reviewer.execute(decision.clone()).unwrap();
                assert_eq!(reviewer.execute(decision).unwrap(),receipt);
            });
        }
    });
    assert_eq!(gate(&ctx)["accepted_pages"], 20);
    let result = ctx
        .execute(json!({"action":"page_assemble","output":"complete.pptx"}))
        .unwrap();
    assert_eq!(result["requires_final_review"], true);
    let rendered = ctx
        .execute(json!({"action":"render","input":"complete.pptx","width":96,"output":"renders"}))
        .unwrap();
    for i in 0..20 {
        assert_eq!(result["pages"][i]["page_id"], format!("slide-{i}"));
        assert_eq!(
            rendered["pages"][i]["sha256"], result["pages"][i]["render"]["sha256"],
            "render of slide {i}"
        );
    }
}
