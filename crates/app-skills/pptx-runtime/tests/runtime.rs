use octos_pptx::{Context, fs::sha_file};
mod common;
use serde_json::{Value, json};
use std::{
    io::Write,
    process::{Command, Stdio},
};

/// Font-selection evidence for a cycle's non-requested (fallback) text ranges, so an OCR
/// failure shows which face actually drew each grapheme run. Used by the macOS OCR test.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn cn_font_usage(ctx: &Context, inspection: &str) -> String {
    let rows = ctx.json(inspection).unwrap_or_default()["fonts"].clone();
    let rows: Vec<Value> = rows
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|r| r["reason"].as_str() != Some("requested"))
                .map(|r| {
                    json!({"start":r["start"],"end":r["end"],"actual":r["actual"],
                        "reason":r["reason"],"missingGlyph":r["missingGlyph"]})
                })
                .collect()
        })
        .unwrap_or_default();
    serde_json::to_string(&rows).unwrap_or_default()
}

fn fixture(root: &std::path::Path) -> Context {
    let ctx = Context::new(root).unwrap();
    let mut image = image::RgbImage::from_fn(1280, 720, |x, y| {
        if (50..1230).contains(&x) && (150..650).contains(&y) {
            image::Rgb([220, 235, 240])
        } else {
            image::Rgb([250, 246, 238])
        }
    });
    for x in 60..=620 {
        for dy in 0..3 {
            image.put_pixel(x, 180 + dy, image::Rgb([20, 20, 20]));
            image.put_pixel(x, 300 - dy, image::Rgb([20, 20, 20]));
        }
    }
    for y in 180..=300 {
        for dx in 0..3 {
            image.put_pixel(60 + dx, y, image::Rgb([20, 20, 20]));
            image.put_pixel(620 - dx, y, image::Rgb([20, 20, 20]));
        }
    }
    image.save(root.join("art.png")).unwrap();
    let spec = json!({"schema_version":1,"canvas":{"width_px":1280,"height_px":720,"width_in":12.8,"height_in":7.2},
        "styles":{"heading":{"font":"Inter","size_pt":28,"bold":true,"locked":true},"body":{"font":"Inter","size_pt":18}},
        "pages":[{"id":"overview","background":"art.png","design":"art.png","copy":{"heading":"Native editable deck","a":"Repeated label","b":"Repeated label","tilted":"Rotated label"},
        "regions":{"label":{"quad":[[60,180],[620,180],[620,300],[60,300]],"scope":"label","basis":"Synthetic card"}},
        "texts":[{"id":"heading","text":"Native editable deck","style":"heading","box":[60,30,1100,90]},
        {"id":"a","text":"Repeated label","style":"body","box":[75,200,500,70],"region":"label"},
        {"id":"b","text":"Repeated label","style":"body","box":[700,200,500,70]},
        {"id":"tilted","text":"Rotated label","style":"body","box":[400,420,450,80],"rotation":-12}]}]});
    ctx.write_json("deck.json", &spec).unwrap();
    ctx
}

