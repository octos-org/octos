use crate::{Context, string};
use anyhow::Context as _;
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
const DOMAINS: [&str; 3] = ["copy", "artwork", "readability"];
const POLICY: &str = ".octos/continuation.json";
const BOOK: &str = ".octos/pptx-evidence-history.json";

fn lock(ctx: &Context) -> Result<std::fs::File> {
    use fs2::FileExt;
    let dir = ctx.path(".octos")?;
    std::fs::create_dir_all(&dir)?;
    ctx.path(&dir)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(ctx.path(".octos/pptx-evidence.lock")?)?;
    file.try_lock_exclusive()
        .map_err(|e| anyhow::anyhow!("another evidence writer is active: {e}"))?;
    Ok(file)
}

pub fn init(ctx: &Context, r: &Value) -> Result<Value> {
    let _lock = lock(ctx)?;
    ensure!(
        [
            POLICY,
            BOOK,
            ".octos/continuation-state.json",
            ".octos/iteration-evidence.json",
            crate::review_routes::PLAN_PATH
        ]
        .iter()
        .all(|p| ctx.path(p).is_ok_and(|p| !p.exists())),
        "preserve existing task policy/history; use evidence_retarget for delivery path changes, never reinitialize to reset history"
    );
    let task = string(r, "task_id")?;
    let owner = match ctx.continuation_owner.as_deref() {
        Some(owner) => {
            ensure!(
                r.get("owner_agent")
                    .is_none_or(|v| v.as_str() == Some(owner)),
                "owner_agent differs from this runtime's identity: {owner}; omit owner_agent to bind automatically"
            );
            owner
        }
        None => r["owner_agent"].as_str().filter(|s| !s.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("No host continuation identity was supplied. Under Octos this is a host/plugin wiring error: do not invent a peer nickname or use inherited CODEX_SESSION_ID. Check doctor.continuation and repair the host before freezing policy. Only an independently launched standalone CLI supplies owner_agent explicitly."))?,
    };
    let window = r["stagnant_attempts"]
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("explicit stagnant_attempts required"))?;
    ensure!(
        window > 0 && window <= u32::MAX as u64 && task.len() <= 256,
        "invalid policy"
    );
    let delivery = r["deliverables"]
        .as_array()
        .filter(|a| !a.is_empty())
        .ok_or_else(|| anyhow::anyhow!("deliverables required"))?;
    for d in delivery {
        let p = d
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("invalid deliverable"))?;
        ensure!(
            !std::path::Path::new(p).is_absolute(),
            "deliverable paths must be relative"
        );
        let resolved = ctx.path(p)?;
        ensure!(
            !p.ends_with('/') && !p.ends_with('\\') && (!resolved.exists() || resolved.is_file()),
            "deliverables must name individual files, not directories: {p}"
        );
    }
    let plan = crate::review_routes::plan(r, owner)?;
    let policy = json!({"schema_version":1,"task_id":task,"owner_agent":owner,"evidence_file":".octos/iteration-evidence.json","stagnant_attempts":window,"deliverables":delivery,"required_checks":[{"id":"package","epsilon":0},{"id":"copy","epsilon":0},{"id":"artwork","epsilon":0},{"id":"readability","epsilon":0}]});
    // Publish a pending snapshot BEFORE enabling the observer. Otherwise the
    // first tool batch sees a missing evidence file and can yield before the
    // producer has had a chance to build/check its first candidate.
    ctx.write_json(
        ".octos/iteration-evidence.json",
        &json!({
            "schema_version":1,"task_id":task,"attempt":0,"phase":"pending",
            "artifacts":[],"checks":[]
        }),
    )?;
    ctx.write_json(crate::review_routes::PLAN_PATH, &plan)?;
    ctx.write_json(POLICY, &policy)?;
    Ok(
        json!({"policy":ctx.binding(POLICY)?,"owner_agent":owner,"identity_source":if ctx.continuation_owner.is_some(){"runtime"}else{"explicit_cli"},"runtime_consumed_current_evidence":false,"quality_approved":false}),
    )
}
/// Change only the declared delivery locations. A durable revision precedes
/// policy publication so the observer can adopt it without resetting counters.
pub fn retarget(ctx: &Context, r: &Value) -> Result<Value> {
    let _lock = lock(ctx)?;
    let previous = ctx.read(POLICY)?;
    let mut policy: Value = serde_json::from_slice(&previous)?;
    let owner = ctx
        .continuation_owner
        .as_deref()
        .or_else(|| r["owner_agent"].as_str())
        .ok_or_else(|| anyhow::anyhow!("runtime owner required"))?;
    ensure!(
        policy["owner_agent"] == owner,
        "this runtime does not own the continuation policy"
    );
    for field in [
        "task_id",
        "stagnant_attempts",
        "required_checks",
        "evidence_file",
        "verification_plan",
        "verification_mode",
        "reviewer_agent",
    ] {
        ensure!(
            r.get(field).is_none(),
            "evidence_retarget changes delivery locations only, not {field}"
        );
    }
    let reason = string(r, "reason")?;
    ensure!(
        !reason.trim().is_empty() && reason.len() <= 4096,
        "a bounded migration reason is required"
    );
    let paths = r["deliverables"]
        .as_array()
        .filter(|a| !a.is_empty() && a.len() <= 512)
        .ok_or_else(|| anyhow::anyhow!("declare 1..512 delivery files"))?;
    let mut seen = BTreeSet::new();
    for value in paths {
        let path = value
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("invalid delivery path"))?;
        let resolved = ctx.path(path)?;
        ensure!(
            !std::path::Path::new(path).is_absolute()
                && !path.ends_with('/')
                && !path.ends_with('\\')
                && (!resolved.exists() || resolved.is_file())
                && seen.insert(resolved),
            "delivery paths must be unique workspace-relative files"
        );
    }
    if policy["deliverables"] == r["deliverables"] {
        return Ok(json!({"changed":false,"history_preserved":true,"quality_approved":false}));
    }
    if ctx.path(".octos/continuation-state.json")?.exists() {
        let state = ctx.json(".octos/continuation-state.json")?;
        ensure!(
            state["policy_digest"]
                .as_str()
                .is_none_or(|s| s.is_empty() || s == crate::fs::digest(&previous)),
            "previous delivery revision has not been consumed; let the host observe it before changing paths again"
        );
    }
    policy["deliverables"] = r["deliverables"].clone();
    let digest = crate::fs::digest(&serde_json::to_vec_pretty(&policy)?);
    let revision = format!(".octos/continuation-delivery-revisions/{digest}.json");
    let record = json!({"schema_version":1,"previous_policy":String::from_utf8(previous)?,"new_policy_digest":digest,"reason":reason});
    if ctx.path(&revision)?.exists() {
        ensure!(
            ctx.json(&revision)? == record,
            "conflicting delivery migration record"
        );
    } else {
        ctx.write_json(&revision, &record)?;
    }
    ctx.replace_json(POLICY, &policy)?;
    Ok(
        json!({"changed":true,"policy":ctx.binding(POLICY)?,"revision":ctx.binding(revision)?,"history_preserved":true,"quality_approved":false,"next":"Record the current checked cycle; retargeting is not a review."}),
    )
}
pub fn status(ctx: &Context, r: &Value) -> Result<Value> {
    let p = ctx.json(POLICY)?;
    let owner = ctx
        .continuation_owner
        .as_deref()
        .or_else(|| r["owner_agent"].as_str());
    let owner_matches = owner.is_some_and(|o| p["owner_agent"] == o);
    let ep = ctx.path(string(&p, "evidence_file")?)?;
    let sp = ctx.path(".octos/continuation-state.json")?;
    let mut v = json!({"configured":true,"runtime_owner":owner,"configured_owner":p["owner_agent"],"owner_matches":owner_matches,"runtime_consumed_current_evidence":false,"process_liveness_verified":false,"quality_approved":false});
    if ep.is_file() {
        let e = ctx.json(&ep)?;
        let bindings = e["artifacts"].as_array().cloned().unwrap_or_default();
        for binding in &bindings {
            if binding["path"].as_str().is_some_and(|p| {
                std::path::Path::new(p).starts_with(".octos/pptx-native-qa-records")
            }) {
                let qa = ctx.json(ctx.verify(binding)?)?;
                v["native_qa"] = json!({"report":binding,"status":qa["status"],"failed":qa["failed"],
                    "unknown":qa["unknown"],"diagnosis":qa["diagnosis"],"inventory":qa["inventory"],"constraint_changes":qa["constraint_changes"]});
            }
            if binding["path"].as_str().is_some_and(|p| {
                std::path::Path::new(p).starts_with(".octos/pptx-comparison-records")
            }) {
                let record = ctx.json(ctx.verify(binding)?)?;
                v["comparison_coverage"] = record["coverage"].clone();
                v["object_coverage"] = record["object_coverage"].clone();
                v["verification_route"] = record["verification_route"].clone();
                v["reference_fidelity"] = record["reference_fidelity"].clone();
                v["local_background_repairs"] = record["local_background_repairs"].clone();
            }
        }
        let missing: Vec<_> = p["deliverables"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|d| {
                let Some(path) = d.as_str().and_then(|p| ctx.path(p).ok()) else {
                    return true;
                };
                !bindings.iter().any(|b| {
                    b["path"].as_str().is_some_and(|candidate| {
                        ctx.path(candidate).is_ok_and(|candidate| candidate == path)
                    }) && ctx.verify(b).is_ok()
                })
            })
            .cloned()
            .collect();
        v["delivery_bindings_current"] = json!(missing.is_empty());
        v["unbound_deliverables"] = json!(missing);
        v["checks"] = e["checks"].clone();
        v["check_feedback"] = crate::check_feedback::summarize(
            &e["checks"],
            &v["native_qa"],
            &v["object_coverage"],
            &v["comparison_coverage"],
        );
    }
    if owner_matches && ep.is_file() && sp.is_file() {
        let e = ctx.json(&ep)?;
        let state = ctx.json(&sp)?;
        v["runtime_consumed_current_evidence"] = json!(
            e["task_id"] == p["task_id"]
                && e["phase"] == "checked"
                && e["attempt"].as_u64().is_some_and(|n| n > 0)
                && state["attempt"] == e["attempt"]
                && state["policy_digest"] == crate::fs::sha_file(&ctx.path(POLICY)?)?
                && state["snapshot_digest"] == crate::fs::sha_file(&ep)?
        );
        v["attempt"] = e["attempt"].clone();
        v["flat_attempts"] = state["flat_attempts"].clone();
        v["warned"] = state["warned"].clone();
    }
    Ok(v)
}
pub fn record(ctx: &Context, r: &Value) -> Result<Value> {
    let _lock = lock(ctx)?;
    let policy = ctx.json(POLICY)?;
    if let Some(owner) = ctx.continuation_owner.as_deref() {
        ensure!(
            policy["owner_agent"] == owner,
            "this runtime ({owner}) does not own the continuation policy; inspect evidence_status and preserve existing history"
        );
    }
    let cp = ctx.path(string(r, "cycle")?)?;
    let (cycle, spec) = crate::review::bound_cycle(ctx, &cp)?;
    let pages: Vec<_> = spec.pages.iter().map(|p| p.id.clone()).collect();
    let page_set: BTreeSet<_> = pages.iter().cloned().collect();
    let old = if ctx.path(BOOK)?.exists() {
        ctx.json(BOOK)?
    } else {
        json!({})
    };
    let verification_plan = if ctx.path(crate::review_routes::PLAN_PATH)?.exists() {
        ctx.binding(crate::review_routes::PLAN_PATH)?
    } else {
        Value::Null
    };
    ensure!(
        old.get("attempt").is_none() || old["verification_plan"] == verification_plan,
        "verification plan changed during the task; preserve the frozen route and history"
    );
    let ep = string(&policy, "evidence_file")?;
    let previous = if ctx.path(ep)?.exists() {
        ctx.json(ep)?
    } else {
        json!({})
    };
    let mut scope_reason = old["scope_change_reason"].clone();
    if !old.as_object().is_some_and(|o| o.is_empty()) {
        ensure!(
            old["task_id"] == policy["task_id"],
            "evidence belongs to another task; preserve existing history"
        );
        let old_pages: BTreeSet<String> = serde_json::from_value(old["pages"].clone())?;
        ensure!(
            old_pages.is_subset(&page_set),
            "page scope cannot shrink; keep all previously checked pages"
        );
        if old_pages != page_set {
            let reason = string(r, "extend_scope_reason")?;
            ensure!(
                !reason.trim().is_empty() && reason.len() <= 4096,
                "a bounded scope extension reason is required"
            );
            ensure!(
                previous["phase"] == "checked"
                    && previous["checks"].as_array().is_some_and(|checks| {
                        ["package", "copy", "artwork", "readability"]
                            .iter()
                            .all(|id| {
                                checks
                                    .iter()
                                    .any(|c| c["id"] == *id && c["status"] == "pass")
                            })
                    }),
                "finish the current page batch before extending its scope; unresolved findings are preserved"
            );
            scope_reason = json!(reason);
        }
    }
    let mut artifacts = vec![
        ctx.binding(&cp)?,
        cycle["spec"].clone(),
        cycle["editable"].clone(),
        cycle["blank"].clone(),
    ];
    if verification_plan.is_object() {
        artifacts.push(verification_plan.clone());
    }
    for source in cycle["sources"].as_array().into_iter().flatten() {
        ctx.verify(source)?;
        artifacts.push(source.clone());
    }
    let mut images = BTreeSet::new();
    for k in ["editable", "blank"] {
        for b in cycle["renders"][k]["pages"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("missing render images"))?
        {
            let p = ctx.verify(b)?;
            images.insert(p);
            artifacts.push(json!({"path":b["path"],"sha256":b["sha256"]}));
        }
    }
    for page in &spec.pages {
        for b in std::iter::once(&page.background).chain(page.design.iter()) {
            artifacts.push(ctx.binding(ctx.verify(b)?)?);
        }
    }
    let mut findings: BTreeMap<String, Value> = old
        .get("findings")
        .cloned()
        .map(serde_json::from_value)
        .transpose()?
        .unwrap_or_default();
    for f in findings.values_mut() {
        *f = crate::finding_guard::refresh(ctx, f, &cycle, &spec)?;
    }
    let review = if let Some(path) = r["review"].as_str() {
        let review = ctx.json(path)?;
        ensure!(
            review["cycle_sha256"] == crate::fs::sha_file(&cp)?,
            "review refers to another cycle"
        );
        // Recheck attached measurements even when callers skip review_submit.
        // Legacy reviews without relation evidence retain the existing gates.
        if review["relations"].is_object() {
            let relation_path = ctx.verify(&review["relations"])?;
            let report = ctx.json(&relation_path)?;
            ensure!(
                report["algorithm"] == crate::relations::ALGORITHM
                    && report["cycle"]["sha256"] == review["cycle_sha256"],
                "stale relation evidence"
            );
            let signals: Vec<_> = report["pages"]
                .as_array()
                .into_iter()
                .flatten()
                .flat_map(|p| p["signals"].as_array().into_iter().flatten())
                .cloned()
                .collect();
            let independent =
                crate::review_routes::authorized(ctx, &policy, &review, &ctx.binding(&cp)?)?;
            crate::relations::validate_responses_with_route(&signals, &review, independent)?;
            artifacts.push(ctx.binding(relation_path)?);
        } else {
            ensure!(
                review.get("relation_reviews").is_none(),
                "relation responses require their bound measurement report"
            );
        }
        artifacts.push(ctx.binding(path)?);
        review
    } else {
        json!({})
    };
    // Fresh native measurements are authoritative. A model-authored pass,
    // dismissed finding or edited inspection report cannot replace them.
    let independent = crate::review_routes::authorized(ctx, &policy, &review, &ctx.binding(&cp)?)?;
    let (repair_contract, local_repairs) = crate::fidelity::repair_contract(
        ctx,
        &cycle,
        &spec,
        &review,
        &old,
        independent,
        &crate::review_routes::load_plan(ctx)?,
    )?;
    let native_qa = crate::qa::evaluate(ctx, string(r, "cycle")?, &repair_contract)?;
    let accepted_measurements = crate::review_routes::adjudicated(
        &native_qa,
        &review,
        independent,
        &spec,
        &crate::review_routes::load_plan(ctx)?,
    )?;
    let verification_feedback = crate::review_routes::feedback(
        &native_qa,
        &review,
        &accepted_measurements,
        &crate::review_routes::load_plan(ctx)?,
    );
    if review["attestation"].is_object() {
        artifacts.push(review["attestation"].clone());
    }
    let qa_digest = crate::fs::digest(&serde_json::to_vec(&native_qa)?);
    let qa_path = format!(".octos/pptx-native-qa-records/{qa_digest}.json");
    if !ctx.path(&qa_path)?.exists() {
        ctx.write_json(&qa_path, &native_qa)?;
    } else {
        ensure!(
            ctx.json(&qa_path)? == native_qa,
            "native QA record was modified"
        );
    }
    let qa_binding = ctx.binding(&qa_path)?;
    artifacts.push(qa_binding.clone());
    let qa_summary = json!({"report":qa_binding,"status":native_qa["status"],"local_background_repairs":local_repairs,
        "independent_visual_acceptances":accepted_measurements,"native_status_unchanged":true,
        "failed":native_qa["failed"],"unknown":native_qa["unknown"],
        "diagnosis":native_qa["diagnosis"],
        "inventory":native_qa["inventory"],"constraint_changes":native_qa["constraint_changes"],
        "next":"Read the native QA report for object IDs, measurements and remedies. Model dispositions cannot clear machine findings."});
    let comparison_coverage = crate::comparison_review::assess(
        ctx,
        &cycle,
        &spec,
        &review,
        &old,
        r["comparisons"].as_str(),
    )?;
    artifacts.extend(comparison_coverage.artifacts.iter().cloned());
    let object_coverage = crate::object_review::assess(&cycle, &spec, &review, &old)?;
    let mut fidelity = crate::fidelity::assess(
        &cycle,
        &spec,
        &review,
        &crate::review_routes::load_plan(ctx)?,
    )?;
    crate::fidelity::require_independent(
        &mut fidelity,
        independent,
        &crate::review_routes::load_plan(ctx)?,
    );
    let arbiter =
        crate::review_routes::arbiter_authorized(ctx, &policy, &review, &ctx.binding(&cp)?)?;
    let mut transitions = review.clone();
    // Retain before receipts when direct record callers omit a prior defect.
    // Only carry omitted fidelity rows into transitions; other historical rows
    // are already retained by the existing ledger.
    let mut rows = review["findings"].as_array().cloned().unwrap_or_default();
    for (id, f) in &findings {
        if id.starts_with("fidelity-") && !rows.iter().any(|r| r["id"] == *id) {
            let mut f = f.clone();
            f.as_object_mut()
                .context("evidence::record: missing required value")?
                .remove("evidence");
            if let Some(local) = crate::finding_guard::scope(ctx, &f, &json!({}), &cycle, &spec)? {
                f["evidence"] = local["image"]["path"].clone();
            }
            rows.push(f);
        }
    }
    transitions["findings"] = json!(rows);
    crate::fidelity::register_defects(ctx, &cp, &fidelity.summary, &mut transitions)?;
    let mut seen = BTreeSet::new();
    if let Some(list) = transitions["findings"].as_array() {
        for f in list {
            let id = string(f, "id")?;
            ensure!(seen.insert(id.to_string()), "duplicate finding id");
            let domain = string(f, "check")?;
            ensure!(DOMAINS.contains(&domain), "unknown check domain");
            ensure!(
                matches!(
                    string(f, "state")?,
                    "open" | "unknown" | "resolved" | "dismissed"
                ),
                "invalid finding state"
            );
            string(f, "note")?;
            let evidence = ctx.path(string(f, "evidence")?)?;
            ensure!(
                images.contains(&evidence),
                "finding evidence must be a current rendered image"
            );
            if let Some(old) = findings.get(id) {
                ensure!(old["check"] == domain, "finding category changed");
            }
            let guarded = crate::finding_guard::transition_with_arbitration(
                ctx,
                f,
                findings.get(id),
                &cycle,
                &spec,
                arbiter,
            )?;
            findings.insert(id.into(), guarded);
        }
    }
    let preflight = crate::review::preflight(ctx, &ctx.verify(&cycle["editable"])?)?;
    let count = preflight["issue_count"].as_u64().unwrap_or(1);
    let mut checks =
        vec![json!({"id":"package","status":if count==0{"pass"}else{"fail"},"remaining":count})];
    for d in DOMAINS {
        let inspected = review["inspected"][d]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let mut coverage = BTreeSet::new();
        for p in inspected {
            let p = p
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("invalid page coverage"))?;
            ensure!(
                page_set.contains(p) && coverage.insert(p.to_string()),
                "unknown/duplicate page coverage"
            );
        }
        let active: Vec<_> = findings
            .values()
            .filter(|f| f["check"] == d && matches!(f["state"].as_str(), Some("open" | "unknown")))
            .collect();
        let missing = pages.len() - coverage.len();
        let visual = matches!(d, "artwork" | "readability");
        let diff_unknown = if visual {
            comparison_coverage.unknown + object_coverage.unknown + fidelity.unknown
        } else if d == "copy" {
            object_coverage.summary["symbol_unknown"]
                .as_u64()
                .unwrap_or(0) as usize
        } else {
            0
        };
        let diff_defects = if visual {
            comparison_coverage.defects + object_coverage.defects + fidelity.defects
        } else if d == "copy" {
            object_coverage.summary["symbol_defects"]
                .as_u64()
                .unwrap_or(0) as usize
        } else {
            0
        };
        let machine = native_qa["findings"]
            .as_array()
            .context("evidence::record: missing required value")?;
        let machine_fail = machine
            .iter()
            .filter(|f| f["check"] == d && f["status"] == "fail")
            .count();
        let machine_unknown = machine
            .iter()
            .filter(|f| {
                f["check"] == d
                    && f["status"] == "unknown"
                    && !accepted_measurements.contains(f["id"].as_str().unwrap_or(""))
            })
            .count();
        let fail = diff_defects + machine_fail > 0 || active.iter().any(|f| f["state"] == "open");
        let unknown = missing + diff_unknown + machine_unknown > 0
            || active.iter().any(|f| f["state"] == "unknown");
        checks.push(json!({"id":d,"status":if fail{"fail"}else if unknown{"unknown"}else{"pass"},"remaining":active.len()+missing+diff_unknown+diff_defects+machine_fail+machine_unknown}));
    }
    if let Some(deliverables) = policy["deliverables"].as_array() {
        for p in deliverables {
            let path = ctx.path(p.as_str().ok_or_else(|| anyhow::anyhow!("deliverable"))?)?;
            if path.is_file() {
                let binding = ctx.binding(&path)?;
                let same_content = |a: &Value| a["sha256"] == binding["sha256"];
                // A blank PPTX or artwork PNG is not the checked editable
                // delivery/preview merely because it is part of the cycle.
                let alias_matches = match path
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(str::to_ascii_lowercase)
                    .as_deref()
                {
                    Some("pptx") => same_content(&cycle["editable"]),
                    Some("png") => cycle["renders"]["editable"]["pages"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .any(same_content),
                    Some("json") => {
                        same_content(&cycle["spec"])
                            || cycle["sources"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .any(same_content)
                    }
                    _ => false,
                };
                if alias_matches {
                    artifacts.push(binding);
                }
            }
            ensure!(
                artifacts.iter().any(|a| a["path"]
                    .as_str()
                    .is_some_and(|p| ctx.path(p).is_ok_and(|p| p == path))),
                "delivery is not bound to this checked cycle: {}. Use evidence_retarget for new paths; copy checked outputs byte-for-byte; run a fresh cycle to bind source JSON. Do not reinitialize history.",
                path.display()
            );
        }
    }
    // Keep the kernel's generic Snapshot wire schema unchanged. Detailed PPTX
    // evidence is a bound immutable artifact, not a new kernel-level field.
    let comparison_record = json!({"local_background_repairs":local_repairs,"reference_fidelity":fidelity.summary,"object_coverage":object_coverage.summary,"object_reviews":object_coverage.reviews,"coverage":comparison_coverage.summary,
        "reviews":comparison_coverage.reviews,"required_pages":comparison_coverage.required_pages,
        "verification_route":{"plan":verification_plan,"independent_visual_acceptances":accepted_measurements,
        "attestation":review["attestation"],"native_status_unchanged":true,"feedback":verification_feedback}});
    let comparison_digest = crate::fs::digest(&serde_json::to_vec_pretty(&comparison_record)?);
    let comparison_path = format!(".octos/pptx-comparison-records/{comparison_digest}.json");
    if ctx.path(&comparison_path)?.exists() {
        ensure!(
            ctx.json(&comparison_path)? == comparison_record,
            "comparison record changed"
        );
    } else {
        ctx.write_json(&comparison_path, &comparison_record)?;
    }
    artifacts.push(ctx.binding(comparison_path)?);
    artifacts.sort_by_key(Value::to_string);
    artifacts.dedup();
    let mut payload = json!({"schema_version":1,"task_id":policy["task_id"],"phase":"checked","artifacts":artifacts,"checks":checks,"scope":page_set,"scope_change_reason":scope_reason});
    let key = crate::fs::digest(&serde_json::to_vec(&payload)?);
    let n = old["attempt"]
        .as_u64()
        .unwrap_or(0)
        .max(previous["attempt"].as_u64().unwrap_or(0));
    let attempt = if old["key"] == key {
        n
    } else {
        n.checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("attempt overflow"))?
    };
    payload["attempt"] = json!(attempt);
    // Old records describe past bytes; only the NEW cycle must verify today.
    let previous_record = if attempt > n && n > 0 {
        let record = json!({"policy":policy,"snapshot":previous,"book":old});
        let hash = crate::fs::digest(&serde_json::to_vec_pretty(&record)?);
        let path = format!(".octos/pptx-evidence-records/{hash}.json");
        if !ctx.path(&path)?.exists() {
            ctx.write_json(&path, &record)?;
        }
        Some(ctx.binding(path)?)
    } else {
        None
    };
    ctx.replace_json(".octos/pptx-evidence-detail.json",&json!({"task_id":policy["task_id"],"cycle":cp,"package_preflight":preflight,"review":review,"findings":findings,"native_qa":qa_summary,"comparison_coverage":comparison_coverage.summary,"object_coverage":object_coverage.summary,"visual_quality_verified_by_tool":false}))?;
    let retained_contract =
        if !local_repairs.is_empty() && checks.iter().any(|c| c["status"] != "pass") {
            &old["qa_contract"]
        } else {
            &native_qa["contract"]
        };
    ctx.replace_json(BOOK,&json!({"task_id":policy["task_id"],"pages":pages,"attempt":attempt,"key":key,"findings":findings,"verification_plan":verification_plan,"qa_contract":retained_contract,"native_qa":qa_summary,"scope_change_reason":scope_reason,"comparison_reviews":comparison_coverage.reviews,"comparison_required_pages":comparison_coverage.required_pages,"object_reviews":object_coverage.reviews}))?;
    ctx.replace_json(ep, &payload)?;
    let check_feedback = crate::check_feedback::summarize(
        &json!(checks),
        &qa_summary,
        &object_coverage.summary,
        &comparison_coverage.summary,
    );
    Ok(
        json!({"evidence":ctx.binding(ep)?,"attempt":attempt,"checks":checks,"check_feedback":check_feedback,"verification_feedback":verification_feedback,"reference_fidelity":fidelity.summary,"native_qa":qa_summary,"comparison_coverage":comparison_coverage.summary,"object_coverage":object_coverage.summary,"previous_record":previous_record,"visual_quality_verified_by_tool":false,"review_provenance":"Fresh deterministic native QA plus human/model review of bound renders; model judgments cannot override machine failures"}),
    )
}
