//! Graph validation (lint rules) for pipelines.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::condition;
use crate::graph::{HandlerKind, PipelineGraph};

/// A validation diagnostic.
#[derive(Debug, Clone)]
pub struct LintDiagnostic {
    pub rule: u32,
    pub severity: Severity,
    pub message: String,
}

/// Diagnostic severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

/// Validate a pipeline graph against all lint rules.
///
/// Returns a list of diagnostics. If any are `Error` severity,
/// the graph should not be executed.
pub fn validate(graph: &PipelineGraph) -> Vec<LintDiagnostic> {
    let mut diags = Vec::new();
    rule_01_start_node(graph, &mut diags);
    rule_02_unreachable_nodes(graph, &mut diags);
    rule_03_edge_targets_exist(graph, &mut diags);
    rule_04_no_self_loops(graph, &mut diags);
    rule_05_goal_gate_edges(graph, &mut diags);
    rule_06_conditions_parse(graph, &mut diags);
    rule_07_known_handler(graph, &mut diags);
    rule_08_prompt_required(graph, &mut diags);
    rule_09_no_duplicate_edges(graph, &mut diags);
    rule_10_positive_weight(graph, &mut diags);
    rule_11_at_least_one_node(graph, &mut diags);
    rule_12_edge_sources_exist(graph, &mut diags);
    rule_13_parallel_converge(graph, &mut diags);
    rule_14_dynamic_parallel(graph, &mut diags);
    rule_15_no_cycles(graph, &mut diags);
    diags
}

/// Check if any diagnostics are errors.
pub fn has_errors(diags: &[LintDiagnostic]) -> bool {
    diags.iter().any(|d| d.severity == Severity::Error)
}

/// Find the start node: named "start", or the only node with no incoming edges.
pub fn find_start_node(graph: &PipelineGraph) -> Option<String> {
    if graph.nodes.contains_key("start") {
        return Some("start".into());
    }

    let incoming: HashSet<&str> = graph.edges.iter().map(|e| e.target.as_str()).collect();
    let sources: Vec<&str> = graph
        .nodes
        .keys()
        .filter(|id| !incoming.contains(id.as_str()))
        .map(|s| s.as_str())
        .collect();

    if sources.len() == 1 {
        Some(sources[0].to_string())
    } else {
        None
    }
}

// ---- Individual rules ----

fn rule_01_start_node(graph: &PipelineGraph, diags: &mut Vec<LintDiagnostic>) {
    if find_start_node(graph).is_none() {
        let incoming: HashSet<&str> = graph.edges.iter().map(|e| e.target.as_str()).collect();
        let sources: Vec<&str> = graph
            .nodes
            .keys()
            .filter(|id| !incoming.contains(id.as_str()))
            .map(|s| s.as_str())
            .collect();

        diags.push(LintDiagnostic {
            rule: 1,
            severity: Severity::Error,
            message: if sources.is_empty() {
                "no start node found (all nodes have incoming edges)".into()
            } else {
                format!(
                    "ambiguous start: {} nodes with no incoming edges: {}",
                    sources.len(),
                    sources.join(", ")
                )
            },
        });
    }
}

fn rule_02_unreachable_nodes(graph: &PipelineGraph, diags: &mut Vec<LintDiagnostic>) {
    let start = match find_start_node(graph) {
        Some(s) => s,
        None => return, // can't check reachability without a start node
    };

    // Build adjacency list
    let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();
    for edge in &graph.edges {
        adj.entry(edge.source.as_str())
            .or_default()
            .push(edge.target.as_str());
    }

    // BFS from start
    let mut visited = HashSet::new();
    let mut queue = VecDeque::new();
    visited.insert(start.as_str());
    queue.push_back(start.as_str());

    while let Some(node) = queue.pop_front() {
        if let Some(neighbors) = adj.get(node) {
            for &next in neighbors {
                if visited.insert(next) {
                    queue.push_back(next);
                }
            }
        }
    }

    for id in graph.nodes.keys() {
        if !visited.contains(id.as_str()) {
            diags.push(LintDiagnostic {
                rule: 2,
                severity: Severity::Warning,
                message: format!("node '{id}' is unreachable from start"),
            });
        }
    }
}