#[test]
fn review_receipt_binds_full_comparison_and_rejects_stale_panels() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = fixture(dir.path());
    ctx.execute(json!({"action":"cycle","spec":"deck.json","output":"bound","width":800}))
        .unwrap();
    let pack = ctx
        .execute(json!({"action":"review_pack","cycle":"bound/cycle.json","output":"pack"}))
        .unwrap();
    let report = ctx.json(ctx.verify(&pack["report"]).unwrap()).unwrap();
    let mut authored = ctx
        .json(ctx.verify(&report["object_review_template"]).unwrap())
        .unwrap();
    assert!(
        !authored["comparisons"][0]["label_findings"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    authored["cycle_sha256"] = json!("[hex-redacted]");
    for row in authored["object_reviews"].as_array_mut().unwrap() {
        row["evidence_hash"] = json!("[hex-redacted]");
    }
    for row in authored["comparisons"].as_array_mut().unwrap() {
        row["comparison_key"] = json!("[hex-redacted]");
        for f in row["label_findings"].as_array_mut().unwrap() {
            f["evidence_sha256"] = json!("[hex-redacted]");
        }
    }
    ctx.write_json("observations.json", &authored).unwrap();
    let submit = json!({"action":"review_submit","pack":pack["report"]["path"],
        "observations":"observations.json","output":"bound-review.json"});
    let result = ctx.execute(submit.clone()).unwrap();
    assert_eq!(result["quality_approved"], false);
    assert!(
        result["comparison_coverage"]["unknown"].as_u64().unwrap() > 0,
        "binding metadata must never pass unreviewed comparisons"
    );
    let bound = ctx.json("bound-review.json").unwrap();
    assert_ne!(bound["comparisons"][0]["comparison_key"], "[hex-redacted]");
    assert_eq!(bound["comparisons"][0]["inspected"], false);
    assert_eq!(
        bound["comparisons"][0]["label_findings"][0]["classification"],
        "unresolved"
    );
    // Exercise the same downstream validator used by evidence_record.
    ctx.execute(json!({"action":"evidence_init","task_id":"receipt-fixture",
        "owner_agent":"fixture-maker","stagnant_attempts":3,"deliverables":["bound/editable.pptx"]})).unwrap();
    ctx.execute(
        json!({"action":"evidence_record","cycle":"bound/cycle.json","review":"bound-review.json"}),
    )
    .unwrap();
    let manifest = ctx
        .json(ctx.verify(&report["comparisons"]).unwrap())
        .unwrap();
    let comparison = ctx
        .json(ctx.verify(&manifest["pages"][0]["report"]).unwrap())
        .unwrap();
    std::fs::write(ctx.verify(&comparison["panel"]).unwrap(), b"changed pixels").unwrap();
    assert!(
        ctx.execute(submit).is_err(),
        "a changed comparison panel invalidates the receipt"
    );
}

#[test]
fn delivery_retarget_and_source_preview_bindings_preserve_existing_history() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mut ctx = fixture(root);
    ctx.continuation_owner = Some("session:fixture:maker".into());
    let mut spec = ctx.json("deck.json").unwrap();
    ctx.write_json("page.json", &spec["pages"][0]).unwrap();
    spec["pages"] = json!([{"path":"page.json"}]);
    ctx.replace_json("deck.json", &spec).unwrap();
    ctx.execute(json!({"action":"cycle","spec":"deck.json","output":"a","width":640}))
        .unwrap();
    ctx.execute(json!({"action":"evidence_init","task_id":"same-task","stagnant_attempts":3,"deliverables":["a/editable.pptx"]})).unwrap();
    ctx.execute(json!({"action":"evidence_record","cycle":"a/cycle.json"}))
        .unwrap();
    let before = std::fs::read(root.join(".octos/pptx-evidence-history.json")).unwrap();
    std::fs::copy(root.join("a/editable.pptx"), root.join("final.pptx")).unwrap();
    std::fs::copy(
        root.join("a/rendered/slide-001.png"),
        root.join("preview.png"),
    )
    .unwrap();
    ctx.execute(json!({"action":"evidence_retarget","deliverables":["final.pptx","preview.png","deck.json","page.json"],"reason":"Use stable final names"})).unwrap();
    assert_eq!(
        std::fs::read(root.join(".octos/pptx-evidence-history.json")).unwrap(),
        before
    );
    let rec = ctx
        .execute(json!({"action":"evidence_record","cycle":"a/cycle.json"}))
        .unwrap();
    assert_eq!(rec["attempt"], 2);
    let e = ctx.json(".octos/iteration-evidence.json").unwrap();
    for name in ["final.pptx", "preview.png", "deck.json", "page.json"] {
        assert!(
            e["artifacts"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a["path"] == name),
            "missing binding: {name}"
        );
    }
    assert_eq!(
        e["checks"][1]["status"], "unknown",
        "retarget is not a visual review"
    );
    std::fs::copy(root.join("a/blank.pptx"), root.join("final.pptx")).unwrap();
    assert!(
        ctx.execute(json!({"action":"evidence_record","cycle":"a/cycle.json"}))
            .is_err(),
        "blank artwork must not masquerade as the editable delivery"
    );
    std::fs::copy(root.join("a/editable.pptx"), root.join("final.pptx")).unwrap();
    std::fs::write(root.join("preview.png"), b"wrong preview").unwrap();
    assert!(
        ctx.execute(json!({"action":"evidence_record","cycle":"a/cycle.json"}))
            .is_err(),
        "wrong preview must fail immediately"
    );
    std::fs::copy(
        root.join("a/rendered/slide-001.png"),
        root.join("preview.png"),
    )
    .unwrap();
    ctx.replace_json("page.json", &json!({"changed":true}))
        .unwrap();
    assert!(
        ctx.execute(json!({"action":"evidence_record","cycle":"a/cycle.json"}))
            .is_err(),
        "changed source cannot reuse old rendered evidence"
    );
    ctx.continuation_owner = Some("session:fixture:other".into());
    assert!(ctx.execute(json!({"action":"evidence_retarget","deliverables":["other.pptx"],"reason":"wrong writer"})).is_err());
}

