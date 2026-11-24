//! Assemble actual accepted artifacts with source formatting, in declared plan order.
use crate::{Context, deck, fs, page_gate, page_verdict, string};
use anyhow::{Context as _, Result, ensure};
use deckcraft_engine::{
    Session,
    cmd::{base64_encode, file},
};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

pub(crate) mod progress;
mod repair;

fn approved_inputs(ctx: &Context) -> Result<Value> {
    let plan_binding = ctx
        .binding(page_gate::PLAN)
        .context("page_assemble requires an active page plan")?;
    let plan = page_gate::plan(ctx)?;
    let mut pages = Vec::new();
    for row in plan["pages"].as_array().context("missing plan pages")? {
        let id = string(row, "id")?;
        let (candidate, binding) = page_gate::load_candidate(ctx, row)
            .with_context(|| format!("page {id} has no candidate"))?;
        page_gate::validate_candidate(ctx, row, &candidate)
            .with_context(|| format!("page {id} candidate is stale"))?;
        let (review, review_binding) = page_gate::current_review(ctx, &plan, row, &binding)
            .with_context(|| format!("page {id} review is stale"))?;
        page_gate::review_ready(&plan, &review)
            .with_context(|| format!("page {id} has not passed review"))?;
        ctx.verify(&review_binding)?;
        ctx.verify(&binding)?;
        pages.push(
            json!({"page_id":id,"candidate_id":page_verdict::token("candidate",&binding)?,
            "candidate":binding,"artifact":candidate["artifact"],"render":candidate["render"],
            "source":candidate["source"],"review":review_binding}),
        );
    }
    ctx.verify(&plan_binding)?;
    Ok(json!({"plan":plan_binding,"pages":pages}))
}

fn assembled_bytes(ctx: &Context, pages: &[Value], output: &Path) -> Result<Vec<u8>> {
    let mut session = Session::new();
    session.prefs.undo_limit = 1;
    for (index, page) in pages.iter().enumerate() {
        let artifact = &page["artifact"];
        let path = ctx.path(string(artifact, "path")?)?;
        let bytes = ctx.read(&path)?;
        ensure!(
            fs::digest(&bytes) == artifact["sha256"],
            "accepted artifact changed for page {}",
            page["page_id"]
        );
        let doc = deck::open_bytes(&path, &bytes)?;
        ensure!(
            doc.slides.len() == 1,
            "accepted artifact must contain one slide"
        );
        if pages.len() == 1 {
            return Ok(bytes);
        }
        if index == 0 {
            file::add_opened(&mut session, "accepted page", None, doc);
        } else {
            ensure!(
                doc.slide_size == session.doc()?.doc.slide_size,
                "page {} has a different physical slide size; a PPTX has one size for all slides",
                page["page_id"]
            );
            drop(doc);
            // Pass the same checked bytes to slide.reuse even if the source path changes.
            session.execute(
                "slide.reuse",
                &json!({"data":base64_encode(&bytes),
                "name":"accepted.pptx","slides":[0],"at":index,"keepFormatting":true}),
            )?;
            session.journal.clear();
        }
    }
    let bytes = file::save_bytes(&session.doc()?.doc, "pptx")?;
    ensure!(
        deck::open_bytes(output, &bytes)?.slides.len() == pages.len(),
        "assembled PPTX lost slides"
    );
    Ok(bytes)
}

// The output name is normalized by Context::path before selecting this receipt.
fn receipt_path(ctx: &Context, output: &Path) -> Result<PathBuf> {
    let relative = output
        .strip_prefix(&ctx.root)?
        .to_str()
        .context("non-UTF8 output path")?;
    ctx.path(format!(
        ".octos/pptx-assemblies/{}.json",
        fs::digest(relative.as_bytes())
    ))
}

fn receipt_body(output: &Value, inputs: &Value, per_page: bool) -> Value {
    json!({"kind":"page-assembly-v1","output":output,"plan":inputs["plan"],"pages":inputs["pages"],
        "engine_revision":crate::ENGINE_REV,"requires_final_review":!per_page,"quality_approved":false})
}

pub(crate) fn delivered_receipt(ctx: &Context, binding: &Value) -> Result<Value> {
    let path = ctx.verify(binding)?;
    let receipt = ctx.json(&path)?;
    let output = ctx.verify(&receipt["output"])?;
    ensure!(
        receipt_path(ctx, &output)? == path,
        "not the canonical page assembly receipt"
    );
    ensure!(
        receipt["plan"] == ctx.binding(page_gate::PLAN)?
            && receipt
                == receipt_body(
                    &ctx.binding(&output)?,
                    &receipt,
                    crate::review_routes::per_page(ctx)?
                ),
        "delivered assembly differs from the active plan or engine"
    );
    let plan = page_gate::plan(ctx)?;
    let planned = plan["pages"].as_array().context("missing plan pages")?;
    let delivered = receipt["pages"]
        .as_array()
        .context("missing assembly pages")?;
    ensure!(
        !planned.is_empty()
            && planned.len() == delivered.len()
            && planned
                .iter()
                .zip(delivered)
                .all(|(p, d)| p["id"] == d["page_id"]
                    && p["candidate"] == d["candidate"]["path"]
                    && p["review"] == d["review"]["path"]),
        "delivered page order or evidence routes differ from the active plan"
    );
    ctx.verify(binding)?;
    Ok(receipt)
}

