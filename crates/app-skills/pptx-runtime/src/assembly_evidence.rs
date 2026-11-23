//! Independent final review of a page-assembled deck. The coordinator can
//! collect an attested review but cannot submit it or award visual passes.
use crate::{Context, fs, page_assembly, page_gate, review, review_routes, string};
use anyhow::{Context as _, Result, ensure};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

const POLICY: &str = ".octos/continuation.json";
const REVIEW: &str = "pptx-assembly-review-v1";
const QA: &str = "pptx-assembly-qa-v1";
const RECORD_BYTES: usize = 256 * 1024;
const MANIFEST_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const DOMAINS: [&str; 4] = ["copy", "artwork", "readability", "editability"];

pub(crate) fn validate_page_checks(ctx: &Context, verdict: &Value) -> Result<()> {
    if verdict["accepted"] == true && review_routes::per_page(ctx)? {
        let invalid: Vec<_> = DOMAINS
            .into_iter()
            .filter(|domain| {
                let check = &verdict["delivery_checks"][*domain];
                check["status"] != "pass"
                    || !check["basis"]
                        .as_str()
                        .is_some_and(|basis| !basis.trim().is_empty())
            })
            .collect();
        ensure!(
            invalid.is_empty(),
            "per_page acceptance requires an independently observed pass and basis for {}. Inside verdict.delivery_checks, give each listed domain an object with status set to the string \"pass\" and basis set to your specific observation. Use review_request_template from your handoff or page_gate(only_page) for the exact nesting.",
            invalid.join(", ")
        );
    }
    Ok(())
}

pub(crate) fn independent(ctx: &Context, policy: &Value, reviewer: &str) -> Result<()> {
    ensure!(
        !page_gate::identity(reviewer).trim().is_empty(),
        "host final reviewer identity required"
    );
    let plan = page_gate::plan(ctx)?;
    ensure!(
        page_gate::identity(string(policy, "owner_agent")?)
            == page_gate::identity(string(&plan, "coordinator_session")?),
        "continuation and page coordinator differ"
    );
    page_gate::independent(&plan, &Value::Null, reviewer)?;
    let route = review_routes::load_plan(ctx)?;
    ensure!(
        route["mode"] != "per_page",
        "per_page final review belongs to the independent page reviewers before compilation"
    );
    if route["mode"] == "independent_visual" {
        ensure!(
            page_gate::identity(string(&route, "reviewer_agent")?) == page_gate::identity(reviewer),
            "only the final reviewer named in the verification plan may submit"
        );
    }
    Ok(())
}

fn check_renders(ctx: &Context, assembly: &Value, render: &Value) -> Result<()> {
    ensure!(
        render["engine_revision"] == assembly["engine_revision"]
            && render["input"] == assembly["output"],
        "final render receipt belongs to a different PPTX or renderer"
    );
    let images = render["pages"]
        .as_array()
        .context("missing final renders")?;
    let pages = assembly["pages"]
        .as_array()
        .context("missing assembly pages")?;
    ensure!(
        images.len() == pages.len() && !pages.is_empty(),
        "final render coverage differs from assembly"
    );
    let mut seen = BTreeSet::new();
    for (i, image) in images.iter().enumerate() {
        let path = ctx.verify(image)?;
        let (w, h) = image::image_dimensions(&path)?;
        ensure!(
            image["page"].as_u64() == Some(i as u64 + 1)
                && seen.insert(path)
                && image["width"].as_u64() == Some(u64::from(w))
                && image["height"].as_u64() == Some(u64::from(h)),
            "final render is repeated, out of order or has incorrect dimensions at slide {}",
            i + 1
        );
    }
    Ok(())
}

