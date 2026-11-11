use image::{Rgb, RgbImage};
use octos_pptx::Context;
use serde_json::json;

fn rectangle(im: &mut RgbImage, x: u32, y: u32, w: u32, h: u32) {
    for px in x..=x + w {
        for dy in 0..3 {
            im.put_pixel(px, y + dy, Rgb([30, 60, 45]));
            im.put_pixel(px, y + h - dy, Rgb([30, 60, 45]));
        }
    }
    for py in y..=y + h {
        for dx in 0..3 {
            im.put_pixel(x + dx, py, Rgb([30, 60, 45]));
            im.put_pixel(x + w - dx, py, Rgb([30, 60, 45]));
        }
    }
}

fn fixture() -> (tempfile::TempDir, Context) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Context::new(dir.path()).unwrap();
    let mut art = RgbImage::from_pixel(640, 360, Rgb([245, 240, 225]));
    rectangle(&mut art, 40, 40, 360, 160);
    art.save(ctx.root.join("blank.png")).unwrap();
    ctx.write_json("deck.json", &json!({"schema_version":1,
      "canvas":{"width_px":640,"height_px":360,"width_in":12.8,"height_in":7.2},
      "styles":{"body":{"font":"Inter","size_pt":18}},
      "pages":[{"id":"sample","background":"blank.png","copy":{"caption":"Example label"},
      "regions":{"card":{"quad":[[40,40],[400,40],[400,200],[40,200]],"scope":"label","basis":"A model says this is measured"}},
      "texts":[{"id":"caption","text":"Example label","style":"body","box":[60,60,280,80],"region":"card"}]}]})).unwrap();
    ctx.execute(json!({"action":"evidence_init","task_id":"visual-proof","owner_agent":"test","stagnant_attempts":3,"deliverables":["final.pptx"]})).unwrap();
    (dir, ctx)
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
#[test]
fn independent_route_preserves_unknowns_and_rejects_self_approval_and_native_failures() {
    use octos_pptx::review_routes as routes;
    let (_dir, mut ctx) = fixture();
    // A test-only alternate initial plan. Production init freezes this before work.
    let plan = routes::plan(
        &json!({"verification_mode":"independent_visual","reviewer_agent":"peer-reviewer"}),
        "test",
    )
    .unwrap();
    ctx.replace_json(routes::PLAN_PATH, &plan).unwrap();
    assert!(
        routes::plan(
            &json!({"verification_mode":"independent_visual","reviewer_agent":"test"}),
            "test"
        )
        .is_err()
    );
    cycle(&ctx, "route");
    let binding = ctx.binding("route/cycle.json").unwrap();
    let spec = serde_json::from_value(ctx.json("route/resolved-spec.json").unwrap()).unwrap();
    let policy = ctx.json(".octos/continuation.json").unwrap();
    let mut review = json!({"object_reviews":[{"page":"sample","object_id":"caption","checks":{}}],
        "measurement_reviews":[{"id":"owner","decision":"accepted_visual","basis":"The label sits inside the entire visible border, excluding its outer shadow."}]});
    for k in octos_pptx::object_review::CHECKS {
        review["object_reviews"][0]["checks"][k] =
            json!({"status":"pass","observation":"Checked synthetic pixels"});
    }
    let mut qa = json!({"status":"unknown","findings":[{"id":"owner","page":1,"object_id":"caption","code":"OWNER_BOUNDARY_UNVERIFIED","status":"unknown"}]});
    ctx.continuation_owner = Some("test".into());
    routes::attest(&ctx, &binding, &mut review).unwrap();
    assert!(!routes::authorized(&ctx, &policy, &review, &binding).unwrap());
    assert!(routes::adjudicated(&qa, &review, false, &spec, &plan).is_err());
    ctx.continuation_owner = Some("peer-reviewer".into());
    assert!(routes::permitted(&ctx).unwrap());
    routes::attest(&ctx, &binding, &mut review).unwrap();
    assert!(routes::authorized(&ctx, &policy, &review, &binding).unwrap());
    assert!(
        routes::adjudicated(&qa, &review, true, &spec, &plan)
            .unwrap()
            .contains("owner")
    );
    assert_eq!(qa["findings"][0]["status"], "unknown");
    let mut stale = binding.clone();
    stale["sha256"] = json!("different");
    assert!(routes::authorized(&ctx, &policy, &review, &stale).is_err());
    review["measurement_reviews"][0]["basis"] = json!("Changed after attestation");
    assert!(routes::authorized(&ctx, &policy, &review, &binding).is_err());
    qa["findings"][0]["status"] = json!("fail");
    assert!(routes::adjudicated(&qa, &review, true, &spec, &plan).is_err());
    qa["findings"][0]["status"] = json!("unknown");
    qa["findings"][0]["code"] = json!("CONSTRAINT_CHANGED");
    assert!(routes::adjudicated(&qa, &review, true, &spec, &plan).is_err());
    assert!(
        ctx.execute(json!({"action":"evidence_retarget","verification_mode":"strict"}))
            .is_err()
    );
}
