//! Graph validation (lint/type-check rules) for pipelines.

use std::collections::{HashMap, HashSet, VecDeque};

use octos_agent::tools::policy::TOOL_GROUPS;

use crate::condition;
use crate::graph::{HandlerKind, PipelineEdge, PipelineGraph, PipelineNode};

/// Default static fan-out cap used by the pre-execution validator.
pub const DEFAULT_STATIC_FANOUT_LIMIT: usize = 50;

/// A validation diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LintDiagnostic {
    /// Backward-compatible numeric rule identifier.
    pub rule: u32,
    /// Stable typed rule identifier.
    pub rule_id: RuleId,
    pub severity: Severity,
    pub location: GraphLocation,
    pub message: String,
    pub fix_hint: Option<String>,
}

/// New typed diagnostic name requested by the pre-execution checker contract.
pub type PipelineDiagnostic = LintDiagnostic;

/// Diagnostic severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

/// Location attached to a pipeline diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphLocation {
    Graph,
    Node(String),
    Edge { source: String, target: String },
}

/// Stable identifiers for validation rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RuleId {
    StartNode,
    UnreachableNode,
    EdgeTargetExists,
    SelfLoop,
    GoalGateEdges,
    ConditionParses,
    KnownHandler,
    PromptRequired,
    DuplicateEdge,
    PositiveWeight,
    AtLeastOneNode,
    EdgeSourceExists,
    ParallelConverge,
    DynamicParallel,
    Dag,
    TemplateBinding,
    EdgeReferenced,
    KnownModel,
    KnownToolPolicy,
    FanoutBound,
    HumanGateResolver,
    DanglingReference,
}

/// Context used for validation checks that depend on runtime/catalog state.
#[derive(Debug, Clone)]
pub struct ValidationContext {
    /// Provider/model catalog keys. `None` skips model-existence validation.
    pub known_models: Option<HashSet<String>>,
    /// Known tool names. `None` skips tool-name validation, but group names
    /// are still checked against `TOOL_GROUPS`.
    pub known_tools: Option<HashSet<String>>,
    /// Runtime template channels that are valid without an upstream node.
    pub runtime_channels: HashSet<String>,
    /// Artifact names declared outside the DOT graph.
    pub known_artifacts: HashSet<String>,
    /// Static per-node fan-out cap.
    pub fanout_limit: usize,
}

impl Default for ValidationContext {
    fn default() -> Self {
        Self {
            known_models: None,
            known_tools: Some(default_known_tools()),
            runtime_channels: default_runtime_channels(),
            known_artifacts: HashSet::new(),
            fanout_limit: DEFAULT_STATIC_FANOUT_LIMIT,
        }
    }
}

impl ValidationContext {
    pub fn with_known_models(mut self, models: HashSet<String>) -> Self {
        self.known_models = Some(models);
        self
    }

