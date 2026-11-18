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

pub(crate) fn check_rubric(plan: &Value) -> Result<()> {
    let Some(rubric) = plan.get("rubric") else {
        return Ok(());
    };
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
    let Some(rubric) = plan.get("rubric") else {
        ensure!(
            verdict["kind"] != KIND || verdict.get("rubric").is_none(),
            "published rubric no longer matches plan"
        );
        return Ok(());
    };
    ensure!(
        &verdict["rubric"] == rubric,
        "verdict must bind the task rubric"
    );
    let scores = verdict["component_scores"]
        .as_object()
        .context("component_scores must use the task rubric")?;
    let caps = rubric.as_object().context("invalid task rubric")?;
    ensure!(
        scores.len() == caps.len(),
        "component_scores must contain exactly the task rubric categories"
    );
    let mut total = 0.0;
    for (key, cap) in caps {
        let n = scores
            .get(key)
            .and_then(Value::as_f64)
            .with_context(|| format!("missing numeric rubric component {key}"))?;
        ensure!(
            cap.as_f64().is_some_and(|max| (0.0..=max).contains(&n)),
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
    if rejected_without_finding || verdict.get("disposition").is_some() {
        if disposition(verdict).is_none() {
            errors.push("a disposition requires a rejected verdict, peer role (reviewer, arbiter or tool_maintainer), reason and resume_when; otherwise list actionable findings".to_owned());
        }
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
                };
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
    page_gate::validate_candidate(ctx, row, &candidate)?;

    let reserved = [
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
        let phase = resolution_phase(prior, &v);
        ensure!(
            matches!(phase, "arbitrate" | "diagnose")
                && v["page_id"] == id
                && v["candidate"] == binding
                && v["reviewer_session"].as_str().map(page_gate::identity)
                    == Some(page_gate::identity(reviewer)),
            "the original reviewer must bind the report to the exact referred candidate"
        );
        let head = token("verdict", &json!({"sha256":fs::digest(bytes)}))?;
        let bound = ctx.binding(path)?;
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
                    let prior = disposition(&current)
                        .context("blocked report requires an existing peer referral")?;
                    ensure!(
                        current["candidate_sha256"] == verdict["candidate_sha256"]
                            && resolution_phase(prior, &current)
                                == resolution_phase(&verdict["disposition"], &verdict),
                        "blocked report must resolve the existing referral for the same candidate and phase"
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
                let prior_id = token("verdict", &json!({"sha256":prior}))?;
                ensure!(
                    request["replaces_verdict_id"] == prior_id,
                    "review changed; read the previous verdict before revision (current {prior_id})"
                );
                ensure!(
                    ctx.read(format!(".octos/page-verdicts/{prior}.json"))? == *previous,
                    "previous archived verdict differs from current review"
                );
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
    Ok(
        json!({"page":id,"candidate_id":request["candidate_id"],"verdict_id":verdict_id,
        "review":ctx.binding(&dest)?,"archive":ctx.binding(archive)?,"accepted":verdict["accepted"],
        "score":verdict["score"],"notification_sent":false,"quality_approved":false}),
    )
}
