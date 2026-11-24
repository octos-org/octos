//! Construct the page gate's envelope from a reviewer decision and a pinned publication.
use crate::{Context, fs, page_gate};
use anyhow::{Context as _, Result, ensure};
use serde_json::{Value, json};

pub(crate) const KIND: &str = "page-review-v1";

pub(crate) fn token(kind: &str, binding: &Value) -> Result<String> {
    let hash = binding["sha256"]
        .as_str()
        .context("missing publication digest")?;
    ensure!(
        hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid publication digest"
    );
    // Preserve every bit while keeping opaque revision IDs distinct from credential-shaped strings.
    let parts: Vec<_> = (0..8).map(|i| &hash[i * 8..i * 8 + 8]).collect();
    Ok(format!("{kind}-{}", parts.join("-")))
}

pub(crate) fn lock_page(ctx: &Context, id: &str) -> Result<fs::WorkspaceLock> {
    ctx.lock_file(&format!(
        ".octos/page-locks/{}.lock",
        fs::digest(id.as_bytes())
    ))
}

/// An unanswered request pinned to the available artifact. Only the reviewer
/// supplies observations, scores and a decision.
pub(crate) fn submission_template(
    plan: &Value,
    page: &str,
    candidate_id: &str,
    prior: Option<&str>,
) -> Result<Value> {
    let mut checks = json!({});
    for domain in crate::assembly_evidence::DOMAINS {
        checks[domain] = json!({"status":null,"basis":""});
    }
    let mut scores = json!({});
    for category in plan["rubric"]
        .as_object()
        .context("per_page review requires a frozen rubric")?
        .keys()
    {
        scores[category] = Value::Null;
    }
    let mut request = json!({"action":"page_review_submit","page":page,"candidate_id":candidate_id,
        "verdict":{"accepted":null,"score":null,"component_scores":scores,"critical_defects":[],
        "open_required_findings":[],"evidence":[],"delivery_checks":checks}});
    if let Some(prior) = prior {
        request["replaces_verdict_id"] = json!(prior);
    }
    Ok(request)
}

pub(crate) fn check_rubric(rubric: &Value) -> Result<()> {
    let maxima = rubric
        .as_object()
        .context("rubric must map component names to maximum points")?;
    let mut total = 0.0;
    for (name, max) in maxima {
        let weight = max.as_f64().context("rubric weights must be numeric")?;
        ensure!(
            !name.trim().is_empty() && weight > 0.0 && weight <= 100.0,
            "invalid rubric component {name}"
        );
        total += weight;
    }
    ensure!(
        (total - 100.0).abs() < 0.000001,
        "rubric weights must sum to 100"
    );
    Ok(())
}

pub(crate) fn verify_scores(plan: &Value, verdict: &Value) -> Result<()> {
    let maxima = if let Some(rubric) = plan.get("rubric") {
        ensure!(
            &verdict["rubric"] == rubric,
            "verdict must bind the task rubric"
        );
        ensure!(
            verdict.get("component_maxima").is_none_or(|v| v == rubric),
            "component_maxima disagrees with the task rubric"
        );
        Some(rubric)
    } else {
        ensure!(
            verdict["kind"] != KIND || verdict.get("rubric").is_none(),
            "published rubric no longer matches plan"
        );
        // Existing v1 reviews may have just a score or an older, free-form
        // evidence report. Check the numeric ledger when a native verdict
        // actually supplies one, without inventing historical category caps.
        if verdict["kind"] != KIND
            || (verdict.get("component_scores").is_none()
                && verdict.get("component_maxima").is_none())
        {
            return Ok(());
        }
        let caps = verdict.get("component_maxima");
        if let Some(caps) = caps {
            check_rubric(caps)?;
        }
        caps
    };
    let scores = verdict["component_scores"]
        .as_object()
        .context("component_scores must use the task rubric")?;
    ensure!(!scores.is_empty(), "component_scores must include a score");
    let caps = maxima.and_then(Value::as_object);
    if let Some(caps) = caps {
        ensure!(
            scores.len() == caps.len(),
            "component_scores must contain exactly the task rubric categories"
        );
    }
    let mut total = 0.0;
    for (key, value) in scores {
        let n = value
            .as_f64()
            .with_context(|| format!("missing numeric rubric component {key}"))?;
        let max = match caps {
            Some(caps) => caps
                .get(key)
                .and_then(Value::as_f64)
                .with_context(|| format!("unexpected rubric component {key}"))?,
            None => 100.0,
        };
        ensure!(
            (0.0..=max).contains(&n),
            "{key} score exceeds its rubric range"
        );
        total += n;
    }
    ensure!(
        verdict["score"]
            .as_f64()
            .is_some_and(|n| (n - total).abs() < 0.000001),
        "score must equal sum of component_scores"
    );
    Ok(())
}