    pub fn with_known_artifacts<I, S>(mut self, artifacts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.known_artifacts = artifacts.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_runtime_channels<I, S>(mut self, channels: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.runtime_channels
            .extend(channels.into_iter().map(Into::into));
        self
    }
}

/// Validate a pipeline graph against all lint/type-check rules.
///
/// Returns a list of diagnostics. If any are `Error` severity, the graph
/// should not be executed.
pub fn validate(graph: &PipelineGraph) -> Vec<PipelineDiagnostic> {
    validate_with_context(graph, &ValidationContext::default())
}

/// Validate a pipeline graph with caller-supplied catalog/runtime facts.
pub fn validate_with_context(
    graph: &PipelineGraph,
    context: &ValidationContext,
) -> Vec<PipelineDiagnostic> {
    let mut diags = Vec::new();
    rule_11_at_least_one_node(graph, &mut diags);
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
    rule_12_edge_sources_exist(graph, &mut diags);
    rule_13_parallel_converge(graph, &mut diags);
    rule_14_dynamic_parallel(graph, &mut diags);
    rule_15_no_cycles(graph, &mut diags);
    rule_16_template_bindings(graph, context, &mut diags);
    rule_17_edge_referenced(graph, &mut diags);
    rule_18_known_models(graph, context, &mut diags);
    rule_19_known_tools(graph, context, &mut diags);
    rule_20_fanout_bound(graph, context, &mut diags);
    rule_21_human_gate_resolver(graph, &mut diags);
    rule_22_dangling_refs(graph, context, &mut diags);
    diags
}

/// Result-oriented validation entry point.
pub fn validate_result(graph: &PipelineGraph) -> Result<(), Vec<PipelineDiagnostic>> {
    let diags = validate(graph);
    if has_errors(&diags) {
        Err(diags)
    } else {
        Ok(())
    }
}

/// Check if any diagnostics are errors.
pub fn has_errors(diags: &[PipelineDiagnostic]) -> bool {
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
        .map(String::as_str)
        .collect();

    if sources.len() == 1 {
        Some(sources[0].to_string())
    } else {
        None
    }
}

fn diagnostic(
    rule: u32,
    rule_id: RuleId,
    severity: Severity,
    location: GraphLocation,
    message: impl Into<String>,
    fix_hint: impl Into<Option<String>>,
) -> PipelineDiagnostic {
    PipelineDiagnostic {
        rule,
        rule_id,
        severity,
        location,
        message: message.into(),
        fix_hint: fix_hint.into(),
    }
}

fn node_diag(
    rule: u32,
    rule_id: RuleId,
    severity: Severity,
    node_id: &str,
    message: impl Into<String>,
    fix_hint: impl Into<Option<String>>,
) -> PipelineDiagnostic {
    diagnostic(
        rule,
        rule_id,
        severity,
        GraphLocation::Node(node_id.to_string()),
        message,
        fix_hint,
    )
}

fn edge_diag(
    rule: u32,
    rule_id: RuleId,
    severity: Severity,
    edge: &PipelineEdge,
    message: impl Into<String>,
    fix_hint: impl Into<Option<String>>,
) -> PipelineDiagnostic {
    diagnostic(
        rule,
        rule_id,
        severity,
        GraphLocation::Edge {
            source: edge.source.clone(),
            target: edge.target.clone(),
        },
        message,
        fix_hint,
    )
}

// ---- Individual rules ----

fn rule_01_start_node(graph: &PipelineGraph, diags: &mut Vec<PipelineDiagnostic>) {
    if find_start_node(graph).is_some() {
        return;
    }

    let incoming: HashSet<&str> = graph.edges.iter().map(|e| e.target.as_str()).collect();
    let sources: Vec<&str> = graph
        .nodes
        .keys()
        .filter(|id| !incoming.contains(id.as_str()))
        .map(String::as_str)
        .collect();

    diags.push(diagnostic(
        1,
        RuleId::StartNode,
        Severity::Error,
        GraphLocation::Graph,
        if sources.is_empty() {
            "no start node found (all nodes have incoming edges)".to_string()
        } else {
            format!(
                "ambiguous start: {} nodes with no incoming edges: {}",
                sources.len(),
                sources.join(", ")
            )
        },
        Some("Add a single `start` node or connect all roots behind one source node.".to_string()),
    ));
}

fn rule_02_unreachable_nodes(graph: &PipelineGraph, diags: &mut Vec<PipelineDiagnostic>) {
    let start = match find_start_node(graph) {
        Some(s) => s,
        None => return,
    };

    let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();
    for edge in &graph.edges {
        adj.entry(edge.source.as_str())
            .or_default()
            .push(edge.target.as_str());
    }

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
            diags.push(node_diag(
                2,
                RuleId::UnreachableNode,
                Severity::Error,
                id,
                format!("node '{id}' is an orphan and is unreachable from start"),
                Some("Connect the node from the start component or remove it.".to_string()),
            ));
        }
    }
}

fn rule_03_edge_targets_exist(graph: &PipelineGraph, diags: &mut Vec<PipelineDiagnostic>) {
    for edge in &graph.edges {
        if !graph.nodes.contains_key(&edge.target) {
            diags.push(edge_diag(
                3,
                RuleId::EdgeTargetExists,
                Severity::Error,
                edge,
                format!("edge target '{}' does not exist", edge.target),
                Some("Declare the target node or remove this edge.".to_string()),
            ));
        }
    }
}

fn rule_04_no_self_loops(graph: &PipelineGraph, diags: &mut Vec<PipelineDiagnostic>) {
    for edge in &graph.edges {
        if edge.source != edge.target {
            continue;
        }
        let node = graph.nodes.get(&edge.source);
        let is_retry = node.is_some_and(|n| n.max_retries > 0) || is_retry_or_guard_edge(edge);
        if !is_retry {
            diags.push(edge_diag(
                4,
                RuleId::SelfLoop,
                Severity::Warning,
                edge,
                format!(
                    "self-loop on '{}' without max_retries or retry marker",
                    edge.source
                ),
                Some(
                    "Set `max_retries`, label the edge `retry`, or remove the self-loop."
                        .to_string(),
                ),
            ));
        }
    }
}

fn rule_05_goal_gate_edges(graph: &PipelineGraph, diags: &mut Vec<PipelineDiagnostic>) {
    for node in graph.nodes.values() {
        if !node.goal_gate {
            continue;
        }
        let has_outgoing = graph.edges.iter().any(|e| e.source == node.id);
        if !has_outgoing {
            diags.push(node_diag(
                5,
                RuleId::GoalGateEdges,
                Severity::Warning,
                &node.id,
                format!(
                    "goal_gate node '{}' has no outgoing edges (will always terminate)",
                    node.id
                ),
                Some("Add explicit success/failure edges if later work should run.".to_string()),
            ));
        }
    }
}

fn rule_06_conditions_parse(graph: &PipelineGraph, diags: &mut Vec<PipelineDiagnostic>) {
    for edge in &graph.edges {
        let Some(cond) = edge.condition.as_ref() else {
            continue;
        };
        if let Err(e) = condition::parse_condition(cond) {
            diags.push(edge_diag(
                6,
                RuleId::ConditionParses,
                Severity::Error,
                edge,
                format!(
                    "edge {} -> {}: invalid condition '{}': {}",
                    edge.source, edge.target, cond, e
                ),
                Some(
                    "Use supported condition syntax such as `outcome.status == \"pass\"`."
                        .to_string(),
                ),
            ));
        }
    }
}

