//! Immutable, page-owned JSON inputs. Media stays in place and is bound by hash.
use crate::{Context, fs, spec::Spec};
use anyhow::{Context as _, Result, ensure};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

fn relative_asset(ctx: &Context, asset: &mut Value) -> Result<()> {
    let bound = ctx.verify(asset)?;
    let rel = bound.strip_prefix(&ctx.root)?;
    // The containing directory is always .octos/page-sources/<digest>.
    asset["path"] = json!(Path::new("../../..").join(rel));
    Ok(())
}

fn write_same(ctx: &Context, path: PathBuf, bytes: &[u8]) -> Result<Value> {
    if !path.exists() {
        if let Err(error) = ctx.write_new(&path, bytes) {
            if !path.exists() {
                return Err(error);
            }
        }
    }
    let binding = ctx.binding(&path)?;
    ensure!(
        binding["sha256"] == fs::digest(bytes),
        "frozen page input changed: {}",
        path.display()
    );
    Ok(binding)
}

pub fn freeze(ctx: &Context, spec: &Spec) -> Result<Vec<Value>> {
    ensure!(
        spec.pages.len() == 1,
        "freezing requires exactly one selected page"
    );
    let mut page = serde_json::to_value(&spec.pages[0])?;
    relative_asset(ctx, &mut page["background"])?;
    if !page["design"].is_null() {
        relative_asset(ctx, &mut page["design"])?;
    }
    if let Some(layers) = page.get_mut("artwork_layers").and_then(Value::as_array_mut) {
        for layer in layers {
            relative_asset(ctx, &mut layer["image"])?;
        }
    }
    let root = json!({"schema_version":spec.schema_version,"canvas":spec.canvas,
        "styles":spec.styles,"pages":[{"path":"page.json"}]});
    let fingerprint = fs::digest(&serde_json::to_vec(&[&root, &page])?);
    let dir = ctx.path(".octos/page-sources")?.join(fingerprint);
    let page_binding = write_same(
        ctx,
        dir.join("page.json"),
        &serde_json::to_vec_pretty(&page)?,
    )?;
    let root_binding = write_same(
        ctx,
        dir.join("root.json"),
        &serde_json::to_vec_pretty(&root)?,
    )?;
    // The frozen inputs must be independently rebuildable with the normal resolver.
    let rebuilt = crate::spec::load(ctx, &ctx.verify(&root_binding)?, Some(&spec.pages[0].id))
        .context("cannot read frozen build inputs")?;
    ensure!(
        serde_json::to_value(rebuilt)? == serde_json::to_value(spec)?,
        "frozen inputs differ from resolved page"
    );
    Ok(vec![root_binding, page_binding])
}