fn checks(
    observations: &Value,
    assembly: &Value,
    receipt: &Value,
    render: &Value,
) -> Result<Vec<Value>> {
    ensure!(
        observations["assembly_sha256"] == receipt["sha256"]
            && observations["render_receipt_sha256"] == render["sha256"],
        "observations must identify the assembly and final render receipt actually inspected"
    );
    let notes = observations["pages"]
        .as_object()
        .context("observations.pages must map page IDs to checks")?;
    let pages = assembly["pages"]
        .as_array()
        .context("missing assembly pages")?;
    ensure!(
        notes.len() == pages.len(),
        "independent final review must account for every page"
    );
    let mut results = Vec::new();
    for domain in DOMAINS {
        let mut failed = 0usize;
        let mut unknown = 0usize;
        for page in pages {
            let id = string(page, "page_id")?;
            let note = notes
                .get(id)
                .with_context(|| format!("missing final review of {id}"))?;
            let row = &note[domain];
            ensure!(
                !string(row, "basis")?.trim().is_empty(),
                "{id} {domain} needs the reviewer's observation"
            );
            match row["status"].as_str() {
                Some("fail") => failed += 1,
                Some("unknown") => unknown += 1,
                Some("pass") => (),
                _ => anyhow::bail!("invalid {domain} status for {id}"),
            }
        }
        results.push(json!({"id":domain,"status":if failed>0 {"fail"} else if unknown>0 {"unknown"} else {"pass"},
            "remaining":failed+unknown}));
    }
    let critical = observations["critical_defects"]
        .as_array()
        .context("critical_defects must be an array")?;
    let required = observations["open_required_findings"]
        .as_array()
        .context("open_required_findings must be an array")?;
    ensure!(
        (critical.is_empty() && required.is_empty())
            || results.iter().any(|c| c["status"] != "pass"),
        "open findings must be reflected in the page checks"
    );
    Ok(results)
}

fn archive(ctx: &Context, prefix: &str, body: &Value) -> Result<Value> {
    let bytes = serde_json::to_vec_pretty(body)?;
    let path = format!(".octos/{prefix}/{}.json", fs::digest(&bytes));
    if ctx.path(&path)?.exists() {
        ensure!(ctx.read(&path)? == bytes, "archived evidence was changed");
    } else {
        ctx.write_new(&path, &bytes)?;
    }
    ctx.binding(path)
}

fn destination(ctx: &Context, name: &str) -> Result<std::path::PathBuf> {
    let p = ctx.path(name)?;
    ensure!(
        p.extension().is_some_and(|v| v == "json") && !p.starts_with(ctx.root.join(".octos")),
        "use a workspace delivery JSON path outside internal records"
    );
    Ok(p)
}

// This head is shared across review destination aliases and assembly revisions.
fn review_head_path(ctx: &Context, verdict: &Value) -> Result<std::path::PathBuf> {
    let key = json!([
        string(verdict, "task_id")?,
        string(&verdict["assembly"], "path")?
    ]);
    ctx.path(format!(
        ".octos/pptx-assembly-review-heads/{}.json",
        fs::digest(&serde_json::to_vec(&key)?)
    ))
}

pub(crate) fn current_review(ctx: &Context, verdict: &Value, binding: &Value) -> Result<Value> {
    let head = review_head_path(ctx, verdict)?;
    ensure!(
        ctx.json(&head)?["review"] == *binding,
        "a different independent final verdict is current"
    );
    ctx.binding(head)
}

