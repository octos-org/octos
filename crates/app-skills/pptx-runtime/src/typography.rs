//! Actual exported text formatting and explicit same-role group checks.
//! Group membership is authored from the design; no semantic role is inferred.
use crate::spec::{Spec, Style};
use anyhow::{Result, ensure};
use deckcraft_engine::model::{Shape, resolve};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeSet;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Group {
    pub id: String,
    pub members: Vec<String>,
    pub fields: Vec<String>,
    pub basis: String,
}

pub fn validate(groups: &[Group], ids: &BTreeSet<&String>) -> Result<()> {
    let mut names = BTreeSet::new();
    for g in groups {
        ensure!(
            !g.id.trim().is_empty() && names.insert(&g.id) && !g.basis.trim().is_empty(),
            "typography group needs a unique id and an independent same-role basis"
        );
        let members: BTreeSet<_> = g.members.iter().collect();
        ensure!(
            members.len() >= 2
                && members.len() == g.members.len()
                && members.iter().all(|id| ids.contains(id)),
            "typography group members must be distinct known text IDs"
        );
        let fields: BTreeSet<_> = g.fields.iter().collect();
        ensure!(
            !fields.is_empty()
                && fields.len() == g.fields.len()
                && fields
                    .iter()
                    .all(|f| ["font", "size_pt", "bold", "italic"].contains(&f.as_str())),
            "typography group fields support font, size_pt, bold, italic only"
        );
    }
    Ok(())
}

pub fn font_runs(ctx: &resolve::Ctx, shape: &Shape) -> Vec<Value> {
    let mut out = Vec::new();
    if let Some(body) = &shape.text {
        for (pi, p) in body.paragraphs.iter().enumerate() {
            for (ri, run) in p
                .runs
                .iter()
                .enumerate()
                .filter(|(_, r)| !r.text.trim().is_empty())
            {
                let props = resolve::run(ctx, shape, p, &run.props);
                out.push(json!({"paragraph":pi,"run":ri,"text":run.text,
                    "font":props.font,"size_pt":props.size,"bold":props.bold.unwrap_or(false),
                    "italic":props.italic.unwrap_or(false),
                    "basis":"reopened PPTX formatting with inherited properties resolved"}));
            }
        }
    }
    out
}

fn same(field: &str, a: &Value, b: &Value) -> bool {
    if a.is_null() || b.is_null() {
        return false;
    }
    if field == "size_pt" {
        return a
            .as_f64()
            .zip(b.as_f64())
            .is_some_and(|(a, b)| (a - b).abs() <= 0.011);
    }
    a == b
}

pub fn declared_mismatch(runs: &[Value], style: &Style) -> bool {
    let expected = json!(style);
    runs.is_empty()
        || runs.iter().any(|r| {
            ["font", "size_pt", "bold", "italic"]
                .iter()
                .any(|f| !same(f, &r[f], &expected[f]))
        })
}

pub fn assess(spec: &Spec, rows: &[Value]) -> Vec<Value> {
    let mut results = Vec::new();
    for (pi, page) in spec.pages.iter().enumerate() {
        for group in &page.typography_groups {
            let mut members = Vec::new();
            let mut differences = Vec::new();
            let mut missing = Vec::new();
            let mut reference = None;
            for id in &group.members {
                let row = rows
                    .iter()
                    .find(|v| v["page_id"] == page.id && v["object_id"] == *id);
                let runs = row.and_then(|r| r["font_runs"].as_array());
                if runs.is_none_or(|r| r.is_empty()) {
                    missing.push(id);
                }
                for run in runs.into_iter().flatten() {
                    let reference: &Value = reference.get_or_insert_with(|| run.clone());
                    for field in &group.fields {
                        if run[field].is_null() {
                            missing.push(id);
                        } else if !reference[field].is_null()
                            && !same(field, &reference[field], &run[field])
                        {
                            differences.push(json!({"object_id":id,"field":field,
                                "reference":reference[field],"actual":run[field]}));
                        }
                    }
                }
                members.push(json!({"object_id":id,"font_runs":runs,"context_viewport":row.map(|v|&v["context_viewport"])}));
            }
            results.push(json!({"page":pi+1,"page_id":page.id,"group_id":group.id,"basis":group.basis,
                "fields":group.fields,"members":members,"differences":differences,"missing":missing,
                "status":if !differences.is_empty(){"fail"}else if !missing.is_empty(){"unknown"}else{"consistent"},
                "semantics_verified":false,"quality_approved":false}));
        }
    }
    results
}
