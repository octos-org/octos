//! Local change evidence for model-authored visual finding transitions.
//! Changed pixels are necessary for a repair claim, never sufficient for quality.
use crate::{Context, geometry, spec::Spec};
use anyhow::Context as _;
use anyhow::{Result, ensure};
use serde_json::{Value, json};

pub(crate) fn scope(
    ctx: &Context,
    f: &Value,
    old: &Value,
    cycle: &Value,
    spec: &Spec,
) -> Result<Option<Value>> {
    let named_page = f
        .get("page")
        .or_else(|| old.get("page"))
        .and_then(Value::as_str);
    let numbered_page = if let Some(number) = f.get("page_number") {
        let index = number
            .as_u64()
            .and_then(|n| n.checked_sub(1))
            .and_then(|n| usize::try_from(n).ok())
            .filter(|&n| n < spec.pages.len())
            .ok_or_else(|| {
                anyhow::anyhow!("page_number must be a 1-based display page inside this cycle")
            })?;
        Some(spec.pages[index].id.as_str())
    } else {
        None
    };
    if let (Some(named), Some(numbered)) = (named_page, numbered_page) {
        ensure!(
            named == numbered,
            "page_number and source page ID refer to different slides; use review_pack page_numbers to resolve the current cycle"
        );
    }
    let page = named_page.or(numbered_page);
    let inferred = if f.get("box").is_none() && old.get("box").is_none() {
        if let (Some(page), Some(object)) = (page, f["object_id"].as_str()) {
            let items = crate::object_review::inventory(cycle, spec)?;
            let item = items
                .iter()
                .find(|v| v["page"] == page && v["object_id"] == object)
                .ok_or_else(|| anyhow::anyhow!("unknown finding page/object"))?;
            let i = spec
                .pages
                .iter()
                .position(|p| p.id == page)
                .context("finding_guard::scope: missing required value")?;
            let (w, h) =
                image::image_dimensions(ctx.verify(&cycle["renders"]["editable"]["pages"][i])?)?;
            let a: [f64; 4] = serde_json::from_value(item["viewport_normalized_xyxy"].clone())?;
            Some(json!([
                a[0] * w as f64,
                a[1] * h as f64,
                (a[2] - a[0]) * w as f64,
                (a[3] - a[1]) * h as f64
            ]))
        } else {
            None
        }
    } else {
        None
    };
    let area = f
        .get("box")
        .or_else(|| old.get("box"))
        .or(inferred.as_ref());
    let (Some(page), Some(area)) = (page, area) else {
        return Ok(None);
    };
    let i = spec
        .pages
        .iter()
        .position(|p| p.id == page)
        .ok_or_else(|| anyhow::anyhow!("unknown finding page"))?;
    let binding = &cycle["renders"]["editable"]["pages"][i];
    let path = ctx.verify(binding)?;
    if let Some(evidence) = f["evidence"].as_str() {
        ensure!(
            ctx.path(evidence)? == path,
            "finding evidence is not the actual render of its selected page"
        );
    }
    let (w, h) = image::image_dimensions(path)?;
    let b = geometry::rect(area)?;
    ensure!(
        b[0] >= 0.
            && b[1] >= 0.
            && b[2] > 0.
            && b[3] > 0.
            && b[0] + b[2] <= f64::from(w)
            && b[1] + b[3] <= f64::from(h),
        "finding box must be a nonempty viewport inside the current rendered page"
    );
    Ok(Some(
        json!({"page":page,"page_number":i+1,"box":b,"image":binding}),
    ))
}
pub(crate) fn crop_digest(ctx: &Context, s: &Value) -> Result<String> {
    let im = crate::review::load_image(ctx, &ctx.verify(&s["image"])?)?.to_rgb8();
    pixel_digest(&im, geometry::rect(&s["box"])?)
}

pub(crate) fn pixel_digest(im: &image::RgbImage, b: [f64; 4]) -> Result<String> {
    let (x, y) = (b[0].floor() as u32, b[1].floor() as u32);
    let (w, h) = (
        (b[0] + b[2]).ceil() as u32 - x,
        (b[1] + b[3]).ceil() as u32 - y,
    );
    ensure!(
        x + w <= im.width() && y + h <= im.height(),
        "finding evidence dimensions changed"
    );
    let cropped = image::imageops::crop_imm(im, x, y, w, h).to_image();
    let mut bytes = Vec::from(w.to_le_bytes());
    bytes.extend(h.to_le_bytes());
    bytes.extend(cropped.as_raw());
    Ok(crate::fs::digest(&bytes))
}

