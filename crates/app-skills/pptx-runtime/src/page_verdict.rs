//! Construct the page gate's envelope from a reviewer decision and a pinned publication.
use crate::{Context, fs, page_gate};
use anyhow::{Context as _, Result, ensure};
use serde_json::{Value, json};
use std::collections::BTreeSet;

const KIND: &str = "page-review-v1";

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
        Some("reviewer" | "tool_maintainer" | "task_owner")
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
            errors.push("a disposition requires a rejected verdict, owner (reviewer, tool_maintainer or task_owner), reason and resume_when; otherwise list actionable findings".to_owned());
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

    let mut verdict = request["verdict"].clone();
    let fields = verdict.as_object().context("verdict must be an object")?;
    let reserved: BTreeSet<_> = [
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
    ]
    .into_iter()
    .collect();
    ensure!(
        !fields.keys().any(|k| reserved.contains(k.as_str())),
        "omit tool-owned identity, candidate, artifact and rubric fields"
    );
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
    if let Some(prior) = request.get("replaces_verdict_id") {
        verdict["replaces_verdict_id"] = prior.clone();
    }
    validate(ctx, &plan, &mut verdict)?;

    let dest = ctx.path(crate::string(row, "review")?)?;
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
    let old = dest.exists().then(|| ctx.read(&dest)).transpose()?;
    let identical = old.as_deref() == Some(bytes.as_slice());
    if !identical {
        match old.as_ref() {
            None => ensure!(
                request.get("replaces_verdict_id").is_none(),
                "previous verdict is absent"
            ),
            Some(previous) => {
                let current: Value = serde_json::from_slice(previous)?;
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
