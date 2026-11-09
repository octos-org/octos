//! Mechanical batching, never batched approval. Stop at the first failure and
//! report the completed prefix so callers do not repeat successful mutations.
use crate::{Context, string};
use anyhow::Context as _;
use anyhow::{Result, ensure};
use serde_json::{Value, json};

pub fn run(ctx: &Context, request: &Value) -> Result<Value> {
    let steps = request["steps"]
        .as_array()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("operations requires nonempty steps"))?;
    // No recursive expansion. Each step uses the same ordinary validator.
    for step in steps {
        ensure!(
            step.is_object() && step["action"].is_string(),
            "each step needs an action"
        );
        ensure!(
            step["action"] != "operations",
            "nested operations are not supported"
        );
    }
    let mut results = Vec::new();
    let mut media = Vec::new();
    let images = request
        .get("images")
        .map(|v| {
            v.as_array()
                .ok_or_else(|| anyhow::anyhow!("images must be an array"))
        })
        .transpose()?;
    if let Some(images) = images {
        for path in images {
            ctx.path(
                path.as_str()
                    .ok_or_else(|| anyhow::anyhow!("images entries must be paths"))?,
            )?;
        }
    }
    for (i, step) in steps.iter().enumerate() {
        match ctx.execute(step.clone()) {
            Ok(value) => {
                if let Some(paths) = value["model_media"].as_array() {
                    for p in paths {
                        if !media.contains(p) {
                            media.push(p.clone());
                        }
                    }
                }
                let failed = value["tool_success"] == false
                    || (value["qa_command"] == true && value["status"] != "pass");
                results.push(json!({"step":i,"action":step["action"],"result":value}));
                if failed {
                    return Ok(
                        json!({"tool_success":false,"completed_steps":i,"failed_step":i,"results":results,"quality_approved":false}),
                    );
                }
            }
            Err(e) => {
                return Ok(
                    json!({"tool_success":false,"completed_steps":i,"failed_step":i,
                "results":results,"error":format!("{e:#}"),"quality_approved":false,
                "next":"Successful steps already ran. Inspect the failed step's artifacts before retry; this is not a rollback."}),
                );
            }
        }
    }
    if let Some(paths) = images {
        for path in paths {
            let path = path
                .as_str()
                .context("operations::run: missing required value")?;
            let binding = match ctx.binding(path) {
                Ok(b) => b,
                Err(e) => {
                    return Ok(
                        json!({"tool_success":false,"completed_steps":steps.len(),"results":results,
                    "error":format!("operations completed but image attachment failed: {e:#}"),"quality_approved":false}),
                    );
                }
            };
            if !media.contains(&binding["path"]) {
                media.push(binding["path"].clone());
            }
        }
    }
    Ok(
        json!({"tool_success":true,"completed_steps":steps.len(),"results":results,
        "model_media":media,"quality_approved":false}),
    )
}

/// Read a selected group of original/current/sweep pixels AND their measurements
/// together. The index must belong to the supplied, still-valid cycle. This
/// avoids a separate file read for every crop without making visual decisions.
pub fn review_view(ctx: &Context, request: &Value) -> Result<Value> {
    let cycle_path = ctx.path(string(request, "cycle")?)?;
    crate::review::bound_cycle(ctx, &cycle_path)?;
    let index = ctx.json(string(request, "index")?)?;
    ensure!(
        index["cycle"] == ctx.binding(cycle_path)?,
        "view index belongs to another cycle"
    );
    let groups = index["batches"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("invalid view index"))?;
    let requested = request["groups"]
        .as_array()
        .filter(|g| !g.is_empty())
        .ok_or_else(|| anyhow::anyhow!("groups must be nonempty 0-based reading-group indexes"))?;
    let mut selected = Vec::new();
    let mut media = Vec::new();
    for id in requested {
        let i = id
            .as_u64()
            .and_then(|i| usize::try_from(i).ok())
            .filter(|&i| i < groups.len())
            .ok_or_else(|| anyhow::anyhow!("reading-group index out of range"))?;
        let group = &groups[i];
        for image in group["images"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("invalid group images"))?
        {
            ctx.verify(&image["image"])?;
            let path = &image["image"]["path"];
            if !media.contains(path) {
                media.push(path.clone());
            }
        }
        selected.push(json!({"group_index":i,"evidence":group}));
    }
    Ok(
        json!({"cycle":index["cycle"],"review_id":index["review_id"],"group_count":groups.len(),
        "selected":selected,"model_media":media,"quality_approved":false,
        "note":"These are source pixels and measurement candidates, not automatic visual judgments. No resampling. Record independent observations for every selected object."}),
    )
}
