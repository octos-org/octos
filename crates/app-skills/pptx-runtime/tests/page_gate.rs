//! Synthetic workflow admission, not a visual-quality evaluation.
#[path = "support/assembled_delivery.rs"]
mod assembled_delivery;
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
fn fresh_plans_require_rubric_and_accept_task_specific_maxima() {
    let t = tempfile::tempdir().unwrap();
    let ctx = Context::new(t.path()).unwrap();
    let path = ".octos/pptx-page-plan.json";
    let mut p = json!({"schema_version":1,"coordinator_session":"coord",
        "pages":[{"id":"a","maker_session":"maker","candidate":"cand.json","review":"review.json"}]});
    ctx.write_json(path, &p).unwrap();
    for version in [1, 2] {
        p["schema_version"] = json!(version);
        ctx.replace_json(path, &p).unwrap();
        assert!(
            ctx.execute(json!({"action":"page_gate"}))
                .unwrap_err()
                .to_string()
                .contains("rubric")
        );
    }
    p["rubric"] = json!({"content":70,"layout":35});
    ctx.replace_json(path, &p).unwrap();
    assert!(
        ctx.execute(json!({"action":"page_gate"}))
            .unwrap_err()
            .to_string()
            .contains("sum to 100")
    );
    p["rubric"]["layout"] = json!(30);
    ctx.replace_json(path, &p).unwrap();
    let status = gate(&ctx);
    assert_eq!(status["pages"][0]["work_stage"], "make");
    assert_eq!(status["rubric"], p["rubric"]);
    assert_eq!(status["schema_version"], 2);
}

#[test]
fn v1_native_scores_check_reported_arithmetic_without_regrading_other_pages() {
    let t = tempfile::tempdir().unwrap();
    let mut ctx = fixture(t.path());
    approve(&ctx, "beta"); // Historical, independent scalar-only verdict.
    ctx.continuation_owner = Some("reviewer".into());
    let mut req = verdict_request(&ctx);
    req["verdict"]["component_scores"] = json!({"readability":75,"copy":14});
    assert!(
        ctx.execute(req.clone())
            .unwrap_err()
            .to_string()
            .contains("sum of component_scores")
    );
    req["verdict"]["component_scores"]["copy"] = json!(19);
    req["verdict"]["component_maxima"] = json!({"readability":75,"copy":30});
    assert!(
        ctx.execute(req.clone())
            .unwrap_err()
            .to_string()
            .contains("sum to 100")
    );
    req["verdict"]["component_maxima"] = json!({"readability":74,"copy":26});
    assert!(
        ctx.execute(req.clone())
            .unwrap_err()
            .to_string()
            .contains("range")
    );
    assert!(!t.path().join("alpha-review.json").exists());
    req["verdict"]["component_maxima"] = json!({"readability":75,"copy":25});
    ctx.execute(req).unwrap();
    assert_eq!(gate(&ctx)["pages"][1]["work_stage"], "complete");
    let mut p = ctx.json(".octos/pptx-page-plan.json").unwrap();
    p["schema_version"] = json!(2);
    ctx.replace_json(".octos/pptx-page-plan.json", &p).unwrap();
    assert!(
        ctx.execute(json!({"action":"page_gate"}))
            .unwrap_err()
            .to_string()
            .contains("rubric")
    );
}

#[test]
fn scoped_page_gate_matches_its_full_row_without_an_assembly_verdict() {
    let t = tempfile::tempdir().unwrap();
    let ctx = fixture(t.path());
    let full = gate(&ctx);
    let single = ctx
        .execute(json!({"action":"page_gate","only_page":"beta"}))
        .unwrap();
    assert_eq!(single["pages"], json!([full["pages"][1]]));
    assert_eq!(single["plan_page_count"], 2);
    assert_eq!(single["assembly_checked"], false);
    assert!(single.get("ready_for_assembly").is_none());
    assert!(
        ctx.execute(json!({"action":"page_gate","only_page":"missing"}))
            .is_err()
    );
    assert!(
        ctx.execute(json!({"action":"page_gate","only_page":7}))
            .is_err()
    );
}

#[test]
fn original_reviewer_can_release_pinned_pixels_after_the_maker_edits_its_draft() {
    let t = tempfile::tempdir().unwrap();
    let mut ctx = fixture(t.path());
    edit(&ctx, ".octos/pptx-page-plan.json", |p| {
        p["schema_version"] = json!(2);
        p["rubric"] = json!({"test":100});
        p["pages"][0]["reviewer_session"] = json!("reviewer");
    });
    ctx.continuation_owner = Some("reviewer".into());
    let mut req = verdict_request(&ctx);
    req["verdict"] = json!({"score":96,"component_scores":{"test":96},"accepted":true,
        "critical_defects":[],"open_required_findings":[],"evidence":["evidence.json"]});
    let prior = ctx.execute(req.clone()).unwrap();
    edit(&ctx, "alpha.json", |p| {
        p["copy"]["label"] = json!("next draft")
    });
    assert!(ctx.execute(req.clone()).is_err());
    req["verdict"]["accepted"] = json!(false);
    req["verdict"]["open_required_findings"] = json!(["old label"]);
    assert!(
        ctx.execute(req.clone()).is_err(),
        "release must name the previous verdict"
    );
    req["replaces_verdict_id"] = prior["verdict_id"].clone();
    ctx.continuation_owner = Some("another-reviewer".into());
    assert!(ctx.execute(req.clone()).is_err());
    ctx.continuation_owner = Some("reviewer".into());
    let released = ctx.execute(req.clone()).unwrap();
    assert_eq!(released["accepted"], false);
    assert_eq!(ctx.execute(req.clone()).unwrap(), released);
    let candidate = ctx.json("alpha-candidate.json").unwrap();
    std::fs::write(ctx.verify(&candidate["render"]).unwrap(), b"foreign").unwrap();
    assert!(ctx.execute(req).is_err());
}