fn disposition_valid(disposition: &Value) -> bool {
    matches!(
        disposition["owner"].as_str(),
        Some("reviewer" | "tool_maintainer" | "arbiter" | "task_owner")
    ) && ["reason", "resume_when"].iter().all(|k| {
        disposition[k]
            .as_str()
            .is_some_and(|s| !s.trim().is_empty())
    })
}

pub(crate) fn disposition(review: &Value) -> Option<&Value> {
    (review["kind"] == KIND
        && review["accepted"] == false
        && disposition_valid(&review["disposition"]))
    .then_some(&review["disposition"])
}

/// Legacy task_owner verdicts remain immutable. Derive peer work from the bound
/// findings without changing the grade or treating an old waiver request as authority.
pub(crate) fn resolution_phase(disposition: &Value, verdict: &Value) -> &'static str {
    match disposition["owner"].as_str() {
        Some("reviewer") => "review",
        Some("tool_maintainer") => "diagnose",
        Some("task_owner")
            if disposition["secondary_owner"] == "tool_maintainer"
                || ["critical_defects", "open_required_findings"]
                    .iter()
                    .any(|key| {
                        verdict[key]
                            .as_array()
                            .is_some_and(|items| !items.is_empty())
                    }) =>
        {
            "diagnose"
        }
        _ => "arbitrate",
    }
}

// A reviewer can redirect a live referral while a specialist is still working.
// Its result can close the earlier referral only for the same unresolved items.
fn matches_investigated_referral(ctx: &Context, current: &Value, revised: &Value) -> Result<bool> {
    use std::collections::BTreeSet;
    fn ids(v: &Value, key: &str) -> Option<BTreeSet<String>> {
        v[key]
            .as_array()?
            .iter()
            .map(|f| f.as_str().or_else(|| f["id"].as_str()).map(str::to_owned))
            .collect()
    }
    let phase = resolution_phase(&revised["disposition"], revised);
    let mut head = current.clone();
    let mut seen = BTreeSet::new();
    loop {
        let Some(owner) = disposition(&head) else {
            return Ok(false);
        };
        if head["candidate_sha256"] != revised["candidate_sha256"]
            || head["page_id"] != revised["page_id"]
            || head["reviewer_session"].as_str().map(page_gate::identity)
                != revised["reviewer_session"]
                    .as_str()
                    .map(page_gate::identity)
        {
            return Ok(false);
        }
        if resolution_phase(owner, &head) == phase {
            return Ok(true);
        }
        let Some(previous_id) = head["replaces_verdict_id"].as_str() else {
            return Ok(false);
        };
        let hash = previous_id
            .strip_prefix("verdict-")
            .unwrap_or("")
            .replace('-', "");
        ensure!(
            previous_id == token("verdict", &json!({"sha256":hash}))? && seen.insert(hash.clone()),
            "invalid immutable referral history"
        );
        let bytes = ctx.read(format!(".octos/page-verdicts/{hash}.json"))?;
        ensure!(fs::digest(&bytes) == hash, "archived referral changed");
        head = serde_json::from_slice(&bytes)?;
        for key in ["critical_defects", "open_required_findings"] {
            let Some(current_items) = ids(revised, key) else {
                return Ok(false);
            };
            if ids(&head, key).as_ref() != Some(&current_items)
                || ids(current, key).as_ref() != Some(&current_items)
            {
                return Ok(false);
            }
        }
        if revised["critical_defects"] == json!([])
            && revised["open_required_findings"] == json!([])
        {
            return Ok(false);
        }
    }
}

