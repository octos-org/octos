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
fn should_report_all_objects_and_keep_missing_ownership_unknown() {
    let (_dir, ctx) = fixture();
    change(&ctx, |s| {
        s["pages"][0]["copy"]["free"] = json!("Unbound caption");
        s["pages"][0]["texts"].as_array_mut().unwrap().push(json!({
            "id":"free","text":"Unbound caption","style":"body","box":[50,250,280,80]}));
    });
    cycle(&ctx, "unbound");
    let q = qa(&ctx, "unbound");
    assert_eq!(q["inventory"]["expected_text_objects"], 2);
    assert_eq!(q["objects"].as_array().unwrap().len(), 2);
    assert!(has(&q, "OWNER_UNVERIFIED"));
    assert_eq!(q["status"], "unknown");
    assert_eq!(claim(&ctx, "unbound")["checks"][3]["status"], "unknown");
}

#[test]
fn should_allow_missing_owner_metadata_to_be_added_without_moving_the_target() {
    let (_dir, ctx) = fixture();
    change(&ctx, |s| {
        let region = s["pages"][0]["regions"]["card"].as_object_mut().unwrap();
        region.remove("scope");
        region.remove("basis");
    });
    cycle(&ctx, "legacy");
    assert_eq!(claim(&ctx, "legacy")["checks"][3]["status"], "unknown");
    change(&ctx, |s| {
        s["pages"][0]["regions"]["card"]["scope"] = json!("label");
        s["pages"][0]["regions"]["card"]["basis"] = json!("Independent synthetic card");
    });
    cycle(&ctx, "measured");
    assert_eq!(claim(&ctx, "measured")["checks"][3]["status"], "pass");
    change(&ctx, |s| {
        s["pages"][0]["regions"]["card"]["quad"] = json!([[0, 0], [640, 0], [640, 360], [0, 360]]);
    });
    cycle(&ctx, "relaxed");
    assert_ne!(claim(&ctx, "relaxed")["checks"][3]["status"], "pass");
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

#[test]
fn should_reject_expanding_owner_or_deleting_copy_as_a_fix_and_accept_real_repair() {
    let (_dir, ctx) = fixture();
    change(&ctx, |s| {
        s["pages"][0]["texts"][0]["box"] = json!([450, 60, 160, 80])
    });
    cycle(&ctx, "outside");
    assert_eq!(claim(&ctx, "outside")["checks"][3]["status"], "fail");
    let original = ctx.json("deck.json").unwrap();
    change(&ctx, |s| {
        s["pages"][0]["regions"]["card"]["quad"] = json!([[0, 0], [640, 0], [640, 360], [0, 360]])
    });
    cycle(&ctx, "expanded");
    let expanded = claim(&ctx, "expanded");
    assert_ne!(
        expanded["checks"][3]["status"], "pass",
        "do not move the target around bad text"
    );
    assert!(
        !expanded["native_qa"]["constraint_changes"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    ctx.replace_json("deck.json", &original).unwrap();
    change(&ctx, |s| {
        s["pages"][0]["texts"] = json!([]);
        s["pages"][0]["copy"] = json!({});
    });
    cycle(&ctx, "deleted");
    assert_ne!(
        claim(&ctx, "deleted")["checks"][1]["status"],
        "pass",
        "deleting both spec and text must not close a copy error"
    );
    ctx.replace_json("deck.json", &original).unwrap();
    change(&ctx, |s| {
        s["pages"][0]["texts"][0]["box"] = json!([60, 60, 280, 80])
    });
    cycle(&ctx, "fixed");
    let r = claim(&ctx, "fixed");
    assert_eq!(
        r["checks"][3]["status"], "pass",
        "fixing the real text against the original owner should pass"
    );
    assert_eq!(r["visual_quality_verified_by_tool"], false);
}

#[test]
fn should_recompute_rotated_label_constraints_without_a_model_judgment() {
    let (_dir, ctx) = fixture();
    let q = octos_pptx::geometry::frame([40., 40., 360., 160.], 12.);
    change(&ctx, |s| {
        s["pages"][0]["regions"]["card"]["quad"] = json!(q)
    });
    cycle(&ctx, "tilted");
    assert!(has(&qa(&ctx, "tilted"), "BASELINE_MISMATCH"));
    assert_eq!(claim(&ctx, "tilted")["checks"][3]["status"], "fail");
}

#[test]
fn should_report_baseline_angles_in_degrees_and_not_as_position_coordinates() {
    let (_dir, ctx) = fixture();
    change(&ctx, |s| {
        s["pages"][0]["texts"][0]["rotation"] = json!(-20.)
    });
    cycle(&ctx, "negative-angle");
    let before = qa(&ctx, "negative-angle");
    let finding = before["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["code"] == "BASELINE_MISMATCH")
        .unwrap();
    assert_eq!(finding["observed"]["unit"], "degrees");
    assert_eq!(finding["observed"]["actual_deg"], -20.);
    assert_eq!(finding["observed"]["expected_deg"], 0.);
    assert_eq!(finding["observed"]["delta_deg"], -20.);
    assert_eq!(finding["observed"]["tolerance_deg"], 2.);
    change(&ctx, |s| s["pages"][0]["texts"][0]["box"][1] = json!(72));
    cycle(&ctx, "translation");
    let after = qa(&ctx, "translation");
    let translated = after["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["code"] == "BASELINE_MISMATCH")
        .unwrap();
    assert_eq!(finding["observed"], translated["observed"]);
}

#[test]
fn should_allow_contained_local_owners_to_refine_only_unverified_legacy_owners() {
    let (_dir, ctx) = fixture();
    change(&ctx, |s| {
        s["pages"][0]["regions"]["card"] = json!([40, 40, 360, 160])
    });
    cycle(&ctx, "unverified-parent");
    claim(&ctx, "unverified-parent");
    change(&ctx, |s| {
        s["pages"][0]["regions"]["local"] = json!({
            "quad":[[50,50],[390,50],[390,190],[50,190]],
            "parent":"card", "scope":"label", "basis":"Separate synthetic local artwork boundary"
        });
        s["pages"][0]["texts"][0]["region"] = json!("local");
    });
    cycle(&ctx, "contained-local");
    assert!(!has(&qa(&ctx, "contained-local"), "CONSTRAINT_CHANGED"));
    // A replacement that exceeds the original envelope is not refinement.
    change(&ctx, |s| {
        s["pages"][0]["regions"]["local"]["quad"] = json!([[0, 0], [640, 0], [640, 360], [0, 360]])
    });
    cycle(&ctx, "outside-parent");
    assert!(has(&qa(&ctx, "outside-parent"), "CONSTRAINT_CHANGED"));
    change(&ctx, |s| {
        s["pages"][0]["regions"]["local"]["quad"] =
            json!([[50, 50], [390, 50], [390, 190], [50, 190]])
    });
    cycle(&ctx, "record-local");
    assert_eq!(claim(&ctx, "record-local")["native_qa"]["status"], "pass");
    let saved = ctx.json(".octos/pptx-evidence-history.json").unwrap();
    assert_eq!(
        saved["qa_contract"]["pages"]["overview"]["owners"]["caption"],
        "local"
    );
    // Once verified, renaming/replacing a local target may not erase its bounds.
    change(&ctx, |s| {
        s["pages"][0]["regions"]["replacement"] = json!({
            "quad":[[60,60],[380,60],[380,180],[60,180]],
            "parent":"local", "scope":"label", "basis":"Claimed replacement"
        });
        s["pages"][0]["texts"][0]["region"] = json!("replacement");
    });
    cycle(&ctx, "replace-verified");
    assert!(has(&qa(&ctx, "replace-verified"), "CONSTRAINT_CHANGED"));
}

#[test]
fn should_not_call_a_clean_geometry_check_visual_approval() {
    let (_dir, ctx) = fixture();
    cycle(&ctx, "clean");
    let q = qa(&ctx, "clean");
    assert_eq!(q["status"], "pass", "{q}");
    assert_eq!(q["quality_approved"], false);
    assert_eq!(q["visual_review_required"], true);
}

#[test]
fn should_refine_a_prose_only_label_to_a_measured_child_without_resetting_history() {
    let (_dir, ctx) = fixture();
    change(&ctx, |s| {
        s["pages"][0]["regions"]["card"]["quad"] =
            json!([[10, 10], [500, 10], [500, 250], [10, 250]]);
        s["pages"][0]["regions"]["card"]["max_lines"] = json!(2);
    });
    cycle(&ctx, "wrong-envelope");
    claim(&ctx, "wrong-envelope");
    assert!(has(
        &qa(&ctx, "wrong-envelope"),
        "OWNER_BOUNDARY_UNVERIFIED"
    ));
    change(&ctx, |s| {
        s["pages"][0]["regions"]["local"] = json!({
            "quad":[[40,40],[400,40],[400,200],[40,200]], "parent":"card",
            "scope":"label", "basis":"Visible synthetic border", "max_lines":2
        });
        s["pages"][0]["texts"][0]["region"] = json!("local");
    });
    cycle(&ctx, "measured-child");
    let report = qa(&ctx, "measured-child");
    assert!(!has(&report, "CONSTRAINT_CHANGED"), "{report}");
    assert_eq!(report["status"], "pass", "{report}");
    let recorded = claim(&ctx, "measured-child");
    assert_eq!(recorded["attempt"], 2);
    let saved = ctx.json(".octos/pptx-evidence-history.json").unwrap();
    assert_eq!(
        saved["qa_contract"]["pages"]["overview"]["owners"]["caption"],
        "local"
    );
    assert_eq!(
        saved["qa_contract"]["pages"]["overview"]["regions"]["card"]["max_lines"],
        2
    );
}

#[test]
fn should_reject_unmeasured_children_and_dropped_or_shifted_constraints() {
    let (_dir, ctx) = fixture();
    change(&ctx, |s| {
        s["pages"][0]["regions"]["card"]["quad"] =
            json!([[10, 10], [500, 10], [500, 250], [10, 250]]);
        s["pages"][0]["regions"]["card"]["max_lines"] = json!(2);
    });
    cycle(&ctx, "envelope");
    claim(&ctx, "envelope");
    change(&ctx, |s| {
        s["pages"][0]["regions"]["local"] = json!({
            "quad":[[60,60],[380,60],[380,180],[60,180]], "parent":"card",
            "scope":"label", "basis":"Unsupported claim", "max_lines":2
        });
        s["pages"][0]["texts"][0]["region"] = json!("local");
    });
    cycle(&ctx, "unsupported-child");
    assert!(has(&qa(&ctx, "unsupported-child"), "CONSTRAINT_CHANGED"));
    change(&ctx, |s| {
        s["pages"][0]["regions"]["local"]["quad"] =
            json!([[40, 40], [400, 40], [400, 200], [40, 200]]);
        s["pages"][0]["regions"]["local"]
            .as_object_mut()
            .unwrap()
            .remove("max_lines");
    });
    cycle(&ctx, "dropped-limit");
    assert!(has(&qa(&ctx, "dropped-limit"), "CONSTRAINT_CHANGED"));
    // Positional targets must not move merely by shrinking the owner.
    change(&ctx, |s| {
        s["pages"][0]["texts"][0]["region"] = json!("card");
        s["pages"][0]["regions"]["card"]["ink_align"] = json!("center");
    });
    cycle(&ctx, "position-target");
    claim(&ctx, "position-target");
    change(&ctx, |s| {
        s["pages"][0]["texts"][0]["region"] = json!("local");
        s["pages"][0]["regions"]["local"]["ink_align"] = json!("center");
        s["pages"][0]["regions"]["local"]["max_lines"] = json!(2);
    });
    cycle(&ctx, "moved-position-target");
    assert!(has(
        &qa(&ctx, "moved-position-target"),
        "CONSTRAINT_CHANGED"
    ));
}

#[test]
fn should_expose_all_users_of_a_shared_owner_without_claiming_semantic_approval() {
    let (_dir, ctx) = fixture();
    change(&ctx, |s| {
        s["pages"][0]["copy"]["detail"] = json!("Separate caption");
        s["pages"][0]["texts"].as_array_mut().unwrap().push(json!({
            "id":"detail","text":"Separate caption","style":"body",
            "box":[60,120,280,60],"region":"card"
        }));
    });
    cycle(&ctx, "shared");
    let report = qa(&ctx, "shared");
    let owner = &report["owner_audit"][0];
    assert_eq!(owner["object_ids"], json!(["caption", "detail"]));
    assert_eq!(owner["boundary"]["status"], "supported");
    assert_eq!(owner["semantic_ownership_verified"], false);
    assert_eq!(report["quality_approved"], false);
}

#[test]
fn should_not_confuse_provenance_rewording_with_a_changed_geometric_target() {
    let (_dir, ctx) = fixture();
    cycle(&ctx, "original-note");
    claim(&ctx, "original-note");
    let original = ctx.json(".octos/pptx-evidence-history.json").unwrap()["qa_contract"].clone();
    change(&ctx, |s| {
        s["pages"][0]["regions"]["card"]["basis"] =
            json!("The same independently drawn border, described differently");
    });
    cycle(&ctx, "reworded-note");
    let report = qa(&ctx, "reworded-note");
    assert!(!has(&report, "CONSTRAINT_CHANGED"));
    assert_eq!(report["contract"], original);
    change(&ctx, |s| {
        s["pages"][0]["regions"]["card"]["quad"] =
            json!([[40, 40], [430, 40], [430, 200], [40, 200]]);
    });
    cycle(&ctx, "actual-target-change");
    assert!(has(&qa(&ctx, "actual-target-change"), "CONSTRAINT_CHANGED"));
}

#[test]
fn should_measure_forbidden_ink_and_declared_centering_and_line_count() {
    let (_dir, ctx) = fixture();
    change(&ctx, |s| {
        s["pages"][0]["regions"]["card"]["forbidden"] =
            json!([[[55, 55], [360, 55], [360, 130], [55, 130]]]);
        s["pages"][0]["regions"]["card"]["ink_align"] = json!("center");
        s["pages"][0]["regions"]["card"]["max_lines"] = json!(1);
        s["pages"][0]["copy"]["caption"] = json!("First line\nSecond line");
        s["pages"][0]["texts"][0]["text"] = json!("First line\nSecond line");
    });
    cycle(&ctx, "constraints");
    let q = qa(&ctx, "constraints");
    assert!(has(&q, "INK_IN_FORBIDDEN_REGION"), "{q}");
    assert!(has(&q, "INK_NOT_CENTERED"), "{q}");
    assert!(has(&q, "LINE_COUNT_EXCEEDED"), "{q}");
    assert_eq!(claim(&ctx, "constraints")["checks"][3]["status"], "fail");
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
fn should_leave_unsupported_grouped_text_unknown_instead_of_skipping_it() {
    use deckcraft_engine::model::{Shape, ShapeId, ShapeKind, Xfrm};
    let (_dir, ctx) = fixture();
    cycle(&ctx, "grouped");
    let mut doc = octos_pptx::deck::open(&ctx, &ctx.root.join("grouped/editable.pptx")).unwrap();
    let slide = std::sync::Arc::make_mut(&mut doc.slides[0]);
    let child = slide
        .shapes
        .iter()
        .find(|s| s.name.starts_with("octos:text:"))
        .unwrap()
        .clone();
    slide.shapes.push(Shape {
        id: ShapeId(999),
        name: "Extra group".into(),
        xfrm: Some(Xfrm::new(0., 0., 400., 200.)),
        kind: ShapeKind::Group {
            children: vec![child],
            child: Xfrm::new(0., 0., 400., 200.),
        },
        ..Default::default()
    });
    std::fs::write(
        ctx.root.join("grouped/editable.pptx"),
        deckcraft_engine::cmd::file::save_bytes(&doc, "pptx").unwrap(),
    )
    .unwrap();
    let mut c = ctx.json("grouped/cycle.json").unwrap();
    c["editable"] = ctx.binding("grouped/editable.pptx").unwrap();
    c["renders"]["editable"]["input"] = c["editable"].clone();
    ctx.replace_json("grouped/cycle.json", &c).unwrap();
    let q = qa(&ctx, "grouped");
    assert!(has(&q, "UNSUPPORTED_TEXT_CONTAINER"), "{q}");
    assert_ne!(q["status"], "pass");
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
fn should_not_treat_a_legacy_text_rectangle_as_independent_artwork_geometry() {
    let (_dir, ctx) = fixture();
    change(&ctx, |s| {
        s["pages"][0]["regions"]["card"] = json!([60, 60, 20, 20])
    });
    cycle(&ctx, "legacy");
    let q = qa(&ctx, "legacy");
    assert!(has(&q, "OWNER_UNVERIFIED"));
    assert!(
        !has(&q, "INK_OUTSIDE_OWNER"),
        "a guessed rectangle cannot prove an artwork collision"
    );
    assert_eq!(q["status"], "unknown");
}

#[test]
fn should_not_fail_font_line_height_when_actual_ink_is_contained() {
    let (_dir, ctx) = fixture();
    change(&ctx, |s| {
        s["pages"][0]["copy"]["caption"] = json!("HI");
        s["pages"][0]["texts"][0]["text"] = json!("HI");
    });
    cycle(&ctx, "measured");
    let inspection = ctx
        .execute(json!({"action":"inspect","cycle":"measured/cycle.json"}))
        .unwrap();
    let b = &inspection["objects"][0]["ink"]["bounds"];
    let bottom = b[1].as_f64().unwrap() + b[3].as_f64().unwrap();
    change(&ctx, |s| {
        s["pages"][0]["texts"][0]["box"][3] = json!(bottom - 60. + 0.25)
    });
    cycle(&ctx, "tight");
    let inspection = ctx
        .execute(json!({"action":"inspect","cycle":"tight/cycle.json"}))
        .unwrap();
    assert_eq!(
        inspection["objects"][0]["layout"]["overflow"], true,
        "fixture must exercise the metric/ink distinction"
    );
    assert!(!has(&qa(&ctx, "tight"), "TEXT_LAYOUT_OVERFLOW"));
}

#[test]
fn should_not_depend_on_page_order_content_or_canvas_scale() {
    for factor in [1., 2.] {
        let (_dir, ctx) = fixture();
        change(&ctx, |s| {
            s["canvas"]["width_px"] = json!(640. * factor);
            s["canvas"]["height_px"] = json!(360. * factor);
            s["pages"][0]["id"] = json!("unseen-topic");
            s["pages"][0]["copy"]["caption"] = json!("Fresh content");
            s["pages"][0]["texts"][0]["text"] = json!("Fresh content");
            s["pages"][0]["texts"][0]["box"] =
                json!([620. * factor, 60. * factor, 280. * factor, 80. * factor]);
            s["pages"][0]["regions"]["card"]["quad"] = json!([
                [40. * factor, 40. * factor],
                [400. * factor, 40. * factor],
                [400. * factor, 200. * factor],
                [40. * factor, 200. * factor]
            ]);
            let mut other = s["pages"][0].clone();
            other["id"] = json!("another-page");
            other["texts"][0]["box"] =
                json!([60. * factor, 60. * factor, 280. * factor, 80. * factor]);
            s["pages"].as_array_mut().unwrap().insert(0, other);
        });
        cycle(&ctx, "permuted");
        let q = qa(&ctx, "permuted");
        let out = q["findings"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["code"] == "INK_OUTSIDE_SLIDE")
            .unwrap();
        assert_eq!(out["page"], 2);
        assert_eq!(out["object_id"], "caption");
    }
}