#[test]
fn assigned_reviewer_can_refresh_only_the_render_and_must_grade_again() {
    let t = tempfile::tempdir().unwrap();
    let mut ctx = fixture(t.path());
    edit(&ctx, ".octos/pptx-page-plan.json", |p| {
        p["schema_version"] = json!(2);
        p["rubric"] = json!({"test":100});
        p["pages"][0]["reviewer_session"] = json!("reviewer");
    });
    ctx.continuation_owner = Some("reviewer".into());
    let mut decision = verdict_request(&ctx);
    decision["verdict"] = json!({"accepted":true,"score":98,"component_scores":{"test":98},
        "open_required_findings":[],"critical_defects":[],"evidence":["evidence.json"]});
    let original = ctx.execute(decision.clone()).unwrap();
    let artifact = ctx.binding("postbuild.pptx").unwrap();
    ctx.execute(json!({"action":"render","input":"postbuild.pptx","output":"recheck","width":128}))
        .unwrap();
    let refresh = json!({"action":"page_publish","page":"alpha","refresh_render_only":true,
        "candidate_id":decision["candidate_id"],"render_receipt":"recheck/render.json"});
    for unauthorized in ["coordinator", "maker-alpha", "other-reviewer"] {
        ctx.continuation_owner = Some(unauthorized.into());
        assert!(ctx.execute(refresh.clone()).is_err());
    }
    ctx.continuation_owner = Some("reviewer".into());
    let mut wrong = refresh.clone();
    wrong["render_receipt"] = json!("beta-render/render.json");
    assert!(ctx.execute(wrong).is_err());
    let next = ctx.execute(refresh.clone()).unwrap();
    assert_eq!(next["artifact"], artifact);
    assert_eq!(next["replaces_verdict_id"], original["verdict_id"]);
    let pending = gate(&ctx)["pages"][0].clone();
    assert_eq!(pending["review_current"], false);
    assert_eq!(pending["replaces_verdict_id"], original["verdict_id"]);
    assert!(pending.get("score").is_none());
    let replay = ctx.execute(refresh).unwrap();
    assert_eq!(replay["changed"], false);
    assert_eq!(replay["candidate_id"], next["candidate_id"]);
    decision["replaces_verdict_id"] = original["verdict_id"].clone();
    assert!(ctx.execute(decision.clone()).is_err());
    decision["candidate_id"] = next["candidate_id"].clone();
    ctx.execute(decision).unwrap();
    assert_eq!(gate(&ctx)["pages"][0]["ready"], true);
}

#[test]
fn submission_checks_all_fields_identity_frozen_rubric_and_evidence_before_writing() {
    let t = tempfile::tempdir().unwrap();
    let mut ctx = fixture(t.path());
    let mut p = ctx.json(".octos/pptx-page-plan.json").unwrap();
    p["schema_version"] = json!(2);
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
    let mut other = ctx.json("alpha-review.json").unwrap();
    other["page_id"] = json!("beta");
    other["candidate_sha256"] = ctx.binding("beta-candidate.json").unwrap()["sha256"].clone();
    ctx.write_json("beta-review.json", &other).unwrap();
    assert_eq!(
        gate(&ctx)["pages"][1]["review_current"],
        false,
        "a copied ledger has no matching native revision"
    );
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
    let policy = json!({"schema_version":1,"owner_agent":"coordinator","task_id":"fixture","evidence_file":"whole-task.json",
        "required_checks":[{"id":"package"},{"id":"copy"},{"id":"artwork"},{"id":"readability"}],"deliverables":["final.pptx"],"stagnant_attempts":3});
    ctx.write_json(".octos/continuation.json", &policy).unwrap();
    ctx.write_json("whole-task.json",&json!({"schema_version":1,"task_id":"fixture","phase":"pending","attempt":0,"checks":[],"artifacts":[]})).unwrap();
    assert!(ctx.execute(json!({"action":"evidence_blocked"})).is_err());
    ctx.continuation_owner = Some("coordinator".into());
    let blocked = ctx.execute(json!({"action":"evidence_blocked"})).unwrap();
    assert_eq!(blocked["attempt"], 1);
    assert_eq!(blocked["quality_approved"], false);
    assert_eq!(
        ctx.execute(json!({"action":"evidence_blocked"})).unwrap(),
        blocked
    );
    let frozen = ctx.json("whole-task.json").unwrap();
    assert_eq!(frozen["phase"], "blocked");
    assert!(
        frozen["artifacts"]
            .as_array()
            .unwrap()
            .contains(&ctx.binding("result.json").unwrap())
    );
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
    assert!(ctx.execute(json!({"action":"evidence_blocked"})).is_err());
    assert_eq!(ctx.json("whole-task.json").unwrap(), frozen);
}

