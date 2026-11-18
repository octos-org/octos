//! Task-local admission for the declared one-maker-per-page workflow.
//! Verifies recorded evidence; never scores pixels or authenticates peer execution.
use crate::{Context, string};
use anyhow::{Context as _, Result, ensure};
use quick_xml::{Reader, events::Event};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    io::{Cursor, Read},
    path::Path,
};

pub const PLAN: &str = ".octos/pptx-page-plan.json";

pub(crate) fn identity(s: &str) -> &str {
    s.strip_prefix("session:").unwrap_or(s)
}

fn nonempty<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    let s = string(v, key)?;
    ensure!(
        !s.is_empty() && s.trim() == s,
        "{key} must be a nonempty trimmed string"
    );
    Ok(s)
}
fn array<'a>(v: &'a Value, key: &str) -> Result<&'a Vec<Value>> {
    v[key]
        .as_array()
        .with_context(|| format!("{key} must be an array"))
}
fn bindings(ctx: &Context, v: &Value, key: &str, required: bool) -> Result<()> {
    let rows = array(v, key)?;
    ensure!(
        !required || !rows.is_empty(),
        "{key} must contain bound evidence"
    );
    for row in rows {
        ctx.verify(row)
            .with_context(|| format!("{key}: stale/missing evidence"))?;
    }
    Ok(())
}
pub(crate) fn plan(ctx: &Context) -> Result<Value> {
    let p = ctx.json(PLAN)?;
    ensure!(
        p["schema_version"] == 1,
        "unsupported page plan schema_version"
    );
    let coordinator = nonempty(&p, "coordinator_session")?;
    let min = p
        .get("min_score")
        .map(|v| v.as_f64().context("min_score must be numeric"))
        .transpose()?
        .unwrap_or(95.0);
    ensure!(
        min.is_finite() && (0.0..=100.0).contains(&min),
        "min_score must be between 0 and 100"
    );
    crate::page_verdict::check_rubric(&p)?;
    let pages = array(&p, "pages")?;
    ensure!(!pages.is_empty(), "page plan must cover at least one page");
    let mut ids = BTreeSet::new();
    let mut makers = BTreeSet::new();
    let mut publications = BTreeSet::new();
    for page in pages {
        let id = nonempty(page, "id")?;
        let maker = nonempty(page, "maker_session")?;
        ensure!(ids.insert(id), "duplicate page id {id}");
        ensure!(
            makers.insert(identity(maker)),
            "one page per maker required; duplicate maker_session {maker}"
        );
        ensure!(
            identity(maker) != identity(coordinator),
            "coordinator cannot be a page maker"
        );
        for field in ["candidate", "review"] {
            ensure!(
                publications.insert(ctx.path(nonempty(page, field)?)?),
                "each page needs its own candidate and review paths"
            );
        }
    }
    Ok(p)
}
fn slide_count(ctx: &Context, path: &Path) -> Result<usize> {
    let mut zip = zip::ZipArchive::new(Cursor::new(ctx.read(path)?))?;
    let mut xml = Vec::new();
    zip.by_name("ppt/presentation.xml")?
        .take(8 * 1024 * 1024 + 1)
        .read_to_end(&mut xml)?;
    ensure!(
        xml.len() <= 8 * 1024 * 1024,
        "presentation XML exceeds 8 MiB"
    );
    let mut reader = Reader::from_reader(xml.as_slice());
    let mut count = 0;
    let mut in_list = false;
    loop {
        match reader.read_event()? {
            Event::Start(e) | Event::Empty(e) => match e.local_name().as_ref() {
                b"sldIdLst" => in_list = true,
                b"sldId" if in_list => count += 1,
                _ => {}
            },
            Event::End(e) if e.local_name().as_ref() == b"sldIdLst" => in_list = false,
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(count)
}
fn source_matches(ctx: &Context, actual: &Value, expected: &Value) -> bool {
    actual["sha256"] == expected["sha256"]
        && actual["path"]
            .as_str()
            .zip(expected["path"].as_str())
            .is_some_and(|(a, b)| {
                ctx.path(a)
                    .ok()
                    .zip(ctx.path(b).ok())
                    .is_some_and(|(a, b)| a == b)
            })
}
fn resolved_build(ctx: &Context, candidate: &Value, id: &str) -> Result<(Value, bool)> {
    let build = ctx
        .json(ctx.verify(&candidate["build_receipt"])?)
        .context("missing native build receipt")?;
    ensure!(
        build["pages"] == json!([id]),
        "build receipt does not describe this one page"
    );
    ensure!(
        array(&build, "sources")?
            .iter()
            .any(|s| source_matches(ctx, s, &candidate["source"])),
        "candidate source is not bound by its build receipt"
    );
    let resolved = ctx
        .json(ctx.verify(&build["spec"])?)
        .context("native resolved spec is stale")?;
    ensure!(
        array(&resolved, "pages")?.len() == 1 && resolved["pages"][0]["id"] == id,
        "resolved build spec describes another page"
    );
    // Derive artwork from the actual native build instead of trusting a manually
    // complete dependencies list. Other pages in an old root spec are irrelevant.
    let page = &resolved["pages"][0];
    ctx.verify(&page["background"])?;
    if !page["design"].is_null() {
        ctx.verify(&page["design"])?;
    }
    if let Some(layers) = page["artwork_layers"].as_array() {
        for layer in layers {
            ctx.verify(&layer["image"])?;
        }
    }
    let mut current = build["editable"].clone();
    let mut has_postbuild_edits = false;
    ctx.verify(&current)?;
    for binding in array(candidate, "receipts")? {
        let path = ctx.verify(binding)?;
        if let Ok(receipt) = ctx.json(path) {
            if receipt["input"].is_object()
                && receipt["output"].is_object()
                && receipt["input"]["sha256"] == current["sha256"]
            {
                ctx.verify(&receipt["input"])?;
                ctx.verify(&receipt["output"])?;
                let output = &receipt["output"];
                if current["sha256"] != output["sha256"] {
                    // A native empty batch still reserializes OOXML. It has no
                    // authoring commands to lose when regenerating the source.
                    let empty_batch = receipt["commands_sha256"] == crate::fs::digest(b"[]")
                        && receipt["results"].as_array().is_some_and(Vec::is_empty);
                    has_postbuild_edits = output["sha256"] != build["editable"]["sha256"]
                        && (has_postbuild_edits || !empty_batch);
                }
                current = output.clone();
            }
        }
    }
    ensure!(
        current["sha256"] == candidate["artifact"]["sha256"],
        "candidate is not the build artifact or a bound native post-build receipt chain"
    );
    Ok((resolved, has_postbuild_edits))
}
pub(crate) fn validate_candidate(ctx: &Context, row: &Value, candidate: &Value) -> Result<()> {
    let id = nonempty(row, "id")?;
    let maker = nonempty(row, "maker_session")?;
    ensure!(
        candidate["page_id"] == id && candidate["maker_session"] == maker,
        "candidate page/owner differs from plan"
    );
    let source = ctx
        .verify(&candidate["source"])
        .context("candidate source changed")?;
    ensure!(
        ctx.json(source)?["id"] == id,
        "candidate source names another page"
    );
    let artifact = ctx
        .verify(&candidate["artifact"])
        .context("candidate PPTX changed")?;
    ensure!(
        slide_count(ctx, &artifact)? == 1,
        "candidate PPTX must contain exactly one page"
    );
    let render = ctx
        .verify(&candidate["render"])
        .context("candidate render changed")?;
    image::image_dimensions(render).context("candidate render must be an image")?;
    bindings(ctx, &candidate, "dependencies", false)?;
    bindings(ctx, &candidate, "receipts", true)?;
    let (resolved, _) = resolved_build(ctx, &candidate, id)?;
    if let Some(approved) = row.get("approved_background") {
        ctx.verify(approved)
            .context("approved background evidence changed")?;
        ensure!(
            resolved["pages"][0]["background"]["sha256"] == approved["sha256"],
            "built page background differs from its plan-bound approved artwork"
        );
    }
    let render_receipt = ctx
        .json(ctx.verify(&candidate["render_receipt"])?)
        .context("native render receipt is stale")?;
    ctx.verify(&render_receipt["input"])?;
    ensure!(
        render_receipt["input"]["sha256"] == candidate["artifact"]["sha256"],
        "render receipt belongs to another PPTX, not the current post-build candidate"
    );
    ensure!(
        array(&render_receipt, "pages")?.len() == 1
            && source_matches(ctx, &render_receipt["pages"][0], &candidate["render"]),
        "candidate preview differs from its actual native render receipt"
    );
    Ok(())
}

pub(crate) fn maker_row<'a>(ctx: &Context, plan: &'a Value, id: &str) -> Result<&'a Value> {
    let row = array(plan, "pages")?
        .iter()
        .find(|row| row["id"] == id)
        .context("page is not in the active page plan")?;
    ensure!(
        ctx.continuation_owner.as_deref().map(identity)
            == Some(identity(nonempty(row, "maker_session")?)),
        "only the declared page owner may publish its candidate"
    );
    Ok(row)
}