pub fn submit(ctx: &Context, r: &Value) -> Result<Value> {
    let _lock = ctx.lock_file(".octos/pptx-evidence.lock")?;
    let policy = ctx.json(POLICY)?;
    let reviewer = ctx
        .continuation_owner
        .as_deref()
        .context("host final reviewer identity required")?;
    independent(ctx, &policy, reviewer)?;
    let receipt = ctx.binding(string(r, "assembly")?)?;
    let assembly = page_assembly::delivered_receipt(ctx, &receipt)?;
    let renders = ctx.binding(string(r, "render_receipt")?)?;
    check_renders(ctx, &assembly, &ctx.json(ctx.verify(&renders)?)?)?;
    if let Some(name) = r.get("template_output") {
        ensure!(
            ["observations", "report", "output", "previous_review"]
                .iter()
                .all(|k| r.get(k).is_none()),
            "request the observations template separately from submitting a verdict"
        );
        let path = destination(
            ctx,
            name.as_str().context("template_output must be a path")?,
        )?;
        for delivery in policy["deliverables"]
            .as_array()
            .context("missing policy deliverables")?
        {
            ensure!(
                ctx.path(delivery.as_str().context("invalid deliverable")?)? != path,
                "choose a template path separate from the delivery outputs"
            );
        }
        let mut notes = BTreeMap::new();
        for page in assembly["pages"]
            .as_array()
            .context("missing assembled pages")?
        {
            let row: BTreeMap<_, _> = DOMAINS
                .iter()
                .map(|domain| (*domain, json!({"status":"unknown","basis":""})))
                .collect();
            notes.insert(string(page, "page_id")?, row);
        }
        let template = json!({"assembly_sha256":receipt["sha256"],"render_receipt_sha256":renders["sha256"],
            "pages":notes,"critical_defects":[],"open_required_findings":[]});
        if path.exists() {
            ensure!(
                ctx.json(&path)? == template,
                "preserve the existing observations and choose a new template path"
            );
        } else {
            ctx.write_json(&path, &template)?;
        }
        return Ok(
            json!({"observations_template":ctx.binding(path)?,"pages":notes.len(),"quality_approved":false,
            "next":"Enter your independent observations for all four domains per page, then submit the observations together with your report to a separate native verdict path."}),
        );
    }
    let notes = ctx.binding(string(r, "observations")?)?;
    let observed = ctx.json(ctx.verify(&notes)?)?;
    let result = checks(&observed, &assembly, &receipt, &renders)?;
    if result.iter().all(|check| check["status"] == "pass") {
        page_assembly::verified_receipt(ctx, &receipt)?;
    } else {
        // A maker can already be repairing its mutable source. A rejection
        // describes the bytes the reviewer saw. It never authorizes delivery.
        page_assembly::verify_delivered_bytes(ctx, &assembly)?;
    }
    let report = ctx.binding(string(r, "report")?)?;
    ensure!(
        !ctx.read(ctx.verify(&report)?)?.is_empty(),
        "empty independent QA report"
    );
    let output = destination(ctx, string(r, "output")?)?;
    let body = json!({"kind":REVIEW,"task_id":policy["task_id"],"reviewer_session":reviewer,
        "assembly":receipt,"render_receipt":renders,"observations":notes,"report":report,"checks":result,
        "verification_plan":ctx.binding(review_routes::PLAN_PATH)?});
    let head_path = review_head_path(ctx, &body)?;
    let head = head_path
        .exists()
        .then(|| ctx.json(&head_path))
        .transpose()?;
    let prior_binding = output.exists().then(|| ctx.binding(&output)).transpose()?;
    let mut unchanged = false;
    if let Some(old_binding) = &prior_binding {
        let mut old = ctx.json(&output)?;
        ensure!(
            old["kind"] == REVIEW
                && old["task_id"] == policy["task_id"]
                && old["reviewer_session"].as_str().map(page_gate::identity)
                    == Some(page_gate::identity(reviewer)),
            "only the same independent reviewer may replace the existing native verdict"
        );
        verify_attestation(ctx, &old)?;
        old.as_object_mut()
            .context("invalid existing final verdict")?
            .remove("attestation");
        unchanged = old == body;
        ensure!(
            unchanged || r["previous_review"] == old_binding["sha256"],
            "existing verdict differs; supply its previous_review SHA-256 before revising"
        );
    }
    if let Some(head) = &head {
        ensure!(
            head["reviewer_session"].as_str().map(page_gate::identity)
                == Some(page_gate::identity(reviewer)),
            "the current final reviewer owns the verdict"
        );
        if !unchanged || prior_binding.as_ref() != Some(&head["review"]) {
            ensure!(
                r["previous_review"] == head["review"]["sha256"],
                "another final verdict is current; supply its previous_review SHA-256"
            );
            // On retry the new verdict may already have reached its destination
            // before the head advanced. All other prior heads must still match.
            if !(unchanged && head["review"]["path"] == string(&ctx.binding(&output)?, "path")?) {
                ctx.verify(&head["review"])?;
            }
        }
    }
    if !unchanged {
        let attestation = archive(ctx, "pptx-assembly-reviews", &body)?;
        let mut verdict = body;
        verdict["attestation"] = attestation;
        if prior_binding.is_some() {
            ctx.replace_json(&output, &verdict)?;
        } else {
            ctx.write_json(&output, &verdict)?;
        }
    }
    let binding = ctx.binding(output)?;
    let next = json!({"review":binding,"reviewer_session":reviewer});
    if head.as_ref() != Some(&next) {
        ctx.replace_json(&head_path, &next)?;
    }
    Ok(json!({"review":binding,"checks":result,"reviewer_session":reviewer}))
}

