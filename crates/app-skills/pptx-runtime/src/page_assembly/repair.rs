//! Replace a reviewed delivery in place only after a bound, independent rejection.
use super::{approved_inputs, assembled_bytes, receipt_body};
use crate::{Context, assembly_evidence, deck, fs, page_gate, page_verdict, review_routes, string};
use anyhow::{Context as _, Result, ensure};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

fn relative<'a>(ctx: &Context, path: &'a Path) -> Result<&'a str> {
    path.strip_prefix(&ctx.root)?
        .to_str()
        .context("non-UTF8 delivery path")
}

fn rows(body: &Value) -> Result<&Vec<Value>> {
    body["pages"]
        .as_array()
        .context("receipt is missing its page list")
}

fn reviewed_failure(ctx: &Context, review: &Value) -> Result<Value> {
    let policy = ctx.json(".octos/continuation.json")?;
    let owner = ctx
        .continuation_owner
        .as_deref()
        .context("host coordinator identity required")?;
    ensure!(
        page_gate::identity(owner) == page_gate::identity(string(&policy, "owner_agent")?),
        "only the delivery coordinator can replace a reviewed deck"
    );
    let body = ctx.json(ctx.verify(review)?)?;
    ensure!(
        body["kind"] == "pptx-assembly-review-v1"
            && body["task_id"] == policy["task_id"]
            && body["verification_plan"] == ctx.binding(review_routes::PLAN_PATH)?,
        "repair review belongs to a different task"
    );
    assembly_evidence::verify_attestation(ctx, &body)?;
    assembly_evidence::current_review(ctx, &body, review)?;
    assembly_evidence::independent(ctx, &policy, string(&body, "reviewer_session")?)?;
    ensure!(
        body["checks"].as_array().is_some_and(|checks| checks
            .iter()
            .any(|c| c["status"] == "fail" || c["status"] == "unknown")),
        "previous final review has no unresolved check"
    );
    ctx.verify(&body["observations"])?;
    ctx.verify(&body["report"])?;
    Ok(body)
}

fn prepare(
    ctx: &Context,
    dir: &Path,
    review: &Value,
    inputs: &Value,
    output: &Path,
    receipt: &Path,
    render_dir: &Path,
) -> Result<Value> {
    ensure!(
        ctx.verify(&review["assembly"])? == receipt,
        "previous review names a different delivery"
    );
    let old = ctx.json(receipt)?;
    ensure!(
        ctx.verify(&old["output"])? == output
            && old["plan"] == inputs["plan"]
            && old["kind"] == "page-assembly-v1",
        "old output or assembly plan differs from this task"
    );
    let pages = rows(inputs)?;
    let previous = rows(&old)?;
    ensure!(
        previous.len() == pages.len()
            && previous
                .iter()
                .zip(pages)
                .all(|(a, b)| a["page_id"] == b["page_id"]),
        "page order or coverage changed; this repair requires the same plan"
    );
    ensure!(
        previous
            .iter()
            .zip(pages)
            .any(|(a, b)| a["artifact"]["sha256"] != b["artifact"]["sha256"]),
        "repair requires a changed independently approved slide"
    );
    let old_renders = ctx.json(ctx.verify(&review["render_receipt"])?)?;
    let images = rows(&old_renders)?;
    ensure!(
        old_renders["input"] == old["output"]
            && old_renders["engine_revision"] == old["engine_revision"]
            && images.len() == pages.len(),
        "prior render does not cover the reviewed package"
    );
    for (i, row) in images.iter().enumerate() {
        ensure!(
            row["page"] == i + 1
                && ctx.verify(row)? == render_dir.join(format!("slide-{:03}.png", i + 1)),
            "previous render file is missing, changed or outside its declared location"
        );
    }

    let staged = dir.join("deck.pptx");
    let bytes = assembled_bytes(ctx, pages, output)?;
    let hash = fs::digest(&bytes);
    if staged.exists() {
        ensure!(
            ctx.binding(&staged)?["sha256"] == hash,
            "staged PPTX belongs to a different repair"
        );
    } else {
        ctx.write_new(&staged, &bytes)?;
    }
    drop(bytes);
    let next = receipt_body(
        &json!({"path":relative(ctx,output)?,"sha256":hash}),
        inputs,
        false,
    );
    let width = old_renders["requested_width"]
        .as_u64()
        .or_else(|| images[0]["width"].as_u64())
        .context("missing reviewed render width")?;
    let request = json!({"action":"render","input":relative(ctx,&staged)?,
        "output":relative(ctx,&dir.join("png"))?,"width":width});
    Ok(
        json!({"prior_assembly":old, "prior_render":old_renders, "assembly":next, "render_request":request}),
    )
}