/// Capture a defect independently of the Maker's final-check policy. The
/// Reviewer can publish an immutable before-state as soon as it sees a defect.
pub fn register(ctx: &Context, request: &Value) -> Result<Value> {
    let cycle_path = ctx.path(crate::string(request, "cycle")?)?;
    let (cycle, spec) = crate::review::bound_cycle(ctx, &cycle_path)?;
    let list = request["findings"]
        .as_array()
        .filter(|v| !v.is_empty() && v.len() <= 512)
        .ok_or_else(|| anyhow::anyhow!("findings requires 1..512 local defects"))?;
    let mut seen = std::collections::BTreeSet::new();
    // Validate the whole batch before writing any receipt.
    let mut prepared = Vec::new();
    for f in list {
        let id = crate::string(f, "id")?;
        ensure!(
            id.len() <= 256 && seen.insert(id),
            "duplicate or oversized finding id"
        );
        ensure!(
            matches!(
                f["check"].as_str(),
                Some("copy" | "artwork" | "readability")
            ),
            "invalid finding check"
        );
        ensure!(
            f["state"].is_null() || f["state"] == "open",
            "register records open defects, not completion claims"
        );
        ensure!(
            !crate::string(f, "note")?.trim().is_empty(),
            "a concrete defect observation is required"
        );
        let mut local = scope(ctx, f, &Value::Null, &cycle, &spec)?.ok_or_else(|| {
            anyhow::anyhow!(
                "finding {id} needs page + object_id, or page + render-pixel box, before repair"
            )
        })?;
        local["crop_sha256"] = json!(crop_digest(ctx, &local)?);
        prepared.push((f, local));
    }
    let mut findings = Vec::new();
    for (f, local) in prepared {
        let i = local["page_number"]
            .as_u64()
            .context("finding_guard::register: missing required value")? as usize
            - 1;
        let mut receipt = json!({"schema":"pptx-finding-registration-v1", "id":f["id"],
            "check":f["check"], "note":f["note"], "cycle":ctx.binding(&cycle_path)?,
            "before":local, "quality_proven":false});
        let key = crate::fs::digest(&serde_json::to_vec(&receipt)?);
        let dir = format!(".octos/pptx-finding-registrations/{key}");
        let receipt_path = format!("{dir}/registration.json");
        let (w, h) = image::image_dimensions(ctx.verify(&local["image"])?)?;
        let area = geometry::rect(&local["box"])?;
        let mut sources = vec![
            ("current", local["image"].clone()),
            ("blank", cycle["renders"]["blank"]["pages"][i].clone()),
        ];
        if let Some(design) = &spec.pages[i].design {
            sources.push(("design", design.clone()));
        }
        let mut views = serde_json::Map::new();
        for (role, source) in sources {
            let image = ctx.verify(&source)?;
            let (sw, sh) = image::image_dimensions(&image)?;
            let b = [
                area[0] * sw as f64 / w as f64,
                area[1] * sh as f64 / h as f64,
                area[2] * sw as f64 / w as f64,
                area[3] * sh as f64 / h as f64,
            ];
            let output = format!("{dir}/{role}.png");
            if !ctx.path(&output)?.exists() {
                crate::review::crop(ctx, &json!({"input":image,"box":b,"output":output}))?;
            }
            views.insert(role.into(), ctx.binding(&output)?);
        }
        receipt["views"] = json!(views);
        if ctx.path(&receipt_path)?.exists() {
            ensure!(
                ctx.json(&receipt_path)? == receipt,
                "registered evidence was modified"
            );
        } else {
            ctx.write_json(&receipt_path, &receipt)?;
        }
        let mut registered = f.clone();
        registered["state"] = json!("open");
        registered["page"] = local["page"].clone();
        registered["box"] = local["box"].clone();
        registered["evidence"] = local["image"]["path"].clone();
        registered["registration"] = ctx.binding(&receipt_path)?;
        findings.push(registered);
    }
    Ok(
        json!({"findings":findings,"quality_approved":false,"next":"Publish these registered findings before repair. Preserve registration and the viewport in later reviews; changed pixels alone do not prove a fix."}),
    )
}

