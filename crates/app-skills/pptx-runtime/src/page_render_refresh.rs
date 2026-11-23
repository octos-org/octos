use crate::{Context, deck, fs, page_gate, page_verdict, string};
use anyhow::{Context as _, Result, ensure};
use serde_json::{Value, json};

/// The assigned reviewer can change the preview of the same editable file.
/// The maker-owned build, source, artifact and dependencies stay bound. A
/// changed publication invalidates the old verdict until it is reviewed.
pub(crate) fn refresh(ctx: &Context, request: &Value) -> Result<Value> {
    ensure!(
        request
            .as_object()
            .context("missing request")?
            .keys()
            .all(|k| matches!(
                k.as_str(),
                "action" | "page" | "candidate_id" | "render_receipt" | "refresh_render_only"
            )),
        "render refresh only takes page, candidate_id and render_receipt"
    );
    let plan = page_gate::plan(ctx)?;
    ensure!(
        plan["schema_version"] == 2,
        "render refresh requires a version 2 plan"
    );
    let id = string(request, "page")?;
    let row = plan["pages"]
        .as_array()
        .context("missing pages")?
        .iter()
        .find(|p| p["id"] == id)
        .context("page outside active plan")?;
    let reviewer = ctx
        .continuation_owner
        .as_deref()
        .context("missing host reviewer identity")?;
    ensure!(
        row["reviewer_session"]
            .as_str()
            .is_some_and(|assigned| page_gate::identity(assigned) == page_gate::identity(reviewer)),
        "only the assigned page reviewer can refresh its render"
    );
    page_gate::independent(&plan, row, reviewer)?;
    let _lock = page_verdict::lock_page(ctx, id)?;
    let (current, binding) = page_gate::load_candidate(ctx, row)?;
    let verdict_path = ctx.path(string(row, "review")?)?;
    let previous_verdict = if verdict_path.exists() {
        let (_, prior) = page_gate::read_review_head(ctx, &plan, row)?;
        Some(page_verdict::token("verdict", &prior)?)
    } else {
        None
    };
    let expected = string(request, "candidate_id")?;
    let head = page_verdict::token("candidate", &binding)?;
    let archive = ctx.path(format!(
        ".octos/page-render-revisions/{}.json",
        fs::digest(expected.as_bytes())
    ))?;
    let original = if head == expected {
        ctx.read(ctx.verify(&binding)?)?
    } else {
        ctx.read(&archive)
            .context("stale candidate; read the current page publication")?
    };
    ensure!(
        page_verdict::token("candidate", &json!({"sha256":fs::digest(&original)}))? == expected,
        "archived page candidate differs"
    );
    let mut next: Value = serde_json::from_slice(&original)?;
    let render_path = ctx.path(string(request, "render_receipt")?)?;
    ensure!(
        render_path.file_name().is_some_and(|f| f == "render.json"),
        "expected a native render.json"
    );
    let receipt = ctx.json(&render_path)?;
    let receipt_binding = ctx.binding(&render_path)?;
    ensure!(
        receipt["input"] == next["artifact"],
        "new render must bind the identical editable PPTX"
    );
    let width = receipt["requested_width"]
        .as_u64()
        .context("missing requested render width")?;
    // Reuse the normal existing-render validation without rendering a new file.
    let checked = deck::render(
        ctx,
        &json!({"input":next["artifact"]["path"],
        "output":render_path.parent().context("missing render directory")?,"width":width}),
    )?;
    ensure!(
        checked == receipt,
        "render receipt changed while validating it"
    );
    next["render"] = receipt["pages"][0].clone();
    next["render_receipt"] = receipt_binding;
    page_gate::validate_candidate(ctx, row, &next)?;
    ensure!(
        head == expected || next == current,
        "stale candidate; the editable file or its evidence changed"
    );
    if next != current {
        if archive.exists() {
            ensure!(
                ctx.read(&archive)? == original,
                "archived page candidate changed"
            );
        } else {
            ctx.write_new(&archive, &original)?;
        }
        let destination = ctx.verify(&binding)?;
        ctx.replace_json(&destination, &next)?;
    }
    let updated = ctx.binding(string(row, "candidate")?)?;
    let mut receipt = json!({"page":id, "candidate_id":page_verdict::token("candidate", &updated)?,
        "candidate":updated, "render":next["render"], "artifact":next["artifact"],
        "changed":next != current, "quality_approved":false, "replaces_verdict_id":previous_verdict,
        "next":"Inspect the exact bound render and use page_review_submit with this candidate_id."});
    drop(_lock);
    crate::page_assembly::progress::after_publication(ctx, &plan, &mut receipt);
    Ok(receipt)
}