pub(crate) fn verify_delivered_bytes(ctx: &Context, receipt: &Value) -> Result<()> {
    let output = ctx.verify(&receipt["output"])?;
    let bytes = assembled_bytes(
        ctx,
        receipt["pages"]
            .as_array()
            .context("missing assembly pages")?,
        &output,
    )?;
    ensure!(
        fs::digest(&bytes) == receipt["output"]["sha256"],
        "recorded page artifacts do not reproduce the delivered package"
    );
    Ok(())
}

pub(crate) fn verified_receipt(ctx: &Context, binding: &Value) -> Result<Value> {
    let receipt = delivered_receipt(ctx, binding)?;
    let current = receipt_body(
        &receipt["output"],
        &approved_inputs(ctx)?,
        crate::review_routes::per_page(ctx)?,
    );
    ensure!(
        receipt == current,
        "assembly receipt no longer matches current approved pages and reviews"
    );
    ctx.verify(binding)?;
    Ok(receipt)
}

// Only a review binding may advance, and only after independently revalidating
// its replacement. Candidates, source bytes, page order, plan and engine stay pinned.
fn verify_refresh(prior: &Value, current: &Value) -> Result<()> {
    let mut updated = prior.clone();
    let old = updated
        .get_mut("pages")
        .and_then(Value::as_array_mut)
        .context("invalid assembly receipt pages")?;
    let new = current["pages"]
        .as_array()
        .context("missing approved pages")?;
    ensure!(old.len() == new.len(), "assembly page count changed");
    for (old, new) in old.iter_mut().zip(new) {
        let prior_review = old
            .get_mut("review")
            .context("missing prior review binding")?;
        ensure!(
            prior_review["path"] == new["review"]["path"]
                && prior_review["sha256"].as_str().is_some_and(
                    |hash| hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit())
                ),
            "invalid prior review binding"
        );
        *prior_review = new["review"].clone();
    }
    ensure!(
        updated == *current,
        "assembly output, plan, candidates or engine changed; use a fresh output path"
    );
    Ok(())
}

pub fn assemble(ctx: &Context, request: &Value) -> Result<Value> {
    ensure!(
        request
            .as_object()
            .context("missing assembly request")?
            .keys()
            .all(|key| matches!(
                key.as_str(),
                "action" | "output" | "repair_review" | "stage_repair_render"
            )),
        "page_assemble takes output, optional repair_review and stage_repair_render; page selection and order come from the active plan"
    );
    let plan_binding = ctx
        .binding(page_gate::PLAN)
        .context("page_assemble requires an active page plan")?;
    let plan = ctx.json(ctx.verify(&plan_binding)?)?;
    if plan["schema_version"] == 2 {
        ensure!(
            ctx.continuation_owner.as_deref().map(page_gate::identity)
                == Some(page_gate::identity(string(&plan, "coordinator_session")?)),
            "only the declared page coordinator can assemble a version 2 plan"
        );
    }
    let output = ctx.path(string(request, "output")?)?;
    ensure!(
        output.extension().is_some_and(|ext| ext == "pptx"),
        "output must end in .pptx"
    );
    ensure!(
        !crate::review_routes::per_page(ctx)? || output != ctx.root.join(progress::OUTPUT),
        "choose a final output distinct from the rolling progress PPTX"
    );
    let path = receipt_path(ctx, &output)?;
    let lock = path.with_extension("lock");
    let _output_lock = ctx.lock_file(lock.to_str().context("non-UTF8 lock path")?)?;
    let stage = request
        .get("stage_repair_render")
        .map(|value| {
            value
                .as_bool()
                .context("stage_repair_render must be a boolean")
        })
        .transpose()?
        .unwrap_or(false);
    if let Some(review) = request.get("repair_review") {
        ensure!(
            !crate::review_routes::per_page(ctx)?,
            "per_page repairs must close on individual pages before compilation"
        );
        return repair::promote(ctx, review, &output, &path, stage);
    }
    ensure!(!stage, "stage_repair_render requires a bound repair_review");
    let existing = output.exists();
    ensure!(
        !existing || path.exists(),
        "existing output has no native assembly receipt; use a fresh output path"
    );
    let prior = path.exists().then(|| ctx.json(&path)).transpose()?;
    let inputs = approved_inputs(ctx)?;
    ensure!(
        inputs["plan"] == plan_binding,
        "page plan changed before assembly"
    );
    let pages = inputs["pages"]
        .as_array()
        .context("missing approved pages")?;
    // On retry there is no DeckCraft session, serialization or renderer call.
    let bytes = if existing {
        None
    } else {
        Some(assembled_bytes(ctx, pages, &output)?)
    };
    let _locks = pages
        .iter()
        .map(|page| page_verdict::lock_page(ctx, string(page, "page_id")?))
        .collect::<Result<Vec<_>>>()?;
    ensure!(
        approved_inputs(ctx)? == inputs,
        "approved pages or reviews changed during assembly; retry with the current plan"
    );
    let output_binding = match &bytes {
        Some(bytes) => {
            json!({"path":output.strip_prefix(&ctx.root)?.to_str().context("non-UTF8 output path")?, "sha256":fs::digest(bytes)})
        }
        None => ctx.binding(&output)?,
    };
    let mut result = receipt_body(
        &output_binding,
        &inputs,
        crate::review_routes::per_page(ctx)?,
    );
    if let Some(prior) = prior {
        verify_refresh(&prior, &result)?;
        if prior != result {
            ctx.replace_json(&path, &result)?;
        }
    } else {
        // Bind before publishing. An interruption between these writes can resume
        // only from the same verified inputs and expected output bytes.
        ctx.write_json(&path, &result)?;
    }
    if let Some(bytes) = bytes {
        ctx.write_new(&output, &bytes)?;
    }
    result["receipt"] = ctx.binding(path)?;
    Ok(result)
}