pub(crate) fn verify_attestation(ctx: &Context, verdict: &Value) -> Result<()> {
    let mut body = verdict.clone();
    let attestation = body
        .as_object_mut()
        .context("missing final verdict")?
        .remove("attestation")
        .context("missing native final review attestation")?;
    let path = format!(
        ".octos/pptx-assembly-reviews/{}.json",
        fs::digest(&serde_json::to_vec_pretty(&body)?)
    );
    ensure!(
        attestation == ctx.binding(&path)? && ctx.json(&path)? == body,
        "independent final verdict differs from its native submission"
    );
    Ok(())
}

/// A later whole-deck revision replaces its verdict at the delivery path.
/// A page reviewed in response must retain the independent judgment it used.
pub(crate) fn stable_page_evidence(ctx: &Context, binding: Value) -> Result<Value> {
    let path = ctx.path(string(&binding, "path")?)?;
    if !path.extension().is_some_and(|ext| ext == "json")
        || path.starts_with(ctx.root.join(".octos"))
    {
        return Ok(binding);
    }
    let bytes = ctx.read(&path)?;
    if let Ok(verdict) = serde_json::from_slice::<Value>(&bytes)
        && verdict["kind"] == REVIEW
    {
        ensure!(
            fs::digest(&bytes) == binding["sha256"],
            "final verdict changed during page submission"
        );
        verify_attestation(ctx, &verdict)?;
        return Ok(verdict["attestation"].clone());
    }
    Ok(binding)
}

/// Keep declared deliveries directly visible to existing clients. Complex page
/// review histories can exceed the observer's single JSON record limit.
fn shard_dependencies(
    ctx: &Context,
    snapshot: &mut Value,
    deliveries: &BTreeSet<String>,
) -> Result<()> {
    if serde_json::to_vec_pretty(snapshot)?.len() <= RECORD_BYTES {
        return Ok(());
    }
    let all = snapshot["artifacts"].take();
    let mut direct = Vec::new();
    let mut shards = Vec::new();
    let mut rows = Vec::new();
    let mut size = 0usize;
    let mut total = 0usize;
    for binding in all.as_array().context("missing assembly bindings")? {
        if deliveries.contains(string(binding, "path")?) {
            direct.push(binding.clone());
            continue;
        }
        // Account for array indentation and separators as well as row bytes.
        let bytes = serde_json::to_vec_pretty(binding)?.len() + 16;
        ensure!(
            bytes <= RECORD_BYTES,
            "individual artifact binding exceeds observer limit"
        );
        if size + bytes > RECORD_BYTES / 2 && !rows.is_empty() {
            shards.push(archive(
                ctx,
                "continuation-artifact-manifests",
                &json!(rows),
            )?);
            rows.clear();
            size = 0;
        }
        size += bytes;
        total += bytes;
        ensure!(
            total <= MANIFEST_BYTES,
            "assembled dependency inventory exceeds 16 MiB"
        );
        rows.push(binding.clone());
    }
    if !rows.is_empty() {
        shards.push(archive(
            ctx,
            "continuation-artifact-manifests",
            &json!(rows),
        )?);
    }
    snapshot["artifacts"] = json!(direct);
    snapshot["dependency_manifests"] = json!(shards);
    ensure!(
        serde_json::to_vec_pretty(snapshot)?.len() <= RECORD_BYTES,
        "declared delivery bindings exceed the observer record limit"
    );
    Ok(())
}

struct DeliveryReview {
    assembly: Value,
    receipt: Value,
    summary: Value,
    checks: Vec<Value>,
    bindings: Vec<Value>,
}

