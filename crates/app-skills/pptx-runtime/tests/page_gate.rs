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