/// Publish native build/render bindings, never a quality decision or a peer wake.
pub fn publish(ctx: &Context, request: &Value) -> Result<Value> {
    let p = plan(ctx)?;
    let id = nonempty(request, "page")?;
    let row = maker_row(ctx, &p, id)?;
    let _lock = crate::page_verdict::lock_page(ctx, id)?;
    let maker = nonempty(row, "maker_session")?;
    let cycle_path = ctx.path(nonempty(request, "cycle")?)?;
    let build = ctx.json(&cycle_path)?;
    ensure!(
        build["pages"] == json!([id]),
        "publication requires this one page's native cycle"
    );
    let mut source = None;
    let mut dependencies = Vec::new();
    for binding in array(&build, "sources")? {
        let path = ctx.verify(binding)?;
        let value = ctx.json(path)?;
        if value["id"] == id {
            ensure!(source.is_none(), "ambiguous page source in native receipt");
            source = Some(binding.clone());
        } else if value["pages"].is_array() {
            dependencies.push(binding.clone());
        }
    }
    let resolved = ctx.json(ctx.verify(&build["spec"])?)?;
    ensure!(
        array(&resolved, "pages")?.len() == 1 && resolved["pages"][0]["id"] == id,
        "resolved spec is not for this one page"
    );
    let page = &resolved["pages"][0];
    dependencies.push(page["background"].clone());
    if !page["design"].is_null() {
        dependencies.push(page["design"].clone());
    }
    if let Some(layers) = page["artwork_layers"].as_array() {
        for layer in layers {
            dependencies.push(layer["image"].clone());
        }
    }
    // Resolved assets contain absolute paths; publish canonical workspace-relative
    // bindings so event consumers need no host-path access or path exceptions.
    for binding in &mut dependencies {
        *binding = ctx.binding(ctx.verify(binding)?)?;
    }
    let mut receipts = vec![ctx.binding(&cycle_path)?];
    for name in ["inspection", "native_qa"] {
        if build[name]["report"].is_object() {
            receipts.push(build[name]["report"].clone());
        }
    }
    if let Some(extra) = request["receipts"].as_array() {
        for path in extra {
            receipts.push(
                ctx.binding(
                    path.as_str()
                        .context("receipts must name native receipt paths")?,
                )?,
            );
        }
    }
    let render_path = request["render_receipt"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| {
            cycle_path.parent().map(|out| {
                out.join("rendered/render.json")
                    .to_string_lossy()
                    .into_owned()
            })
        })
        .context("missing native render receipt")?;
    let render = ctx.json(&render_path)?;
    ensure!(
        array(&render, "pages")?.len() == 1,
        "publication requires a one-page render"
    );
    let artifact = match request["artifact"].as_str() {
        Some(path) => ctx.binding(path)?,
        None => build["editable"].clone(),
    };
    let candidate = json!({"page_id":id,"maker_session":maker,
        "source":source.context("native receipt does not bind a page source JSON")?,
        "build_receipt":ctx.binding(&cycle_path)?,"artifact":artifact,
        "render":render["pages"][0],"render_receipt":ctx.binding(&render_path)?,
        "dependencies":dependencies,"receipts":receipts});
    validate_candidate(ctx, row, &candidate)?;
    let destination = ctx.path(nonempty(row, "candidate")?)?;
    let unchanged = destination.exists() && ctx.json(&destination)? == candidate;
    if destination.exists() && !unchanged {
        let previous = ctx.binding(&destination)?;
        let review_path = ctx.path(nonempty(row, "review")?)?;
        if review_path.exists() {
            let review = ctx.json(review_path)?;
            ensure!(
                !(review["accepted"] == true && review["candidate_sha256"] == previous["sha256"]),
                "accepted candidate is pinned; independent reviewer must release it before replacement"
            );
        }
    }
    // The same validator is used at review/assembly admission, so a later source
    // or asset edit invalidates this candidate instead of carrying acceptance.
    if !unchanged {
        ctx.replace_json(&destination, &candidate)?;
    }
    let bound = ctx.binding(&destination)?;
    let candidate_id = crate::page_verdict::token("candidate", &bound)?;
    Ok(
        json!({"candidate":bound,"candidate_id":candidate_id,"ready_for_independent_review":true,
        "notification_sent":false,"quality_approved":false,
        "handoff":{"kind":"page_candidate_ready","page_id":id,"maker_session":maker,"candidate_id":candidate_id,
            "candidate":bound,"cycle":ctx.binding(&cycle_path)?,"artifact":candidate["artifact"],
            "render":candidate["render"],"source":candidate["source"],"review_path":row["review"]},
        "next":"Keep these inputs fixed. The host routes this bound candidate to the existing independent reviewer; publication is not acceptance."}),
    )
}