fn rule_07_known_handler(_graph: &PipelineGraph, _diags: &mut Vec<PipelineDiagnostic>) {
    // HandlerKind is an enum. If parsing succeeded the handler is known; if it
    // failed, the parser fell back to Codergen for backward compatibility.
}

fn rule_08_prompt_required(graph: &PipelineGraph, diags: &mut Vec<PipelineDiagnostic>) {
    for node in graph.nodes.values() {
        if node.handler == HandlerKind::Codergen && node.prompt.is_none() {
            diags.push(node_diag(
                8,
                RuleId::PromptRequired,
                Severity::Warning,
                &node.id,
                format!(
                    "codergen node '{}' has no prompt (will use default worker prompt)",
                    node.id
                ),
                Some("Add a `prompt` attribute describing this node's task.".to_string()),
            ));
        }
    }
}

fn rule_09_no_duplicate_edges(graph: &PipelineGraph, diags: &mut Vec<PipelineDiagnostic>) {
    let mut seen = HashSet::new();
    for edge in &graph.edges {
        let key = (&edge.source, &edge.target);
        if !seen.insert(key) {
            diags.push(edge_diag(
                9,
                RuleId::DuplicateEdge,
                Severity::Warning,
                edge,
                format!("duplicate edge from '{}' to '{}'", edge.source, edge.target),
                Some(
                    "Remove the duplicate or make the conditions mutually meaningful.".to_string(),
                ),
            ));
        }
    }
}

fn rule_10_positive_weight(graph: &PipelineGraph, diags: &mut Vec<PipelineDiagnostic>) {
    for edge in &graph.edges {
        if edge.weight <= 0.0 {
            diags.push(edge_diag(
                10,
                RuleId::PositiveWeight,
                Severity::Error,
                edge,
                format!(
                    "edge {} -> {}: weight must be positive, got {}",
                    edge.source, edge.target, edge.weight
                ),
                Some("Use a positive weight or omit the attribute for the default.".to_string()),
            ));
        }
    }
}

fn rule_11_at_least_one_node(graph: &PipelineGraph, diags: &mut Vec<PipelineDiagnostic>) {
    if graph.nodes.is_empty() {
        diags.push(diagnostic(
            11,
            RuleId::AtLeastOneNode,
            Severity::Error,
            GraphLocation::Graph,
            "graph has no nodes",
            Some("Declare at least one node.".to_string()),
        ));
    }
}

fn rule_12_edge_sources_exist(graph: &PipelineGraph, diags: &mut Vec<PipelineDiagnostic>) {
    for edge in &graph.edges {
        if !graph.nodes.contains_key(&edge.source) {
            diags.push(edge_diag(
                12,
                RuleId::EdgeSourceExists,
                Severity::Error,
                edge,
                format!("edge source '{}' does not exist", edge.source),
                Some("Declare the source node or remove this edge.".to_string()),
            ));
        }
    }
}

fn rule_13_parallel_converge(graph: &PipelineGraph, diags: &mut Vec<PipelineDiagnostic>) {
    for node in graph.nodes.values() {
        if node.handler != HandlerKind::Parallel {
            continue;
        }

        match &node.converge {
            None => diags.push(node_diag(
                13,
                RuleId::ParallelConverge,
                Severity::Error,
                &node.id,
                format!("parallel node '{}' missing converge attribute", node.id),
                Some("Set `converge` to the node that joins branch outputs.".to_string()),
            )),
            Some(target) if !graph.nodes.contains_key(target) => diags.push(node_diag(
                13,
                RuleId::ParallelConverge,
                Severity::Error,
                &node.id,
                format!(
                    "parallel node '{}' converge target '{}' does not exist",
                    node.id, target
                ),
                Some("Declare the converge target node.".to_string()),
            )),
            _ => {}
        }

        let has_targets = graph.edges.iter().any(|e| e.source == node.id);
        if !has_targets {
            diags.push(node_diag(
                13,
                RuleId::ParallelConverge,
                Severity::Warning,
                &node.id,
                format!("parallel node '{}' has no outgoing edges", node.id),
                Some("Add branch edges or change the handler kind.".to_string()),
            ));
        }
    }
}

fn rule_14_dynamic_parallel(graph: &PipelineGraph, diags: &mut Vec<PipelineDiagnostic>) {
    for node in graph.nodes.values() {
        if node.handler != HandlerKind::DynamicParallel {
            continue;
        }

        match &node.converge {
            None => diags.push(node_diag(
                14,
                RuleId::DynamicParallel,
                Severity::Error,
                &node.id,
                format!(
                    "dynamic_parallel node '{}' missing converge attribute",
                    node.id
                ),
                Some("Set `converge` to the node that joins dynamic worker outputs.".to_string()),
            )),
            Some(target) if !graph.nodes.contains_key(target) => diags.push(node_diag(
                14,
                RuleId::DynamicParallel,
                Severity::Error,
                &node.id,
                format!(
                    "dynamic_parallel node '{}' converge target '{}' does not exist",
                    node.id, target
                ),
                Some("Declare the converge target node.".to_string()),
            )),
            _ => {}
        }

        if node.prompt.is_none() {
            diags.push(node_diag(
                14,
                RuleId::DynamicParallel,
                Severity::Warning,
                &node.id,
                format!(
                    "dynamic_parallel node '{}' has no prompt (will use default planning prompt)",
                    node.id
                ),
                Some("Add a planning `prompt` for deterministic task generation.".to_string()),
            ));
        }
    }
}

