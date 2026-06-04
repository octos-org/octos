//! L2 entry: compile an LLM-authored typed IR ([`crate::ir`]) into a validated,
//! profile-checked [`PipelineGraph`] ready for execution.
//!
//! Flow: parse JSON IR → compile to graph → cycle check → [`ValidationProfile`]
//! gate. Every failure is returned as structured [`ComposeError`] feedback
//! suitable for an LLM emit → validate → repair loop; a graph is never returned
//! unless it passed all gates.
//!
//! Note: full model/tool validation (which needs runtime facts the executor
//! projects in) is performed by [`crate::validate::diagnostics_with_context`]
//! inside the executor at run time, not here.

use crate::graph::PipelineGraph;
use crate::ir::{self, PipelineIr};
use crate::profile::{ProfileViolation, ValidationProfile, validate_under_profile};

/// Structured failure from [`compose`], suitable to feed back to the LLM.
#[derive(Debug)]
pub enum ComposeError {
    /// The IR JSON did not deserialize (unknown kind/field, malformed JSON).
    Parse(String),
    /// The IR was well-formed but could not compile (e.g. dangling edge).
    Compile(String),
    /// The compiled graph contains a cycle.
    Cycle(String),
    /// The compiled graph failed structural validation (no/ambiguous start node,
    /// unreachable nodes, fanout `converge` target missing, edge-condition parse,
    /// …) — the same rules the executor runs, surfaced here for repair.
    Structural(Vec<String>),
    /// The compiled graph violated the autonomy profile.
    Profile(Vec<ProfileViolation>),
}

impl ComposeError {
    /// A flat, LLM-facing list of error lines for repair feedback.
    pub fn feedback_lines(&self) -> Vec<String> {
        match self {
            ComposeError::Parse(m) => vec![format!("parse error: {m}")],
            ComposeError::Compile(m) => vec![format!("compile error: {m}")],
            ComposeError::Cycle(m) => vec![format!(
                "cycle: {m}. If this is an intentional retry/guard loop, set \
                 \"back_edge\": true on the looping edge; otherwise remove the cycle."
            )],
            ComposeError::Structural(errs) => {
                errs.iter().map(|e| format!("structural: {e}")).collect()
            }
            ComposeError::Profile(vs) => vs
                .iter()
                .map(|v| match &v.node {
                    Some(n) => format!("node '{n}': {}", v.message),
                    None => format!("graph: {}", v.message),
                })
                .collect(),
        }
    }
}

impl std::fmt::Display for ComposeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.feedback_lines().join("; "))
    }
}

impl std::error::Error for ComposeError {}

/// Compile + validate an IR JSON string under a profile. Returns a graph only
/// if every gate passed; otherwise structured feedback for repair.
pub fn compose(ir_json: &str, profile: &ValidationProfile) -> Result<PipelineGraph, ComposeError> {
    let ir: PipelineIr =
        serde_json::from_str(ir_json).map_err(|e| ComposeError::Parse(e.to_string()))?;
    let graph = ir::compile(&ir).map_err(|e| ComposeError::Compile(e.to_string()))?;
    // Cycle gate: exempt ONLY edges explicitly marked back_edge (label set by
    // ir::compile from IrEdge.back_edge). Unlike the DOT helper this does NOT
    // honor retry/guard keywords in edge CONDITION text, so an LLM's accidental
    // wording can't bypass the cycle gate.
    if let Err(cycle) = detect_cycle_ir(&graph) {
        return Err(ComposeError::Cycle(cycle));
    }
    // Structural validation (start node, reachability, fanout converge target,
    // edge-condition parse, …) — the same rules the executor runs, so a bad IR
    // fails foreground compose instead of dead-ending in the background run and
    // defeating the repair loop. Models/tools come from the trusted contract, so
    // declare them known to avoid spurious unknown-model/-tool diagnostics.
    let known_models: Vec<String> = graph
        .nodes
        .values()
        .filter_map(|n| n.model.clone())
        .collect();
    let known_tools: Vec<String> = graph
        .nodes
        .values()
        .flat_map(|n| n.tools.iter().cloned())
        .collect();
    let ctx = crate::validate::ValidationContext::default()
        .with_known_models(known_models)
        .with_known_tools(known_tools);
    let diags = crate::validate::diagnostics_with_context(&graph, &ctx);
    if crate::validate::has_errors(&diags) {
        let errs: Vec<String> = diags
            .iter()
            .filter(|d| d.severity == crate::validate::Severity::Error)
            .map(|d| format!("{}: {}", d.rule_id.code(), d.message))
            .collect();
        return Err(ComposeError::Structural(errs));
    }
    let violations = validate_under_profile(&graph, profile);
    if !violations.is_empty() {
        return Err(ComposeError::Profile(violations));
    }
    Ok(graph)
}