#[test]
fn comparison_separates_reference_and_render_signals_without_editing_inputs() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let ctx = Context::new(root).unwrap();
    let mut blank = image::RgbImage::from_pixel(160, 100, image::Rgb([255, 255, 255]));
    // Synthetic fixed artwork plus a label-sized mark, with no domain content.
    for y in 20..70 {
        for x in 15..35 {
            blank.put_pixel(x, y, image::Rgb([60, 60, 60]));
        }
    }
    let mut original = blank.clone();
    for y in 30..45 {
        for x in 65..95 {
            original.put_pixel(x, y, image::Rgb([0, 0, 0]));
        }
    }
    let mut moved = blank.clone();
    for y in 50..65 {
        for x in 105..135 {
            moved.put_pixel(x, y, image::Rgb([0, 0, 0]));
        }
    }
    blank.save(root.join("blank.png")).unwrap();
    original.save(root.join("design.png")).unwrap();
    moved.save(root.join("render.png")).unwrap();
    let before = sha_file(&root.join("design.png")).unwrap();
    let result=ctx.execute(json!({"action":"compare","before":"design.png","background":"blank.png","after":"render.png","output":"proof.png","box":[10,10,140,70],"tolerance":0})).unwrap();
    assert_eq!(result["reference_only_pixels"], 450);
    assert_eq!(result["candidate_only_pixels"], 450);
    assert_eq!(
        result["regions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["reference_only_pixels"].as_u64().unwrap())
            .sum::<u64>(),
        450
    );
    assert_eq!(
        result["regions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["candidate_only_pixels"].as_u64().unwrap())
            .sum::<u64>(),
        450
    );
    assert_eq!(result["quality_approved"], false);
    assert_eq!(result["visual_review_required"], true);
    assert_eq!(
        image::image_dimensions(root.join("proof.png")).unwrap(),
        (420, 140)
    );
    assert_eq!(sha_file(&root.join("design.png")).unwrap(), before);
    assert_eq!(result["views"].as_object().unwrap().len(), 6);
    let equal=ctx.execute(json!({"action":"compare","before":"design.png","background":"blank.png","after":"design.png","output":"equal.png"})).unwrap();
    assert_eq!(equal["reference_only_pixels"], 0);
    assert_eq!(equal["candidate_only_pixels"], 0);
    assert_eq!(
        equal["quality_approved"], false,
        "equal pixels cannot approve content or design"
    );
    image::imageops::resize(&original, 320, 200, image::imageops::FilterType::Triangle)
        .save(root.join("large.png"))
        .unwrap();
    let scaled=ctx.execute(json!({"action":"compare","before":"design.png","background":"blank.png","after":"large.png","output":"scaled.png","box":[0,0,320,200]})).unwrap();
    assert_eq!(scaled["normalized_to"], json!([320, 200]));
    assert_eq!(scaled["candidate_only_pixels"], 0);
    // Artwork drift can produce a signal without any new text: never call it a
    // text defect automatically or collapse it into a quality percentage.
    let drift=ctx.execute(json!({"action":"compare","before":"blank.png","background":"design.png","after":"design.png","output":"drift.png"})).unwrap();
    assert!(drift["reference_only_pixels"].as_u64().unwrap() > 0);
    assert_eq!(drift["candidate_only_pixels"], 0);
    assert_eq!(drift["quality_approved"], false);
    assert!(ctx.execute(json!({"action":"compare","before":"design.png","background":"blank.png","after":"render.png","output":"bad.png","box":[150,90,40,30]})).is_err());
    assert!(ctx.execute(json!({"action":"compare","before":"design.png","background":"blank.png","after":"render.png","output":"bad.png","tolerance":255})).is_err());
    image::RgbImage::new(160, 80)
        .save(root.join("wrong-aspect.png"))
        .unwrap();
    assert!(ctx.execute(json!({"action":"compare","before":"wrong-aspect.png","background":"blank.png","after":"render.png","output":"bad.png"})).is_err());
}

#[test]
fn inspect_exposes_unverified_labels_even_when_they_do_not_overflow() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = fixture(dir.path());
    ctx.execute(
        json!({"action":"cycle","spec":"deck.json","output":"review-candidate","width":640}),
    )
    .unwrap();
    let report = ctx.json("review-candidate/inspection.json").unwrap();
    let object = report["objects"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["object_id"] == "b")
        .unwrap();
    assert_eq!(object["layout"]["overflow"], false);
    assert!(
        object["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "local_label_ownership_unverified")
    );
    let group = report["visual_review_queue"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| {
            g["objects"]
                .as_array()
                .unwrap()
                .iter()
                .any(|o| o["object_id"] == "b")
        })
        .unwrap();
    assert_eq!(group["status"], "unreviewed");
    let viewport = group["viewport"].as_array().unwrap();
    assert!(
        viewport
            .iter()
            .all(|v| (0.0..=1.0).contains(&v.as_f64().unwrap()))
    );
    assert!(viewport[0].as_f64().unwrap() < 700.0 / 1280.0);
    assert!(viewport[2].as_f64().unwrap() > 1200.0 / 1280.0);
    assert!(report["ownership_unverified_objects"].as_u64().unwrap() > 0);
    assert_eq!(report["quality_approved"], false);
}

#[test]
fn completed_batch_can_expand_without_replaying_mutated_old_sources() {
    let dir = tempfile::tempdir().unwrap();
    let mut ctx = fixture(dir.path());
    ctx.continuation_owner = Some("session:fixture:maker".into());
    // This test needs a genuinely complete initial batch. Other tests retain
    // the fixture's intentionally unverified captions to test unknown coverage.
    let mut initial = ctx.json("deck.json").unwrap();
    initial["pages"][0]["texts"]
        .as_array_mut()
        .unwrap()
        .retain(|t| t["id"] == "a");
    initial["pages"][0]["copy"] = json!({"a":"Repeated label"});
    ctx.replace_json("deck.json", &initial).unwrap();
    ctx.execute(json!({"action":"cycle","spec":"deck.json","output":"first","width":640}))
        .unwrap();
    ctx.execute(json!({"action":"evidence_init","task_id":"same-task","stagnant_attempts":3,"deliverables":["final.pptx"]})).unwrap();
    std::fs::copy(
        dir.path().join("first/editable.pptx"),
        dir.path().join("final.pptx"),
    )
    .unwrap();
    let comparisons = ctx.json("first/comparisons/manifest.json").unwrap();
    let page = &comparisons["pages"][0];
    let report = ctx.json(ctx.verify(&page["report"]).unwrap()).unwrap();
    let ids: Vec<_> = report["regions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].clone())
        .collect();
    let labels:Vec<_>=report["label_evidence"]["findings"].as_array().unwrap().iter().map(|f|
        json!({"id":f["id"],"evidence_sha256":f["evidence"]["sha256"],"classification":"acceptable_difference",
            "reason_code":"intentional_change","note":"Fixture reference intentionally omits the added text"})).collect();
    ctx.write_json("review.json", &json!({"cycle_sha256":sha_file(&dir.path().join("first/cycle.json")).unwrap(),"inspected":{"copy":["overview"],"artwork":["overview"],"readability":["overview"]},"comparisons":[{"page":"overview","comparison_key":page["comparison_key"],"inspected":true,"note":"Synthetic fixture reviewed","label_findings":labels,"regions":[{"ids":ids,"classification":"acceptable_difference","note":"Fixture reference omits the intentionally added native labels"}]}],"findings":[{"id":"label","check":"readability","state":"resolved","note":"Checked original page","evidence":"first/rendered/slide-001.png"}]})).unwrap();
    // This test covers retained review history, not a repair: the unchanged
    // synthetic label is an intentional difference, so do not claim it moved.
    let mut review = ctx.json("review.json").unwrap();
    review["object_reviews"] = common::object_judgments(&ctx, "first");
    review["findings"][0]["state"] = json!("dismissed");
    ctx.replace_json("review.json", &review).unwrap();
    ctx.execute(
        json!({"action":"evidence_record","cycle":"first/cycle.json","review":"review.json"}),
    )
    .unwrap();
    let before = ctx.json(".octos/iteration-evidence.json").unwrap();
    let mut spec = ctx.json("deck.json").unwrap();
    let mut added = spec["pages"][0].clone();
    added["id"] = json!("details");
    spec["pages"].as_array_mut().unwrap().push(added);
    ctx.replace_json("deck.json", &spec).unwrap();
    ctx.execute(json!({"action":"cycle","spec":"deck.json","output":"second","width":640}))
        .unwrap();
    std::fs::copy(
        dir.path().join("second/editable.pptx"),
        dir.path().join("final.pptx"),
    )
    .unwrap();
    let request = json!({"action":"evidence_record","cycle":"second/cycle.json","extend_scope_reason":"Continue the authorized remaining pages"});
    assert!(
        ctx.execute(json!({"action":"evidence_record","cycle":"second/cycle.json"}))
            .is_err(),
        "expansion must be explicit"
    );
    let result = ctx.execute(request.clone()).unwrap();
    assert_eq!(result["attempt"], 2);
    assert_eq!(
        result["checks"][1]["status"], "unknown",
        "new pages need actual inspection"
    );
    let book = ctx.json(".octos/pptx-evidence-history.json").unwrap();
    assert_eq!(book["pages"], json!(["overview", "details"]));
    assert_eq!(book["findings"]["label"]["state"], "dismissed");
    let archive = result["previous_record"]["path"].as_str().unwrap();
    assert_eq!(ctx.json(archive).unwrap()["snapshot"], before);
    assert_eq!(
        ctx.execute(request).unwrap()["attempt"],
        2,
        "identical evidence is idempotent"
    );
    let shrink = json!({"action":"evidence_record","cycle":"first/cycle.json","extend_scope_reason":"Cannot remove pages"});
    assert!(ctx.execute(shrink).is_err());
}

#[test]
fn evidence_uses_host_identity_and_reports_misbound_policies() {
    let dir = tempfile::tempdir().unwrap();
    let mut ctx = Context::new(dir.path()).unwrap();
    ctx.continuation_owner = Some("session:profile:local:tui#peer-maker".into());
    let base = json!({"action":"evidence_init","task_id":"sample","stagnant_attempts":3,"deliverables":["out.pptx"]});
    let mut wrong = base.clone();
    wrong["owner_agent"] = json!("maker");
    assert!(ctx.execute(wrong).is_err());
    assert!(!dir.path().join(".octos/continuation.json").exists());
    ctx.execute(base).unwrap();
    let pending = ctx.json(".octos/iteration-evidence.json").unwrap();
    assert_eq!(pending["phase"], "pending");
    assert_eq!(pending["attempt"], 0);
    let policy = ctx.json(".octos/continuation.json").unwrap();
    assert_eq!(
        policy["owner_agent"],
        "session:profile:local:tui#peer-maker"
    );
    let status = ctx.execute(json!({"action":"evidence_status"})).unwrap();
    assert_eq!(status["owner_matches"], true);
    assert_eq!(status["runtime_consumed_current_evidence"], false);
    ctx.continuation_owner = Some("session:profile:local:tui#reviewer".into());
    let status = ctx.execute(json!({"action":"evidence_status"})).unwrap();
    assert_eq!(status["owner_matches"], false);
    assert_eq!(status["runtime_consumed_current_evidence"], false);
}

#[test]
fn build_reopen_render_patch_and_preserve_editable_text_without_external_programs() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let ctx = fixture(&root);
    let bin = env!("CARGO_BIN_EXE_octos-pptx");
    let mut child = Command::new(bin)
        .arg("pptx_tool")
        .current_dir(&root)
        .env("OCTOS_SESSION_WORKSPACE", &root)
        .env("PATH", root.join("no-executables"))
        .env_remove("PYTHONPATH")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(br#"{"action":"cycle","spec":"deck.json","output":"attempt1","width":1280}"#)
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["success"], true);
    let full = octos_pptx::deck::open(&ctx, &root.join("attempt1/editable.pptx")).unwrap();
    let blank = octos_pptx::deck::open(&ctx, &root.join("attempt1/blank.pptx")).unwrap();
    assert_eq!(
        full.slides[0]
            .shapes
            .iter()
            .filter(|s| s.text.is_some())
            .count(),
        4
    );
    assert_eq!(blank.slides[0].shapes.len(), 1);
    assert_eq!(full.media[0].data, blank.media[0].data);
    assert_eq!(
        image::image_dimensions(root.join("attempt1/rendered/slide-001.png")).unwrap(),
        (1280, 720)
    );
    let a = std::fs::read(root.join("attempt1/rendered/slide-001.png")).unwrap();
    let b = std::fs::read(root.join("attempt1/blank-rendered/slide-001.png")).unwrap();
    assert_ne!(a, b);
    assert_eq!(
        octos_pptx::review::preflight(&ctx, &root.join("attempt1/editable.pptx")).unwrap()["issue_count"],
        0
    );
    let old = sha_file(&root.join("attempt1/editable.pptx")).unwrap();
    ctx.execute(json!({"action":"patch","spec":"deck.json","output":"revised.json","edits":[{"page":"overview","id":"a","set":{"box":[75,215,500,70]}}]})).unwrap();
    ctx.execute(json!({"action":"cycle","spec":"revised.json","output":"attempt2","width":1280}))
        .unwrap();
    assert_eq!(sha_file(&root.join("attempt1/editable.pptx")).unwrap(), old);
    assert_ne!(
        sha_file(&root.join("attempt2/rendered/slide-001.png")).unwrap(),
        sha_file(&root.join("attempt1/rendered/slide-001.png")).unwrap()
    );
    assert_eq!(
        std::fs::read(root.join("attempt2/blank-rendered/slide-001.png")).unwrap(),
        b
    );
    assert!(
        ctx.execute(json!({"action":"build","spec":"deck.json","output":"attempt1"}))
            .is_err()
    );
}

#[test]
fn evidence_never_silently_passes_missing_reviews_or_drops_unresolved_findings() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let ctx = fixture(&root);
    ctx.execute(json!({"action":"cycle","spec":"deck.json","output":"a","width":640}))
        .unwrap();
    ctx.execute(json!({"action":"evidence_init","task_id":"synthetic","owner_agent":"peer-1","stagnant_attempts":3,"deliverables":["a/editable.pptx"]})).unwrap();
    let rec = json!({"action":"evidence_record","cycle":"a/cycle.json"});
    let e = ctx.execute(rec.clone()).unwrap();
    assert_eq!(e["checks"][1]["status"], "unknown");
    assert_eq!(ctx.execute(rec.clone()).unwrap()["attempt"], e["attempt"]);
    let review = json!({"cycle_sha256":sha_file(&root.join("a/cycle.json")).unwrap(),"inspected":{"copy":["overview"],"artwork":["overview"],"readability":["overview"]},"findings":[{"id":"defect","page":"overview","box":[0,0,640,360],"check":"readability","state":"open","note":"Synthetic unresolved defect","evidence":"a/rendered/slide-001.png"}]});
    ctx.write_json("review.json", &review).unwrap();
    let rec = json!({"action":"evidence_record","cycle":"a/cycle.json","review":"review.json"});
    assert_eq!(ctx.execute(rec).unwrap()["checks"][3]["status"], "fail");
    let mut review2 = review.clone();
    review2["findings"] = json!([]);
    ctx.write_json("review2.json", &review2).unwrap();
    assert_eq!(
        ctx.execute(
            json!({"action":"evidence_record","cycle":"a/cycle.json","review":"review2.json"})
        )
        .unwrap()["checks"][3]["status"],
        "fail"
    );
    assert_eq!(
        ctx.execute(json!({"action":"evidence_status","owner_agent":"peer-1"}))
            .unwrap()["runtime_consumed_current_evidence"],
        false
    );
    std::fs::write(root.join("a/rendered/slide-001.png"), b"changed").unwrap();
    assert!(
        ctx.execute(json!({"action":"evidence_record","cycle":"a/cycle.json"}))
            .is_err()
    );
}