fn rule_15_no_cycles(graph: &PipelineGraph, diags: &mut Vec<PipelineDiagnostic>) {
    if let Some(cycle_path) = detect_unmarked_cycle(graph) {
        diags.push(diagnostic(
            15,
            RuleId::Dag,
            Severity::Error,
            GraphLocation::Graph,
            format!("cycle detected: {}", cycle_path.join(" -> ")),
            Some(
                "Break the cycle or mark the back-edge as `label=\"retry\"`/`label=\"guard\"`."
                    .to_string(),
            ),
        ));
    }
}

fn rule_16_template_bindings(
    graph: &PipelineGraph,
    context: &ValidationContext,
    diags: &mut Vec<PipelineDiagnostic>,
) {
    let upstream = transitive_upstream_by_node(graph);
    for node in graph.nodes.values() {
        for (field, template) in node_templates(node) {
            for var in template_variables(template) {
                if template_var_resolves(graph, context, &upstream, node, &var) {
                    continue;
                }
                diags.push(node_diag(
                    16,
                    RuleId::TemplateBinding,
                    Severity::Error,
                    &node.id,
                    format!(
                        "node '{}' {} references unbound template variable '{{{}}}'",
                        node.id, field, var
                    ),
                    Some(
                        "Use `{input}`, a runtime channel, a declared artifact/checkpoint, or an upstream node id.".to_string(),
                    ),
                ));
            }
        }
    }
}

fn rule_17_edge_referenced(graph: &PipelineGraph, diags: &mut Vec<PipelineDiagnostic>) {
    for edge in &graph.edges {
        let Some(target) = graph.nodes.get(&edge.target) else {
            continue;
        };
        let consumed = node_templates(target)
            .any(|(_, template)| template_references_source(template, &edge.source));
        if !consumed {
            diags.push(edge_diag(
                17,
                RuleId::EdgeReferenced,
                Severity::Warning,
                edge,
                format!(
                    "edge {} -> {} is declared but target template never references '{}'",
                    edge.source, edge.target, edge.source
                ),
                Some(format!(
                    "Reference `{{{}}}` in node '{}' or remove the dead data edge.",
                    edge.source, edge.target
                )),
            ));
        }
    }
}

fn rule_18_known_models(
    graph: &PipelineGraph,
    context: &ValidationContext,
    diags: &mut Vec<PipelineDiagnostic>,
) {
    let Some(known_models) = context.known_models.as_ref() else {
        return;
    };
    if known_models.is_empty() {
        return;
    }

    if let Some(model) = graph.default_model.as_deref() {
        check_model_value(
            model,
            known_models,
            GraphLocation::Graph,
            "graph default_model",
            diags,
        );
    }

    for node in graph.nodes.values() {
        if let Some(model) = node.model.as_deref() {
            check_model_value(
                model,
                known_models,
                GraphLocation::Node(node.id.clone()),
                "model",
                diags,
            );
        }
        if let Some(model) = node.planner_model.as_deref() {
            check_model_value(
                model,
                known_models,
                GraphLocation::Node(node.id.clone()),
                "planner_model",
                diags,
            );
        }
    }
}

fn rule_19_known_tools(
    graph: &PipelineGraph,
    context: &ValidationContext,
    diags: &mut Vec<PipelineDiagnostic>,
) {
    let known_groups: HashSet<&str> = TOOL_GROUPS.iter().map(|g| g.name).collect();
    for node in graph.nodes.values() {
        for tool in &node.tools {
            if tool.is_empty() {
                continue;
            }
            if tool.starts_with("group:") {
                if !known_groups.contains(tool.as_str()) && !tool.starts_with("group:robot:") {
                    diags.push(node_diag(
                        19,
                        RuleId::KnownToolPolicy,
                        Severity::Error,
                        &node.id,
                        format!("node '{}' references unknown tool group '{}'", node.id, tool),
                        Some("Use a known group such as `group:fs`, `group:runtime`, `group:search`, `group:web`, or `group:sessions`.".to_string()),
                    ));
                }
                continue;
            }

            let Some(known_tools) = context.known_tools.as_ref() else {
                continue;
            };
            if !known_tools.contains(tool) {
                diags.push(node_diag(
                    19,
                    RuleId::KnownToolPolicy,
                    Severity::Error,
                    &node.id,
                    format!("node '{}' references unknown tool '{}'", node.id, tool),
                    Some(
                        "Use a registered tool name or a known `group:*` policy group.".to_string(),
                    ),
                ));
            }
        }
    }
}