fn rule_03_edge_targets_exist(graph: &PipelineGraph, diags: &mut Vec<LintDiagnostic>) {
    for edge in &graph.edges {
        if !graph.nodes.contains_key(&edge.target) {
            diags.push(LintDiagnostic {
                rule: 3,
                severity: Severity::Error,
                message: format!("edge target '{}' does not exist", edge.target),
            });
        }
    }
}

fn rule_04_no_self_loops(graph: &PipelineGraph, diags: &mut Vec<LintDiagnostic>) {
    for edge in &graph.edges {
        if edge.source == edge.target {
            let node = graph.nodes.get(&edge.source);
            let is_retry = node.is_some_and(|n| n.max_retries > 0);
            if !is_retry {
                diags.push(LintDiagnostic {
                    rule: 4,
                    severity: Severity::Warning,
                    message: format!("self-loop on '{}' without max_retries > 0", edge.source),
                });
            }
        }
    }
}

fn rule_05_goal_gate_edges(graph: &PipelineGraph, diags: &mut Vec<LintDiagnostic>) {
    for node in graph.nodes.values() {
        if node.goal_gate {
            let has_outgoing = graph.edges.iter().any(|e| e.source == node.id);
            if !has_outgoing {
                diags.push(LintDiagnostic {
                    rule: 5,
                    severity: Severity::Warning,
                    message: format!(
                        "goal_gate node '{}' has no outgoing edges (will always terminate)",
                        node.id
                    ),
                });
            }
        }
    }
}

fn rule_06_conditions_parse(graph: &PipelineGraph, diags: &mut Vec<LintDiagnostic>) {
    for edge in &graph.edges {
        if let Some(ref cond) = edge.condition {
            if let Err(e) = condition::parse_condition(cond) {
                diags.push(LintDiagnostic {
                    rule: 6,
                    severity: Severity::Error,
                    message: format!(
                        "edge {} -> {}: invalid condition '{}': {}",
                        edge.source, edge.target, cond, e
                    ),
                });
            }
        }
    }
}

fn rule_07_known_handler(graph: &PipelineGraph, diags: &mut Vec<LintDiagnostic>) {
    // HandlerKind is an enum, so if it parsed successfully it's known.
    // This rule catches the case where the parser couldn't identify the handler
    // and defaulted to Codergen — but since we default, this is always ok.
    // Keep as a placeholder for future custom handlers.
    let _ = (graph, diags);
}

fn rule_08_prompt_required(graph: &PipelineGraph, diags: &mut Vec<LintDiagnostic>) {
    for node in graph.nodes.values() {
        if node.handler == HandlerKind::Codergen && node.prompt.is_none() {
            diags.push(LintDiagnostic {
                rule: 8,
                severity: Severity::Warning,
                message: format!(
                    "codergen node '{}' has no prompt (will use default worker prompt)",
                    node.id
                ),
            });
        }
    }
}

fn rule_09_no_duplicate_edges(graph: &PipelineGraph, diags: &mut Vec<LintDiagnostic>) {
    let mut seen = HashSet::new();
    for edge in &graph.edges {
        let key = (&edge.source, &edge.target);
        if !seen.insert(key) {
            diags.push(LintDiagnostic {
                rule: 9,
                severity: Severity::Warning,
                message: format!("duplicate edge from '{}' to '{}'", edge.source, edge.target),
            });
        }
    }
}

fn rule_10_positive_weight(graph: &PipelineGraph, diags: &mut Vec<LintDiagnostic>) {
    for edge in &graph.edges {
        if edge.weight <= 0.0 {
            diags.push(LintDiagnostic {
                rule: 10,
                severity: Severity::Error,
                message: format!(
                    "edge {} -> {}: weight must be positive, got {}",
                    edge.source, edge.target, edge.weight
                ),
            });
        }
    }
}

fn rule_11_at_least_one_node(graph: &PipelineGraph, diags: &mut Vec<LintDiagnostic>) {
    if graph.nodes.is_empty() {
        diags.push(LintDiagnostic {
            rule: 11,
            severity: Severity::Error,
            message: "graph has no nodes".into(),
        });
    }
}