#[test]
fn validates_copy_occurrences_locked_styles_paths_and_geometry() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let ctx = fixture(&root);
    assert!(ctx.path("../escaped.pptx").is_err());
    let p = json!({"action":"patch","spec":"deck.json","output":"bad.json","edits":[{"page":"overview","id":"heading","set":{"format":{"size_pt":8}}}]});
    assert!(ctx.execute(p).is_err());
    let p = json!({"action":"patch","spec":"deck.json","output":"bad.json","edits":[{"page":"overview","id":"a","set":{"text":"Changed without source approval"}}]});
    assert!(ctx.execute(p).is_err());
    let q = octos_pptx::geometry::frame([10., 20., 400., 90.], -15.);
    let result = ctx
        .execute(json!({"action":"geometry","quad":q,"padding":5}))
        .unwrap();
    assert!((result["rotation"].as_f64().unwrap() + 15.).abs() < 1e-8);
    assert_eq!(result["perspective_or_shear"], false);
    let b: [f64; 4] = serde_json::from_value(result["box"].clone()).unwrap();
    for p in octos_pptx::geometry::frame(b, -15.) {
        assert!(octos_pptx::geometry::contains(q, p, -4.99));
    }
}

#[cfg(unix)]
#[test]
fn refuses_symlink_escape_and_dangling_targets() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let ctx = Context::new(a.path()).unwrap();
    std::os::unix::fs::symlink(b.path(), a.path().join("escape")).unwrap();
    assert!(ctx.write_new("escape/a", b"private").is_err());
    std::os::unix::fs::symlink(b.path().join("absent"), a.path().join("dangling")).unwrap();
    assert!(ctx.write_new("dangling", b"private").is_err());
}