fn rule_20_fanout_bound(
    graph: &PipelineGraph,
    context: &ValidationContext,
    diags: &mut Vec<PipelineDiagnostic>,
) {
    let mut outgoing: HashMap<&str, usize> = HashMap::new();
    for edge in &graph.edges {
        *outgoing.entry(edge.source.as_str()).or_default() += 1;
    }

    for (node_id, degree) in outgoing {
        if degree > context.fanout_limit {
            diags.push(node_diag(
                20,
                RuleId::FanoutBound,
                Severity::Error,
                node_id,
                format!(
                    "node '{node_id}' fan-out degree {degree} exceeds cap {}",
                    context.fanout_limit
                ),
                Some("Reduce outgoing edges or split the fan-out into bounded stages.".to_string()),
            ));
        }
    }

    for node in graph.nodes.values() {
        let Some(max_tasks) = node.max_tasks else {
            continue;
        };
        if usize::try_from(max_tasks).unwrap_or(usize::MAX) > context.fanout_limit {
            diags.push(node_diag(
                20,
                RuleId::FanoutBound,
                Severity::Error,
                &node.id,
                format!(
                    "node '{}' max_tasks {} exceeds cap {}",
                    node.id, max_tasks, context.fanout_limit
                ),
                Some("Lower `max_tasks` or raise the validator cap explicitly.".to_string()),
            ));
        }
    }
}

fn rule_21_human_gate_resolver(graph: &PipelineGraph, diags: &mut Vec<PipelineDiagnostic>) {
    for node in graph.nodes.values() {
        if node.human_gate && node.human_resolver.as_deref().is_none_or(str::is_empty) {
            diags.push(node_diag(
                21,
                RuleId::HumanGateResolver,
                Severity::Error,
                &node.id,
                format!("human-gate node '{}' has no resolver configured", node.id),
                Some("Set `human_resolver` or `resolver` on the gate node.".to_string()),
            ));
        }
    }
}

fn rule_22_dangling_refs(
    graph: &PipelineGraph,
    context: &ValidationContext,
    diags: &mut Vec<PipelineDiagnostic>,
) {
    let checkpoints: HashSet<&str> = graph
        .nodes
        .values()
        .flat_map(|node| {
            node.checkpoints
                .iter()
                .map(|checkpoint| checkpoint.name.as_str())
        })
        .collect();

    for node in graph.nodes.values() {
        for (field, template) in node_templates(node) {
            for var in template_variables(template) {
                if let Some(name) = var
                    .strip_prefix("artifact:")
                    .or_else(|| var.strip_prefix("artifact."))
                {
                    if !context.known_artifacts.contains(name) {
                        diags.push(node_diag(
                            22,
                            RuleId::DanglingReference,
                            Severity::Error,
                            &node.id,
                            format!(
                                "node '{}' {} references unknown artifact '{}'",
                                node.id, field, name
                            ),
                            Some(
                                "Declare the artifact in validation context or fix the reference."
                                    .to_string(),
                            ),
                        ));
                    }
                }
                if let Some(name) = var
                    .strip_prefix("checkpoint:")
                    .or_else(|| var.strip_prefix("checkpoint."))
                {
                    if !checkpoints.contains(name) {
                        diags.push(node_diag(
                            22,
                            RuleId::DanglingReference,
                            Severity::Error,
                            &node.id,
                            format!(
                                "node '{}' {} references unknown checkpoint '{}'",
                                node.id, field, name
                            ),
                            Some("Declare `checkpoint=\"...\"` on an upstream node or fix the reference.".to_string()),
                        ));
                    }
                }
            }
        }
    }
}

fn check_model_value(
    value: &str,
    known_models: &HashSet<String>,
    location: GraphLocation,
    field_name: &str,
    diags: &mut Vec<PipelineDiagnostic>,
) {
    for model in value.split(',').map(str::trim).filter(|m| !m.is_empty()) {
        if known_models.contains(model) {
            continue;
        }
        diags.push(diagnostic(
            18,
            RuleId::KnownModel,
            Severity::Error,
            location.clone(),
            format!("{field_name} references unknown model '{model}'"),
            Some("Use a model key present in the provider catalog/model stylesheet.".to_string()),
        ));
    }
}

fn node_templates(node: &PipelineNode) -> impl Iterator<Item = (&'static str, &str)> {
    node.prompt
        .as_deref()
        .map(|prompt| ("prompt", prompt))
        .into_iter()
        .chain(
            node.worker_prompt
                .as_deref()
                .map(|prompt| ("worker_prompt", prompt)),
        )
}

fn template_variables(template: &str) -> Vec<String> {
    let mut vars = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        rest = &rest[start + 1..];
        if rest.starts_with('{') {
            rest = &rest[1..];
            continue;
        }
        let Some(end) = rest.find('}') else {
            break;
        };
        let candidate = &rest[..end];
        rest = &rest[end + 1..];
        if is_template_identifier(candidate) {
            vars.push(candidate.to_string());
        }
    }
    vars.sort();
    vars.dedup();
    vars
}

fn is_template_identifier(candidate: &str) -> bool {
    let mut chars = candidate.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':' | '/'))
}

fn template_references_source(template: &str, source: &str) -> bool {
    template_variables(template).iter().any(|var| {
        var == source
            || var
                .strip_prefix(source)
                .is_some_and(|rest| matches!(rest, ".output" | ":output"))
    })
}