fn complete_render(ctx: &Context, mut prepared: Value) -> Result<Value> {
    let mut next_renders = deck::render(ctx, &prepared["render_request"])?;
    next_renders["input"] = prepared["assembly"]["output"].clone();
    for (page, old_page) in next_renders["pages"]
        .as_array_mut()
        .context("incomplete staged render")?
        .iter_mut()
        .zip(rows(&prepared["prior_render"])?)
    {
        page["path"] = old_page["path"].clone();
    }
    prepared
        .as_object_mut()
        .context("invalid staged repair")?
        .remove("render_request");
    prepared["render"] = next_renders;
    Ok(prepared)
}

struct Replacement {
    dest: PathBuf,
    staged: PathBuf,
    prior: String,
    next: String,
}

impl Replacement {
    // All destinations and staged payloads are checked before the first rename.
    // After interruption each owned file may be either generation, nothing else.
    fn preflight(&self, ctx: &Context) -> Result<()> {
        let found = ctx.binding(&self.dest)?;
        ensure!(
            found["sha256"] == self.prior || found["sha256"] == self.next,
            "delivery file changed outside the pending repair: {}",
            self.dest.display()
        );
        if self.staged.exists() || found["sha256"] != self.next {
            ensure!(
                ctx.binding(&self.staged)?["sha256"] == self.next,
                "staged repair file changed or disappeared"
            );
        }
        Ok(())
    }

    fn publish(&self, ctx: &Context) -> Result<()> {
        self.preflight(ctx)?;
        if ctx.binding(&self.dest)?["sha256"] != self.next {
            std::fs::rename(&self.staged, &self.dest)
                .context("promote staged file within the task filesystem")?;
        } else if self.staged.exists() {
            std::fs::remove_file(&self.staged)?;
        }
        Ok(())
    }
}

fn change(
    ctx: &Context,
    before: &Value,
    after: &Value,
    expected: &Path,
    staged: PathBuf,
) -> Result<Replacement> {
    ensure!(
        string(before, "path")? == relative(ctx, expected)?
            && after["path"] == before["path"]
            && ctx.path(expected)? == expected,
        "repair must preserve canonical owned delivery paths"
    );
    Ok(Replacement {
        dest: expected.to_owned(),
        staged,
        prior: string(before, "sha256")?.into(),
        next: string(after, "sha256")?.into(),
    })
}

fn both_generations(ctx: &Context, path: &Path, before: &Value, after: &Value) -> Result<()> {
    let current = ctx.json(path)?;
    ensure!(
        &current == before || &current == after,
        "stale repair: a different receipt owns {}",
        path.display()
    );
    Ok(())
}

