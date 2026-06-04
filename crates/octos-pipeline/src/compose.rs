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
    // Use the engine's marker-aware cycle rule, not the naive all-cycle check:
    // intentional retry/guard back-edges (IrEdge.back_edge) are permitted.
    if let Err(cycle) = crate::validate::detect_cycles_ignoring_marked_back_edges(&graph) {
        return Err(ComposeError::Cycle(cycle));
    }
    let violations = validate_under_profile(&graph, profile);
    if !violations.is_empty() {
        return Err(ComposeError::Profile(violations));
    }
    Ok(graph)
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
        // The retry/revision loop pattern real LLMs author: a gate that loops
        // back on failure, marked as an intentional back-edge.
        let json = r#"{
            "id":"p",
            "nodes":[
                {"id":"g","kind":{"type":"gate"}},
                {"id":"w","kind":{"type":"transform","prompt":"do work"}}
            ],
            "edges":[
                {"source":"g","target":"w"},
                {"source":"w","target":"g","condition":"outcome.status == \"fail\"","back_edge":true}
            ]
        }"#;
        assert!(
            compose_l2(json).is_ok(),
            "a marked retry loop should compose: {:?}",
            compose_l2(json).err()
        );
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