fn template_var_resolves(
    graph: &PipelineGraph,
    context: &ValidationContext,
    upstream: &HashMap<String, HashSet<String>>,
    node: &PipelineNode,
    var: &str,
) -> bool {
    if context.runtime_channels.contains(var) {
        return true;
    }
    if var == "task" && node.handler == HandlerKind::DynamicParallel {
        return true;
    }
    if var
        .strip_prefix("artifact:")
        .or_else(|| var.strip_prefix("artifact."))
        .is_some_and(|name| context.known_artifacts.contains(name))
    {
        return true;
    }
    if var
        .strip_prefix("checkpoint:")
        .or_else(|| var.strip_prefix("checkpoint."))
        .is_some()
    {
        return graph
            .nodes
            .values()
            .flat_map(|n| n.checkpoints.iter())
            .any(|checkpoint| {
                var.ends_with(checkpoint.name.as_str()) && !checkpoint.name.is_empty()
            });
    }
    let normalized = var
        .strip_suffix(".output")
        .or_else(|| var.strip_suffix(":output"))
        .unwrap_or(var);
    upstream
        .get(&node.id)
        .is_some_and(|nodes| nodes.contains(normalized))
}

fn transitive_upstream_by_node(graph: &PipelineGraph) -> HashMap<String, HashSet<String>> {
    let mut reverse: HashMap<&str, Vec<&str>> = HashMap::new();
    for edge in &graph.edges {
        reverse
            .entry(edge.target.as_str())
            .or_default()
            .push(edge.source.as_str());
    }

    graph
        .nodes
        .keys()
        .map(|node_id| {
            let mut seen = HashSet::new();
            let mut queue: VecDeque<&str> = reverse
                .get(node_id.as_str())
                .cloned()
                .unwrap_or_default()
                .into();
            while let Some(next) = queue.pop_front() {
                if !seen.insert(next.to_string()) {
                    continue;
                }
                if let Some(parents) = reverse.get(next) {
                    for parent in parents {
                        queue.push_back(parent);
                    }
                }
            }
            (node_id.clone(), seen)
        })
        .collect()
}

fn detect_unmarked_cycle(graph: &PipelineGraph) -> Option<Vec<String>> {
    let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();
    for edge in &graph.edges {
        if is_retry_or_guard_edge(edge) {
            continue;
        }
        adj.entry(edge.source.as_str())
            .or_default()
            .push(edge.target.as_str());
    }

    let mut white: HashSet<&str> = graph.nodes.keys().map(String::as_str).collect();
    let mut gray: HashSet<&str> = HashSet::new();
    let mut black: HashSet<&str> = HashSet::new();
    let mut path: Vec<&str> = Vec::new();

    fn dfs<'a>(
        node: &'a str,
        adj: &HashMap<&str, Vec<&'a str>>,
        white: &mut HashSet<&'a str>,
        gray: &mut HashSet<&'a str>,
        black: &mut HashSet<&'a str>,
        path: &mut Vec<&'a str>,
    ) -> Option<Vec<String>> {
        white.remove(node);
        gray.insert(node);
        path.push(node);

        if let Some(neighbors) = adj.get(node) {
            for &next in neighbors {
                if black.contains(next) {
                    continue;
                }
                if gray.contains(next) {
                    let cycle_start = path.iter().position(|&n| n == next).unwrap_or(0);
                    let mut cycle: Vec<String> = path[cycle_start..]
                        .iter()
                        .map(|n| (*n).to_string())
                        .collect();
                    cycle.push(next.to_string());
                    return Some(cycle);
                }
                if let Some(cycle) = dfs(next, adj, white, gray, black, path) {
                    return Some(cycle);
                }
            }
        }

        path.pop();
        gray.remove(node);
        black.insert(node);
        None
    }

    let all_nodes: Vec<&str> = graph.nodes.keys().map(String::as_str).collect();
    for node in all_nodes {
        if white.contains(node) {
            if let Some(cycle) = dfs(node, &adj, &mut white, &mut gray, &mut black, &mut path) {
                return Some(cycle);
            }
        }
    }
    None
}

fn is_retry_or_guard_edge(edge: &PipelineEdge) -> bool {
    let label = edge
        .label
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let condition = edge
        .condition
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    label.contains("retry")
        || label.contains("guard")
        || condition.contains("retry")
        || condition.contains("guard")
}