pub(crate) fn current_review(
    ctx: &Context,
    p: &Value,
    row: &Value,
    candidate: &Value,
) -> Result<Value> {
    let id = nonempty(row, "id")?;
    let review = ctx
        .json(nonempty(row, "review")?)
        .context("independent review is pending")?;
    ensure!(review["page_id"] == id, "review names another page");
    ensure!(
        review["candidate_sha256"] == candidate["sha256"],
        "review is for a different candidate; review the exact current artifact, including postbuild repairs"
    );
    crate::page_verdict::verify_scores(p, &review)?;
    let reviewer = nonempty(&review, "reviewer_session")?;
    independent(p, row, reviewer)?;
    review["score"]
        .as_f64()
        .context("review score must be numeric")?;
    ensure!(
        review["accepted"].is_boolean(),
        "review accepted must be boolean"
    );
    array(&review, "critical_defects")?;
    array(&review, "open_required_findings")?;
    bindings(ctx, &review, "evidence", true)?;
    Ok(review)
}

pub(crate) fn independent(p: &Value, row: &Value, reviewer: &str) -> Result<()> {
    if let Some(assigned) = row.get("reviewer_session") {
        ensure!(
            assigned.as_str().map(identity) == Some(identity(reviewer)),
            "different reviewer assigned to this page"
        );
    }
    ensure!(
        identity(reviewer) != identity(p["coordinator_session"].as_str().unwrap_or_default())
            && !array(p, "pages")?.iter().any(|r| r["maker_session"]
                .as_str()
                .is_some_and(|maker| identity(maker) == identity(reviewer))),
        "reviewer must be independent of coordinator and all page makers"
    );
    Ok(())
}