fn rule_12_edge_sources_exist(graph: &PipelineGraph, diags: &mut Vec<LintDiagnostic>) {
    for edge in &graph.edges {
        if !graph.nodes.contains_key(&edge.source) {
            diags.push(LintDiagnostic {
                rule: 12,
                severity: Severity::Error,
                message: format!("edge source '{}' does not exist", edge.source),
            });
        }
    }
}

fn rule_13_parallel_converge(graph: &PipelineGraph, diags: &mut Vec<LintDiagnostic>) {
    for node in graph.nodes.values() {
        if node.handler != HandlerKind::Parallel {
            continue;
        }

        // Must have converge attribute
        match &node.converge {
            None => {
                diags.push(LintDiagnostic {
                    rule: 13,
                    severity: Severity::Error,
                    message: format!("parallel node '{}' missing converge attribute", node.id),
                });
            }
            Some(target) if !graph.nodes.contains_key(target) => {
                diags.push(LintDiagnostic {
                    rule: 13,
                    severity: Severity::Error,
                    message: format!(
                        "parallel node '{}' converge target '{}' does not exist",
                        node.id, target
                    ),
                });
            }
            _ => {}
        }

        // Must have outgoing edges (otherwise nothing to parallelize)
        let has_targets = graph.edges.iter().any(|e| e.source == node.id);
        if !has_targets {
            diags.push(LintDiagnostic {
                rule: 13,
                severity: Severity::Warning,
                message: format!("parallel node '{}' has no outgoing edges", node.id),
            });
        }
    }
}

fn rule_15_no_cycles(graph: &PipelineGraph, diags: &mut Vec<LintDiagnostic>) {
    if let Err(cycle_path) = graph.detect_cycles() {
        diags.push(LintDiagnostic {
            rule: 15,
            severity: Severity::Error,
            message: cycle_path,
        });
    }
}