fn default_runtime_channels() -> HashSet<String> {
    [
        "input",
        "user_input",
        "outcome",
        "context",
        "workspace",
        "files",
        "artifacts",
        "previous",
        "task",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

fn default_known_tools() -> HashSet<String> {
    let mut tools: HashSet<String> = TOOL_GROUPS
        .iter()
        .flat_map(|group| group.tools.iter().copied())
        .map(str::to_string)
        .collect();
    tools.extend(
        [
            "apply_patch",
            "bash",
            "browser",
            "deep_search",
            "delegate",
            "diff_edit",
            "edit_file",
            "exec_command",
            "glob",
            "grep",
            "list_dir",
            "manage_skills",
            "read_file",
            "search",
            "shell",
            "spawn",
            "spawn_agent",
            "synthesize_research",
            "web_fetch",
            "web_search",
            "write_file",
            "write_stdin",
        ]
        .into_iter()
        .map(str::to_string),
    );
    tools
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_dot;

    fn assert_rule(diags: &[PipelineDiagnostic], rule_id: RuleId) {
        assert!(
            diags.iter().any(|d| d.rule_id == rule_id),
            "expected {rule_id:?} in {diags:?}"
        );
    }

    fn known_models(names: &[&str]) -> ValidationContext {
        ValidationContext::default()
            .with_known_models(names.iter().map(|name| (*name).to_string()).collect())
    }

    #[test]
    fn test_valid_graph() {
        let dot = r#"
            digraph test {
                start [prompt="Begin with {input}", tools="read_file"]
                finish [prompt="End using {start}", tools="write_file"]
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
        assert_rule(&diags, RuleId::StartNode);
    }

    #[test]
    fn test_unreachable_node_is_error() {
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
        assert!(has_errors(&diags));
        assert_rule(&diags, RuleId::UnreachableNode);
        assert!(
            diags
                .iter()
                .any(|d| d.location == GraphLocation::Node("orphan".into()))
        );
    }

    #[test]
    fn template_binding_rejects_unbound_variable() {
        let graph = parse_dot(
            r#"
            digraph test {
                start [prompt="Use {missing}"]
            }
        "#,
        )
        .unwrap();
        let diags = validate(&graph);
        assert!(has_errors(&diags));
        assert_rule(&diags, RuleId::TemplateBinding);
        assert!(diags.iter().any(|d| d.fix_hint.is_some()));
    }

    #[test]
    fn template_binding_accepts_input_runtime_artifact_and_upstream() {
        let graph = parse_dot(
            r#"
            digraph test {
                start [prompt="Use {input} and {topic} and {artifact:deck}"]
                analyze [prompt="Analyze {start.output} with {context}"]
                start -> analyze
            }
        "#,
        )
        .unwrap();
        let ctx = ValidationContext::default()
            .with_known_artifacts(["deck"])
            .with_runtime_channels(["topic"]);
        let diags = validate_with_context(&graph, &ctx);
        assert!(!has_errors(&diags), "unexpected errors: {diags:?}");
    }

    #[test]
    fn dead_edge_warns_when_target_template_does_not_consume_source() {
        let graph = parse_dot(
            r#"
            digraph test {
                start [prompt="Use {input}"]
                finish [prompt="Ignore predecessor"]
                start -> finish
            }
        "#,
        )
        .unwrap();
        let diags = validate(&graph);
        assert!(!has_errors(&diags), "dead edge should warn only: {diags:?}");
        assert_rule(&diags, RuleId::EdgeReferenced);
    }

    #[test]
    fn edge_reference_passes_when_target_template_consumes_source() {
        let graph = parse_dot(
            r#"
            digraph test {
                start [prompt="Use {input}"]
                finish [prompt="Use {start}"]
                start -> finish
            }
        "#,
        )
        .unwrap();
        let diags = validate(&graph);
        assert!(
            !diags.iter().any(|d| d.rule_id == RuleId::EdgeReferenced),
            "edge should be consumed: {diags:?}"
        );
    }

    #[test]
    fn cycle_without_retry_marker_is_error() {
        let graph = parse_dot(
            r#"
            digraph test {
                start [prompt="A"]
                b [prompt="B"]
                start -> b
                b -> start
            }
        "#,
        )
        .unwrap();
        let diags = validate(&graph);
        assert!(has_errors(&diags));
        assert_rule(&diags, RuleId::Dag);
    }

    #[test]
    fn cycle_with_retry_marker_is_allowed() {
        let graph = parse_dot(
            r#"
            digraph test {
                start [prompt="A"]
                b [prompt="B"]
                start -> b
                b -> start [label="retry"]
            }
        "#,
        )
        .unwrap();
        let diags = validate(&graph);
        assert!(
            !diags.iter().any(|d| d.rule_id == RuleId::Dag),
            "retry back-edge should be ignored by DAG check: {diags:?}"
        );
    }

    #[test]
    fn model_catalog_rejects_unknown_model() {
        let graph = parse_dot(
            r#"
            digraph test {
                start [model="missing-model", prompt="A"]
            }
        "#,
        )
        .unwrap();
        let diags = validate_with_context(&graph, &known_models(&["known-model"]));
        assert!(has_errors(&diags));
        assert_rule(&diags, RuleId::KnownModel);
    }

    #[test]
    fn model_catalog_accepts_known_model_pool() {
        let graph = parse_dot(
            r#"
            digraph test {
                start [handler="dynamic_parallel", model="fast-a,fast-b", planner_model="strong-a", converge="merge", prompt="Plan"]
                merge [handler="noop"]
                start -> merge
            }
        "#,
        )
        .unwrap();
        let diags = validate_with_context(&graph, &known_models(&["fast-a", "fast-b", "strong-a"]));
        assert!(
            !diags.iter().any(|d| d.rule_id == RuleId::KnownModel),
            "known model pool should pass: {diags:?}"
        );
    }

    #[test]
    fn tool_policy_rejects_unknown_tool_and_group() {
        let graph = parse_dot(
            r#"
            digraph test {
                start [prompt="A", tools="read_file,not_a_tool,group:nope"]
            }
        "#,
        )
        .unwrap();
        let diags = validate(&graph);
        assert!(has_errors(&diags));
        assert_eq!(
            diags
                .iter()
                .filter(|d| d.rule_id == RuleId::KnownToolPolicy)
                .count(),
            2
        );
    }

    #[test]
    fn tool_policy_accepts_known_tool_and_group() {
        let graph = parse_dot(
            r#"
            digraph test {
                start [prompt="A", tools="read_file,group:search"]
            }
        "#,
        )
        .unwrap();
        let diags = validate(&graph);
        assert!(
            !diags.iter().any(|d| d.rule_id == RuleId::KnownToolPolicy),
            "known tool/group should pass: {diags:?}"
        );
    }

    #[test]
    fn fanout_over_cap_is_error() {
        let dot = format!(
            "digraph test {{ start [prompt=\"A\"] {} }}",
            (0..51)
                .map(|i| format!("start -> n{i}; n{i} [prompt=\"N\"]"))
                .collect::<Vec<_>>()
                .join("; ")
        );
        let graph = parse_dot(&dot).unwrap();
        let diags = validate(&graph);
        assert!(has_errors(&diags));
        assert_rule(&diags, RuleId::FanoutBound);
    }

    #[test]
    fn fanout_at_cap_is_allowed() {
        let dot = format!(
            "digraph test {{ start [prompt=\"A\"] {} }}",
            (0..50)
                .map(|i| format!("start -> n{i}; n{i} [prompt=\"{{start}}\"]"))
                .collect::<Vec<_>>()
                .join("; ")
        );
        let graph = parse_dot(&dot).unwrap();
        let diags = validate(&graph);
        assert!(
            !diags.iter().any(|d| d.rule_id == RuleId::FanoutBound),
            "cap-sized fanout should pass: {diags:?}"
        );
    }

    #[test]
    fn human_gate_requires_resolver() {
        let graph = parse_dot(
            r#"
            digraph test {
                start [handler="gate", human_gate="true", prompt="approve"]
            }
        "#,
        )
        .unwrap();
        let diags = validate(&graph);
        assert!(has_errors(&diags));
        assert_rule(&diags, RuleId::HumanGateResolver);
    }

    #[test]
    fn human_gate_with_resolver_passes() {
        let graph = parse_dot(
            r#"
            digraph test {
                start [handler="gate", human_gate="true", resolver="operator", prompt="approve"]
            }
        "#,
        )
        .unwrap();
        let diags = validate(&graph);
        assert!(
            !diags.iter().any(|d| d.rule_id == RuleId::HumanGateResolver),
            "resolver should satisfy human gate: {diags:?}"
        );
    }

    #[test]
    fn dangling_artifact_and_checkpoint_refs_are_errors() {
        let graph = parse_dot(
            r#"
            digraph test {
                start [prompt="Use {artifact:deck} and {checkpoint:post_search}"]
            }
        "#,
        )
        .unwrap();
        let diags = validate(&graph);
        assert!(has_errors(&diags));
        assert_eq!(
            diags
                .iter()
                .filter(|d| d.rule_id == RuleId::DanglingReference)
                .count(),
            2
        );
    }

    #[test]
    fn declared_artifact_and_checkpoint_refs_pass() {
        let graph = parse_dot(
            r#"
            digraph test {
                start [prompt="Seed", checkpoint="post_search"]
                finish [prompt="Use {artifact:deck} and {checkpoint:post_search} and {start}"]
                start -> finish
            }
        "#,
        )
        .unwrap();
        let ctx = ValidationContext::default().with_known_artifacts(["deck"]);
        let diags = validate_with_context(&graph, &ctx);
        assert!(
            !diags.iter().any(|d| d.rule_id == RuleId::DanglingReference),
            "declared references should pass: {diags:?}"
        );
    }

    #[test]
    fn test_parallel_missing_converge() {
        let dot = r#"
            digraph test {
                start [handler="parallel"]
                a [prompt="A"]
                start -> a
            }
        "#;
        let graph = parse_dot(dot).unwrap();
        let diags = validate(&graph);
        assert!(has_errors(&diags));
        assert_rule(&diags, RuleId::ParallelConverge);
    }

    #[test]
    fn test_parallel_valid() {
        let dot = r#"
            digraph test {
                start [handler="parallel", converge="merge"]
                a [prompt="A"]
                b [prompt="B"]
                merge [prompt="Merge {a} {b}"]
                start -> a
                start -> b
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
                start -> b [weight="0"]
            }
        "#;
        let graph = parse_dot(dot).unwrap();
        let diags = validate(&graph);
        assert!(has_errors(&diags));
        assert_rule(&diags, RuleId::PositiveWeight);
    }

    #[test]
    fn test_dynamic_parallel_missing_converge() {
        let dot = r#"
            digraph test {
                start [handler="dynamic_parallel", prompt="Plan"]
                next [prompt="Next"]
                start -> next
            }
        "#;
        let graph = parse_dot(dot).unwrap();
        let diags = validate(&graph);
        assert!(has_errors(&diags));
        assert_rule(&diags, RuleId::DynamicParallel);
    }

    #[test]
    fn test_dynamic_parallel_no_prompt_warning() {
        let dot = r#"
            digraph test {
                start [handler="dynamic_parallel", converge="analyze"]
                analyze [prompt="Analyze {start}"]
                start -> analyze
            }
        "#;
        let graph = parse_dot(dot).unwrap();
        let diags = validate(&graph);
        assert!(!has_errors(&diags));
        assert!(
            diags
                .iter()
                .any(|d| d.rule_id == RuleId::DynamicParallel && d.severity == Severity::Warning)
        );
    }
}