pub(crate) fn review_ready(p: &Value, review: &Value) -> Result<f64> {
    let score = review["score"]
        .as_f64()
        .context("review score must be numeric")?;
    let min = p["min_score"].as_f64().unwrap_or(95.0);
    ensure!(
        score.is_finite() && score >= min && score <= 100.0,
        "review score {score} is below {min} or outside 0..100"
    );
    ensure!(
        review["accepted"] == true,
        "review has not accepted this page"
    );
    ensure!(
        array(review, "critical_defects")?.is_empty(),
        "critical defects remain"
    );
    ensure!(
        array(review, "open_required_findings")?.is_empty(),
        "required findings remain"
    );
    Ok(score)
}

pub(crate) fn load_candidate(ctx: &Context, row: &Value) -> Result<(Value, Value)> {
    let path = ctx.path(nonempty(row, "candidate")?)?;
    let bytes = ctx.read(&path).context("candidate is not published")?;
    let candidate = serde_json::from_slice(&bytes).context("candidate JSON is invalid")?;
    let binding = json!({"path":path.strip_prefix(&ctx.root)?.to_string_lossy(),
        "sha256":crate::fs::digest(&bytes)});
    Ok((candidate, binding))
}

fn stable_state(ctx: &Context, binding: &Value, mut state: Value) -> Value {
    if ctx.verify(binding).is_err() {
        state["ready"] = json!(false);
        state["candidate_current"] = json!(false);
        state["review_current"] = json!(false);
        state["ready_for_review"] = json!(false);
        state["work_stage"] = json!("make");
        state["reason"] = json!(
            "candidate publication changed during assessment; assess the current pointer again"
        );
        state.as_object_mut().unwrap().remove("score");
        state.as_object_mut().unwrap().remove("reviewer_session");
        state.as_object_mut().unwrap().remove("disposition");
        state
            .as_object_mut()
            .unwrap()
            .remove("review_disposition_needed");
    }
    state
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;

    #[test]
    fn assessment_binds_parsed_pointer_bytes_and_rejects_concurrent_republication() {
        let root = tempfile::tempdir().unwrap();
        let ctx = Context::new(root.path()).unwrap();
        ctx.write_json("candidate.json", &json!({"version":"A"}))
            .unwrap();
        let (value, binding) =
            load_candidate(&ctx, &json!({"candidate":"candidate.json"})).unwrap();
        assert_eq!(value["version"], "A");
        assert_eq!(binding, ctx.binding("candidate.json").unwrap());
        ctx.replace_json("candidate.json", &json!({"version":"B"}))
            .unwrap();
        let state = stable_state(
            &ctx,
            &binding,
            json!({"ready":true,"candidate_current":true,
            "review_current":true,"ready_for_review":false,"work_stage":"complete","score":99}),
        );
        assert_eq!(state["ready"], false);
        assert_eq!(state["candidate_current"], false);
        assert_eq!(state["work_stage"], "make");
        assert!(state.get("score").is_none());
    }
}