fn registered_baseline(ctx: &Context, f: &Value) -> Result<Option<Value>> {
    if !f["registration"].is_object() {
        return Ok(None);
    }
    let receipt = ctx.json(ctx.verify(&f["registration"])?)?;
    ensure!(
        receipt["schema"] == "pptx-finding-registration-v1"
            && receipt["id"] == f["id"]
            && receipt["check"] == f["check"],
        "finding registration identity mismatch"
    );
    let cp = ctx.verify(&receipt["cycle"])?;
    let (cycle, spec) = crate::review::bound_cycle(ctx, &cp)?;
    let before = &receipt["before"];
    let measured = scope(ctx, before, &Value::Null, &cycle, &spec)?
        .ok_or_else(|| anyhow::anyhow!("incomplete finding registration"))?;
    ensure!(
        measured["image"] == before["image"]
            && crop_digest(ctx, &measured)? == before["crop_sha256"],
        "stale finding baseline"
    );
    for b in receipt["views"]
        .as_object()
        .into_iter()
        .flat_map(|v| v.values())
    {
        ctx.verify(b)?;
    }
    let mut origin = f.clone();
    origin["state"] = json!("open");
    origin["page"] = before["page"].clone();
    origin["box"] = before["box"].clone();
    origin["local_evidence"] = before.clone();
    Ok(Some(origin))
}

pub fn transition(
    ctx: &Context,
    f: &Value,
    old: Option<&Value>,
    cycle: &Value,
    spec: &Spec,
) -> Result<Value> {
    transition_with_arbitration(ctx, f, old, cycle, spec, false)
}

