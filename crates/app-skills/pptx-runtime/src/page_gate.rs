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
fn plan(ctx: &Context) -> Result<Value> {
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
    let pages = array(&p, "pages")?;
    ensure!(!pages.is_empty(), "page plan must cover at least one page");
    let mut ids = BTreeSet::new();
    let mut makers = BTreeSet::new();
    for page in pages {
        let id = nonempty(page, "id")?;
        let maker = nonempty(page, "maker_session")?;
        ensure!(ids.insert(id), "duplicate page id {id}");
        ensure!(
            makers.insert(maker),
            "one page per maker required; duplicate maker_session {maker}"
        );
        ensure!(maker != coordinator, "coordinator cannot be a page maker");
        ctx.path(nonempty(page, "candidate")?)?;
        ctx.path(nonempty(page, "review")?)?;
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
fn resolved_build(ctx: &Context, candidate: &Value, id: &str) -> Result<Value> {
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
                current = receipt["output"].clone();
            }
        }
    }
    ensure!(
        current["sha256"] == candidate["artifact"]["sha256"],
        "candidate is not the build artifact or a bound native post-build receipt chain"
    );
    Ok(resolved)
}
fn page_ready(ctx: &Context, p: &Value, row: &Value) -> Result<f64> {
    let id = nonempty(row, "id")?;
    let maker = nonempty(row, "maker_session")?;
    let candidate_path = ctx.path(nonempty(row, "candidate")?)?;
    let candidate = ctx
        .json(&candidate_path)
        .context("candidate is not published")?;
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
    resolved_build(ctx, &candidate, id)?;
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
    let review = ctx
        .json(nonempty(row, "review")?)
        .context("independent review is pending")?;
    ensure!(review["page_id"] == id, "review names another page");
    ensure!(
        review["candidate_sha256"] == ctx.binding(&candidate_path)?["sha256"],
        "review is for a different candidate; review the exact current artifact, including postbuild repairs"
    );
    let reviewer = nonempty(&review, "reviewer_session")?;
    ensure!(
        reviewer != p["coordinator_session"].as_str().unwrap_or_default()
            && !array(p, "pages")?
                .iter()
                .any(|r| r["maker_session"] == reviewer),
        "reviewer must be independent of coordinator and all page makers"
    );
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
        array(&review, "critical_defects")?.is_empty(),
        "critical defects remain"
    );
    ensure!(
        array(&review, "open_required_findings")?.is_empty(),
        "required findings remain"
    );
    bindings(ctx, &review, "evidence", true)?;
    Ok(score)
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
        let candidate = ctx.binding(nonempty(page, "candidate")?).ok();
        match page_ready(ctx, &p, page) {
            Ok(score) => {
                accepted += 1;
                rows.push(json!({"id":page["id"],"candidate":candidate,"ready":true,"score":score}));
            }
            Err(e) => rows.push(json!({"id":page["id"],"candidate":candidate,"ready":false,"reason":format!("{e:#}")})),
        }
    }
    Ok(
        json!({"active":true,"plan":ctx.binding(PLAN)?,"page_count":rows.len(),
        "accepted_pages":accepted,"ready_for_assembly":accepted==rows.len(),"pages":rows,
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
            let candidate = ctx.json(nonempty(page, "candidate")?)?;
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
            let resolved = resolved_build(ctx, &candidate, nonempty(page, "id")?)?;
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