fn deck_review(ctx: &Context, policy: &Value, r: &Value) -> Result<DeliveryReview> {
    let verdict_binding = ctx.binding(string(r, "review")?)?;
    let verdict = ctx.json(ctx.verify(&verdict_binding)?)?;
    ensure!(
        verdict["kind"] == REVIEW
            && verdict["task_id"] == policy["task_id"]
            && verdict["verification_plan"] == ctx.binding(review_routes::PLAN_PATH)?,
        "final review belongs to a different task or verification plan"
    );
    independent(ctx, policy, string(&verdict, "reviewer_session")?)?;
    verify_attestation(ctx, &verdict)?;
    let head = current_review(ctx, &verdict, &verdict_binding)?;
    let assembly = page_assembly::verified_receipt(ctx, &verdict["assembly"])?;
    let render = ctx.json(ctx.verify(&verdict["render_receipt"])?)?;
    check_renders(ctx, &assembly, &render)?;
    let observations = ctx.json(ctx.verify(&verdict["observations"])?)?;
    let results = checks(
        &observations,
        &assembly,
        &verdict["assembly"],
        &verdict["render_receipt"],
    )?;
    ensure!(
        verdict["checks"] == json!(results),
        "reviewer checks no longer match the submitted observations"
    );
    let mut bindings = [
        &verdict_binding,
        &head,
        &verdict["attestation"],
        &verdict["report"],
        &verdict["observations"],
        &verdict["render_receipt"],
        &verdict["verification_plan"],
    ]
    .into_iter()
    .cloned()
    .collect::<Vec<_>>();
    for row in render["pages"].as_array().context("missing renders")? {
        bindings.push(row.clone());
    }
    Ok(DeliveryReview {
        receipt: verdict["assembly"].clone(),
        assembly,
        summary: verdict_binding,
        checks: results,
        bindings,
    })
}

fn page_reviews(ctx: &Context, r: &Value) -> Result<DeliveryReview> {
    ensure!(
        r.get("review").is_none(),
        "per_page delivery takes an assembly receipt and qa path"
    );
    let receipt = ctx.binding(string(r, "assembly")?)?;
    let assembly = page_assembly::verified_receipt(ctx, &receipt)?;
    let mut pages = Vec::new();
    let mut bindings = vec![ctx.binding(review_routes::PLAN_PATH)?];
    for row in assembly["pages"]
        .as_array()
        .context("missing assembled pages")?
    {
        let signed = &row["review"];
        let verdict = ctx.json(ctx.verify(signed)?)?;
        validate_page_checks(ctx, &verdict)?;
        let archived = ctx.binding(format!(
            ".octos/page-verdicts/{}.json",
            string(signed, "sha256")?
        ))?;
        ensure!(
            archived["sha256"] == signed["sha256"],
            "page review archive changed"
        );
        bindings.push(archived);
        pages.push(json!({"page_id":row["page_id"],"reviewer_session":verdict["reviewer_session"],
            "score":verdict["score"],"review":signed,"render":row["render"],"delivery_checks":verdict["delivery_checks"]}));
    }
    let checks = DOMAINS
        .iter()
        .map(|id| json!({"id":id,"status":"pass","remaining":0}))
        .collect();
    Ok(DeliveryReview {
        assembly,
        receipt,
        summary: json!({"mode":"per_page","pages":pages}),
        checks,
        bindings,
    })
}