// The bound reviewer records the specialist's published report. The host owns
// peer identity/dispatch attestation; this validates the report's file and scope.
pub(crate) fn checked_blocker(ctx: &Context, plan: &Value, verdict: &Value) -> Result<()> {
    let disp = disposition(verdict).context("a blocker requires a rejected native verdict")?;
    let phase = resolution_phase(disp, verdict);
    ensure!(
        matches!(phase, "diagnose" | "arbitrate"),
        "a blocker requires independent peer investigation"
    );
    let path = crate::string(disp, "blocked_report")?;
    let evidence = verdict["evidence"]
        .as_array()
        .context("missing bound evidence")?;
    let bound = evidence
        .iter()
        .find(|b| b["path"] == path)
        .context("blocked_report must be bound in verdict evidence")?;
    let report = ctx.json(ctx.verify(bound)?)?;
    let page = report.get("page").or_else(|| report.get("page_id"));
    ensure!(
        report["outcome"] == "blocked"
            && report["phase"] == phase
            && report["candidate_sha256"] == verdict["candidate_sha256"]
            && page == Some(&verdict["page_id"])
            && report.get("page_id").is_none_or(|id| Some(id) == page),
        "blocker must describe the investigated phase and exact page candidate"
    );
    let peer = page_gate::identity(crate::string(&report, "peer")?);
    ensure!(
        !peer.trim().is_empty()
            && peer != page_gate::identity(crate::string(verdict, "reviewer_session")?)
            && peer != page_gate::identity(crate::string(plan, "coordinator_session")?)
            && !plan["pages"]
                .as_array()
                .context("missing pages")?
                .iter()
                .any(|p| p["maker_session"].as_str().map(page_gate::identity) == Some(peer)),
        "blocker investigation must come from a separate peer"
    );
    if phase == "arbitrate" {
        let verification = crate::review_routes::load_plan(ctx)?;
        if let Some(arbiter) = verification["arbiter_agent"].as_str() {
            ensure!(
                peer == page_gate::identity(arbiter),
                "blocker report names a different arbiter"
            );
        }
    }
    for key in ["attempted_remedy", "remaining_blocker"] {
        let note = crate::string(&report, key)?.trim();
        ensure!(
            !note.is_empty()
                && !note
                    .split([':', '—', '–'])
                    .next()
                    .unwrap_or("")
                    .trim()
                    .eq_ignore_ascii_case("pending"),
            "unfinished {key} in blocker report; await the current specialist's final publication"
        );
    }
    if let Some(evidence) = report.get("evidence") {
        ensure!(
            evidence.as_array().is_some_and(|rows| !rows.is_empty()),
            "blocker report has an empty or malformed evidence inventory"
        );
    }
    Ok(())
}