fn page_state(ctx: &Context, p: &Value, row: &Value) -> Value {
    let mut state = json!({"id":row["id"],"maker_session":row["maker_session"],
        "candidate":null,"ready":false,"candidate_current":false,
        "review_current":false,"ready_for_review":false,"work_stage":"make"});
    let (candidate, binding) = match load_candidate(ctx, row) {
        Ok(value) => value,
        Err(error) => {
            state["reason"] = json!(format!("{error:#}"));
            return state;
        }
    };
    state["candidate_id"] = json!(crate::page_verdict::token("candidate", &binding).ok());
    state["candidate"] = binding.clone();
    if let Err(error) = validate_candidate(ctx, row, &candidate) {
        state["reason"] = json!(format!("{error:#}"));
        return state;
    }
    state["candidate_current"] = json!(true);
    let review = match current_review(ctx, p, row, &state["candidate"]) {
        Ok(review) => review,
        Err(error) => {
            state["work_stage"] = json!("review");
            state["ready_for_review"] = json!(true);
            state["reason"] = json!(format!("{error:#}"));
            return stable_state(ctx, &binding, state);
        }
    };
    state["review_current"] = json!(true);
    state["reviewer_session"] = review["reviewer_session"].clone();
    state["score"] = review["score"].clone();
    match review_ready(p, &review) {
        Ok(_) => {
            state["ready"] = json!(true);
            state["work_stage"] = json!("complete");
        }
        Err(error) => {
            let actionable = [
                "critical_defects",
                "open_required_findings",
                "open_minor_findings",
            ]
            .iter()
            .any(|key| review[*key].as_array().is_some_and(|rows| !rows.is_empty()))
                || review["findings"].as_array().is_some_and(|rows| {
                    rows.iter()
                        .any(|finding| finding["state"] == "open" || finding["status"] == "open")
                });
            state["work_stage"] = json!(if actionable { "repair" } else { "review" });
            state["review_disposition_needed"] = json!(!actionable);
            state["ready_for_review"] = json!(!actionable);
            if let Some(disposition) = crate::page_verdict::disposition(&review) {
                let phase = if disposition.get("blocked_report").is_some()
                    && crate::page_verdict::checked_blocker(ctx, p, &review).is_ok()
                {
                    "blocked"
                } else {
                    crate::page_verdict::resolution_phase(disposition, &review)
                };
                state["work_stage"] = json!(phase);
                state["review_disposition_needed"] = json!(true);
                state["ready_for_review"] = json!(phase == "review");
                state["disposition"] = disposition.clone();
            }
            state["reason"] = json!(format!("{error:#}"));
        }
    }
    stable_state(ctx, &binding, state)
}

