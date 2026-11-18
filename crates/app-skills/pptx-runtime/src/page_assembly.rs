//! Assemble actual accepted artifacts with source formatting, in declared plan order.
use crate::{Context, deck, fs, page_gate, page_verdict, string};
use anyhow::{Context as _, Result, ensure};
use deckcraft_engine::{
    Session,
    cmd::{base64_encode, file},
};
use serde_json::{Value, json};
use std::path::Path;

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

pub fn assemble(ctx: &Context, request: &Value) -> Result<Value> {
    ensure!(
        request
            .as_object()
            .context("missing assembly request")?
            .keys()
            .all(|key| matches!(key.as_str(), "action" | "output")),
        "page_assemble takes only output; page selection and order come from the active plan"
    );
    let output = ctx.path(string(request, "output")?)?;
    ensure!(
        output.extension().is_some_and(|ext| ext == "pptx"),
        "output must end in .pptx"
    );
    ensure!(!output.exists(), "use a fresh output PPTX path");
    let inputs = approved_inputs(ctx)?;
    let pages = inputs["pages"]
        .as_array()
        .context("missing approved pages")?;
    let bytes = assembled_bytes(ctx, pages, &output)?;
    // Native publication and reviewer actions use these same nonblocking locks.
    let _locks = pages
        .iter()
        .map(|page| page_verdict::lock_page(ctx, string(page, "page_id")?))
        .collect::<Result<Vec<_>>>()?;
    ensure!(
        approved_inputs(ctx)? == inputs,
        "approved pages or reviews changed during assembly; retry with the current plan"
    );
    let path = ctx.write_new(output, &bytes)?;
    Ok(
        json!({"output":ctx.binding(path)?,"plan":inputs["plan"],"pages":pages,
        "engine_revision":crate::ENGINE_REV,"requires_final_review":true,"quality_approved":false}),
    )
}