#[test]
fn bind_finished_report_once_on_the_original_candidate_and_review() {
    let t = tempfile::tempdir().unwrap();
    let mut ctx = fixture(t.path());
    ctx.continuation_owner = Some("reviewer".into());
    approve(&ctx, "beta");
    let mut req = verdict_request(&ctx);
    req["verdict"]["disposition"] = json!({"owner":"tool_maintainer","reason":"Font unresolved","resume_when":"Changed font capability"});
    let original = ctx.execute(req.clone()).unwrap();
    req["replaces_verdict_id"] = original["verdict_id"].clone();
    req["verdict"]["disposition"]["owner"] = json!("arbiter");
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
    let mut changed = verdict_request(&ctx);
    changed["replaces_verdict_id"] = referral["verdict_id"].clone();
    changed["verdict"]["open_required_findings"] = json!(["different-finding"]);
    changed["verdict"]["disposition"] = json!({"owner":"tool_maintainer","reason":"Different investigation","resume_when":"New data"});
    let mut invalid = changed.clone();
    invalid["verdict"]["disposition"]["blocked_report"] = json!("specialist.json");
    invalid["verdict"]["evidence"] = json!(["evidence.json", "specialist.json"]);
    assert!(
        ctx.execute(invalid).is_err(),
        "old investigation cannot resolve different findings"
    );
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
fn separately_rendered_candidate_keeps_the_source_the_maker_built() {
    let t = tempfile::tempdir().unwrap();
    let mut ctx = fixture(t.path());
    let build = json!({"action":"build","spec":"deck.json","page":"alpha","output":"frozen-build","freeze_inputs":true});
    ctx.continuation_owner = Some("maker-beta".into());
    assert!(ctx.execute(build.clone()).is_err());
    ctx.continuation_owner = Some("maker-alpha".into());
    let built = ctx.execute(build).unwrap();
    edit(&ctx, "alpha.json", |p| {
        p["copy"]["label"] = json!("new draft");
    });
    ctx.execute(json!({"action":"render","input":built["editable"]["path"],"output":"external-render","width":128})).unwrap();
    ctx.execute(json!({"action":"page_publish","page":"alpha","cycle":"frozen-build/cycle.json","render_receipt":"external-render/render.json"})).unwrap();
    assert_eq!(gate(&ctx)["pages"][0]["candidate_current"], true);
    let candidate = ctx.json("alpha-candidate.json").unwrap();
    assert!(
        std::path::Path::new(candidate["source"]["path"].as_str().unwrap())
            .starts_with(".octos/page-sources")
    );
    assert_eq!(
        ctx.json(ctx.verify(&candidate["source"]).unwrap()).unwrap()["copy"]["label"],
        "alpha"
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
    assert!(std::path::Path::new(source).starts_with(".octos/page-sources"));
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
    assert_eq!(
        ctx.execute(json!({"action":"page_assemble","output":"assembled.pptx"}))
            .unwrap(),
        assembled
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
fn assembly_refreshes_only_the_review_without_rewriting_deck_or_render() {
    let t = tempfile::tempdir().unwrap();
    let mut ctx = fixture(t.path());
    ctx.continuation_owner = Some("reviewer".into());
    let mut verdict = verdict_request(&ctx);
    verdict["verdict"]["accepted"] = json!(true);
    verdict["verdict"]["score"] = json!(96);
    verdict["verdict"]["open_required_findings"] = json!([]);
    let first_verdict = ctx.execute(verdict.clone()).unwrap();
    approve(&ctx, "beta");
    let assemble = json!({"action":"page_assemble","output":"final/deck.pptx"});
    let first = ctx.execute(assemble.clone()).unwrap();
    let deck = ctx.verify(&first["output"]).unwrap();
    let modified = std::fs::metadata(&deck).unwrap().modified().unwrap();
    let render = ctx
        .execute(
            json!({"action":"render","input":"final/deck.pptx","width":64,"output":"final/render"}),
        )
        .unwrap();

    verdict["replaces_verdict_id"] = first_verdict["verdict_id"].clone();
    verdict["verdict"]["score"] = json!(97);
    let second_verdict = ctx.execute(verdict.clone()).unwrap();
    let updated = ctx.execute(assemble.clone()).unwrap();
    assert_eq!(updated["pages"][0]["review"], second_verdict["review"]);
    assert_eq!(first["output"], updated["output"]);
    assert_eq!(
        std::fs::metadata(&deck).unwrap().modified().unwrap(),
        modified
    );
    assert!(ctx.verify(&first["receipt"]).is_err());
    let persisted = ctx.json(ctx.verify(&updated["receipt"]).unwrap()).unwrap();
    assert_eq!(persisted["pages"], updated["pages"]);
    for page in render["pages"].as_array().unwrap() {
        ctx.verify(page).unwrap();
    }
    assert_eq!(ctx.execute(assemble.clone()).unwrap(), updated);

    verdict["replaces_verdict_id"] = second_verdict["verdict_id"].clone();
    verdict["verdict"]["accepted"] = json!(false);
    verdict["verdict"]["open_required_findings"] = json!(["readability"]);
    ctx.execute(verdict).unwrap();
    assert!(ctx.execute(assemble).is_err());
    ctx.verify(&updated["receipt"]).unwrap();
    ctx.verify(&updated["output"]).unwrap();
}

#[test]
fn assembly_rejects_unbound_or_changed_output_plan_and_candidate_on_replay() {
    let t = tempfile::tempdir().unwrap();
    let ctx = fixture(t.path());
    approve(&ctx, "alpha");
    approve(&ctx, "beta");
    let call = json!({"action":"page_assemble","output":"final/deck.pptx"});
    ctx.write_new("final/deck.pptx", b"existing user output")
        .unwrap();
    assert!(ctx.execute(call.clone()).is_err());
    assert_eq!(
        ctx.read("final/deck.pptx").unwrap(),
        b"existing user output"
    );
    std::fs::remove_file(t.path().join("final/deck.pptx")).unwrap();
    let assembled = ctx.execute(call.clone()).unwrap();
    let plan = ctx.json(".octos/pptx-page-plan.json").unwrap();
    edit(&ctx, ".octos/pptx-page-plan.json", |p| {
        p["pages"].as_array_mut().unwrap().reverse()
    });
    assert!(ctx.execute(call.clone()).is_err());
    ctx.replace_json(".octos/pptx-page-plan.json", &plan)
        .unwrap();
    let candidate = ctx.json("beta-candidate.json").unwrap();
    edit(&ctx, "beta-candidate.json", |c| {
        c["comment"] = json!("new candidate")
    });
    edit(&ctx, "beta-review.json", |r| {
        r["candidate_sha256"] = ctx.binding("beta-candidate.json").unwrap()["sha256"].clone()
    });
    assert!(
        ctx.execute(call.clone()).is_err(),
        "changed current candidate with new approval"
    );
    ctx.replace_json("beta-candidate.json", &candidate).unwrap();
    edit(&ctx, "beta-review.json", |r| {
        r["candidate_sha256"] = ctx.binding("beta-candidate.json").unwrap()["sha256"].clone()
    });
    std::fs::write(t.path().join("final/deck.pptx"), b"modified after assembly").unwrap();
    assert!(ctx.execute(call).is_err());
    assert_eq!(
        ctx.read("final/deck.pptx").unwrap(),
        b"modified after assembly"
    );
    ctx.verify(&assembled["receipt"]).unwrap();
}

#[test]
fn assembly_resumes_if_receipt_was_published_before_the_output() {
    let t = tempfile::tempdir().unwrap();
    let ctx = fixture(t.path());
    approve(&ctx, "alpha");
    approve(&ctx, "beta");
    let request = json!({"action":"page_assemble","output":"deck.pptx"});
    let receipt = ctx.execute(request.clone()).unwrap();
    std::fs::remove_file(ctx.verify(&receipt["output"]).unwrap()).unwrap();
    assert_eq!(ctx.execute(request).unwrap(), receipt);
    ctx.verify(&receipt["output"]).unwrap();
}

#[test]
fn repeated_render_reuses_matching_previews_and_recovers_interrupted_publication() {
    let t = tempfile::tempdir().unwrap();
    let ctx = fixture(t.path());
    let render =
        json!({"action":"render","input":"postbuild.pptx","width":64,"output":"alpha-render"});
    let receipt = "alpha-render/render.json";
    let mut initial = ctx.json(receipt).unwrap();
    let png = ctx.verify(&initial["pages"][0]).unwrap();
    let modified = std::fs::metadata(&png).unwrap().modified().unwrap();
    assert_eq!(ctx.execute(render.clone()).unwrap(), initial);
    let mut different_width = render.clone();
    different_width["width"] = json!(320);
    assert!(ctx.execute(different_width.clone()).is_err());

    initial.as_object_mut().unwrap().remove("requested_width");
    ctx.replace_json(receipt, &initial).unwrap();
    assert_eq!(
        ctx.execute(render.clone()).unwrap(),
        initial,
        "legacy receipt"
    );
    assert!(
        ctx.execute(different_width).is_err(),
        "legacy receipt geometry"
    );
    let mut different_input = render.clone();
    different_input["input"] = json!("beta-postbuild.pptx");
    assert!(ctx.execute(different_input).is_err());
    std::fs::remove_file(ctx.path(receipt).unwrap()).unwrap();
    let recovered = ctx.execute(render.clone()).unwrap();
    assert_eq!(recovered["pages"], initial["pages"]);
    assert_eq!(
        std::fs::metadata(&png).unwrap().modified().unwrap(),
        modified
    );

    std::fs::write(&png, b"modified preview").unwrap();
    assert!(ctx.execute(render.clone()).is_err());
    std::fs::remove_file(ctx.path(receipt).unwrap()).unwrap();
    assert!(
        ctx.execute(render).is_err(),
        "partial render cannot adopt changed bytes"
    );
    assert_eq!(std::fs::read(&png).unwrap(), b"modified preview");
}

#[test]
fn twenty_independent_native_page_owners_can_publish_review_and_assemble_in_order() {
    run_twenty_pages(false);
}

#[test]
fn twenty_independent_page_reviews_finish_at_compile_without_a_deck_render() {
    run_twenty_pages(true);
}

#[test]
fn image_only_roster_shares_the_physical_canvas_before_any_page_is_rendered() {
    let tmp = tempfile::tempdir().unwrap();
    let mut ctx = Context::new(tmp.path()).unwrap();
    ctx.continuation_owner = Some("session:outer".into());
    image::RgbImage::from_pixel(96, 72, image::Rgb([210, 222, 225]))
        .save_with_format(tmp.path().join("ref.png"), image::ImageFormat::Jpeg)
        .unwrap();
    let rows: Vec<_> = ["a", "b"].into_iter().map(|id| json!({"id":id,"spec":format!("{id}.json"),"reference":"ref.png",
        "candidate":format!("{id}-candidate.json"),"review":format!("{id}-review.json"),"maker_session":format!("maker-{id}"),"reviewer_session":format!("review-{id}")})).collect();
    ctx.write_json(".octos/pptx-page-plan.json", &json!({"schema_version":2,"coordinator_session":"outer","rubric":{"visual":100},"pages":rows})).unwrap();
    ctx.execute(json!({"action":"evidence_init","task_id":"image-canvas","stagnant_attempts":3,"deliverables":["final.pptx","qa.json"]})).unwrap();
    let canvas = ctx.json(".octos/pptx-verification-plan.json").unwrap()["canvas"].clone();
    assert_eq!(
        (canvas["width_px"].as_f64(), canvas["height_px"].as_f64()),
        (Some(96.), Some(72.))
    );
    ctx.execute(json!({"action":"page_assignments"})).unwrap();
    let batch = ctx.json(".octos/pptx-peer-batch.json").unwrap();
    for job in batch["jobs"].as_array().unwrap() {
        let (_, payload) = job["message"]
            .as_str()
            .unwrap()
            .split_once("Page assignment\n")
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(payload).unwrap()["canvas"],
            canvas
        );
    }
    ctx.continuation_owner = Some("maker-a".into());
    let mut spec = json!({"schema_version":1,"canvas":{"width_px":96,"height_px":72,"width_in":4,"height_in":3},"styles":{"title":{"font":"Inter","size_pt":10}},
        "pages":[{"id":"a","background":"ref.png","copy":{"title":"Life"},"texts":[{"id":"title","text":"Life","style":"title","box":[0,0,96,60]}]}]});
    ctx.write_json("a.json", &spec).unwrap();
    let run = json!({"action":"cycle","spec":"a.json","page":"a","width":96,"output":"one","freeze_inputs":true,"publish_candidate":true});
    assert!(
        ctx.execute(run.clone())
            .unwrap_err()
            .to_string()
            .contains("frozen deck canvas")
    );
    assert!(!tmp.path().join("one").exists());
    spec["canvas"] = canvas;
    ctx.replace_json("a.json", &spec).unwrap();
    let receipt = ctx.execute(run).unwrap();
    assert!(receipt["peer_review_handoff"]["candidate_id"].is_string());
}

fn run_twenty_pages(per_page: bool) {
    use std::sync::Barrier;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let mut ctx = Context::new(root).unwrap();
    ctx.continuation_owner = Some("session:outer".into());
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
            "spec":format!("{id}.json"),"instructions":format!("Work only on {id}."),
            "candidate":format!("{id}-candidate.json"),"review":format!("{id}-verdict.json")}));
    }
    ctx.write_json(
        ".octos/pptx-page-plan.json",
        &json!({"schema_version":2,"min_score":95,
        "coordinator_session":"outer","rubric":{"visual":100},"pages":pages}),
    )
    .unwrap();
    if per_page {
        ctx.execute(json!({"action":"evidence_init","task_id":"twenty",
            "stagnant_attempts":3,"deliverables":["complete.pptx","qa.json"]}))
            .unwrap();
        let request = json!({"action":"page_assignments"});
        let batch = ctx.execute(request.clone()).unwrap();
        assert_eq!(batch["assignments"], 20);
        assert_eq!(batch["dispatched"], false);
        let path = ".octos/pptx-peer-batch.json";
        assert_eq!(
            ctx.binding(path).unwrap()["sha256"],
            batch["peer_send_input"]["pptx_batch"]
        );
        let prepared = ctx.json(path).unwrap();
        for (i, job) in prepared["jobs"].as_array().unwrap().iter().enumerate() {
            assert_eq!(job["phase"], "make");
            assert_eq!(job["peer"], format!("maker-{i}"));
            let (_, data) = job["message"]
                .as_str()
                .unwrap()
                .split_once("Page assignment\n")
                .unwrap();
            let row: Value = serde_json::from_str(data).unwrap();
            assert_eq!(row["page"], format!("slide-{i}"));
            assert_eq!(row["spec"], format!("slide-{i}.json"));
        }
        assert_eq!(ctx.execute(request).unwrap(), batch);
        let resume =
            json!({"action":"page_assignments","only_page":"slide-3","resume_after_round":1});
        assert!(
            ctx.execute(json!({"action":"page_assignments","resume_after_round":1}))
                .is_err()
        );
        assert_eq!(ctx.execute(resume.clone()).unwrap()["assignments"], 1);
        let recovery = &ctx.json(path).unwrap()["jobs"][0];
        assert_eq!(recovery["peer"], "maker-3");
        assert_eq!(recovery["resume_after_round"], 1);
        assert_ne!(
            ctx.execute(resume).unwrap()["peer_send_input"],
            batch["peer_send_input"]
        );
        let mut wrong = Context::new(root).unwrap();
        wrong.continuation_owner = Some("maker-1".into());
        assert!(wrong.execute(json!({"action":"page_assignments"})).is_err());
    }
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
                let mut request = json!({"action":"cycle","spec":format!("{id}.json"),
                    "output":format!("{id}-build"),"page":id,"publish_candidate":true,"freeze_inputs":true});
                let separate_publication = per_page && i == 3;
                if separate_publication {
                    request.as_object_mut().unwrap().remove("freeze_inputs");
                    request.as_object_mut().unwrap().remove("publish_candidate");
                }
                let mut made = maker.execute(request).unwrap();
                if separate_publication {
                    let published = maker.execute(json!({"action":"page_publish","page":id,"cycle":format!("{id}-build/cycle.json")})).unwrap();
                    made["peer_review_handoff"] = published["handoff"].clone();
                    let source = published["handoff"]["source"]["path"].as_str().unwrap();
                    assert!(std::path::Path::new(source).starts_with(".octos/page-sources"));
                    let path = format!("{id}.json");
                    let mut next_draft = maker.json(&path).unwrap();
                    next_draft["pages"][0]["notes"] = json!("A private draft for a future revision");
                    maker.replace_json(&path, &next_draft).unwrap();
                }
                let mut reviewer = Context::new(root).unwrap();
                reviewer.continuation_owner = Some(format!("reviewer-{i}"));
                let mut decision = json!({"action":"page_review_submit","page":id,
                    "candidate_id":made["peer_review_handoff"]["candidate_id"],
                    "verdict":{"accepted":true,"score":96,"component_scores":{"visual":96},
                        "critical_defects":[],"open_required_findings":[],"evidence":[made["inspection"]["report"]]}});
                if per_page {
                    assert!(reviewer.execute(decision.clone()).unwrap_err().to_string().contains("pass and basis"));
                    let template = &made["peer_review_handoff"]["review_request_template"];
                    assert_eq!(template["candidate_id"], decision["candidate_id"]);
                    assert!(template["verdict"]["accepted"].is_null());
                    assert!(template["verdict"]["component_scores"].get("visual").is_some());
                    assert!(reviewer.execute(template.clone()).is_err());
                    let mut outcomes = template["verdict"]["delivery_checks"].clone();
                    assert_eq!(outcomes.as_object().unwrap().len(), 4);
                    for domain in ["copy","artwork","readability","editability"] {
                        outcomes[domain] = json!({"status":"pass","basis":format!("test observation for {id}/{domain}")});
                    }
                    decision["verdict"]["delivery_checks"] = outcomes;
                    if i == 3 {
                        let mut coordinator = Context::new(root).unwrap();
                        coordinator.continuation_owner = Some("session:outer".into());
                        let assignment = || {
                            let prepared = coordinator.execute(json!({"action":"page_assignments","only_page":id})).unwrap();
                            assert_eq!(prepared["assignments"], 1);
                            coordinator.json(".octos/pptx-peer-batch.json").unwrap()["jobs"][0].clone()
                        };
                        let review_job = assignment();
                        // A delayed maker turn must leave the exact candidate being reviewed in place.
                        let cycle_receipt = format!("{id}-build/cycle.json");
                        let replay = maker.execute(json!({"action":"page_publish","page":id,"cycle":cycle_receipt})).unwrap();
                        assert_eq!(replay["candidate_id"], decision["candidate_id"]);
                        let stale = maker.execute(json!({"action":"cycle","spec":format!("{id}.json"),"page":id,
                            "output":format!("{id}-premature"),"publish_candidate":true,"freeze_inputs":true})).unwrap_err();
                        assert!(stale.to_string().contains("awaiting its assigned reviewer"));
                        assert!(!root.join(format!("{id}-premature")).exists());
                        assert_eq!(assignment()["candidate_sha256"], review_job["candidate_sha256"]);
                        assert_eq!(review_job["phase"], "review");
                        assert_eq!(review_job["peer"], "reviewer-3");
                        assert!(review_job["message"].as_str().unwrap().contains(decision["candidate_id"].as_str().unwrap()));
                        let mut reject = decision.clone();
                        reject["verdict"]["accepted"] = json!(false);
                        reject["verdict"]["delivery_checks"]["copy"] = json!({"status":"fail","basis":"test correction needed"});
                        let title = json!({"id":"copy-title","kind":"visible","box":[0,0,40,20],"impact":"The fixture title cannot be read at presentation size"});
                        for unsupported in [
                            json!("RGB distance must be below 1"),
                            json!({"id":"color","kind":"requirement","rule":"rgb-distance-one","impact":"Measured distance exceeds one"}),
                            json!({"id":"fake-native","kind":"native_qa"}),
                            json!({"id":"copy-title","kind":"visible","impact":"outside the page","box":[20000,0,20,10]}),
                        ] {
                            reject["verdict"]["open_required_findings"] = json!([unsupported]);
                            assert!(reviewer.execute(reject.clone()).is_err());
                            assert_eq!(assignment()["phase"], "review");
                        }
                        reject["verdict"]["open_required_findings"] = json!([title]);
                        let disputed = reviewer.execute(reject.clone()).unwrap();
                        let referrals = coordinator.execute(json!({"action":"page_assignments","only_page":id})).unwrap();
                        assert_eq!(referrals["assignments"], 1);
                        let referral = assignment();
                        assert_eq!(referral["peer"], "reviewer-4");
                        assert_eq!(referral["phase"], "arbitrate");
                        assert_eq!(disputed["repair_event"]["phase"], "arbitrate");
                        assert_eq!(disputed["repair_event"]["review"], disputed["review"]);
                        let mut ruling = json!({"action":"page_repair_decide","page":id,
                            "candidate_id":disputed["candidate_id"],"verdict_id":disputed["verdict_id"]});
                        ruling["decision"] = json!("dispute");
                        ruling["finding_observations"] = json!({"copy-title":"Fixture corroborator could read the original title."});
                        assert!(reviewer.execute(ruling.clone()).is_err());
                        assert!(maker.execute(ruling.clone()).is_err());
                        assert!(coordinator.execute(ruling.clone()).is_err());
                        let mut other = Context::new(root).unwrap();
                        other.continuation_owner = Some("reviewer-9".into());
                        assert!(other.execute(ruling.clone()).is_err());
                        other.continuation_owner = Some("session:reviewer-4".into());
                        let returned = other.execute(ruling.clone()).unwrap();
                        assert_eq!(returned["work_stage"], "review");
                        assert_eq!(other.execute(ruling.clone()).unwrap(), returned);
                        assert_eq!(assignment()["phase"], "review");
                        ruling["decision"] = json!("confirm");
                        assert!(other.execute(ruling.clone()).is_err());
                        let premature = json!({"action":"cycle","page":id,"spec":format!("{id}.json"),"output":format!("{id}-premature"),"publish_candidate":true});
                        assert!(maker.execute(premature.clone()).is_err());
                        reject["replaces_verdict_id"] = disputed["verdict_id"].clone();
                        assert!(reviewer.execute(reject.clone()).unwrap_err().to_string().contains("disputed on the same evidence"));
                        reject["verdict"]["open_required_findings"][0]["impact"] = json!("The last characters are clipped in the marked region.");
                        let first = reviewer.execute(reject).unwrap();
                        assert!(other.execute(ruling.clone()).is_err());
                        ruling["verdict_id"] = first["verdict_id"].clone();
                        ruling["finding_observations"] = json!({"copy-title":"Fixture second peer confirms that the final characters are clipped."});
                        assert!(maker.execute(premature).unwrap_err().to_string().contains("not authorized"));
                        assert!(!root.join(format!("{id}-premature")).exists());
                        let confirmed = other.execute(ruling.clone()).unwrap();
                        assert_eq!(confirmed["work_stage"], "repair");
                        assert_eq!(confirmed["repair_event"]["decision"], confirmed["receipt"]);
                        assert_eq!(confirmed["repair_event"]["phase"], "repair");
                        assert_eq!(confirmed["quality_approved"], false);
                        assert_eq!(other.execute(ruling.clone()).unwrap(), confirmed);
                        let repair_job = assignment();
                        assert_eq!(repair_job["phase"], "repair");
                        assert_eq!(repair_job["peer"], "maker-3");
                        assert!(repair_job["message"].as_str().unwrap().contains(first["verdict_id"].as_str().unwrap()));
                        let spec_path = format!("{id}.json");
                        let mut repaired = maker.json(&spec_path).unwrap();
                        repaired["pages"][0]["copy"]["title"] = json!("Repaired title");
                        repaired["pages"][0]["texts"][0]["text"] = json!("Repaired title");
                        maker.replace_json(&spec_path, &repaired).unwrap();
                        let updated = maker.execute(json!({"action":"cycle","spec":spec_path,"page":id,
                            "output":format!("{id}-repaired"),"publish_candidate":true,"freeze_inputs":true})).unwrap();
                        assert!(reviewer.execute(decision.clone()).is_err());
                        decision["candidate_id"] = updated["peer_review_handoff"]["candidate_id"].clone();
                        let rereview_job = assignment();
                        assert_eq!(rereview_job["phase"], "review");
                        assert_ne!(rereview_job["candidate_sha256"], review_job["candidate_sha256"]);
                        assert!(rereview_job["message"].as_str().unwrap().contains(decision["candidate_id"].as_str().unwrap()));
                        let next = &updated["peer_review_handoff"]["review_request_template"];
                        assert_eq!(next["replaces_verdict_id"], first["verdict_id"]);
                        decision["replaces_verdict_id"] = next["replaces_verdict_id"].clone();
                        decision["verdict"]["evidence"] = json!([updated["inspection"]["report"]]);
                    }
                }
                let receipt = reviewer.execute(decision.clone()).unwrap();
                assert_eq!(reviewer.execute(decision).unwrap(),receipt);
            });
        }
    });
    assert_eq!(gate(&ctx)["accepted_pages"], 20);
    if per_page {
        assert_eq!(
            ctx.execute(json!({"action":"page_assignments"})).unwrap()["assignments"],
            0
        );
    }
    let progress = ".octos/pptx-progress/ready.pptx";
    if per_page {
        let before_refresh = ctx.binding(progress).unwrap();
        let snapshot = ctx.execute(json!({"action":"page_progress"})).unwrap();
        assert_eq!(snapshot["output"], before_refresh);
        let expected: Vec<_> = (0..20).map(|i| format!("slide-{i}")).collect();
        assert_eq!(snapshot["pages"], json!(expected));
    } else {
        assert!(!ctx.path(progress).unwrap().exists());
    }
    for wrong in [
        None,
        Some("reviewer-1"),
        Some("final-reviewer"),
        Some("maker-1"),
    ] {
        let mut other = Context::new(root).unwrap();
        other.continuation_owner = wrong.map(str::to_owned);
        let error = other
            .execute(json!({"action":"page_assemble","output":"complete.pptx"}))
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("only the declared page coordinator")
        );
        assert!(!root.join("complete.pptx").exists());
    }
    let result = ctx
        .execute(json!({"action":"page_assemble","output":"complete.pptx"}))
        .unwrap();
    assert_eq!(result["requires_final_review"], !per_page);
    if per_page {
        assert_eq!(
            ctx.binding(progress).unwrap()["sha256"],
            result["output"]["sha256"]
        );
        let collect = json!({"action":"evidence_assembled","assembly":result["receipt"]["path"],"qa":"qa.json"});
        let observed = ctx.execute(collect.clone()).unwrap();
        assert_eq!(observed["all_checks_pass"], true);
        assert_eq!(ctx.execute(collect.clone()).unwrap(), observed);
        let qa = ctx.json("qa.json").unwrap();
        assert_eq!(qa["review"]["pages"].as_array().unwrap().len(), 20);
        for i in 0..20 {
            assert_eq!(
                qa["review"]["pages"][i]["reviewer_session"],
                format!("reviewer-{i}")
            );
            assert_eq!(
                qa["review"]["pages"][i]["render"],
                result["pages"][i]["render"]
            );
        }
        assert!(
            ctx.execute(
                json!({"action":"render","input":"complete.pptx","width":96,"output":"renders"})
            )
            .is_err()
        );
        assert!(!root.join("renders/render.json").exists());
        let changed = ctx.path("slide-3-verdict.json").unwrap();
        std::fs::write(changed, b"{}").unwrap();
        assert!(
            ctx.execute(collect).is_err(),
            "delivery cannot reuse a stale page verdict"
        );
        return;
    }
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

