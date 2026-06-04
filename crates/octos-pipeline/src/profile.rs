//! Per-autonomy-level validation profile for LLM-authored pipelines.
//!
//! The typed IR ([`crate::ir`]) already makes capability-escalating graphs
//! *unrepresentable*, but a [`ValidationProfile`] is the defense-in-depth gate
//! that runs on the *compiled* [`PipelineGraph`] before execution: it enforces
//! a handler allowlist, total node / edge / depth caps, a shell ban, and a
//! no-implicit-all-builtins rule. It also bounds any future raw-DOT (L3) path,
//! where the IR's structural guarantee does not apply.
//!
//! This is intentionally a standalone gate layered *on top of* the existing
//! structural validator ([`crate::validate`]); it does not modify the existing
//! rule set.

use std::collections::{HashMap, HashSet};

use crate::graph::{HandlerKind, PipelineGraph};

/// Tool names treated as a shell escape hatch and banned under `ban_shell`.
const SHELL_TOOLS: &[&str] = &["shell", "bash", "exec", "exec_command"];

/// Bounds applied to an LLM-authored / -influenced graph at a given autonomy
/// level.
#[derive(Debug, Clone)]
pub struct ValidationProfile {
    /// Handlers the LLM is permitted to use at this level.
    pub allowed_handlers: HashSet<HandlerKind>,
    pub max_nodes: usize,
    pub max_edges: usize,
    /// Longest chain length, in nodes.
    pub max_depth: usize,
    /// Reject the `shell` handler and shell-family tools.
    pub ban_shell: bool,
}

impl ValidationProfile {
    /// Bounds for L2 (palette-composed) pipelines.
    pub fn l2_default() -> Self {
        let allowed_handlers = [
            HandlerKind::Codergen,
            HandlerKind::Gate,
            HandlerKind::Noop,
            HandlerKind::Parallel,
            HandlerKind::DynamicParallel,
        ]
        .into_iter()
        .collect();
        Self {
            allowed_handlers,
            max_nodes: 40,
            max_edges: 120,
            max_depth: 20,
            ban_shell: true,
        }
    }
}

/// A single profile violation. `node` is `None` for graph-level violations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileViolation {
    pub node: Option<String>,
    pub message: String,
}

impl ProfileViolation {
    fn graph(message: impl Into<String>) -> Self {
        Self {
            node: None,
            message: message.into(),
        }
    }
    fn node(id: &str, message: impl Into<String>) -> Self {
        Self {
            node: Some(id.to_string()),
            message: message.into(),
        }
    }
}

/// Check a compiled graph against a profile. An empty result means it passes.
pub fn validate_under_profile(
    graph: &PipelineGraph,
    profile: &ValidationProfile,
) -> Vec<ProfileViolation> {
    let mut violations = Vec::new();

    if graph.nodes.len() > profile.max_nodes {
        violations.push(ProfileViolation::graph(format!(
            "graph has {} nodes, exceeds max_nodes {}",
            graph.nodes.len(),
            profile.max_nodes
        )));
    }
    if graph.edges.len() > profile.max_edges {
        violations.push(ProfileViolation::graph(format!(
            "graph has {} edges, exceeds max_edges {}",
            graph.edges.len(),
            profile.max_edges
        )));
    }
    // Depth is skipped on cyclic graphs — a cycle is already reported by the
    // structural validator, and a longest-path on a cycle is undefined.
    if let Some(depth) = longest_chain(graph) {
        if depth > profile.max_depth {
            violations.push(ProfileViolation::graph(format!(
                "graph depth {} exceeds max_depth {}",
                depth, profile.max_depth
            )));
        }
    }

    for node in graph.nodes.values() {
        if !profile.allowed_handlers.contains(&node.handler) {
            violations.push(ProfileViolation::node(
                &node.id,
                format!("handler {:?} is not allowed at this level", node.handler),
            ));
        }
        if profile.ban_shell {
            if node.handler == HandlerKind::Shell {
                violations.push(ProfileViolation::node(&node.id, "shell handler is banned"));
            }
            if let Some(tool) = node
                .tools
                .iter()
                .find(|t| SHELL_TOOLS.contains(&t.as_str()))
            {
                violations.push(ProfileViolation::node(
                    &node.id,
                    format!("shell-family tool '{tool}' is banned"),
                ));
            }
        }
        // A codergen node with no tools means "all builtins" — too broad for an
        // LLM-authored level; require an explicit allowlist. DynamicParallel is
        // included because its synthetic per-task workers inherit the node's
        // tools, so an empty list there also widens to all builtins.
        if matches!(
            node.handler,
            HandlerKind::Codergen | HandlerKind::DynamicParallel
        ) && node.tools.is_empty()
        {
            violations.push(ProfileViolation::node(
                &node.id,
                "node must declare an explicit tool allowlist (empty = all builtins)",
            ));
        }
    }

    violations
}

