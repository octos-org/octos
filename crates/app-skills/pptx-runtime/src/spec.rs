use crate::{Context, geometry, string};
use anyhow::{Context as _, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Canvas {
    pub width_px: f64,
    pub height_px: f64,
    pub width_in: f64,
    pub height_in: f64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    pub schema_version: u32,
    pub canvas: Canvas,
    pub styles: BTreeMap<String, Value>,
    pub pages: Vec<Page>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Page {
    pub id: String,
    pub background: Value,
    #[serde(default)]
    pub design: Option<Value>,
    pub copy: BTreeMap<String, String>,
    #[serde(default)]
    pub regions: BTreeMap<String, Value>,
    pub texts: Vec<Text>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub typography_groups: Vec<crate::typography::Group>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artwork_symbols: Vec<crate::authoring::ArtworkSymbol>,
    #[serde(default)]
    pub source_status: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Text {
    pub id: String,
    pub text: String,
    pub style: String,
    #[serde(rename = "box")]
    pub bounds: [f64; 4],
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub copy_id: Option<String>,
    /// Half-open Unicode-scalar offsets in normalized authoritative copy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copy_range: Option<[usize; 2]>,
    #[serde(default)]
    pub rotation: f64,
    #[serde(default)]
    pub format: Option<Value>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Style {
    pub font: String,
    pub size_pt: f64,
    pub color: String,
    pub bold: bool,
    pub italic: bool,
    pub align: String,
    pub anchor: String,
    pub wrap: bool,
    pub margins_px: [f64; 4],
    pub line_spacing: f64,
    pub space_before_pt: f64,
    pub space_after_pt: f64,
    pub locked: bool,
}
impl Default for Style {
    fn default() -> Self {
        Self {
            font: String::new(),
            size_pt: 0.,
            color: "000000".into(),
            bold: false,
            italic: false,
            align: "left".into(),
            anchor: "top".into(),
            wrap: true,
            margins_px: [0.; 4],
            line_spacing: 1.,
            space_before_pt: 0.,
            space_after_pt: 0.,
            locked: false,
        }
    }
}
impl Spec {
    pub fn style(&self, t: &Text) -> Result<Style> {
        let mut v = self
            .styles
            .get(&t.style)
            .ok_or_else(|| anyhow::anyhow!("unknown style {}", t.style))?
            .clone();
        ensure!(v.is_object(), "style must be an object");
        if let Some(over) = &t.format {
            ensure!(
                !v["locked"].as_bool().unwrap_or(false)
                    || over.as_object().is_some_and(|o| o.is_empty()),
                "locked style cannot be overridden"
            );
            for (k, val) in over
                .as_object()
                .ok_or_else(|| anyhow::anyhow!("format must be an object"))?
            {
                v[k] = val.clone()
            }
        }
        let s: Style = serde_json::from_value(v)?;
        ensure!(
            !s.font.trim().is_empty()
                && s.size_pt > 0.
                && s.size_pt <= 1000.
                && s.size_pt.is_finite(),
            "font and positive size required"
        );
        ensure!(
            matches!(s.align.as_str(), "left" | "center" | "right" | "justify")
                && matches!(s.anchor.as_str(), "top" | "middle" | "bottom"),
            "unsupported text alignment"
        );
        ensure!(
            s.margins_px.iter().all(|n| n.is_finite() && *n >= 0.)
                && s.line_spacing.is_finite()
                && s.line_spacing > 0.
                && s.line_spacing <= 10.,
            "invalid spacing or margin"
        );
        ensure!(
            s.space_before_pt.is_finite()
                && s.space_after_pt.is_finite()
                && s.space_before_pt >= 0.
                && s.space_after_pt >= 0.,
            "invalid paragraph spacing"
        );
        let c = s.color.trim_start_matches('#');
        ensure!(
            c.len() == 6 && c.bytes().all(|b| b.is_ascii_hexdigit()),
            "color must be six hex digits"
        );
        Ok(s)
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(self.schema_version == 1, "unsupported schema_version");
        let c = &self.canvas;
        ensure!(
            [c.width_px, c.height_px, c.width_in, c.height_in]
                .iter()
                .all(|n| n.is_finite() && *n > 0.)
                && c.width_px <= 32768.
                && c.height_px <= 32768.
                && c.width_in <= 100.
                && c.height_in <= 100.,
            "invalid canvas"
        );
        ensure!(
            (c.width_px / c.height_px - c.width_in / c.height_in).abs() < 0.001,
            "physical and pixel aspect ratio differ"
        );
        ensure!(
            !self.pages.is_empty() && self.pages.len() <= 1000,
            "invalid page count"
        );
        let mut pages = BTreeSet::new();
        for p in &self.pages {
            ensure!(
                !p.id.is_empty() && pages.insert(&p.id),
                "missing/duplicate page id"
            );
            ensure!(p.texts.len() <= 10000, "too many text objects");
            let mut ids = BTreeSet::new();
            let mut seen: BTreeMap<&String, Vec<([usize; 2], bool)>> = BTreeMap::new();
            for t in &p.texts {
                ensure!(
                    !t.id.is_empty() && ids.insert(&t.id),
                    "duplicate/empty text id in {}",
                    p.id
                );
                geometry::rect(&json!(t.bounds))?;
                ensure!(
                    t.rotation.is_finite() && t.rotation.abs() < 360.,
                    "invalid rotation"
                );
                let key = t.copy_id.as_ref().unwrap_or(&t.id);
                let canonical = normalized(
                    p.copy
                        .get(key)
                        .ok_or_else(|| anyhow::anyhow!("missing copy for {}/{key}", p.id))?,
                );
                let chars: Vec<_> = canonical.chars().collect();
                let range = t.copy_range.unwrap_or([0, chars.len()]);
                ensure!(
                    range[0] <= range[1]
                        && range[1] <= chars.len()
                        && (t.copy_range.is_none() || range[0] < range[1]),
                    "invalid copy range for {}/{key}",
                    p.id
                );
                ensure!(
                    normalized(&chars[range[0]..range[1]].iter().collect::<String>())
                        == normalized(&t.text),
                    "copy mismatch for {}/{key}",
                    p.id
                );
                seen.entry(key)
                    .or_default()
                    .push((range, t.copy_range.is_some()));
                if let Some(owner) = &t.region {
                    geometry::quad(
                        p.regions
                            .get(owner)
                            .ok_or_else(|| anyhow::anyhow!("missing region {owner}"))?,
                    )?;
                }
                self.style(t)?;
            }
            for symbol in &p.artwork_symbols {
                ensure!(ids.insert(&symbol.id), "duplicate native/artwork object id");
                symbol.validate(p, &self.canvas)?;
                seen.entry(&symbol.copy_id)
                    .or_default()
                    .push((symbol.copy_range, true));
            }
            ensure!(
                seen.len() == p.copy.len(),
                "unrepresented copy occurrences in {}",
                p.id
            );
            for (key, mut spans) in seen {
                ensure!(
                    spans.len() == 1 || spans.iter().all(|(_, fragment)| *fragment),
                    "duplicate whole copy occurrence {key}; use explicit copy_range for fragments"
                );
                spans.sort_by_key(|(range, _)| range[0]);
                let mut end = 0;
                for (range, _) in spans {
                    ensure!(
                        range[0] == end,
                        "copy fragments have a gap or overlap for {}/{key}",
                        p.id
                    );
                    end = range[1];
                }
                ensure!(
                    end == normalized(&p.copy[key]).chars().count(),
                    "unrepresented copy fragment for {}/{key}",
                    p.id
                );
            }
            let text_ids = p.texts.iter().map(|t| &t.id).collect();
            crate::typography::validate(&p.typography_groups, &text_ids)?;
            for r in p.regions.values() {
                geometry::quad(r)?;
                for field in ["ink_align", "ink_valign"] {
                    if let Some(v) = r.get(field) {
                        ensure!(
                            v == "center",
                            "{field} supports the explicit center constraint only"
                        );
                    }
                }
                if let Some(n) = r.get("max_lines") {
                    ensure!(
                        n.as_u64().is_some_and(|n| n > 0 && n <= 10000),
                        "max_lines must be a positive integer"
                    );
                }
                if let Some(regions) = r.get("forbidden") {
                    let regions =
                        regions
                            .as_array()
                            .filter(|a| a.len() <= 256)
                            .ok_or_else(|| {
                                anyhow::anyhow!(
                                    "forbidden must be a bounded array of exclusion quads"
                                )
                            })?;
                    for region in regions {
                        geometry::quad(region)?;
                    }
                }
            }
        }
        Ok(())
    }
}
pub fn normalized(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn asset(ctx: &Context, v: &mut Value, base: &Path) -> Result<()> {
    let p = v
        .as_str()
        .or_else(|| v.get("path").and_then(Value::as_str))
        .ok_or_else(|| anyhow::anyhow!("asset needs a path"))?;
    let path = ctx.path(base.join(p))?;
    let b = ctx.binding(&path).with_context(|| format!(
        "asset reference {p:?}, resolved from {}; relative asset paths use the containing spec/page directory, not the workspace root", base.display()))?;
    if let Some(h) = v.get("sha256").and_then(Value::as_str) {
        ensure!(Some(h) == b["sha256"].as_str(), "asset hash mismatch")
    }
    *v = b;
    v["path"] = json!(path);
    Ok(())
}
pub fn load(ctx: &Context, path: &Path, page: Option<&str>) -> Result<Spec> {
    Ok(load_with_sources(ctx, path, page)?.0)
}
pub fn load_with_sources(
    ctx: &Context,
    path: &Path,
    page: Option<&str>,
) -> Result<(Spec, Vec<Value>)> {
    let path = ctx.path(path)?;
    let mut sources = vec![ctx.binding(&path)?];
    let mut v = ctx.json(&path)?;
    let base = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("missing spec parent"))?;
    for p in v
        .get_mut("pages")
        .ok_or_else(|| anyhow::anyhow!("pages required"))?
        .as_array_mut()
        .ok_or_else(|| anyhow::anyhow!("pages must be an array"))?
    {
        ensure!(p.is_object(), "page must be an object");
        let mut parent = base.to_path_buf();
        if let Some(s) = p.get("path").and_then(Value::as_str) {
            let file = ctx.path(base.join(s))?;
            sources.push(ctx.binding(&file)?);
            parent = file
                .parent()
                .ok_or_else(|| anyhow::anyhow!("page parent"))?
                .into();
            *p = ctx.json(&file)?;
        }
        ensure!(p.is_object(), "page file must contain an object");
        let page_id = p["id"].as_str().unwrap_or("<missing id>").to_owned();
        asset(ctx, &mut p["background"], &parent)
            .with_context(|| format!("page {page_id}: background"))?;
        if let Some(d) = p.get_mut("design").filter(|d| !d.is_null()) {
            asset(ctx, d, &parent).with_context(|| format!("page {page_id}: design"))?;
        }
    }
    let mut spec: Spec = serde_json::from_value(v)?;
    spec.validate()?;
    if let Some(id) = page {
        spec.pages.retain(|p| p.id == id);
        ensure!(spec.pages.len() == 1, "unknown page {id}");
    }
    for binding in &sources {
        ctx.verify(binding)?;
    }
    Ok((spec, sources))
}
pub fn patch(ctx: &Context, r: &Value) -> Result<Value> {
    let source = ctx.path(string(r, "spec")?)?;
    if let Some(h) = r["expected_sha256"].as_str() {
        ensure!(crate::fs::sha_file(&source)? == h, "stale spec")
    }
    let spec = load(ctx, &source, None)?;
    let mut v = serde_json::to_value(&spec)?;
    let edits = r["edits"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("edits must be an array"))?;
    let mut changes = Vec::new();
    for edit in edits {
        let page = string(edit, "page")?;
        let id = string(edit, "id")?;
        let pages = v["pages"]
            .as_array_mut()
            .ok_or_else(|| anyhow::anyhow!("pages"))?;
        let p = pages
            .iter_mut()
            .find(|p| p["id"] == page)
            .ok_or_else(|| anyhow::anyhow!("unknown page {page}"))?;
        let texts = p["texts"]
            .as_array_mut()
            .ok_or_else(|| anyhow::anyhow!("texts"))?;
        let t = texts
            .iter_mut()
            .find(|t| t["id"] == id)
            .ok_or_else(|| anyhow::anyhow!("unknown object {id}"))?;
        let before = serde_json::to_value(spec.style(&serde_json::from_value(t.clone())?)?)?;
        let mode = edit
            .get("format_mode")
            .map(|v| {
                v.as_str()
                    .ok_or_else(|| anyhow::anyhow!("format_mode must be merge or replace"))
            })
            .transpose()?
            .unwrap_or("merge");
        ensure!(
            matches!(mode, "merge" | "replace"),
            "format_mode must be merge or replace"
        );
        ensure!(
            edit.get("format_mode").is_none() || edit["set"].get("format").is_some(),
            "format_mode requires set.format"
        );
        for (k, val) in edit["set"]
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("set must be object"))?
        {
            if !matches!(
                k.as_str(),
                "box" | "rotation" | "format" | "style" | "region" | "text"
            ) {
                bail!("unsupported edit field {k}")
            }
            if k == "format" {
                merge_format(t, val, mode)?;
            } else {
                t[k] = val.clone();
            }
        }
        let after = serde_json::to_value(spec.style(&serde_json::from_value(t.clone())?)?)?;
        let fields: Vec<_> = before
            .as_object()
            .unwrap()
            .iter()
            .filter(|(k, v)| after.get(*k) != Some(*v))
            .map(|(k, _)| k.clone())
            .collect();
        changes.push(json!({"page":page,"id":id,"format_mode":mode,
            "effective_format_before":before,"effective_format_after":after,
            "changed_format_fields":fields}));
        // Content changes require a separately stated authoritative copy update.
        if let Some(copy) = edit.get("copy") {
            ensure!(
                t["copy_range"].is_null(),
                "fragment edits cannot rewrite authoritative copy; preserve the span or explicitly revise the content plan"
            );
            let key = t["copy_id"].as_str().unwrap_or(id).to_owned();
            p["copy"][key] = copy.clone();
        }
    }
    let spec: Spec = serde_json::from_value(v)?;
    spec.validate()?;
    let out = ctx.write_json(string(r, "output")?, &serde_json::to_value(spec)?)?;
    Ok(json!({"spec":ctx.binding(out)?,"changes":changes,"quality_approved":false}))
}

pub(crate) fn merge_format(t: &mut Value, value: &Value, mode: &str) -> Result<()> {
    if mode == "replace" {
        ensure!(
            value.is_object() || value.is_null(),
            "replacement format must be an object or null"
        );
        t["format"] = value.clone();
    } else {
        let fields = value.as_object().ok_or_else(|| {
            anyhow::anyhow!(
                "partial format must be an object; use format_mode:replace to clear overrides"
            )
        })?;
        if t["format"].is_null() {
            t["format"] = json!({});
        }
        let target = t["format"]
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("format must be an object"))?;
        target.extend(fields.clone());
    }
    Ok(())
}