pub(super) fn promote(
    ctx: &Context,
    binding: &Value,
    output: &Path,
    receipt: &Path,
    stage: bool,
) -> Result<Value> {
    let _evidence = ctx.lock_file(".octos/pptx-evidence.lock")?;
    let review = reviewed_failure(ctx, binding)?;
    ensure!(
        string(&review["assembly"], "path")? == relative(ctx, receipt)?,
        "failed review is for a different output"
    );
    let renders = ctx.path(string(&review["render_receipt"], "path")?)?;
    let render_dir = renders.parent().context("missing render directory")?;
    ensure!(
        renders
            .file_name()
            .is_some_and(|name| name == "render.json")
            && !renders.starts_with(ctx.root.join(".octos")),
        "invalid final render receipt location"
    );
    let _renders = deck::lock_render(ctx, render_dir)?;
    let inputs = approved_inputs(ctx)?;
    let _pages = rows(&inputs)?
        .iter()
        .map(|p| page_verdict::lock_page(ctx, string(p, "page_id")?))
        .collect::<Result<Vec<_>>>()?;

    // Keep old/new metadata for failed-response replay. Staged decks and images
    // are moved into place (or discarded when identical) rather than retained.
    let journal = ctx.path(format!(
        ".octos/pptx-assembly-repairs/{}/repair.json",
        fs::digest(&serde_json::to_vec(binding)?)
    ))?;
    let dir = journal
        .parent()
        .context("missing journal directory")?
        .join(fs::digest(&serde_json::to_vec(&inputs)?));
    let tx = if journal.exists() {
        if stage {
            return Ok(
                json!({"stage":"journal_exists","repair_history":ctx.binding(&journal)?,
                "next":"Resume the same repair without stage_repair_render."}),
            );
        }
        ctx.json(&journal)?
    } else {
        let prepared = prepare(ctx, &dir, &review, &inputs, output, receipt, render_dir)?;
        let request = &prepared["render_request"];
        let pending = ctx.path(string(request, "output")?)?.join("render.json");
        let raster_probe = if stage || pending.is_file() {
            Value::Null
        } else {
            crate::renderer_probe::check(ctx, &json!({}))?
        };
        if stage || raster_probe["passed"] == false {
            return Ok(
                json!({"stage":"awaiting_repair_render","render_request":request,
                "staged_pptx":ctx.binding(string(request,"input")?)?,
                "local_renderer_probe":raster_probe,
                "next":"Render this request with the approved DeckCraft deployment, then repeat page_assemble with the same repair_review and omit stage_repair_render."}),
            );
        }
        let tx = complete_render(ctx, prepared)?;
        ensure!(
            approved_inputs(ctx)? == inputs,
            "approved pages changed during repair preparation"
        );
        ctx.write_json(&journal, &tx)?;
        tx
    };
    let (old, new) = (&tx["prior_assembly"], &tx["assembly"]);
    let (old_render, new_render) = (&tx["prior_render"], &tx["render"]);
    for (old, binding) in [
        (old, &review["assembly"]),
        (old_render, &review["render_receipt"]),
    ] {
        ensure!(
            fs::digest(&serde_json::to_vec_pretty(old)?) == binding["sha256"],
            "archived prior receipt differs from independent review"
        );
    }
    ensure!(
        *new == receipt_body(&new["output"], &inputs, false)
            && new_render["input"] == new["output"]
            && new_render["engine_revision"] == new["engine_revision"],
        "staged repair is for different approved inputs"
    );
    both_generations(ctx, receipt, old, new)?;
    both_generations(ctx, &renders, old_render, new_render)?;
    let mut files = vec![change(
        ctx,
        &old["output"],
        &new["output"],
        output,
        dir.join("deck.pptx"),
    )?];
    let old_images = rows(old_render)?;
    let images = rows(new_render)?;
    ensure!(
        images.len() == old_images.len() && images.len() == rows(&inputs)?.len(),
        "staged render coverage changed"
    );
    for (i, (prior, next)) in old_images.iter().zip(images).enumerate() {
        let name = format!("slide-{:03}.png", i + 1);
        files.push(change(
            ctx,
            prior,
            next,
            &render_dir.join(&name),
            dir.join("png").join(name),
        )?);
    }
    for file in &files {
        file.preflight(ctx)?;
    }
    ensure!(
        approved_inputs(ctx)? == inputs,
        "approved pages changed while staging repair"
    );
    for file in &files {
        file.publish(ctx)?;
    }
    for (path, before, after) in [
        (renders.as_path(), old_render, new_render),
        (receipt, old, new),
    ] {
        both_generations(ctx, path, before, after)?;
        if ctx.json(path)? != *after {
            ctx.replace_json(path, after)?;
        }
    }
    let mut result = new.clone();
    result["receipt"] = ctx.binding(receipt)?;
    result["render_receipt"] = ctx.binding(renders)?;
    result["repair_history"] = ctx.binding(journal)?;
    Ok(result)
}