/// Longest chain length (in nodes) for a DAG via topological DP, or `None` if
/// the graph is cyclic (the cycle is reported by the structural validator).
fn longest_chain(graph: &PipelineGraph) -> Option<usize> {
    // Longest FORWARD chain: marked back-edges (label == "back_edge") are
    // treated as absent, so an intentional retry loop does not disable depth
    // enforcement — a long forward chain plus a back-edge can't slip past
    // max_depth. (An UNMARKED cycle is rejected earlier by the cycle gate, so
    // by the time depth is checked the forward graph is acyclic.)
    let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut indeg: HashMap<&str, usize> = graph.nodes.keys().map(|k| (k.as_str(), 0)).collect();
    for e in &graph.edges {
        if e.label.as_deref() == Some("back_edge") {
            continue;
        }
        adj.entry(e.source.as_str())
            .or_default()
            .push(e.target.as_str());
        *indeg.entry(e.target.as_str()).or_insert(0) += 1;
    }

    let mut dist: HashMap<&str, usize> = graph.nodes.keys().map(|k| (k.as_str(), 1)).collect();
    let mut queue: Vec<&str> = Vec::new();
    for (n, d) in &indeg {
        if *d == 0 {
            queue.push(*n);
        }
    }

    let mut i = 0;
    while i < queue.len() {
        let u = queue[i];
        i += 1;
        let du = *dist.get(u).unwrap_or(&1);
        if let Some(neighbors) = adj.get(u) {
            for &w in neighbors {
                if du + 1 > *dist.get(w).unwrap_or(&1) {
                    dist.insert(w, du + 1);
                }
                if let Some(d) = indeg.get_mut(w) {
                    *d -= 1;
                    if *d == 0 {
                        queue.push(w);
                    }
                }
            }
        }
    }

    dist.values().copied().max()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{PipelineEdge, PipelineNode};

    fn node(id: &str, handler: HandlerKind, tools: &[&str]) -> PipelineNode {
        PipelineNode {
            id: id.to_string(),
            handler,
            tools: tools.iter().map(|s| s.to_string()).collect(),
            ..PipelineNode::default()
        }
    }

    fn graph_with(nodes: Vec<PipelineNode>, edges: &[(&str, &str)]) -> PipelineGraph {
        let nodes = nodes.into_iter().map(|n| (n.id.clone(), n)).collect();
        let edges = edges
            .iter()
            .map(|(s, t)| PipelineEdge {
                source: s.to_string(),
                target: t.to_string(),
                label: None,
                condition: None,
                weight: 1.0,
            })
            .collect();
        PipelineGraph {
            id: "t".to_string(),
            label: None,
            default_model: None,
            max_total_tokens: None,
            default_timeout_secs: None,
            nodes,
            edges,
            subgraphs: Vec::new(),
        }
    }

    #[test]
    fn should_pass_clean_l2_graph() {
        let g = graph_with(
            vec![
                node("r", HandlerKind::Codergen, &["read_file"]),
                node("s", HandlerKind::Codergen, &["read_file"]),
            ],
            &[("r", "s")],
        );
        assert!(validate_under_profile(&g, &ValidationProfile::l2_default()).is_empty());
    }

    #[test]
    fn should_reject_shell_handler() {
        let g = graph_with(vec![node("x", HandlerKind::Shell, &["read_file"])], &[]);
        let v = validate_under_profile(&g, &ValidationProfile::l2_default());
        assert!(
            v.iter()
                .any(|x| x.message.contains("shell handler is banned"))
        );
        assert!(v.iter().any(|x| x.message.contains("not allowed")));
    }

    #[test]
    fn should_reject_shell_tool_smuggle() {
        let g = graph_with(
            vec![node("c", HandlerKind::Codergen, &["read_file", "bash"])],
            &[],
        );
        let v = validate_under_profile(&g, &ValidationProfile::l2_default());
        assert!(v.iter().any(|x| x.message.contains("'bash'")));
    }

    #[test]
    fn should_reject_codergen_empty_tools() {
        let g = graph_with(vec![node("c", HandlerKind::Codergen, &[])], &[]);
        let v = validate_under_profile(&g, &ValidationProfile::l2_default());
        assert!(
            v.iter()
                .any(|x| x.message.contains("explicit tool allowlist"))
        );
    }

    #[test]
    fn should_reject_too_many_nodes() {
        let mut p = ValidationProfile::l2_default();
        p.max_nodes = 1;
        let g = graph_with(
            vec![
                node("a", HandlerKind::Gate, &[]),
                node("b", HandlerKind::Gate, &[]),
            ],
            &[],
        );
        assert!(
            validate_under_profile(&g, &p)
                .iter()
                .any(|x| x.message.contains("max_nodes"))
        );
    }

    #[test]
    fn should_reject_depth_over_cap() {
        let mut p = ValidationProfile::l2_default();
        p.max_depth = 2;
        let g = graph_with(
            vec![
                node("a", HandlerKind::Gate, &[]),
                node("b", HandlerKind::Gate, &[]),
                node("c", HandlerKind::Gate, &[]),
                node("d", HandlerKind::Gate, &[]),
            ],
            &[("a", "b"), ("b", "c"), ("c", "d")],
        );
        assert!(
            validate_under_profile(&g, &p)
                .iter()
                .any(|x| x.message.contains("depth"))
        );
    }

    #[test]
    fn should_skip_depth_on_cyclic_graph() {
        let mut p = ValidationProfile::l2_default();
        p.max_depth = 1;
        let g = graph_with(
            vec![
                node("a", HandlerKind::Gate, &[]),
                node("b", HandlerKind::Gate, &[]),
            ],
            &[("a", "b"), ("b", "a")],
        );
        let v = validate_under_profile(&g, &p);
        assert!(!v.iter().any(|x| x.message.contains("depth")));
    }

    #[test]
    fn should_enforce_depth_through_marked_back_edge() {
        // P2c: a long forward chain + a MARKED retry back-edge must still hit
        // max_depth — the back-edge must not disable depth enforcement.
        let mut p = ValidationProfile::l2_default();
        p.max_depth = 2;
        let mut g = graph_with(
            vec![
                node("a", HandlerKind::Gate, &[]),
                node("b", HandlerKind::Gate, &[]),
                node("c", HandlerKind::Gate, &[]),
                node("d", HandlerKind::Gate, &[]),
            ],
            &[("a", "b"), ("b", "c"), ("c", "d")],
        );
        g.edges.push(PipelineEdge {
            source: "d".to_string(),
            target: "a".to_string(),
            label: Some("back_edge".to_string()),
            condition: None,
            weight: 1.0,
        });
        assert!(
            validate_under_profile(&g, &p)
                .iter()
                .any(|x| x.message.contains("depth")),
            "forward depth 4 must exceed max_depth 2 despite the back-edge"
        );
    }

    #[test]
    fn should_require_explicit_tools_on_dynamic_parallel() {
        // P2d: dynamic_parallel with empty tools (workers inherit them) must be
        // flagged, same as codergen.
        let g = graph_with(vec![node("f", HandlerKind::DynamicParallel, &[])], &[]);
        let v = validate_under_profile(&g, &ValidationProfile::l2_default());
        assert!(
            v.iter()
                .any(|x| x.message.contains("explicit tool allowlist"))
        );
    }
}
