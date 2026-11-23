//! Reconcile the independent reviewer's source observations before the page plan freezes.
use crate::{Context, editable_copy, fs, page_gate, review_routes, spec, string};
use anyhow::{Context as _, Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

const FIELD: &str = "source_copy_review";
const ARCHIVE: &str = ".octos/source-copy-reviews";

pub(crate) fn reference(row: &Value) -> Result<Option<&Value>> {
    let inputs = row["inputs"].as_array().into_iter().flatten();
    let prepared = row.get("source_artwork").is_some()
        || inputs
            .clone()
            .any(|v| matches!(v["role"].as_str(), Some("preparation" | "measurements")));
    // Authored copy can have a visual design supplied purely for styling.
    // Its images do not establish required wording.
    let original_and_copy = row.get("reference").is_some()
        && row.get("editable_copy").is_some()
        && !inputs.clone().any(|v| v["role"] == "canonical-copy");
    if !prepared && !original_and_copy {
        return Ok(None);
    }
    if let Some(source) = row["source_artwork"]
        .get("image")
        .or_else(|| row.get("reference"))
    {
        if !prepared {
            let path = source.as_str().or_else(|| source["path"].as_str());
            if path.is_none_or(|path| image::ImageFormat::from_path(path).is_err()) {
                return Ok(None);
            }
        }
        return Ok(Some(source));
    }
    let mut designs = inputs.filter(|v| v["role"] == "design");
    let result = designs.next();
    ensure!(
        designs.next().is_none(),
        "ambiguous source image in page assignment"
    );
    Ok(result)
}

pub(crate) fn source_binding(ctx: &Context, source: &Value) -> Result<Value> {
    let path = ctx.path(
        source
            .as_str()
            .or_else(|| source["path"].as_str())
            .context("source image needs a path")?,
    )?;
    crate::review::image_dimensions(&path).context("cannot read source reference image")?;
    let binding = ctx.binding(path)?;
    ensure!(
        source
            .get("sha256")
            .is_none_or(|hash| hash == &binding["sha256"]),
        "source image changed"
    );
    Ok(binding)
}

fn revision(plan: &Value, row: &Value, source: &Value) -> Result<String> {
    let mut row = row.clone();
    row.as_object_mut()
        .context("invalid page row")?
        .remove(FIELD);
    Ok(fs::digest(&serde_json::to_vec(&json!([
        plan["coordinator_session"],
        plan["shared_brief"],
        row,
        source
    ]))?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Region {
    location: String,
    observation: String,
    labels: Vec<String>,
}
#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum Change {
    Add {
        id: String,
        text: String,
        reason: String,
    },
    Replace {
        id: String,
        text: String,
        reason: String,
    },
    Exclude {
        id: String,
        reason: String,
    },
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Report {
    regions: Vec<Region>,
    confirmed_ids: Vec<String>,
    #[serde(default)]
    notes: Vec<String>,
    changes: Vec<Change>,
    unresolved: Vec<String>,
}

struct Reconciliation {
    additions: Vec<Value>,
    unconfirmed: Vec<String>,
}

fn reconcile(report: &Report, copy: &mut editable_copy::Copy) -> Result<Reconciliation> {
    ensure!(
        !report.regions.is_empty(),
        "record the regions inspected in the original image"
    );
    let original_ids: BTreeSet<_> = copy.keys().cloned().collect();
    let mut accounted = BTreeSet::new();
    for id in &report.confirmed_ids {
        ensure!(
            original_ids.contains(id) && accounted.insert(id),
            "unknown or duplicate confirmed source occurrence {id}"
        );
    }
    for change in &report.changes {
        let (id, reason, text, must_exist) = match change {
            Change::Add { id, text, reason } => (id, reason, Some(text), false),
            Change::Replace { id, text, reason } => (id, reason, Some(text), true),
            Change::Exclude { id, reason } => (id, reason, None, true),
        };
        ensure!(
            !id.trim().is_empty() && !reason.trim().is_empty() && accounted.insert(id),
            "duplicate confirmation/change or missing id/reason for {id}"
        );
        ensure!(
            copy.contains_key(id) == must_exist,
            "source change targets the wrong occurrence: {id}"
        );
        if let Some(text) = text {
            ensure!(!text.trim().is_empty(), "empty source text for {id}");
            copy.insert(id.clone(), text.clone());
        } else {
            copy.remove(id);
        }
    }
    ensure!(
        report.unresolved.iter().all(|u| !u.trim().is_empty()),
        "describe each unresolved source question"
    );
    ensure!(
        report.notes.iter().all(|n| !n.trim().is_empty()),
        "empty source-review note"
    );
    let unconfirmed = original_ids
        .into_iter()
        .filter(|id| !accounted.contains(id))
        .collect();
    let mut counts = BTreeMap::new();
    for text in copy.values() {
        *counts.entry(spec::normalized(text)).or_insert(0usize) += 1;
    }
    let mut additions = Vec::new();
    for region in &report.regions {
        ensure!(
            !region.location.trim().is_empty() && !region.observation.trim().is_empty(),
            "give each inspected region a location and an observation"
        );
        for (index, text) in region.labels.iter().enumerate() {
            ensure!(!text.trim().is_empty(), "empty source label");
            let n = counts.entry(spec::normalized(text)).or_default();
            if *n == 0 {
                // The reviewer has already identified a separate required occurrence.
                // Allocate its ID here so the reviewer need not repeat the same fact.
                let digest = fs::digest(&serde_json::to_vec(&(&region.location, index, text))?);
                let base = format!("source-{}", &digest[..16]);
                let mut id = base.clone();
                let mut suffix = 1;
                while copy.contains_key(&id) {
                    id = format!("{base}-{suffix}");
                    suffix += 1;
                }
                copy.insert(id.clone(), text.clone());
                additions.push(json!({"id":id,"location":region.location,"text":text}));
            } else {
                *n -= 1;
            }
        }
    }
    Ok(Reconciliation {
        additions,
        unconfirmed,
    })
}

fn receipt(ctx: &Context, binding: &Value) -> Result<Value> {
    let sha = string(binding, "sha256")?;
    ensure!(
        binding["path"] == format!("{ARCHIVE}/{sha}.json"),
        "expected a native source-copy receipt"
    );
    let bytes = ctx.read(string(binding, "path")?)?;
    ensure!(fs::digest(&bytes) == sha, "source-copy receipt changed");
    let saved: Value = serde_json::from_slice(&bytes)?;
    ensure!(
        saved["kind"] == "source-copy-review-v1",
        "invalid source-copy receipt"
    );
    Ok(saved)
}

/// Honor any source audit already recorded, including on initialization retries.
pub(crate) fn check_before_freeze(ctx: &Context, plan: &Value) -> Result<()> {
    let mut problems = Vec::new();
    for row in plan["pages"].as_array().context("missing pages")? {
        let Some(binding) = row.get(FIELD) else {
            continue;
        };
        let check = || -> Result<()> {
            let reference = reference(row)?.context("recorded source review has no original")?;
            let source = source_binding(ctx, reference)?;
            let saved = receipt(ctx, binding)?;
            ensure!(
                saved["reviewer"] == page_gate::identity(string(row, "reviewer_session")?)
                    && saved["current_revision"] == revision(plan, row, &source)?,
                "source, assignment or editable copy changed; review the current page"
            );
            ensure!(
                saved["ready_to_freeze"] == true,
                "source review incomplete: unconfirmed IDs {}, unresolved {}",
                saved["unconfirmed_ids"],
                saved["report"]["unresolved"]
            );
            Ok(())
        };
        if let Err(error) = check() {
            problems.push(format!("{}: {error:#}", string(row, "id")?));
        }
    }
    ensure!(
        problems.is_empty(),
        "invalid recorded source review:\n{}",
        problems.join("\n")
    );
    Ok(())
}

pub(crate) fn check_candidate_source(ctx: &Context, row: &Value) -> Result<()> {
    if let Some(binding) = row.get(FIELD) {
        let saved = receipt(ctx, binding)?;
        ctx.verify(&saved["source"])
            .context("independently reviewed source changed")?;
    }
    Ok(())
}

pub(crate) fn require_init(ctx: &Context, plan: &Value, row: &Value) -> Result<()> {
    if plan["schema_version"] == 2 && row.get(FIELD).is_some() {
        ensure!(
            ctx.path(review_routes::PLAN_PATH)?.is_file(),
            "freeze the recorded source review with evidence_init before publishing"
        );
    }
    Ok(())
}

pub fn review(ctx: &Context, request: &Value) -> Result<Value> {
    let reviewer = page_gate::identity(
        ctx.continuation_owner
            .as_deref()
            .context("source review requires the assigned reviewer session")?,
    );
    ensure!(!reviewer.trim().is_empty(), "empty reviewer identity");
    let report = request
        .get("report")
        .map(|_| ctx.json(string(request, "report")?))
        .transpose()?;
    // Only the short native draft update is serialized. Reviewers inspect pixels in parallel.
    let _lock = report
        .as_ref()
        .map(|_| ctx.wait_for_lock(crate::evidence::LOCK))
        .transpose()?;
    let mut plan = page_gate::plan(ctx)?;
    let id = string(request, "page")?;
    let idx = plan["pages"]
        .as_array()
        .context("missing pages")?
        .iter()
        .position(|p| p["id"] == id)
        .context("page not in draft plan")?;
    let row = &plan["pages"][idx];
    ensure!(
        page_gate::identity(string(row, "reviewer_session")?) == reviewer,
        "different reviewer assigned to page {id}"
    );
    page_gate::independent(&plan, row, reviewer)?;
    let source = source_binding(
        ctx,
        reference(row)?.context("page has no source-image copy assignment")?,
    )?;
    let mut copy = editable_copy::declared(row)?.context(
        "prepare editable_copy on the draft page first; use an empty map for a text-free source",
    )?;
    let before = revision(&plan, row, &source)?;
    let token = fs::digest(&serde_json::to_vec(&(&before, row.get(FIELD)))?);
    let Some(value) = report else {
        return Ok(
            json!({"page":id,"source":source,"expected_sha256":token,"next":"Inspect the original by region, then reconcile the draft editable_copy and submit a report. Read help topic source_copy_review for the schema."}),
        );
    };
    let expected = string(request, "expected_sha256")?;
    if let Some(binding) = row.get(FIELD) {
        let saved = receipt(ctx, binding)?;
        if saved["current_revision"] == before
            && saved["input_revision"] == expected
            && saved["report"] == value
        {
            return Ok(response(binding, &saved));
        }
    }
    ensure!(
        !ctx.path(review_routes::PLAN_PATH)?.exists(),
        "source plan already frozen"
    );
    ensure!(
        expected == token,
        "source-copy review is stale; reopen the current page"
    );
    let report: Report =
        serde_json::from_value(value.clone()).context("invalid source-copy review report")?;
    let reconciliation = reconcile(&report, &mut copy)?;
    let ready = reconciliation.unconfirmed.is_empty() && report.unresolved.is_empty();
    if ready {
        plan["pages"][idx]["editable_copy"] = json!(copy);
    }
    let saved = json!({"kind":"source-copy-review-v1","page":id,"source":source,"reviewer":reviewer,
        "input_revision":token,"current_revision":revision(&plan,&plan["pages"][idx],&source)?,
        "report":value,"automatic_additions":reconciliation.additions,"unconfirmed_ids":reconciliation.unconfirmed,"ready_to_freeze":ready});
    let bytes = serde_json::to_vec_pretty(&saved)?;
    let path = ctx.path(format!("{ARCHIVE}/{}.json", fs::digest(&bytes)))?;
    if !path.exists() {
        ctx.write_new(&path, &bytes)?;
    }
    let binding = ctx.binding(&path)?;
    ensure!(
        binding["sha256"] == fs::digest(&bytes),
        "source-copy archive differs"
    );
    plan["pages"][idx][FIELD] = binding.clone();
    ctx.replace_json(page_gate::PLAN, &plan)?;
    Ok(response(&binding, &saved))
}

fn response(binding: &Value, saved: &Value) -> Value {
    json!({"receipt":binding,"page":saved["page"],"ready_to_freeze":saved["ready_to_freeze"],
        "automatic_additions":saved["automatic_additions"],"unconfirmed_ids":saved["unconfirmed_ids"],"unresolved":saved["report"]["unresolved"],"quality_approved":false})
}