fn rule_14_dynamic_parallel(graph: &PipelineGraph, diags: &mut Vec<LintDiagnostic>) {
    for node in graph.nodes.values() {
        if node.handler != HandlerKind::DynamicParallel {
            continue;
        }

        // Must have converge attribute
        match &node.converge {
            None => {
                diags.push(LintDiagnostic {
                    rule: 14,
                    severity: Severity::Error,
                    message: format!(
                        "dynamic_parallel node '{}' missing converge attribute",
                        node.id
                    ),
                });
            }
            Some(target) if !graph.nodes.contains_key(target) => {
                diags.push(LintDiagnostic {
                    rule: 14,
                    severity: Severity::Error,
                    message: format!(
                        "dynamic_parallel node '{}' converge target '{}' does not exist",
                        node.id, target
                    ),
                });
            }
            _ => {}
        }

        // Warn if no prompt (planning prompt)
        if node.prompt.is_none() {
            diags.push(LintDiagnostic {
                rule: 14,
                severity: Severity::Warning,
                message: format!(
                    "dynamic_parallel node '{}' has no prompt (will use default planning prompt)",
                    node.id
                ),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_dot;

    #[test]
    fn test_valid_graph() {
        let dot = r#"
            digraph test {
                start [prompt="Begin"]
                finish [prompt="End"]
                start -> finish
            }
        "#;
        let graph = parse_dot(dot).unwrap();
        let diags = validate(&graph);
        assert!(!has_errors(&diags), "unexpected errors: {diags:?}");
    }

    #[test]
    fn test_no_start_node() {
        let dot = r#"
            digraph test {
                a [prompt="A"]
                b [prompt="B"]
                a -> b
                b -> a
            }
        "#;
        let graph = parse_dot(dot).unwrap();
        let diags = validate(&graph);
        assert!(has_errors(&diags));
        assert!(diags.iter().any(|d| d.rule == 1));
    }

    #[test]
    fn test_unreachable_node() {
        let dot = r#"
            digraph test {
                start [prompt="Begin"]
                finish [prompt="End"]
                orphan [prompt="Orphan"]
                start -> finish
            }
        "#;
        let graph = parse_dot(dot).unwrap();
        let diags = validate(&graph);
        assert!(
            diags
                .iter()
                .any(|d| d.rule == 2 && d.message.contains("orphan"))
        );
    }

    #[test]
    fn test_parallel_missing_converge() {
        let dot = r#"
            digraph test {
                fan [handler="parallel"]
                a [prompt="A"]
                fan -> a
            }
        "#;
        let graph = parse_dot(dot).unwrap();
        let diags = validate(&graph);
        assert!(has_errors(&diags));
        assert!(
            diags
                .iter()
                .any(|d| d.rule == 13 && d.message.contains("missing converge"))
        );
    }

    #[test]
    fn test_parallel_converge_not_found() {
        let dot = r#"
            digraph test {
                fan [handler="parallel", converge="nonexistent"]
                a [prompt="A"]
                fan -> a
            }
        "#;
        let graph = parse_dot(dot).unwrap();
        let diags = validate(&graph);
        assert!(has_errors(&diags));
        assert!(
            diags
                .iter()
                .any(|d| d.rule == 13 && d.message.contains("does not exist"))
        );
    }

    #[test]
    fn test_parallel_valid() {
        let dot = r#"
            digraph test {
                fan [handler="parallel", converge="merge"]
                a [prompt="A"]
                b [prompt="B"]
                merge [prompt="Merge"]
                fan -> a
                fan -> b
                a -> merge
                b -> merge
            }
        "#;
        let graph = parse_dot(dot).unwrap();
        let diags = validate(&graph);
        assert!(!has_errors(&diags), "unexpected errors: {diags:?}");
    }

    #[test]
    fn test_positive_weight() {
        let dot = r#"
            digraph test {
                a -> b [weight="0"]
            }
        "#;
        let graph = parse_dot(dot).unwrap();
        let diags = validate(&graph);
        assert!(has_errors(&diags));
        assert!(diags.iter().any(|d| d.rule == 10));
    }

    #[test]
    fn test_dynamic_parallel_missing_converge() {
        let dot = r#"
            digraph test {
                plan [handler="dynamic_parallel", prompt="Plan"]
                next [prompt="Next"]
                plan -> next
            }
        "#;
        let graph = parse_dot(dot).unwrap();
        let diags = validate(&graph);
        assert!(has_errors(&diags));
        assert!(
            diags
                .iter()
                .any(|d| d.rule == 14 && d.message.contains("missing converge"))
        );
    }

    #[test]
    fn test_dynamic_parallel_converge_not_found() {
        let dot = r#"
            digraph test {
                plan [handler="dynamic_parallel", converge="nonexistent", prompt="Plan"]
                next [prompt="Next"]
                plan -> next
            }
        "#;
        let graph = parse_dot(dot).unwrap();
        let diags = validate(&graph);
        assert!(has_errors(&diags));
        assert!(
            diags
                .iter()
                .any(|d| d.rule == 14 && d.message.contains("does not exist"))
        );
    }

    #[test]
    fn test_dynamic_parallel_no_prompt_warning() {
        let dot = r#"
            digraph test {
                plan [handler="dynamic_parallel", converge="analyze"]
                analyze [prompt="Analyze"]
                plan -> analyze
            }
        "#;
        let graph = parse_dot(dot).unwrap();
        let diags = validate(&graph);
        assert!(!has_errors(&diags));
        assert!(
            diags
                .iter()
                .any(|d| d.rule == 14 && d.message.contains("no prompt"))
        );
    }

    #[test]
    fn test_dynamic_parallel_valid() {
        let dot = r#"
            digraph test {
                plan [handler="dynamic_parallel", converge="analyze", prompt="Generate angles"]
                analyze [prompt="Cross-reference"]
                synthesize [prompt="Write report", goal_gate="true"]
                plan -> analyze
                analyze -> synthesize
            }
        "#;
        let graph = parse_dot(dot).unwrap();
        let diags = validate(&graph);
        assert!(!has_errors(&diags), "unexpected errors: {diags:?}");
        // No warnings about rule 14
        assert!(!diags.iter().any(|d| d.rule == 14));
    }
}