#[test]
fn relative_assets_multi_page_fit_and_crops_use_the_actual_files() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = fixture(dir.path());
    let mut spec = ctx.json("deck.json").unwrap();
    let mut page = spec["pages"][0].clone();
    page["id"] = json!("second");
    page["background"] = json!("../art.png");
    page["design"] = json!("../art.png");
    ctx.write_json("pages/second.json", &page).unwrap();
    spec["pages"]
        .as_array_mut()
        .unwrap()
        .push(json!({"path":"pages/second.json"}));
    // A feasible interval tests format preservation. The old 60x12 box could
    // not fit at the size floor; returning a partial shrink was not success.
    spec["pages"][0]["texts"][1]["box"] = json!([75, 200, 180, 25]);
    spec["pages"][0]["texts"][1]["format"] = json!({"color":"BB0000","italic":true});
    ctx.write_json("multi.json", &spec).unwrap();
    let proposal = ctx
        .execute(
            json!({"action":"fit","spec":"multi.json","objects":["overview/a"],"min_size_pt":8}),
        )
        .unwrap();
    let set = &proposal["proposals"][0]["set"];
    assert_eq!(set["format"]["color"], "BB0000");
    assert_eq!(set["format"]["italic"], true);
    assert!(set["format"]["size_pt"].as_f64().unwrap() < 18.);
    let build = ctx
        .execute(json!({"action":"build","spec":"multi.json","output":"multi"}))
        .unwrap();
    let doc = octos_pptx::deck::open(&ctx, &ctx.verify(&build["editable"]).unwrap()).unwrap();
    assert_eq!(doc.slides.len(), 2);
    let info = ctx
        .execute(json!({"action":"image_info","input":"art.png"}))
        .unwrap();
    assert_eq!(info["width"], 1280);
    let crop=ctx.execute(json!({"action":"crop","input":"art.png","output":"crop.png","box":[1270.5,710.5,9.5,9.5]})).unwrap();
    assert_eq!(crop["box"], json!([1270, 710, 10, 10]));
    assert_eq!(
        image::image_dimensions(ctx.path("crop.png").unwrap()).unwrap(),
        (10, 10)
    );
}