fn validate(ctx: &Context, plan: &Value, verdict: &mut Value) -> Result<()> {
    let mut errors = Vec::new();
    if let Err(error) = crate::assembly_evidence::validate_page_checks(ctx, verdict) {
        errors.push(error.to_string());
    }
    let score = verdict["score"].as_f64();
    if !score.is_some_and(|n| (0.0..=100.0).contains(&n)) {
        errors.push("score must be between 0 and 100".to_owned());
    }
    if !verdict["accepted"].is_boolean() {
        errors.push("accepted must be boolean".to_owned());
    }
    for key in ["critical_defects", "open_required_findings"] {
        if !verdict[key].is_array() {
            errors.push(format!("{key} must be an array"));
        }
    }
    if verdict["accepted"] == true
        && (score < Some(plan["min_score"].as_f64().unwrap_or(95.0))
            || ["critical_defects", "open_required_findings"]
                .iter()
                .any(|k| verdict[k].as_array().is_some_and(|a| !a.is_empty())))
    {
        errors.push(
            "accepted verdict conflicts with score threshold or unresolved findings".to_owned(),
        );
    }
    let rejected_without_finding = verdict["accepted"] == false
        && ["critical_defects", "open_required_findings"]
            .iter()
            .all(|k| verdict[k].as_array().is_some_and(|v| v.is_empty()));
    if (rejected_without_finding || verdict.get("disposition").is_some())
        && disposition(verdict).is_none()
    {
        errors.push("a disposition requires a rejected verdict, peer role (reviewer, arbiter or tool_maintainer), reason and resume_when; otherwise list actionable findings".to_owned());
    }
    if let Err(err) = verify_scores(plan, verdict) {
        errors.push(err.to_string());
    }
    match verdict["evidence"].as_array() {
        Some(rows) if !rows.is_empty() => {
            let mut bound = Vec::new();
            for (i, item) in rows.iter().enumerate() {
                let result = match item.as_str() {
                    Some(path) => ctx.binding(path),
                    None => ctx.verify(item).and_then(|p| ctx.binding(p)),
                }
                .and_then(|binding| crate::assembly_evidence::stable_page_evidence(ctx, binding));
                match result {
                    Ok(b) => bound.push(b),
                    Err(err) => errors.push(format!("evidence[{i}]: {err:#}")),
                }
            }
            verdict["evidence"] = json!(bound);
        }
        _ => errors
            .push("evidence must contain the reviewed evidence files as a flat array".to_owned()),
    }
    ensure!(errors.is_empty(), "{}", errors.join("\n"));
    Ok(())
}