#[test]
fn regional_measurements_use_one_pair_of_originals_without_creating_probe_images() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Context::new(dir.path()).unwrap();
    let before = image::RgbImage::from_pixel(4, 2, image::Rgb([25, 30, 80]));
    let mut after = image::RgbImage::from_pixel(4, 3, image::Rgb([25, 30, 80]));
    after.put_pixel(1, 0, image::Rgb([55, 0, 110]));
    before.save(dir.path().join("source.png")).unwrap();
    after.save(dir.path().join("render.png")).unwrap();
    let req = json!({"action":"compare","before":"source.png","after":"render.png",
        "regions":[{"id":"ink","box":[1,0,1,1]},{"id":"background","box":[0,1,4,1]}],"receipt":"measure.json"});
    let result = ctx.execute(req.clone()).unwrap();
    assert_eq!(
        result["regions"][0]["mean_absolute_rgb_error"],
        json!([30.0, 30.0, 30.0])
    );
    assert_eq!(result["regions"][1]["mean_channel_error"], 0.0);
    assert_eq!(result["quality_approved"], false);
    assert_eq!(result["before"], ctx.binding("source.png").unwrap());
    assert_eq!(
        ctx.json("measure.json").unwrap()["regions"],
        result["regions"]
    );
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 3);
    for regions in [
        json!([]),
        json!([{"id":"bad","box":[3,2,1,1]}]),
        json!([{"id":"bad","box":[4294967295_u32,0,2,1]}]),
        json!([{"id":"bad","box":[0,0,0,1]}]),
        json!([{"id":"same","box":[0,0,1,1]},{"id":"same","box":[0,0,1,1]}]),
    ] {
        let mut bad = req.clone();
        bad.as_object_mut().unwrap().remove("receipt");
        bad["regions"] = regions;
        assert!(ctx.execute(bad).is_err());
    }
}