#[test]
fn malformed_autofit_duplicate_shapes_and_missing_relationships_are_reported() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Context::new(dir.path()).unwrap();
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default();
    for (name, xml) in [
        ("[Content_Types].xml", "<Types/>"),
        (
            "_rels/.rels",
            r#"<Relationships><Relationship Id="r1" Target="ppt/presentation.xml"/><Relationship Id="r2" Target="missing.xml"/></Relationships>"#,
        ),
        ("ppt/presentation.xml", "<presentation/>"),
        (
            "ppt/slides/slide1.xml",
            r#"<slide><cNvPr id="1"/><cNvPr id="1"/><bodyPr><noAutofit/><spAutoFit/></bodyPr></slide>"#,
        ),
    ] {
        zip.start_file(name, options).unwrap();
        zip.write_all(xml.as_bytes()).unwrap();
    }
    ctx.write_new("malformed.pptx", &zip.finish().unwrap().into_inner())
        .unwrap();
    let report = octos_pptx::review::preflight(&ctx, &ctx.path("malformed.pptx").unwrap()).unwrap();
    let issues = report["issues"].to_string();
    assert!(issues.contains("duplicate shape"));
    assert!(issues.contains("multiple text autofit"));
    assert!(issues.contains("missing relationship"));
}

#[test]
fn mcp_shutdown_has_no_extra_plugin_envelope() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_octos-pptx"))
        .arg("mcp")
        .current_dir(dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{},\"clientInfo\":{\"name\":\"fixture\",\"version\":\"1\"}}}\n").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(!text.contains("mcp_closed"));
    for row in text.lines() {
        let v: Value = serde_json::from_str(row).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
    }
    assert!(text.contains("serverInfo"), "{text}");
}

