//! Rolling working copy. Native verdicts own page acceptance, the final assembly owns delivery.
use super::assembled_bytes;
use crate::{Context, fs, page_gate, review_routes, string};
use anyhow::{Context as _, Result, ensure};
use serde_json::{Value, json};

pub(crate) const OUTPUT: &str = ".octos/pptx-progress/ready.pptx";
const STATE: &str = ".octos/pptx-progress/publication.json";
const LOCK: &str = ".octos/pptx-progress/publish.lock";

pub(crate) fn after_publication(ctx: &Context, plan: &Value, receipt: &mut Value) {
    let mut update = || -> Result<()> {
        if review_routes::per_page(ctx)? {
            refresh(ctx, plan)?;
            receipt["progress_path"] = json!(OUTPUT);
        }
        Ok(())
    };
    if let Err(error) = update() {
        // The page decision has already committed. Recovery needs no new model verdict.
        receipt["progress_error"] = json!(format!("{error:#}"));
    }
}

pub(crate) fn reconcile(ctx: &Context, request: &Value) -> Result<Value> {
    ensure!(
        request.as_object().is_some_and(|o| o.len() == 1),
        "page_progress only takes action"
    );
    ensure!(
        review_routes::per_page(ctx)?,
        "page_progress requires per_page delivery"
    );
    let plan = page_gate::plan(ctx)?;
    ensure!(
        ctx.continuation_owner.as_deref().map(page_gate::identity)
            == Some(page_gate::identity(string(&plan, "coordinator_session")?)),
        "only the declared coordinator can request a progress refresh"
    );
    refresh(ctx, &plan)
}

fn approved_slides(ctx: &Context, plan: &Value) -> Result<Vec<Value>> {
    let mut pages = Vec::new();
    for row in plan["pages"].as_array().context("missing page roster")? {
        if !ctx.path(string(row, "review")?)?.try_exists()? {
            continue;
        }
        // These native, archived verdicts already validated the observed checks.
        // A different candidate head (e.g. a refreshed render) needs a fresh verdict.
        let (review, _) = page_gate::read_review_head(ctx, plan, row)?;
        if review["accepted"] != true {
            continue;
        }
        let (candidate, head) = page_gate::load_candidate(ctx, row)?;
        if head != review["candidate"] {
            continue;
        }
        page_gate::review_ready(plan, &review)?;
        ensure!(
            candidate["artifact"] == review["artifact"],
            "verdict artifact mismatch"
        );
        pages.push(json!({"page_id":row["id"],"artifact":review["artifact"]}));
    }
    Ok(pages)
}

fn refresh(ctx: &Context, plan: &Value) -> Result<Value> {
    // No page lock is held here. Concurrent reviewers can publish their decisions;
    // queued writers rescan the latest heads and coalesce identical slide sets.
    let _guard = ctx.wait_for_lock(LOCK)?;
    let output = ctx.path(OUTPUT)?;
    ensure!(
        output == ctx.root.join(OUTPUT) && ctx.path(STATE)? == ctx.root.join(STATE),
        "progress destinations must be the native workspace paths"
    );
    let state_path = ctx.path(STATE)?;
    let prior = if state_path.try_exists()? {
        ctx.json(&state_path)?
    } else {
        Value::Null
    };
    let actual = if output.try_exists()? {
        json!(fs::sha_file(&output)?)
    } else {
        Value::Null
    };
    ensure!(
        actual.is_null()
            || actual == prior["sha256"]
            || prior.get("pending_sha256") == Some(&actual),
        "progress PPTX was created or changed externally; preserve it before refreshing"
    );

    let pages = approved_slides(ctx, plan)?;
    let page_ids: Vec<_> = pages.iter().map(|p| &p["page_id"]).collect();
    let relative_output = output.strip_prefix(&ctx.root)?.to_string_lossy();
    let status = |hash: &Value| {
        json!({"output": if hash.is_null() { Value::Null } else { json!({"path":relative_output,"sha256":hash}) },
        "pages":page_ids,"total_pages":plan["pages"].as_array().unwrap().len(),"quality_approved":false})
    };
    let inputs = json!({"engine_revision":crate::ENGINE_REV,"pages":pages});
    if prior["inputs"] == inputs
        && actual == prior["sha256"]
        && prior.get("pending_sha256").is_none()
        && (pages.is_empty() == actual.is_null())
    {
        return Ok(status(&actual));
    }
    if pages.is_empty() && actual.is_null() && prior.is_null() {
        return Ok(status(&actual));
    }
    let bytes = if pages.is_empty() {
        None
    } else {
        Some(assembled_bytes(ctx, &pages, &output)?)
    };
    let next = bytes.as_deref().map(fs::digest);
    let next = json!(next);
    if actual != next {
        // Persist both sides of the replacement first. After interruption the file
        // may contain either digest; unrelated files are never adopted or replaced.
        ctx.replace_json(&state_path, &json!({"sha256":actual,"pending_sha256":next}))?;
        match bytes {
            Some(bytes) if actual.is_null() => {
                ctx.write_new(&output, &bytes)?;
            }
            Some(bytes) => ctx.replace_bytes(&output, &bytes)?,
            None => std::fs::remove_file(&output)?,
        }
    }
    ctx.replace_json(&state_path, &json!({"inputs":inputs,"sha256":next}))?;
    Ok(status(&next))
}