#[test]
fn verified_repair_requires_a_reproduced_failure_of_the_published_page() {
    let tmp = tempfile::tempdir().unwrap();
    let mut ctx = Context::new(tmp.path()).unwrap();
    image::RgbImage::from_pixel(640, 360, image::Rgb([255, 255, 255]))
        .save(tmp.path().join("paper.png"))
        .unwrap();
    ctx.write_json(
        "years.json",
        &json!({"schema_version":1,
        "canvas":{"width_px":640,"height_px":360,"width_in":12.8,"height_in":7.2},
        "styles":{"body":{"font":"Inter","size_pt":32}},
        "pages":[{"id":"years","background":"paper.png","copy":{"label":"1953 to 1961"},
            "texts":[{"id":"label","text":"1953 to 1961","style":"body","box":[620,60,280,80]}]}]}),
    )
    .unwrap();
    ctx.write_json(".octos/pptx-page-plan.json", &json!({"schema_version":2,
        "coordinator_session":"outer","rubric":{"readability":100},"repair_requirements":{"years":"Include both years"},
        "pages":[{"id":"years","maker_session":"maker","reviewer_session":"reviewer","spec":"years.json","candidate":"candidate.json","review":"review.json"}]})).unwrap();
    ctx.continuation_owner = Some("outer".into());
    ctx.execute(
        json!({"action":"evidence_init","task_id":"year-overflow","verification_mode":"per_page",
        "arbiter_agent":"other","deliverables":["final.pptx"],"stagnant_attempts":2}),
    )
    .unwrap();
    ctx.continuation_owner = Some("maker".into());
    let built = ctx.execute(json!({"action":"cycle","spec":"years.json","page":"years","output":"made","publish_candidate":true})).unwrap();
    let report = ctx
        .json(ctx.verify(&built["native_qa"]["report"]).unwrap())
        .unwrap();
    let findings = report["findings"].as_array().unwrap();
    let overflow = findings
        .iter()
        .find(|f| f["code"] == "INK_OUTSIDE_SLIDE" && f["status"] == "fail")
        .expect("year label crosses the actual slide edge");
    let unknown = findings
        .iter()
        .find(|f| f["status"] == "unknown")
        .expect("unverified owner");
    let mut rejected = built["peer_review_handoff"]["review_request_template"].clone();
    let v = &mut rejected["verdict"];
    v["accepted"] = json!(false);
    v["score"] = json!(92);
    v["component_scores"] = json!({"readability":92});
    v["evidence"] = json!([built["native_qa"]["report"]]);
    v["open_required_findings"] = json!([{"kind":"native_qa","id":unknown["id"]}]);
    ctx.continuation_owner = Some("reviewer".into());
    assert!(
        ctx.execute(rejected.clone())
            .unwrap_err()
            .to_string()
            .contains("failed native check")
    );
    assert!(!tmp.path().join("review.json").exists());
    rejected["verdict"]["open_required_findings"] = json!([{"id":"both-years","kind":"requirement","rule":"years","impact":"Second year extends beyond the edge"}]);
    let referred = ctx.execute(rejected.clone()).unwrap();
    assert_eq!(gate(&ctx)["pages"][0]["confirmation_peer"], "other");
    rejected["replaces_verdict_id"] = referred["verdict_id"].clone();
    rejected["verdict"]["open_required_findings"] =
        json!([{"kind":"native_qa","id":overflow["id"]}]);
    let current = ctx.execute(rejected).unwrap();
    assert_eq!(current["accepted"], false);
    assert_eq!(referred["repair_event"]["phase"], "arbitrate");
    assert_eq!(current["repair_event"]["phase"], "repair");
    assert!(current["repair_event"]["decision"].is_null());
    assert_eq!(gate(&ctx)["pages"][0]["work_stage"], "repair");
    ctx.continuation_owner = Some("outer".into());
    let batch = ctx.execute(json!({"action":"page_assignments"})).unwrap();
    assert_eq!(batch["assignments"], 1);
    assert_eq!(batch["specialist_pages"], json!([]));
    let jobs = ctx.json(".octos/pptx-peer-batch.json").unwrap();
    assert_eq!(jobs["jobs"][0]["peer"], "maker");
    assert_eq!(jobs["jobs"][0]["phase"], "repair");
}