#[test]
fn invalid_structures_are_errors_and_font_fallback_is_visible() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = fixture(dir.path());
    assert!(ctx.execute(json!("not a request")).is_err());
    ctx.write_json("bad.json", &json!({"pages":["not a page"]}))
        .unwrap();
    assert!(
        ctx.execute(json!({"action":"build","spec":"bad.json","output":"bad"}))
            .is_err()
    );
    let mut spec = ctx.json("deck.json").unwrap();
    spec["styles"]["heading"] = json!("not a style");
    ctx.write_json("bad-style.json", &spec).unwrap();
    assert!(
        ctx.execute(json!({"action":"build","spec":"bad-style.json","output":"bad-style"}))
            .is_err()
    );
    spec = ctx.json("deck.json").unwrap();
    spec["styles"]["body"]["font"] = json!("Deliberately Absent Test Font 4F6D");
    ctx.write_json("fallback.json", &spec).unwrap();
    let result = ctx
        .execute(json!({"action":"cycle","spec":"fallback.json","output":"fallback","width":640}))
        .unwrap();
    assert!(
        result["inspection"]["font_summary"]["fallback_runs"]
            .as_u64()
            .unwrap()
            > 0
    );
    let report = ctx.json("fallback/inspection.json").unwrap();
    assert!(report["objects"].as_array().unwrap().iter().any(|r| {
        r["findings"]
            .as_array()
            .unwrap()
            .contains(&json!("font_fallback"))
    }));
}