pub fn submit(ctx: &Context, request: &Value) -> Result<Value> {
    let reviewer = ctx
        .continuation_owner
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .context("page review submission requires the host reviewer identity")?;
    let plan = page_gate::plan(ctx)?;
    let id = crate::string(request, "page")?;
    let row = plan["pages"]
        .as_array()
        .context("missing pages")?
        .iter()
        .find(|r| r["id"] == id)
        .context("page outside active plan")?;
    page_gate::independent(&plan, row, reviewer)?;
    let _lock = lock_page(ctx, id)?;
    let (candidate, binding) = page_gate::load_candidate(ctx, row)?;
    ensure!(
        request["candidate_id"] == token("candidate", &binding)?,
        "candidate changed or missing candidate_id; use the publication actually reviewed"
    );
    // A tracked v2 review can be withdrawn against its exact delivered pixels
    // even after the maker has edited the next draft. Positive reviews and
    // first-time submissions continue to validate every live source.
    let prior_native_review = plan["schema_version"] == 2
        && request["verdict"]["accepted"] == false
        && page_gate::read_review_head(ctx, &plan, row).is_ok_and(|(prior, _)| {
            prior["candidate_sha256"] == binding["sha256"]
                && prior["reviewer_session"].as_str().map(page_gate::identity)
                    == Some(page_gate::identity(reviewer))
        });
    if prior_native_review {
        page_gate::validate_reviewed_package(ctx, row, &candidate)?;
    } else {
        page_gate::validate_candidate(ctx, row, &candidate)?;
    }

    let reserved = [
        "source_line_coverage",
        "repair_gate",
        "native_text_baseline",
        "kind",
        "page_id",
        "reviewer_session",
        "candidate_sha256",
        "candidate",
        "source",
        "artifact",
        "render",
        "replaces_verdict_id",
        "rubric",
    ];
    let dest = ctx.path(crate::string(row, "review")?)?;
    let old = dest.exists().then(|| ctx.read(&dest)).transpose()?;
    let mut replaces = request.get("replaces_verdict_id").cloned();
    let mut verdict = if let Some(report) = request.get("blocked_report") {
        ensure!(
            request.get("verdict").is_none(),
            "blocked_report preserves the existing verdict; omit verdict"
        );
        let path = report
            .as_str()
            .context("blocked_report must be a report path")?;
        let bytes = old
            .as_deref()
            .context("no rejected native verdict to bind")?;
        let mut v: Value = serde_json::from_slice(bytes)?;
        let prior =
            disposition(&v).context("bind a report only to a rejected native peer referral")?;
        let bound = ctx.binding(path)?;
        let report_value = ctx.json(ctx.verify(&bound)?)?;
        let phase = crate::string(&report_value, "phase")?;
        ensure!(
            matches!(phase, "arbitrate" | "diagnose")
                && v["page_id"] == id
                && v["candidate"] == binding
                && v["reviewer_session"].as_str().map(page_gate::identity)
                    == Some(page_gate::identity(reviewer)),
            "the original reviewer must bind the report to the exact referred candidate"
        );
        let head = token("verdict", &json!({"sha256":fs::digest(bytes)}))?;
        let same = prior["blocked_report"] == bound["path"]
            && v["evidence"].as_array().is_some_and(|a| a.contains(&bound));
        ensure!(
            replaces
                .as_ref()
                .is_some_and(|r| r == &head || (same && r == &v["replaces_verdict_id"])),
            "stale referral; use the current verdict_id"
        );
        if same {
            replaces = v.get("replaces_verdict_id").cloned();
        }
        v["disposition"]["owner"] = json!(if phase == "arbitrate" {
            "arbiter"
        } else {
            "tool_maintainer"
        });
        v["disposition"]["blocked_report"] = bound["path"].clone();
        let evidence = v["evidence"]
            .as_array_mut()
            .context("missing review evidence")?;
        if !same && !evidence.contains(&bound) {
            evidence.push(bound);
        }
        for field in reserved {
            v.as_object_mut().unwrap().remove(field);
        }
        v
    } else {
        let v = request["verdict"].clone();
        let fields = v.as_object().context("verdict must be an object")?;
        ensure!(
            !fields.keys().any(|k| reserved.contains(&k.as_str())),
            "omit tool-owned identity, candidate, artifact and rubric fields"
        );
        v
    };
    verdict["kind"] = json!(KIND);
    verdict["page_id"] = row["id"].clone();
    verdict["reviewer_session"] = json!(reviewer);
    verdict["candidate_sha256"] = binding["sha256"].clone();
    verdict["candidate"] = binding.clone();
    for key in ["source", "artifact", "render"] {
        verdict[key] = candidate[key].clone();
    }
    if let Some(rubric) = plan.get("rubric") {
        verdict["rubric"] = rubric.clone();
    }
    if let Some(prior) = replaces {
        verdict["replaces_verdict_id"] = prior;
    }
    validate(ctx, &plan, &mut verdict)?;
    let previous: Option<Value> = old.as_deref().map(serde_json::from_slice).transpose()?;
    crate::source_detail_review::validate(ctx, row, &candidate, previous.as_ref(), &verdict)?;
    let lines = crate::page_source_lines::assess(ctx, &candidate, &verdict)?;
    if lines["total"] != 0 {
        verdict["source_line_coverage"] = lines;
    }
    if crate::repair_gate::guarded(ctx)? && verdict["accepted"] == false {
        crate::repair_gate::validate(ctx, &plan, &candidate, &verdict)?;
        if disposition(&verdict).is_none()
            && let Some(baseline) = crate::repair_text::baseline(ctx, &candidate, &verdict)?
        {
            verdict["native_text_baseline"] = baseline;
        }
        verdict["repair_gate"] = json!(crate::repair_gate::VERSION);
    }
    if verdict["disposition"].get("blocked_report").is_some() {
        ensure!(
            request["replaces_verdict_id"].is_string(),
            "investigate the rejected verdict before recording a blocker"
        );
        checked_blocker(ctx, &plan, &verdict)?;
    }

    for evidence in verdict["evidence"]
        .as_array()
        .context("missing verdict evidence")?
    {
        ensure!(
            ctx.verify(evidence)? != dest,
            "review evidence must not reference its own mutable destination"
        );
    }
    let bytes = serde_json::to_vec_pretty(&verdict)?;
    let digest = fs::digest(&bytes);
    let archive = format!(".octos/page-verdicts/{digest}.json");
    let verdict_id = token("verdict", &json!({"sha256":digest}))?;
    let identical = old.as_deref() == Some(bytes.as_slice());
    if !identical {
        ensure!(
            verdict["disposition"]["owner"] != "task_owner",
            "route rubric disputes to arbiter or capability limitations to tool_maintainer; human approval cannot resolve a page verdict"
        );
        match old.as_ref() {
            None => ensure!(
                request.get("replaces_verdict_id").is_none(),
                "previous verdict is absent"
            ),
            Some(previous) => {
                let current: Value = serde_json::from_slice(previous)?;
                if verdict["disposition"].get("blocked_report").is_some() {
                    ensure!(
                        matches_investigated_referral(ctx, &current, &verdict)?,
                        "blocked report must resolve an investigated referral for the same candidate and findings"
                    );
                }
                ensure!(
                    current["kind"] == KIND && current["page_id"] == id,
                    "preserve existing review file; configure a fresh plan review path for native submissions"
                );
                ensure!(
                    current["reviewer_session"]
                        .as_str()
                        .map(page_gate::identity)
                        == Some(page_gate::identity(reviewer)),
                    "a different reviewer owns the current verdict"
                );
                let prior = fs::digest(previous);
                let prior_binding = json!({"sha256":prior});
                let prior_id = token("verdict", &prior_binding)?;
                ensure!(
                    request["replaces_verdict_id"] == prior_id,
                    "review changed; read the previous verdict before revision, then set top-level replaces_verdict_id to {prior_id} alongside page, candidate_id and verdict"
                );
                ensure!(
                    ctx.read(format!(".octos/page-verdicts/{prior}.json"))? == *previous,
                    "previous archived verdict differs from current review"
                );
                crate::repair_gate::validate_revision(ctx, &prior_binding, &current, &verdict)?;
            }
        }
    }
    // Save the immutable version first. A retry after a crash can finish replacing the head.
    if ctx.path(&archive)?.exists() {
        ensure!(ctx.read(&archive)? == bytes, "archived verdict changed");
    } else {
        ctx.write_new(&archive, &bytes)?;
    }
    ctx.verify(&binding)?;
    if !identical {
        if old.is_some() {
            ctx.replace_json(&dest, &verdict)?;
        } else {
            ctx.write_new(&dest, &bytes)?;
        }
    }
    let mut receipt = json!({"page":id,"candidate_id":request["candidate_id"],"verdict_id":verdict_id,
        "review":ctx.binding(&dest)?,"archive":ctx.binding(archive)?,"accepted":verdict["accepted"],
        "score":verdict["score"],"notification_sent":false,"quality_approved":false});
    // The verdict is already committed. If routing evidence is unavailable, the
    // coordinator can reconcile it without asking the reviewer to sign again.
    match crate::repair_gate::followup(ctx, &plan, row, &verdict, &receipt["review"]) {
        Ok(Some(event)) => receipt["repair_event"] = event,
        Ok(None) => {}
        Err(error) => receipt["repair_route_error"] = json!(error.to_string()),
    }
    drop(_lock);
    crate::page_assembly::progress::after_publication(ctx, &plan, &mut receipt);
    Ok(receipt)
}