pub fn transition_with_arbitration(
    ctx: &Context,
    f: &Value,
    old: Option<&Value>,
    cycle: &Value,
    spec: &Spec,
    arbiter_authorized: bool,
) -> Result<Value> {
    let none = Value::Null;
    let registered = registered_baseline(ctx, f)?;
    if let (Some(old), Some(origin)) = (old, registered.as_ref())
        && old["local_evidence"].is_object()
    {
        ensure!(
            old["local_evidence"]["page"] == origin["page"]
                && old["local_evidence"]["box"] == origin["box"],
            "cannot replace a finding's original viewport"
        );
    }
    let old = old
        .filter(|v| v["local_evidence"].is_object())
        .or(registered.as_ref())
        .or(old)
        .unwrap_or(&none);
    let mut next = f.clone();
    // Tool-owned fields must never be populated from model JSON.
    for field in ["closure_check", "local_evidence"] {
        next.as_object_mut()
            .context("finding_guard::transition_with_arbitration: missing required value")?
            .remove(field);
    }
    let mut current = scope(ctx, f, old, cycle, spec)?;
    ensure!(
        f["state"] != "open" || current.is_some(),
        "an open finding needs page + object_id or page + render-pixel box before repair"
    );
    if let Some(origin) = registered.as_ref() {
        ensure!(
            current
                .as_ref()
                .is_some_and(|s| s["page"] == origin["page"] && s["box"] == origin["box"]),
            "registered defect viewport cannot change"
        );
    }
    if let Some(s) = current.as_mut() {
        s["crop_sha256"] = json!(crop_digest(ctx, s)?);
        next["page"] = s["page"].clone();
        next["box"] = s["box"].clone();
    }
    let previous = &old["local_evidence"];
    if old["state"] == "dismissed"
        && f["state"] == "dismissed"
        && old["closure_check"]["code"] == "INDEPENDENT_ARBITRATION_RECORDED"
        && old["closure_check"]["accepted"] == true
        && current.as_ref().is_some_and(|s| {
            s["page"] == previous["page"]
                && s["box"] == previous["box"]
                && s["crop_sha256"] == previous["crop_sha256"]
        })
    {
        next["closure_check"] = old["closure_check"].clone();
        next["local_evidence"] = previous.clone();
        return Ok(next);
    }
    if f["id"]
        .as_str()
        .is_some_and(|id| id.starts_with("fidelity-"))
        && f["state"] == "dismissed"
    {
        let reason = f["arbitration"]["basis"]
            .as_str()
            .is_some_and(|s| !s.trim().is_empty());
        if !arbiter_authorized || !reason || current.is_none() || previous.is_null() {
            next["state"] = json!("open");
            next["local_evidence"] = previous.clone();
            next["closure_check"] = json!({"code":"INDEPENDENT_ARBITRATION_REQUIRED","accepted":false,
                "next":"A frozen third-session arbiter must inspect bound source/current evidence and explain a false positive. Otherwise repair or report unresolved at the existing stop threshold."});
            return Ok(next);
        }
        next["closure_check"] = json!({"code":"INDEPENDENT_ARBITRATION_RECORDED","accepted":true,
            "quality_proven":false,"basis":f["arbitration"]["basis"]});
        next["local_evidence"] = current
            .context("finding_guard::transition_with_arbitration: missing required value")?;
        return Ok(next);
    }
    if old["state"] == "resolved"
        && f["state"] == "resolved"
        && !previous.is_null()
        && current.as_ref().is_some_and(|s| {
            s["page"] == previous["page"]
                && s["box"] == previous["box"]
                && s["crop_sha256"] == previous["crop_sha256"]
        })
        && old["closure_check"]["accepted"] == true
    {
        next["local_evidence"] = previous.clone();
        next["closure_check"] = old["closure_check"].clone();
        return Ok(next);
    }
    if old["state"] == "open" && f["state"] == "open" && !previous.is_null() {
        // Keep the first observation, not a moving baseline during repair.
        next["page"] = previous["page"].clone();
        next["box"] = previous["box"].clone();
        next["local_evidence"] = previous.clone();
        return Ok(next);
    }
    // All model-authored finding categories need local evidence. A layout
    // defect mislabeled as copy must not evade the same repair check.
    if f["state"] == "resolved" {
        let reason = match current.as_ref() {
            None => Some("MISSING_LOCAL_REPAIR_EVIDENCE"),
            Some(_) if previous.is_null() => Some("MISSING_LOCAL_REPAIR_EVIDENCE"),
            Some(_) if ctx.verify(&previous["image"]).is_err() => Some("STALE_REPAIR_BASELINE"),
            Some(s)
                if image::image_dimensions(ctx.verify(&previous["image"])?)?
                    != image::image_dimensions(ctx.verify(&s["image"])?)? =>
            {
                Some("REPAIR_RENDER_DIMENSIONS_CHANGED")
            }
            Some(s) if previous["page"] != s["page"] || previous["box"] != s["box"] => {
                Some("DEFECT_REGION_CHANGED")
            }
            Some(s) if previous["crop_sha256"] == s["crop_sha256"] => {
                Some("UNCHANGED_DEFECT_REGION")
            }
            Some(_) => None,
        };
        if let Some(code) = reason {
            next["state"] = json!(if old["state"] == "open" {
                "open"
            } else {
                "unknown"
            });
            next["closure_check"] = json!({"code":code,"accepted":false,"requested_state":"resolved",
              "next":"Record an open defect with its page ID and local render-pixel box before repair; preserve that viewport and render dimensions and supply changed actual pixels. Rerender at the original dimensions if resolution changed; a rescale, note or unrelated page edit cannot close it. Independent visual review is still required."});
            next["local_evidence"] = previous.clone();
            return Ok(next);
        }
        next["closure_check"] = json!({"code":"LOCAL_CHANGE_RECORDED","accepted":true,"before":previous,"after":current,
            "quality_proven":false});
    }
    next["local_evidence"] = current.unwrap_or(Value::Null);
    Ok(next)
}

/// A closed finding is reusable only while its local actual pixels are stable.
pub fn refresh(ctx: &Context, old: &Value, cycle: &Value, spec: &Spec) -> Result<Value> {
    let arbitrated = old["state"] == "dismissed"
        && old["closure_check"]["code"] == "INDEPENDENT_ARBITRATION_RECORDED";
    if old["state"] != "resolved" && !arbitrated {
        return Ok(old.clone());
    }
    let mut result = old.clone();
    // Historical evidence names the old render; bind the current render here.
    let mut request = old.clone();
    request
        .as_object_mut()
        .context("finding_guard::refresh: missing required value")?
        .remove("evidence");
    request
        .as_object_mut()
        .context("finding_guard::refresh: missing required value")?
        .remove("page_number");
    let current = scope(ctx, &request, old, cycle, spec)?;
    let same = if let Some(s) = current {
        old["local_evidence"]["crop_sha256"] == crop_digest(ctx, &s)?
    } else {
        false
    };
    if !same || old["closure_check"]["accepted"] != true {
        result["state"] = json!("unknown");
        result["closure_check"] = json!({"code":"CLOSED_FINDING_NEEDS_REVIEW","accepted":false,
          "next":"The local render changed or the old closure has no local repair evidence. Reinspect this finding; historical prose is not a current visual check."});
    }
    Ok(result)
}