pub fn record(ctx: &Context, r: &Value) -> Result<Value> {
    let _lock = ctx.lock_file(".octos/pptx-evidence.lock")?;
    let policy = ctx.json(POLICY)?;
    let owner = ctx
        .continuation_owner
        .as_deref()
        .context("host coordinator identity required")?;
    ensure!(
        page_gate::identity(owner) == page_gate::identity(string(&policy, "owner_agent")?),
        "only the continuation coordinator may record assembly evidence"
    );
    let review = if review_routes::per_page(ctx)? {
        page_reviews(ctx, r)?
    } else {
        deck_review(ctx, &policy, r)?
    };
    let assembly = review.assembly;
    let results = review.checks;
    let preflight = review::preflight(ctx, &ctx.verify(&assembly["output"])?)?;
    let issues = preflight["issue_count"]
        .as_u64()
        .context("package preflight missing")?;
    let mut checks = vec![
        json!({"id":"package","status":if issues==0 {"pass"} else {"fail"},"remaining":issues}),
    ];
    for mut row in results.iter().filter(|r| r["id"] != "editability").cloned() {
        if row["id"] == "copy" {
            // Native editability is a required part of copy verification on this route.
            let edit = &results[3];
            let statuses = [row["status"].as_str(), edit["status"].as_str()];
            row["status"] = json!(if statuses.contains(&Some("fail")) {
                "fail"
            } else if statuses.contains(&Some("unknown")) {
                "unknown"
            } else {
                "pass"
            });
            row["remaining"] = json!(
                row["remaining"].as_u64().unwrap_or(0) + edit["remaining"].as_u64().unwrap_or(0)
            );
        }
        checks.push(row);
    }
    let required = policy["required_checks"]
        .as_array()
        .context("missing required checks")?;
    ensure!(
        !required.is_empty(),
        "the policy declares no required checks"
    );
    for requirement in required {
        let id = string(requirement, "id")?;
        ensure!(
            checks.iter().any(|c| c["id"] == id),
            "assembled evidence has no verifier for required policy check {id}"
        );
    }
    let mut qa = json!({"kind":QA,"task_id":policy["task_id"],"package_preflight":preflight,"review":review.summary,
        "output":assembly["output"],"page_count":assembly["pages"].as_array().unwrap().len(),"checks":checks});
    if qa["review"]["mode"] == "per_page" {
        qa["assembly"] = review.receipt.clone();
    }
    let qa_path = destination(ctx, string(r, "qa")?)?;
    let mut artifacts = BTreeMap::new();
    let mut bind = |value: &Value| -> Result<()> {
        let path = ctx.path(string(value, "path")?)?;
        let relative = path
            .strip_prefix(&ctx.root)?
            .to_str()
            .context("non-UTF8 artifact path")?
            .to_owned();
        if let Some(prior) = artifacts.get(&relative) {
            let prior: &Value = prior;
            ensure!(
                prior["sha256"] == value["sha256"],
                "conflicting bindings for {relative}"
            );
            return Ok(());
        }
        ctx.verify(value)?;
        artifacts.insert(
            relative.clone(),
            json!({"path":relative,"sha256":value["sha256"]}),
        );
        Ok(())
    };
    for binding in [&assembly["output"], &assembly["plan"], &review.receipt]
        .into_iter()
        .chain(&review.bindings)
    {
        bind(binding)?;
    }
    let mut scope = BTreeSet::new();
    for page in assembly["pages"]
        .as_array()
        .context("missing assembly pages")?
    {
        scope.insert(string(page, "page_id")?.to_owned());
        for key in ["candidate", "artifact", "render", "source", "review"] {
            bind(&page[key])?;
        }
        let candidate = ctx.json(ctx.path(string(&page["candidate"], "path")?)?)?;
        let accepted = ctx.json(ctx.path(string(&page["review"], "path")?)?)?;
        for key in ["build_receipt", "render_receipt"] {
            bind(&candidate[key])?;
        }
        // A legacy candidate may omit its source artwork from dependencies.
        // The native build independently binds the resolved spec and assets.
        let build = ctx.json(string(&candidate["build_receipt"], "path")?)?;
        bind(&build["spec"])?;
        let resolved = ctx.json(string(&build["spec"], "path")?)?;
        let spec_page = &resolved["pages"][0];
        bind(&spec_page["background"])?;
        if !spec_page["design"].is_null() {
            bind(&spec_page["design"])?;
        }
        for layer in spec_page["artwork_layers"].as_array().into_iter().flatten() {
            bind(&layer["image"])?;
        }
        for v in [
            &candidate["dependencies"],
            &candidate["receipts"],
            &accepted["evidence"],
        ] {
            for b in v
                .as_array()
                .context("missing candidate/review evidence bindings")?
            {
                bind(b)?;
            }
        }
    }
    let qa_relative = qa_path
        .strip_prefix(&ctx.root)?
        .to_str()
        .context("non-UTF8 QA path")?;
    ensure!(
        !artifacts.contains_key(qa_relative),
        "QA path collides with existing review evidence"
    );
    let mut deliveries = BTreeSet::new();
    for value in policy["deliverables"]
        .as_array()
        .context("missing policy deliverables")?
    {
        let p = ctx.path(value.as_str().context("invalid deliverable path")?)?;
        let relative = p
            .strip_prefix(&ctx.root)?
            .to_str()
            .context("invalid path")?;
        deliveries.insert(relative.to_owned());
        ensure!(
            p == qa_path || artifacts.contains_key(relative),
            "policy deliverable has no current assembly or independent review binding: {}",
            p.display()
        );
    }
    let ep = string(&policy, "evidence_file")?;
    let previous = ctx.json(ep)?;
    ensure!(
        previous["task_id"] == policy["task_id"],
        "evidence belongs to another task"
    );
    let old = previous["attempt"]
        .as_u64()
        .context("invalid continuation attempt")?;
    let before: BTreeSet<String> =
        serde_json::from_value(previous.get("scope").cloned().unwrap_or_else(|| json!([])))?;
    ensure!(
        before.is_empty() || before == scope,
        "assembled scope differs from previously checked pages"
    );
    // A different evidence route must not silently waive open cycle findings.
    if ctx.path(".octos/pptx-evidence-history.json")?.exists() {
        let book = ctx.json(".octos/pptx-evidence-history.json")?;
        ensure!(
            book["task_id"] == policy["task_id"]
                && book["findings"].as_object().is_some_and(|rows| rows
                    .values()
                    .all(|f| !matches!(f["state"].as_str(), Some("open" | "unknown")))),
            "resolve open cycle findings before recording assembled delivery"
        );
    }
    let qa_bytes = serde_json::to_vec_pretty(&qa)?;
    artifacts.insert(
        qa_relative.to_owned(),
        json!({"path":qa_relative,"sha256":fs::digest(&qa_bytes)}),
    );
    let mut snapshot = json!({"schema_version":1,"task_id":policy["task_id"],"phase":"checked","attempt":old,
        "checks":checks,"scope":scope,"artifacts":artifacts.into_values().collect::<Vec<_>>()});
    shard_dependencies(ctx, &mut snapshot, &deliveries)?;
    if snapshot != previous {
        snapshot["attempt"] = json!(old.checked_add(1).context("attempt overflow")?);
    }
    ensure!(
        serde_json::to_vec_pretty(&snapshot)?.len() <= 256 * 1024,
        "assembled evidence exceeds the observer's 256 KiB limit"
    );
    if qa_path.exists() {
        let prior = ctx.json(&qa_path)?;
        if prior != qa {
            ensure!(
                prior["kind"] == QA && prior["task_id"] == policy["task_id"],
                "QA path contains existing unrelated work"
            );
            archive(ctx, "pptx-assembly-qa-history", &prior)?;
            ctx.replace_json(&qa_path, &qa)?;
        }
    } else {
        ctx.write_new(&qa_path, &qa_bytes)?;
    }
    if previous != snapshot {
        if old > 0 {
            archive(ctx, "pptx-evidence-records", &previous)?;
        }
        ctx.replace_json(ep, &snapshot)?;
    }
    Ok(
        json!({"evidence":ctx.binding(ep)?,"qa":ctx.binding(qa_path)?,"checks":checks,"attempt":snapshot["attempt"],
        "all_checks_pass":checks.iter().all(|c| c["status"]=="pass"),"runtime_consumed_current_evidence":false}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn large_34_page_inventories_keep_every_hash_and_deliverable() {
        let workspace = tempfile::tempdir().unwrap();
        let ctx = Context::new(workspace.path()).unwrap();
        let mut paths: BTreeMap<String, Value> = BTreeMap::new();
        for page in 1..=34 {
            for check in 0..64 {
                let path = format!(
                    "pages/slide-{page:03}/independent-review/rendered/region-{check:03}/reference-and-editable-comparison.png"
                );
                paths.insert(
                    path.clone(),
                    json!({"path":path,"sha256":fs::digest(path.as_bytes())}),
                );
            }
        }
        let delivery = "final/editable.pptx".to_string();
        paths.insert(
            delivery.clone(),
            json!({"path":delivery,"sha256":fs::digest(b"test")}),
        );
        let mut snapshot =
            json!({"schema_version":1,"artifacts":paths.values().collect::<Vec<_>>()});
        assert!(serde_json::to_vec_pretty(&snapshot).unwrap().len() > RECORD_BYTES);
        shard_dependencies(&ctx, &mut snapshot, &BTreeSet::from([delivery.clone()])).unwrap();
        assert_eq!(
            snapshot["artifacts"],
            json!([paths.remove(&delivery).unwrap()])
        );
        assert!(serde_json::to_vec_pretty(&snapshot).unwrap().len() <= RECORD_BYTES);
        for shard in snapshot["dependency_manifests"].as_array().unwrap() {
            let data = ctx.read(ctx.verify(shard).unwrap()).unwrap();
            assert!(data.len() <= RECORD_BYTES);
            for binding in serde_json::from_slice::<Vec<Value>>(&data).unwrap() {
                assert_eq!(
                    paths.remove(binding["path"].as_str().unwrap()),
                    Some(binding)
                );
            }
        }
        assert!(
            paths.is_empty(),
            "do not truncate large independent evidence inventories"
        );
    }
}