#[test]
fn missing_native_labels_stay_with_page_maker_while_shared_locks_are_held() {
    let dir = tempfile::tempdir().unwrap();
    let mut ctx = Context::new(dir.path()).unwrap();
    image::RgbImage::from_pixel(96, 72, image::Rgb([255, 255, 255]))
        .save(dir.path().join("paper.png"))
        .unwrap();
    let spec = |title: &str, labels: &[(&str, &str)]| {
        let mut texts = Vec::new();
        let mut copy = json!({});
        for (i, (id, text)) in std::iter::once(("title", title))
            .chain(labels.iter().copied())
            .enumerate()
        {
            texts.push(json!({"id":id,"text":text,"style":"body","box":[2,2+i*15,80,12]}));
            copy[id] = json!(text);
        }
        json!({"schema_version":1,"canvas":{"width_px":96,"height_px":72,"width_in":4,"height_in":3},
            "styles":{"body":{"font":"Inter","size_pt":10}},"pages":[{"id":"p","background":"paper.png","copy":copy,"texts":texts}]})
    };
    ctx.write_json(
        "original.json",
        &spec("First", &[("key1", "Alpha"), ("key2", "Beta")]),
    )
    .unwrap();
    let original = ctx
        .execute(json!({"action":"cycle","spec":"original.json","output":"original","width":96}))
        .unwrap();
    let reference = &original["renders"]["editable"]["pages"][0];
    ctx.verify(reference).unwrap();
    ctx.write_json("page.json", &spec("First", &[])).unwrap();
    ctx.write_json(".octos/pptx-page-plan.json", &json!({"schema_version":2,
        "coordinator_session":"outer","rubric":{"content":100},"repair_requirements":{"labels":"Source legend entries must be editable text"},
        "pages":[{"id":"p","maker_session":"maker","reviewer_session":"reviewer","spec":"page.json","reference":reference,"editable_copy":{},"candidate":"candidate.json","review":"review.json"}]})).unwrap();
    ctx.continuation_owner = Some("outer".into());
    ctx.execute(
        json!({"action":"evidence_init","task_id":"labels","verification_mode":"per_page",
        "arbiter_agent":"other","deliverables":["final.pptx"],"stagnant_attempts":2}),
    )
    .unwrap();
    let cycle = |ctx: &Context, name: &str| {
        ctx.execute(json!({"action":"cycle","spec":"page.json","page":"p",
        "output":name,"freeze_inputs":true,"publish_candidate":true}))
            .unwrap()
    };
    ctx.continuation_owner = Some("maker".into());
    let first = cycle(&ctx, "initial");
    let mut reject = first["peer_review_handoff"]["review_request_template"].clone();
    let v = &mut reject["verdict"];
    v["accepted"] = json!(false);
    v["score"] = json!(78);
    v["component_scores"] = json!({"content":78});
    let render = &first["page_publication"]["handoff"]["render"];
    assert!(render["sha256"].is_string());
    assert_ne!(render["sha256"], reference["sha256"]);
    v["evidence"] = json!([reference, render, first["native_qa"]["report"]]);
    v["open_required_findings"] = json!([{"id":"legend","kind":"requirement","rule":"labels",
        "impact":"Diagram labels must remain editable","native_text":{"title":"First"}}]);
    ctx.continuation_owner = Some("reviewer".into());
    assert!(
        ctx.execute(reject.clone())
            .unwrap_err()
            .to_string()
            .contains("already meets")
    );
    reject["verdict"]["open_required_findings"][0]["native_text"] = json!({"a":"Alpha","b":"Beta"});
    let prior = ctx.execute(reject).unwrap();
    let reviewed = ctx.binding("candidate.json").unwrap();
    ctx.continuation_owner = Some("other".into());
    ctx.execute(json!({"action":"page_repair_decide","page":"p","candidate_id":prior["candidate_id"],
        "verdict_id":prior["verdict_id"],"decision":"confirm","finding_observations":{"legend":"The approved key has both labels, and neither is editable."}})).unwrap();
    ctx.continuation_owner = Some("maker".into());
    ctx.replace_json("page.json", &spec("Different title", &[]))
        .unwrap();
    let unrelated = cycle(&ctx, "unrelated");
    let obs = &unrelated["page_publication"]["repair_observation"];
    assert!(unrelated["peer_review_handoff"].is_null());
    assert_eq!(
        obs["findings"]["legend"],
        json!({"changed_from_reviewed":[],"changed_this_attempt":[],"remaining":["a","b"]})
    );
    assert_eq!(ctx.binding("candidate.json").unwrap(), reviewed);
    let retry = ctx
        .execute(json!({"action":"page_publish","page":"p","cycle":"unrelated/cycle.json"}))
        .unwrap();
    assert_eq!(&retry["repair_observation"], obs);
    let scope = ctx
        .execute(json!({"action":"page_gate","only_page":"p"}))
        .unwrap();
    let receipts = ctx
        .path(
            scope["pages"][0]["repair_observations_dir"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
    assert_eq!(std::fs::read_dir(&receipts).unwrap().count(), 1);

    // Simulate other pages holding both shared locks. Use a bounded wait so that
    // accidentally acquiring the blocking assembly lock fails without hanging CI.
    let (done, done_rx) = std::sync::mpsc::channel();
    let locks: Vec<_> = [
        ".octos/pptx-progress/publish.lock",
        ".octos/pptx-peer-batch.lock",
    ]
    .iter()
    .map(|name| {
        let f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(ctx.path(name).unwrap())
            .unwrap();
        fs2::FileExt::lock_exclusive(&f).unwrap();
        f
    })
    .collect();
    let guard = std::thread::spawn(move || {
        let local_finished = done_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .is_ok();
        drop(locks);
        local_finished
    });
    ctx.replace_json(
        "page.json",
        &spec("Different title", &[("a", "Alpha"), ("b", "Wrong")]),
    )
    .unwrap();
    let partial = cycle(&ctx, "partial");
    assert_eq!(
        partial["page_publication"]["repair_observation"]["findings"]["legend"],
        json!({"changed_from_reviewed":["a","b"],"changed_this_attempt":["a","b"],"remaining":["b"]})
    );
    assert!(partial["peer_review_handoff"].is_null());
    assert_eq!(ctx.binding("candidate.json").unwrap(), reviewed);
    ctx.replace_json(
        "page.json",
        &spec("Different title", &[("a", "Alpha"), ("b", "Beta")]),
    )
    .unwrap();
    let fixed = cycle(&ctx, "fixed");
    let _ = done.send(());
    assert!(
        guard.join().unwrap(),
        "repair waited for a different page's shared lock"
    );
    assert_eq!(
        fixed["page_publication"]["repair_observation"]["native_text_met"],
        true
    );
    assert_eq!(
        fixed["page_publication"]["repair_observation"]["findings"]["legend"]["changed_this_attempt"],
        json!(["b"])
    );
    assert_eq!(
        fixed["page_publication"]["ready_for_independent_review"],
        true
    );
    assert_eq!(fixed["page_publication"]["quality_approved"], false);
    assert_eq!(std::fs::read_dir(&receipts).unwrap().count(), 3);
    assert_eq!(gate(&ctx)["pages"][0]["work_stage"], "review");
    let mut accept = fixed["peer_review_handoff"]["review_request_template"].clone();
    let v = &mut accept["verdict"];
    v["accepted"] = json!(true);
    v["score"] = json!(98);
    v["component_scores"] = json!({"content":98});
    v["source_detail_review"]["scope"] = json!("whole_page");
    v["source_detail_review"]["resolved_findings"]["legend"] =
        json!("Both restored native labels are present in the current render.");
    v["evidence"]
        .as_array_mut()
        .unwrap()
        .push(fixed["native_qa"]["report"].clone());
    for check in v["delivery_checks"].as_object_mut().unwrap().values_mut() {
        *check = json!({"status":"pass","basis":"Synthetic reviewer observation"});
    }
    ctx.continuation_owner = Some("reviewer".into());
    let accepted = ctx.execute(accept).unwrap();
    assert_eq!(accepted["accepted"], true);
    let receipt = ctx
        .json(
            ctx.verify(&fixed["page_publication"]["repair_observation"]["receipt"])
                .unwrap(),
        )
        .unwrap();
    assert_eq!(
        ctx.json(ctx.verify(&accepted["archive"]).unwrap()).unwrap()["artifact"]["sha256"],
        receipt["proposed_artifact_sha256"]
    );
}

#[test]
fn per_page_verdict_requires_current_source_line_reviews_before_assembly() {
    let dir = tempfile::tempdir().unwrap();
    let mut ctx = fixture(dir.path());
    edit(&ctx, "alpha.json", |p| p["design"] = json!("design.png"));
    let hash = ctx.binding("design.png").unwrap()["sha256"].clone();
    ctx.write_json("measurements.json",&json!({"source_sha256":hash,"canvas":[40,30],"lines":[{"id":"t07","text":"alpha","box":[1,2,10,8]}]})).unwrap();
    let intake = json!({"action":"source_lines","page":"alpha","design":"design.png","ocr":"measurements.json"});
    ctx.execute(intake.clone()).unwrap();
    ctx.continuation_owner = Some("maker-alpha".into());
    let made=ctx.execute(json!({"action":"cycle","page":"alpha","spec":"deck.json","output":"line-cycle","publish_candidate":true,"freeze_inputs":true,"width":64})).unwrap();
    let id = made["page_publication"]["candidate_id"].clone();
    ctx.continuation_owner = Some("reviewer".into());
    let mut req = json!({"action":"page_review_submit","page":"alpha","candidate_id":id,"verdict":{"accepted":true,"score":96,"critical_defects":[],"open_required_findings":[],"evidence":["evidence.json"]}});
    let err = ctx.execute(req.clone()).unwrap_err();
    assert!(err.to_string().contains("pending source lines"), "{err:#}");
    req["verdict"]["source_line_reviews"] = json!([{"page":"alpha","source_line_id":"t07","status":"native","observation":"Synthetic candidate carries the original label"}]);
    ctx.execute(req).unwrap();
    assert_eq!(gate(&ctx)["accepted_pages"], 1);
    let mut added = intake;
    added["additional_lines"] = json!([{"id":"manual","text":"missed","box":[17,12,5,7]}]);
    ctx.execute(added).unwrap();
    assert_eq!(
        gate(&ctx)["accepted_pages"],
        0,
        "a newly found line invalidates the old acceptance"
    );
}