/// Cycle detection for the IR path: exempts ONLY edges explicitly marked as a
/// back-edge (`label == "back_edge"`, set by `ir::compile` from `back_edge:
/// true`). Does NOT honor retry/guard keywords in edge condition text.
fn detect_cycle_ir(graph: &PipelineGraph) -> Result<(), String> {
    use std::collections::{HashMap, HashSet};
    let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();
    for e in &graph.edges {
        if e.label.as_deref() == Some("back_edge") {
            continue;
        }
        adj.entry(e.source.as_str())
            .or_default()
            .push(e.target.as_str());
    }
    fn dfs<'a>(
        node: &'a str,
        adj: &HashMap<&str, Vec<&'a str>>,
        gray: &mut HashSet<&'a str>,
        black: &mut HashSet<&'a str>,
        path: &mut Vec<&'a str>,
    ) -> Result<(), String> {
        gray.insert(node);
        path.push(node);
        if let Some(neighbors) = adj.get(node) {
            for &next in neighbors {
                if black.contains(next) {
                    continue;
                }
                if gray.contains(next) {
                    let start = path.iter().position(|&n| n == next).unwrap_or(0);
                    let mut cyc: Vec<&str> = path[start..].to_vec();
                    cyc.push(next);
                    return Err(format!("cycle detected: {}", cyc.join(" -> ")));
                }
                dfs(next, adj, gray, black, path)?;
            }
        }
        path.pop();
        gray.remove(node);
        black.insert(node);
        Ok(())
    }
    let mut gray = HashSet::new();
    let mut black = HashSet::new();
    let mut path = Vec::new();
    for node in graph.nodes.keys() {
        if !black.contains(node.as_str()) {
            dfs(node.as_str(), &adj, &mut gray, &mut black, &mut path)?;
        }
    }
    Ok(())
}

/// Convenience: compose under the L2 default profile.
pub fn compose_l2(ir_json: &str) -> Result<PipelineGraph, ComposeError> {
    compose(ir_json, &ValidationProfile::l2_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"{
        "id":"demo",
        "nodes":[
            {"id":"r","kind":{"type":"research","prompt":"find X"}},
            {"id":"s","kind":{"type":"synthesize","prompt":"write"}}
        ],
        "edges":[{"source":"r","target":"s"}]
    }"#;

    #[test]
    fn should_compose_valid_l2_ir() {
        let g = compose_l2(VALID).expect("valid IR composes");
        assert_eq!(g.nodes.len(), 2);
        assert_eq!(g.edges.len(), 1);
    }

    #[test]
    fn should_reject_unknown_kind_at_parse() {
        let json = r#"{"id":"p","nodes":[{"id":"n","kind":{"type":"shell"}}]}"#;
        assert!(matches!(compose_l2(json), Err(ComposeError::Parse(_))));
    }

    #[test]
    fn should_reject_dangling_edge_at_compile() {
        let json = r#"{"id":"p","nodes":[{"id":"a","kind":{"type":"gate"}}],"edges":[{"source":"a","target":"ghost"}]}"#;
        assert!(matches!(compose_l2(json), Err(ComposeError::Compile(_))));
    }

    #[test]
    fn should_reject_cycle() {
        let json = r#"{
            "id":"p",
            "nodes":[{"id":"a","kind":{"type":"gate"}},{"id":"b","kind":{"type":"gate"}}],
            "edges":[{"source":"a","target":"b"},{"source":"b","target":"a"}]
        }"#;
        assert!(matches!(compose_l2(json), Err(ComposeError::Cycle(_))));
    }

    #[test]
    fn should_accept_marked_back_edge_loop() {
        // The retry/revision loop pattern real LLMs author: an entry node, then
        // a gate that loops back to work on failure, marked as a back-edge.
        let json = r#"{
            "id":"p",
            "nodes":[
                {"id":"start","kind":{"type":"research","prompt":"begin"}},
                {"id":"work","kind":{"type":"transform","prompt":"do work"}},
                {"id":"check","kind":{"type":"gate"}}
            ],
            "edges":[
                {"source":"start","target":"work"},
                {"source":"work","target":"check"},
                {"source":"check","target":"work","condition":"outcome.status == \"fail\"","back_edge":true}
            ]
        }"#;
        assert!(
            compose_l2(json).is_ok(),
            "a marked retry loop with an entry node should compose: {:?}",
            compose_l2(json).err()
        );
    }

    #[test]
    fn should_reject_unmarked_cycle_even_with_retry_in_condition() {
        // P2b: an unmarked cycle whose condition merely contains the word
        // "retry" must NOT bypass the cycle gate (the IR path honors only the
        // explicit back_edge flag, not condition keywords).
        let json = r#"{
            "id":"p",
            "nodes":[
                {"id":"a","kind":{"type":"gate"}},
                {"id":"b","kind":{"type":"gate"}}
            ],
            "edges":[
                {"source":"a","target":"b"},
                {"source":"b","target":"a","condition":"outcome.content contains \"retry\""}
            ]
        }"#;
        assert!(matches!(compose_l2(json), Err(ComposeError::Cycle(_))));
    }

    #[test]
    fn should_reject_structurally_invalid_ir() {
        // P2a: a fanout whose converge target does not exist must fail compose
        // (structural), not slip through to the background run.
        let json = r#"{
            "id":"p",
            "nodes":[
                {"id":"f","kind":{"type":"fanout","worker_prompt":"do {task}","converge":"ghost"}}
            ],
            "edges":[]
        }"#;
        assert!(matches!(compose_l2(json), Err(ComposeError::Structural(_))));
    }

    #[test]
    fn should_reject_profile_violation_with_feedback() {
        // A tiny profile makes the 2-node graph exceed max_nodes.
        let mut p = ValidationProfile::l2_default();
        p.max_nodes = 1;
        match compose(VALID, &p) {
            Err(e @ ComposeError::Profile(_)) => {
                assert!(e.feedback_lines().iter().any(|l| l.contains("max_nodes")));
            }
            other => panic!("expected profile violation, got {other:?}"),
        }
    }
}