#[cfg(target_os = "macos")]
#[test]
fn ocr_localizes_actual_text_in_image_coordinates() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = fixture(dir.path());
    let mut spec = ctx.json("deck.json").unwrap();
    // T-0558: draw the Chinese in one bundled Simplified Chinese face at a comfortable size.
    // The shared 18 pt "Inter" body style leaves Han glyphs to per-grapheme fallback, which
    // mixes Shippori Mincho (文字定位) and LXGW WenKai Lite (验证); Vision then drops the
    // leading 文 at this position. "LXGW WenKai Lite" is bundled from the pinned craft-fonts
    // input on every CI host, so the render (and this test) stay deterministic.
    spec["styles"]["cjk"] = json!({"font":"LXGW WenKai Lite","size_pt":32});
    spec["pages"][0]["copy"]["cn"] = json!("文字定位验证");
    spec["pages"][0]["texts"]
        .as_array_mut()
        .unwrap()
        .push(json!({"id":"cn","text":"文字定位验证","style":"cjk","box":[75,540,600,110]}));
    ctx.replace_json("deck.json", &spec).unwrap();
    ctx.execute(json!({"action":"cycle","spec":"deck.json","output":"ocr-source","width":1280}))
        .unwrap();
    let result = ctx.execute(json!({"action":"ocr","input":"ocr-source/rendered/slide-001.png","output":"ocr.json","languages":["zh-Hans","en-US"]})).unwrap();
    assert_eq!(result["image"]["width_px"], 1280);
    assert_eq!(result["image"]["height_px"], 720);
    assert_eq!(result["quality_approved"], false);
    let detections = result["detections"].as_array().unwrap();
    assert!(
        detections
            .iter()
            .any(|d| d["text"].as_str().unwrap().contains("文字定位验证")),
        "Chinese text must be recognized: {detections:?}; cn font usage: {}",
        cn_font_usage(&ctx, "ocr-source/inspection.json")
    );
    let title = detections
        .iter()
        .find(|d| d["text"].as_str().unwrap().contains("Native editable"))
        .expect("OCR must read actual glyphs");
    assert_eq!(title["quad"].as_array().unwrap().len(), 4);
    let b = title["box"].as_array().unwrap();
    assert!(b[0].as_f64().unwrap() >= 50.0 && b[0].as_f64().unwrap() < 160.0);
    assert!(b[1].as_f64().unwrap() >= 20.0 && b[1].as_f64().unwrap() < 120.0);
    assert!(
        ctx.execute(json!({"action":"ocr","input":"../outside.png"}))
            .is_err()
    );
    assert_eq!(ctx.json("ocr.json").unwrap(), result);
}

#[test]
fn evidence_refuses_directory_deliverables_before_initializing_policy() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Context::new(dir.path()).unwrap();
    for path in ["exports/", "."] {
        assert!(ctx.execute(json!({"action":"evidence_init","task_id":"test","owner_agent":"standalone-owner","stagnant_attempts":3,"deliverables":[path]})).is_err());
        assert!(!dir.path().join(".octos/continuation.json").exists());
    }
}