pub fn check(ctx: &Context) -> Result<Value> {
    if !ctx.path(PLAN)?.exists() {
        return Ok(
            json!({"active":false,"plan":PLAN,"ready_for_assembly":false,
            "quality_approved":false,"agent_execution_verified":false}),
        );
    }
    let p = plan(ctx)?;
    let mut rows = Vec::new();
    let mut accepted = 0;
    for page in array(&p, "pages")? {
        let state = page_state(ctx, &p, page);
        if state["ready"] == true {
            accepted += 1;
        }
        rows.push(state);
    }
    Ok(
        json!({"active":true,"plan":ctx.binding(PLAN)?,"page_count":rows.len(),
        "accepted_pages":accepted,"ready_for_assembly":accepted==rows.len(),"pages":rows,
        "coordinator_session":p["coordinator_session"],
        "quality_approved":false,"agent_execution_verified":false,
        "limitations":"Validates recorded page ownership and bound review evidence. The host must verify real peer identities/execution; an independent model judges visuals and the final combined deck."}),
    )
}

pub fn require_ready(
    ctx: &Context,
    count: usize,
    sources: Option<(&crate::spec::Spec, &[Value])>,
) -> Result<()> {
    if count <= 1 || !ctx.path(PLAN)?.exists() {
        return Ok(());
    }
    let checked = check(ctx)?;
    let pending = checked["pages"]
        .as_array()
        .context("page gate has no coverage")?
        .iter()
        .filter(|p| p["ready"] != true)
        .map(|p| format!("{}: {}", p["id"], p["reason"].as_str().unwrap_or("pending")))
        .collect::<Vec<_>>()
        .join("; ");
    ensure!(
        checked["ready_for_assembly"] == true,
        "page-first gate ({PLAN}): {pending}"
    );
    let p = plan(ctx)?;
    let pages = array(&p, "pages")?;
    ensure!(
        count == pages.len(),
        "page-first assembly has {count} pages; plan requires {}",
        pages.len()
    );
    if let Some((spec, actual_sources)) = sources {
        let ids = spec.pages.iter().map(|p| p.id.clone()).collect::<Vec<_>>();
        let expected = pages
            .iter()
            .map(|p| p["id"].as_str().unwrap_or_default().to_owned())
            .collect::<Vec<_>>();
        ensure!(
            ids == expected,
            "page-first assembly page order/IDs differ from plan"
        );
        for (index, page) in pages.iter().enumerate() {
            let (candidate, binding) = load_candidate(ctx, page)?;
            ensure!(
                binding == checked["pages"][index]["candidate"],
                "accepted page {} changed during assembly preflight; check the current review",
                page["id"]
            );
            let approved_source = ctx.verify(&candidate["source"])?;
            ensure!(
                actual_sources
                    .iter()
                    .any(|s| s["sha256"] == candidate["source"]["sha256"]
                        && s["path"].as_str().is_some_and(|path| ctx
                            .path(path)
                            .is_ok_and(|p| p == approved_source))),
                "page-first assembly source differs from accepted page {}",
                page["id"]
            );
            let (resolved, has_postbuild_edits) =
                resolved_build(ctx, &candidate, nonempty(page, "id")?)?;
            ensure!(
                !has_postbuild_edits,
                "source-only assembly would discard post-build edits on accepted page {}; use page_assemble to retain the accepted slide artifacts in plan order",
                page["id"]
            );
            ensure!(
                serde_json::to_value(&spec.canvas)? == resolved["canvas"]
                    && serde_json::to_value(&spec.styles)? == resolved["styles"],
                "page-first assembly shared canvas/styles differ from accepted page {}",
                page["id"]
            );
            ensure!(
                serde_json::to_value(&spec.pages[index])? == resolved["pages"][0],
                "page-first assembly resolved page differs from accepted page {}",
                page["id"]
            );
        }
    }
    Ok(())
}
